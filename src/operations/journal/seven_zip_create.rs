use super::JournalPath;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipCreatePhase {
    WorkspacePlanned,
    WorkspaceOwned,
    CandidateCreateIntent,
    CandidateReady,
    StagePlanned,
    StageOwned,
    StageReady,
    WorkspaceCleaned,
    PublishIntent,
    Published,
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipObjectKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipSourceKind {
    File,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipObjectRole {
    WorkspaceRoot,
    InputContainer,
    InputDirectory,
    InputFile,
    Listfile,
    CandidateArchive,
    StageArchive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipCreateSource {
    pub source: JournalPath,
    pub source_identity: String,
    pub source_kind: JournalSevenZipSourceKind,
    pub entry_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipManifestEntry {
    pub id: u32,
    pub relative_path: String,
    pub kind: JournalSevenZipObjectKind,
    pub role: JournalSevenZipObjectRole,
    pub source_index: Option<u32>,
    pub source_relative_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipObservationState {
    Planned,
    CreateIntent,
    Owned,
    DeleteIntent,
    Deleted,
    Published,
    CreateFailed,
    NeverCreated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipObservation {
    pub object_id: u32,
    pub state: JournalSevenZipObservationState,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipPendingAction {
    pub object_id: u32,
    pub action: JournalSevenZipObservationState,
    #[serde(default)]
    pub expected_identity: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipCreate {
    pub sources: Vec<JournalSevenZipCreateSource>,
    pub provider_revision: u64,
    pub executable: JournalPath,
    pub executable_identity: String,
    pub seven_zip_dll: JournalPath,
    pub seven_zip_dll_identity: String,
    pub output: JournalPath,
    pub output_parent_identity: String,
    pub workspace: JournalPath,
    pub workspace_parent_identity: String,
    pub stage: JournalPath,
    pub manifest: Vec<JournalSevenZipManifestEntry>,
    pub observations: Vec<JournalSevenZipObservation>,
    pub phase: JournalSevenZipCreatePhase,
    pub aborting: bool,
    pub sequence: u64,
    #[serde(default)]
    pub pending_action: Option<JournalSevenZipPendingAction>,
    #[serde(default)]
    pub output_identity: Option<String>,
}

impl JournalSevenZipCreate {
    pub fn validate(&self) -> Result<(), String> {
        if self.sources.is_empty() || self.sources.len() > 1024 {
            return Err("7z ledger needs 1 to 1024 ordered sources".into());
        }
        if self.manifest.len() > 1029
            || self.manifest.is_empty()
            || self
                .manifest
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.role,
                        JournalSevenZipObjectRole::InputDirectory
                            | JournalSevenZipObjectRole::InputFile
                    )
                })
                .count()
                > 1024
        {
            return Err("7z manifest exceeds its fixed object bound".into());
        }
        if self.sequence == 0
            || self.executable_identity.is_empty()
            || self.seven_zip_dll_identity.is_empty()
            || self.output_parent_identity.is_empty()
            || self.workspace_parent_identity.is_empty()
        {
            return Err("7z ledger identity or sequence is invalid".into());
        }
        for path in [
            &self.executable,
            &self.seven_zip_dll,
            &self.output,
            &self.workspace,
            &self.stage,
        ] {
            if !valid_absolute_windows_path(path) {
                return Err("7z captured provider and operation paths must be bounded, absolute, and NUL-free".into());
            }
        }
        if self
            .sources
            .iter()
            .any(|source| !valid_absolute_windows_path(&source.source))
        {
            return Err("7z source paths must be bounded, absolute, and NUL-free".into());
        }
        if self.sequence == 1
            && (self.phase != JournalSevenZipCreatePhase::WorkspacePlanned
                || self.aborting
                || self.pending_action.is_some()
                || self.observations.len() != self.manifest.len()
                || self.observations.iter().any(|fact| {
                    fact.state != JournalSevenZipObservationState::Planned
                        || fact.identity.is_some()
                        || fact.length.is_some()
                        || fact.sha256.is_some()
                }))
        {
            return Err(
                "7z initial durable plan must contain only planned object observations".into(),
            );
        }
        if self.executable_identity.len() > 256
            || self.seven_zip_dll_identity.len() > 256
            || self.output_parent_identity.len() > 256
            || self.workspace_parent_identity.len() > 256
            || self.sources.iter().any(|source| {
                source.source_identity.len() > 256
                    || !super::super::model::seven_zip_windows_component(&source.entry_name)
            })
        {
            return Err("7z captured identity or source name exceeds its bound".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut paths: Vec<String> = Vec::new();
        let mut directories = std::collections::BTreeSet::new();
        let mut fixed_roles = std::collections::BTreeSet::new();
        let mut name_bytes = 0usize;
        for entry in &self.manifest {
            if !ids.insert(entry.id) || !safe_relative_path(&entry.relative_path) {
                return Err("7z manifest has an invalid ID or relative path".into());
            }
            let archive_name = entry.source_relative_path.as_ref().and_then(|relative| {
                self.sources
                    .get(entry.source_index.unwrap_or(u32::MAX) as usize)
                    .map(|source| {
                        if relative.is_empty() {
                            source.entry_name.clone()
                        } else {
                            format!("{}/{relative}", source.entry_name)
                        }
                    })
            });
            let charged_name = archive_name.as_deref().unwrap_or(&entry.relative_path);
            name_bytes = name_bytes.saturating_add(charged_name.len());
            if name_bytes > 2 * 1024 * 1024 {
                return Err("7z manifest names exceed the UTF-8 budget".into());
            }
            if archive_name
                .as_deref()
                .unwrap_or(&entry.relative_path)
                .split('/')
                .count()
                > 128
            {
                return Err("7z manifest path exceeds the depth bound".into());
            }
            let folded = entry.relative_path.to_lowercase();
            if paths
                .iter()
                .any(|existing| windows_ordinal_equal(existing, &entry.relative_path))
            {
                return Err("7z manifest paths collide".into());
            }
            paths.push(entry.relative_path.clone());
            if entry.kind == JournalSevenZipObjectKind::Directory {
                directories.insert(folded);
            }
            if !matches!(
                entry.role,
                JournalSevenZipObjectRole::InputFile | JournalSevenZipObjectRole::InputDirectory
            ) && !fixed_roles.insert(entry.role)
            {
                return Err("7z manifest repeats a fixed object role".into());
            }
            if entry.source_index.is_some() != entry.source_relative_path.is_some()
                || entry
                    .source_index
                    .is_some_and(|index| index as usize >= self.sources.len())
                || entry
                    .source_relative_path
                    .as_ref()
                    .is_some_and(|path| !path.is_empty() && !safe_relative_path(path))
            {
                return Err("7z manifest source link is invalid".into());
            }
            if !matches!(
                entry.role,
                JournalSevenZipObjectRole::InputDirectory | JournalSevenZipObjectRole::InputFile
            ) && (entry.source_index.is_some() || entry.source_relative_path.is_some())
            {
                return Err("fixed 7z workspace roles cannot carry source links".into());
            }
            if entry.kind == JournalSevenZipObjectKind::File
                && matches!(
                    entry.role,
                    JournalSevenZipObjectRole::InputDirectory
                        | JournalSevenZipObjectRole::WorkspaceRoot
                )
            {
                return Err("7z directory role has file kind".into());
            }
        }
        for (index, source) in self.sources.iter().enumerate() {
            if !self.manifest.iter().any(|entry| {
                entry.source_index == Some(index as u32)
                    && entry.source_relative_path.as_deref() == Some("")
                    && windows_ordinal_equal(
                        entry.relative_path.rsplit('/').next().unwrap_or_default(),
                        &source.entry_name,
                    )
            }) {
                return Err(
                    "7z source has no materialized root preserving its captured name".into(),
                );
            }
        }
        for path in &paths {
            let mut parent = relative_parent(path);
            while let Some(name) = parent {
                if !directories.contains(&name.to_lowercase()) {
                    return Err("7z manifest parent is absent or is not a directory".into());
                }
                parent = relative_parent(name);
            }
        }
        for role in [
            JournalSevenZipObjectRole::WorkspaceRoot,
            JournalSevenZipObjectRole::InputContainer,
            JournalSevenZipObjectRole::Listfile,
            JournalSevenZipObjectRole::CandidateArchive,
            JournalSevenZipObjectRole::StageArchive,
        ] {
            if !fixed_roles.contains(&role) {
                return Err("7z manifest is missing a fixed workspace object".into());
            }
        }
        for entry in &self.manifest {
            let expected = match entry.role {
                JournalSevenZipObjectRole::WorkspaceRoot => {
                    Some(("workspace", JournalSevenZipObjectKind::Directory))
                }
                JournalSevenZipObjectRole::InputContainer => {
                    Some(("workspace/input", JournalSevenZipObjectKind::Directory))
                }
                JournalSevenZipObjectRole::Listfile => {
                    Some(("workspace/inputs.txt", JournalSevenZipObjectKind::File))
                }
                JournalSevenZipObjectRole::CandidateArchive => {
                    Some(("workspace/candidate.7z", JournalSevenZipObjectKind::File))
                }
                JournalSevenZipObjectRole::StageArchive => {
                    Some(("stage.7z", JournalSevenZipObjectKind::File))
                }
                JournalSevenZipObjectRole::InputDirectory
                | JournalSevenZipObjectRole::InputFile => None,
            };
            if expected
                .is_some_and(|(path, kind)| entry.relative_path != path || entry.kind != kind)
            {
                return Err("7z fixed workspace object has an unexpected path or kind".into());
            }
            if matches!(
                entry.role,
                JournalSevenZipObjectRole::InputDirectory | JournalSevenZipObjectRole::InputFile
            ) && !entry.relative_path.starts_with("workspace/input/")
            {
                return Err("7z captured input escaped its private input container".into());
            }
            if matches!(
                entry.role,
                JournalSevenZipObjectRole::InputDirectory | JournalSevenZipObjectRole::InputFile
            ) {
                let index = entry
                    .source_index
                    .ok_or("7z materialized input has no source index")?
                    as usize;
                let relative = entry
                    .source_relative_path
                    .as_deref()
                    .ok_or("7z materialized input has no source path")?;
                let expected_kind = match self.sources[index].source_kind {
                    JournalSevenZipSourceKind::File => JournalSevenZipObjectRole::InputFile,
                    JournalSevenZipSourceKind::Folder => JournalSevenZipObjectRole::InputDirectory,
                };
                if relative.is_empty() && entry.role != expected_kind {
                    return Err("7z source root has the wrong materialized kind".into());
                }
                if relative.is_empty() {
                    let leaf = entry.relative_path.rsplit('/').next().unwrap_or_default();
                    if leaf != self.sources[index].entry_name {
                        return Err(
                            "7z source root does not preserve its captured entry name".into()
                        );
                    }
                } else if self.sources[index].source_kind == JournalSevenZipSourceKind::File {
                    return Err("7z file source cannot have descendants".into());
                }
                let expected = if relative.is_empty() {
                    format!("workspace/input/{}", self.sources[index].entry_name)
                } else {
                    format!(
                        "workspace/input/{}/{relative}",
                        self.sources[index].entry_name
                    )
                };
                if entry.relative_path != expected {
                    return Err(
                        "7z materialized path does not exactly match its source link".into(),
                    );
                }
                let expected_role = match entry.kind {
                    JournalSevenZipObjectKind::File => JournalSevenZipObjectRole::InputFile,
                    JournalSevenZipObjectKind::Directory => {
                        JournalSevenZipObjectRole::InputDirectory
                    }
                };
                if entry.role != expected_role {
                    return Err("7z materialized role and kind disagree".into());
                }
            }
        }
        let mut observation_ids = std::collections::BTreeSet::new();
        for observation in &self.observations {
            if !ids.contains(&observation.object_id)
                || !observation_ids.insert(observation.object_id)
            {
                return Err("7z observation references unknown or duplicate object".into());
            }
            if matches!(
                observation.state,
                JournalSevenZipObservationState::Owned
                    | JournalSevenZipObservationState::DeleteIntent
                    | JournalSevenZipObservationState::Deleted
            ) && observation.identity.as_ref().is_none_or(String::is_empty)
            {
                return Err("7z owned observation needs an identity".into());
            }
            if observation
                .identity
                .as_ref()
                .is_some_and(|identity| identity.len() > 256)
            {
                return Err("7z observed identity exceeds its serialized growth reserve".into());
            }
            if observation.sha256.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                return Err("7z input digest is malformed".into());
            }
            let role = self
                .manifest
                .iter()
                .find(|entry| entry.id == observation.object_id)
                .map(|entry| entry.role);
            if (observation.length.is_some() || observation.sha256.is_some())
                && role != Some(JournalSevenZipObjectRole::InputFile)
            {
                return Err("only captured input files may carry content facts".into());
            }
            if observation.state == JournalSevenZipObservationState::Published
                && role != Some(JournalSevenZipObjectRole::StageArchive)
            {
                return Err("only the verified stage may become published".into());
            }
        }
        if self.aborting && self.phase == JournalSevenZipCreatePhase::Published {
            return Err("7z abort cannot publish".into());
        }
        if self.phase == JournalSevenZipCreatePhase::Discarded && !self.aborting {
            return Err("7z discard is only valid after the abort latch".into());
        }
        if self.phase == JournalSevenZipCreatePhase::Published
            && self.output_identity.as_ref().is_none_or(String::is_empty)
        {
            return Err("published 7z output needs an observed identity".into());
        }
        if self.phase != JournalSevenZipCreatePhase::Published && self.output_identity.is_some() {
            return Err("7z output identity may only be assigned after publication".into());
        }
        if self
            .output_identity
            .as_ref()
            .is_some_and(|identity| identity.len() > 256)
        {
            return Err("7z output identity exceeds its serialized growth bound".into());
        }
        if self
            .pending_action
            .as_ref()
            .is_some_and(|action| !ids.contains(&action.object_id))
        {
            return Err("7z pending action references an unknown object".into());
        }
        if self.pending_action.as_ref().is_some_and(|action| {
            !matches!(
                action.action,
                JournalSevenZipObservationState::CreateIntent
                    | JournalSevenZipObservationState::DeleteIntent
            ) || (action.action == JournalSevenZipObservationState::DeleteIntent
                && action
                    .expected_identity
                    .as_ref()
                    .is_none_or(String::is_empty))
                || (action.action == JournalSevenZipObservationState::CreateIntent
                    && action.expected_identity.is_some())
        }) {
            return Err("7z pending action is not permitted in the current abort state".into());
        }
        if let Some(action) = &self.pending_action {
            let current = self
                .observations
                .iter()
                .find(|fact| fact.object_id == action.object_id);
            if action.action == JournalSevenZipObservationState::CreateIntent
                && current
                    .is_none_or(|fact| fact.state != JournalSevenZipObservationState::CreateIntent)
                || action.action == JournalSevenZipObservationState::DeleteIntent
                    && current.is_none_or(|fact| {
                        fact.state != JournalSevenZipObservationState::DeleteIntent
                            || fact.identity.as_ref() != action.expected_identity.as_ref()
                    })
            {
                return Err(
                    "7z action intent does not match the object's exact prior state".into(),
                );
            }
        }
        let observation = |role| {
            self.manifest
                .iter()
                .find(|entry| entry.role == role)
                .and_then(|entry| {
                    self.observations
                        .iter()
                        .find(|fact| fact.object_id == entry.id)
                })
        };
        if self.phase == JournalSevenZipCreatePhase::WorkspaceOwned
            && observation(JournalSevenZipObjectRole::WorkspaceRoot).is_none_or(|fact| {
                if self.aborting {
                    !matches!(
                        fact.state,
                        JournalSevenZipObservationState::Owned
                            | JournalSevenZipObservationState::DeleteIntent
                            | JournalSevenZipObservationState::Deleted
                    )
                } else {
                    fact.state != JournalSevenZipObservationState::Owned
                }
            })
        {
            return Err(
                "7z owned workspace phase needs its exact root identity observation".into(),
            );
        }
        if !self.aborting
            && self.phase == JournalSevenZipCreatePhase::CandidateCreateIntent
            && self.pending_action.as_ref().is_none_or(|action| {
                self.manifest
                    .iter()
                    .find(|entry| entry.role == JournalSevenZipObjectRole::CandidateArchive)
                    .is_none_or(|candidate| {
                        action.object_id != candidate.id
                            || action.action != JournalSevenZipObservationState::CreateIntent
                            || self
                                .observations
                                .iter()
                                .find(|fact| fact.object_id == candidate.id)
                                .is_none_or(|fact| {
                                    fact.state != JournalSevenZipObservationState::CreateIntent
                                })
                    })
            })
        {
            return Err("candidate creation phase needs its exact durable create intent".into());
        }
        if !self.aborting
            && matches!(
                self.phase,
                JournalSevenZipCreatePhase::WorkspaceCleaned
                    | JournalSevenZipCreatePhase::PublishIntent
                    | JournalSevenZipCreatePhase::Published
            )
            && self
                .manifest
                .iter()
                .filter(|entry| entry.role != JournalSevenZipObjectRole::StageArchive)
                .any(|entry| {
                    self.observations
                        .iter()
                        .find(|fact| fact.object_id == entry.id)
                        .is_none_or(|fact| fact.state != JournalSevenZipObservationState::Deleted)
                })
        {
            return Err("7z workspace cleanup phase retains unresolved workspace objects".into());
        }
        if self.phase == JournalSevenZipCreatePhase::Discarded
            && self.manifest.iter().any(|entry| {
                self.observations
                    .iter()
                    .find(|fact| fact.object_id == entry.id)
                    .is_none_or(|fact| {
                        !matches!(
                            fact.state,
                            JournalSevenZipObservationState::Deleted
                                | JournalSevenZipObservationState::NeverCreated
                                | JournalSevenZipObservationState::CreateFailed
                                | JournalSevenZipObservationState::Published
                        )
                    })
            })
        {
            return Err("7z discard phase retains an unresolved object".into());
        }
        if self.phase == JournalSevenZipCreatePhase::Discarded && self.pending_action.is_some() {
            return Err("discarded 7z ledger cannot retain a pending action".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipCreatePhase::WorkspaceCleaned
                | JournalSevenZipCreatePhase::PublishIntent
        ) && observation(JournalSevenZipObjectRole::StageArchive).is_none_or(|fact| {
            if self.aborting {
                !matches!(
                    fact.state,
                    JournalSevenZipObservationState::Owned
                        | JournalSevenZipObservationState::DeleteIntent
                        | JournalSevenZipObservationState::Deleted
                )
            } else {
                fact.state != JournalSevenZipObservationState::Owned
            }
        }) {
            return Err("7z verified stage must remain owned through publication".into());
        }
        if matches!(
            self.phase,
            JournalSevenZipCreatePhase::CandidateReady
                | JournalSevenZipCreatePhase::StagePlanned
                | JournalSevenZipCreatePhase::StageOwned
        ) && observation(JournalSevenZipObjectRole::CandidateArchive).is_none_or(|fact| {
            if self.aborting {
                !matches!(
                    fact.state,
                    JournalSevenZipObservationState::Owned
                        | JournalSevenZipObservationState::DeleteIntent
                        | JournalSevenZipObservationState::Deleted
                )
            } else {
                fact.state != JournalSevenZipObservationState::Owned
            }
        }) {
            return Err("7z candidate phase needs the captured candidate identity".into());
        }
        if !self.aborting
            && matches!(
                self.phase,
                JournalSevenZipCreatePhase::StageReady
                    | JournalSevenZipCreatePhase::WorkspaceCleaned
                    | JournalSevenZipCreatePhase::PublishIntent
                    | JournalSevenZipCreatePhase::Published
            )
            && self
                .manifest
                .iter()
                .filter(|entry| entry.role == JournalSevenZipObjectRole::InputFile)
                .any(|entry| {
                    self.observations
                        .iter()
                        .find(|fact| fact.object_id == entry.id)
                        .is_none_or(|fact| fact.length.is_none() || fact.sha256.is_none())
                })
        {
            return Err("7z verified stage is missing captured input digests".into());
        }
        if !self.aborting
            && matches!(
                self.phase,
                JournalSevenZipCreatePhase::StageOwned
                    | JournalSevenZipCreatePhase::StageReady
                    | JournalSevenZipCreatePhase::WorkspaceCleaned
                    | JournalSevenZipCreatePhase::PublishIntent
            )
            && observation(JournalSevenZipObjectRole::StageArchive)
                .is_none_or(|fact| fact.state != JournalSevenZipObservationState::Owned)
        {
            return Err("7z stage phase needs its exact owned identity".into());
        }
        if self.phase == JournalSevenZipCreatePhase::Published
            && observation(JournalSevenZipObjectRole::StageArchive)
                .is_none_or(|fact| fact.state != JournalSevenZipObservationState::Published)
        {
            return Err("published 7z output needs the exact stage publication receipt".into());
        }
        let root_id = self
            .manifest
            .iter()
            .find(|entry| entry.role == JournalSevenZipObjectRole::WorkspaceRoot)
            .map(|entry| entry.id);
        if observation(JournalSevenZipObjectRole::WorkspaceRoot)
            .is_some_and(|root| root.state == JournalSevenZipObservationState::Deleted)
            && self
                .manifest
                .iter()
                .filter(|entry| {
                    entry.role != JournalSevenZipObjectRole::StageArchive
                        && Some(entry.id) != root_id
                })
                .any(|entry| {
                    self.observations
                        .iter()
                        .find(|fact| fact.object_id == entry.id)
                        .is_none_or(|fact| {
                            matches!(
                                fact.state,
                                JournalSevenZipObservationState::Planned
                                    | JournalSevenZipObservationState::CreateIntent
                                    | JournalSevenZipObservationState::Owned
                                    | JournalSevenZipObservationState::DeleteIntent
                            )
                        })
                })
        {
            return Err("7z workspace root was removed before its children".into());
        }
        Ok(())
    }

    pub fn follows(&self, previous: &Self) -> bool {
        if self == previous {
            return true;
        }
        self.sources == previous.sources
            && self.provider_revision == previous.provider_revision
            && self.executable == previous.executable
            && self.executable_identity == previous.executable_identity
            && self.seven_zip_dll == previous.seven_zip_dll
            && self.seven_zip_dll_identity == previous.seven_zip_dll_identity
            && self.output == previous.output
            && self.output_parent_identity == previous.output_parent_identity
            && self.workspace == previous.workspace
            && self.workspace_parent_identity == previous.workspace_parent_identity
            && self.stage == previous.stage
            && self.manifest == previous.manifest
            && self.observations.len() == previous.observations.len()
            && previous
                .output_identity
                .as_ref()
                .is_none_or(|identity| self.output_identity.as_ref() == Some(identity))
            && self.sequence == previous.sequence.checked_add(1).unwrap_or(0)
            && (!previous.aborting || self.aborting)
            && (previous.phase != JournalSevenZipCreatePhase::Published
                || (self.phase == JournalSevenZipCreatePhase::Published && !self.aborting))
            && ((!self.aborting
                && (self.phase == previous.phase
                    || self.phase_rank() == previous.phase_rank() + 1))
                || (self.aborting && self.phase == previous.phase)
                || (self.aborting && self.phase == JournalSevenZipCreatePhase::Discarded)
                || (previous.phase == JournalSevenZipCreatePhase::PublishIntent
                    && self.phase == JournalSevenZipCreatePhase::Published
                    && !self.aborting))
            && previous.observations.iter().all(|old| {
                self.observations
                    .iter()
                    .find(|new| new.object_id == old.object_id)
                    .is_some_and(|new| observation_follows(old, new, self.aborting))
            })
            && changed_observation_ids(self, previous).is_some_and(|changed| {
                changed.len() <= 1
                    && (changed.is_empty()
                        || previous
                            .pending_action
                            .as_ref()
                            .is_some_and(|action| changed == [action.object_id])
                        || self
                            .pending_action
                            .as_ref()
                            .is_some_and(|action| changed == [action.object_id])
                        || (previous.phase == JournalSevenZipCreatePhase::PublishIntent
                            && self.phase == JournalSevenZipCreatePhase::Published
                            && changed.iter().all(|id| {
                                self.manifest.iter().any(|entry| {
                                    entry.id == *id
                                        && entry.role == JournalSevenZipObjectRole::StageArchive
                                }) && self.observations.iter().any(|fact| {
                                    fact.object_id == *id
                                        && fact.state == JournalSevenZipObservationState::Published
                                })
                            }))
                        || (self.aborting
                            && changed.iter().all(|id| {
                                let old = previous
                                    .observations
                                    .iter()
                                    .find(|fact| fact.object_id == *id)
                                    .unwrap();
                                let new = self
                                    .observations
                                    .iter()
                                    .find(|fact| fact.object_id == *id)
                                    .unwrap();
                                old.state == JournalSevenZipObservationState::Planned
                                    && new.state == JournalSevenZipObservationState::NeverCreated
                            }))
                        || changed.iter().all(|id| {
                            let old = previous
                                .observations
                                .iter()
                                .find(|fact| fact.object_id == *id)
                                .unwrap();
                            let new = self
                                .observations
                                .iter()
                                .find(|fact| fact.object_id == *id)
                                .unwrap();
                            old.state == new.state
                                && old.identity == new.identity
                                && ((old.length.is_none() && new.length.is_some())
                                    || (old.sha256.is_none() && new.sha256.is_some()))
                        }))
            })
            && (!self.aborting
                || self.observations.iter().all(|new| {
                    previous
                        .observations
                        .iter()
                        .any(|old| old.object_id == new.object_id)
                }))
            && previous.pending_action.as_ref().is_none_or(|action| {
                (self.pending_action.as_ref() == Some(action)
                    && self.aborting
                    && !previous.aborting)
                    || (self.pending_action.as_ref() != Some(action)
                        && self
                            .observations
                            .iter()
                            .find(|observation| observation.object_id == action.object_id)
                            .is_some_and(|observation| match action.action {
                                JournalSevenZipObservationState::CreateIntent => matches!(
                                    observation.state,
                                    JournalSevenZipObservationState::Owned
                                        | JournalSevenZipObservationState::CreateFailed
                                ),
                                JournalSevenZipObservationState::DeleteIntent => {
                                    observation.state == JournalSevenZipObservationState::Deleted
                                        && action.expected_identity.as_ref().is_some_and(
                                            |identity| {
                                                observation.identity.as_ref() == Some(identity)
                                            },
                                        )
                                }
                                _ => false,
                            }))
            })
            && self.pending_action.as_ref().is_none_or(|action| {
                previous.pending_action.as_ref().is_none()
                    && (action.action != JournalSevenZipObservationState::CreateIntent
                        || !self.aborting)
                    || previous.pending_action.as_ref() == Some(action)
            })
    }

    fn phase_rank(&self) -> u8 {
        match self.phase {
            JournalSevenZipCreatePhase::WorkspacePlanned => 0,
            JournalSevenZipCreatePhase::WorkspaceOwned => 1,
            JournalSevenZipCreatePhase::CandidateCreateIntent => 2,
            JournalSevenZipCreatePhase::CandidateReady => 3,
            JournalSevenZipCreatePhase::StagePlanned => 4,
            JournalSevenZipCreatePhase::StageOwned => 5,
            JournalSevenZipCreatePhase::StageReady => 6,
            JournalSevenZipCreatePhase::WorkspaceCleaned => 7,
            JournalSevenZipCreatePhase::PublishIntent => 8,
            JournalSevenZipCreatePhase::Published => 9,
            JournalSevenZipCreatePhase::Discarded => 10,
        }
    }
}

fn observation_follows(
    old: &JournalSevenZipObservation,
    new: &JournalSevenZipObservation,
    aborting: bool,
) -> bool {
    let allowed = matches!(
        (old.state, new.state),
        (
            JournalSevenZipObservationState::Planned,
            JournalSevenZipObservationState::CreateIntent
        ) | (
            JournalSevenZipObservationState::Planned,
            JournalSevenZipObservationState::NeverCreated
        ) | (
            JournalSevenZipObservationState::CreateIntent,
            JournalSevenZipObservationState::Owned
        ) | (
            JournalSevenZipObservationState::CreateIntent,
            JournalSevenZipObservationState::CreateFailed
        ) | (
            JournalSevenZipObservationState::Owned,
            JournalSevenZipObservationState::DeleteIntent
        ) | (
            JournalSevenZipObservationState::DeleteIntent,
            JournalSevenZipObservationState::Deleted
        ) | (
            JournalSevenZipObservationState::Owned,
            JournalSevenZipObservationState::Published
        )
    );
    old.identity
        .as_ref()
        .is_none_or(|identity| new.identity.as_ref() == Some(identity))
        && old.length.is_none_or(|length| new.length == Some(length))
        && old
            .sha256
            .as_ref()
            .is_none_or(|digest| new.sha256.as_ref() == Some(digest))
        && (old.state == new.state
            || (allowed
                && (new.state != JournalSevenZipObservationState::NeverCreated || aborting)))
}

fn changed_observation_ids(
    current: &JournalSevenZipCreate,
    previous: &JournalSevenZipCreate,
) -> Option<Vec<u32>> {
    if current.observations.len() != previous.observations.len() {
        return None;
    }
    Some(
        previous
            .observations
            .iter()
            .filter_map(|old| {
                current
                    .observations
                    .iter()
                    .find(|new| new.object_id == old.object_id)
                    .filter(|new| *new != old)
                    .map(|_| old.object_id)
            })
            .collect(),
    )
}

fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('\\')
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(super::super::model::seven_zip_windows_component)
}

fn valid_absolute_windows_path(path: &super::JournalPath) -> bool {
    let units = &path.units;
    if units.is_empty() || units.len() > 32768 || units.contains(&0) {
        return false;
    }
    let drive_root = units.len() >= 3
        && matches!(units[0], 65..=90 | 97..=122)
        && units[1] == b':' as u16
        && (units[2] == b'\\' as u16 || units[2] == b'/' as u16);
    let unc_root = units.len() >= 3
        && (units[0] == b'\\' as u16 || units[0] == b'/' as u16)
        && units[1] == units[0]
        && units[2] != units[0];
    drive_root || unc_root
}

fn relative_parent(path: &str) -> Option<&str> {
    path.rfind(|character| character == '\\' || character == '/')
        .map(|index| &path[..index])
}

fn windows_ordinal_equal(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::CompareStringOrdinal;
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        unsafe { CompareStringOrdinal(&left, &right, true) }.0 == 2
    }
    #[cfg(not(windows))]
    {
        left.eq_ignore_ascii_case(right)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn fixture() -> JournalSevenZipCreate {
        let paths = [
            (
                "workspace",
                JournalSevenZipObjectKind::Directory,
                JournalSevenZipObjectRole::WorkspaceRoot,
            ),
            (
                "workspace/input",
                JournalSevenZipObjectKind::Directory,
                JournalSevenZipObjectRole::InputContainer,
            ),
            (
                "workspace/inputs.txt",
                JournalSevenZipObjectKind::File,
                JournalSevenZipObjectRole::Listfile,
            ),
            (
                "workspace/candidate.7z",
                JournalSevenZipObjectKind::File,
                JournalSevenZipObjectRole::CandidateArchive,
            ),
            (
                "stage.7z",
                JournalSevenZipObjectKind::File,
                JournalSevenZipObjectRole::StageArchive,
            ),
            (
                "workspace/input/root.txt",
                JournalSevenZipObjectKind::File,
                JournalSevenZipObjectRole::InputFile,
            ),
        ];
        let manifest = paths
            .iter()
            .enumerate()
            .map(|(index, (path, kind, role))| JournalSevenZipManifestEntry {
                id: index as u32,
                relative_path: (*path).into(),
                kind: *kind,
                role: *role,
                source_index: (index == 5).then_some(0),
                source_relative_path: (index == 5).then(|| String::new()),
            })
            .collect::<Vec<_>>();
        JournalSevenZipCreate {
            sources: vec![JournalSevenZipCreateSource {
                source: JournalPath {
                    units: vec![
                        67, 58, 92, 115, 114, 99, 92, 114, 111, 111, 116, 46, 116, 120, 116,
                    ],
                },
                source_identity: "source-id".into(),
                source_kind: JournalSevenZipSourceKind::File,
                entry_name: "root.txt".into(),
            }],
            provider_revision: 3,
            executable: JournalPath {
                units: vec![67, 58, 92, 55, 122, 92, 55, 122, 46, 101, 120, 101],
            },
            executable_identity: "exe-id".into(),
            seven_zip_dll: JournalPath {
                units: vec![67, 58, 92, 55, 122, 92, 55, 122, 46, 100, 108, 108],
            },
            seven_zip_dll_identity: "dll-id".into(),
            output: JournalPath {
                units: vec![67, 58, 92, 115, 114, 99, 92, 111, 117, 116, 46, 55, 122],
            },
            output_parent_identity: "parent-id".into(),
            workspace: JournalPath {
                units: vec![
                    67, 58, 92, 116, 101, 109, 112, 92, 119, 111, 114, 107, 115, 112, 97, 99, 101,
                ],
            },
            workspace_parent_identity: "workspace-parent".into(),
            stage: JournalPath {
                units: vec![
                    67, 58, 92, 115, 114, 99, 92, 115, 116, 97, 103, 101, 46, 55, 122,
                ],
            },
            observations: manifest
                .iter()
                .map(|entry| JournalSevenZipObservation {
                    object_id: entry.id,
                    state: JournalSevenZipObservationState::Planned,
                    identity: None,
                    length: None,
                    sha256: None,
                })
                .collect(),
            manifest,
            phase: JournalSevenZipCreatePhase::WorkspacePlanned,
            aborting: false,
            sequence: 1,
            pending_action: None,
            output_identity: None,
        }
    }

    fn trace_step(
        previous: &JournalSevenZipCreate,
        phase: JournalSevenZipCreatePhase,
        object_id: u32,
        state: JournalSevenZipObservationState,
        aborting: bool,
    ) -> JournalSevenZipCreate {
        let mut next = previous.clone();
        next.sequence += 1;
        next.phase = phase;
        next.aborting = aborting;
        let observation = next
            .observations
            .iter_mut()
            .find(|fact| fact.object_id == object_id)
            .unwrap();
        observation.state = state;
        match state {
            JournalSevenZipObservationState::CreateIntent => {
                next.pending_action = Some(JournalSevenZipPendingAction {
                    object_id,
                    action: state,
                    expected_identity: None,
                });
            }
            JournalSevenZipObservationState::Owned => {
                observation.identity = Some(format!("object-{object_id}"));
                next.pending_action = None;
            }
            JournalSevenZipObservationState::CreateFailed => next.pending_action = None,
            JournalSevenZipObservationState::DeleteIntent => {
                next.pending_action = Some(JournalSevenZipPendingAction {
                    object_id,
                    action: state,
                    expected_identity: observation.identity.clone(),
                });
            }
            JournalSevenZipObservationState::Deleted => next.pending_action = None,
            JournalSevenZipObservationState::NeverCreated => next.pending_action = None,
            _ => {}
        }
        assert!(
            next.validate().is_ok(),
            "invalid trace step phase={:?} id={object_id} state={state:?}: {:?}",
            next.phase,
            next.validate()
        );
        assert!(
            next.follows(previous),
            "invalid transition from {:?} to {:?}",
            previous.phase,
            next.phase
        );
        next
    }

    fn phase_step(
        previous: &JournalSevenZipCreate,
        phase: JournalSevenZipCreatePhase,
        aborting: bool,
    ) -> JournalSevenZipCreate {
        let mut next = previous.clone();
        next.sequence += 1;
        next.phase = phase;
        next.aborting = aborting;
        assert!(
            next.validate().is_ok(),
            "phase {:?}: {:?}",
            next.phase,
            next.validate()
        );
        assert!(
            next.follows(previous),
            "phase transition {:?} -> {:?}",
            previous.phase,
            next.phase
        );
        next
    }

    #[test]
    fn seven_zip_manifest_checks_container_parent_graph_and_ordinal_collisions() {
        let mut ledger = fixture();
        assert!(ledger.validate().is_ok());
        ledger.manifest[5].relative_path = "workspace/root.txt".into();
        assert!(ledger.validate().is_err());

        let mut ledger = fixture();
        ledger.manifest.push(JournalSevenZipManifestEntry {
            id: 6,
            relative_path: "workspace/input/ROOT.TXT".into(),
            kind: JournalSevenZipObjectKind::File,
            role: JournalSevenZipObjectRole::InputFile,
            source_index: Some(0),
            source_relative_path: Some("other.txt".into()),
        });
        ledger.observations.push(JournalSevenZipObservation {
            object_id: 6,
            state: JournalSevenZipObservationState::Planned,
            identity: None,
            length: None,
            sha256: None,
        });
        assert!(ledger.validate().is_err());
    }

    #[test]
    fn create_intent_crash_gap_is_retained_and_never_inferred_absent() {
        let planned = fixture();
        let mut intent = planned.clone();
        intent.sequence = 2;
        intent.observations[0].state = JournalSevenZipObservationState::CreateIntent;
        intent.pending_action = Some(JournalSevenZipPendingAction {
            object_id: 0,
            action: JournalSevenZipObservationState::CreateIntent,
            expected_identity: None,
        });
        assert!(intent.validate().is_ok());
        assert!(intent.follows(&planned));

        let mut inferred_absence = intent.clone();
        inferred_absence.sequence = 3;
        inferred_absence.aborting = true;
        inferred_absence.pending_action = None;
        inferred_absence.observations[0].state = JournalSevenZipObservationState::NeverCreated;
        assert!(!inferred_absence.follows(&intent));
    }

    #[test]
    fn manifest_rejects_missing_source_root_and_excessive_depth() {
        let mut missing_root = fixture();
        missing_root.manifest[5].source_relative_path = Some("nested.txt".into());
        assert!(missing_root.validate().is_err());

        let mut deep = fixture();
        deep.manifest[5].relative_path = format!("workspace/input/{}root.txt", "a/".repeat(128));
        assert!(deep.validate().is_err());

        let mut corrupt_source_index = fixture();
        corrupt_source_index.manifest[5].source_index = Some(99);
        assert!(corrupt_source_index.validate().is_err());

        let mut directory_content = fixture();
        directory_content.manifest[5].role = JournalSevenZipObjectRole::InputDirectory;
        directory_content.manifest[5].kind = JournalSevenZipObjectKind::Directory;
        directory_content.sources[0].source_kind = JournalSevenZipSourceKind::Folder;
        directory_content.observations[5].length = Some(0);
        assert!(directory_content.validate().is_err());
    }

    #[test]
    fn directory_only_empty_source_has_a_valid_materialized_root() {
        let mut directory = fixture();
        directory.sources[0].source_kind = JournalSevenZipSourceKind::Folder;
        directory.sources[0].entry_name = "empty-folder".into();
        directory.manifest[5].relative_path = "workspace/input/empty-folder".into();
        directory.manifest[5].kind = JournalSevenZipObjectKind::Directory;
        directory.manifest[5].role = JournalSevenZipObjectRole::InputDirectory;
        assert!(directory.validate().is_ok(), "{:?}", directory.validate());
    }

    fn complete_trace(
        mut current: super::JournalSevenZipCreate,
    ) -> Vec<super::JournalSevenZipCreate> {
        use JournalSevenZipCreatePhase as Phase;
        use JournalSevenZipObservationState as State;
        let mut snapshots = vec![current.clone()];
        current = trace_step(
            &current,
            Phase::WorkspacePlanned,
            0,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspacePlanned, 0, State::Owned, false);
        current = phase_step(&current, Phase::WorkspaceOwned, false);
        current = trace_step(
            &current,
            Phase::WorkspaceOwned,
            1,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspaceOwned, 1, State::Owned, false);
        current = trace_step(
            &current,
            Phase::WorkspaceOwned,
            2,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspaceOwned, 2, State::Owned, false);
        current = trace_step(
            &current,
            Phase::WorkspaceOwned,
            5,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspaceOwned, 5, State::Owned, false);
        snapshots.push(current.clone());
        if current.manifest[5].role == JournalSevenZipObjectRole::InputFile {
            let mut sealed = current.clone();
            sealed.sequence += 1;
            sealed.observations[5].length = Some(17);
            sealed.observations[5].sha256 = Some("a".repeat(64));
            assert!(sealed.validate().is_ok(), "seal: {:?}", sealed.validate());
            assert!(sealed.follows(&current));
            current = sealed;
        }
        current = trace_step(
            &current,
            Phase::CandidateCreateIntent,
            3,
            State::CreateIntent,
            false,
        );
        snapshots.push(current.clone());
        current = trace_step(&current, Phase::CandidateReady, 3, State::Owned, false);
        snapshots.push(current.clone());
        current = phase_step(&current, Phase::StagePlanned, false);
        snapshots.push(current.clone());
        current = trace_step(&current, Phase::StagePlanned, 4, State::CreateIntent, false);
        current = trace_step(&current, Phase::StageOwned, 4, State::Owned, false);
        snapshots.push(current.clone());
        current = phase_step(&current, Phase::StageReady, false);
        snapshots.push(current.clone());
        for id in [3, 5, 2, 1, 0] {
            current = trace_step(&current, Phase::StageReady, id, State::DeleteIntent, false);
            current = trace_step(&current, Phase::StageReady, id, State::Deleted, false);
        }
        current = phase_step(&current, Phase::WorkspaceCleaned, false);
        snapshots.push(current.clone());
        let mut publish_intent = current.clone();
        publish_intent.sequence += 1;
        publish_intent.phase = Phase::PublishIntent;
        assert!(publish_intent.validate().is_ok());
        assert!(publish_intent.follows(&current));
        snapshots.push(publish_intent.clone());
        let mut published = publish_intent.clone();
        published.sequence += 1;
        published.phase = Phase::Published;
        published.output_identity = Some("published-output-id".into());
        published.observations[4].state = State::Published;
        assert!(published.validate().is_ok());
        assert!(published.follows(&publish_intent));
        let mut abort_after_publish = published.clone();
        abort_after_publish.sequence += 1;
        abort_after_publish.aborting = true;
        abort_after_publish.phase = Phase::Discarded;
        assert!(!abort_after_publish.follows(&published));
        snapshots.push(published);
        snapshots
    }

    #[test]
    fn complete_nonempty_file_trace_reaches_published_with_single_action_barriers() {
        let trace = complete_trace(fixture());
        let published = trace.last().unwrap();
        assert_eq!(published.phase, JournalSevenZipCreatePhase::Published);
    }

    #[test]
    fn complete_directory_only_empty_tree_trace_reaches_published() {
        let mut directory = fixture();
        directory.sources[0].source_kind = JournalSevenZipSourceKind::Folder;
        directory.sources[0].entry_name = "empty-folder".into();
        directory.manifest[5].relative_path = "workspace/input/empty-folder".into();
        directory.manifest[5].kind = JournalSevenZipObjectKind::Directory;
        directory.manifest[5].role = JournalSevenZipObjectRole::InputDirectory;
        let trace = complete_trace(directory);
        let published = trace.last().unwrap();
        assert_eq!(published.phase, JournalSevenZipCreatePhase::Published);
    }

    #[test]
    fn abort_trace_can_account_unattempted_children_and_remove_root_last() {
        use JournalSevenZipCreatePhase as Phase;
        use JournalSevenZipObservationState as State;
        let mut current = fixture();
        current = trace_step(
            &current,
            Phase::WorkspacePlanned,
            0,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspacePlanned, 0, State::Owned, false);
        current = phase_step(&current, Phase::WorkspaceOwned, false);
        current = phase_step(&current, Phase::WorkspaceOwned, true);
        for id in [1, 2, 3, 4, 5] {
            current = trace_step(
                &current,
                Phase::WorkspaceOwned,
                id,
                State::NeverCreated,
                true,
            );
        }
        current = trace_step(
            &current,
            Phase::WorkspaceOwned,
            0,
            State::DeleteIntent,
            true,
        );
        current = trace_step(&current, Phase::WorkspaceOwned, 0, State::Deleted, true);
        let discarded = trace_step(&current, Phase::Discarded, 0, State::Deleted, true);
        assert!(discarded.validate().is_ok());
    }

    #[test]
    fn abort_latched_candidate_intent_must_be_resolved_before_cleanup() {
        use JournalSevenZipCreatePhase as Phase;
        use JournalSevenZipObservationState as State;
        let mut current = fixture();
        current = trace_step(
            &current,
            Phase::WorkspacePlanned,
            0,
            State::CreateIntent,
            false,
        );
        current = trace_step(&current, Phase::WorkspacePlanned, 0, State::Owned, false);
        current = phase_step(&current, Phase::WorkspaceOwned, false);
        current = trace_step(
            &current,
            Phase::CandidateCreateIntent,
            3,
            State::CreateIntent,
            false,
        );
        let aborting = phase_step(&current, Phase::CandidateCreateIntent, true);
        assert!(aborting.pending_action.is_some());
        for receipt_state in [State::Owned, State::CreateFailed] {
            let mut resolved = trace_step(
                &aborting,
                Phase::CandidateCreateIntent,
                3,
                receipt_state,
                true,
            );
            if receipt_state == State::Owned {
                resolved = trace_step(
                    &resolved,
                    Phase::CandidateCreateIntent,
                    3,
                    State::DeleteIntent,
                    true,
                );
                resolved = trace_step(
                    &resolved,
                    Phase::CandidateCreateIntent,
                    3,
                    State::Deleted,
                    true,
                );
            }
            for id in [1, 2, 4, 5] {
                resolved = trace_step(
                    &resolved,
                    Phase::CandidateCreateIntent,
                    id,
                    State::NeverCreated,
                    true,
                );
            }
            resolved = trace_step(
                &resolved,
                Phase::CandidateCreateIntent,
                0,
                State::DeleteIntent,
                true,
            );
            resolved = trace_step(
                &resolved,
                Phase::CandidateCreateIntent,
                0,
                State::Deleted,
                true,
            );
            let discarded = phase_step(&resolved, Phase::Discarded, true);
            assert!(discarded.validate().is_ok(), "{:?}", discarded.validate());
        }
    }

    #[test]
    fn abort_cleanup_is_reachable_from_each_prepublication_barrier() {
        use JournalSevenZipCreatePhase as Phase;
        use JournalSevenZipObservationState as State;
        let trace = complete_trace(fixture());
        for snapshot in trace
            .iter()
            .take_while(|step| step.phase != Phase::Published)
        {
            let mut current = if snapshot.aborting {
                snapshot.clone()
            } else {
                phase_step(snapshot, snapshot.phase, true)
            };
            if let Some(action) = current.pending_action.clone() {
                let receipt = match action.action {
                    State::CreateIntent => {
                        if action.object_id == 3 {
                            State::CreateFailed
                        } else {
                            State::Owned
                        }
                    }
                    State::DeleteIntent => State::Deleted,
                    _ => unreachable!(),
                };
                current = trace_step(&current, current.phase, action.object_id, receipt, true);
                if receipt == State::Owned {
                    current = trace_step(
                        &current,
                        current.phase,
                        action.object_id,
                        State::DeleteIntent,
                        true,
                    );
                    current = trace_step(
                        &current,
                        current.phase,
                        action.object_id,
                        State::Deleted,
                        true,
                    );
                }
            }
            let non_root_ids = current
                .manifest
                .iter()
                .filter(|entry| entry.role != JournalSevenZipObjectRole::WorkspaceRoot)
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            for id in non_root_ids {
                let state = current
                    .observations
                    .iter()
                    .find(|fact| fact.object_id == id)
                    .unwrap()
                    .state;
                match state {
                    State::Planned => {
                        current = trace_step(&current, current.phase, id, State::NeverCreated, true)
                    }
                    State::Owned => {
                        current =
                            trace_step(&current, current.phase, id, State::DeleteIntent, true);
                        current = trace_step(&current, current.phase, id, State::Deleted, true);
                    }
                    State::DeleteIntent => {
                        current = trace_step(&current, current.phase, id, State::Deleted, true)
                    }
                    State::NeverCreated | State::CreateFailed | State::Deleted => {}
                    State::CreateIntent | State::Published => {
                        panic!("barrier retained unsupported state {state:?}")
                    }
                }
            }
            let root = current
                .observations
                .iter()
                .find(|fact| fact.object_id == 0)
                .unwrap()
                .state;
            match root {
                State::Planned => {
                    current = trace_step(&current, current.phase, 0, State::NeverCreated, true)
                }
                State::Owned => {
                    current = trace_step(&current, current.phase, 0, State::DeleteIntent, true);
                    current = trace_step(&current, current.phase, 0, State::Deleted, true);
                }
                State::DeleteIntent => {
                    current = trace_step(&current, current.phase, 0, State::Deleted, true)
                }
                State::Deleted | State::NeverCreated => {}
                State::CreateIntent | State::CreateFailed | State::Published => {
                    panic!("root retained unsupported state {root:?}")
                }
            }
            let discarded = phase_step(&current, Phase::Discarded, true);
            assert!(
                discarded.validate().is_ok(),
                "phase {:?}: {:?}",
                snapshot.phase,
                discarded.validate()
            );
        }
    }
}
