use super::JournalPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_ITEMS: usize = 1024;
const MAX_NAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipHandlePhase {
    CandidateCreateIntent,
    CandidateReady,
    StageCreateIntent,
    StageReady,
    CandidateCleaned,
    PublishIntent,
    Published,
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipHandleObjectState {
    Planned,
    CreateIntent,
    Owned,
    CreateFailed,
    NeverCreated,
    DeleteIntent,
    Deleted,
    Published,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipHandleIdentity {
    pub volume: u32,
    pub file_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipHandleSource {
    pub path: JournalPath,
    pub identity: JournalSevenZipHandleIdentity,
    pub entry_name: String,
    pub directory: bool,
    pub length: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipHandleEntry {
    pub name: String,
    pub directory: bool,
    pub source_index: Option<u32>,
    pub length: u64,
    pub source_identity: Option<JournalSevenZipHandleIdentity>,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipHandleObject {
    pub path: JournalPath,
    pub state: JournalSevenZipHandleObjectState,
    #[serde(default)]
    pub identity: Option<JournalSevenZipHandleIdentity>,
    #[serde(default)]
    pub length: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipHandleCreate {
    pub sources: Vec<JournalSevenZipHandleSource>,
    pub provider_revision: u64,
    pub executable: JournalPath,
    pub executable_identity: String,
    pub seven_zip_dll: JournalPath,
    pub seven_zip_dll_identity: String,
    pub output: JournalPath,
    pub output_parent_identity: String,
    pub manifest: Vec<JournalSevenZipHandleEntry>,
    pub candidate: JournalSevenZipHandleObject,
    pub stage: JournalSevenZipHandleObject,
    pub phase: JournalSevenZipHandlePhase,
    pub aborting: bool,
    pub sequence: u64,
    #[serde(default)]
    pub archive_identity: Option<JournalSevenZipHandleIdentity>,
}

impl JournalSevenZipHandleCreate {
    pub fn validate(&self) -> Result<(), String> {
        if self.sources.is_empty() || self.sources.len() > MAX_ITEMS {
            return Err("7z handle ledger source count is out of bounds".into());
        }
        if self.manifest.is_empty() || self.manifest.len() > MAX_ITEMS {
            return Err("7z handle ledger manifest count is out of bounds".into());
        }
        if self.sequence == 0 || self.provider_revision == 0 {
            return Err("7z handle ledger sequence or provider revision is zero".into());
        }
        let mut names = Vec::<&str>::new();
        let mut name_bytes = 0usize;
        let mut capture_payload_bytes = 0u64;
        let mut roots = HashSet::new();
        let mut root_counts = vec![0usize; self.sources.len()];
        for entry in &self.manifest {
            name_bytes = name_bytes
                .checked_add(entry.name.len())
                .ok_or("7z name size overflow")?;
            let components = entry.name.split('/').collect::<Vec<_>>();
            if name_bytes > MAX_NAME_BYTES
                || entry.name.is_empty()
                || entry.name.starts_with('/')
                || entry.name.contains('\\')
                || components.len() > 128
                || components
                    .iter()
                    .any(|component| !ordinary_component(component))
                || names
                    .iter()
                    .any(|name| ordinal_name_equal(name, &entry.name))
            {
                return Err(
                    "7z handle manifest names are empty, aliased, or over the limit".into(),
                );
            }
            names.push(&entry.name);
            if let Some(index) = entry.source_index {
                if index as usize >= self.sources.len() {
                    return Err("7z source association is out of range".into());
                }
                if components.len() == 1 {
                    roots.insert(index);
                    root_counts[index as usize] += 1;
                }
            }
            if entry.directory {
                if entry.source_identity.is_some() || entry.sha256.is_some() || entry.length != 0 {
                    return Err("7z directory manifest entry contains file metadata".into());
                }
            } else if entry.source_index.is_none()
                || entry.source_identity.is_none()
                || entry.sha256.as_deref().is_none_or(|s| !valid_sha256(s))
            {
                return Err("7z file manifest entry lacks captured source metadata".into());
            } else {
                capture_payload_bytes = capture_payload_bytes
                    .checked_add(entry.length)
                    .ok_or("7z captured payload size overflow")?;
            }
        }
        let maximum_archive_bytes = capture_payload_bytes
            .checked_mul(2)
            .and_then(|size| size.checked_add(8 * 1024 * 1024))
            .and_then(|size| size.checked_add(32))
            .ok_or("7z archive size bound overflow")?;
        for (index, source) in self.sources.iter().enumerate() {
            let root = self
                .manifest
                .iter()
                .find(|entry| ordinal_name_equal(&entry.name, &source.entry_name));
            let file_metadata_matches = source.directory
                || root.is_some_and(|entry| {
                    entry.length == source.length
                        && entry.source_identity.as_ref() == Some(&source.identity)
                        && entry.sha256.as_ref() == source.sha256.as_ref()
                });
            let digest_valid = if source.directory {
                source.sha256.is_none()
            } else {
                source.sha256.as_deref().is_some_and(valid_sha256)
            };
            if source.entry_name.is_empty()
                || !roots.contains(&(index as u32))
                || root_counts[index] != 1
                || root.is_none_or(|entry| {
                    entry.source_index != Some(index as u32) || entry.directory != source.directory
                })
                || !file_metadata_matches
                || !digest_valid
            {
                return Err("7z source roots or captured digests do not match the manifest".into());
            }
        }
        for entry in &self.manifest {
            let mut ancestor = String::new();
            let components = entry.name.split('/').collect::<Vec<_>>();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                if !ancestor.is_empty() {
                    ancestor.push('/');
                }
                ancestor.push_str(component);
                if self
                    .manifest
                    .iter()
                    .find(|candidate| ordinal_name_equal(&candidate.name, &ancestor))
                    .is_some_and(|candidate| !candidate.directory)
                {
                    return Err("7z file manifest entry is an ancestor of another entry".into());
                }
            }
        }
        for object in [&self.candidate, &self.stage] {
            match (object.state, &object.identity) {
                (
                    JournalSevenZipHandleObjectState::Owned
                    | JournalSevenZipHandleObjectState::DeleteIntent
                    | JournalSevenZipHandleObjectState::Deleted
                    | JournalSevenZipHandleObjectState::Published,
                    Some(_),
                ) => {}
                (
                    JournalSevenZipHandleObjectState::Planned
                    | JournalSevenZipHandleObjectState::CreateIntent
                    | JournalSevenZipHandleObjectState::CreateFailed
                    | JournalSevenZipHandleObjectState::NeverCreated,
                    None,
                ) => {}
                _ => return Err("7z object state and retained identity disagree".into()),
            }
        }
        if self.candidate.path == self.stage.path
            || self.output == self.candidate.path
            || self.output == self.stage.path
        {
            return Err("7z output, candidate, and stage paths must be distinct".into());
        }
        let output_parent = windows_parent(&self.output).ok_or("7z output path is not absolute")?;
        if windows_parent(&self.candidate.path).as_deref() != Some(output_parent.as_str())
            || windows_parent(&self.stage.path).as_deref() != Some(output_parent.as_str())
            || self
                .sources
                .iter()
                .any(|source| windows_parent(&source.path).is_none())
            || windows_parent(&self.executable).is_none()
            || windows_parent(&self.seven_zip_dll).is_none()
        {
            return Err("7z source/provider paths must be absolute and temporary objects must be output siblings".into());
        }
        if self.candidate.state == JournalSevenZipHandleObjectState::Published {
            return Err("7z candidate cannot be published".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipHandlePhase::CandidateReady
                | JournalSevenZipHandlePhase::StageCreateIntent
                | JournalSevenZipHandlePhase::StageReady
                | JournalSevenZipHandlePhase::CandidateCleaned
                | JournalSevenZipHandlePhase::PublishIntent
                | JournalSevenZipHandlePhase::Published
        ) && (self
            .candidate
            .length
            .is_none_or(|length| length > maximum_archive_bytes))
        {
            return Err("7z candidate length is absent or exceeds the captured-size bound".into());
        }
        if self.phase == JournalSevenZipHandlePhase::CandidateCreateIntent
            && self.candidate.length.is_some()
        {
            return Err("7z candidate length was recorded before readiness".into());
        }
        if !matches!(
            self.phase,
            JournalSevenZipHandlePhase::StageReady
                | JournalSevenZipHandlePhase::CandidateCleaned
                | JournalSevenZipHandlePhase::PublishIntent
                | JournalSevenZipHandlePhase::Published
                | JournalSevenZipHandlePhase::Discarded
        ) && self.stage.length.is_some()
        {
            return Err("7z stage length was recorded before verification".into());
        }
        if self.phase == JournalSevenZipHandlePhase::CandidateCreateIntent
            && !self.aborting
            && (!matches!(
                self.candidate.state,
                JournalSevenZipHandleObjectState::CreateIntent
                    | JournalSevenZipHandleObjectState::Owned
            ) || self.stage.state != JournalSevenZipHandleObjectState::Planned)
        {
            return Err("initial 7z handle state does not match candidate intent".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipHandlePhase::CandidateReady
                | JournalSevenZipHandlePhase::StageCreateIntent
        ) && !self.aborting
            && (self.candidate.state != JournalSevenZipHandleObjectState::Owned
                || self.candidate.length.is_none())
        {
            return Err("candidate-ready phase lacks a drained owned candidate".into());
        }
        if !self.aborting
            && self.phase == JournalSevenZipHandlePhase::CandidateReady
            && self.stage.state != JournalSevenZipHandleObjectState::Planned
        {
            return Err("candidate-ready phase has an unexpected stage state".into());
        }
        if !self.aborting
            && self.phase == JournalSevenZipHandlePhase::StageCreateIntent
            && !matches!(
                self.stage.state,
                JournalSevenZipHandleObjectState::CreateIntent
                    | JournalSevenZipHandleObjectState::Owned
            )
        {
            return Err("stage-create phase lacks its create intent".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipHandlePhase::StageReady
                | JournalSevenZipHandlePhase::CandidateCleaned
                | JournalSevenZipHandlePhase::PublishIntent
        ) && ((!self.aborting && self.stage.state != JournalSevenZipHandleObjectState::Owned)
            || (self.aborting
                && !matches!(
                    self.stage.state,
                    JournalSevenZipHandleObjectState::Owned
                        | JournalSevenZipHandleObjectState::DeleteIntent
                        | JournalSevenZipHandleObjectState::Deleted
                ))
            || self.stage.length != self.candidate.length
            || self
                .stage
                .length
                .is_none_or(|length| length > maximum_archive_bytes)
            || self.candidate.identity.is_none())
        {
            return Err("stage-ready phase lacks matching verified stage facts".into());
        }
        if self.phase == JournalSevenZipHandlePhase::Published
            && (self.stage.state != JournalSevenZipHandleObjectState::Published
                || self.candidate.state != JournalSevenZipHandleObjectState::Deleted
                || self.stage.length != self.candidate.length
                || self
                    .stage
                    .length
                    .is_none_or(|length| length > maximum_archive_bytes)
                || self.archive_identity.as_ref() != self.stage.identity.as_ref())
        {
            return Err("7z publication receipt does not match the retained stage".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipHandlePhase::CandidateCleaned
                | JournalSevenZipHandlePhase::PublishIntent
        ) && ((!self.aborting
            && (self.candidate.state != JournalSevenZipHandleObjectState::Deleted
                || self.stage.state != JournalSevenZipHandleObjectState::Owned))
            || (self.aborting
                && (!matches!(
                    self.candidate.state,
                    JournalSevenZipHandleObjectState::Deleted
                        | JournalSevenZipHandleObjectState::DeleteIntent
                ) || !matches!(
                    self.stage.state,
                    JournalSevenZipHandleObjectState::Owned
                        | JournalSevenZipHandleObjectState::DeleteIntent
                        | JournalSevenZipHandleObjectState::Deleted
                ))))
        {
            return Err("publication phase lacks candidate cleanup and owned stage".into());
        }
        if self.phase == JournalSevenZipHandlePhase::Discarded
            && (!self.aborting
                || !matches!(
                    self.candidate.state,
                    JournalSevenZipHandleObjectState::Deleted
                        | JournalSevenZipHandleObjectState::CreateFailed
                        | JournalSevenZipHandleObjectState::NeverCreated
                )
                || !matches!(
                    self.stage.state,
                    JournalSevenZipHandleObjectState::Deleted
                        | JournalSevenZipHandleObjectState::CreateFailed
                        | JournalSevenZipHandleObjectState::NeverCreated
                ))
        {
            return Err("discarded phase still has unresolved object ownership".into());
        }
        Ok(())
    }

    /// 受領記録は段階順ではなく、許可された一段の状態遷移だけを受け入れる。
    pub fn follows(&self, previous: &Self) -> bool {
        if self == previous {
            return true;
        }
        if previous.sequence.checked_add(1) != Some(self.sequence)
            || self.sources != previous.sources
            || self.provider_revision != previous.provider_revision
            || self.executable != previous.executable
            || self.executable_identity != previous.executable_identity
            || self.seven_zip_dll != previous.seven_zip_dll
            || self.seven_zip_dll_identity != previous.seven_zip_dll_identity
            || self.output != previous.output
            || self.output_parent_identity != previous.output_parent_identity
            || self.manifest != previous.manifest
            || (previous.aborting && !self.aborting)
        {
            return false;
        }
        let candidate_changed = self.candidate != previous.candidate;
        let stage_changed = self.stage != previous.stage;
        let phase_changed = self.phase != previous.phase;
        if candidate_changed && stage_changed {
            return false;
        }
        if !candidate_changed && !stage_changed && !phase_changed {
            return false;
        }
        if phase_changed
            && !matches!(
                (previous.phase, self.phase),
                (
                    JournalSevenZipHandlePhase::CandidateCreateIntent,
                    JournalSevenZipHandlePhase::CandidateReady
                ) | (
                    JournalSevenZipHandlePhase::CandidateReady,
                    JournalSevenZipHandlePhase::StageCreateIntent
                ) | (
                    JournalSevenZipHandlePhase::StageCreateIntent,
                    JournalSevenZipHandlePhase::StageReady
                ) | (
                    JournalSevenZipHandlePhase::StageReady,
                    JournalSevenZipHandlePhase::CandidateCleaned
                ) | (
                    JournalSevenZipHandlePhase::CandidateCleaned,
                    JournalSevenZipHandlePhase::PublishIntent
                ) | (
                    JournalSevenZipHandlePhase::PublishIntent,
                    JournalSevenZipHandlePhase::Published
                ) | (_, JournalSevenZipHandlePhase::Discarded)
            )
        {
            return false;
        }
        if phase_changed
            && (candidate_changed || stage_changed)
            && !matches!(
                (previous.phase, self.phase, candidate_changed, stage_changed),
                (
                    JournalSevenZipHandlePhase::CandidateCreateIntent,
                    JournalSevenZipHandlePhase::CandidateReady,
                    true,
                    false
                ) | (
                    JournalSevenZipHandlePhase::CandidateReady,
                    JournalSevenZipHandlePhase::StageCreateIntent,
                    false,
                    true
                ) | (
                    JournalSevenZipHandlePhase::StageCreateIntent,
                    JournalSevenZipHandlePhase::StageReady,
                    false,
                    true
                ) | (
                    JournalSevenZipHandlePhase::StageReady,
                    JournalSevenZipHandlePhase::CandidateCleaned,
                    true,
                    false
                ) | (
                    JournalSevenZipHandlePhase::PublishIntent,
                    JournalSevenZipHandlePhase::Published,
                    false,
                    true
                )
            )
        {
            return false;
        }
        if !candidate_changed && !stage_changed {
            if self.phase == JournalSevenZipHandlePhase::CandidateReady {
                return self.candidate.state == previous.candidate.state
                    && self.candidate.identity == previous.candidate.identity
                    && self.candidate.path == previous.candidate.path
                    && previous.candidate.length.is_none()
                    && self.candidate.length.is_some();
            }
            if self.phase == JournalSevenZipHandlePhase::StageReady {
                return self.stage.state == previous.stage.state
                    && self.stage.identity == previous.stage.identity
                    && self.stage.path == previous.stage.path
                    && previous.stage.length.is_none()
                    && self.stage.length == self.candidate.length;
            }
            if self.phase == JournalSevenZipHandlePhase::Published {
                return self.stage.state == JournalSevenZipHandleObjectState::Published
                    && self.archive_identity == self.stage.identity;
            }
            if self.phase == JournalSevenZipHandlePhase::Discarded {
                return self.aborting
                    && matches!(
                        self.candidate.state,
                        JournalSevenZipHandleObjectState::Deleted
                            | JournalSevenZipHandleObjectState::CreateFailed
                            | JournalSevenZipHandleObjectState::NeverCreated
                    )
                    && matches!(
                        self.stage.state,
                        JournalSevenZipHandleObjectState::Deleted
                            | JournalSevenZipHandleObjectState::CreateFailed
                            | JournalSevenZipHandleObjectState::NeverCreated
                    );
            }
            return true;
        }
        let (old, new) = if candidate_changed {
            (&previous.candidate, &self.candidate)
        } else {
            (&previous.stage, &self.stage)
        };
        if phase_changed
            && self.phase == JournalSevenZipHandlePhase::CandidateReady
            && candidate_changed
        {
            return old.state == JournalSevenZipHandleObjectState::Owned
                && new.state == old.state
                && old.identity == new.identity
                && old.path == new.path
                && old.length.is_none()
                && new.length.is_some();
        }
        if phase_changed && self.phase == JournalSevenZipHandlePhase::StageReady && stage_changed {
            return old.state == JournalSevenZipHandleObjectState::Owned
                && new.state == old.state
                && old.identity == new.identity
                && old.path == new.path
                && old.length.is_none()
                && new.length == self.candidate.length;
        }
        let valid = matches!(
            (old.state, new.state),
            (
                JournalSevenZipHandleObjectState::Planned,
                JournalSevenZipHandleObjectState::CreateIntent
            ) | (
                JournalSevenZipHandleObjectState::Planned,
                JournalSevenZipHandleObjectState::NeverCreated
            ) | (
                JournalSevenZipHandleObjectState::CreateIntent,
                JournalSevenZipHandleObjectState::Owned
            ) | (
                JournalSevenZipHandleObjectState::CreateIntent,
                JournalSevenZipHandleObjectState::CreateFailed
            ) | (
                JournalSevenZipHandleObjectState::CreateIntent,
                JournalSevenZipHandleObjectState::NeverCreated
            ) | (
                JournalSevenZipHandleObjectState::Owned,
                JournalSevenZipHandleObjectState::DeleteIntent
            ) | (
                JournalSevenZipHandleObjectState::DeleteIntent,
                JournalSevenZipHandleObjectState::Deleted
            ) | (
                JournalSevenZipHandleObjectState::Owned,
                JournalSevenZipHandleObjectState::Published
            )
        );
        if !valid || old.path != new.path {
            return false;
        }
        let identity_stable = old.identity.is_none() || old.identity == new.identity;
        if !identity_stable {
            return false;
        }
        if new.state == JournalSevenZipHandleObjectState::NeverCreated && !self.aborting {
            return false;
        }
        if new.state == JournalSevenZipHandleObjectState::Owned && new.length.is_some() {
            return false;
        }
        if old.length.is_some() && old.length != new.length {
            return false;
        }
        if new.state == JournalSevenZipHandleObjectState::Published {
            return !candidate_changed
                && self.phase == JournalSevenZipHandlePhase::Published
                && self.archive_identity == new.identity;
        }
        true
    }
}

fn ordinary_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with([' ', '.'])
    {
        return false;
    }
    if component
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ordinal_name_equal(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::CompareStringOrdinal;
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        // ZIPの名前衝突判定もWindowsのOrdinalIgnoreCaseに合わせる。
        unsafe { CompareStringOrdinal(&left, &right, true).0 == 2 }
    }
    #[cfg(not(windows))]
    {
        left.eq_ignore_ascii_case(right)
    }
}

fn windows_parent(path: &JournalPath) -> Option<String> {
    let decoded = String::from_utf16(&path.units).ok()?.replace('/', "\\");
    let bytes = decoded.as_bytes();
    let absolute = decoded.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\');
    if !absolute {
        return None;
    }
    let parent = decoded.rsplit_once('\\')?.0;
    if parent.is_empty() {
        return None;
    }
    Some(parent.to_uppercase())
}

#[cfg(test)]
pub(crate) fn fixture() -> JournalSevenZipHandleCreate {
    use super::{JournalPath, JournalSevenZipHandleIdentity as Identity};
    let path = |value: &str| JournalPath {
        units: value.encode_utf16().collect(),
    };
    let identity = Identity {
        volume: 7,
        file_index: 11,
    };
    JournalSevenZipHandleCreate {
        sources: vec![JournalSevenZipHandleSource {
            path: path(r"C:\source\root.txt"),
            identity: identity.clone(),
            entry_name: "root.txt".into(),
            directory: false,
            length: 3,
            sha256: Some("a".repeat(64)),
        }],
        provider_revision: 1,
        executable: path(r"C:\7z\7z.exe"),
        executable_identity: "exe".into(),
        seven_zip_dll: path(r"C:\7z\7z.dll"),
        seven_zip_dll_identity: "dll".into(),
        output: path(r"C:\out\archive.7z"),
        output_parent_identity: "parent".into(),
        manifest: vec![JournalSevenZipHandleEntry {
            name: "root.txt".into(),
            directory: false,
            source_index: Some(0),
            length: 3,
            source_identity: Some(identity.clone()),
            sha256: Some("a".repeat(64)),
        }],
        candidate: JournalSevenZipHandleObject {
            path: path(r"C:\out\candidate.tmp"),
            state: JournalSevenZipHandleObjectState::CreateIntent,
            identity: None,
            length: None,
        },
        stage: JournalSevenZipHandleObject {
            path: path(r"C:\out\stage.tmp"),
            state: JournalSevenZipHandleObjectState::Planned,
            identity: None,
            length: None,
        },
        phase: JournalSevenZipHandlePhase::CandidateCreateIntent,
        aborting: false,
        sequence: 1,
        archive_identity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_handle_ledger_is_valid_and_equal_snapshots_are_idempotent() {
        let ledger = fixture();
        ledger.validate().unwrap();
        assert!(ledger.follows(&ledger));
        let mut advanced = ledger.clone();
        advanced.sequence += 1;
        advanced.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 12,
        });
        advanced.candidate.state = JournalSevenZipHandleObjectState::Owned;
        assert!(advanced.follows(&ledger));
        let mut forged = advanced.clone();
        forged.sequence += 1;
        forged.candidate.identity.as_mut().unwrap().file_index += 1;
        assert!(!forged.follows(&advanced));

        let mut terminal_sequence = fixture();
        terminal_sequence.sequence = u64::MAX;
        let mut wrapped = terminal_sequence.clone();
        wrapped.candidate.state = JournalSevenZipHandleObjectState::Owned;
        wrapped.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 12,
        });
        assert!(terminal_sequence.follows(&terminal_sequence));
        assert!(!wrapped.follows(&terminal_sequence));
    }

    #[test]
    fn manifest_and_transition_validation_rejects_aliases_unsafe_paths_and_skipped_states() {
        let mut invalid = fixture();
        invalid.manifest[0].name = "../root.txt".into();
        assert!(invalid.validate().is_err());

        let mut invalid = fixture();
        invalid.manifest.push(invalid.manifest[0].clone());
        assert!(invalid.validate().is_err());

        let mut invalid = fixture();
        let mut descendant = invalid.manifest[0].clone();
        descendant.name = "root.txt/child".into();
        invalid.manifest.push(descendant);
        assert!(invalid.validate().is_err());

        let mut invalid = fixture();
        invalid.manifest[0].sha256 = Some("g".repeat(64));
        assert!(invalid.validate().is_err());

        let mut invalid = fixture();
        invalid
            .manifest
            .extend((0..MAX_ITEMS).map(|index| JournalSevenZipHandleEntry {
                name: format!("extra-{index}"),
                directory: true,
                source_index: None,
                length: 0,
                source_identity: None,
                sha256: None,
            }));
        assert!(invalid.validate().is_err());

        let original = fixture();
        let mut skipped = original.clone();
        skipped.sequence += 2;
        skipped.candidate.state = JournalSevenZipHandleObjectState::Owned;
        skipped.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 13,
        });
        assert!(!skipped.follows(&original));

        let mut premature = original.clone();
        premature.sequence += 1;
        premature.phase = JournalSevenZipHandlePhase::Published;
        assert!(!premature.follows(&original));
    }

    #[test]
    fn ready_archive_lengths_use_checked_captured_payload_bound() {
        use JournalSevenZipHandleObjectState as State;
        use JournalSevenZipHandlePhase as Phase;

        let mut overflow = fixture();
        overflow.sources[0].length = u64::MAX;
        overflow.manifest[0].length = u64::MAX;
        assert!(overflow.validate().is_err());

        let mut oversized_ready = fixture();
        oversized_ready.candidate.state = State::Owned;
        oversized_ready.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 12,
        });
        oversized_ready.candidate.length = Some(8 * 1024 * 1024 + 39);
        oversized_ready.phase = Phase::CandidateReady;
        assert!(oversized_ready.validate().is_err());
    }

    #[test]
    fn normal_candidate_stage_publish_graph_keeps_identities_stable() {
        use JournalSevenZipHandleObjectState as State;
        use JournalSevenZipHandlePhase as Phase;
        fn step(current: &mut JournalSevenZipHandleCreate, next: &mut JournalSevenZipHandleCreate) {
            next.sequence = current.sequence + 1;
            assert!(next.follows(current), "invalid transition: {:?}", next);
            next.validate().unwrap();
            *current = next.clone();
        }
        let mut current = fixture();
        let mut next = current.clone();
        next.candidate.state = State::Owned;
        next.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 12,
        });
        step(&mut current, &mut next);
        next.phase = Phase::CandidateReady;
        next.candidate.length = Some(3);
        step(&mut current, &mut next);
        next.phase = Phase::StageCreateIntent;
        next.stage.state = State::CreateIntent;
        step(&mut current, &mut next);
        next.stage.state = State::Owned;
        next.stage.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 13,
        });
        step(&mut current, &mut next);
        next.phase = Phase::StageReady;
        next.stage.length = Some(3);
        step(&mut current, &mut next);
        next.candidate.state = State::DeleteIntent;
        step(&mut current, &mut next);
        next.candidate.state = State::Deleted;
        step(&mut current, &mut next);
        next.phase = Phase::CandidateCleaned;
        step(&mut current, &mut next);
        next.phase = Phase::PublishIntent;
        step(&mut current, &mut next);
        next.phase = Phase::Published;
        next.stage.state = State::Published;
        next.archive_identity = next.stage.identity.clone();
        step(&mut current, &mut next);
    }

    #[test]
    fn aborted_create_requires_live_never_created_receipts_before_discarded() {
        use JournalSevenZipHandleObjectState as State;
        use JournalSevenZipHandlePhase as Phase;
        let mut current = fixture();
        let mut next = current.clone();
        next.sequence += 1;
        next.aborting = true;
        next.candidate.state = State::NeverCreated;
        assert!(next.follows(&current));
        next.validate().unwrap();
        current = next.clone();
        next.sequence += 1;
        next.stage.state = State::NeverCreated;
        assert!(next.follows(&current));
        next.validate().unwrap();
        current = next.clone();
        next.sequence += 1;
        next.phase = Phase::Discarded;
        assert!(next.follows(&current));
        next.validate().unwrap();
    }

    #[test]
    fn abort_cleanup_from_candidate_and_verified_stages_retains_exact_facts() {
        use JournalSevenZipHandleObjectState as State;
        use JournalSevenZipHandlePhase as Phase;

        fn advance(
            current: &mut JournalSevenZipHandleCreate,
            next: &mut JournalSevenZipHandleCreate,
        ) {
            next.sequence = current.sequence.checked_add(1).unwrap();
            assert!(next.follows(current), "invalid transition: {next:?}");
            next.validate().unwrap();
            *current = next.clone();
        }

        fn ready_stage() -> JournalSevenZipHandleCreate {
            let mut current = fixture();
            let mut next = current.clone();
            next.candidate.state = State::Owned;
            next.candidate.identity = Some(JournalSevenZipHandleIdentity {
                volume: 7,
                file_index: 12,
            });
            advance(&mut current, &mut next);
            next.phase = Phase::CandidateReady;
            next.candidate.length = Some(3);
            advance(&mut current, &mut next);
            next.phase = Phase::StageCreateIntent;
            next.stage.state = State::CreateIntent;
            advance(&mut current, &mut next);
            next.stage.state = State::Owned;
            next.stage.identity = Some(JournalSevenZipHandleIdentity {
                volume: 7,
                file_index: 13,
            });
            advance(&mut current, &mut next);
            next.phase = Phase::StageReady;
            next.stage.length = Some(3);
            advance(&mut current, &mut next);
            current
        }

        fn discard_owned_stage(mut current: JournalSevenZipHandleCreate, phase: Phase) {
            current.phase = phase;
            let candidate_identity = current.candidate.identity.clone();
            let stage_identity = current.stage.identity.clone();
            let candidate_length = current.candidate.length;
            let retained_stage_length = current.stage.length;
            if current.candidate.state == State::Owned {
                let mut next = current.clone();
                next.aborting = true;
                next.candidate.state = State::DeleteIntent;
                advance(&mut current, &mut next);
                next.candidate.state = State::Deleted;
                advance(&mut current, &mut next);
            }
            if current.stage.state == State::Owned {
                let mut next = current.clone();
                next.aborting = true;
                next.stage.state = State::DeleteIntent;
                advance(&mut current, &mut next);
                next.stage.state = State::Deleted;
                advance(&mut current, &mut next);
            } else if matches!(current.stage.state, State::Planned | State::CreateIntent) {
                let mut next = current.clone();
                next.aborting = true;
                next.stage.state = State::NeverCreated;
                advance(&mut current, &mut next);
            }
            let mut next = current.clone();
            next.phase = Phase::Discarded;
            advance(&mut current, &mut next);
            assert_eq!(current.candidate.identity, candidate_identity);
            assert_eq!(current.candidate.length, candidate_length);
            assert_eq!(current.stage.identity, stage_identity);
            assert_eq!(current.stage.length, retained_stage_length);
        }

        let mut candidate_ready = fixture();
        let mut next = candidate_ready.clone();
        next.candidate.state = State::Owned;
        next.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 12,
        });
        advance(&mut candidate_ready, &mut next);
        next.phase = Phase::CandidateReady;
        next.candidate.length = Some(3);
        advance(&mut candidate_ready, &mut next);
        let mut cleanup = candidate_ready.clone();
        cleanup.aborting = true;
        cleanup.candidate.state = State::DeleteIntent;
        advance(&mut candidate_ready, &mut cleanup);
        cleanup.candidate.state = State::Deleted;
        advance(&mut candidate_ready, &mut cleanup);
        cleanup.stage.state = State::NeverCreated;
        advance(&mut candidate_ready, &mut cleanup);
        cleanup.phase = Phase::Discarded;
        advance(&mut candidate_ready, &mut cleanup);
        assert_eq!(candidate_ready.candidate.length, Some(3));

        let stage_owned = {
            let mut current = fixture();
            let mut next = current.clone();
            next.candidate.state = State::Owned;
            next.candidate.identity = Some(JournalSevenZipHandleIdentity {
                volume: 7,
                file_index: 12,
            });
            advance(&mut current, &mut next);
            next.phase = Phase::CandidateReady;
            next.candidate.length = Some(3);
            advance(&mut current, &mut next);
            next.phase = Phase::StageCreateIntent;
            next.stage.state = State::CreateIntent;
            advance(&mut current, &mut next);
            next.stage.state = State::Owned;
            next.stage.identity = Some(JournalSevenZipHandleIdentity {
                volume: 7,
                file_index: 13,
            });
            advance(&mut current, &mut next);
            current
        };
        discard_owned_stage(stage_owned, Phase::StageCreateIntent);
        discard_owned_stage(ready_stage(), Phase::StageReady);
        let mut publish_intent = ready_stage();
        let mut next = publish_intent.clone();
        next.candidate.state = State::DeleteIntent;
        advance(&mut publish_intent, &mut next);
        next.candidate.state = State::Deleted;
        advance(&mut publish_intent, &mut next);
        next.phase = Phase::CandidateCleaned;
        advance(&mut publish_intent, &mut next);
        next.phase = Phase::PublishIntent;
        advance(&mut publish_intent, &mut next);
        discard_owned_stage(publish_intent, Phase::PublishIntent);
    }
}
