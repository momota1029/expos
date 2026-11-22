use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, Global};
use install_layout::{VersionDir, lock_install_root, read_current};

use crate::install_info;
use crate::shell::{Message, Translator};

use super::settings::Settings;
use super::source::{self, DirSource, GitHubSource, ReleaseSource};
use super::{Channel, Offer, apply};

const RUNNING: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
struct Installation {
    root: PathBuf,
    data_root: PathBuf,
    fixture: Option<PathBuf>,
    auto_check: bool,
    restart_settings: Option<Settings>,
}

impl Installation {
    fn from_env() -> Option<Self> {
        let root = install_info::install_root()?
            .is_absolute()
            .then(|| install_layout::install_root().ok())??;
        let launched = install_info::version_dir()
            .is_some_and(|dir| install_info::is_running_version_dir(&root, &dir, RUNNING));
        let fixture = match std::env::var_os("EXPOS_UPDATE_SOURCE") {
            Some(value) => {
                let path = value.to_str()?.strip_prefix("dir:")?;
                if path.is_empty() {
                    return None;
                }
                Some(std::path::absolute(path).ok()?)
            }
            None if launched => None,
            None => return None,
        };
        Some(Self {
            root,
            data_root: crate::app_paths::data_root()?,
            fixture,
            restart_settings: launched.then(restart_settings).flatten(),
            auto_check: launched
                && std::env::var_os("EXPOS_NO_UPDATE_CHECK").as_deref() != Some("1".as_ref()),
        })
    }

    fn lock(&self) -> io::Result<install_layout::InstallLock> {
        let guard = lock_install_root(&self.root)?;
        let install = self.root.canonicalize()?;
        // A missing data root can still sit under a junction in an existing ancestor.
        let mut data = std::path::absolute(&self.data_root)?;
        let mut tail = Vec::new();
        let data = loop {
            match data.canonicalize() {
                Ok(base) => break base.join(tail.iter().rev().collect::<PathBuf>()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let Some(name) = data.file_name() else {
                        return Err(error);
                    };
                    tail.push(name.to_owned());
                    data.pop();
                }
                Err(error) => return Err(error),
            }
        };
        if !data_path_is_separate(&install, &data) {
            return Err(io::Error::other(
                "the app data directory overlaps the updater's install paths",
            ));
        }
        Ok(guard)
    }

    fn source(&self) -> Box<dyn ReleaseSource> {
        match &self.fixture {
            Some(path) => Box::new(DirSource(path.clone())),
            None => Box::new(GitHubSource),
        }
    }
}

fn data_path_is_separate(install: &Path, data: &Path) -> bool {
    let install = PathBuf::from(install.to_string_lossy().to_lowercase());
    let data = PathBuf::from(data.to_string_lossy().to_lowercase());
    let Ok(relative) = data.strip_prefix(&install) else {
        return true;
    };
    let Some(first) = relative.iter().next().and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    !matches!(first, ".staging" | ".lock" | "current" | "expos.exe")
        && VersionDir::parse(first).is_none()
}

#[derive(Clone)]
struct Pending {
    offer: Offer,
    current: Option<VersionDir>,
}

enum Status {
    Loading,
    Idle,
    Checking,
    Saving,
    Offered(Pending),
    ConfirmDowngrade(Pending),
    Applying,
    Ready(VersionDir),
    UpToDate,
    Failed(String),
}

struct Updater {
    installation: Installation,
    settings: Settings,
    status: Status,
    generation: u64,
    switch_notice: bool,
    restarting: bool,
}

impl Global for Updater {}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum NoticeAction {
    Apply,
    ConfirmDowngrade,
    Restart,
    Dismiss,
}

pub(crate) struct Notice {
    pub text: String,
    pub action: Option<(NoticeAction, Message)>,
    pub dismissible: bool,
    pub separate_data: bool,
    pub generation: u64,
}

