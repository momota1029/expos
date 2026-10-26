//! File information pane's transient, bounded SHA-256 work.

use crate::{directory::EntrySourceIdentity, entry_identity::EntryKey};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

const READ_BUFFER_SIZE: usize = 256 * 1024;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Unavailable,
    Loading,
    Ready(String),
    Failed,
}

#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) ticket: u64,
    pub(crate) target: Target,
    pub(crate) cancelled: Arc<AtomicBool>,
}

pub(crate) struct Completion {
    pub(crate) guard: Option<File>,
    pub(crate) result: Result<String, ()>,
}

/// Shell-owned coordinator. A cancelled worker retains the sole running slot until it exits;
/// repeated target changes only replace the single pending target.
#[derive(Default)]
pub(crate) struct Session {
    ticket: u64,
    current: Option<Target>,
    state: State,
    running: Option<(u64, Arc<AtomicBool>)>,
    pending: Option<(u64, Target)>,
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

    pub(crate) fn finish(&mut self, ticket: u64, result: Result<String, ()>) -> Option<Job> {
        if self
            .running
            .as_ref()
            .is_some_and(|(running, _)| *running == ticket)
        {
            self.running = None;
        }
        if ticket == self.ticket {
            self.state = match result {
                Ok(digest) => State::Ready(digest),
                Err(()) => State::Failed,
            };
        }
        let pending = self.pending.take();
        pending.map(|(ticket, target)| self.start(ticket, target))
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

pub(crate) fn hash(job: &Job) -> Completion {
    let mut file = match open_read_target(&job.target) {
        Ok(file) => file,
        Err(()) => {
            return Completion {
                guard: None,
                result: Err(()),
            };
        }
    };
    match hash_opened(&mut file, job) {
        Ok((digest, guard)) => Completion {
            guard: Some(guard),
            result: Ok(digest),
        },
        Err(()) => Completion {
            guard: None,
            result: Err(()),
        },
    }
}

fn hash_opened(file: &mut File, job: &Job) -> Result<(String, File), ()> {
    let before = file.metadata().map_err(|_| ())?;
    let before_change_time = change_time(file).ok_or(())?;
    if !valid_metadata(&before, &job.target) || !job.target.source_identity.matches_file(file) {
        return Err(());
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; READ_BUFFER_SIZE];
    loop {
        if job.cancelled.load(Ordering::Acquire) {
            return Err(());
        }
        let read = file.read(&mut buffer).map_err(|_| ())?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if job.cancelled.load(Ordering::Acquire) {
        return Err(());
    }
    let after = file.metadata().map_err(|_| ())?;
    let after_change_time = change_time(file).ok_or(())?;
    if !valid_metadata(&after, &job.target)
        || !job.target.source_identity.matches_file(file)
        || signature(&before) != signature(&after)
        || before_change_time != after_change_time
    {
        return Err(());
    }
    if job.cancelled.load(Ordering::Acquire) {
        return Err(());
    }
    let guard = open_commit_guard(&job.target)?;
    let guard_metadata = guard.metadata().map_err(|_| ())?;
    let guard_change_time = change_time(&guard).ok_or(())?;
    if job.cancelled.load(Ordering::Acquire)
        || !valid_metadata(&guard_metadata, &job.target)
        || !job.target.source_identity.matches_file(&guard)
        || !job.target.source_identity.matches_file(file)
        || signature(&after) != signature(&guard_metadata)
        || after_change_time != guard_change_time
    {
        return Err(());
    }
    Ok((format!("{:x}", digest.finalize()), guard))
}

fn valid_metadata(metadata: &std::fs::Metadata, target: &Target) -> bool {
    metadata.is_file()
        && metadata.len() == target.size
        && metadata.modified().ok() == Some(target.modified)
        && !metadata_is_reparse_or_directory(metadata)
}

fn signature(metadata: &std::fs::Metadata) -> (u64, Option<SystemTime>) {
    (metadata.len(), metadata.modified().ok())
}

#[cfg(windows)]
fn change_time(file: &File) -> Option<i64> {
    use ::windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx},
    };
    use std::{mem::size_of, os::windows::io::AsRawHandle};

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
fn metadata_is_reparse_or_directory(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    let attributes = metadata.file_attributes();
    attributes & 0x10 != 0 || attributes & 0x400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_or_directory(metadata: &std::fs::Metadata) -> bool {
    !metadata.is_file()
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
