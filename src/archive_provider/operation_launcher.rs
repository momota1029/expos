// Lease-bound 7-Zip process execution. This adapter deliberately owns no
// operation model state; its caller keeps the detached worker until return.

#[cfg(test)]
use super::application_image::ApplicationImageLease;
use super::*;
use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::OsStrExt,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED, EXCEPTION_BREAKPOINT, HANDLE},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            Diagnostics::Debug::{
                ContinueDebugEvent, DEBUG_EVENT, EXCEPTION_DEBUG_EVENT, EXIT_PROCESS_DEBUG_EVENT,
                LOAD_DLL_DEBUG_EVENT, WaitForDebugEvent,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, DEBUG_ONLY_THIS_PROCESS,
                EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, PROCESS_INFORMATION,
                ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};

const TERMINATED_EXIT: u32 = 0x7fff_0002;
const POLL_MS: u32 = 25;

/// Facts from one detached archive worker. No UI or operation state is changed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OperationOutcome {
    Exited { code: u32 },
    Cancelled,
    TimedOut,
    LaunchFailed,
    BindingRejected,
}

pub(super) fn run_seven_zip_helper(
    launch: super::super::seven_zip_helper::SevenZipHelperLaunch,
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> OperationOutcome {
    let args = launch.args().to_vec();
    let inherited = launch.inherited_handles();
    run_with_handles(
        &LaunchAdmission::Helper(launch),
        &args,
        cwd,
        deadline,
        cancelled,
        &inherited,
    )
}

pub(crate) fn run_seven_zip_reader(
    launch: super::super::seven_zip_reader::SevenZipReadLaunch,
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<crate::archive_manifest::ArchiveManifest, OperationOutcome> {
    let deadline = super::super::seven_zip_reader::bounded_deadline(deadline);
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let args = launch.args().to_vec();
    let inherited = launch.inherited_handles();
    let admission = LaunchAdmission::Reader(launch);
    let outcome = run_with_handles(&admission, &args, cwd, deadline, cancelled, &inherited);
    let LaunchAdmission::Reader(launch) = &admission else {
        unreachable!()
    };
    finish_reader_after_drain(outcome, cancelled, deadline, || {
        launch
            .read_records_after_drain()
            .map_err(|_| OperationOutcome::LaunchFailed)
    })
}

pub(crate) fn run_seven_zip_inspection(
    launch: super::super::seven_zip_reader::SevenZipInspectionLaunch,
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<super::super::seven_zip_extract_plan::Validated7zExtractPlan, OperationOutcome> {
    let deadline = super::super::seven_zip_reader::bounded_deadline(deadline);
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let args = launch.args().to_vec();
    let inherited = launch.inherited_handles();
    let admission = LaunchAdmission::Inspection(launch);
    let outcome = run_with_handles(&admission, &args, cwd, deadline, cancelled, &inherited);
    let LaunchAdmission::Inspection(launch) = &admission else {
        unreachable!()
    };
    finish_inspection_after_drain(outcome, cancelled, deadline, || {
        launch
            .indexed_records_after_drain()
            .map_err(|_| OperationOutcome::LaunchFailed)
    })
}

pub(crate) fn run_seven_zip_decode(
    launch: super::super::seven_zip_decode::SevenZipDecodeLaunch<'_>,
    cwd: &std::path::Path,
    caller_deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<super::super::seven_zip_decode_protocol::Receipt, OperationOutcome> {
    let deadline =
        caller_deadline.min(Instant::now() + super::super::seven_zip_decode::MAX_DECODE_WORK);
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let args = launch.args().to_vec();
    let inherited = launch.inherited_handles();
    let admission = LaunchAdmission::Decode(launch);
    let outcome = run_with_handles(&admission, &args, cwd, deadline, cancelled, &inherited);
    let LaunchAdmission::Decode(launch) = &admission else {
        unreachable!()
    };
    finish_decode_after_drain(outcome, cancelled, deadline, || {
        let receipt = launch
            .read_receipt_after_drain()
            .map_err(|_| OperationOutcome::LaunchFailed)?;
        if !super::super::seven_zip_decode_protocol::receipt_matches(launch.request(), &receipt)
            .map_err(|_| OperationOutcome::LaunchFailed)?
            || !launch.held_source_and_spool_match()
        {
            return Err(OperationOutcome::LaunchFailed);
        }
        Ok(receipt)
    })
}

#[cfg(test)]
pub(crate) fn run_worker_decode_drain_fixture_for_test(
    mode: &str,
    marker: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    drain_completed: &AtomicBool,
) -> OperationOutcome {
    let outcome = tests::run_worker_decode_drain_fixture(mode, marker, deadline, cancelled);
    eprintln!("worker drain fixture {mode} ended with {outcome:?}; marker exists={}", marker.exists());
    // run() が返るのはJob Object配下のdescendantを含むdrain完了後だけ。
    drain_completed.store(true, Ordering::Release);
    outcome
}

#[cfg(test)]
pub(crate) fn worker_fixture_descendant_alive_for_test(pid: u32) -> bool {
    tests::descendant_alive(pid)
}

#[cfg(test)]
pub(crate) fn worker_fixture_descendant_stopped_for_test(pid: u32) {
    tests::assert_descendant_stopped(pid);
}

fn finish_decode_after_drain(
    outcome: OperationOutcome,
    cancelled: &AtomicBool,
    deadline: Instant,
    read_receipt: impl FnOnce() -> Result<
        super::super::seven_zip_decode_protocol::Receipt,
        OperationOutcome,
    >,
) -> Result<super::super::seven_zip_decode_protocol::Receipt, OperationOutcome> {
    finish_decode_after_drain_with_clock(outcome, cancelled, deadline, Instant::now, read_receipt)
}

fn finish_decode_after_drain_with_clock(
    outcome: OperationOutcome,
    cancelled: &AtomicBool,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    read_receipt: impl FnOnce() -> Result<
        super::super::seven_zip_decode_protocol::Receipt,
        OperationOutcome,
    >,
) -> Result<super::super::seven_zip_decode_protocol::Receipt, OperationOutcome> {
    if outcome != (OperationOutcome::Exited { code: 0 }) {
        return Err(outcome);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let receipt = read_receipt()?;
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    Ok(receipt)
}

fn finish_inspection_after_drain(
    outcome: OperationOutcome,
    cancelled: &AtomicBool,
    deadline: Instant,
    records_after_drain: impl FnOnce() -> Result<
        Vec<super::super::seven_zip_extract_plan::IndexedRecord>,
        OperationOutcome,
    >,
) -> Result<super::super::seven_zip_extract_plan::Validated7zExtractPlan, OperationOutcome> {
    finish_inspection_with_validation(
        outcome,
        cancelled,
        deadline,
        records_after_drain,
        |records| {
            super::super::seven_zip_extract_plan::Validated7zExtractPlan::validate(records)
                .map_err(|_| OperationOutcome::LaunchFailed)
        },
    )
}

fn finish_inspection_with_validation(
    outcome: OperationOutcome,
    cancelled: &AtomicBool,
    deadline: Instant,
    records_after_drain: impl FnOnce() -> Result<
        Vec<super::super::seven_zip_extract_plan::IndexedRecord>,
        OperationOutcome,
    >,
    validate: impl FnOnce(
        Vec<super::super::seven_zip_extract_plan::IndexedRecord>,
    ) -> Result<
        super::super::seven_zip_extract_plan::Validated7zExtractPlan,
        OperationOutcome,
    >,
) -> Result<super::super::seven_zip_extract_plan::Validated7zExtractPlan, OperationOutcome> {
    if outcome != (OperationOutcome::Exited { code: 0 }) {
        return Err(outcome);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let records = records_after_drain()?;
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let plan = validate(records)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    Ok(plan)
}

fn finish_reader_after_drain(
    outcome: OperationOutcome,
    cancelled: &AtomicBool,
    deadline: Instant,
    read_records: impl FnOnce() -> Result<Vec<crate::archive_manifest::RawRecord>, OperationOutcome>,
) -> Result<crate::archive_manifest::ArchiveManifest, OperationOutcome> {
    if outcome != (OperationOutcome::Exited { code: 0 }) {
        return Err(outcome);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let records = read_records()?;
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    let manifest = crate::archive_manifest::ArchiveManifest::validate(records)
        .map_err(|_| OperationOutcome::LaunchFailed)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(OperationOutcome::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(OperationOutcome::TimedOut);
    }
    Ok(manifest)
}

#[cfg(test)]
pub(super) fn run(
    lease: ArchiveOperationLease,
    args: &[OsString],
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> OperationOutcome {
    run_with_handles(
        &LaunchAdmission::Cli(lease),
        args,
        cwd,
        deadline,
        cancelled,
        &[],
    )
}

#[cfg(test)]
pub(super) fn run_with_test_handles(
    lease: ArchiveOperationLease,
    args: &[OsString],
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    inherited: &[windows::Win32::Foundation::HANDLE],
) -> OperationOutcome {
    run_with_handles(
        &LaunchAdmission::Cli(lease),
        args,
        cwd,
        deadline,
        cancelled,
        inherited,
    )
}

#[cfg(test)]
pub(super) fn run_application_for_test(
    provider: Archive7zCreationLease,
    image: ApplicationImageLease,
    args: &[OsString],
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> OperationOutcome {
    run_with_handles(
        &LaunchAdmission::ApplicationTest { provider, image },
        args,
        cwd,
        deadline,
        cancelled,
        &[],
    )
}

enum LaunchAdmission<'a> {
    #[cfg(test)]
    Cli(ArchiveOperationLease),
    #[cfg(test)]
    ApplicationTest {
        provider: Archive7zCreationLease,
        image: ApplicationImageLease,
    },
    Helper(super::super::seven_zip_helper::SevenZipHelperLaunch),
    Reader(super::super::seven_zip_reader::SevenZipReadLaunch),
    Inspection(super::super::seven_zip_reader::SevenZipInspectionLaunch),
    Decode(super::super::seven_zip_decode::SevenZipDecodeLaunch<'a>),
}

impl LaunchAdmission<'_> {
    fn root(&self) -> &ArchiveFileHint {
        match self {
            #[cfg(test)]
            Self::Cli(lease) => &lease.binding.executable,
            #[cfg(test)]
            Self::ApplicationTest { image, .. } => image.expected_root(),
            Self::Helper(launch) => launch.image().expected_root(),
            Self::Reader(launch) => launch.image().expected_root(),
            Self::Inspection(launch) => launch.lease().image().expected_root(),
            Self::Decode(launch) => launch.image().expected_root(),
        }
    }

    fn dll(&self) -> &ArchiveFileHint {
        match self {
            #[cfg(test)]
            Self::Cli(lease) => &lease.binding.zip_dll,
            #[cfg(test)]
            Self::ApplicationTest { provider, .. } => &provider.0.binding.zip_dll,
            Self::Helper(launch) => &launch.provider().0.binding.zip_dll,
            Self::Reader(launch) => &launch.provider().0.binding.zip_dll,
            Self::Inspection(launch) => &launch.lease().provider().0.binding.zip_dll,
            Self::Decode(launch) => &launch.lease().provider().0.binding.zip_dll,
        }
    }

    fn is_reader(&self) -> bool {
        matches!(self, Self::Reader(_) | Self::Inspection(_))
    }

    fn is_decode(&self) -> bool {
        matches!(self, Self::Decode(_))
    }
}

fn run_with_handles(
    admission: &LaunchAdmission<'_>,
    args: &[OsString],
    cwd: &std::path::Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    inherited: &[windows::Win32::Foundation::HANDLE],
) -> OperationOutcome {
    let expected_root = admission.root();
    let expected_dll = admission.dll();
    if cwd.as_os_str().is_empty()
        || !cwd.is_absolute()
        || cwd.as_os_str().encode_wide().any(|unit| unit == 0)
    {
        return OperationOutcome::LaunchFailed;
    }
    let mut nul = wide(OsStr::new("NUL"));
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let stdio = match unsafe {
        CreateFileW(
            PCWSTR(nul.as_mut_ptr()),
            0x8000_0000 | 0x4000_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&security),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    } {
        Ok(handle) => Handle(handle),
        Err(_) => return OperationOutcome::LaunchFailed,
    };
    let job = match unsafe { CreateJobObjectW(None, PCWSTR::null()) } {
        Ok(job) => Handle(job),
        Err(_) => return OperationOutcome::LaunchFailed,
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if admission.is_reader() {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = 256 * 1024 * 1024;
    } else if admission.is_decode() {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = 1024 * 1024 * 1024;
    }
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    }
    .is_err()
    {
        return OperationOutcome::LaunchFailed;
    }

    let app = wide(OsStr::new(&expected_root.path));
    let mut command = command_line(&expected_root.path, args);
    let cwd_wide = wide(cwd.as_os_str());
    let mut inherited_handles = Vec::with_capacity(inherited.len() + 1);
    inherited_handles.push(stdio.0);
    inherited_handles.extend(inherited.iter().copied());
    if inherited_handles
        .iter()
        .any(|handle| handle.is_invalid() || handle.0.is_null())
    {
        return OperationOutcome::LaunchFailed;
    }
    let Some(attrs) = attributes(&inherited_handles) else {
        return OperationOutcome::LaunchFailed;
    };
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdio.0;
    startup.StartupInfo.hStdOutput = stdio.0;
    startup.StartupInfo.hStdError = stdio.0;
    startup.lpAttributeList = attrs.ptr;
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            PCWSTR(app.as_ptr()),
            PWSTR(command.as_mut_ptr()),
            None,
            None,
            true,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | DEBUG_ONLY_THIS_PROCESS
                | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR(cwd_wide.as_ptr()),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut process_info,
        )
    };
    let Ok(()) = created else {
        return OperationOutcome::LaunchFailed;
    };
    let process = Handle(process_info.hProcess);
    let thread = Handle(process_info.hThread);
    if unsafe { AssignProcessToJobObject(job.0, process.0) }.is_err() {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(process.0, TERMINATED_EXIT);
        }
        drain_unassigned_process(process.0);
        return OperationOutcome::LaunchFailed;
    }
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        unsafe {
            let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
        }
        drain_until_empty(job.0, &mut DebugEvidence::default());
        return OperationOutcome::LaunchFailed;
    }

    // Debugger event は Continue まで debuggee を停止する。7-Zip は operation DLL を
    // LoadLibrary で後から読むため、最初は executable identity だけを検証する。
    let mut events = DebugEvidence::default();
    let mut resumed = false;
    let mut held: Option<(u32, u32, bool)> = None;
    let mut outcome = None;
    loop {
        let now = Instant::now();
        if outcome.is_none() && (cancelled.load(Ordering::Acquire) || now >= deadline) {
            outcome = Some(if cancelled.load(Ordering::Acquire) {
                OperationOutcome::Cancelled
            } else {
                OperationOutcome::TimedOut
            });
            unsafe {
                let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
            }
        }
        let mut event = DEBUG_EVENT::default();
        if unsafe { WaitForDebugEvent(&mut event, POLL_MS) }.is_err() {
            if active_processes(job.0) == Some(0) {
                break;
            }
            continue;
        }
        let was_initial_breakpoint = event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT
            && events.initial_breakpoints.contains(&event.dwProcessId);
        observe_received_debug_event(&mut events, &event);
        let root_first_chance_breakpoint = event.dwProcessId == process_info.dwProcessId
            && event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT
            && unsafe { event.u.Exception.ExceptionRecord.ExceptionCode } == EXCEPTION_BREAKPOINT
            && unsafe { event.u.Exception.dwFirstChance != 0 };
        if root_first_chance_breakpoint && !resumed {
            held = Some((event.dwProcessId, event.dwThreadId, true));
            if outcome.is_some() {
                break;
            }
            if !startup_breakpoint_matches(
                event.dwProcessId,
                event.dwThreadId,
                process_info.dwProcessId,
                process_info.dwThreadId,
                was_initial_breakpoint,
            ) {
                outcome = Some(OperationOutcome::BindingRejected);
                break;
            }
            if !matches_root_startup_expected(
                &events,
                expected_root,
                expected_dll,
                process_info.dwProcessId,
            ) {
                #[cfg(test)]
                eprintln!(
                    "operation gate reject: pid={} image={:?} expected_image={:?} image_ambiguous={} dlls={:?} expected_dll={:?} dll_ambiguous={}",
                    process_info.dwProcessId,
                    events.images.get(&process_info.dwProcessId),
                    expected_root,
                    events
                        .ambiguous_processes
                        .contains(&process_info.dwProcessId),
                    events.zip_dlls.get(&process_info.dwProcessId),
                    expected_dll,
                    events
                        .ambiguous_processes
                        .contains(&process_info.dwProcessId),
                );
                outcome = Some(OperationOutcome::BindingRejected);
                break;
            }
            if cancelled.load(Ordering::Acquire) {
                outcome = Some(OperationOutcome::Cancelled);
                unsafe {
                    let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
                }
                break;
            }
            if Instant::now() >= deadline {
                outcome = Some(OperationOutcome::TimedOut);
                unsafe {
                    let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
                }
                break;
            }
            let status = exception_continue_status(
                &mut events.initial_breakpoints,
                event.dwProcessId,
                unsafe { event.u.Exception.ExceptionRecord.ExceptionCode },
                unsafe { event.u.Exception.dwFirstChance },
            );
            if unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status).is_ok() } {
                held = None;
                resumed = true;
                continue;
            }
            outcome = Some(OperationOutcome::LaunchFailed);
            break;
        }
        if event.dwProcessId == process_info.dwProcessId
            && event.dwDebugEventCode == LOAD_DLL_DEBUG_EVENT
            && !matches_root_startup_expected(
                &events,
                expected_root,
                expected_dll,
                process_info.dwProcessId,
            )
        {
            // image は記録済みだが、Continue 前なのでまだ debuggee は先へ進んでいない。
            held = Some((event.dwProcessId, event.dwThreadId, false));
            if outcome.is_none() {
                outcome = Some(OperationOutcome::BindingRejected);
            }
            break;
        }
        let status = if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
            let info = unsafe { event.u.Exception };
            exception_continue_status(
                &mut events.initial_breakpoints,
                event.dwProcessId,
                info.ExceptionRecord.ExceptionCode,
                info.dwFirstChance,
            )
        } else {
            DBG_CONTINUE
        };
        if !unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status).is_ok() } {
            held = Some((
                event.dwProcessId,
                event.dwThreadId,
                event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT,
            ));
            outcome = Some(OperationOutcome::LaunchFailed);
            break;
        }
        if event.dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT
            && event.dwProcessId == process_info.dwProcessId
            && active_processes(job.0) == Some(0)
        {
            break;
        }
    }

    if let Some(reason) = outcome.take() {
        unsafe {
            let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
            let _ = windows::Win32::System::Threading::TerminateProcess(process.0, TERMINATED_EXIT);
        }
        if let Some((pid, tid, is_exception)) = held.take() {
            unsafe {
                let status = if is_exception {
                    DBG_EXCEPTION_NOT_HANDLED
                } else {
                    DBG_CONTINUE
                };
                let _ = ContinueDebugEvent(pid, tid, status);
            }
        }
        drain_until_empty(job.0, &mut events);
        return reason;
    }

    if !resumed {
        unsafe {
            let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
        }
        if let Some((pid, tid, is_exception)) = held.take() {
            unsafe {
                let status = if is_exception {
                    DBG_EXCEPTION_NOT_HANDLED
                } else {
                    DBG_CONTINUE
                };
                let _ = ContinueDebugEvent(pid, tid, status);
            }
        }
        drain_until_empty(job.0, &mut events);
        return OperationOutcome::BindingRejected;
    }

    // Keep the accepted lease and all native handles in this frame until the
    // Job Object reports no active processes, including descendants.
    let mut code = 0;
    let _ = unsafe { GetExitCodeProcess(process.0, &mut code) };
    let was_cancelled = cancelled.load(Ordering::Acquire);
    let was_timeout = Instant::now() >= deadline;
    drain_until_empty(job.0, &mut events);
    if was_cancelled {
        OperationOutcome::Cancelled
    } else if was_timeout {
        OperationOutcome::TimedOut
    } else if !matches_root_expected(
        &events,
        expected_root,
        expected_dll,
        process_info.dwProcessId,
    ) {
        OperationOutcome::BindingRejected
    } else {
        OperationOutcome::Exited { code }
    }
}