pub(crate) fn start(cx: &mut App) {
    let Some(installation) = Installation::from_env() else {
        return;
    };
    cx.set_global(Updater {
        installation: installation.clone(),
        settings: Settings::default(),
        status: Status::Loading,
        generation: 0,
        switch_notice: false,
        restarting: false,
    });
    cx.spawn(async move |cx| {
        let auto = installation.auto_check;
        let load = cx
            .background_executor()
            .spawn(async move {
                let guard = installation.lock()?;
                let mut settings = Settings::load(&installation.data_root)?;
                apply::cleanup_staging(&installation.root, &guard);
                if let Some(handoff) = installation.restart_settings {
                    let updated = Settings {
                        last_check_unix: settings.last_check_unix.max(handoff.last_check_unix),
                        ..handoff
                    };
                    if settings != updated {
                        updated.save(&installation.data_root)?;
                        settings = updated;
                    }
                }
                Ok::<_, io::Error>(settings)
            })
            .await;
        cx.update(|cx| {
            let updater = cx.global_mut::<Updater>();
            match load {
                Ok(settings) => {
                    updater.settings = settings;
                    updater.status = Status::Idle;
                }
                Err(error) => updater.status = Status::Failed(error.to_string()),
            }
            cx.refresh_windows();
        });
        if auto {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            cx.update(|cx| check(Check::Automatic, cx));
        }
    })
    .detach();
}

pub(crate) fn available(cx: &App) -> bool {
    cx.has_global::<Updater>()
}

pub(crate) fn menu_state(cx: &App) -> Option<(Channel, bool, bool)> {
    let updater = cx.try_global::<Updater>()?;
    Some((
        updater.settings.channel,
        updater.settings.automatic,
        can_check(&updater.status),
    ))
}

pub(crate) fn running_label(translator: &Translator) -> String {
    translator
        .text(Message::UpdateCurrent)
        .replace("{version}", RUNNING)
        .replace("{channel}", &channel_label(Channel::current(), translator))
}

pub(crate) fn channel_label(channel: Channel, translator: &Translator) -> String {
    translator
        .text(match channel {
            Channel::Stable => Message::UpdateStable,
            Channel::Beta => Message::UpdateBeta,
        })
        .into_owned()
}

fn can_check(status: &Status) -> bool {
    matches!(
        status,
        Status::Idle | Status::Offered(_) | Status::UpToDate | Status::Failed(_)
    )
}

#[derive(Clone, Copy)]
pub(crate) enum Check {
    Automatic,
    Manual,
    Choose(Channel),
}

pub(crate) fn check(request: Check, cx: &mut App) {
    let Some(updater) = cx.try_global::<Updater>() else {
        return;
    };
    if !can_check(&updater.status) {
        return;
    }
    let automatic = matches!(request, Check::Automatic);
    if automatic
        && (!matches!(updater.status, Status::Idle) || !updater.settings.auto_due(Settings::now()))
    {
        return;
    }
    let channel = match request {
        Check::Choose(channel) if channel != updater.settings.channel => Some(channel),
        Check::Choose(_) => return,
        _ => None,
    };
    let switching = channel.is_some_and(|channel| channel != Channel::current());
    let updater = cx.global_mut::<Updater>();
    updater.switch_notice = switching;
    updater.status = Status::Checking;
    updater.generation += 1;
    let installation = updater.installation.clone();
    cx.refresh_windows();

    cx.spawn(async move |cx| {
        let (settings, result) = cx
            .background_executor()
            .spawn(async move {
                let mut saved = None;
                let result = (|| {
                    let guard = installation.lock()?;
                    let mut settings = Settings::load(&installation.data_root)?;
                    if automatic && !settings.auto_due(Settings::now()) {
                        saved = Some(settings);
                        return Ok(None);
                    }
                    if let Some(channel) = channel {
                        settings.channel = channel;
                    }
                    settings.last_check_unix = Some(Settings::now());
                    settings.save(&installation.data_root)?;
                    saved = Some(settings);
                    let current = read_current(&installation.root)?;
                    drop(guard);
                    let offer = source::check(
                        installation.source().as_ref(),
                        RUNNING,
                        settings.channel,
                        switching,
                    )?;
                    Ok::<_, io::Error>(offer.map(|offer| Pending { offer, current }))
                })();
                (saved, result)
            })
            .await;
        cx.update(|cx| {
            let updater = cx.global_mut::<Updater>();
            if let Some(settings) = settings {
                updater.settings = settings;
            }
            updater.status = match result {
                Ok(Some(pending)) => {
                    let channel = Channel::of_version(&pending.offer.version.version().to_string());
                    updater.switch_notice |= channel != Channel::current();
                    Status::Offered(pending)
                }
                Ok(None) if !automatic => Status::UpToDate,
                Ok(None) => Status::Idle,
                Err(error) => Status::Failed(error.to_string()),
            };
            cx.refresh_windows();
        });
    })
    .detach();
}

