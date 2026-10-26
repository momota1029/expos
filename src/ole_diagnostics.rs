//! Optional, path-free observer for one local OLE drag investigation.

use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const ENV_PATH: &str = "GPUI_FILER_OLE_TRACE_PATH";
static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetClass {
    Folder,
    Background,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authorization {
    Authorized,
    Rejected,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropEffect {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinPolicy {
    CopyOnly,
    MoveCapable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureHandoffStep {
    ReleaseRequested,
    NotCurrentBeforeRelease,
    ReleaseRejected,
    Released,
    TouchPromotionRequested,
    TouchPromotionRejected,
    ReadyForOle,
    NotReadyAfterRelease,
    OleStartRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OleResult {
    Dropped,
    Cancelled,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Accepted,
    Rejected,
    Started,
    Succeeded,
    Partial,
    Cancelled,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionRejection {
    CandidateIneligible,
    CapacityReached,
    IdExhausted,
    IdentityUnavailable,
    PinDrainReservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    FileDrop {
        local: bool,
        source_count: usize,
        target: TargetClass,
    },
    NativeCandidate {
        token: u64,
        outcome: Outcome,
    },
    CaptureHandoff {
        token: u64,
        step: CaptureHandoffStep,
    },
    Pin {
        token: u64,
        outcome: Outcome,
    },
    Admission {
        native_token: u64,
        operation: Option<(u64, u64)>,
        outcome: Outcome,
        target_effect: Option<DropEffect>,
        rejection: Option<AdmissionRejection>,
    },
    OleTerminal {
        token: u64,
        operation: Option<(u64, u64)>,
        result: OleResult,
        returned_effect: Option<u32>,
        shell_performed_effect: Option<u32>,
        pin_policy: Option<PinPolicy>,
        authorization: Authorization,
    },
    HeldCancel {
        job: u64,
        attempt: u64,
        result: bool,
    },
    PinDrain {
        token: u64,
        deferred_count: usize,
    },
    HeldRelease {
        job: u64,
        attempt: u64,
        result: bool,
    },
    Worker {
        job: u64,
        attempt: u64,
        outcome: Outcome,
    },
}

struct Trace {
    tx: Sender<RecordedEvent>,
    started: Instant,
}

struct RecordedEvent {
    event: Event,
    elapsed_us: u128,
    unix_us: u128,
}

pub(crate) fn initialize() -> InitStatus {
    TRACE.get_or_init(|| {
        let path = std::env::var_os(ENV_PATH)?;
        let file = match create_new(&path) {
            Ok(file) => file,
            Err(_) => return None,
        };
        let (tx, rx) = mpsc::channel::<RecordedEvent>();
        let session = session_identity();
        let started = Instant::now();
        let initialized_unix_us = unix_time_us();
        thread::Builder::new()
            .name("gpui-filer-ole-trace".into())
            .spawn(move || write_events(file, rx, session, initialized_unix_us))
            .ok()?;
        Some(Trace { tx, started })
    });
    match TRACE.get() {
        Some(Some(_)) => InitStatus::Enabled,
        Some(None) => {
            if std::env::var_os(ENV_PATH).is_some() {
                InitStatus::Failed
            } else {
                InitStatus::Disabled
            }
        }
        None => InitStatus::Failed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitStatus {
    Enabled,
    Disabled,
    Failed,
}

pub(crate) fn record(event: Event) {
    if let Some(Some(trace)) = TRACE.get() {
        let _ = trace.tx.send(RecordedEvent {
            event,
            elapsed_us: trace.started.elapsed().as_micros(),
            unix_us: unix_time_us(),
        });
    }
}

fn unix_time_us() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

fn create_new(path: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(Path::new(path))
}

fn session_identity() -> u128 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    elapsed ^ (u128::from(std::process::id()) << 64)
}

fn write_events(
    mut file: std::fs::File,
    rx: mpsc::Receiver<RecordedEvent>,
    session: u128,
    initialized_unix_us: u128,
) {
    let sequence = AtomicU64::new(0);
    let _ = write_line(
        &mut file,
        session,
        &sequence,
        0,
        initialized_unix_us,
        "{\"kind\":\"initialized\"}",
    );
    while let Ok(recorded) = rx.recv() {
        let _ = write_line(
            &mut file,
            session,
            &sequence,
            recorded.elapsed_us,
            recorded.unix_us,
            &serialize(recorded.event),
        );
    }
}

fn write_line(
    file: &mut impl Write,
    session: u128,
    sequence: &AtomicU64,
    elapsed_us: u128,
    unix_us: u128,
    event: &str,
) -> std::io::Result<()> {
    let seq = sequence.fetch_add(1, Ordering::Relaxed);
    writeln!(
        file,
        "{{\"session\":\"{session:032x}\",\"seq\":{seq},\"elapsed_us\":{elapsed_us},\"unix_us\":{unix_us},\"event\":{event}}}"
    )?;
    file.flush()
}

fn serialize(event: Event) -> String {
    match event {
        Event::FileDrop {
            local,
            source_count,
            target,
        } => format!(
            "{{\"kind\":\"file_drop\",\"local\":{local},\"source_count\":{source_count},\"target\":\"{}\"}}",
            target.as_str()
        ),
        Event::NativeCandidate { token, outcome } => format!(
            "{{\"kind\":\"native_candidate\",\"token\":{token},\"outcome\":\"{}\"}}",
            outcome.as_str()
        ),
        Event::CaptureHandoff { token, step } => format!(
            "{{\"kind\":\"capture_handoff\",\"token\":{token},\"step\":\"{}\"}}",
            step.as_str()
        ),
        Event::Pin { token, outcome } => format!(
            "{{\"kind\":\"pin\",\"token\":{token},\"outcome\":\"{}\"}}",
            outcome.as_str()
        ),
        Event::Admission {
            native_token,
            operation,
            outcome,
            target_effect,
            rejection,
        } => format!(
            "{{\"kind\":\"admission\",\"native_token\":{native_token},\"operation\":{},\"outcome\":\"{}\",\"target_effect\":{},\"rejection\":{}}}",
            operation.map_or_else(
                || "null".into(),
                |(job, attempt)| format!("{{\"job\":{job},\"attempt\":{attempt}}}")
            ),
            outcome.as_str(),
            target_effect.map_or_else(|| "null".into(), |v| format!("\"{}\"", v.as_str())),
            rejection.map_or_else(|| "null".into(), |value| format!("\"{}\"", value.as_str()))
        ),
        Event::OleTerminal {
            token,
            operation,
            result,
            returned_effect,
            shell_performed_effect,
            pin_policy,
            authorization,
        } => format!(
            "{{\"kind\":\"ole_terminal\",\"token\":{token},\"operation\":{},\"result\":\"{}\",\"returned_effect\":{},\"shell_performed_effect\":{},\"pin_policy\":{},\"authorization\":\"{}\"}}",
            operation.map_or_else(
                || "null".into(),
                |(job, attempt)| format!("{{\"job\":{job},\"attempt\":{attempt}}}")
            ),
            result.as_str(),
            returned_effect.map_or_else(|| "null".into(), |v| v.to_string()),
            shell_performed_effect.map_or_else(|| "null".into(), |v| v.to_string()),
            pin_policy.map_or_else(|| "null".into(), |v| format!("\"{}\"", v.as_str())),
            authorization.as_str()
        ),
        Event::HeldCancel {
            job,
            attempt,
            result,
        } => format!(
            "{{\"kind\":\"held_cancel\",\"job\":{job},\"attempt\":{attempt},\"result\":{result}}}"
        ),
        Event::PinDrain {
            token,
            deferred_count,
        } => format!(
            "{{\"kind\":\"pin_drain\",\"token\":{token},\"deferred_count\":{deferred_count}}}"
        ),
        Event::HeldRelease {
            job,
            attempt,
            result,
        } => format!(
            "{{\"kind\":\"held_release\",\"job\":{job},\"attempt\":{attempt},\"result\":{result}}}"
        ),
        Event::Worker {
            job,
            attempt,
            outcome,
        } => format!(
            "{{\"kind\":\"worker\",\"job\":{job},\"attempt\":{attempt},\"outcome\":\"{}\"}}",
            outcome.as_str()
        ),
    }
}

impl TargetClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Background => "background",
            Self::Missing => "missing",
        }
    }
}
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

impl AdmissionRejection {
    fn as_str(self) -> &'static str {
        match self {
            Self::CandidateIneligible => "candidate_ineligible",
            Self::CapacityReached => "capacity_reached",
            Self::IdExhausted => "id_exhausted",
            Self::IdentityUnavailable => "identity_unavailable",
            Self::PinDrainReservation => "pin_drain_reservation",
        }
    }
}

impl DropEffect {
    fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }
}

impl PinPolicy {
    fn as_str(self) -> &'static str {
        match self {
            Self::CopyOnly => "copy_only",
            Self::MoveCapable => "move_capable",
        }
    }
}

impl CaptureHandoffStep {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseRequested => "release_requested",
            Self::NotCurrentBeforeRelease => "not_current_before_release",
            Self::ReleaseRejected => "release_rejected",
            Self::Released => "released",
            Self::TouchPromotionRequested => "touch_promotion_requested",
            Self::TouchPromotionRejected => "touch_promotion_rejected",
            Self::ReadyForOle => "ready_for_ole",
            Self::NotReadyAfterRelease => "not_ready_after_release",
            Self::OleStartRequested => "ole_start_requested",
        }
    }
}
impl Authorization {
    fn as_str(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
            Self::Rejected => "rejected",
            Self::NotApplicable => "not_applicable",
        }
    }
}