#[cfg(test)]
pub(super) fn dynamic_lease_for_test(expected_dll: &std::path::Path) -> ArchiveOperationLease {
    tests::dynamic_operation_lease(expected_dll)
}

#[cfg(test)]
fn matches_root_binding(
    evidence: &DebugEvidence,
    binding: &ArchiveOperationBinding,
    root_pid: u32,
) -> bool {
    matches_root_expected(evidence, &binding.executable, &binding.zip_dll, root_pid)
}

fn matches_root_expected(
    evidence: &DebugEvidence,
    expected_root: &ArchiveFileHint,
    expected_dll: &ArchiveFileHint,
    root_pid: u32,
) -> bool {
    if evidence.ambiguous_processes.contains(&root_pid) {
        return false;
    }
    let Some(Some(image)) = evidence.images.get(&root_pid) else {
        return false;
    };
    let Some(dlls) = evidence.zip_dlls.get(&root_pid) else {
        return false;
    };
    same_hint(image, expected_root) && dlls.len() == 1 && same_hint(&dlls[0], expected_dll)
}

#[cfg(test)]
fn matches_root_startup_binding(
    evidence: &DebugEvidence,
    binding: &ArchiveOperationBinding,
    root_pid: u32,
) -> bool {
    matches_root_startup_expected(evidence, &binding.executable, &binding.zip_dll, root_pid)
}