pub(crate) fn toggle_automatic(cx: &mut App) {
    let Some(updater) = cx.try_global::<Updater>() else {
        return;
    };
    if !can_check(&updater.status) {
        return;
    }
    let updater = cx.global_mut::<Updater>();
    let automatic = !updater.settings.automatic;
    updater.generation += 1;
    updater.switch_notice = false;
    let previous = std::mem::replace(&mut updater.status, Status::Saving);
    let installation = updater.installation.clone();
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let _guard = installation.lock()?;
                let mut settings = Settings::load(&installation.data_root)?;
                settings.automatic = automatic;
                settings.save(&installation.data_root)?;
                Ok::<_, io::Error>(settings)
            })
            .await;
        cx.update(|cx| {
            let updater = cx.global_mut::<Updater>();
            match result {
                Ok(settings) => {
                    updater.settings = settings;
                    updater.status = previous;
                }
                Err(error) => updater.status = Status::Failed(error.to_string()),
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

pub(crate) fn notice_rows(cx: &App) -> usize {
    cx.try_global::<Updater>().map_or(0, |updater| {
        if matches!(updater.status, Status::Loading | Status::Idle) {
            0
        } else {
            1 + usize::from(updater.switch_notice)
        }
    })
}

pub(crate) fn notice(translator: &Translator, cx: &App) -> Option<Notice> {
    let updater = cx.try_global::<Updater>()?;
    let (message, detail, action, dismissible) = match &updater.status {
        Status::Loading | Status::Idle => return None,
        Status::Checking => (Message::UpdateChecking, String::new(), None, false),
        Status::Saving => (Message::UpdateSaving, String::new(), None, false),
        Status::Offered(pending) => (
            Message::UpdateAvailable,
            pending.offer.version.version().to_string(),
            Some((NoticeAction::Apply, Message::UpdateRestart)),
            true,
        ),
        Status::ConfirmDowngrade(pending) => (
            Message::UpdateDowngrade,
            pending.offer.version.version().to_string(),
            Some((NoticeAction::ConfirmDowngrade, Message::UpdateContinue)),
            true,
        ),
        Status::Applying => (Message::UpdateInstalling, String::new(), None, false),
        Status::Ready(version) => (
            Message::UpdateInstalled,
            version.version().to_string(),
            (!updater.restarting).then_some((NoticeAction::Restart, Message::UpdateRestart)),
            false,
        ),
        Status::UpToDate => (Message::UpdateUpToDate, String::new(), None, true),
        Status::Failed(error) => (Message::UpdateFailed, error.clone(), None, true),
    };
    let text = translator
        .text(message)
        .replace("{version}", &detail)
        .replace("{error}", &detail);
    Some(Notice {
        text,
        action,
        dismissible,
        separate_data: updater.switch_notice,
        generation: updater.generation,
    })
}

pub(crate) fn act(action: NoticeAction, generation: u64, cx: &mut App) -> Option<Restart> {
    let updater = cx.try_global::<Updater>()?;
    if updater.generation != generation || updater.restarting {
        return None;
    }
    let updater = cx.global_mut::<Updater>();
    if action == NoticeAction::Restart
        && let Status::Ready(version) = &updater.status
    {
        return Some(Restart {
            root: updater.installation.root.clone(),
            version: version.clone(),
            settings: updater.settings,
        });
    }
    let pending = match (&updater.status, action) {
        (Status::Offered(pending), NoticeAction::Apply) if pending.offer.is_downgrade => {
            updater.status = Status::ConfirmDowngrade(pending.clone());
            updater.generation += 1;
            cx.refresh_windows();
            return None;
        }
        (Status::Offered(pending), NoticeAction::Apply)
        | (Status::ConfirmDowngrade(pending), NoticeAction::ConfirmDowngrade) => pending.clone(),
        (
            Status::Offered(_) | Status::ConfirmDowngrade(_) | Status::UpToDate | Status::Failed(_),
            NoticeAction::Dismiss,
        ) => {
            updater.status = Status::Idle;
            updater.switch_notice = false;
            updater.generation += 1;
            cx.refresh_windows();
            return None;
        }
        _ => return None,
    };
    updater.switch_notice = false;
    updater.status = Status::Applying;
    updater.generation += 1;
    let installation = updater.installation.clone();
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                apply::apply(
                    &installation.root,
                    installation.source().as_ref(),
                    &pending.offer,
                    pending.current.as_ref(),
                )
                .map(|()| pending.offer.version)
            })
            .await;
        cx.update(|cx| {
            cx.global_mut::<Updater>().status = match result {
                Ok(version) => Status::Ready(version),
                Err(error) => Status::Failed(error.to_string()),
            };
            cx.refresh_windows();
        });
    })
    .detach();
    None
}

