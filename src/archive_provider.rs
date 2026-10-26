//! Optional external archive-tool discovery.
//!
//! Built-in ZIP support does not depend on this state. A 7-Zip provider is
//! reported only after the exact PATH result completes its harmless `i` probe.

use gpui::Global;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(test)]
static PROBE_TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(super) fn lock_probe_tests() -> std::sync::MutexGuard<'static, ()> {
    PROBE_TEST_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// 次の 7-Zip worker slice まで、この検証 API の呼び出し元はまだ存在しない。
#[allow(dead_code)]
mod seven_zip_extract_plan;
#[cfg(test)]
pub(crate) use seven_zip_extract_plan::IndexedRecord;
pub(crate) use seven_zip_extract_plan::Validated7zExtractPlan;

#[cfg(windows)]
#[path = "archive_provider/application_image.rs"]
pub(crate) mod application_image;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_decode.rs"]
mod seven_zip_decode;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_decode_protocol.rs"]
mod seven_zip_decode_protocol;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_helper.rs"]
mod seven_zip_helper;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_helper_protocol.rs"]
mod seven_zip_helper_protocol;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_inspection_protocol.rs"]
mod seven_zip_inspection_protocol;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_reader.rs"]
mod seven_zip_reader;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_reader_protocol.rs"]
mod seven_zip_reader_protocol;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_reader_sdk.rs"]
mod seven_zip_reader_sdk;
#[cfg(windows)]
#[path = "archive_provider/seven_zip_sdk.rs"]
mod seven_zip_sdk;

#[cfg(windows)]
pub(crate) fn run_seven_zip_helper_from_process_args() -> Option<i32> {
    if let Some(result) = seven_zip_decode::run_decode_from_process_args() {
        return Some(result);
    }
    if let Some(result) = seven_zip_reader::run_inspection_from_process_args() {
        return Some(result);
    }
    if let Some(result) = seven_zip_reader::run_reader_from_process_args() {
        return Some(result);
    }
    seven_zip_helper::run_helper_from_process_args()
}