fn matches_root_startup_expected(
    evidence: &DebugEvidence,
    expected_root: &ArchiveFileHint,
    expected_dll: &ArchiveFileHint,
    root_pid: u32,
) -> bool {
    if evidence.ambiguous_processes.contains(&root_pid) {
        return false;
    }
    let Some(Some(image)) = evidence.images.get(&root_pid) else {
        return false;
    };
    if !same_hint(image, expected_root) {
        return false;
    }
    match evidence.zip_dlls.get(&root_pid) {
        None => true,
        Some(dlls) => dlls.len() == 1 && same_hint(&dlls[0], expected_dll),
    }
}

fn same_hint(left: &ArchiveFileHint, right: &ArchiveFileHint) -> bool {
    left.identity == right.identity
}

fn drain_until_empty(job: HANDLE, evidence: &mut DebugEvidence) {
    loop {
        match active_processes(job) {
            Some(0) => {
                while receive_continue(job, evidence, 0) {}
                return;
            }
            Some(_) => {
                receive_continue(job, evidence, POLL_MS);
            }
            None => std::thread::sleep(Duration::from_millis(POLL_MS as u64)),
        }
    }
}

fn drain_unassigned_process(process: HANDLE) {
    let mut evidence = DebugEvidence::default();
    loop {
        if unsafe { WaitForSingleObject(process, 0) } == windows::Win32::Foundation::WAIT_OBJECT_0 {
            while receive_continue(HANDLE::default(), &mut evidence, 0) {}
            return;
        }
        receive_continue(HANDLE::default(), &mut evidence, POLL_MS);
    }
}

fn receive_continue(job: HANDLE, evidence: &mut DebugEvidence, timeout: u32) -> bool {
    let mut event = DEBUG_EVENT::default();
    if unsafe { WaitForDebugEvent(&mut event, timeout) }.is_err() {
        return false;
    }
    observe_received_debug_event(evidence, &event);
    let status = if event.dwDebugEventCode == EXCEPTION_DEBUG_EVENT {
        let info = unsafe { event.u.Exception };
        exception_continue_status(
            &mut evidence.initial_breakpoints,
            event.dwProcessId,
            info.ExceptionRecord.ExceptionCode,
            info.dwFirstChance,
        )
    } else {
        DBG_CONTINUE
    };
    unsafe {
        ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status).ok();
    }
    let _ = job;
    true
}

fn command_line(executable: &std::path::Path, args: &[OsString]) -> Vec<u16> {
    let mut parts = vec![executable.as_os_str().to_owned()];
    parts.extend(args.iter().cloned());
    let text = parts
        .iter()
        .map(|part| quote_arg(part))
        .collect::<Vec<_>>()
        .join(" ");
    wide(OsStr::new(&text))
}