impl OleResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_is_stable_and_contains_no_path_data() {
        let event = Event::FileDrop {
            local: true,
            source_count: 2,
            target: TargetClass::Folder,
        };
        assert_eq!(
            serialize(event),
            "{\"kind\":\"file_drop\",\"local\":true,\"source_count\":2,\"target\":\"folder\"}"
        );
        let line = serialize(event);
        assert!(!line.contains('\\'));
        assert!(!line.contains(":/"));
    }

    #[test]
    fn admission_serialization_correlates_attempt_and_static_rejection() {
        let line = serialize(Event::Admission {
            native_token: 9,
            operation: Some((3, 4)),
            outcome: Outcome::Rejected,
            target_effect: None,
            rejection: Some(AdmissionRejection::PinDrainReservation),
        });

        assert_eq!(
            line,
            "{\"kind\":\"admission\",\"native_token\":9,\"operation\":{\"job\":3,\"attempt\":4},\"outcome\":\"rejected\",\"target_effect\":null,\"rejection\":\"pin_drain_reservation\"}"
        );
    }

    #[test]
    fn trace_lines_are_valid_json_with_ordered_metadata() {
        let mut bytes = Vec::new();
        let sequence = AtomicU64::new(0);
        let started = Instant::now();
        let unix_us = unix_time_us();
        write_line(
            &mut bytes,
            17,
            &sequence,
            0,
            unix_us,
            "{\"kind\":\"initialized\"}",
        )
        .unwrap();
        write_line(
            &mut bytes,
            17,
            &sequence,
            started.elapsed().as_micros(),
            unix_time_us(),
            &serialize(Event::FileDrop {
                local: true,
                source_count: 1,
                target: TargetClass::Folder,
            }),
        )
        .unwrap();

        let lines = String::from_utf8(bytes).unwrap();
        let records = lines
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(records[0]["event"]["kind"], "initialized");
        assert_eq!(records[0]["seq"], 0);
        assert_eq!(records[1]["event"]["kind"], "file_drop");
        assert_eq!(records[1]["seq"], 1);
        let first_elapsed = records[0]["elapsed_us"].as_u64().unwrap();
        let second_elapsed = records[1]["elapsed_us"].as_u64().unwrap();
        assert!(second_elapsed >= first_elapsed);
        let first_unix = records[0]["unix_us"].as_u64().unwrap();
        let second_unix = records[1]["unix_us"].as_u64().unwrap();
        assert!(second_unix >= first_unix);
    }

    #[test]
    fn absent_environment_disables_the_writer() {
        assert_eq!(initialize(), InitStatus::Disabled);
        assert!(matches!(TRACE.get(), Some(None)));
    }
}