#[cfg(windows)]
pub(crate) use seven_zip_helper::{SevenZipHelperEntry, SevenZipHelperLaunch};

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SevenZipHelperRunOutcome {
    Exited { code: u32 },
    Cancelled,
    TimedOut,
    LaunchFailed,
    BindingRejected,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SevenZipDecodeFailure {
    Cancelled,
    Failed,
}

#[cfg(windows)]
pub(crate) fn build_seven_zip_helper_launch(
    provider: Archive7zCreationLease,
    image: application_image::ApplicationImageLease,
    output: &std::fs::File,
    entries: &[SevenZipHelperEntry<'_>],
) -> Option<SevenZipHelperLaunch> {
    seven_zip_helper::build_helper_launch(provider, image, output, entries)
}

#[cfg(windows)]
pub(crate) fn run_seven_zip_helper_launch(
    launch: seven_zip_helper::SevenZipHelperLaunch,
    cwd: &Path,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> SevenZipHelperRunOutcome {
    windows_probe::run_helper_launch(launch, cwd, deadline, cancelled)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SevenZipFormat {
    pub(crate) name: String,
    /// 7-Zip reports per-format capabilities in this token; keep the raw value for diagnostics.
    pub(crate) flags: String,
}

impl SevenZipFormat {
    /// Report create support only for the exact 7z format and its documented leading `C` flag.
    /// This is descriptive metadata; execution still requires a captured operation binding.
    pub(crate) fn supports_7z_creation(&self) -> bool {
        self.name == "7z" && self.flags.starts_with('C')
    }

    pub(crate) fn has_valid_flags(&self) -> bool {
        valid_format_flags(&self.flags)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SevenZipExecutable {
    pub(crate) path: PathBuf,
    /// Candidate facts observed for diagnostics and later revalidation only.
    /// They are deliberately not an execution authorization.
    pub(crate) execution_hints: ArchiveExecutionHints,
    /// Descriptive details remain optional and never gate executable readiness.
    pub(crate) version: Option<String>,
    /// Format records reported by `7z i`; absent means capture or parsing was incomplete.
    pub(crate) formats: Option<Vec<SevenZipFormat>>,
}

/// Discovery evidence for the candidate and the image actually observed by the
/// probe. A process-load binding identifies what that probe launched and loaded;
/// later operations still need to revalidate the provider revision and pin both
/// file routes before treating it as execution authorization.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ArchiveExecutionHints {
    pub(crate) candidate_executable: Option<ArchiveFileHint>,
    pub(crate) loaded_zip_dll: Option<ArchiveFileHint>,
    pub(crate) operation_binding: Option<ArchiveOperationBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveFileHint {
    pub(crate) path: PathBuf,
    pub(crate) identity: ArchiveFileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveFileIdentity {
    pub(crate) volume_serial: u32,
    pub(crate) file_index: u64,
}

/// Process-load evidence tying one launched executable identity to one loaded
/// `7z.dll` identity in the same probe tree. This is not a durable route pin or
/// authorization for a later operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveOperationBinding {
    pub(crate) executable: ArchiveFileHint,
    pub(crate) zip_dll: ArchiveFileHint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveProviderState {
    Unknown,
    Discovering,
    Unavailable,
    Available(SevenZipExecutable),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveProviderSnapshot {
    pub(crate) revision: u64,
    pub(crate) state: ArchiveProviderState,
}

impl ArchiveProviderSnapshot {
    /// Immutable UI and worker admission share the same read-capability predicate.
    pub(crate) fn seven_zip_read_binding(
        &self,
    ) -> Option<(&SevenZipFormat, &ArchiveOperationBinding)> {
        let ArchiveProviderState::Available(executable) = &self.state else {
            return None;
        };
        let formats = executable.formats.as_ref()?;
        let mut matches = formats.iter().filter(|format| format.name == "7z");
        let format = matches.next()?;
        if matches.next().is_some() || !format.has_valid_flags() {
            return None;
        }
        Some((
            format,
            executable.execution_hints.operation_binding.as_ref()?,
        ))
    }
}

#[derive(Debug)]
struct ArchiveProviderData {
    snapshot: ArchiveProviderSnapshot,
}

/// The single app-wide authority for optional 7-Zip availability.
#[derive(Clone, Debug)]
pub(crate) struct ArchiveProviderOwner(Arc<Mutex<ArchiveProviderData>>);

impl Global for ArchiveProviderOwner {}

impl Default for ArchiveProviderOwner {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(ArchiveProviderData {
            snapshot: ArchiveProviderSnapshot {
                revision: 0,
                state: ArchiveProviderState::Unknown,
            },
        })))
    }
}

impl ArchiveProviderOwner {
    pub(crate) fn snapshot(&self) -> ArchiveProviderSnapshot {
        self.0
            .lock()
            .expect("archive provider state poisoned")
            .snapshot
            .clone()
    }

    pub(crate) fn begin_discovery(&self) -> u64 {
        let mut data = self.0.lock().expect("archive provider state poisoned");
        let revision = data
            .snapshot
            .revision
            .checked_add(1)
            .expect("archive provider generation exhausted");
        data.snapshot = ArchiveProviderSnapshot {
            revision,
            state: ArchiveProviderState::Discovering,
        };
        revision
    }

    /// 旧CLI経路の回帰テスト用。productionの7-Zip operationはapplication-image leaseを使う。
    #[cfg(windows)]
    #[cfg(test)]
    pub(crate) fn lease_operation_binding(&self, revision: u64) -> Option<ArchiveOperationLease> {
        windows_probe::lease_operation_binding(self, revision, || {})
    }

    /// Admit an archive creation attempt only for the currently captured 7z creator.
    /// This blocking method belongs on a detached worker.
    #[cfg(windows)]
    pub(crate) fn lease_7z_creation(
        &self,
        revision: u64,
        expected_binding: &ArchiveOperationBinding,
    ) -> Option<Archive7zCreationLease> {
        windows_probe::lease_7z_creation_binding(self, revision, expected_binding, || {})
    }

    #[cfg(windows)]
    pub(crate) fn capture_7z_read_capability(&self) -> Option<Archive7zReadCapability> {
        let data = self.0.lock().expect("archive provider state poisoned");
        let snapshot = &data.snapshot;
        let (format, binding) = snapshot.seven_zip_read_binding()?;
        Some(Archive7zReadCapability {
            revision: snapshot.revision,
            binding: binding.clone(),
            format: format.clone(),
        })
    }

    /// provider mutex を保持したまま、読取 capability が current な間に session transition を
    /// linearize する。callback は値の move だけに限定し、重い処理や provider 再入をしない。
    #[cfg(windows)]
    pub(crate) fn with_current_7z_read<R>(
        &self,
        capability: &Archive7zReadCapability,
        commit: impl FnOnce() -> R,
    ) -> Option<R> {
        let data = self.0.lock().expect("archive provider state poisoned");
        let current = &data.snapshot;
        if current.revision != capability.revision {
            return None;
        }
        let ArchiveProviderState::Available(executable) = &current.state else {
            return None;
        };
        let Some(binding) = executable.execution_hints.operation_binding.as_ref() else {
            return None;
        };
        let Some(formats) = executable.formats.as_ref() else {
            return None;
        };
        let mut matches = formats.iter().filter(|format| format.name == "7z");
        let Some(format) = matches.next() else {
            return None;
        };
        if matches.next().is_some()
            || binding != &capability.binding
            || format != &capability.format
            || !format.has_valid_flags()
        {
            return None;
        }
        Some(commit())
    }

    #[cfg(windows)]
    pub(crate) fn lease_7z_read(
        &self,
        capability: &Archive7zReadCapability,
    ) -> Option<Archive7zReadLease> {
        windows_probe::lease_7z_read(self, capability, || {})
    }

    #[cfg(windows)]
    pub(crate) fn lease_7z_extraction(
        &self,
        capability: &Archive7zReadCapability,
        image: application_image::ApplicationImageLease,
    ) -> Option<Archive7zExtractionLease> {
        Some(Archive7zExtractionLease(std::sync::Arc::new(
            Archive7zExtractionLeaseInner {
                provider: self.lease_7z_read(capability)?,
                image,
            },
        )))
    }

    #[cfg(windows)]
    pub(crate) fn admit_7z_extraction(
        &self,
        revision: u64,
        expected_binding: &ArchiveOperationBinding,
        image: application_image::ApplicationImageLease,
    ) -> Option<Archive7zExtractionLease> {
        let capability = self.capture_7z_read_capability()?;
        if capability.revision != revision || &capability.binding != expected_binding {
            return None;
        }
        self.lease_7z_extraction(&capability, image)
    }

    #[cfg(windows)]
    pub(crate) fn build_7z_read_launch(
        &self,
        capability: &Archive7zReadCapability,
        image: application_image::ApplicationImageLease,
        source: &std::fs::File,
    ) -> Option<seven_zip_reader::SevenZipReadLaunch> {
        let provider = self.lease_7z_read(capability)?;
        seven_zip_reader::build_read_launch(provider, image, source)
    }

    #[cfg(windows)]
    pub(crate) fn read_7z_archive(
        &self,
        capability: &Archive7zReadCapability,
        image: application_image::ApplicationImageLease,
        source: &std::fs::File,
        cwd: &Path,
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<
        crate::archive_manifest::ArchiveManifest,
        windows_probe::operation_launcher::OperationOutcome,
    > {
        let launch = self
            .build_7z_read_launch(capability, image, source)
            .ok_or(windows_probe::operation_launcher::OperationOutcome::LaunchFailed)?;
        windows_probe::operation_launcher::run_seven_zip_reader(launch, cwd, deadline, cancelled)
    }

    #[cfg(windows)]
    pub(crate) fn inspect_7z_archive(
        &self,
        lease: &Archive7zExtractionLease,
        source: &std::fs::File,
        cwd: &Path,
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<
        seven_zip_extract_plan::Validated7zExtractPlan,
        windows_probe::operation_launcher::OperationOutcome,
    > {
        let launch = seven_zip_reader::build_inspection_launch(lease.clone(), source)
            .ok_or(windows_probe::operation_launcher::OperationOutcome::LaunchFailed)?;
        windows_probe::operation_launcher::run_seven_zip_inspection(
            launch, cwd, deadline, cancelled,
        )
    }

    #[cfg(windows)]
    pub(crate) fn decode_7z_archive(
        &self,
        lease: Archive7zExtractionLease,
        source: &std::fs::File,
        spool: &mut std::fs::File,
        plan: std::sync::Arc<seven_zip_extract_plan::Validated7zExtractPlan>,
        cwd: &Path,
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), SevenZipDecodeFailure> {
        seven_zip_decode::run_parent(lease, source, spool, plan, cwd, deadline, cancelled)
            .map(|_| ())
            .map_err(windows_probe::decode_failure)
    }

    pub(crate) fn seven_zip_extract_plan_digest(
        plan: &seven_zip_extract_plan::Validated7zExtractPlan,
    ) -> Result<[u8; 32], ()> {
        #[cfg(windows)]
        { seven_zip_decode_protocol::plan_digest(plan) }
        #[cfg(not(windows))]
        { let _ = plan; Err(()) }
    }

    pub(crate) fn commit_discovery(
        &self,
        revision: u64,
        executable: Option<SevenZipExecutable>,
    ) -> bool {
        let mut data = self.0.lock().expect("archive provider state poisoned");
        if data.snapshot.revision != revision
            || data.snapshot.state != ArchiveProviderState::Discovering
        {
            return false;
        }
        data.snapshot.state = executable.map_or(
            ArchiveProviderState::Unavailable,
            ArchiveProviderState::Available,
        );
        true
    }
}

/// Holds both no-follow route leases for one accepted operation attempt.
/// Keeping this type and its handles opaque prevents callers from substituting
/// a different route after identity validation.
#[cfg(windows)]
pub(crate) struct ArchiveOperationLease {
    #[cfg(test)]
    revision: u64,
    binding: ArchiveOperationBinding,
    _executable: windows_probe::Lease,
    _zip_dll: windows_probe::Lease,
}

/// Opaque admission token for a 7z creation attempt.
#[cfg(windows)]
pub(crate) struct Archive7zCreationLease(ArchiveOperationLease);

/// Immutable read authorization captured from one exact provider revision.
#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Archive7zReadCapability {
    revision: u64,
    binding: ArchiveOperationBinding,
    format: SevenZipFormat,
}

/// Opaque read-purpose route lease, distinct from creation admission.
#[cfg(windows)]
pub(crate) struct Archive7zReadLease(ArchiveOperationLease);

#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct Archive7zExtractionLease(std::sync::Arc<Archive7zExtractionLeaseInner>);

#[cfg(windows)]
struct Archive7zExtractionLeaseInner {
    provider: Archive7zReadLease,
    image: application_image::ApplicationImageLease,
}

#[cfg(windows)]
impl Archive7zExtractionLease {
    pub(super) fn provider(&self) -> &Archive7zReadLease {
        &self.0.provider
    }
    pub(super) fn image(&self) -> &application_image::ApplicationImageLease {
        &self.0.image
    }
}

#[cfg(windows)]
struct ApplicationRouteLease(windows_probe::Lease);

#[cfg(windows)]
impl ApplicationRouteLease {
    fn identity(&self) -> ArchiveFileIdentity {
        self.0.identity()
    }

    fn leaf_handle(&self) -> windows::Win32::Foundation::HANDLE {
        self.0.leaf_handle()
    }
}

#[cfg(windows)]
fn lease_application_route(path: &Path) -> Option<ApplicationRouteLease> {
    windows_probe::lease(path).map(ApplicationRouteLease)
}

#[cfg(windows)]
impl Archive7zCreationLease {
    #[cfg(test)]
    fn into_operation_lease(self) -> ArchiveOperationLease {
        self.0
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_DESCRIPTION_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeOutcome {
    Usable,
    Unusable,
    AdmissionBusy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProbeDescription {
    version: Option<String>,
    formats: Option<Vec<SevenZipFormat>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProbeResult {
    outcome: ProbeOutcome,
    description: ProbeDescription,
    execution_hints: ArchiveExecutionHints,
}

impl ProbeResult {
    #[cfg(test)]
    fn usable(description: ProbeDescription) -> Self {
        Self {
            outcome: ProbeOutcome::Usable,
            description,
            execution_hints: ArchiveExecutionHints::default(),
        }
    }

    fn usable_with_hints(
        description: ProbeDescription,
        execution_hints: ArchiveExecutionHints,
    ) -> Self {
        Self {
            outcome: ProbeOutcome::Usable,
            description,
            execution_hints,
        }
    }

    fn unusable() -> Self {
        Self {
            outcome: ProbeOutcome::Unusable,
            description: ProbeDescription::default(),
            execution_hints: ArchiveExecutionHints::default(),
        }
    }

    fn admission_busy() -> Self {
        Self {
            outcome: ProbeOutcome::AdmissionBusy,
            description: ProbeDescription::default(),
            execution_hints: ArchiveExecutionHints::default(),
        }
    }
}

/// Search captured PATH entries and prove the first match can answer `7z i`.
pub(crate) fn discover_from_path(
    path: Option<OsString>,
    current_dir: PathBuf,
) -> Option<SevenZipExecutable> {
    let candidates = std::env::split_paths(path.as_deref()?)
        .filter_map(|directory| resolve_path_entry(&directory, &current_dir))
        .map(|directory| directory.join("7z.exe"))
        .collect::<Vec<_>>();
    #[cfg(windows)]
    return discover_candidates(
        candidates,
        std::time::Instant::now() + DISCOVERY_TIMEOUT,
        |candidate, timeout| {
            windows_probe::probe_with_description(candidate, &current_dir, &["i"], timeout)
        },
    )
    .map(|(path, description, execution_hints)| SevenZipExecutable {
        path,
        execution_hints,
        version: description.version,
        formats: description.formats,
    });
    #[cfg(not(windows))]
    None
}

fn discover_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
    deadline: std::time::Instant,
    mut probe: impl FnMut(&Path, Duration) -> ProbeResult,
) -> Option<(PathBuf, ProbeDescription, ArchiveExecutionHints)> {
    for candidate in candidates {
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let result = probe(&candidate, remaining.min(PROBE_TIMEOUT));
            match result.outcome {
                ProbeOutcome::Usable => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    return Some((candidate, result.description, result.execution_hints));
                }
                ProbeOutcome::Unusable => break,
                ProbeOutcome::AdmissionBusy => {
                    if !wait_for_probe_slot(deadline) {
                        return None;
                    }
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn wait_for_probe_slot(deadline: std::time::Instant) -> bool {
    while std::time::Instant::now() < deadline {
        if !windows_probe::admission_is_held() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(not(windows))]
fn wait_for_probe_slot(_deadline: std::time::Instant) -> bool {
    false
}

#[cfg(test)]
fn first_usable_executable(
    candidates: impl IntoIterator<Item = PathBuf>,
    mut probe: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file() && probe(candidate))
}

/// Resolve one PATH entry against the request's captured cwd without consulting
/// process-global cwd again. Parent traversal is rejected so later route pinning
/// can prove every component that the resulting absolute path will traverse.
fn resolve_path_entry(entry: &Path, captured_cwd: &Path) -> Option<PathBuf> {
    let joined = if entry.is_absolute() {
        entry.to_path_buf()
    } else {
        captured_cwd.join(entry)
    };
    if !joined.is_absolute() {
        return None;
    }
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return None,
            std::path::Component::Normal(name) => resolved.push(name),
        }
    }
    resolved.is_absolute().then_some(resolved)
}

fn parse_probe_description(bytes: &[u8]) -> ProbeDescription {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return ProbeDescription::default();
    };
    if text.contains('\0') {
        return ProbeDescription::default();
    }

    let version = text.lines().find_map(|line| {
        let version = line.strip_prefix("7-Zip ")?.split_whitespace().next()?;
        let valid = !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            && version.bytes().any(|byte| byte.is_ascii_digit());
        valid.then(|| version.to_owned())
    });

    let mut parsed_formats = None;
    if let Some(section) = text.lines().position(|line| line.trim() == "Formats:") {
        let mut records = Vec::new();
        let mut saw_terminator = false;
        let mut malformed = false;
        for line in text.lines().skip(section + 1) {
            if line.trim() == "Codecs:" {
                saw_terminator = true;
                break;
            }
            let line = line.trim();
            if line.is_empty() || line == "--" {
                continue;
            }
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else {
                malformed = true;
                break;
            };
            let (flags, name_or_time) = if first.parse::<u32>().is_ok() {
                let Some(flags) = fields.next() else {
                    malformed = true;
                    break;
                };
                (flags, fields.next())
            } else if valid_format_flags(first) {
                (first, fields.next())
            } else {
                malformed = true;
                break;
            };
            if !valid_format_flags(flags) {
                malformed = true;
                break;
            }
            let Some(mut name) = name_or_time else {
                malformed = true;
                break;
            };
            if valid_time_flags(name) {
                let Some(next) = fields.next() else {
                    malformed = true;
                    break;
                };
                name = next;
            } else if resembles_time_flags(name) {
                malformed = true;
                break;
            }
            if !valid_format_name(name) {
                malformed = true;
                break;
            }
            records.push(SevenZipFormat {
                name: name.to_owned(),
                flags: flags.to_owned(),
            });
        }
        if saw_terminator && !malformed && !records.is_empty() {
            parsed_formats = Some(records);
        }
    }

    ProbeDescription {
        version,
        formats: parsed_formats,
    }
}

fn valid_format_flags(flags: &str) -> bool {
    flags.len() >= 8
        && flags
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'.' | b'+'))
}

fn valid_format_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_' | b'.'))
}

fn valid_time_flags(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 5
        && matches!(bytes[0], b'w' | b'.')
        && matches!(bytes[1], b'u' | b'.')
        && matches!(bytes[2], b'd' | b'.')
        && matches!(bytes[3], b'n' | b'.')
        && bytes[4..].iter().all(|byte| byte.is_ascii_digit())
}

fn resembles_time_flags(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 4
        && bytes[..4]
            .iter()
            .filter(|byte| matches!(**byte, b'w' | b'u' | b'd' | b'n' | b'.'))
            .count()
            >= 3
}

#[cfg(windows)]
mod windows_probe {
    use super::*;
    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
        sync::{
            Arc, OnceLock,
            atomic::{AtomicBool, AtomicU8, Ordering},
        },
        thread,
        time::Instant,
    };

    #[cfg(test)]
    use std::sync::atomic::AtomicUsize;
    #[cfg(test)]
    use windows::Win32::Foundation::GetHandleInformation;
    use windows::{
        Win32::{
            Foundation::{
                CloseHandle, DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED, ERROR_BROKEN_PIPE,
                ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
                EXCEPTION_BREAKPOINT, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
                WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            Security::SECURITY_ATTRIBUTES,
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
                FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED,
                FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ,
                GetFileInformationByHandle, GetFinalPathNameByHandleW, OPEN_EXISTING, ReadFile,
            },
            System::{
                Com::CoCreateGuid,
                Diagnostics::Debug::{
                    CREATE_PROCESS_DEBUG_EVENT, ContinueDebugEvent, DEBUG_EVENT,
                    EXCEPTION_DEBUG_EVENT, EXIT_PROCESS_DEBUG_EVENT, LOAD_DLL_DEBUG_EVENT,
                    WaitForDebugEvent,
                },
                IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
                },
                Pipes::{
                    ConnectNamedPipe, CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
                    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
                },
                Threading::{
                    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateProcessW,
                    DEBUG_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                    PROCESS_INFORMATION, ResetEvent, ResumeThread, STARTF_USESTDHANDLES,
                    STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
                },
            },
        },
        core::PCWSTR,
    };

    pub(super) mod operation_launcher {
        include!("archive_provider/operation_launcher.rs");
    }

    pub(super) fn run_helper_launch(
        launch: super::seven_zip_helper::SevenZipHelperLaunch,
        cwd: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> super::SevenZipHelperRunOutcome {
        use operation_launcher::OperationOutcome;
        match operation_launcher::run_seven_zip_helper(launch, cwd, deadline, cancelled) {
            OperationOutcome::Exited { code } => super::SevenZipHelperRunOutcome::Exited { code },
            OperationOutcome::Cancelled => super::SevenZipHelperRunOutcome::Cancelled,
            OperationOutcome::TimedOut => super::SevenZipHelperRunOutcome::TimedOut,
            OperationOutcome::LaunchFailed => super::SevenZipHelperRunOutcome::LaunchFailed,
            OperationOutcome::BindingRejected => super::SevenZipHelperRunOutcome::BindingRejected,
        }
    }

    #[cfg(test)]
    pub(crate) fn run_worker_decode_drain_fixture_for_test(
        mode: &str,
        marker: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        drained: &AtomicBool,
    ) -> super::SevenZipDecodeFailure {
        decode_failure(
            operation_launcher::run_worker_decode_drain_fixture_for_test(
                mode, marker, deadline, cancelled, drained,
            ),
        )
    }

    pub(super) fn decode_failure(
        outcome: operation_launcher::OperationOutcome,
    ) -> super::SevenZipDecodeFailure {
        match outcome {
            operation_launcher::OperationOutcome::Cancelled => {
                super::SevenZipDecodeFailure::Cancelled
            }
            _ => super::SevenZipDecodeFailure::Failed,
        }
    }

    #[cfg(test)]
    pub(crate) fn worker_fixture_descendant_alive_for_test(pid: u32) -> bool {
        operation_launcher::worker_fixture_descendant_alive_for_test(pid)
    }

    #[cfg(test)]
    pub(crate) fn worker_fixture_descendant_stopped_for_test(pid: u32) {
        operation_launcher::worker_fixture_descendant_stopped_for_test(pid)
    }

    #[cfg(test)]
    pub(crate) fn launch_sdk_test_child(
        expected_dll: &Path,
        args: &[OsString],
        cwd: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        inherited: &[windows::Win32::Foundation::HANDLE],
    ) -> Option<u32> {
        let lease = operation_launcher::dynamic_lease_for_test(expected_dll);
        match operation_launcher::run_with_test_handles(
            lease, args, cwd, deadline, cancelled, inherited,
        ) {
            operation_launcher::OperationOutcome::Exited { code } => Some(code),
            _ => None,
        }
    }

    const POLL_MS: u32 = 20;
    const TERMINATED_EXIT: u32 = 0x7fff_0001;
    const PIPE_CHUNK_BYTES: usize = 8 * 1024;
    static PROBE_ADMITTED: OnceLock<std::sync::Mutex<bool>> = OnceLock::new();
    #[cfg(test)]
    thread_local! {
        static HANDLE_CLOSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    #[cfg(test)]
    pub(super) static PENDING_READ_SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static READ_POLL_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static READ_MAX_OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    static READ_OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static READ_CANCELLATION_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static DEBUG_EVENTS_WAITED: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static DEBUG_EVENTS_CONTINUED: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static DEBUG_FILES_SEEN: AtomicUsize = AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static DEBUG_FILES_CLOSED: AtomicUsize = AtomicUsize::new(0);

    struct Handle(HANDLE);
    // Windows kernel handles are process-independent values; ownership is moved
    // to the reaper only after the worker has been created.
    unsafe impl Send for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
                #[cfg(test)]
                HANDLE_CLOSES.with(|closes| closes.set(closes.get() + 1));
            }
        }
    }

    struct Admission;
    impl Admission {
        fn acquire() -> Option<Self> {
            let lock = PROBE_ADMITTED.get_or_init(|| std::sync::Mutex::new(false));
            let mut active = lock.lock().ok()?;
            if *active {
                return None;
            }
            *active = true;
            Some(Self)
        }
        fn release() {
            if let Some(lock) = PROBE_ADMITTED.get() {
                if let Ok(mut active) = lock.lock() {
                    *active = false;
                }
            }
        }

        fn is_held() -> bool {
            PROBE_ADMITTED
                .get()
                .and_then(|lock| lock.lock().ok().map(|active| *active))
                .unwrap_or(false)
        }
    }

    pub(super) fn admission_is_held() -> bool {
        Admission::is_held()
    }

    #[cfg(test)]
    pub(super) fn hold_admission_for_test(duration: Duration) -> bool {
        let Some(admission) = Admission::acquire() else {
            return false;
        };
        thread::spawn(move || {
            thread::sleep(duration);
            drop(admission);
            Admission::release();
        });
        true
    }

    pub(super) struct Lease {
        _handles: Vec<Handle>,
        identity: (u32, u32, u32),
    }

    impl Lease {
        pub(super) fn identity(&self) -> ArchiveFileIdentity {
            let (volume_serial, high, low) = self.identity;
            ArchiveFileIdentity {
                volume_serial,
                file_index: (u64::from(high) << 32) | u64::from(low),
            }
        }

        pub(super) fn leaf_handle(&self) -> HANDLE {
            self._handles
                .last()
                .map_or(HANDLE::default(), |handle| handle.0)
        }
    }

    enum ReadCompletion {
        Bytes(usize),
        Empty,
        End,
        Pending,
        Aborted,
        Failed,
    }

    struct PipeRead {
        pipe: Handle,
        event: Handle,
        overlapped: Box<OVERLAPPED>,
        buffer: Box<[u8; PIPE_CHUNK_BYTES]>,
        pending: bool,
    }

    // The boxed OVERLAPPED and buffer stay at stable addresses until the OS
    // confirms completion, including when cleanup transfers them to a reaper.
    unsafe impl Send for PipeRead {}

    impl PipeRead {
        fn new(pipe: Handle, event: Handle) -> Self {
            let mut overlapped = Box::<OVERLAPPED>::default();
            overlapped.hEvent = event.0;
            Self {
                pipe,
                event,
                overlapped,
                buffer: Box::new([0; PIPE_CHUNK_BYTES]),
                pending: false,
            }
        }

        fn begin(&mut self) -> ReadCompletion {
            if self.pending {
                return ReadCompletion::Pending;
            }
            if unsafe { ResetEvent(self.event.0) }.is_err() {
                return ReadCompletion::Failed;
            }
            *self.overlapped = OVERLAPPED {
                hEvent: self.event.0,
                ..OVERLAPPED::default()
            };
            let read = unsafe {
                #[cfg(test)]
                {
                    let outstanding = READ_OUTSTANDING.fetch_add(1, Ordering::Relaxed) + 1;
                    READ_MAX_OUTSTANDING.fetch_max(outstanding, Ordering::Relaxed);
                }
                ReadFile(
                    self.pipe.0,
                    Some(self.buffer.as_mut_slice()),
                    None,
                    Some(&mut *self.overlapped),
                )
            };
            match read {
                Ok(()) => {
                    self.pending = true;
                    self.try_complete()
                }
                Err(error) if is_win32_error(&error, ERROR_IO_PENDING.0) => {
                    self.pending = true;
                    #[cfg(test)]
                    PENDING_READ_SUBMISSIONS.fetch_add(1, Ordering::Relaxed);
                    ReadCompletion::Pending
                }
                Err(error) if is_win32_error(&error, ERROR_BROKEN_PIPE.0) => {
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    ReadCompletion::End
                }
                Err(_) => {
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    ReadCompletion::Failed
                }
            }
        }

        fn try_complete(&mut self) -> ReadCompletion {
            let mut transferred = 0;
            match unsafe {
                GetOverlappedResult(self.pipe.0, &*self.overlapped, &mut transferred, false)
            } {
                Ok(()) => {
                    self.pending = false;
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    if transferred == 0 {
                        ReadCompletion::Empty
                    } else {
                        ReadCompletion::Bytes(transferred as usize)
                    }
                }
                Err(error) if is_win32_error(&error, ERROR_IO_INCOMPLETE.0) => {
                    ReadCompletion::Pending
                }
                Err(error) if is_win32_error(&error, ERROR_BROKEN_PIPE.0) => {
                    self.pending = false;
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    ReadCompletion::End
                }
                Err(error) if is_win32_error(&error, ERROR_OPERATION_ABORTED.0) => {
                    self.pending = false;
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    ReadCompletion::Aborted
                }
                Err(_) => {
                    self.pending = false;
                    #[cfg(test)]
                    READ_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
                    ReadCompletion::Failed
                }
            }
        }

        fn wait_next(&mut self, deadline: Instant, cancelled: &AtomicBool) -> ReadCompletion {
            loop {
                if !self.pending {
                    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                        return ReadCompletion::Pending;
                    }
                    match self.begin() {
                        ReadCompletion::Pending => {}
                        ReadCompletion::Empty => continue,
                        completion => return completion,
                    }
                }
                if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return ReadCompletion::Pending;
                }
                let wait_ms = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(POLL_MS as u128) as u32;
                match unsafe { WaitForSingleObject(self.event.0, wait_ms) } {
                    WAIT_OBJECT_0 => match self.try_complete() {
                        ReadCompletion::Pending => continue,
                        ReadCompletion::Empty => continue,
                        completion => return completion,
                    },
                    WAIT_TIMEOUT => {
                        #[cfg(test)]
                        READ_POLL_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                        return ReadCompletion::Pending;
                    }
                    _ => return ReadCompletion::Failed,
                }
            }
        }

        fn cancel_pending(&mut self) {
            if self.pending {
                let _ = unsafe { CancelIoEx(self.pipe.0, Some(&*self.overlapped)) };
            }
        }

        #[cfg(test)]
        fn confirm_cancelled(&mut self, deadline: Instant) -> bool {
            if !self.pending {
                return true;
            }
            self.cancel_pending();
            loop {
                if unsafe { WaitForSingleObject(self.event.0, POLL_MS) } == WAIT_OBJECT_0 {
                    match self.try_complete() {
                        ReadCompletion::Pending => continue,
                        _ => {
                            #[cfg(test)]
                            READ_CANCELLATION_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
                            return !self.pending;
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return false;
                }
            }
        }
    }

    impl Drop for PipeRead {
        fn drop(&mut self) {
            debug_assert!(
                !self.pending,
                "PipeRead storage cannot be dropped while an overlapped read is pending"
            );
        }
    }

    fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
        error.code().0 as u32 == (0x8007_0000 | code)
    }

    struct PendingConnect {
        pipe: Handle,
        event: Handle,
        overlapped: Box<OVERLAPPED>,
        pending: bool,
        succeeded: bool,
    }

    unsafe impl Send for PendingConnect {}

    impl PendingConnect {
        fn new(pipe: Handle) -> windows::core::Result<Self> {
            Self::new_with_event(pipe, || unsafe {
                CreateEventW(None, true, false, PCWSTR::null())
            })
        }

        fn new_with_event(
            pipe: Handle,
            create_event: impl FnOnce() -> windows::core::Result<HANDLE>,
        ) -> windows::core::Result<Self> {
            let event = create_event()?;
            let event = Handle(event);
            let overlapped = Box::new(OVERLAPPED {
                hEvent: event.0,
                ..OVERLAPPED::default()
            });
            Ok(Self {
                pipe,
                event,
                overlapped,
                pending: false,
                succeeded: false,
            })
        }

        fn start(&mut self) -> Result<(), ()> {
            match unsafe { ConnectNamedPipe(self.pipe.0, Some(&mut *self.overlapped)) } {
                Ok(()) => {
                    self.succeeded = true;
                    Ok(())
                }
                Err(error) if is_win32_error(&error, ERROR_IO_PENDING.0) => {
                    self.pending = true;
                    Ok(())
                }
                Err(error) if is_win32_error(&error, 535) => {
                    self.succeeded = true;
                    Ok(())
                }
                Err(_) => Err(()),
            }
        }

        fn wait_complete(&mut self, timeout: Duration) -> bool {
            if !self.pending {
                return true;
            }
            if unsafe {
                WaitForSingleObject(
                    self.event.0,
                    timeout.as_millis().min(u32::MAX as u128) as u32,
                )
            } != WAIT_OBJECT_0
            {
                return false;
            }
            let mut transferred = 0;
            match unsafe {
                GetOverlappedResult(self.pipe.0, &*self.overlapped, &mut transferred, false)
            } {
                Ok(()) => {
                    self.pending = false;
                    self.succeeded = true;
                    true
                }
                Err(error) if is_win32_error(&error, ERROR_IO_INCOMPLETE.0) => false,
                Err(_) => {
                    self.pending = false;
                    self.succeeded = false;
                    true
                }
            }
        }

        fn cancel_and_wait(&mut self) -> bool {
            if self.pending {
                let _ = unsafe { CancelIoEx(self.pipe.0, Some(&*self.overlapped)) };
            }
            let deadline = Instant::now() + CLEANUP_TIMEOUT;
            while self.pending && Instant::now() < deadline {
                let _ = self.wait_complete(Duration::from_millis(POLL_MS as u64));
            }
            !self.pending
        }
    }

    fn retain_pending_connect(connect: PendingConnect, lease: Lease) {
        let pending = Box::into_raw(Box::new((connect, lease))) as usize;
        let worker = thread::Builder::new()
            .name("7zip-probe-connect-reaper".into())
            .spawn(move || {
                let mut resources =
                    unsafe { Box::from_raw(pending as *mut (PendingConnect, Lease)) };
                let (connect, lease) = &mut *resources;
                while !connect.wait_complete(Duration::from_millis(POLL_MS as u64)) {
                    let _ = lease;
                }
                drop(resources);
                Admission::release();
            });
        if worker.is_err() {
            // Keep the pending OVERLAPPED and its event valid if no cleanup thread can start.
        }
    }

    fn release_connect_setup_failure(lease: Lease) {
        drop(lease);
        Admission::release();
    }

    #[cfg(test)]
    pub(super) fn event_creation_failure_closes_pipe_once_and_releases_slot() -> bool {
        let Some(_admission) = Admission::acquire() else {
            return false;
        };
        let guid = match unsafe { CoCreateGuid() } {
            Ok(guid) => guid,
            Err(_) => {
                Admission::release();
                return false;
            }
        };
        let name = wide(OsStr::new(&format!(r"\\.\pipe\gpui-filer-test-{guid:?}")));
        let mode = NAMED_PIPE_MODE(
            PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
        );
        let access = windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
            0x00000001 | FILE_FLAG_OVERLAPPED.0,
        );
        let raw = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                access,
                mode,
                1,
                0,
                PIPE_CHUNK_BYTES as u32,
                0,
                None,
            )
        };
        if raw.is_invalid() {
            Admission::release();
            return false;
        }
        let before = HANDLE_CLOSES.with(std::cell::Cell::get);
        let result =
            PendingConnect::new_with_event(Handle(raw), || Err(windows::core::Error::from_win32()));
        let mut flags = 0u32;
        let closed = unsafe { GetHandleInformation(raw, &mut flags) }.is_err();
        release_connect_setup_failure(Lease {
            _handles: Vec::new(),
            identity: (0, 0, 0),
        });
        let after = HANDLE_CLOSES.with(std::cell::Cell::get);
        result.is_err() && closed && after == before + 1 && !admission_is_held()
    }

    #[cfg(test)]
    pub(super) fn pending_read_cancel_is_confirmed_before_drop() -> bool {
        pending_read_cancel_test(false)
    }

    #[cfg(test)]
    pub(super) fn unconfirmed_read_cleanup_retains_admission() -> bool {
        pending_read_cancel_test(true)
    }

    #[cfg(test)]
    fn pending_read_cancel_test(force_unconfirmed: bool) -> bool {
        let Some(_admission) = Admission::acquire() else {
            return false;
        };
        let Ok(executable) = std::env::current_exe() else {
            Admission::release();
            return false;
        };
        let Some(lease) = lease(&executable) else {
            Admission::release();
            return false;
        };
        let cancelled = AtomicBool::new(false);
        let result = capture_pipe(lease, Instant::now() + Duration::from_secs(2), &cancelled);
        let Ok((mut read, stdout, lease)) = result else {
            return false;
        };
        let submitted = matches!(read.begin(), ReadCompletion::Pending);
        read.cancel_pending();
        let confirmed =
            !force_unconfirmed && read.confirm_cancelled(Instant::now() + Duration::from_secs(2));
        if !confirmed {
            let cleanup = retain_test_read_until_cancelled(read, stdout, lease, force_unconfirmed);
            if force_unconfirmed {
                let Some((ready, resume_cleanup)) = cleanup else {
                    return false;
                };
                let transferred = ready.recv_timeout(Duration::from_secs(2)).is_ok();
                let held_while_retained = admission_is_held();
                let resumed = resume_cleanup.send(()).is_ok();
                let deadline = Instant::now() + Duration::from_secs(2);
                while admission_is_held() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                return submitted
                    && transferred
                    && held_while_retained
                    && resumed
                    && !admission_is_held();
            }
            return false;
        }
        let no_pending_request = !read.pending;
        drop((read, stdout, lease));
        Admission::release();
        submitted && confirmed && no_pending_request && !admission_is_held()
    }

    #[cfg(test)]
    fn retain_test_read_until_cancelled(
        read: PipeRead,
        stdout: Handle,
        lease: Lease,
        wait_for_test_release: bool,
    ) -> Option<(
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    )> {
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (resume_sender, resume_receiver) = std::sync::mpsc::sync_channel(0);
        let resources = Box::into_raw(Box::new((read, stdout, lease))) as usize;
        let worker = thread::Builder::new()
            .name("7zip-probe-test-read-reaper".into())
            .spawn(move || {
                let (mut read, stdout, lease) =
                    unsafe { *Box::from_raw(resources as *mut (PipeRead, Handle, Lease)) };
                let _ = ready_sender.send(());
                if wait_for_test_release && resume_receiver.recv().is_err() {
                    // Keep the resources alive if the test coordinator disappears.
                    std::mem::forget((read, stdout, lease));
                    return;
                }
                while !read
                    .confirm_cancelled(Instant::now() + Duration::from_millis(POLL_MS as u64))
                {
                    read.cancel_pending();
                    thread::sleep(Duration::from_millis(10));
                }
                drop((read, stdout, lease));
                Admission::release();
            });
        // spawn failure intentionally leaks the raw owner, preserving any OS-visible pending I/O.
        worker.ok().map(|_| (ready_receiver, resume_sender))
    }

    fn capture_pipe(
        lease: Lease,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(PipeRead, Handle, Lease), ()> {
        let guid = match unsafe { CoCreateGuid() } {
            Ok(guid) => guid,
            Err(_) => {
                drop(lease);
                Admission::release();
                return Err(());
            }
        };
        let name = format!(r"\\.\pipe\gpui-filer-7z-{guid:?}");
        let mut name_w = wide(OsStr::new(&name));
        let mode = NAMED_PIPE_MODE(
            PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
        );
        let access = windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
            0x00000001 | FILE_FLAG_OVERLAPPED.0 | FILE_FLAG_FIRST_PIPE_INSTANCE.0,
        );
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(name_w.as_mut_ptr()),
                access,
                mode,
                1,
                0,
                PIPE_CHUNK_BYTES as u32,
                0,
                None,
            )
        };
        if server.is_invalid() {
            drop(lease);
            Admission::release();
            return Err(());
        }
        let mut connect = match PendingConnect::new(Handle(server)) {
            Ok(connect) => connect,
            Err(_) => {
                release_connect_setup_failure(lease);
                return Err(());
            }
        };
        if connect.start().is_err() {
            if !connect.cancel_and_wait() {
                retain_pending_connect(connect, lease);
            } else {
                drop(connect);
                drop(lease);
                Admission::release();
            }
            return Err(());
        }

        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let client = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_mut_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                Some(&security),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        let client = match client {
            Ok(client) => Handle(client),
            Err(_) => {
                if !connect.cancel_and_wait() {
                    retain_pending_connect(connect, lease);
                } else {
                    drop(connect);
                    drop(lease);
                    Admission::release();
                }
                return Err(());
            }
        };
        let connected = loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                break false;
            }
            if connect.wait_complete(Duration::from_millis(POLL_MS as u64)) && connect.succeeded {
                break true;
            }
        };
        if !connected {
            drop(client);
            if !connect.cancel_and_wait() {
                retain_pending_connect(connect, lease);
            } else {
                drop(connect);
                drop(lease);
                Admission::release();
            }
            return Err(());
        }
        let PendingConnect {
            pipe,
            event,
            pending: false,
            ..
        } = connect
        else {
            unreachable!("connected pipe must have no pending connect request")
        };
        Ok((PipeRead::new(pipe, event), client, lease))
    }
    pub(super) fn lease(path: &Path) -> Option<Lease> {
        if !path.is_absolute() {
            return None;
        }
        let mut ancestors = path.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        let mut handles = Vec::with_capacity(ancestors.len());
        let mut identity = None;
        for (index, component) in ancestors.iter().enumerate() {
            let final_component = index + 1 == ancestors.len();
            let component_wide = wide(component.as_os_str());
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(component_wide.as_ptr()),
                    if final_component {
                        0x8000_0000 | FILE_READ_ATTRIBUTES.0
                    } else {
                        FILE_READ_ATTRIBUTES.0
                    },
                    FILE_SHARE_READ,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    None,
                )
                .ok()?
            };
            let handle = Handle(handle);
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe {
                GetFileInformationByHandle(handle.0, &mut info).ok()?;
            }
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return None;
            }
            let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            if is_directory == final_component {
                return None;
            }
            if final_component {
                identity = Some((
                    info.dwVolumeSerialNumber,
                    info.nFileIndexHigh,
                    info.nFileIndexLow,
                ));
            }
            handles.push(handle);
        }
        Some(Lease {
            _handles: handles,
            identity: identity?,
        })
    }

    #[cfg(test)]
    pub(super) fn test_file_hint(path: &Path) -> ArchiveFileHint {
        let route = lease(path).expect("test file has a safe no-follow route");
        let (volume_serial, high, low) = route.identity;
        ArchiveFileHint {
            path: path.to_owned(),
            identity: ArchiveFileIdentity {
                volume_serial,
                file_index: (u64::from(high) << 32) | u64::from(low),
            },
        }
    }

    #[cfg(test)]
    pub(super) fn lease_operation_binding(
        owner: &ArchiveProviderOwner,
        revision: u64,
        after_leases: impl FnOnce(),
    ) -> Option<ArchiveOperationLease> {
        let binding = {
            let snapshot = owner.snapshot();
            if snapshot.revision != revision {
                return None;
            }
            match snapshot.state {
                ArchiveProviderState::Available(executable) => {
                    executable.execution_hints.operation_binding?
                }
                _ => return None,
            }
        };

        // Route opens can touch the filesystem, so they must happen after the
        // authority mutex has been released.
        let executable_lease = lease(&binding.executable.path)?;
        let dll_lease = lease(&binding.zip_dll.path)?;
        after_leases();

        let identity_matches = |lease: &Lease, hint: &ArchiveFileHint| {
            let (volume, high, low) = lease.identity;
            volume == hint.identity.volume_serial
                && ((u64::from(high) << 32) | u64::from(low)) == hint.identity.file_index
        };
        if !identity_matches(&executable_lease, &binding.executable)
            || !identity_matches(&dll_lease, &binding.zip_dll)
        {
            return None;
        }

        let current = owner.snapshot();
        if current.revision != revision
            || !matches!(
                current.state,
                ArchiveProviderState::Available(ref executable)
                    if executable.execution_hints.operation_binding.as_ref() == Some(&binding)
            )
        {
            return None;
        }

        Some(ArchiveOperationLease {
            #[cfg(test)]
            revision,
            binding,
            _executable: executable_lease,
            _zip_dll: dll_lease,
        })
    }

    pub(super) fn lease_7z_creation_binding(
        owner: &ArchiveProviderOwner,
        revision: u64,
        expected_binding: &ArchiveOperationBinding,
        after_leases: impl FnOnce(),
    ) -> Option<Archive7zCreationLease> {
        let binding = {
            let snapshot = owner.snapshot();
            if snapshot.revision != revision {
                return None;
            }
            let ArchiveProviderState::Available(executable) = snapshot.state else {
                return None;
            };
            if !executable
                .formats
                .as_ref()
                .is_some_and(|formats| formats.iter().any(SevenZipFormat::supports_7z_creation))
            {
                return None;
            }
            let binding = executable.execution_hints.operation_binding?;
            if &binding != expected_binding {
                return None;
            }
            binding
        };

        // Route opens can touch the filesystem, so authority stays unlocked.
        let executable_lease = lease(&binding.executable.path)?;
        let dll_lease = lease(&binding.zip_dll.path)?;
        after_leases();

        let identity_matches = |route: &Lease, hint: &ArchiveFileHint| {
            let (volume, high, low) = route.identity;
            volume == hint.identity.volume_serial
                && ((u64::from(high) << 32) | u64::from(low)) == hint.identity.file_index
        };
        if !identity_matches(&executable_lease, &binding.executable)
            || !identity_matches(&dll_lease, &binding.zip_dll)
        {
            return None;
        }

        let current = owner.snapshot();
        if current.revision != revision
            || !matches!(
                current.state,
                ArchiveProviderState::Available(ref executable)
                    if executable.execution_hints.operation_binding.as_ref() == Some(&binding)
                        && executable.formats.as_ref().is_some_and(|formats| {
                            formats.iter().any(SevenZipFormat::supports_7z_creation)
                        })
            )
        {
            return None;
        }

        Some(Archive7zCreationLease(ArchiveOperationLease {
            #[cfg(test)]
            revision,
            binding,
            _executable: executable_lease,
            _zip_dll: dll_lease,
        }))
    }

    pub(super) fn lease_7z_read(
        owner: &ArchiveProviderOwner,
        capability: &Archive7zReadCapability,
        after_leases: impl FnOnce(),
    ) -> Option<Archive7zReadLease> {
        let binding = {
            let snapshot = owner.snapshot();
            if snapshot.revision != capability.revision {
                return None;
            }
            let ArchiveProviderState::Available(executable) = snapshot.state else {
                return None;
            };
            let binding = executable.execution_hints.operation_binding?;
            let formats = executable.formats?;
            let mut seven_zip = formats.into_iter().filter(|format| format.name == "7z");
            let current_format = seven_zip.next()?;
            if seven_zip.next().is_some()
                || current_format != capability.format
                || !current_format.has_valid_flags()
                || binding != capability.binding
            {
                return None;
            }
            binding
        };

        // route の open は待機し得るため、provider の状態 lock を保持しない。
        let executable_lease = lease(&binding.executable.path)?;
        let dll_lease = lease(&binding.zip_dll.path)?;
        after_leases();
        let identity_matches = |route: &Lease, hint: &ArchiveFileHint| {
            let (volume, high, low) = route.identity;
            volume == hint.identity.volume_serial
                && ((u64::from(high) << 32) | u64::from(low)) == hint.identity.file_index
        };
        if !identity_matches(&executable_lease, &binding.executable)
            || !identity_matches(&dll_lease, &binding.zip_dll)
        {
            return None;
        }

        let current = owner.snapshot();
        if current.revision != capability.revision
            || !matches!(current.state,
                ArchiveProviderState::Available(ref executable)
                    if executable.execution_hints.operation_binding.as_ref() == Some(&binding)
                        && executable.formats.as_ref().is_some_and(|formats| {
                            let mut matching = formats.iter().filter(|format| format.name == "7z");
                            matching.next() == Some(&capability.format)
                                && matching.next().is_none()
                                && capability.format.has_valid_flags()
                        })
            )
        {
            return None;
        }
        Some(Archive7zReadLease(ArchiveOperationLease {
            #[cfg(test)]
            revision: capability.revision,
            binding,
            _executable: executable_lease,
            _zip_dll: dll_lease,
        }))
    }

    fn same_identity(path: &Path, expected: (u32, u32, u32)) -> bool {
        lease(path).is_some_and(|lease| lease.identity == expected)
    }

    #[derive(Default)]
    pub(super) struct DebugEvidence {
        images: HashMap<u32, Option<ArchiveFileHint>>,
        zip_dlls: HashMap<u32, Vec<ArchiveFileHint>>,
        ambiguous_processes: HashSet<u32>,
        initial_breakpoints: HashSet<u32>,
        ambiguous: bool,
        pinned_routes: Vec<Lease>,
        #[cfg(test)]
        trace_fixture_events: bool,
        #[cfg(test)]
        fixture_event_trace: Vec<String>,
    }

    fn identity_from_handle(handle: HANDLE) -> Option<(u32, u32, u32)> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle, &mut info).ok()? };
        Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }

    fn debug_file_path(handle: HANDLE) -> Option<PathBuf> {
        if handle.is_invalid() {
            return None;
        }
        let mut path_buffer = vec![0u16; 32 * 1024];
        let length =
            unsafe { GetFinalPathNameByHandleW(handle, &mut path_buffer, FILE_NAME_NORMALIZED) }
                as usize;
        if length == 0 || length >= path_buffer.len() {
            #[cfg(test)]
            eprintln!(
                "debug image path lookup failed: valid_handle=true length={length} capacity={} win32_error={:?}",
                path_buffer.len(),
                std::io::Error::last_os_error()
            );
            return None;
        }
        path_buffer.truncate(length);
        Some(PathBuf::from(std::ffi::OsString::from_wide(&path_buffer)))
    }

    fn observe_debug_file(handle: HANDLE, path: PathBuf) -> Option<(Lease, ArchiveFileHint)> {
        let observed_identity = identity_from_handle(handle)?;
        let route = lease(&path)?;
        if route.identity != observed_identity {
            return None;
        }
        let hint = ArchiveFileHint {
            path: path.clone(),
            identity: ArchiveFileIdentity {
                volume_serial: observed_identity.0,
                file_index: ((observed_identity.1 as u64) << 32) | observed_identity.2 as u64,
            },
        };
        Some((route, hint))
    }

    pub(super) fn exception_continue_status(
        initial_breakpoints: &mut HashSet<u32>,
        process_id: u32,
        exception_code: windows::Win32::Foundation::NTSTATUS,
        first_chance: u32,
    ) -> windows::Win32::Foundation::NTSTATUS {
        if exception_code == EXCEPTION_BREAKPOINT
            && first_chance != 0
            && initial_breakpoints.remove(&process_id)
        {
            DBG_CONTINUE
        } else {
            DBG_EXCEPTION_NOT_HANDLED
        }
    }

    // Record file-object evidence independently from the decision to continue a
    // debug event so a launch authorization gate can hold the startup event.
    pub(super) fn observe_received_debug_event(evidence: &mut DebugEvidence, event: &DEBUG_EVENT) {
        match event.dwDebugEventCode {
            CREATE_PROCESS_DEBUG_EVENT => {
                let info = unsafe { event.u.CreateProcessInfo };
                evidence.initial_breakpoints.insert(event.dwProcessId);
                if !info.hFile.is_invalid() {
                    match debug_file_path(info.hFile)
                        .and_then(|path| observe_debug_file(info.hFile, path))
                    {
                        Some((route, hint)) => {
                            evidence.pinned_routes.push(route);
                            if evidence
                                .images
                                .insert(event.dwProcessId, Some(hint))
                                .is_some()
                            {
                                evidence.ambiguous = true;
                                evidence.ambiguous_processes.insert(event.dwProcessId);
                            }
                        }
                        None => {
                            evidence.ambiguous = true;
                            evidence.ambiguous_processes.insert(event.dwProcessId);
                            evidence.images.insert(event.dwProcessId, None);
                        }
                    }
                    close_debug_file(info.hFile);
                } else {
                    evidence.ambiguous = true;
                    evidence.ambiguous_processes.insert(event.dwProcessId);
                    evidence.images.insert(event.dwProcessId, None);
                }
            }
            LOAD_DLL_DEBUG_EVENT => {
                let info = unsafe { event.u.LoadDll };
                #[cfg(test)]
                let mut load_dll_evidence = if info.hFile.is_invalid() {
                    "invalid file handle".to_owned()
                } else {
                    "unresolved file handle".to_owned()
                };
                if !info.hFile.is_invalid() {
                    match debug_file_path(info.hFile) {
                        Some(path)
                            if path.file_name().is_some_and(|name| {
                                name.to_string_lossy().eq_ignore_ascii_case("7z.dll")
                            }) =>
                        {
                            #[cfg(test)]
                            {
                                load_dll_evidence = format!("7z candidate path={path:?}");
                            }
                            match observe_debug_file(info.hFile, path) {
                                Some((route, hint)) => {
                                    #[cfg(test)]
                                    {
                                        load_dll_evidence =
                                            format!("7z observed path={}", hint.path.display());
                                    }
                                    evidence.pinned_routes.push(route);
                                    evidence
                                        .zip_dlls
                                        .entry(event.dwProcessId)
                                        .or_default()
                                        .push(hint);
                                }
                                None => {
                                    #[cfg(test)]
                                    {
                                        load_dll_evidence.push_str(" identity/route rejected");
                                    }
                                    evidence.ambiguous = true;
                                    evidence.ambiguous_processes.insert(event.dwProcessId);
                                }
                            }
                        }
                        Some(_path) => {
                            #[cfg(test)]
                            {
                                load_dll_evidence = format!("other image path={_path:?}");
                            }
                        }
                        None => {
                            #[cfg(test)]
                            {
                                load_dll_evidence =
                                    "valid file handle but image path unavailable".to_owned();
                            }
                            evidence.ambiguous = true;
                            evidence.ambiguous_processes.insert(event.dwProcessId);
                        }
                    }
                    close_debug_file(info.hFile);
                } else {
                    evidence.ambiguous = true;
                    evidence.ambiguous_processes.insert(event.dwProcessId);
                }
                #[cfg(test)]
                if evidence.trace_fixture_events {
                    if evidence.fixture_event_trace.len() == 64 {
                        evidence.fixture_event_trace.remove(0);
                    }
                    evidence.fixture_event_trace.push(format!(
                        "pid={} kind=LOAD_DLL evidence={load_dll_evidence}",
                        event.dwProcessId
                    ));
                }
            }
            EXIT_PROCESS_DEBUG_EVENT => {
                evidence.initial_breakpoints.remove(&event.dwProcessId);
            }
            _ => {}
        }
    }

    // The creator thread is the debugger for this process tree. Every successful
    // wait has one matching ContinueDebugEvent, even when evidence is unusable.
    fn dispatch_debug_event(evidence: &mut DebugEvidence, timeout_ms: u32) -> bool {
        let mut event = DEBUG_EVENT::default();
        if unsafe { WaitForDebugEvent(&mut event, timeout_ms) }.is_err() {
            return false;
        }
        #[cfg(test)]
        DEBUG_EVENTS_WAITED.fetch_add(1, Ordering::Relaxed);
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
        let continued =
            unsafe { ContinueDebugEvent(event.dwProcessId, event.dwThreadId, status).is_ok() };
        #[cfg(test)]
        {
            if continued {
                DEBUG_EVENTS_CONTINUED.fetch_add(1, Ordering::Relaxed);
            }
            if evidence.trace_fixture_events
                && matches!(
                    event.dwDebugEventCode,
                    CREATE_PROCESS_DEBUG_EVENT | EXCEPTION_DEBUG_EVENT | EXIT_PROCESS_DEBUG_EVENT
                )
            {
                let event_kind = match event.dwDebugEventCode {
                    CREATE_PROCESS_DEBUG_EVENT => "CREATE_PROCESS",
                    EXCEPTION_DEBUG_EVENT => "EXCEPTION",
                    EXIT_PROCESS_DEBUG_EVENT => "EXIT_PROCESS",
                    _ => unreachable!(),
                };
                if evidence.fixture_event_trace.len() == 64 {
                    evidence.fixture_event_trace.remove(0);
                }
                evidence.fixture_event_trace.push(format!(
                    "pid={} tid={} kind={event_kind} code={:?} continue_status={:#010x} continued={continued}",
                    event.dwProcessId,
                    event.dwThreadId,
                    event.dwDebugEventCode,
                    status.0 as u32,
                ));
            }
        }
        if !continued {
            evidence.ambiguous = true;
        }
        continued
    }

    fn close_debug_file(handle: HANDLE) {
        #[cfg(test)]
        DEBUG_FILES_SEEN.fetch_add(1, Ordering::Relaxed);
        let closed = unsafe { CloseHandle(handle).is_ok() };
        #[cfg(test)]
        if closed {
            DEBUG_FILES_CLOSED.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(test))]
        let _ = closed;
    }

    fn operation_binding(evidence: &DebugEvidence) -> Option<(ArchiveFileHint, ArchiveFileHint)> {
        if evidence.ambiguous {
            return None;
        }
        let mut matches = evidence.zip_dlls.iter().filter_map(|(pid, dlls)| {
            if dlls.len() != 1 {
                return None;
            }
            evidence
                .images
                .get(pid)?
                .as_ref()
                .map(|image| (image.clone(), dlls[0].clone()))
        });
        let result = matches.next()?;
        matches.next().is_none().then_some(result)
    }

    fn wait_empty_debug(job: HANDLE, deadline: Instant, evidence: &mut DebugEvidence) -> bool {
        loop {
            match active_processes(job) {
                Some(0) => {
                    while dispatch_debug_event(evidence, 0) {}
                    return true;
                }
                Some(_) if Instant::now() < deadline => {
                    dispatch_debug_event(evidence, POLL_MS);
                }
                _ => return false,
            }
        }
    }

    fn confirm_read_debug(
        read: &mut PipeRead,
        deadline: Instant,
        evidence: &mut DebugEvidence,
    ) -> bool {
        read.cancel_pending();
        while read.pending && Instant::now() < deadline {
            dispatch_debug_event(evidence, 0);
            if unsafe { WaitForSingleObject(read.event.0, POLL_MS) } == WAIT_OBJECT_0 {
                let _ = read.try_complete();
            }
        }
        !read.pending
    }

    fn wait_process_debug(
        process: HANDLE,
        deadline: Instant,
        evidence: &mut DebugEvidence,
    ) -> bool {
        while Instant::now() < deadline {
            if unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0 {
                return true;
            }
            dispatch_debug_event(evidence, POLL_MS);
        }
        unsafe { WaitForSingleObject(process, 0) == WAIT_OBJECT_0 }
    }

    fn finish_terminated_debug_process(process: HANDLE, evidence: &mut DebugEvidence) {
        while unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
            dispatch_debug_event(evidence, POLL_MS);
        }
        while dispatch_debug_event(evidence, 0) {}
    }

    fn finish_debug_cleanup(job: HANDLE, read: &mut PipeRead, evidence: &mut DebugEvidence) {
        loop {
            dispatch_debug_event(evidence, POLL_MS);
            if !read.pending && active_processes(job) == Some(0) {
                while dispatch_debug_event(evidence, 0) {}
                return;
            }
            if read.pending {
                read.cancel_pending();
                if unsafe { WaitForSingleObject(read.event.0, 0) } == WAIT_OBJECT_0 {
                    let _ = read.try_complete();
                }
            }
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn quote_arg(arg: &OsStr) -> String {
        let value = arg.to_string_lossy();
        if !value.is_empty() && !value.chars().any(char::is_whitespace) && !value.contains('"') {
            return value.into_owned();
        }
        let mut quoted = String::from("\"");
        let mut slashes = 0usize;
        for ch in value.chars() {
            if ch == '\\' {
                slashes += 1;
                continue;
            }
            if ch == '"' {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
                quoted.push('"');
            } else {
                quoted.push_str(&"\\".repeat(slashes));
                quoted.push(ch);
            }
            slashes = 0;
        }
        quoted.push_str(&"\\".repeat(slashes * 2));
        quoted.push('"');
        quoted
    }

    struct AttributeList {
        ptr: windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST,
        _storage: Vec<usize>,
        // Windows keeps this pointer in the attribute list until that list is deleted.
        _handles: Vec<HANDLE>,
    }
    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }
    fn attributes(handles: &[HANDLE]) -> Option<AttributeList> {
        let mut size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(
                windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST::default(),
                1,
                0,
                &mut size,
            );
        }
        if size == 0 {
            return None;
        }
        let words = size.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        let ptr = windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST(
            storage.as_mut_ptr().cast(),
        );
        unsafe {
            InitializeProcThreadAttributeList(ptr, 1, 0, &mut size).ok()?;
        }
        let owned_handles = handles.to_vec();
        let attrs = AttributeList {
            ptr,
            _storage: storage,
            _handles: owned_handles,
        };
        unsafe {
            UpdateProcThreadAttribute(
                ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(attrs._handles.as_ptr().cast()),
                std::mem::size_of_val(attrs._handles.as_slice()),
                None,
                None,
            )
            .ok()?;
        }
        Some(attrs)
    }

    fn active_processes(job: HANDLE) -> Option<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
                None,
            )
            .ok()?;
        }
        Some(info.ActiveProcesses)
    }

    #[cfg(test)]
    pub(super) fn probe(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
    ) -> ProbeOutcome {
        probe_with_lease_hook(path, current_dir, args, deadline, || {})
    }

    pub(super) fn probe_with_description(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
    ) -> ProbeResult {
        probe_with_lease_hook_description(path, current_dir, args, deadline, || {})
    }

    #[cfg(test)]
    pub(super) fn probe_with_lease_hook(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
        on_lease: impl FnOnce() + Send + 'static,
    ) -> ProbeOutcome {
        probe_with_lease_hook_description(path, current_dir, args, deadline, on_lease).outcome
    }

    fn probe_with_lease_hook_description(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
        on_lease: impl FnOnce() + Send + 'static,
    ) -> ProbeResult {
        probe_with_capture_fault(path, current_dir, args, deadline, None, on_lease)
    }

    #[cfg(test)]
    pub(super) fn probe_with_injected_read_failure(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
        fail_after_bytes: usize,
    ) -> ProbeResult {
        probe_with_capture_fault(
            path,
            current_dir,
            args,
            deadline,
            Some(fail_after_bytes),
            || {},
        )
    }

    fn probe_with_capture_fault(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline: Duration,
        fail_after_bytes: Option<usize>,
        on_lease: impl FnOnce() + Send + 'static,
    ) -> ProbeResult {
        let Some(_admission) = Admission::acquire() else {
            return ProbeResult::admission_busy();
        };
        let deadline_at = Instant::now() + deadline;
        let path = path.to_path_buf();
        let current_dir = current_dir.to_path_buf();
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let launch_state = Arc::new(AtomicU8::new(0));
        let worker_launch_state = launch_state.clone();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("7zip-probe-supervisor".into())
            .spawn(move || {
                let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                let result = probe_owned(
                    &path,
                    &current_dir,
                    &args,
                    deadline_at,
                    worker_cancelled,
                    worker_launch_state,
                    fail_after_bytes,
                    on_lease,
                );
                let _ = send.send(result);
            });
        if worker.is_err() {
            Admission::release();
            return ProbeResult::unusable();
        }
        match receive.recv_timeout(deadline_at.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                let _ = launch_state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
                ProbeResult::unusable()
            }
        }
    }

    fn probe_owned(
        path: &Path,
        current_dir: &Path,
        args: &[&str],
        deadline_at: Instant,
        cancelled: Arc<AtomicBool>,
        launch_state: Arc<AtomicU8>,
        fail_after_bytes: Option<usize>,
        on_lease: impl FnOnce(),
    ) -> ProbeResult {
        let fail = ProbeResult::unusable;
        let Some(exe_lease) = lease(path) else {
            Admission::release();
            return fail();
        };
        on_lease();
        let (mut read, stdout, exe_lease) = match capture_pipe(exe_lease, deadline_at, &cancelled) {
            Ok(capture) => capture,
            Err(()) => return fail(),
        };
        let mut nul_name = "NUL"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let nul = unsafe {
            CreateFileW(
                PCWSTR(nul_name.as_mut_ptr()),
                0xc000_0000,
                FILE_SHARE_READ,
                Some(&security),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        let Ok(nul) = nul else {
            drop((read, exe_lease));
            Admission::release();
            return fail();
        };
        let nul = Handle(nul);
        if unsafe {
            windows::Win32::Foundation::SetHandleInformation(
                nul.0,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0),
            )
            .is_err()
        } {
            drop(nul);
            drop((read, exe_lease));
            Admission::release();
            return fail();
        }
        let Some(attrs) = attributes(&[nul.0, stdout.0]) else {
            drop(nul);
            drop((read, stdout, exe_lease));
            Admission::release();
            return fail();
        };
        let Ok(job) = (unsafe { CreateJobObjectW(None, PCWSTR::null()) }) else {
            drop(nul);
            drop((read, stdout, exe_lease));
            Admission::release();
            return fail();
        };
        let job = Handle(job);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
            .is_err()
        } {
            drop(job);
            drop(nul);
            drop((read, stdout, exe_lease));
            Admission::release();
            return fail();
        }

        let mut command = quote_arg(path.as_os_str());
        for arg in args {
            command.push(' ');
            command.push_str(&quote_arg(OsStr::new(arg)));
        }
        let mut command = command
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = nul.0;
        startup.StartupInfo.hStdOutput = stdout.0;
        startup.StartupInfo.hStdError = nul.0;
        startup.lpAttributeList = attrs.ptr;
        let app = wide(path.as_os_str());
        let cwd = wide(current_dir.as_os_str());
        let mut debug_evidence = DebugEvidence::default();
        #[cfg(test)]
        {
            debug_evidence.trace_fixture_events = args.iter().any(|arg| {
                matches!(
                    *arg,
                    "--fixture-mode=success_with_dll" | "--fixture-mode=shim_with_dll"
                )
            });
        }
        let mut process_info = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                windows::core::PWSTR(command.as_mut_ptr()),
                None,
                None,
                true,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | DEBUG_PROCESS,
                None,
                PCWSTR(cwd.as_ptr()),
                (&startup as *const STARTUPINFOEXW).cast(),
                &mut process_info,
            )
        };
        let Ok(()) = created else {
            drop(job);
            drop(nul);
            drop((read, stdout, exe_lease));
            Admission::release();
            return fail();
        };
        drop(stdout);
        let process = Handle(process_info.hProcess);
        let primary_thread = Handle(process_info.hThread);
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline_at {
            unsafe {
                let _ =
                    windows::Win32::System::Threading::TerminateProcess(process.0, TERMINATED_EXIT);
            }
            let stopped = wait_process_debug(
                process.0,
                Instant::now() + CLEANUP_TIMEOUT,
                &mut debug_evidence,
            );
            if !stopped {
                finish_terminated_debug_process(process.0, &mut debug_evidence);
            }
            while dispatch_debug_event(&mut debug_evidence, 0) {}
            drop(primary_thread);
            drop(job);
            drop(nul);
            drop(read);
            drop(exe_lease);
            drop(process);
            Admission::release();
            return fail();
        }
        let assigned = unsafe { AssignProcessToJobObject(job.0, process.0).is_ok() };
        if !assigned {
            unsafe {
                let _ =
                    windows::Win32::System::Threading::TerminateProcess(process.0, TERMINATED_EXIT);
            }
            let waited = wait_process_debug(
                process.0,
                Instant::now() + CLEANUP_TIMEOUT,
                &mut debug_evidence,
            );
            if !waited {
                finish_terminated_debug_process(process.0, &mut debug_evidence);
            }
            while dispatch_debug_event(&mut debug_evidence, 0) {}
            drop(primary_thread);
            drop(nul);
            drop(read);
            drop(exe_lease);
            drop(process);
            drop(job);
            Admission::release();
            return fail();
        }
        let launch_claimed = launch_state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        let can_resume = launch_claimed
            && !cancelled.load(Ordering::Acquire)
            && Instant::now() < deadline_at
            && unsafe { ResumeThread(primary_thread.0) != u32::MAX };
        if !can_resume {
            unsafe {
                let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
            }
            let cleaned =
                wait_empty_debug(job.0, Instant::now() + CLEANUP_TIMEOUT, &mut debug_evidence);
            let read_clean = !read.pending;
            if !cleaned || !read_clean {
                finish_debug_cleanup(job.0, &mut read, &mut debug_evidence);
            }
            drop(primary_thread);
            drop(nul);
            drop(read);
            drop(exe_lease);
            drop(process);
            drop(job);
            Admission::release();
            return fail();
        }
        drop(primary_thread);
        let until = deadline_at;
        let mut output = Vec::new();
        let mut overflow = false;
        let mut eof = false;
        let mut eof_at = None;
        let mut normal_exit = false;
        let mut io_ok = true;
        let mut bytes_read = 0usize;
        while !eof && !cancelled.load(Ordering::Acquire) && Instant::now() < until {
            while dispatch_debug_event(&mut debug_evidence, 0) {}
            match read.wait_next(until, &cancelled) {
                ReadCompletion::Bytes(length) => {
                    if !overflow
                        && output.len().saturating_add(length) <= MAX_PROBE_DESCRIPTION_BYTES
                    {
                        output.extend_from_slice(&read.buffer[..length]);
                    } else {
                        overflow = true;
                        output.clear();
                    }
                    bytes_read = bytes_read.saturating_add(length);
                    if fail_after_bytes.is_some_and(|limit| bytes_read >= limit) {
                        io_ok = false;
                        break;
                    }
                }
                ReadCompletion::Empty => continue,
                ReadCompletion::End => {
                    eof = true;
                    eof_at = Some(Instant::now());
                }
                ReadCompletion::Pending => continue,
                ReadCompletion::Failed | ReadCompletion::Aborted => {
                    io_ok = false;
                    break;
                }
            }
            if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
                normal_exit = true;
            }
            while dispatch_debug_event(&mut debug_evidence, 0) {}
        }
        if !normal_exit && unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
            normal_exit = true;
        }
        while !normal_exit && Instant::now() < deadline_at {
            let wait_ms = deadline_at
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(POLL_MS as u128) as u32;
            normal_exit = wait_process_debug(
                process.0,
                Instant::now() + Duration::from_millis(wait_ms as u64),
                &mut debug_evidence,
            );
        }
        let exited_zero = if normal_exit {
            let mut code = 0u32;
            unsafe {
                windows::Win32::System::Threading::GetExitCodeProcess(process.0, &mut code).is_ok()
                    && code == 0
            }
        } else {
            false
        };
        #[cfg(test)]
        let exit_code = if normal_exit {
            let mut code = 0u32;
            unsafe { windows::Win32::System::Threading::GetExitCodeProcess(process.0, &mut code) }
                .ok()
                .map(|()| code)
        } else {
            None
        };
        let capture_valid = eof_at.is_some_and(|eof_at| eof_at <= deadline_at)
            && io_ok
            && !cancelled.load(Ordering::Acquire)
            && !overflow;
        let tree_completed =
            normal_exit && wait_empty_debug(job.0, deadline_at, &mut debug_evidence);
        let mut cleanup_confirmed = tree_completed;
        if !cleanup_confirmed {
            unsafe {
                let _ = TerminateJobObject(job.0, TERMINATED_EXIT);
            }
            cleanup_confirmed =
                wait_empty_debug(job.0, Instant::now() + CLEANUP_TIMEOUT, &mut debug_evidence);
            if !cleanup_confirmed {
                finish_debug_cleanup(job.0, &mut read, &mut debug_evidence);
                cleanup_confirmed = true;
            }
        }
        let io_cleaned = if eof {
            true
        } else {
            read.cancel_pending();
            let cleaned = confirm_read_debug(
                &mut read,
                Instant::now() + CLEANUP_TIMEOUT,
                &mut debug_evidence,
            );
            if !cleaned {
                finish_debug_cleanup(job.0, &mut read, &mut debug_evidence);
            }
            true
        };
        let identity_ok = same_identity(path, exe_lease.identity);
        let candidate_executable_hint = ArchiveFileHint {
            path: path.to_path_buf(),
            identity: ArchiveFileIdentity {
                volume_serial: exe_lease.identity.0,
                file_index: ((exe_lease.identity.1 as u64) << 32) | exe_lease.identity.2 as u64,
            },
        };
        drop(nul);
        if cleanup_confirmed && io_cleaned {
            drop(read);
            drop(exe_lease);
            drop(process);
            drop(job);
            Admission::release();
        } else {
            finish_debug_cleanup(job.0, &mut read, &mut debug_evidence);
            drop((read, exe_lease, process, job));
            Admission::release();
        }
        let binding = operation_binding(&debug_evidence);
        #[cfg(test)]
        if args.iter().any(|arg| {
            matches!(
                *arg,
                "--fixture-mode=success_with_dll" | "--fixture-mode=shim_with_dll"
            )
        }) {
            let image_events = debug_evidence
                .images
                .iter()
                .map(|(pid, image)| (*pid, image.as_ref().map(|hint| hint.path.clone())))
                .collect::<Vec<_>>();
            let dll_events = debug_evidence
                .zip_dlls
                .iter()
                .map(|(pid, dlls)| {
                    (
                        *pid,
                        dlls.iter()
                            .map(|hint| hint.path.clone())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            #[cfg(test)]
            let event_trace = &debug_evidence.fixture_event_trace;
            let captured_output = String::from_utf8_lossy(&output[..output.len().min(1024)]);
            eprintln!(
                "fixture DLL probe: CreateProcessW=completed normal_exit={normal_exit} exit_code={exit_code:?} exited_zero={exited_zero} tree_completed={tree_completed} cleanup_confirmed={cleanup_confirmed} io_cleaned={io_cleaned} capture_valid={capture_valid} identity_ok={identity_ok} ambiguous={} CREATE_PROCESS images={image_events:?} LOAD_DLL 7z.dll={dll_events:?} operation_binding={binding:?} captured_output={captured_output:?} debug_event_trace={event_trace:?}",
                debug_evidence.ambiguous,
            );
        }
        if exited_zero && tree_completed && cleanup_confirmed && io_cleaned && identity_ok {
            ProbeResult::usable_with_hints(
                if capture_valid {
                    parse_probe_description(&output)
                } else {
                    ProbeDescription::default()
                },
                ArchiveExecutionHints {
                    candidate_executable: Some(candidate_executable_hint),
                    loaded_zip_dll: binding.as_ref().map(|(_, dll)| dll.clone()),
                    operation_binding: binding.map(|(executable, zip_dll)| {
                        ArchiveOperationBinding {
                            executable,
                            zip_dll,
                        }
                    }),
                },
            )
        } else {
            fail()
        }
    }
}

#[cfg(all(windows, test))]
pub(crate) use windows_probe::{
    launch_sdk_test_child, run_worker_decode_drain_fixture_for_test,
    worker_fixture_descendant_alive_for_test, worker_fixture_descendant_stopped_for_test,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, thread, time::Duration};

    #[cfg(windows)]
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeGetTime() -> u32;
    }

    #[cfg(windows)]
    #[test]
    fn seven_zip_child_fixture() {
        let Some(mode) =
            std::env::args().find_map(|arg| arg.strip_prefix("--fixture-mode=").map(str::to_owned))
        else {
            return;
        };
        let trace_enabled = matches!(mode.as_str(), "shim_with_dll" | "success_with_dll");
        let marker = |stage: &str| {
            if trace_enabled {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                writeln!(out, "fixture marker: {stage}").unwrap();
                out.flush().unwrap();
            }
        };
        marker("test body entry");
        match mode.as_str() {
            "success" => {
                use std::io::Write;
                std::io::stdout()
                    .write_all(b"7-Zip 24.09 (x64)\nFormats:\n0  C........ 7z 7z\n1  ........ zip zip\nCodecs:\n")
                    .unwrap();
            }
            "success_with_dll" => {
                use std::io::Write;
                use std::os::windows::ffi::OsStrExt;
                let dll = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-dll=")
                            .map(PathBuf::from)
                    })
                    .expect("test DLL path is supplied");
                let dll_wide = dll
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect::<Vec<_>>();
                marker("success_with_dll before LoadLibraryW");
                unsafe {
                    windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(
                        dll_wide.as_ptr(),
                    ))
                    .expect("fixture DLL loads");
                }
                marker("success_with_dll after LoadLibraryW");
                let mut out = std::io::stdout().lock();
                out.write_all(b"7-Zip 24.09 (x64)\nFormats:\n0  C........ 7z 7z\nCodecs:\n")
                    .unwrap();
                out.flush().unwrap();
                marker("success_with_dll before normal return");
            }
            "operation_dynamic_no_dll"
            | "operation_dynamic_one_dll"
            | "operation_dynamic_two_dlls" => {
                use std::os::windows::ffi::OsStrExt;
                let argument = |name: &str| {
                    std::env::args_os()
                        .find_map(|arg| arg.to_str()?.strip_prefix(name).map(PathBuf::from))
                };
                let load_dll = |dll: &Path| {
                    let dll_wide = dll
                        .as_os_str()
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>();
                    unsafe {
                        windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(
                            dll_wide.as_ptr(),
                        ))
                        .expect("operation fixture DLL loads");
                    }
                };
                let marker_path =
                    argument("--fixture-marker=").expect("operation marker path is supplied");
                match mode.as_str() {
                    "operation_dynamic_no_dll" => {
                        std::fs::write(marker_path, b"fixture reached normal return").unwrap();
                    }
                    "operation_dynamic_one_dll" => {
                        let dll = argument("--fixture-dll=")
                            .expect("operation fixture DLL path is supplied");
                        load_dll(&dll);
                        std::fs::write(marker_path, b"fixture passed DLL load").unwrap();
                    }
                    "operation_dynamic_two_dlls" => {
                        let first = argument("--fixture-dll=")
                            .expect("first operation fixture DLL path is supplied");
                        let second = argument("--fixture-second-dll=")
                            .expect("second operation fixture DLL path is supplied");
                        load_dll(&first);
                        let first_marker = argument("--fixture-first-marker=")
                            .expect("first-load marker path is supplied");
                        std::fs::write(first_marker, b"first DLL loaded").unwrap();
                        load_dll(&second);
                        std::fs::write(marker_path, b"fixture passed both DLL loads").unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            "operation_gate_marker" => {
                let marker_path = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-marker=")
                            .map(PathBuf::from)
                    })
                    .expect("operation marker path is supplied");
                // `timeGetTime` forces the test image to import winmm.dll. The
                // fixture rewrites only that import name to 7z.dll so the
                // operation gate sees a statically loaded module.
                let _ = unsafe { timeGetTime() };
                std::fs::write(marker_path, b"entry reached").unwrap();
            }
            "shim_with_dll" => {
                use std::io::Write;
                let dll = std::env::args_os()
                    .find(|arg| arg.to_string_lossy().starts_with("--fixture-dll="))
                    .expect("test DLL path is supplied");
                let child = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-child=")
                            .map(PathBuf::from)
                    })
                    .expect("the original test image path is supplied");
                let mut out = std::io::stdout().lock();
                writeln!(out, "fixture shim: reached shim_with_dll body").unwrap();
                out.flush().unwrap();
                let mut command = Command::new(child);
                command
                    .args([
                        "--exact",
                        "archive_provider::tests::seven_zip_child_fixture",
                        "--nocapture",
                        "--",
                        "--fixture-mode=success_with_dll",
                    ])
                    .arg(dll);
                match command.spawn() {
                    Ok(mut child) => {
                        writeln!(
                            out,
                            "fixture shim: nested spawn succeeded pid={}",
                            child.id()
                        )
                        .unwrap();
                        out.flush().unwrap();
                        match child.wait() {
                            Ok(status) => {
                                writeln!(out, "fixture shim: nested wait returned {status}")
                                    .unwrap();
                                out.flush().unwrap();
                                assert!(status.success(), "nested fixture child failed: {status}");
                            }
                            Err(error) => {
                                writeln!(
                                    out,
                                    "fixture shim: nested wait failed raw_os_error={:?} error={error}",
                                    error.raw_os_error()
                                )
                                .unwrap();
                                out.flush().unwrap();
                                panic!("nested fixture child wait failed: {error}");
                            }
                        }
                    }
                    Err(error) => {
                        writeln!(
                            out,
                            "fixture shim: nested spawn failed raw_os_error={:?} error={error}",
                            error.raw_os_error()
                        )
                        .unwrap();
                        out.flush().unwrap();
                        panic!("nested fixture child failed to spawn: {error}");
                    }
                }
                marker("shim_with_dll before normal return");
            }
            "split" => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                out.write_all(b"7-Zip 24.").unwrap();
                thread::sleep(Duration::from_millis(15));
                out.write_all(b"09 (x64)\nFormats:\n0  C........ 7z 7z\nCodecs:\n")
                    .unwrap();
            }
            "zero_write" => {
                use std::{io::Write, os::windows::io::AsRawHandle};
                let mut out = std::io::stdout().lock();
                out.write_all(b"7-Zip 24.").unwrap();
                out.flush().unwrap();
                thread::sleep(Duration::from_millis(20));
                let stdout = std::io::stdout();
                let mut written = 0;
                unsafe {
                    windows::Win32::Storage::FileSystem::WriteFile(
                        windows::Win32::Foundation::HANDLE(stdout.as_raw_handle() as _),
                        Some(&[]),
                        Some(&mut written),
                        None,
                    )
                    .unwrap();
                }
                thread::sleep(Duration::from_millis(20));
                out.write_all(b"09 (x64)\nFormats:\n0  C........ 7z 7z\nCodecs:\n")
                    .unwrap();
            }
            "malformed" => {
                use std::io::Write;
                std::io::stdout()
                    .write_all(b"not a version\nFormats:\ninvalid\nCodecs:\n")
                    .unwrap();
            }
            "nonzero" => std::process::exit(7),
            "sleep" => thread::sleep(Duration::from_secs(30)),
            "flood" => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let block = [b'x'; 16 * 1024];
                for _ in 0..256 {
                    out.write_all(&block).unwrap();
                    std::io::stderr().write_all(&block).unwrap();
                }
            }
            "descendant" => {
                let exe = std::env::current_exe().unwrap();
                let child = Command::new(exe)
                    .args([
                        "--exact",
                        "archive_provider::tests::seven_zip_child_fixture",
                        "--nocapture",
                        "--",
                        "--fixture-mode=sleep",
                    ])
                    .spawn()
                    .unwrap();
                if let Some(marker) = std::env::args_os().find_map(|arg| {
                    arg.to_str()?
                        .strip_prefix("--fixture-marker=")
                        .map(PathBuf::from)
                }) {
                    std::fs::write(marker, child.id().to_string()).unwrap();
                }
            }
            "descendant_with_dll" | "descendant_error_with_dll" => {
                use std::os::windows::ffi::OsStrExt;
                let dll = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-dll=")
                            .map(PathBuf::from)
                    })
                    .expect("drain fixture DLL path is supplied");
                let dll_wide = dll
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect::<Vec<_>>();
                unsafe {
                    windows::Win32::System::LibraryLoader::LoadLibraryW(
                        windows::core::PCWSTR(dll_wide.as_ptr()),
                    )
                    .expect("drain fixture DLL loads");
                }
                let error_mode = mode == "descendant_error_with_dll";
                let exe = std::env::current_exe().unwrap();
                let mut command = Command::new(exe);
                command.args([
                    "--exact",
                    "archive_provider::tests::seven_zip_child_fixture",
                    "--nocapture",
                    "--",
                    if error_mode {
                        "--fixture-mode=sleep_for"
                    } else {
                        "--fixture-mode=sleep"
                    },
                ]);
                if error_mode {
                    command.arg("--fixture-sleep-ms=1500");
                }
                let child = command.spawn().expect("drain fixture descendant starts");
                let marker = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-marker=")
                            .map(PathBuf::from)
                    })
                    .expect("drain fixture marker path is supplied");
                std::fs::write(marker, child.id().to_string()).unwrap();
                if error_mode {
                    std::process::exit(9);
                }
            }
            "sleep_for" => {
                let millis = std::env::args_os()
                    .find_map(|arg| {
                        arg.to_str()?
                            .strip_prefix("--fixture-sleep-ms=")
                            .map(str::parse::<u64>)
                    })
                    .expect("sleep duration is supplied")
                    .expect("sleep duration is valid");
                thread::sleep(Duration::from_millis(millis));
            }
            _ => std::process::exit(9),
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "only launched as the isolated exact-handle SDK fixture"]
    fn seven_zip_sdk_child_fixture() {
        use std::{
            collections::HashSet,
            os::windows::io::{AsRawHandle, FromRawHandle, RawHandle},
            sync::atomic::AtomicBool,
        };
        let args = std::env::args().collect::<Vec<_>>();
        let Some(mode) = args.iter().find_map(|arg| arg.strip_prefix("--sdk-child=")) else {
            return;
        };
        let required = |name: &str| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(name))
                .expect("SDK fixture argument is present")
        };
        let dll = PathBuf::from(required("--sdk-dll="));
        let output = required("--sdk-output=").parse::<isize>().unwrap();
        let mut raw_items = Vec::new();
        let mut handles = HashSet::new();
        assert!(output != 0 && output != -1 && handles.insert(output));
        for value in args
            .iter()
            .filter_map(|arg| arg.strip_prefix("--sdk-item="))
        {
            let mut fields = value.splitn(4, '|');
            let kind = fields.next().unwrap();
            let source = fields.next().unwrap();
            let length = fields.next().unwrap().parse::<u64>().unwrap();
            let name = fields.next().unwrap();
            let directory = kind == "dir";
            let source = if directory {
                None
            } else {
                Some(source.parse::<isize>().unwrap())
            };
            if let Some(handle) = source {
                assert!(
                    handle != 0 && handle != -1 && handles.insert(handle),
                    "fixture handle must be unique"
                );
            }
            raw_items.push((name.to_owned(), directory, length, source));
        }
        let output = unsafe { std::fs::File::from_raw_handle(output as RawHandle) };
        let disposition = windows::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO {
            DeleteFile: windows::Win32::Foundation::BOOLEAN(1),
        };
        let disposition_ptr: *const windows::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO =
            &disposition;
        assert!(
            unsafe {
                windows::Win32::Storage::FileSystem::SetFileInformationByHandle(
                    windows::Win32::Foundation::HANDLE(output.as_raw_handle() as *mut _),
                    windows::Win32::Storage::FileSystem::FileDispositionInfo,
                    disposition_ptr.cast(),
                    std::mem::size_of_val(&disposition) as u32,
                )
            }
            .is_err(),
            "reduced candidate handle must not have DELETE access"
        );
        let items = raw_items
            .into_iter()
            .map(|(name, directory, length, source)| seven_zip_sdk::Item {
                name: name.encode_utf16().collect(),
                directory,
                length,
                source: source
                    .map(|handle| unsafe { std::fs::File::from_raw_handle(handle as RawHandle) }),
            })
            .collect();
        let output_probe = output.try_clone().unwrap();
        let cancelled = AtomicBool::new(mode == "cancel");
        let result = if mode == "bad-profile" {
            seven_zip_sdk::create_with_unsupported_test_profile(&dll, output, items, &cancelled)
        } else {
            seven_zip_sdk::create_from_handles(&dll, output, items, &cancelled)
        };
        if mode == "cancel" {
            assert_eq!(
                result,
                Err(seven_zip_sdk::E_ABORT),
                "cancellation must be returned from the SDK progress callback"
            );
        } else if mode == "bad-profile" {
            if result.is_ok() {
                std::process::exit(111);
            }
            let phase = seven_zip_sdk::LAST_CREATE_PHASE.load(std::sync::atomic::Ordering::Acquire);
            if phase != 10 {
                std::process::exit(120 + phase as i32);
            }
            if output_probe.metadata().unwrap().len() != 0 {
                std::process::exit(113);
            }
        } else {
            if let Err(hr) = result {
                std::process::exit(match hr {
                    value if value == seven_zip_sdk::E_INVALIDARG => 72,
                    value if value == seven_zip_sdk::E_FAIL => 71,
                    value if value == seven_zip_sdk::E_ABORT => 73,
                    value => 80 + (value.unsigned_abs() % 100) as i32,
                });
            }
        }
    }

    #[test]
    fn reports_unavailable_when_path_has_no_7zip() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            discover_from_path(
                Some(std::env::join_paths([directory.path()]).unwrap()),
                directory.path().to_path_buf()
            ),
            None
        );
        assert_eq!(
            discover_from_path(None, directory.path().to_path_buf()),
            None
        );
    }

    #[test]
    fn parses_only_complete_probe_descriptions() {
        let valid = parse_probe_description(
            b"7-Zip 24.09 (x64)\nFormats:\n0  C........ 7z 7z\n1  ........ zip zip\nCodecs:\n",
        );
        assert_eq!(valid.version.as_deref(), Some("24.09"));
        assert_eq!(
            valid.formats,
            Some(vec![
                SevenZipFormat {
                    name: "7z".into(),
                    flags: "C........".into()
                },
                SevenZipFormat {
                    name: "zip".into(),
                    flags: "........".into()
                },
            ])
        );
        assert_eq!(
            parse_probe_description(b"7-Zip 24.09\nFormats:\n0 broken\nCodecs:\n"),
            ProbeDescription {
                version: Some("24.09".into()),
                formats: None,
            }
        );
        assert_eq!(
            parse_probe_description(
                b"7-Zip 24.09\nFormats:\n0 C........ 7z 7z\n1 invalid\nCodecs:\n"
            )
            .formats,
            None
        );
        assert_eq!(
            parse_probe_description(b"7-Zip 24.09\nFormats:\n0 C........ 7z 7z\n").formats,
            None
        );
        assert_eq!(
            parse_probe_description(b"\xff"),
            ProbeDescription::default()
        );
        assert_eq!(
            parse_probe_description(b"7-Zip 24.09\0"),
            ProbeDescription::default()
        );
    }

    #[test]
    fn parses_real_24_09_format_rows_with_time_flags_and_blank_library_index() {
        let parsed = parse_probe_description(
            b"7-Zip 24.09 (x64)\nFormats:\n\
              0 C...F..........c.a.m+.. w...0  7z       7z 7z\n\
              0  ......................  APFS     apfs img\n\
                 CK.....O.....XC........  Hash     sha256\n\
            Codecs:\n",
        );
        let formats = parsed.formats.expect("complete realistic format section");
        assert_eq!(
            formats,
            vec![
                SevenZipFormat {
                    name: "7z".into(),
                    flags: "C...F..........c.a.m+..".into(),
                },
                SevenZipFormat {
                    name: "APFS".into(),
                    flags: "......................".into(),
                },
                SevenZipFormat {
                    name: "Hash".into(),
                    flags: "CK.....O.....XC........".into(),
                },
            ]
        );
        assert!(formats[0].supports_7z_creation());
        assert!(!formats[1].supports_7z_creation());
        assert!(!formats[2].supports_7z_creation());
    }

    #[test]
    fn rejects_malformed_or_truncated_real_format_metadata_rows() {
        for rows in [
            "0 C........ w...X 7z 7z\n",
            "0 C........ w... 7z 7z\n",
            "0 C........ w...0\n",
            "   CK.....O.....XC........\n",
        ] {
            let text = format!("7-Zip 24.09\nFormats:\n{rows}Codecs:\n");
            assert_eq!(parse_probe_description(text.as_bytes()).formats, None);
        }
    }

    #[test]
    fn recognizes_only_explicit_7z_create_capability() {
        let format = |name: &str, flags: &str| SevenZipFormat {
            name: name.into(),
            flags: flags.into(),
        };

        assert!(format("7z", "C........").supports_7z_creation());
        assert!(!format("zip", "C........").supports_7z_creation());
        assert!(!format("7z", "........").supports_7z_creation());
        assert!(!format("7z", "c........").supports_7z_creation());
        assert!(!format("7z", "").supports_7z_creation());
    }

    #[test]
    fn candidate_identity_is_not_an_operation_binding() {
        let hint = ArchiveFileHint {
            path: PathBuf::from(r"C:\tools\7z.exe"),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index: 2,
            },
        };
        let hints = ArchiveExecutionHints {
            candidate_executable: Some(hint),
            loaded_zip_dll: None,
            operation_binding: None,
        };
        assert!(hints.candidate_executable.is_some());
        assert!(hints.loaded_zip_dll.is_none());
        assert!(hints.operation_binding.is_none());
    }

    #[cfg(windows)]
    fn operation_binding_fixture(
        directory: &Path,
    ) -> (ArchiveProviderOwner, u64, ArchiveOperationBinding) {
        let executable_path = directory.join("7z.exe");
        let dll_path = directory.join("7z.dll");
        std::fs::write(&executable_path, b"exe fixture").unwrap();
        std::fs::write(&dll_path, b"dll fixture").unwrap();
        let binding = ArchiveOperationBinding {
            executable: super::windows_probe::test_file_hint(&executable_path),
            zip_dll: super::windows_probe::test_file_hint(&dll_path),
        };
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(SevenZipExecutable {
                path: executable_path,
                execution_hints: ArchiveExecutionHints {
                    candidate_executable: None,
                    loaded_zip_dll: Some(binding.zip_dll.clone()),
                    operation_binding: Some(binding.clone()),
                },
                version: None,
                formats: None,
            })
        ));
        (owner, revision, binding)
    }

    #[cfg(windows)]
    fn creation_binding_fixture(
        directory: &Path,
        formats: Option<Vec<SevenZipFormat>>,
    ) -> (ArchiveProviderOwner, u64, ArchiveOperationBinding) {
        let (owner, revision, binding) = operation_binding_fixture(directory);
        let mut data = owner.0.lock().unwrap();
        let ArchiveProviderState::Available(executable) = &mut data.snapshot.state else {
            unreachable!();
        };
        executable.formats = formats;
        drop(data);
        (owner, revision, binding)
    }

    #[cfg(windows)]
    fn create_capability() -> Vec<SevenZipFormat> {
        vec![SevenZipFormat {
            name: "7z".into(),
            flags: "C........".into(),
        }]
    }

    #[cfg(windows)]
    fn read_capability() -> Vec<SevenZipFormat> {
        vec![SevenZipFormat {
            name: "7z".into(),
            flags: ".........".into(),
        }]
    }

    #[cfg(windows)]
    #[test]
    fn provider_first_mismatch_rejects_success_and_finishes_current_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, _, _) = creation_binding_fixture(directory.path(), Some(read_capability()));
        let capability = owner.capture_7z_read_capability().unwrap();
        let location = crate::location::Location::Archive {
            source: PathBuf::from(r"C:\fixture\sample.7z"),
            components: vec![],
        };
        let mut session = crate::directory_session::DirectorySession::default();
        let first_ticket = session
            .navigate(location, None)
            .into_iter()
            .find_map(|effect| match effect {
                crate::directory_session::SessionEffect::EnumerateArchive(ticket) => Some(ticket),
                _ => None,
            })
            .unwrap();
        let previous = crate::archive_manifest::ArchiveManifest::validate([
            crate::archive_manifest::RawRecord {
                components: vec!["previous".into()],
                kind: crate::archive_manifest::NodeKind::Directory,
                declared_size: None,
            },
        ])
        .unwrap();
        assert!(session.commit_archive(
            &first_ticket,
            Ok(crate::archive_listing::ArchiveReadResult::test_result(
                previous.clone()
            ))
        ));
        let refresh = session.refresh().unwrap();
        let refresh_ticket = match refresh {
            crate::directory_session::SessionEffect::EnumerateArchive(ticket) => ticket,
            _ => panic!("expected archive refresh"),
        };
        let incoming = crate::archive_manifest::ArchiveManifest::validate([
            crate::archive_manifest::RawRecord {
                components: vec!["late".into()],
                kind: crate::archive_manifest::NodeKind::Directory,
                declared_size: None,
            },
        ])
        .unwrap();
        let result = crate::archive_listing::ArchiveReadResult::test_result(incoming);

        // Provider discovery linearizes first. The current session still receives one failure
        // completion, while the accepted rows remain the last good manifest.
        owner.begin_discovery();
        assert!(
            session
                .commit_archive_7z(&owner, Some(&capability), &refresh_ticket, Ok(result))
                .committed
        );
        assert!(matches!(
            session.status(),
            crate::directory_session::ListingStatus::RefreshFailed(_)
        ));
        assert_eq!(session.archive_manifest(), Some(&previous));
        assert!(
            !session
                .commit_archive_7z(
                    &owner,
                    Some(&capability),
                    &refresh_ticket,
                    Err(crate::directory::DirectoryError("duplicate".into())),
                )
                .committed
        );
    }

    #[cfg(windows)]
    #[test]
    fn current_read_capability_gates_a_b_a_reverse_refresh_duplicate_and_late_failure() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, _, _) = creation_binding_fixture(directory.path(), Some(read_capability()));
        let capability = owner.capture_7z_read_capability().unwrap();
        let location = crate::location::Location::Archive {
            source: PathBuf::from(r"C:\fixture\sample.7z"),
            components: vec![],
        };
        let ticket = |session: &mut crate::directory_session::DirectorySession,
                      location: crate::location::Location| {
            session
                .navigate(location, None)
                .into_iter()
                .find_map(|effect| match effect {
                    crate::directory_session::SessionEffect::EnumerateArchive(ticket) => {
                        Some(ticket)
                    }
                    _ => None,
                })
                .unwrap()
        };
        let manifest = |name: &str| {
            crate::archive_manifest::ArchiveManifest::validate([
                crate::archive_manifest::RawRecord {
                    components: vec![name.into()],
                    kind: crate::archive_manifest::NodeKind::Directory,
                    declared_size: None,
                },
            ])
            .unwrap()
        };
        let mut session = crate::directory_session::DirectorySession::default();
        let a_first = ticket(&mut session, location.clone());
        let _b = ticket(
            &mut session,
            crate::location::Location::Archive {
                source: PathBuf::from(r"C:\fixture\other.7z"),
                components: vec![],
            },
        );
        let a_second = ticket(&mut session, location);
        let pending_status = session.status().clone();
        let stale = session.commit_archive_7z(
            &owner,
            Some(&capability),
            &a_first,
            Ok(crate::archive_listing::ArchiveReadResult::test_result(
                manifest("stale"),
            )),
        );
        assert!(!stale.committed);
        assert!(session.archive_manifest().is_none());
        assert_eq!(*session.status(), pending_status);
        drop(stale);
        let late_failure = session.commit_archive_7z(
            &owner,
            Some(&capability),
            &a_first,
            Err(crate::directory::DirectoryError("late A failure".into())),
        );
        assert!(!late_failure.committed);
        assert!(session.archive_manifest().is_none());
        assert_eq!(*session.status(), pending_status);
        drop(late_failure);
        assert!(
            session
                .commit_archive_7z(
                    &owner,
                    Some(&capability),
                    &a_second,
                    Ok(crate::archive_listing::ArchiveReadResult::test_result(
                        manifest("second")
                    )),
                )
                .committed
        );
        let accepted_second = session.archive_manifest().unwrap().clone();

        let older_refresh = match session.refresh().unwrap() {
            crate::directory_session::SessionEffect::EnumerateArchive(ticket) => ticket,
            _ => panic!("expected refresh ticket"),
        };
        let newer_refresh = match session.refresh().unwrap() {
            crate::directory_session::SessionEffect::EnumerateArchive(ticket) => ticket,
            _ => panic!("expected refresh ticket"),
        };
        assert!(
            session
                .commit_archive_7z(
                    &owner,
                    Some(&capability),
                    &newer_refresh,
                    Ok(crate::archive_listing::ArchiveReadResult::test_result(
                        manifest("newest")
                    )),
                )
                .committed
        );
        let accepted_newest = session.archive_manifest().unwrap().clone();
        let duplicate = session.commit_archive_7z(
            &owner,
            Some(&capability),
            &newer_refresh,
            Ok(crate::archive_listing::ArchiveReadResult::test_result(
                manifest("duplicate"),
            )),
        );
        assert!(!duplicate.committed);
        drop(duplicate);
        let stale_refresh = session.commit_archive_7z(
            &owner,
            Some(&capability),
            &older_refresh,
            Ok(crate::archive_listing::ArchiveReadResult::test_result(
                manifest("late refresh"),
            )),
        );
        assert!(!stale_refresh.committed);
        drop(stale_refresh);
        assert_eq!(session.archive_manifest(), Some(&accepted_newest));
        assert_eq!(
            *session.status(),
            crate::directory_session::ListingStatus::Ready
        );
        assert_ne!(accepted_second, accepted_newest);
    }

    #[cfg(windows)]
    #[test]
    fn seven_zip_read_lease_requires_unique_current_read_capability() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, _) =
            creation_binding_fixture(directory.path(), Some(read_capability()));
        let capability = owner
            .capture_7z_read_capability()
            .expect("read capability does not require the create flag");
        assert_eq!(capability.revision, revision);
        assert!(owner.lease_7z_read(&capability).is_some());

        for formats in [
            None,
            Some(Vec::new()),
            Some(vec![SevenZipFormat {
                name: "zip".into(),
                flags: ".........".into(),
            }]),
            Some(vec![
                SevenZipFormat {
                    name: "7z".into(),
                    flags: ".........".into(),
                };
                2
            ]),
            Some(vec![SevenZipFormat {
                name: "7z".into(),
                flags: "bad".into(),
            }]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, _, _) = creation_binding_fixture(directory.path(), formats);
            assert!(owner.capture_7z_read_capability().is_none());
        }

        assert!(
            ArchiveProviderOwner::default()
                .lease_7z_read(&capability)
                .is_none()
        );
        let mut stale = capability.clone();
        stale.revision += 1;
        assert!(owner.lease_7z_read(&stale).is_none());
        let mut changed_binding = capability.clone();
        changed_binding.binding.zip_dll.identity.file_index ^= 1;
        assert!(owner.lease_7z_read(&changed_binding).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn seven_zip_read_lease_rechecks_state_after_route_open() {
        for change in ["revision", "binding", "capability"] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, _, _) = creation_binding_fixture(directory.path(), Some(read_capability()));
            let capability = owner.capture_7z_read_capability().unwrap();
            let result =
                super::windows_probe::lease_7z_read(&owner, &capability, || match change {
                    "revision" => {
                        owner.begin_discovery();
                    }
                    "binding" => {
                        let mut data = owner.0.lock().unwrap();
                        let ArchiveProviderState::Available(executable) = &mut data.snapshot.state
                        else {
                            unreachable!();
                        };
                        let mut changed = capability.binding.clone();
                        changed.zip_dll.identity.file_index ^= 1;
                        executable.execution_hints.operation_binding = Some(changed);
                    }
                    "capability" => {
                        let mut data = owner.0.lock().unwrap();
                        let ArchiveProviderState::Available(executable) = &mut data.snapshot.state
                        else {
                            unreachable!();
                        };
                        executable.formats = Some(vec![SevenZipFormat {
                            name: "7z".into(),
                            flags: "C........".into(),
                        }]);
                    }
                    _ => unreachable!(),
                });
            assert!(
                result.is_none(),
                "{change} change after route open is rejected"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn creation_lease_requires_current_binding_and_create_capability() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, binding) =
            creation_binding_fixture(directory.path(), Some(create_capability()));
        let lease = owner
            .lease_7z_creation(revision, &binding)
            .expect("exact current create binding is admitted");
        assert_eq!(lease.0.revision, revision);
        assert_eq!(lease.0.binding, binding);
        drop(lease);

        for formats in [
            None,
            Some(Vec::new()),
            Some(vec![SevenZipFormat {
                name: "zip".into(),
                flags: "C........".into(),
            }]),
            Some(vec![SevenZipFormat {
                name: "7z".into(),
                flags: "c........".into(),
            }]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, revision, binding) = creation_binding_fixture(directory.path(), formats);
            assert!(owner.lease_7z_creation(revision, &binding).is_none());
        }

        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, binding) =
            creation_binding_fixture(directory.path(), Some(create_capability()));
        assert!(owner.lease_7z_creation(revision + 1, &binding).is_none());
        let mut different_binding = binding.clone();
        different_binding.zip_dll.identity.file_index ^= 1;
        assert!(
            owner
                .lease_7z_creation(revision, &different_binding)
                .is_none()
        );
        assert!(
            ArchiveProviderOwner::default()
                .lease_7z_creation(0, &binding)
                .is_none()
        );
        {
            let mut data = owner.0.lock().unwrap();
            let ArchiveProviderState::Available(executable) = &mut data.snapshot.state else {
                unreachable!();
            };
            executable.execution_hints.operation_binding = None;
        }
        assert!(owner.lease_7z_creation(revision, &binding).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn creation_lease_rechecks_revision_binding_and_capability_after_route_open() {
        for change in ["revision", "binding", "capability"] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, revision, binding) =
                creation_binding_fixture(directory.path(), Some(create_capability()));
            let result =
                super::windows_probe::lease_7z_creation_binding(&owner, revision, &binding, || {
                    match change {
                        "revision" => {
                            assert_eq!(owner.begin_discovery(), revision + 1);
                        }
                        "binding" => {
                            let mut data = owner.0.lock().unwrap();
                            let ArchiveProviderState::Available(executable) =
                                &mut data.snapshot.state
                            else {
                                unreachable!();
                            };
                            let mut changed = binding.clone();
                            changed.zip_dll.identity.file_index ^= 1;
                            executable.execution_hints.operation_binding = Some(changed);
                        }
                        "capability" => {
                            let mut data = owner.0.lock().unwrap();
                            let ArchiveProviderState::Available(executable) =
                                &mut data.snapshot.state
                            else {
                                unreachable!();
                            };
                            executable.formats = Some(vec![SevenZipFormat {
                                name: "7z".into(),
                                flags: "........".into(),
                            }]);
                        }
                        _ => unreachable!(),
                    }
                });
            assert!(
                result.is_none(),
                "{change} change after route open is rejected"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn creation_lease_rejects_executable_or_dll_file_identity_mismatch() {
        for mismatch_executable in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, revision, binding) =
                creation_binding_fixture(directory.path(), Some(create_capability()));
            let mut changed_binding = binding.clone();
            let hint = if mismatch_executable {
                &mut changed_binding.executable
            } else {
                &mut changed_binding.zip_dll
            };
            hint.identity.file_index ^= 1;
            {
                let mut data = owner.0.lock().unwrap();
                let ArchiveProviderState::Available(executable) = &mut data.snapshot.state else {
                    unreachable!();
                };
                executable.execution_hints.operation_binding = Some(changed_binding.clone());
            }
            assert!(
                owner
                    .lease_7z_creation(revision, &changed_binding)
                    .is_none()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn accepted_creation_lease_pins_executable_and_dll_until_drop() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, binding) =
            creation_binding_fixture(directory.path(), Some(create_capability()));
        let lease = owner
            .lease_7z_creation(revision, &binding)
            .expect("exact current create binding is admitted");
        let moved_executable = directory.path().join("moved.exe");
        let moved_dll = directory.path().join("moved.dll");
        assert!(std::fs::rename(&binding.executable.path, &moved_executable).is_err());
        assert!(std::fs::rename(&binding.zip_dll.path, &moved_dll).is_err());
        drop(lease);
        std::fs::rename(&binding.executable.path, &moved_executable).unwrap();
        std::fs::rename(&moved_executable, &binding.executable.path).unwrap();
        std::fs::rename(&binding.zip_dll.path, &moved_dll).unwrap();
        std::fs::rename(&moved_dll, &binding.zip_dll.path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn operation_binding_lease_accepts_current_identity_and_retains_handles() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, binding) = operation_binding_fixture(directory.path());
        let lease = owner
            .lease_operation_binding(revision)
            .expect("current exact binding is accepted");
        assert_eq!(lease.revision, revision);
        assert_eq!(lease.binding, binding);

        assert!(
            std::fs::rename(
                &binding.executable.path,
                directory.path().join("renamed.exe")
            )
            .is_err()
        );
        assert!(
            std::fs::rename(&binding.zip_dll.path, directory.path().join("renamed.dll")).is_err()
        );
        drop(lease);
        std::fs::rename(
            &binding.executable.path,
            directory.path().join("renamed.exe"),
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn operation_binding_lease_rejects_stale_revision() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, _) = operation_binding_fixture(directory.path());
        assert!(owner.lease_operation_binding(revision + 1).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn operation_binding_lease_rejects_executable_or_dll_identity_mismatch() {
        for mismatch_executable in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let (owner, revision, mut binding) = operation_binding_fixture(directory.path());
            let hint = if mismatch_executable {
                &mut binding.executable
            } else {
                &mut binding.zip_dll
            };
            hint.identity.file_index ^= 1;
            let mut snapshot = owner.0.lock().unwrap();
            let ArchiveProviderState::Available(executable) = &mut snapshot.snapshot.state else {
                unreachable!();
            };
            executable.execution_hints.operation_binding = Some(binding);
            drop(snapshot);

            assert!(owner.lease_operation_binding(revision).is_none());
        }
    }

    #[cfg(windows)]
    #[test]
    fn operation_binding_lease_rejects_binding_changed_after_route_leases() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, mut changed_binding) = operation_binding_fixture(directory.path());
        changed_binding.zip_dll.identity.file_index ^= 1;

        let result = super::windows_probe::lease_operation_binding(&owner, revision, || {
            let mut data = owner.0.lock().unwrap();
            let ArchiveProviderState::Available(executable) = &mut data.snapshot.state else {
                unreachable!();
            };
            executable.execution_hints.operation_binding = Some(changed_binding.clone());
        });
        assert!(result.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn operation_binding_lease_rejects_revision_changed_after_route_leases() {
        let directory = tempfile::tempdir().unwrap();
        let (owner, revision, _) = operation_binding_fixture(directory.path());
        let result = super::windows_probe::lease_operation_binding(&owner, revision, || {
            assert_eq!(owner.begin_discovery(), revision + 1);
        });
        assert!(result.is_none());
    }

    #[test]
    fn relative_path_entry_uses_captured_current_directory() {
        let captured_cwd = PathBuf::from(r"C:\captured\working-directory");
        assert_eq!(
            resolve_path_entry(Path::new(r"tools\7zip"), &captured_cwd),
            Some(captured_cwd.join(r"tools\7zip"))
        );
        assert_eq!(
            resolve_path_entry(Path::new(r"..\outside"), &captured_cwd),
            None
        );
    }

    #[test]
    fn path_discovery_skips_an_unusable_earlier_executable() {
        use std::fs;
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first").join("7z.exe");
        let second = directory.path().join("second").join("7z.exe");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, b"unusable").unwrap();
        fs::write(&second, b"usable").unwrap();

        let found = first_usable_executable([first.clone(), second.clone()], |candidate| {
            candidate == second
        });

        assert_eq!(found, Some(second));
    }

    #[test]
    fn discovery_rejects_usable_result_after_overall_deadline() {
        let candidate = PathBuf::from("late-candidate");
        let found = discover_candidates(
            [candidate],
            std::time::Instant::now() + Duration::from_millis(10),
            |_, _| {
                thread::sleep(Duration::from_millis(20));
                ProbeResult::usable(ProbeDescription::default())
            },
        );
        assert_eq!(found, None);
    }

    #[test]
    fn stale_and_duplicate_discovery_results_cannot_commit() {
        let owner = ArchiveProviderOwner::default();
        let older = owner.begin_discovery();
        let newer = owner.begin_discovery();
        let executable = SevenZipExecutable {
            path: PathBuf::from("7z.exe"),
            execution_hints: ArchiveExecutionHints::default(),
            version: None,
            formats: None,
        };
        assert!(!owner.commit_discovery(older, Some(executable.clone())));
        assert!(owner.commit_discovery(newer, Some(executable.clone())));
        assert!(!owner.commit_discovery(newer, None));
    }

    #[cfg(windows)]
    #[test]
    fn debugger_initial_breakpoints_continue_once_per_process() {
        use std::collections::HashSet;
        use windows::Win32::Foundation::{
            DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED, EXCEPTION_BREAKPOINT,
        };

        let mut pending = HashSet::from([41, 42]);
        assert_eq!(
            super::windows_probe::exception_continue_status(
                &mut pending,
                41,
                EXCEPTION_BREAKPOINT,
                1,
            ),
            DBG_CONTINUE
        );
        assert_eq!(
            super::windows_probe::exception_continue_status(
                &mut pending,
                41,
                EXCEPTION_BREAKPOINT,
                1,
            ),
            DBG_EXCEPTION_NOT_HANDLED,
            "a later application breakpoint must be passed to the debuggee"
        );
        assert_eq!(
            super::windows_probe::exception_continue_status(
                &mut pending,
                42,
                EXCEPTION_BREAKPOINT,
                1,
            ),
            DBG_CONTINUE,
            "each descendant process receives its own initial-breakpoint allowance"
        );
        pending.insert(43);
        assert_eq!(
            super::windows_probe::exception_continue_status(
                &mut pending,
                43,
                EXCEPTION_BREAKPOINT,
                0,
            ),
            DBG_EXCEPTION_NOT_HANDLED,
            "second-chance breakpoints are not debugger startup events"
        );
    }

    #[cfg(windows)]
    #[test]
    fn fixture_probe_modes_are_contained_and_bounded() {
        let _probe_test_guard = super::lock_probe_tests();
        let current = std::env::current_exe().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let run = |mode: &str, timeout| {
            super::windows_probe::probe(
                &current,
                &cwd,
                &[
                    "--exact",
                    "archive_provider::tests::seven_zip_child_fixture",
                    "--nocapture",
                    "--",
                    &format!("--fixture-mode={mode}"),
                ],
                timeout,
            )
        };
        let describe = |mode: &str| {
            super::windows_probe::probe_with_description(
                &current,
                &cwd,
                &[
                    "--exact",
                    "archive_provider::tests::seven_zip_child_fixture",
                    "--nocapture",
                    "--",
                    &format!("--fixture-mode={mode}"),
                ],
                Duration::from_secs(5),
            )
        };
        assert_eq!(run("success", Duration::from_secs(5)), ProbeOutcome::Usable);
        let parsed = describe("success");
        assert_eq!(parsed.outcome, ProbeOutcome::Usable);
        assert_eq!(parsed.description.version.as_deref(), Some("24.09"));
        let candidate = parsed
            .execution_hints
            .candidate_executable
            .as_ref()
            .expect("probe retains the explicitly launched candidate fact");
        assert_eq!(candidate.path, current);
        assert_ne!(candidate.identity.file_index, 0);
        assert!(parsed.execution_hints.loaded_zip_dll.is_none());
        assert!(parsed.execution_hints.operation_binding.is_none());
        assert_eq!(
            parsed.description.formats.as_deref(),
            Some(
                [
                    SevenZipFormat {
                        name: "7z".into(),
                        flags: "C........".into()
                    },
                    SevenZipFormat {
                        name: "zip".into(),
                        flags: "........".into()
                    },
                ]
                .as_slice()
            )
        );
        let zero_write = describe("zero_write");
        assert_eq!(zero_write.outcome, ProbeOutcome::Usable);
        assert_eq!(zero_write.description.version.as_deref(), Some("24.09"));
        let split = describe("split");
        assert_eq!(split.outcome, ProbeOutcome::Usable);
        assert_eq!(split.description.version.as_deref(), Some("24.09"));
        let malformed = describe("malformed");
        assert_eq!(malformed.outcome, ProbeOutcome::Usable);
        assert_eq!(malformed.description, ProbeDescription::default());
        let overflow = describe("flood");
        assert_eq!(overflow.outcome, ProbeOutcome::Usable);
        assert_eq!(overflow.description, ProbeDescription::default());
        let injected_read_failure = super::windows_probe::probe_with_injected_read_failure(
            &current,
            &cwd,
            &[
                "--exact",
                "archive_provider::tests::seven_zip_child_fixture",
                "--nocapture",
                "--",
                "--fixture-mode=success",
            ],
            Duration::from_secs(5),
            1,
        );
        assert_eq!(injected_read_failure.outcome, ProbeOutcome::Usable);
        assert_eq!(
            injected_read_failure.description,
            ProbeDescription::default()
        );
        assert_eq!(
            run("nonzero", Duration::from_secs(5)),
            ProbeOutcome::Unusable
        );
        assert_eq!(run("flood", Duration::from_secs(5)), ProbeOutcome::Usable);
        let pending_before = super::windows_probe::PENDING_READ_SUBMISSIONS
            .load(std::sync::atomic::Ordering::Relaxed);
        super::windows_probe::READ_MAX_OUTSTANDING.store(0, std::sync::atomic::Ordering::Relaxed);
        let poll_timeouts_before =
            super::windows_probe::READ_POLL_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            super::windows_probe::probe(
                &current,
                &cwd,
                &[
                    "--exact",
                    "archive_provider::tests::seven_zip_child_fixture",
                    "--quiet",
                    "--",
                    "--fixture-mode=sleep",
                ],
                Duration::from_millis(200),
            ),
            ProbeOutcome::Unusable
        );
        assert!(
            super::windows_probe::PENDING_READ_SUBMISSIONS
                .load(std::sync::atomic::Ordering::Relaxed)
                - pending_before
                >= 1,
            "silent timeout must submit a pending OVERLAPPED request"
        );
        assert_eq!(
            super::windows_probe::READ_MAX_OUTSTANDING.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "polling must never overlap requests on the same OVERLAPPED storage"
        );
        assert!(
            super::windows_probe::READ_POLL_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed)
                - poll_timeouts_before
                > 1,
            "the same pending request must survive several poll timeouts"
        );
        assert_probe_slot_released();
        assert!(
            super::windows_probe::pending_read_cancel_is_confirmed_before_drop(),
            "cancellation must complete before OVERLAPPED and buffer storage are dropped"
        );
        assert!(
            super::windows_probe::unconfirmed_read_cleanup_retains_admission(),
            "unconfirmed cancellation must keep read storage and admission owned until reaped"
        );
        assert!(super::windows_probe::event_creation_failure_closes_pipe_once_and_releases_slot());
        assert_eq!(
            run("descendant", Duration::from_millis(300)),
            ProbeOutcome::Unusable
        );
        assert_probe_slot_released();
        shim_child_image_and_loaded_dll_are_bound_from_matching_debug_events();
        assert_eq!(
            super::windows_probe::DEBUG_EVENTS_WAITED.load(std::sync::atomic::Ordering::Relaxed),
            super::windows_probe::DEBUG_EVENTS_CONTINUED.load(std::sync::atomic::Ordering::Relaxed),
            "each received debug event is continued exactly once"
        );
        assert_eq!(
            super::windows_probe::DEBUG_FILES_SEEN.load(std::sync::atomic::Ordering::Relaxed),
            super::windows_probe::DEBUG_FILES_CLOSED.load(std::sync::atomic::Ordering::Relaxed),
            "debug-event file handles are closed before probe completion"
        );
    }

    #[cfg(windows)]
    fn shim_child_image_and_loaded_dll_are_bound_from_matching_debug_events() {
        use std::fs;
        let current = std::env::current_exe().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let dll = directory.path().join("7z.dll");
        let shim = directory.path().join("shim.exe");
        fs::copy(
            PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("winmm.dll"),
            &dll,
        )
        .expect("copy a loadable Windows DLL fixture");
        fs::copy(&current, &shim).unwrap();
        let dll_argument = format!("--fixture-dll={}", dll.display());
        let child_argument = format!("--fixture-child={}", current.display());
        let normalized = |path: &Path| {
            let value = path.to_string_lossy();
            value
                .strip_prefix(r"\\?\")
                .unwrap_or(value.as_ref())
                .to_ascii_lowercase()
        };
        let direct_args = [
            "--exact",
            "archive_provider::tests::seven_zip_child_fixture",
            "--nocapture",
            "--",
            "--fixture-mode=success_with_dll",
            dll_argument.as_str(),
        ];
        let direct = super::windows_probe::probe_with_description(
            &current,
            &cwd,
            &direct_args,
            Duration::from_secs(5),
        );
        assert_eq!(direct.outcome, ProbeOutcome::Usable);
        let direct_binding = direct
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("direct child loaded the exact 7z.dll image");
        assert_eq!(
            normalized(&direct_binding.executable.path),
            normalized(&current)
        );
        assert_eq!(normalized(&direct_binding.zip_dll.path), normalized(&dll));
        assert_eq!(
            direct.execution_hints.loaded_zip_dll.as_ref(),
            Some(&direct_binding.zip_dll)
        );

        let args = [
            "--exact",
            "archive_provider::tests::seven_zip_child_fixture",
            "--nocapture",
            "--",
            "--fixture-mode=shim_with_dll",
            dll_argument.as_str(),
            child_argument.as_str(),
        ];
        let result = super::windows_probe::probe_with_description(
            &shim,
            &cwd,
            &args,
            // テスト実行ファイルはhost instrumentationの注入を受けるため、nested起動だけ余裕を持たせる。
            Duration::from_secs(15),
        );
        assert_eq!(result.outcome, ProbeOutcome::Usable);
        assert_eq!(
            result
                .execution_hints
                .candidate_executable
                .as_ref()
                .map(|hint| hint.path.clone()),
            Some(shim)
        );
        let binding = result
            .execution_hints
            .operation_binding
            .as_ref()
            .expect("the child loaded this exact 7z.dll image");
        assert_eq!(normalized(&binding.executable.path), normalized(&current));
        assert_eq!(normalized(&binding.zip_dll.path), normalized(&dll));
        assert_eq!(
            result.execution_hints.loaded_zip_dll.as_ref(),
            Some(&binding.zip_dll)
        );
        assert_ne!(binding.executable.identity.file_index, 0);
        assert_ne!(binding.zip_dll.identity.file_index, 0);
        assert_probe_slot_released();
        assert_eq!(
            super::windows_probe::DEBUG_EVENTS_WAITED.load(std::sync::atomic::Ordering::Relaxed),
            super::windows_probe::DEBUG_EVENTS_CONTINUED.load(std::sync::atomic::Ordering::Relaxed)
        );
        assert_eq!(
            super::windows_probe::DEBUG_FILES_SEEN.load(std::sync::atomic::Ordering::Relaxed),
            super::windows_probe::DEBUG_FILES_CLOSED.load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[cfg(windows)]
    fn assert_probe_slot_released() {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while super::windows_probe::admission_is_held() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !super::windows_probe::admission_is_held(),
            "probe capacity released only after cleanup confirmation"
        );
    }

    #[cfg(windows)]
    #[test]
    fn executable_lease_blocks_replacement_for_probe_lifetime() {
        let _probe_test_guard = super::lock_probe_tests();
        use std::{
            fs,
            sync::{Arc, Mutex},
        };
        let current = std::env::current_exe().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("7z.exe");
        let moved = directory.path().join("replacement.exe");
        fs::copy(&current, &executable).unwrap();
        // Prove this disposable copy is replaceable before the runner acquires its route lease.
        fs::rename(&executable, &moved).unwrap();
        fs::rename(&moved, &executable).unwrap();
        let denied = Arc::new(Mutex::new(false));
        let observed = denied.clone();
        let source_path = executable.clone();
        let replacement_path = moved.clone();
        let args = [
            "--exact",
            "archive_provider::tests::seven_zip_child_fixture",
            "--nocapture",
            "--",
            "--fixture-mode=success",
        ];
        let success = super::windows_probe::probe_with_lease_hook(
            &executable,
            &cwd,
            &args,
            Duration::from_secs(5),
            move || {
                *observed.lock().unwrap() = fs::rename(&source_path, &replacement_path).is_err();
            },
        );
        assert_eq!(success, ProbeOutcome::Usable);
        assert!(*denied.lock().unwrap());
        // Replacement succeeds again after every route handle has been released.
        fs::rename(&executable, &moved).unwrap();
        fs::rename(&moved, &executable).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn executable_route_with_junction_fails_closed() {
        let _probe_test_guard = super::lock_probe_tests();
        use std::fs;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let junction = directory.path().join("route");
        fs::create_dir(&target).unwrap();
        fs::write(
            target.join("7z.exe"),
            b"must not be launched through a junction",
        )
        .unwrap();
        crate::operations::privileged::create_junction_direct(&target, &junction)
            .expect("create junction fixture");

        assert_eq!(
            discover_from_path(
                Some(std::env::join_paths([&junction]).unwrap()),
                directory.path().to_path_buf()
            ),
            None
        );
        assert!(!super::windows_probe::admission_is_held());
        fs::remove_dir(&junction).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn blocked_lease_hook_times_out_and_keeps_admission_until_released() {
        let _probe_test_guard = super::lock_probe_tests();
        let cwd = std::env::current_dir().unwrap();
        assert_probe_slot_released();
        let current = std::env::current_exe().unwrap();
        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let args = [
            "--exact",
            "archive_provider::tests::seven_zip_child_fixture",
        ];
        assert_eq!(
            super::windows_probe::probe_with_lease_hook(
                &current,
                &cwd,
                &args,
                Duration::from_millis(250),
                move || {
                    let _ = entered_sender.send(());
                    let _ = release_receiver.recv();
                },
            ),
            ProbeOutcome::Unusable
        );
        assert!(
            entered_receiver
                .recv_timeout(Duration::from_secs(2))
                .is_ok()
        );
        assert!(super::windows_probe::admission_is_held());
        assert_eq!(
            super::windows_probe::probe(&current, &cwd, &["i"], Duration::from_millis(100)),
            ProbeOutcome::AdmissionBusy
        );
        assert!(release_sender.send(()).is_ok());
        assert_probe_slot_released();
    }

    #[cfg(windows)]
    #[test]
    fn candidate_fallback_waits_for_slot_after_timeout() {
        let _probe_test_guard = super::lock_probe_tests();
        let current = std::env::current_exe().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut second_was_busy = false;
        let mut second_succeeded = false;
        let mut hold_inserted = false;
        let found = discover_candidates(
            [PathBuf::from("candidate-a"), PathBuf::from("candidate-b")],
            std::time::Instant::now() + Duration::from_secs(5),
            |candidate, timeout| {
                if candidate == Path::new("candidate-a") {
                    let timed_out = super::windows_probe::probe(
                        &current,
                        &cwd,
                        &[
                            "--exact",
                            "archive_provider::tests::seven_zip_child_fixture",
                            "--nocapture",
                            "--",
                            "--fixture-mode=descendant",
                        ],
                        timeout.min(Duration::from_millis(300)),
                    );
                    assert_eq!(timed_out, ProbeOutcome::Unusable);
                    hold_inserted =
                        super::windows_probe::hold_admission_for_test(Duration::from_millis(150));
                    ProbeResult::unusable()
                } else {
                    let result = super::windows_probe::probe(
                        &current,
                        &cwd,
                        &[
                            "--exact",
                            "archive_provider::tests::seven_zip_child_fixture",
                            "--nocapture",
                            "--",
                            "--fixture-mode=success",
                        ],
                        timeout,
                    );
                    second_was_busy |= result == ProbeOutcome::AdmissionBusy;
                    second_succeeded |= result == ProbeOutcome::Usable;
                    if result == ProbeOutcome::Usable {
                        ProbeResult::usable(ProbeDescription::default())
                    } else {
                        ProbeResult {
                            outcome: result,
                            description: ProbeDescription::default(),
                            execution_hints: ArchiveExecutionHints::default(),
                        }
                    }
                }
            },
        );
        assert_eq!(
            found.map(|(path, _, _)| path),
            Some(PathBuf::from("candidate-b"))
        );
        if hold_inserted {
            assert!(second_was_busy);
            assert!(second_succeeded);
        }
    }
}