pub(crate) fn restarting(cx: &App) -> bool {
    cx.try_global::<Updater>()
        .is_some_and(|updater| updater.restarting)
}

pub(crate) fn mark_restarting(value: bool, cx: &mut App) {
    cx.global_mut::<Updater>().restarting = value;
    cx.refresh_windows();
}

const RESTART_CHANNEL_ARG: &str = "--expos-update-channel=";

fn restart_settings() -> Option<Settings> {
    let arg = std::env::args_os().nth(1)?;
    let fields = arg.to_str()?.strip_prefix(RESTART_CHANNEL_ARG)?;
    let mut fields = fields.split(':');
    if fields.next()? != RUNNING {
        return None;
    }
    let channel = match fields.next()? {
        "stable" => Channel::Stable,
        "beta" => Channel::Beta,
        _ => return None,
    };
    let automatic = match fields.next()? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    let last_check_unix = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(Settings {
        channel,
        last_check_unix: (last_check_unix != 0).then_some(last_check_unix),
        automatic,
        ..Settings::default()
    })
}

pub(crate) struct Restart {
    root: PathBuf,
    version: VersionDir,
    settings: Settings,
}

impl Restart {
    pub(crate) fn spawn(&self) -> io::Result<()> {
        if read_current(&self.root)?.as_ref() != Some(&self.version) {
            return Err(io::Error::other("the active installation changed"));
        }
        let launcher = self.root.join("expos.exe");
        if !super::ordinary_file(&launcher)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the installed launcher",
            ));
        }
        std::process::Command::new(launcher)
            .arg(format!(
                "{RESTART_CHANNEL_ARG}{}:{}:{}:{}",
                self.version.version(),
                self.settings.channel.as_str(),
                u8::from(self.settings.automatic),
                self.settings.last_check_unix.unwrap_or_default()
            ))
            .env(install_layout::INSTALL_ROOT_ENV, &self.root)
            .env_remove(install_layout::VERSION_DIR_ENV)
            .env_remove("EXPOS_LAUNCHER_WAIT")
            .spawn()?;
        Ok(())
    }
}