fn quote_arg(value: &OsStr) -> String {
    let text = value.to_string_lossy();
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in text.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
            out.push(ch);
            slashes = 0;
            continue;
        }
        out.push_str(&"\\".repeat(slashes));
        slashes = 0;
        out.push(ch);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn startup_breakpoint_matches(
    event_pid: u32,
    event_tid: u32,
    root_pid: u32,
    primary_tid: u32,
    was_initial_breakpoint: bool,
) -> bool {
    event_pid == root_pid && event_tid == primary_tid && was_initial_breakpoint
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn decode_finish_requires_successful_drain_and_final_cancel_deadline_gates() {
        let receipt = super::super::seven_zip_decode_protocol::Receipt {
            invocation_id: [1; 16],
            source: super::super::seven_zip_decode_protocol::FileFact {
                volume_serial: 1,
                file_index: 2,
                length: 3,
            },
            spool: super::super::seven_zip_decode_protocol::FileFact {
                volume_serial: 1,
                file_index: 4,
                length: 0,
            },
            plan_digest: [5; 32],
            final_total: 6,
            final_spool_length: 6,
        };
        let called = AtomicBool::new(false);
        assert_eq!(
            finish_decode_after_drain(
                OperationOutcome::Exited { code: 12 },
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                || {
                    called.store(true, Ordering::Release);
                    Ok(receipt.clone())
                },
            ),
            Err(OperationOutcome::Exited { code: 12 })
        );

        let mut times = [Instant::now(), Instant::now() + Duration::from_secs(2)].into_iter();
        assert_eq!(
            finish_decode_after_drain_with_clock(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                || times.next().expect("two clock reads"),
                || Ok(receipt.clone()),
            ),
            Err(OperationOutcome::TimedOut)
        );
        assert!(!called.load(Ordering::Acquire));

        for (cancelled, deadline, expected) in [
            (
                true,
                Instant::now() + Duration::from_secs(1),
                OperationOutcome::Cancelled,
            ),
            (
                false,
                Instant::now() - Duration::from_secs(1),
                OperationOutcome::TimedOut,
            ),
        ] {
            let called = AtomicBool::new(false);
            assert_eq!(
                finish_decode_after_drain(
                    OperationOutcome::Exited { code: 0 },
                    &AtomicBool::new(cancelled),
                    deadline,
                    || {
                        called.store(true, Ordering::Release);
                        Ok(receipt.clone())
                    },
                ),
                Err(expected)
            );
            assert!(!called.load(Ordering::Acquire));
        }

        let cancelled = AtomicBool::new(false);
        assert_eq!(
            finish_decode_after_drain(
                OperationOutcome::Exited { code: 0 },
                &cancelled,
                Instant::now() + Duration::from_secs(1),
                || {
                    cancelled.store(true, Ordering::Release);
                    Ok(receipt.clone())
                },
            ),
            Err(OperationOutcome::Cancelled)
        );
    }

    fn valid_reader_records() -> Vec<crate::archive_manifest::RawRecord> {
        vec![crate::archive_manifest::RawRecord {
            components: vec!["entry.txt".to_owned()],
            kind: crate::archive_manifest::NodeKind::File,
            declared_size: Some(1),
        }]
    }

    #[test]
    fn reader_finish_skips_copy_for_terminal_outcomes_and_initial_gates() {
        let terminal = [
            OperationOutcome::Exited { code: 7 },
            OperationOutcome::BindingRejected,
            OperationOutcome::LaunchFailed,
            OperationOutcome::TimedOut,
            OperationOutcome::Cancelled,
        ];
        for outcome in terminal {
            let expected = format!("{outcome:?}");
            let called = AtomicBool::new(false);
            let result = finish_reader_after_drain(
                outcome,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                || {
                    called.store(true, Ordering::Release);
                    Ok(valid_reader_records())
                },
            );
            assert_eq!(format!("{result:?}"), format!("Err({expected})"));
            assert!(
                !called.load(Ordering::Acquire),
                "{expected} must not copy the response"
            );
        }

        for (cancelled, deadline, expected) in [
            (
                true,
                Instant::now() + Duration::from_secs(1),
                OperationOutcome::Cancelled,
            ),
            (
                false,
                Instant::now() - Duration::from_secs(1),
                OperationOutcome::TimedOut,
            ),
        ] {
            let called = AtomicBool::new(false);
            let result = finish_reader_after_drain(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(cancelled),
                deadline,
                || {
                    called.store(true, Ordering::Release);
                    Ok(valid_reader_records())
                },
            );
            assert_eq!(result, Err(expected));
            assert!(!called.load(Ordering::Acquire));
        }
    }

    #[test]
    fn reader_finish_rejects_cancellation_arriving_during_response_copy() {
        let cancelled = AtomicBool::new(false);
        let result = finish_reader_after_drain(
            OperationOutcome::Exited { code: 0 },
            &cancelled,
            Instant::now() + Duration::from_secs(1),
            || {
                let detached = valid_reader_records();
                cancelled.store(true, Ordering::Release);
                Ok(detached)
            },
        );
        assert_eq!(result, Err(OperationOutcome::Cancelled));
    }

    #[test]
    fn inspection_finish_validates_only_after_successful_drained_exit_and_final_gates() {
        let valid = || {
            vec![super::super::seven_zip_extract_plan::IndexedRecord {
                index: 0,
                components: vec!["a.txt".into()],
                kind: crate::archive_manifest::NodeKind::File,
                declared_size: Some(0),
                crc32: Some(0),
            }]
        };
        let called = AtomicBool::new(false);
        assert_eq!(
            finish_inspection_after_drain(
                OperationOutcome::Exited { code: 9 },
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                || {
                    called.store(true, Ordering::Release);
                    Ok(valid())
                }
            ),
            Err(OperationOutcome::Exited { code: 9 })
        );
        assert!(!called.load(Ordering::Acquire));

        let cancelled = AtomicBool::new(false);
        assert_eq!(
            finish_inspection_after_drain(
                OperationOutcome::Exited { code: 0 },
                &cancelled,
                Instant::now() + Duration::from_secs(1),
                || {
                    cancelled.store(true, Ordering::Release);
                    Ok(valid())
                }
            ),
            Err(OperationOutcome::Cancelled)
        );
        assert_eq!(
            finish_inspection_after_drain(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(false),
                Instant::now() - Duration::from_secs(1),
                || Ok(valid())
            ),
            Err(OperationOutcome::TimedOut)
        );
        let copy_deadline = Instant::now() + Duration::from_millis(2);
        assert_eq!(
            finish_inspection_after_drain(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(false),
                copy_deadline,
                || {
                    std::thread::sleep(Duration::from_millis(4));
                    Ok(valid())
                }
            ),
            Err(OperationOutcome::TimedOut)
        );
        let validation_cancel = AtomicBool::new(false);
        assert_eq!(
            finish_inspection_with_validation(
                OperationOutcome::Exited { code: 0 },
                &validation_cancel,
                Instant::now() + Duration::from_secs(1),
                || Ok(valid()),
                |records| {
                    let plan =
                        super::super::seven_zip_extract_plan::Validated7zExtractPlan::validate(
                            records,
                        )
                        .map_err(|_| OperationOutcome::LaunchFailed)?;
                    validation_cancel.store(true, Ordering::Release);
                    Ok(plan)
                }
            ),
            Err(OperationOutcome::Cancelled)
        );
        let validation_deadline = Instant::now() + Duration::from_millis(2);
        assert_eq!(
            finish_inspection_with_validation(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(false),
                validation_deadline,
                || Ok(valid()),
                |records| {
                    let plan =
                        super::super::seven_zip_extract_plan::Validated7zExtractPlan::validate(
                            records,
                        )
                        .map_err(|_| OperationOutcome::LaunchFailed)?;
                    std::thread::sleep(Duration::from_millis(4));
                    Ok(plan)
                }
            ),
            Err(OperationOutcome::TimedOut)
        );
        assert!(
            finish_inspection_after_drain(
                OperationOutcome::Exited { code: 0 },
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                || Ok(valid())
            )
            .is_ok()
        );
    }

    #[cfg(windows)]
    use windows::{
        Win32::Foundation::GENERIC_READ,
        Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FileDispositionInfo, GetFileInformationByHandle, ReOpenFile,
            SetFileInformationByHandle,
        },
    };

    #[test]
    fn startup_gate_rejects_unexpected_root_thread_or_breakpoint() {
        assert!(startup_breakpoint_matches(10, 20, 10, 20, true));
        assert!(!startup_breakpoint_matches(10, 21, 10, 20, true));
        assert!(!startup_breakpoint_matches(10, 20, 10, 20, false));
        assert!(!startup_breakpoint_matches(11, 20, 10, 20, true));
    }
    use std::{
        fs,
        os::windows::io::AsRawHandle,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    fn static_import_fixture(
        include_7z_dll: bool,
        expected_dll_matches: bool,
    ) -> (tempfile::TempDir, ArchiveProviderOwner, u64) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("7z.exe");
        let dll = directory.path().join("7z.dll");
        let expected_dll = if expected_dll_matches {
            dll.clone()
        } else {
            directory.path().join("expected.dll")
        };
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();

        // The test binary imports winmm.dll through `timeGetTime`. Rewriting
        // that PE import name exercises a true loader-time 7z.dll event.
        let mut image = fs::read(&executable).unwrap();
        let import = b"WINMM.dll\0";
        let mut rewritten = 0;
        for offset in 0..=image.len() - import.len() {
            if image[offset..offset + import.len()].eq_ignore_ascii_case(import) {
                image[offset..offset + import.len()].copy_from_slice(b"7z.dll\0\0\0\0");
                rewritten += 1;
            }
        }
        assert!(rewritten > 0, "test image has the forced winmm import");
        fs::write(&executable, image).unwrap();
        let system_dll = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("winmm.dll");
        if include_7z_dll {
            fs::copy(&system_dll, &dll).unwrap();
        }
        if expected_dll != dll {
            fs::copy(system_dll, &expected_dll).unwrap();
        }

        let binding = ArchiveOperationBinding {
            executable: test_file_hint(&executable),
            zip_dll: test_file_hint(&expected_dll),
        };
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable,
                execution_hints: ArchiveExecutionHints {
                    candidate_executable: None,
                    loaded_zip_dll: Some(binding.zip_dll.clone()),
                    operation_binding: Some(binding),
                },
                version: None,
                formats: None,
            })
        ));
        (directory, owner, revision)
    }

    fn marker_args(marker: &std::path::Path, mode: &str) -> Vec<OsString> {
        [
            "--exact".to_owned(),
            "archive_provider::tests::seven_zip_child_fixture".to_owned(),
            "--nocapture".to_owned(),
            "--".to_owned(),
            format!("--fixture-mode={mode}"),
            format!("--fixture-marker={}", marker.display()),
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    fn file_info_for_test(handle: HANDLE) -> BY_HANDLE_FILE_INFORMATION {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle, &mut info).unwrap() };
        info
    }

    fn file_identity_for_test(handle: HANDLE) -> (u32, u64) {
        file_identity_from_info(file_info_for_test(handle))
    }

    fn file_identity_from_info(info: BY_HANDLE_FILE_INFORMATION) -> (u32, u64) {
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }

    fn operation_dll(directory: &std::path::Path) -> std::path::PathBuf {
        let path = directory.join("7z.dll");
        let system_dll = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("winmm.dll");
        fs::copy(system_dll, &path).unwrap();
        path
    }

    pub(super) fn dynamic_operation_lease(
        expected_dll: &std::path::Path,
    ) -> super::super::ArchiveOperationLease {
        let executable = std::env::current_exe().unwrap();
        let current_dir = std::env::current_dir().unwrap();
        let dll_arg = format!("--fixture-dll={}", expected_dll.display());
        let probe = super::super::windows_probe::probe_with_description(
            &executable,
            &current_dir,
            &[
                "--exact",
                "archive_provider::tests::seven_zip_child_fixture",
                "--nocapture",
                "--",
                "--fixture-mode=success_with_dll",
                &dll_arg,
            ],
            Duration::from_secs(15),
        );
        assert_eq!(
            probe.outcome,
            super::super::ProbeOutcome::Usable,
            "fixture probe failed: outcome={:?}, description={:?}, hints={:?}",
            probe.outcome,
            probe.description,
            probe.execution_hints
        );
        let binding = probe
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("fixture probe observes the dynamically loaded DLL");
        assert_eq!(
            binding.zip_dll.identity,
            test_file_hint(expected_dll).identity,
            "probe bound a different DLL image"
        );
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable,
                execution_hints: probe.execution_hints,
                version: probe.description.version,
                formats: probe.description.formats,
            }),
        ));
        owner
            .lease_operation_binding(revision)
            .expect("fixture operation binding leases successfully")
    }

    fn dynamic_operation_args(
        mode: &str,
        marker: &std::path::Path,
        dll_args: &[(&str, &std::path::Path)],
        first_marker: Option<&std::path::Path>,
    ) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("--exact"),
            OsString::from("archive_provider::tests::seven_zip_child_fixture"),
            OsString::from("--nocapture"),
            OsString::from("--"),
            OsString::from(format!("--fixture-mode={mode}")),
        ];
        args.extend(
            dll_args
                .iter()
                .map(|(name, path)| OsString::from(format!("{name}={}", path.display()))),
        );
        if let Some(path) = first_marker {
            args.push(OsString::from(format!(
                "--fixture-first-marker={}",
                path.display()
            )));
        }
        args.push(OsString::from(format!(
            "--fixture-marker={}",
            marker.display()
        )));
        args
    }

    fn run_dynamic_operation(lease: ArchiveOperationLease, args: &[OsString]) -> OperationOutcome {
        run(
            lease,
            args,
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(false),
        )
    }

    pub(super) fn run_worker_decode_drain_fixture(
        mode: &str,
        marker: &std::path::Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> OperationOutcome {
        let directory = tempfile::tempdir().unwrap();
        let expected_dll = operation_dll(directory.path());
        let lease = dynamic_operation_lease(&expected_dll);
        let args = dynamic_operation_args(
            mode,
            marker,
            &[("--fixture-dll", &expected_dll)],
            None,
        );
        run(
            lease,
            &args,
            &std::env::current_dir().unwrap(),
            deadline,
            cancelled,
        )
    }

    pub(super) fn descendant_alive(pid: u32) -> bool {
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        };
        match process {
            Ok(process) => {
                let alive = unsafe {
                    windows::Win32::System::Threading::WaitForSingleObject(process, 0)
                        != windows::Win32::Foundation::WAIT_OBJECT_0
                };
                unsafe { windows::Win32::Foundation::CloseHandle(process).unwrap() };
                alive
            }
            Err(error) if error.code().0 as u32 == 0x8007_0057 => false,
            Err(error) => panic!("could not query worker fixture descendant: {error}"),
        }
    }

    fn collect_tree_entries(
        root: &std::path::Path,
        current: &std::path::Path,
        entries: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
    ) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if entry.file_type().unwrap().is_dir() {
                entries.insert(relative, None);
                collect_tree_entries(root, &path, entries);
            } else {
                entries.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }

    #[test]
    fn dynamic_expected_dll_is_validated_before_post_load_marker() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let directory = tempfile::tempdir().unwrap();
        let expected_dir = directory.path().join("expected");
        fs::create_dir(&expected_dir).unwrap();
        let expected_dll = operation_dll(&expected_dir);
        let marker = directory.path().join("post-load-marker");
        let lease = dynamic_operation_lease(&expected_dll);
        let args = dynamic_operation_args(
            "operation_dynamic_one_dll",
            &marker,
            &[("--fixture-dll", &expected_dll)],
            None,
        );

        let result = run_dynamic_operation(lease, &args);
        assert_eq!(result, OperationOutcome::Exited { code: 0 });
        assert_eq!(fs::read(&marker).unwrap(), b"fixture passed DLL load");
    }

    #[test]
    fn dynamic_wrong_dll_is_rejected_before_post_load_marker() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let directory = tempfile::tempdir().unwrap();
        let expected_dir = directory.path().join("expected");
        let wrong_dir = directory.path().join("wrong");
        fs::create_dir(&expected_dir).unwrap();
        fs::create_dir(&wrong_dir).unwrap();
        let expected_dll = operation_dll(&expected_dir);
        let wrong_dll = operation_dll(&wrong_dir);
        let marker = directory.path().join("post-load-marker");
        let lease = dynamic_operation_lease(&expected_dll);
        let args = dynamic_operation_args(
            "operation_dynamic_one_dll",
            &marker,
            &[("--fixture-dll", &wrong_dll)],
            None,
        );

        let result = run_dynamic_operation(lease, &args);
        assert_eq!(result, OperationOutcome::BindingRejected);
        assert!(
            !marker.exists(),
            "wrong DLL load must not return to fixture code"
        );
    }

    #[test]
    fn dynamic_root_exit_without_dll_is_rejected() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let directory = tempfile::tempdir().unwrap();
        let dll_dir = directory.path().join("expected");
        fs::create_dir(&dll_dir).unwrap();
        let expected_dll = operation_dll(&dll_dir);
        let marker = directory.path().join("normal-return-marker");
        let lease = dynamic_operation_lease(&expected_dll);
        let args = dynamic_operation_args("operation_dynamic_no_dll", &marker, &[], None);

        let result = run_dynamic_operation(lease, &args);
        assert_eq!(result, OperationOutcome::BindingRejected);
        assert!(
            marker.exists(),
            "fixture should exit normally without loading DLL"
        );
    }

    #[test]
    fn later_wrong_dll_is_rejected_before_final_marker() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let directory = tempfile::tempdir().unwrap();
        let expected_dir = directory.path().join("expected");
        let wrong_dir = directory.path().join("wrong");
        fs::create_dir(&expected_dir).unwrap();
        fs::create_dir(&wrong_dir).unwrap();
        let expected_dll = operation_dll(&expected_dir);
        let wrong_dll = operation_dll(&wrong_dir);
        let first_marker = directory.path().join("first-load-marker");
        let final_marker = directory.path().join("final-marker");
        let lease = dynamic_operation_lease(&expected_dll);
        let args = dynamic_operation_args(
            "operation_dynamic_two_dlls",
            &final_marker,
            &[
                ("--fixture-dll", &expected_dll),
                ("--fixture-second-dll", &wrong_dll),
            ],
            Some(&first_marker),
        );

        let result = run_dynamic_operation(lease, &args);
        assert_eq!(result, OperationOutcome::BindingRejected);
        assert!(
            first_marker.exists(),
            "the first matching DLL load completed"
        );
        assert!(
            !final_marker.exists(),
            "second DLL load must be rejected before return"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires GPUI_FILER_7ZIP_TEST_EXECUTABLE to name an installed absolute 7z.exe"]
    fn installed_seven_zip_operation_launch() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let configured = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to an absolute installed 7z.exe path");
        let executable_path = std::path::PathBuf::from(configured);
        assert!(
            executable_path.is_absolute(),
            "GPUI_FILER_7ZIP_TEST_EXECUTABLE must be an absolute path, got {executable_path:?}"
        );
        assert!(
            executable_path.is_file(),
            "configured 7-Zip executable does not exist: {executable_path:?}"
        );
        let current_dir = std::env::current_dir().expect("current directory is available");

        let probe = super::super::windows_probe::probe_with_description(
            &executable_path,
            &current_dir,
            &["i"],
            Duration::from_secs(15),
        );
        assert_eq!(
            probe.outcome,
            super::super::ProbeOutcome::Usable,
            "real 7-Zip probe did not succeed: outcome={:?}, description={:?}, hints={:?}",
            probe.outcome,
            probe.description,
            probe.execution_hints
        );
        let binding = probe
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("successful real 7-Zip probe must observe the loaded 7z.dll binding");
        assert_eq!(
            binding.executable.identity,
            super::super::windows_probe::test_file_hint(&executable_path).identity,
            "probe bound a different executable file identity"
        );
        assert!(
            binding.zip_dll.path.is_file(),
            "probe bound a missing 7z.dll image: {:?}",
            binding.zip_dll.path
        );

        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable_path,
                execution_hints: probe.execution_hints,
                version: probe.description.version,
                formats: probe.description.formats,
            }),
        ));
        let lease = owner
            .lease_operation_binding(revision)
            .expect("current probed binding should be leased for launch");
        let result = run(
            lease,
            &[OsString::from("i")],
            &current_dir,
            Instant::now() + Duration::from_secs(30),
            &AtomicBool::new(false),
        );
        assert_eq!(
            result,
            OperationOutcome::Exited { code: 0 },
            "real 7-Zip operation launch failed after successful probe: {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires GPUI_FILER_7ZIP_TEST_EXECUTABLE to name an installed absolute 7z.exe"]
    fn installed_seven_zip_creation_with_explicit_cwd() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let configured = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to an absolute installed 7z.exe path");
        let executable_path = std::path::PathBuf::from(configured);
        assert!(executable_path.is_absolute());
        assert!(executable_path.is_file());
        let probe_cwd = std::env::current_dir().expect("current directory is available");
        let probe = super::super::windows_probe::probe_with_description(
            &executable_path,
            &probe_cwd,
            &["i"],
            Duration::from_secs(15),
        );
        assert_eq!(
            probe.outcome,
            super::super::ProbeOutcome::Usable,
            "installed 7-Zip probe failed: outcome={:?}, description={:?}, hints={:?}",
            probe.outcome,
            probe.description,
            probe.execution_hints
        );
        assert!(
            probe.description.formats.as_ref().is_some_and(|formats| {
                formats.iter().any(SevenZipFormat::supports_7z_creation)
            }),
            "installed provider does not report 7z create support"
        );
        let binding = probe
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("successful provider probe must capture its loaded DLL binding")
            .clone();
        assert_eq!(
            binding.executable.identity,
            super::super::windows_probe::test_file_hint(&executable_path).identity
        );

        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable_path,
                execution_hints: probe.execution_hints,
                version: probe.description.version,
                formats: probe.description.formats,
            }),
        ));
        let creation_lease = owner
            .lease_7z_creation(revision, &binding)
            .expect("current exact binding with create capability is admitted");

        let workspace = tempfile::tempdir().unwrap();
        let input = workspace.path().join("input");
        let nested = input.join("nested");
        let empty = input.join("empty-dir");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&empty).unwrap();
        let japanese_name = "普通の file.txt";
        let leading_dash_name = "-leading.txt";
        let at_name = "@literal.txt";
        let japanese_bytes = b"Japanese and spaces".to_vec();
        let dash_bytes = b"leading dash".to_vec();
        let at_bytes = b"leading at sign".to_vec();
        let binary_bytes = [0, 1, 2, 0xff, 0x80, b'\n'];
        fs::write(input.join(japanese_name), &japanese_bytes).unwrap();
        fs::write(input.join(leading_dash_name), &dash_bytes).unwrap();
        fs::write(input.join(at_name), &at_bytes).unwrap();
        fs::write(nested.join("payload.bin"), binary_bytes).unwrap();
        let input_sentinel = input.join("unselected-sentinel.txt");
        fs::write(&input_sentinel, b"must not be selected from cwd").unwrap();
        let sentinel = workspace.path().join("outside-cwd-sentinel.txt");
        fs::write(&sentinel, b"must not be included").unwrap();

        let archive = workspace.path().join("candidate.7z");
        let list_file = workspace.path().join("inputs.txt");
        let list_contents =
            format!("{japanese_name}\n{leading_dash_name}\n{at_name}\nnested\nempty-dir\n");
        fs::write(&list_file, list_contents.as_bytes()).unwrap();
        let create_args = [
            OsString::from("a"),
            OsString::from("-t7z"),
            OsString::from("-scsUTF-8"),
            OsString::from("-spd"),
            OsString::from("-y"),
            OsString::from(archive.as_os_str()),
            OsString::from(format!("@{}", list_file.display())),
        ];
        let create_result = run(
            creation_lease.into_operation_lease(),
            &create_args,
            &input,
            Instant::now() + Duration::from_secs(60),
            &AtomicBool::new(false),
        );
        assert_eq!(
            create_result,
            OperationOutcome::Exited { code: 0 },
            "installed 7-Zip create failed: {create_result:?}"
        );
        assert!(archive.is_file());

        let extracted = workspace.path().join("extracted");
        fs::create_dir(&extracted).unwrap();
        let extract_lease = owner
            .lease_operation_binding(revision)
            .expect("same captured provider binding remains usable for extraction");
        let extract_args = [
            OsString::from("x"),
            OsString::from("-y"),
            OsString::from(format!("-o{}", extracted.display())),
            OsString::from("--"),
            OsString::from(archive.as_os_str()),
        ];
        let extract_result = run(
            extract_lease,
            &extract_args,
            &extracted,
            Instant::now() + Duration::from_secs(60),
            &AtomicBool::new(false),
        );
        assert_eq!(
            extract_result,
            OperationOutcome::Exited { code: 0 },
            "installed 7-Zip extraction failed: {extract_result:?}"
        );

        let mut actual = std::collections::BTreeMap::new();
        collect_tree_entries(&extracted, &extracted, &mut actual);
        let expected = std::collections::BTreeMap::from([
            (japanese_name.to_owned(), Some(japanese_bytes)),
            (leading_dash_name.to_owned(), Some(dash_bytes)),
            (at_name.to_owned(), Some(at_bytes)),
            ("nested".to_owned(), None),
            ("nested/payload.bin".to_owned(), Some(binary_bytes.to_vec())),
            ("empty-dir".to_owned(), None),
        ]);
        assert_eq!(actual, expected, "archive entry set or bytes differ");
        assert!(sentinel.is_file());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires GPUI_FILER_7ZIP_TEST_EXECUTABLE to name an installed absolute 7z.exe"]
    fn installed_seven_zip_input_handle_sharing() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        use windows::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};

        let configured = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to an absolute installed 7z.exe path");
        let executable_path = std::path::PathBuf::from(configured);
        assert!(executable_path.is_absolute());
        assert!(executable_path.is_file());
        let probe_cwd = std::env::current_dir().expect("current directory is available");
        let probe = super::super::windows_probe::probe_with_description(
            &executable_path,
            &probe_cwd,
            &["i"],
            Duration::from_secs(15),
        );
        assert_eq!(
            probe.outcome,
            super::super::ProbeOutcome::Usable,
            "installed 7-Zip probe failed: outcome={:?}, description={:?}, hints={:?}",
            probe.outcome,
            probe.description,
            probe.execution_hints
        );
        assert!(
            probe.description.formats.as_ref().is_some_and(|formats| {
                formats.iter().any(SevenZipFormat::supports_7z_creation)
            }),
            "installed provider does not report 7z create support"
        );
        let binding = probe
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("probe must capture the loaded 7z.dll binding")
            .clone();
        assert_eq!(
            binding.executable.identity,
            super::super::windows_probe::test_file_hint(&executable_path).identity
        );
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable_path,
                execution_hints: probe.execution_hints,
                version: probe.description.version,
                formats: probe.description.formats,
            })
        ));

        let workspace = tempfile::tempdir().unwrap();
        let input = workspace.path().join("input");
        fs::create_dir(&input).unwrap();
        let source = input.join("ordinary.txt");
        let listfile = workspace.path().join("inputs.txt");
        fs::write(&source, b"share compatibility probe\n").unwrap();
        fs::write(&listfile, b"ordinary.txt\n").unwrap();
        let open_pin = |path: &std::path::Path, access: u32| {
            let wide_path = wide(path.as_os_str());
            Handle(unsafe {
                CreateFileW(
                    windows::core::PCWSTR(wide_path.as_ptr()),
                    access,
                    FILE_SHARE_READ,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    None,
                )
                .expect("pin fixture input handle")
            })
        };
        let create = |archive: &std::path::Path| {
            let lease = owner
                .lease_7z_creation(revision, &binding)
                .expect("exact current provider retains 7z creation capability");
            let args = [
                OsString::from("a"),
                OsString::from("-t7z"),
                OsString::from("-scsUTF-8"),
                OsString::from("-spd"),
                OsString::from("-y"),
                OsString::from(archive.as_os_str()),
                OsString::from(format!("@{}", listfile.display())),
            ];
            run(
                lease.into_operation_lease(),
                &args,
                &input,
                Instant::now() + Duration::from_secs(45),
                &AtomicBool::new(false),
            )
        };

        let incompatible_archive = workspace.path().join("incompatible.7z");
        let delete_pin = open_pin(&source, GENERIC_READ.0 | 0x0001_0000);
        let incompatible_result = create(&incompatible_archive);
        assert!(
            matches!(incompatible_result, OperationOutcome::Exited { code } if code != 0),
            "7-Zip with source pinned for delete should exit nonzero, got {incompatible_result:?}"
        );
        drop(delete_pin);
        let renamed_source = input.join("ordinary-renamed.txt");
        fs::rename(&source, &renamed_source)
            .expect("source can be renamed after delete pin release");
        fs::rename(&renamed_source, &source).unwrap();

        let source_pin = open_pin(&source, GENERIC_READ.0);
        let listfile_pin = open_pin(&listfile, GENERIC_READ.0);
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&source)
                .is_err()
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&listfile)
                .is_err()
        );
        let renamed_source = input.join("read-pinned.txt");
        let renamed_listfile = workspace.path().join("read-pinned-list.txt");
        assert!(fs::rename(&source, &renamed_source).is_err());
        assert!(fs::rename(&listfile, &renamed_listfile).is_err());

        let compatible_archive = workspace.path().join("compatible.7z");
        let compatible_result = create(&compatible_archive);
        assert_eq!(
            compatible_result,
            OperationOutcome::Exited { code: 0 },
            "read-pinned inputs should be usable by installed 7-Zip: {compatible_result:?}"
        );
        drop((source_pin, listfile_pin));
        fs::rename(&source, &renamed_source).expect("source rename succeeds after pin release");
        fs::rename(&renamed_source, &source).unwrap();
        fs::rename(&listfile, &renamed_listfile)
            .expect("listfile rename succeeds after pin release");
        fs::rename(&renamed_listfile, &listfile).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn reopen_file_delete_handoff_preserves_original_object_identity() {
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_READ_ATTRIBUTES, OPEN_EXISTING,
        };

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("input.bin");
        let renamed = directory.path().join("drained-input.bin");
        fs::write(&original, b"original object").unwrap();
        let expected_identity = super::super::windows_probe::test_file_hint(&original).identity;
        let wide_original = wide(original.as_os_str());
        // アンカーはドレイン後も元のファイルオブジェクトを保持し、同じ対象へ削除権限を再オープンする。
        let anchor = Handle(unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide_original.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .unwrap()
        });
        let pin = Handle(unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide_original.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .unwrap()
        });
        let blocked_delete = unsafe {
            ReOpenFile(
                anchor.0,
                0x0001_0000,
                FILE_SHARE_READ,
                FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        assert!(
            blocked_delete.is_err(),
            "DELETE reopen must wait for reader drain"
        );
        drop(pin);

        fs::rename(&original, &renamed).unwrap();
        fs::write(&original, b"replacement sentinel").unwrap();
        let cleanup = Handle(unsafe {
            ReOpenFile(
                anchor.0,
                0x0001_0000,
                FILE_SHARE_READ,
                FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .expect("reopen delete access on original object after read pin drains")
        });
        let mut info = windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
        unsafe {
            GetFileInformationByHandle(cleanup.0, &mut info).unwrap();
        }
        assert_eq!(
            (
                info.dwVolumeSerialNumber,
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
            ),
            (
                expected_identity.volume_serial,
                expected_identity.file_index
            ),
            "ReOpenFile must retain original file identity after path replacement"
        );
        let disposition = FILE_DISPOSITION_INFO {
            DeleteFile: windows::Win32::Foundation::BOOLEAN(1),
        };
        unsafe {
            SetFileInformationByHandle(
                cleanup.0,
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .unwrap();
        }
        drop(cleanup);
        drop(anchor);
        assert!(
            !renamed.exists(),
            "original renamed object should be deleted by handle"
        );
        assert_eq!(fs::read(&original).unwrap(), b"replacement sentinel");
    }

    #[test]
    fn static_import_is_validated_before_entry_and_then_runs() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker = directory.path().join("entry-marker");
        let lease = owner
            .lease_operation_binding(revision)
            .expect("fixture routes are leased");
        let result = run(
            lease,
            &marker_args(&marker, "operation_gate_marker"),
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(false),
        );
        assert_eq!(result, OperationOutcome::Exited { code: 0 });
        assert_eq!(fs::read(marker).unwrap(), b"entry reached");
    }

    #[test]
    fn operation_launcher_rejects_empty_relative_and_nul_working_directories() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        use std::os::windows::ffi::OsStringExt;

        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker = directory.path().join("must-not-exist");
        let mut nul_units = directory
            .path()
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        nul_units.extend([0, b'x' as u16]);
        let nul_cwd = std::path::PathBuf::from(OsString::from_wide(&nul_units));
        assert!(nul_cwd.is_absolute(), "NUL case must reach the NUL check");
        let invalid_cwds = [
            std::path::PathBuf::new(),
            std::path::PathBuf::from("relative"),
            nul_cwd,
        ];
        for cwd in invalid_cwds {
            let lease = owner.lease_operation_binding(revision).unwrap();
            let result = run(
                lease,
                &marker_args(&marker, "operation_gate_marker"),
                &cwd,
                Instant::now() + Duration::from_secs(15),
                &AtomicBool::new(false),
            );
            assert_eq!(result, OperationOutcome::LaunchFailed);
            assert!(
                !marker.exists(),
                "invalid cwd must be rejected before spawn"
            );
        }
    }

    #[test]
    fn executable_identity_mismatch_is_rejected_before_fixture_entry() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker = directory.path().join("must-not-exist");
        let mut lease = owner.lease_operation_binding(revision).unwrap();
        lease.binding.executable.identity.file_index ^= 1;
        let result = run(
            lease,
            &marker_args(&marker, "operation_gate_marker"),
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(false),
        );
        assert_eq!(result, OperationOutcome::BindingRejected);
        assert!(
            !marker.exists(),
            "unverified code must not reach the test body"
        );
    }

    #[test]
    fn dll_identity_mismatch_is_rejected_before_fixture_entry() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, false);
        let marker = directory.path().join("must-not-exist");
        let lease = owner.lease_operation_binding(revision).unwrap();
        let result = run(
            lease,
            &marker_args(&marker, "operation_gate_marker"),
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(false),
        );
        assert_eq!(result, OperationOutcome::BindingRejected);
        assert!(
            !marker.exists(),
            "unverified code must not reach the test body"
        );
    }

    #[test]
    fn missing_static_dll_is_rejected_before_fixture_entry() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(false, false);
        let marker = directory.path().join("must-not-exist");
        let lease = owner.lease_operation_binding(revision).unwrap();
        let result = run(
            lease,
            &marker_args(&marker, "operation_gate_marker"),
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result,
            OperationOutcome::BindingRejected | OperationOutcome::TimedOut
        ));
        assert!(
            !marker.exists(),
            "missing imported DLL must not enter user code"
        );
    }

    #[test]
    fn cancellation_before_resume_keeps_fixture_entry_unreached() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker = directory.path().join("must-not-exist");
        let lease = owner.lease_operation_binding(revision).unwrap();
        let result = run(
            lease,
            &marker_args(&marker, "operation_gate_marker"),
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(15),
            &AtomicBool::new(true),
        );
        assert_eq!(result, OperationOutcome::Cancelled);
        assert!(!marker.exists());
    }

    #[test]
    fn cancellation_waits_for_descendant_exit_before_returning() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker_path = directory.path().join("descendant-pid");
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_worker = cancelled.clone();
        let lease = owner.lease_operation_binding(revision).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let worker_marker = marker_path.clone();
        let worker = thread::spawn(move || {
            run(
                lease,
                &marker_args(&worker_marker, "descendant"),
                &cwd,
                Instant::now() + Duration::from_secs(20),
                &cancel_worker,
            )
        });
        let marker_deadline = Instant::now() + Duration::from_secs(10);
        while !marker_path.exists() && Instant::now() < marker_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker_path.exists(),
            "fixture descendant must be running first"
        );
        let executable = directory.path().join("7z.exe");
        let dll = directory.path().join("7z.dll");
        assert!(
            fs::rename(&executable, directory.path().join("leased-executable.exe")).is_err(),
            "consumed operation lease must pin the executable until cleanup"
        );
        assert!(
            fs::rename(&dll, directory.path().join("leased-7z.dll")).is_err(),
            "consumed operation lease must pin the DLL until cleanup"
        );
        cancelled.store(true, Ordering::Release);
        assert_eq!(worker.join().unwrap(), OperationOutcome::Cancelled);
        assert_descendant_stopped(fs::read_to_string(marker_path).unwrap().parse().unwrap());
    }

    #[test]
    fn helper_bundle_lives_through_cancelled_descendant_job_drain() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        use windows::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

        let (directory, owner, revision) = static_import_fixture(true, true);
        let executable = directory.path().join("7z.exe");
        let dll = directory.path().join("7z.dll");
        {
            let mut data = owner.0.lock().unwrap();
            let ArchiveProviderState::Available(executable) = &mut data.snapshot.state else {
                unreachable!();
            };
            executable.formats = Some(vec![SevenZipFormat {
                name: "7z".into(),
                flags: "C........".into(),
            }]);
        }
        let binding = match owner.snapshot().state {
            ArchiveProviderState::Available(executable) => executable
                .execution_hints
                .operation_binding
                .expect("fixture binding"),
            _ => unreachable!(),
        };
        let provider = owner.lease_7z_creation(revision, &binding).unwrap();
        let image =
            super::super::super::application_image::lease_explicit_path_for_test(&executable)
                .unwrap();

        let output_path = directory.path().join("candidate.7z");
        let source_path = directory.path().join("source.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_path, b"held source").unwrap();
        let output = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let source = fs::OpenOptions::new()
            .read(true)
            .open(&source_path)
            .unwrap();
        let entries = [crate::archive_provider::SevenZipHelperEntry {
            name: "source.bin".into(),
            directory: false,
            expected_length: 11,
            source: Some(&source),
        }];
        let launch =
            super::super::super::build_seven_zip_helper_launch(provider, image, &output, &entries)
                .unwrap();
        let inherited = launch.inherited_handles();
        let expected_mapping = launch.descriptor_for_test().unwrap();
        let expected_output_identity =
            file_identity_for_test(HANDLE(output.as_raw_handle().cast()));
        let expected_source_identity =
            file_identity_for_test(HANDLE(source.as_raw_handle().cast()));
        assert_eq!(expected_mapping.handles.len(), 2);
        assert_eq!(expected_mapping.entries[0].expected_length, 11);

        let marker_path = directory.path().join("helper-descendant-pid");
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        let worker_marker = marker_path.clone();
        let inherited_raw = inherited
            .iter()
            .map(|handle| handle.0 as usize)
            .collect::<Vec<_>>();
        let cwd = std::env::current_dir().unwrap();
        let worker = thread::spawn(move || {
            let inherited = inherited_raw
                .into_iter()
                .map(|raw| HANDLE(raw as *mut _))
                .collect::<Vec<_>>();
            run_with_handles(
                &LaunchAdmission::Helper(launch),
                &marker_args(&worker_marker, "descendant"),
                &cwd,
                Instant::now() + Duration::from_secs(20),
                &worker_cancel,
                &inherited,
            )
        });
        let marker_deadline = Instant::now() + Duration::from_secs(10);
        while !marker_path.exists() && Instant::now() < marker_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker_path.exists(),
            "the helper test descendant must be running"
        );
        let mapping_raw = inherited[0];
        let mapping_descriptor = unsafe {
            let borrowed = std::os::windows::io::BorrowedHandle::borrow_raw(mapping_raw.0);
            super::super::super::seven_zip_helper_protocol::read_mapping(borrowed).unwrap()
        };
        assert_eq!(mapping_descriptor, expected_mapping);
        assert_eq!(
            file_identity_for_test(inherited[1]),
            expected_output_identity
        );
        assert_eq!(
            file_identity_for_test(inherited[2]),
            expected_source_identity
        );
        for handle in &inherited {
            let mut flags = 0u32;
            assert!(unsafe { GetHandleInformation(*handle, &mut flags) }.is_ok());
            assert_ne!(flags & HANDLE_FLAG_INHERIT.0, 0);
        }
        assert!(
            fs::rename(&executable, directory.path().join("leased-app.exe")).is_err(),
            "application and provider routes remain pinned while the owned launch is in flight"
        );
        assert!(
            fs::rename(&dll, directory.path().join("leased-dll.dll")).is_err(),
            "provider DLL route remains pinned while the owned launch is in flight"
        );
        cancelled.store(true, Ordering::Release);
        assert_eq!(worker.join().unwrap(), OperationOutcome::Cancelled);
        assert_descendant_stopped(fs::read_to_string(marker_path).unwrap().parse().unwrap());
        for handle in inherited {
            let mut flags = 0u32;
            assert!(unsafe { GetHandleInformation(handle, &mut flags) }.is_err());
        }
        assert!(output.metadata().is_ok());
        assert_eq!(source.metadata().unwrap().len(), 11);
        assert_eq!(fs::read(&source_path).unwrap(), b"held source");
    }

    #[test]
    fn timeout_waits_for_descendant_exit_before_returning() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let (directory, owner, revision) = static_import_fixture(true, true);
        let marker = directory.path().join("descendant-pid");
        let lease = owner.lease_operation_binding(revision).unwrap();
        let started = Instant::now();
        let result = run(
            lease,
            &marker_args(&marker, "descendant"),
            &std::env::current_dir().unwrap(),
            started + Duration::from_secs(2),
            &AtomicBool::new(false),
        );
        assert_eq!(result, OperationOutcome::TimedOut);
        assert!(started.elapsed() >= Duration::from_secs(1));
        let child_pid = fs::read_to_string(marker).unwrap().parse().unwrap();
        assert_descendant_stopped(child_pid);
    }

    #[test]
    fn root_match_rejects_ambiguous_or_descendant_only_dll_evidence() {
        let image = ArchiveFileHint {
            path: PathBuf::from("root.exe"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 2,
            },
        };
        let dll = ArchiveFileHint {
            path: PathBuf::from("7z.dll"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 3,
            },
        };
        let binding = ArchiveOperationBinding {
            executable: image.clone(),
            zip_dll: dll.clone(),
        };
        let root = 10;
        let mut evidence = DebugEvidence::default();
        evidence.images.insert(root, Some(image.clone()));
        assert!(matches_root_startup_binding(&evidence, &binding, root));
        assert!(!matches_root_binding(&evidence, &binding, root));
        evidence.zip_dlls.insert(root + 1, vec![dll.clone()]);
        assert!(matches_root_startup_binding(&evidence, &binding, root));
        assert!(!matches_root_binding(&evidence, &binding, root));
        evidence
            .zip_dlls
            .insert(root, vec![dll.clone(), dll.clone()]);
        assert!(!matches_root_startup_binding(&evidence, &binding, root));
        assert!(!matches_root_binding(&evidence, &binding, root));
        evidence.ambiguous_processes.insert(root);
        assert!(!matches_root_startup_binding(&evidence, &binding, root));
        assert!(!matches_root_binding(&evidence, &binding, root));
    }

    #[test]
    fn application_root_is_independent_from_provider_cli_root() {
        let application_root = ArchiveFileHint {
            path: PathBuf::from("application.exe"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 10,
            },
        };
        let provider_root = ArchiveFileHint {
            path: PathBuf::from("provider-7z.exe"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 20,
            },
        };
        let dll = ArchiveFileHint {
            path: PathBuf::from("7z.dll"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 30,
            },
        };
        let mut evidence = DebugEvidence::default();
        evidence.images.insert(7, Some(application_root.clone()));
        assert!(matches_root_startup_expected(
            &evidence,
            &application_root,
            &dll,
            7
        ));
        assert!(!matches_root_startup_expected(
            &evidence,
            &provider_root,
            &dll,
            7
        ));
        evidence.zip_dlls.insert(7, vec![dll.clone()]);
        assert!(matches_root_expected(&evidence, &application_root, &dll, 7));
        assert!(!matches_root_expected(&evidence, &provider_root, &dll, 7));
        let wrong_dll = ArchiveFileHint {
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 31,
            },
            ..dll.clone()
        };
        assert!(!matches_root_expected(
            &evidence,
            &application_root,
            &wrong_dll,
            7
        ));
    }

    #[test]
    #[ignore = "requires GPUI_FILER_7ZIP_TEST_EXECUTABLE pointing to an installed 7z.exe"]
    fn installed_provider_launches_test_executable_as_application_root() {
        let _probe_test_guard = super::super::super::lock_probe_tests();
        let executable = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to an installed 7z.exe");
        let executable = PathBuf::from(executable);
        let search = executable.parent().unwrap().as_os_str().to_owned();
        let provider = discover_from_path(Some(search), std::env::current_dir().unwrap())
            .expect("installed provider should be discoverable");
        assert_eq!(provider.path, executable);
        let binding = provider
            .execution_hints
            .operation_binding
            .clone()
            .expect("provider probe should observe its executable and DLL");
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(revision, Some(provider)));
        let provider_lease = owner
            .lease_7z_creation(revision, &binding)
            .expect("installed 7-Zip provider should have the create capability");

        let test_executable = std::env::current_exe().unwrap();
        assert_ne!(
            test_executable, executable,
            "helper root differs from CLI root"
        );
        application_image::initialize();
        let image = application_image::lease_for_operation()
            .expect("test executable image should have been captured");
        let args = vec![
            OsString::from("--exact"),
            OsString::from("archive_provider::tests::seven_zip_child_fixture"),
            OsString::from("--nocapture"),
            OsString::from("--"),
            OsString::from("--fixture-mode=success_with_dll"),
            OsString::from(format!("--fixture-dll={}", binding.zip_dll.path.display())),
        ];
        let outcome = run_application_for_test(
            provider_lease,
            image,
            &args,
            &std::env::current_dir().unwrap(),
            Instant::now() + Duration::from_secs(20),
            &AtomicBool::new(false),
        );
        assert_eq!(outcome, OperationOutcome::Exited { code: 0 });
    }

    pub(super) fn assert_descendant_stopped(child_pid: u32) {
        assert_ne!(child_pid, 0);
        let child = unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_SYNCHRONIZE
                    | windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                child_pid,
            )
        };
        match child {
            Ok(child) => {
                assert_eq!(
                    unsafe { windows::Win32::System::Threading::WaitForSingleObject(child, 0) },
                    windows::Win32::Foundation::WAIT_OBJECT_0,
                    "adapter must not return while its child remains alive"
                );
                unsafe { windows::Win32::Foundation::CloseHandle(child).unwrap() };
            }
            Err(error) if error.code().0 as u32 == 0x8007_0057 => {
                // Windows reports ERROR_INVALID_PARAMETER when the PID exited.
            }
            Err(error) => panic!("could not confirm descendant exit: {error}"),
        }
    }
}
