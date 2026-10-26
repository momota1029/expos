//! タブ構成と window 状態の保存・復元。生きているタブと disk をつなぐ。
//!
//! `session_store` は形式（DTO / 検証 / 順序付き writer / Windows の置換）だけを知る。
//! ここは「何をいつ書くか」を持ち、GPUI の view から呼ばれる唯一の層。
//! 生きているタブの owner は `FilerShell` のまま。snapshot は保存用の detached value で、
//! 復元のあとも第二の application model として残さない。
//!
//! 保存する rect は logical pixels。display の `visible_bounds` も logical なので、
//! そのまま比べられる。

use super::session_store::windows::{SessionFile, SessionPaths};
use super::session_store::{
    self, AsyncOrderedWriter, BoundsValue, RestoreOutcome, SAVE_DEBOUNCE, SAVE_DEBOUNCE_CAP,
    SessionSnapshot, WindowSnapshot, should_write,
};
use gpui::{App, Bounds, DisplayId, Pixels, Window, WindowBounds, point, px, size};
use std::time::Instant;

/// 自動保存が失敗したときに、自動で試す回数。
///
/// 通らないまま試し続けると、待ち時間 0 の loop で CPU を回し続けてしまう。
/// ここを超えたら次の変更まで書かない。
const MAX_WRITE_ATTEMPTS: u32 = 3;
const WRITE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// 保存先を決められなかった。`LOCALAPPDATA` が無い環境。
const NO_LOCATION: &str = "保存先を決められません";

/// 復元を受け取らない最小の window 寸法。
///
/// v5 の `load_window_bounds` と同じ値。小さすぎる保存値は位置ごと捨てて既定で開く。
const MIN_WINDOW_WIDTH: f32 = 320.0;
const MIN_WINDOW_HEIGHT: f32 = 240.0;

/// 保存が無いときに開く大きさ。
const DEFAULT_WINDOW_WIDTH: f32 = 840.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 620.0;

/// 起動時に `FilerShell` が使う restore data と folder preference state。
#[derive(Default, Clone)]
pub(super) struct Startup {
    pub(super) window_id: u64,
    /// 復元に使う内容。無ければ既定で始める。
    pub(super) restore: Option<Box<SessionSnapshot>>,
    /// 保存できない理由。ユーザーに見せる。
    pub(super) notice: Option<String>,
}

/// Application が一つだけ所有する session writer authority。
#[derive(Default)]
pub(super) struct SessionAutosaveOwner {
    autosave: Option<Autosave>,
    writes_disabled: bool,
    writes_paused: bool,
    next_barrier_id: u64,
    durable_barrier: Option<DurableSessionBarrier>,
    next_close_id: u64,
    close_ticket: Option<DurableSessionClose>,
    window_id_high_water: u64,
    workspace_id_high_water: u64,
    tab_id_high_water: u64,
    next_tab_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SessionBarrierId(u64);

struct DurableSessionBarrier {
    id: SessionBarrierId,
    receipt: async_channel::Receiver<Result<u64, String>>,
    result: Option<Result<u64, String>>,
}

struct DurableSessionClose {
    id: u64,
    snapshot: SessionSnapshot,
    receipt: async_channel::Receiver<Result<u64, String>>,
    result: Option<Result<u64, String>>,
}

impl gpui::Global for SessionAutosaveOwner {}

impl SessionAutosaveOwner {
    pub(super) fn disabled() -> Self {
        Self::default()
    }

    fn from_autosave(autosave: Option<Autosave>, restore: Option<&SessionSnapshot>) -> Self {
        let mut owner = Self {
            autosave,
            ..Self::default()
        };
        if let Some(snapshot) = restore {
            owner.observe_marks(snapshot);
            owner.next_tab_id = snapshot.tab_id_high_water.saturating_add(1);
        }
        owner
    }

    pub(super) fn observe_marks(&mut self, snapshot: &SessionSnapshot) {
        self.window_id_high_water = self.window_id_high_water.max(snapshot.window_id_high_water);
        self.workspace_id_high_water = self
            .workspace_id_high_water
            .max(snapshot.workspace_id_high_water);
        self.tab_id_high_water = self.tab_id_high_water.max(snapshot.tab_id_high_water);
        self.next_tab_id = self
            .next_tab_id
            .max(snapshot.tab_id_high_water.saturating_add(1));
    }

    pub(super) fn marks(&self) -> (u64, u64, u64) {
        (
            self.window_id_high_water.max(1),
            self.workspace_id_high_water.max(1),
            self.tab_id_high_water.max(1),
        )
    }

    pub(super) fn reserve_window_id(&mut self) -> Option<u64> {
        let id = self.window_id_high_water.max(1).checked_add(1)?;
        self.window_id_high_water = id;
        Some(id)
    }

    pub(super) fn reserve_workspace_id(&mut self) -> Option<u64> {
        let id = self.workspace_id_high_water.max(1).checked_add(1)?;
        self.workspace_id_high_water = id;
        Some(id)
    }

    pub(super) fn observe_tab_id(&mut self, id: u64) {
        let next = id.checked_add(1).unwrap_or(u64::MAX);
        self.tab_id_high_water = self.tab_id_high_water.max(id);
        self.next_tab_id = self.next_tab_id.max(next);
    }

    pub(super) fn reserve_tab_ids(&mut self, count: usize) -> Option<std::ops::Range<usize>> {
        let count = u64::try_from(count).ok()?;
        let end = self.next_tab_id.checked_add(count)?;
        let start = self.next_tab_id;
        let start_key = usize::try_from(start).ok()?;
        let end_key = usize::try_from(end).ok()?;
        if count > 0 {
            self.tab_id_high_water = end.checked_sub(1)?;
            self.next_tab_id = end;
        }
        Some(start_key..end_key)
    }

    pub(super) fn has_writer(&self) -> bool {
        self.autosave.is_some()
    }

    pub(super) fn disable_writes(&mut self) {
        if self.durable_barrier.is_some() {
            return;
        }
        self.autosave = None;
        self.writes_disabled = true;
        self.writes_paused = false;
    }

    pub(super) fn set_writes_paused(&mut self, paused: bool) {
        if paused && self.close_ticket.is_some() {
            return;
        }
        if !self.writes_disabled && (paused || self.durable_barrier.is_none()) {
            self.writes_paused = paused;
        }
    }

    #[allow(dead_code)] // cross-window transfer の transaction から durable receipt を使う。
    pub(super) fn begin_durable_barrier(
        &mut self,
        snapshot: SessionSnapshot,
    ) -> Result<SessionBarrierId, String> {
        if self.writes_disabled
            || !self.writes_paused
            || self.durable_barrier.is_some()
            || self.close_ticket.is_some()
        {
            return Err("session durable barrier requires paused session writes".into());
        }
        let autosave = self
            .autosave
            .as_mut()
            .ok_or_else(|| "session snapshot writer is unavailable".to_owned())?;
        let id = SessionBarrierId(
            self.next_barrier_id
                .checked_add(1)
                .ok_or_else(|| "session barrier ID exhausted".to_owned())?,
        );
        let receipt = autosave.durable_barrier(snapshot)?;
        self.next_barrier_id = id.0;
        self.durable_barrier = Some(DurableSessionBarrier {
            id,
            receipt,
            result: None,
        });
        Ok(id)
    }

    pub(super) fn begin_close(&mut self, snapshot: SessionSnapshot) -> Result<u64, String> {
        if self.durable_barrier.is_some() || self.close_ticket.is_some() || self.writes_paused {
            return Err("session persistence is already committed to another lifecycle".into());
        }
        let autosave = self
            .autosave
            .as_mut()
            .ok_or_else(|| "session snapshot writer is unavailable".to_owned())?;
        let id = self
            .next_close_id
            .checked_add(1)
            .ok_or_else(|| "session close ticket ID exhausted".to_owned())?;
        let receipt = autosave.durable_barrier(snapshot.clone())?;
        autosave.discard_pending();
        self.writes_paused = true;
        self.next_close_id = id;
        self.close_ticket = Some(DurableSessionClose {
            id,
            snapshot,
            receipt,
            result: None,
        });
        Ok(id)
    }

    pub(super) fn poll_close(&mut self, id: u64) -> Option<Result<u64, String>> {
        let ticket = self.close_ticket.as_mut()?;
        if ticket.id != id {
            return None;
        }
        if let Some(result) = &ticket.result {
            return Some(result.clone());
        }
        let result = match ticket.receipt.try_recv() {
            Ok(result) => Some(result),
            Err(async_channel::TryRecvError::Empty) => None,
            Err(async_channel::TryRecvError::Closed) => Some(Err(
                "session snapshot writer stopped before durable receipt".into(),
            )),
        };
        if let Some(result) = result {
            ticket.result = Some(result.clone());
            Some(result)
        } else {
            None
        }
    }

    pub(super) fn replace_close(
        &mut self,
        id: u64,
        snapshot: SessionSnapshot,
    ) -> Result<(), String> {
        let ticket = self
            .close_ticket
            .as_ref()
            .ok_or_else(|| "session close ticket is no longer active".to_owned())?;
        if ticket.id != id || ticket.result.is_none() {
            return Err("session close ticket is pending or stale".into());
        }
        let receipt = self
            .autosave
            .as_mut()
            .ok_or_else(|| "session snapshot writer is unavailable".to_owned())?
            .durable_barrier(snapshot.clone())?;
        let ticket = self.close_ticket.as_mut().expect("ticket checked");
        ticket.snapshot = snapshot;
        ticket.receipt = receipt;
        ticket.result = None;
        Ok(())
    }

    pub(super) fn close_snapshot(&self, id: u64) -> Option<&SessionSnapshot> {
        self.close_ticket
            .as_ref()
            .filter(|ticket| ticket.id == id)
            .map(|ticket| &ticket.snapshot)
    }

    pub(super) fn finish_close(
        &mut self,
        id: u64,
        retry_snapshot: Option<SessionSnapshot>,
    ) -> bool {
        if !self
            .close_ticket
            .as_ref()
            .is_some_and(|ticket| ticket.id == id)
        {
            return false;
        }
        self.close_ticket = None;
        self.writes_paused = false;
        if let Some(snapshot) = retry_snapshot {
            if let Some(autosave) = self.autosave.as_mut() {
                autosave.request(snapshot, Instant::now());
            }
        }
        true
    }

    pub(super) fn close_pending(&self) -> bool {
        self.close_ticket.is_some()
    }

    #[allow(dead_code)] // transfer transaction polls without taking ownership of the receipt.
    pub(super) fn poll_durable_barrier(
        &mut self,
        id: SessionBarrierId,
    ) -> Option<Result<u64, String>> {
        let barrier = self.durable_barrier.as_mut()?;
        if barrier.id != id {
            return None;
        }
        if let Some(result) = &barrier.result {
            return Some(result.clone());
        }
        match barrier.receipt.try_recv() {
            Ok(result) => {
                barrier.result = Some(result.clone());
                Some(result)
            }
            Err(async_channel::TryRecvError::Empty) => None,
            Err(async_channel::TryRecvError::Closed) => {
                let result = Err("session snapshot writer stopped before durable receipt".into());
                barrier.result = Some(result.clone());
                Some(result)
            }
        }
    }

    #[allow(dead_code)] // rollback and retry keep the same paused transaction ticket.
    pub(super) fn replace_durable_barrier(
        &mut self,
        id: SessionBarrierId,
        snapshot: SessionSnapshot,
    ) -> Result<(), String> {
        let Some(barrier) = self.durable_barrier.as_ref() else {
            return Err("session durable barrier is no longer active".into());
        };
        if barrier.id != id || barrier.result.is_none() {
            return Err("session durable barrier is still pending or stale".into());
        }
        let receipt = self
            .autosave
            .as_mut()
            .ok_or_else(|| "session snapshot writer is unavailable".to_owned())?
            .durable_barrier(snapshot)?;
        let barrier = self.durable_barrier.as_mut().expect("barrier was checked");
        barrier.receipt = receipt;
        barrier.result = None;
        Ok(())
    }

    #[allow(dead_code)] // called only after the transfer commit or rollback is complete.
    pub(super) fn finish_durable_barrier(
        &mut self,
        id: SessionBarrierId,
        live_snapshot: SessionSnapshot,
        now: Instant,
    ) -> bool {
        let completed = self
            .durable_barrier
            .as_ref()
            .is_some_and(|barrier| barrier.id == id && matches!(barrier.result, Some(Ok(_))));
        if !completed || self.writes_disabled {
            return false;
        }
        self.durable_barrier = None;
        let Some(autosave) = self.autosave.as_mut() else {
            return false;
        };
        self.writes_paused = false;
        autosave.request(live_snapshot, now);
        true
    }

    pub(super) fn request(&mut self, snapshot: SessionSnapshot, now: Instant) -> bool {
        if self.writes_disabled || self.writes_paused || self.close_ticket.is_some() {
            return false;
        }
        let Some(autosave) = self.autosave.as_mut() else {
            return false;
        };
        autosave.request(snapshot, now);
        true
    }

    pub(super) fn next_check(&self) -> Option<Instant> {
        if self.writes_disabled || self.writes_paused {
            return None;
        }
        self.autosave.as_ref()?.next_check()
    }

    pub(super) fn flush_if_due(
        &mut self,
        now: Instant,
    ) -> Option<(Option<String>, Option<Instant>)> {
        let autosave = self.autosave.as_mut()?;
        if self.writes_paused {
            if let Some(barrier) = self.durable_barrier.as_mut()
                && barrier.result.is_none()
            {
                match barrier.receipt.try_recv() {
                    Ok(result) => barrier.result = Some(result),
                    Err(async_channel::TryRecvError::Closed) => {
                        barrier.result = Some(Err(
                            "session snapshot writer stopped before durable receipt".into(),
                        ));
                    }
                    Err(async_channel::TryRecvError::Empty) => {}
                }
            }
            return Some((autosave.failure().map(str::to_owned), None));
        }
        autosave.flush_if_due(now);
        Some((autosave.failure().map(str::to_owned), autosave.next_check()))
    }

    pub(super) fn reopen(&mut self) -> Option<String> {
        if self.writes_disabled {
            return Some("session writes remain disabled because restore was incomplete".into());
        }
        if self.autosave.is_some() {
            return None;
        }
        let opened = open_session();
        self.autosave = opened.autosave;
        opened.notice
    }
}

/// Application が一つだけ所有する folder preference の snapshot と writer 状態。
///
/// Shell はこれを通じて読み書きする。Shell ごとに snapshot や revision を複製すると、
/// 同じ path の別 tab 更新や writer completion が競合して新しい値を失う。
pub(super) struct FolderPreferencesOwner {
    pub(super) snapshot: crate::folder_view_preferences::Snapshot,
    pub(super) writer: Option<crate::folder_view_preferences::Writer>,
    pub(super) write_revision: u64,
    pub(super) write_in_flight: Option<u64>,
    pub(super) written_revision: u64,
    pub(super) write_failures: u32,
    pub(super) retry_scheduled: bool,
    pub(super) writer_watching: bool,
    pub(super) close_revision: Option<u64>,
    pub(super) close_result: Option<Result<(), String>>,
}

impl gpui::Global for FolderPreferencesOwner {}

impl FolderPreferencesOwner {
    pub(super) fn disabled() -> Self {
        Self::new(Default::default(), None)
    }

    fn new(
        snapshot: crate::folder_view_preferences::Snapshot,
        writer: Option<crate::folder_view_preferences::Writer>,
    ) -> Self {
        let revision = snapshot.revision;
        Self {
            snapshot,
            writer,
            write_revision: revision,
            write_in_flight: None,
            written_revision: revision,
            write_failures: 0,
            retry_scheduled: false,
            writer_watching: false,
            close_revision: None,
            close_result: None,
        }
    }

    pub(super) fn get(
        &self,
        path: &std::path::Path,
    ) -> Option<crate::folder_view_preferences::FolderViewPrefs> {
        self.snapshot.get(path)
    }

    pub(super) fn insert(
        &mut self,
        path: &std::path::Path,
        prefs: crate::folder_view_preferences::FolderViewPrefs,
    ) -> bool {
        self.snapshot.insert(path, prefs)
    }

    pub(super) fn begin_close_write(&mut self) -> Result<Option<u64>, String> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(None);
        };
        if self.write_revision <= self.written_revision && self.write_in_flight.is_none() {
            return Ok(None);
        }
        let revision = self.write_revision;
        let bytes = self.snapshot.encode().map_err(|error| error.to_string())?;
        writer.request(revision, bytes)?;
        self.write_in_flight = Some(revision);
        self.close_revision = Some(revision);
        self.close_result = None;
        Ok(Some(revision))
    }

    pub(super) fn close_write_result(&self, revision: u64) -> Option<Result<(), String>> {
        if self.close_revision != Some(revision) {
            return None;
        }
        self.close_result
            .clone()
            .or_else(|| (self.written_revision >= revision).then_some(Ok(())))
    }
}

#[cfg(test)]
pub(super) fn folder_preferences_owner_for_test(
    path: std::path::PathBuf,
) -> (
    FolderPreferencesOwner,
    crate::folder_view_preferences::StoreFile,
) {
    let file = crate::folder_view_preferences::StoreFile::for_test(path);
    let snapshot = file.load().expect("test folder preference store loads");
    let owner = FolderPreferencesOwner::new(
        snapshot,
        Some(crate::folder_view_preferences::Writer::start(file.clone())),
    );
    (owner, file)
}

struct SessionOpen {
    restore: Option<Box<SessionSnapshot>>,
    notice: Option<String>,
    autosave: Option<Autosave>,
}

impl Startup {
    /// 保存を読み書きしない起動。`--demo` のように列挙しない一覧のとき。
    ///
    /// 保存を読むと `--demo` が無視される。書くと開発用の一覧で本物の保存を潰す。
    pub(super) fn disabled() -> Self {
        Self::default()
    }
}

/// 起動時の restore data と application session writer を作る。
pub(super) fn open() -> (Startup, SessionAutosaveOwner, FolderPreferencesOwner) {
    let (folder_preferences, folder_writer, folder_notice) = open_folder_preferences();
    let session = open_session();
    let notice = join_notice(session.notice, folder_notice);
    let owner = SessionAutosaveOwner::from_autosave(session.autosave, session.restore.as_deref());
    (
        Startup {
            window_id: 1,
            restore: session.restore,
            notice: (!notice.is_empty()).then_some(notice),
        },
        owner,
        FolderPreferencesOwner::new(folder_preferences, folder_writer),
    )
}

fn open_session() -> SessionOpen {
    let Some(paths) = SessionPaths::default_for_app() else {
        return SessionOpen {
            notice: Some(NO_LOCATION.into()),
            restore: None,
            autosave: None,
        };
    };
    let file = SessionFile::new(paths);
    let primary = match file.read_primary() {
        Ok(bytes) => bytes,
        // 読めなかっただけで内容は残っているかもしれない。消さずに知らせる。
        Err(reason) => {
            return SessionOpen {
                notice: Some(reason),
                restore: None,
                autosave: None,
            };
        }
    };
    // backup が読めなくても、primary だけで判断できる。
    let backup = file.read_backup().unwrap_or(None);
    match session_store::resolve_restore(primary.as_deref(), backup.as_deref()) {
        RestoreOutcome::Absent => SessionOpen {
            restore: None,
            notice: None,
            autosave: Some(Autosave::new(file)),
        },
        RestoreOutcome::Restored(snapshot) => SessionOpen {
            restore: Some(snapshot),
            autosave: Some(Autosave::new(file)),
            notice: None,
        },
        RestoreOutcome::Unreadable(reason) => SessionOpen {
            notice: Some(reason),
            restore: None,
            autosave: None,
        },
        RestoreOutcome::FutureSchema { found } => SessionOpen {
            notice: Some(format!(
                "保存が新しい形式（schema {found}）なので上書きしません"
            )),
            autosave: None,
            restore: None,
        },
    }
}

fn join_notice(primary: Option<String>, secondary: Option<String>) -> String {
    match (primary, secondary) {
        (Some(a), Some(b)) => format!("{a}; {b}"),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => String::new(),
    }
}

fn open_folder_preferences() -> (
    crate::folder_view_preferences::Snapshot,
    Option<crate::folder_view_preferences::Writer>,
    Option<String>,
) {
    use crate::folder_view_preferences::{LoadError, StoreFile, Writer};
    let Some(file) = StoreFile::default_for_app() else {
        return (
            Default::default(),
            None,
            Some("folder preferences: LOCALAPPDATA is unavailable".into()),
        );
    };
    match file.load() {
        Ok(snapshot) => (snapshot, Some(Writer::start(file)), None),
        Err(LoadError::Unreadable(reason)) => (
            Default::default(),
            None,
            Some(format!("folder preferences were preserved: {reason}")),
        ),
        Err(LoadError::FutureSchema(version)) => (
            Default::default(),
            None,
            Some(format!(
                "folder preference schema {version} is newer; file was preserved"
            )),
        ),
    }
}

/// 起動時に開く window の位置と大きさ。
///
/// `display` は置く display。渡さないと platform が主 display を選び、保存した rect が
/// 別の display の上だと「画面外」と見なされて中央へ戻される。
///
/// 保存値を受け取らなかった場合は既定の中央位置へ落とす。
pub(super) struct InitialWindow {
    pub(super) bounds: WindowBounds,
    pub(super) display: Option<DisplayId>,
}

impl InitialWindow {
    /// 保存された window 状態から起動値を作る。
    ///
    /// 保存が無い、値が有限でない、display が一つも無い場合は [`InitialWindow::fallback`]。
    pub(super) fn restore_window(wanted: Option<&WindowSnapshot>, cx: &App) -> Self {
        let Some(wanted) = wanted else {
            return Self::fallback(cx);
        };
        let Some(rect) = wanted.normal_bounds.and_then(rect_of) else {
            return Self::fallback(cx);
        };
        let displays = cx.displays();
        let areas: Vec<_> = displays
            .iter()
            .map(|display| display.visible_bounds())
            .collect();
        // 重なりの一番大きい display へ置く。どこにも重ならなければ主 display。
        let chosen = choose_work_area(rect, &areas)
            .map(|index| (displays[index].id(), areas[index]))
            .or_else(|| {
                cx.primary_display()
                    .map(|display| (display.id(), display.visible_bounds()))
            });
        let Some((display, area)) = chosen else {
            // display を一つも取れない。置き場所を推測せず既定へ落とす。
            return Self::fallback(cx);
        };
        let fitted = fit_into(rect, area);
        let bounds = if wanted.maximized {
            // `Maximized` の rect は「最大化する前の位置」。platform が同じ意味で使う。
            WindowBounds::Maximized(fitted)
        } else {
            WindowBounds::Windowed(fitted)
        };
        Self {
            bounds,
            display: Some(display),
        }
    }

    /// 保存が無いときの起動値。中央に既定の大きさで開く。
    fn fallback(cx: &App) -> Self {
        Self {
            bounds: WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                cx,
            )),
            display: None,
        }
    }
}

/// 今の window 状態を保存用の値にする。
///
/// `WindowBounds::Maximized` の rect は最大化する前の位置なので、そのまま normal bounds
/// に使える。v6 は fullscreen を要求しないが、もし来ても restore bounds を通常位置として扱う。
pub(super) fn window_snapshot(window: &Window, sidebar_width: f32) -> WindowSnapshot {
    let bounds = window.window_bounds();
    let rect = bounds.get_bounds();
    WindowSnapshot {
        normal_bounds: Some(BoundsValue {
            x: rect.origin.x.as_f32(),
            y: rect.origin.y.as_f32(),
            width: rect.size.width.as_f32(),
            height: rect.size.height.as_f32(),
        }),
        maximized: matches!(bounds, WindowBounds::Maximized(_)),
        // 取得時の DPI。復元では今の display の work area に合わせるだけにして、
        // 拡大率そのものは platform に任せる。
        dpi: Some(window.scale_factor()),
        sidebar_width,
        closed_tabs: Vec::new(),
    }
}

/// 保存された rect を GPUI の値にする。
///
/// 有限でない値と、小さすぎる寸法は受け取らない。受け取らない場合は既定で開く。
fn rect_of(value: BoundsValue) -> Option<Bounds<Pixels>> {
    let finite = value.x.is_finite()
        && value.y.is_finite()
        && value.width.is_finite()
        && value.height.is_finite();
    if !finite || value.width < MIN_WINDOW_WIDTH || value.height < MIN_WINDOW_HEIGHT {
        return None;
    }
    Some(Bounds::new(
        point(px(value.x), px(value.y)),
        size(px(value.width), px(value.height)),
    ))
}

/// rect を置く work area を選ぶ。
///
/// 中心が入っている area を最優先する。platform 側の `check_given_bounds` も
/// 「中心が display の上にあるか」で見るので、ここを合わせておくと保存した rect が
/// 置き直されない。中心がどこにも無ければ、重なりの面積が一番大きい area。
/// どれとも重ならない（画面外）ときは `None`。
fn choose_work_area(rect: Bounds<Pixels>, areas: &[Bounds<Pixels>]) -> Option<usize> {
    if let Some(index) = areas.iter().position(|area| area.contains(&rect.center())) {
        return Some(index);
    }
    areas
        .iter()
        .enumerate()
        .filter_map(|(index, area)| {
            let overlap = rect.intersect(area);
            if overlap.is_empty() {
                return None;
            }
            Some((
                index,
                overlap.size.width.as_f32() * overlap.size.height.as_f32(),
            ))
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
}

/// 保存された rect を work area の中へ戻す。
///
/// 中心が area の中なら、そのまま使う。platform 側も「中心が display の上にあるか」で
/// 置き直すので、判定を合わせておくと保存した位置が勝手に動かない。
/// 外れているときは大きさを area に収めて中央へ置き直す。
fn fit_into(rect: Bounds<Pixels>, area: Bounds<Pixels>) -> Bounds<Pixels> {
    if area.contains(&rect.center()) {
        return rect;
    }
    Bounds::centered_at(
        area.center(),
        size(
            rect.size.width.min(area.size.width),
            rect.size.height.min(area.size.height),
        ),
    )
}

/// 自動保存。変更が静まるまで待ってから書く。
///
/// 生きているタブを持たない。呼び出し側が写した snapshot を渡す。
pub(super) struct Autosave {
    writer: AsyncOrderedWriter,
    /// 次に載せる revision。単調に増やす。
    revision: u64,
    /// 未保存要求が更新されるたび増える。古い write 完了で新しい snapshot を消さない。
    pending_generation: u64,
    /// 直列 worker に渡した write と durable receipt。
    in_flight: Option<(u64, async_channel::Receiver<Result<u64, String>>)>,
    /// 完了確認のための次回 poll。
    poll_at: Option<Instant>,
    /// 未保存の内容。新しい要求で置き換える。
    pending: Option<SessionSnapshot>,
    /// 最後の変更時刻。静かさの判定に使う。
    quiet_since: Option<Instant>,
    /// 最初の未保存変更の時刻。待ち続けないための上限に使う。
    pending_since: Option<Instant>,
    /// 自動で試した回数。書き込みが通らないときに回り続けないため。
    attempts: u32,
    /// 直近の失敗。知らせるために残す。
    failure: Option<String>,
}

impl Autosave {
    fn new(file: SessionFile) -> Self {
        Self {
            writer: AsyncOrderedWriter::new(file),
            revision: 0,
            pending_generation: 0,
            in_flight: None,
            poll_at: None,
            pending: None,
            quiet_since: None,
            pending_since: None,
            attempts: 0,
            failure: None,
        }
    }

    /// 保存を要求する。まだ書かない。
    pub(super) fn request(&mut self, snapshot: SessionSnapshot, now: Instant) {
        self.pending = Some(snapshot);
        self.pending_generation = self.pending_generation.wrapping_add(1);
        self.quiet_since = Some(now);
        // 最初の未保存変更から数える。要求のたびに延ばすと、変更が続く間は書けない。
        self.pending_since.get_or_insert(now);
        self.attempts = 0;
    }

    fn discard_pending(&mut self) {
        self.pending = None;
        self.quiet_since = None;
        self.pending_since = None;
    }

    /// 次に確認すべき時刻。未保存が無ければ `None`。
    ///
    /// 静かさの期限と、変更が続くときの上限の早い方。
    pub(super) fn next_check(&self) -> Option<Instant> {
        if self.in_flight.is_some() {
            return self.poll_at;
        }
        let quiet = self.quiet_since?;
        let pending = self.pending_since?;
        Some((quiet + SAVE_DEBOUNCE).min(pending + SAVE_DEBOUNCE_CAP))
    }

    /// 書く条件が揃っていれば書く。
    pub(super) fn flush_if_due(&mut self, now: Instant) {
        self.poll_completion(now);
        if self.in_flight.is_some() {
            return;
        }
        let (Some(quiet_since), Some(pending_since)) = (self.quiet_since, self.pending_since)
        else {
            return;
        };
        let quiet = now.saturating_duration_since(quiet_since);
        let pending = now.saturating_duration_since(pending_since);
        if !should_write(quiet, pending) {
            return;
        }
        if let Some(snapshot) = self.pending.clone() {
            self.write(snapshot, now);
        }
    }

    /// debounce を待たずに書き始める。receipt は次回の autosave poll で確認する。
    #[cfg(test)]
    pub(super) fn flush_now(&mut self, snapshot: SessionSnapshot) {
        self.pending = Some(snapshot);
        self.pending_generation = self.pending_generation.wrapping_add(1);
        self.write(self.pending.clone().expect("snapshot set"), Instant::now());
    }

    /// Transfer 用の順序付き durable barrier。呼び出し側は先に autosave を pause する。
    /// enqueue 後は receipt を捨てても write 自体は止まらないため、成功後に commit するか、
    /// rollback snapshot を durable に書くまで pause を解除してはならない。
    #[allow(dead_code)] // transfer slice で Application owner から呼び出す。
    pub(super) fn durable_barrier(
        &mut self,
        mut snapshot: SessionSnapshot,
    ) -> Result<async_channel::Receiver<Result<u64, String>>, String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "session write revision exhausted".to_owned())?;
        snapshot.write_revision = self.revision;
        let bytes = snapshot.to_json().map_err(|error| error.to_string())?;
        Ok(self.writer.write(self.revision, bytes))
    }

    /// 直近の失敗。
    pub(super) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// 条件が揃った snapshot を直列 worker に渡す。
    fn write(&mut self, mut snapshot: SessionSnapshot, now: Instant) {
        let Some(revision) = self.revision.checked_add(1) else {
            self.apply_result(
                Err("session write revision exhausted".into()),
                self.pending_generation,
                now,
            );
            return;
        };
        self.revision = revision;
        snapshot.write_revision = self.revision;
        match snapshot.to_json() {
            Ok(bytes) => {
                self.in_flight = Some((
                    self.pending_generation,
                    self.writer.write(self.revision, bytes),
                ));
                self.poll_at = Some(now + WRITE_POLL_INTERVAL);
            }
            Err(error) => self.apply_result(Err(error.to_string()), self.pending_generation, now),
        }
    }

    fn poll_completion(&mut self, now: Instant) {
        let Some((generation, receipt)) = self.in_flight.as_ref() else {
            return;
        };
        let generation = *generation;
        match receipt.try_recv() {
            Ok(Ok(_revision)) => {
                self.in_flight = None;
                self.poll_at = None;
                self.apply_result(Ok(()), generation, now);
            }
            Ok(Err(reason)) => {
                self.in_flight = None;
                self.poll_at = None;
                self.apply_result(Err(reason), generation, now);
            }
            Err(async_channel::TryRecvError::Empty) => {
                self.poll_at = Some(now + WRITE_POLL_INTERVAL);
            }
            Err(async_channel::TryRecvError::Closed) => {
                self.in_flight = None;
                self.poll_at = None;
                self.apply_result(
                    Err("session snapshot writer stopped before durable receipt".into()),
                    generation,
                    now,
                );
            }
        }
    }

    fn apply_result(&mut self, result: Result<(), String>, generation: u64, now: Instant) {
        match result {
            Ok(()) => {
                if generation == self.pending_generation {
                    self.pending = None;
                    self.quiet_since = None;
                    self.pending_since = None;
                }
                self.attempts = 0;
                self.failure = None;
            }
            Err(reason) => {
                self.failure = Some(reason);
                if generation == self.pending_generation {
                    self.attempts += 1;
                    // 失敗をそのままにすると期限が過去のままになるため、今から数え直す。
                    self.quiet_since = Some(now);
                    self.pending_since = Some(now);
                    if self.attempts > MAX_WRITE_ATTEMPTS {
                        self.quiet_since = None;
                        self.pending_since = None;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{
        LayoutValue, LocationValue, PathValue, SCHEMA_VERSION, SortValue, TabSnapshot,
    };
    use std::path::Path;
    use std::time::Duration;

    fn wait_for_write(autosave: &mut Autosave, now: &mut Instant) {
        for _ in 0..1000 {
            autosave.flush_if_due(*now);
            if autosave.in_flight.is_none() {
                return;
            }
            *now += WRITE_POLL_INTERVAL;
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("async session write did not produce a durable receipt");
    }

    fn snapshot(name: &str) -> SessionSnapshot {
        let window = session_store::WindowSessionSnapshot {
            id: 1,
            workspaces: vec![session_store::WorkspaceSnapshot {
                id: 1,
                name: "Workspace 1".into(),
                panes: vec![session_store::PaneSnapshot {
                    key: 1,
                    active: Some(1),
                    tabs: vec![TabSnapshot {
                        key: 1,
                        location: LocationValue::Path {
                            path: PathValue::from_path(Path::new(name)).unwrap(),
                        },
                        layout: LayoutValue::Rows,
                        icon_scale: 1.0,
                        sort: SortValue::None,
                        group: session_store::GroupValue::None,
                        locked: false,
                    }],
                }],
                pane_layout: session_store::PaneLayoutSnapshot::Pane { key: 1 },
                focused_pane: 1,
            }],
            closed_workspaces: Vec::new(),
            active_workspace: 1,
            window: None,
        };
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: 0,
            windows: vec![window],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: 1,
            tab_id_high_water: 1,
            select_extension_on_rename: false,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: "default".into(),
            language_override: "auto".into(),
            clean_shutdown: false,
        }
    }

    /// 静まるまで書かず、静まったら一度だけ書く。
    #[test]
    fn a_request_waits_for_the_quiet_period() {
        let directory = tempfile::tempdir().unwrap();
        let file = SessionFile::new(SessionPaths::new(directory.path().join("session.json")));
        let mut autosave = Autosave::new(file);
        let start = Instant::now();

        autosave.request(snapshot(r"C:\work"), start);
        // 期限の手前では書かない。
        autosave.flush_if_due(start + SAVE_DEBOUNCE / 2);
        assert!(autosave.failure().is_none());
        assert_eq!(autosave.next_check(), Some(start + SAVE_DEBOUNCE));

        // 期限を過ぎたら書いて、未保存が消える。
        autosave.flush_if_due(start + SAVE_DEBOUNCE);
        let mut now = start + SAVE_DEBOUNCE;
        wait_for_write(&mut autosave, &mut now);
        assert_eq!(autosave.next_check(), None);
    }

    /// 変更が続いても上限で一度書く。
    #[test]
    fn a_burst_of_changes_still_writes_by_the_cap() {
        let directory = tempfile::tempdir().unwrap();
        let file = SessionFile::new(SessionPaths::new(directory.path().join("session.json")));
        let mut autosave = Autosave::new(file);
        let start = Instant::now();

        autosave.request(snapshot(r"C:\work"), start);
        // 上限の直前に変更が来た。静かさは満たしていないが、最初の変更から上限を過ぎている。
        let now = start + SAVE_DEBOUNCE_CAP;
        autosave.request(snapshot(r"C:\work\sub"), now - Duration::from_millis(1));

        autosave.flush_if_due(now);
        let mut now = now;
        wait_for_write(&mut autosave, &mut now);
        assert_eq!(autosave.next_check(), None);
    }

    /// 書けない間は待ち時間 0 の再試行を続けず、回数を超えたら自動を止める。
    #[test]
    fn a_failing_write_backs_off_and_stops() {
        let directory = tempfile::tempdir().unwrap();
        // 親が通常ファイルなので、保存先の directory を作れない。
        let blocker = directory.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let file = SessionFile::new(SessionPaths::new(blocker.join("session.json")));
        let mut autosave = Autosave::new(file);
        let start = Instant::now();

        autosave.request(snapshot(r"C:\work"), start);
        let mut now = start + SAVE_DEBOUNCE;
        for _ in 0..=MAX_WRITE_ATTEMPTS {
            autosave.flush_if_due(now);
            wait_for_write(&mut autosave, &mut now);
            // 次の確認は必ず未来にある。0ms で回り続けない。
            let next = autosave.next_check();
            assert!(next.is_none_or(|at| at > now), "{next:?}");
            now += SAVE_DEBOUNCE;
        }
        assert!(autosave.failure().is_some());
        // 自動では諦めている。
        assert_eq!(autosave.next_check(), None);

        // 次の変更でまた試す。
        autosave.request(snapshot(r"C:\work"), now);
        assert!(autosave.next_check().is_some());
    }

    /// 終了時は debounce を待たずに書き、正常終了の印を残す。
    #[test]
    fn a_shutdown_write_skips_the_wait_and_marks_clean() {
        let directory = tempfile::tempdir().unwrap();
        let file = SessionFile::new(SessionPaths::new(directory.path().join("session.json")));
        let mut autosave = Autosave::new(file);

        autosave.request(snapshot(r"C:\work"), Instant::now());
        let mut clean = snapshot(r"C:\work");
        clean.clean_shutdown = true;
        autosave.flush_now(clean);

        let mut now = Instant::now() + SAVE_DEBOUNCE_CAP;
        wait_for_write(&mut autosave, &mut now);
        assert_eq!(autosave.next_check(), None);
        let bytes = std::fs::read(directory.path().join("session.json")).unwrap();
        let written = SessionSnapshot::from_json(&bytes).unwrap();
        assert!(written.clean_shutdown);
        // 保存する revision は writer が決める。
        assert_eq!(written.write_revision, 1);
    }

    /// 保存を止めた起動は、読みも書きもしない。
    #[test]
    fn a_disabled_startup_saves_nothing() {
        let startup = Startup::disabled();
        assert!(!SessionAutosaveOwner::disabled().has_writer());
        assert!(startup.restore.is_none());
        assert!(startup.notice.is_none());
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    /// 中心が work area の中なら、保存した位置をそのまま使う。
    #[test]
    fn a_visible_window_keeps_its_saved_position() {
        let area = rect(0.0, 0.0, 1920.0, 1080.0);
        let saved = rect(100.0, 100.0, 800.0, 600.0);
        assert_eq!(fit_into(saved, area), saved);
    }

    /// 画面外の window は work area の中央へ戻す。大きさは変えない。
    #[test]
    fn an_off_screen_window_is_centred_in_the_work_area() {
        let area = rect(0.0, 0.0, 1920.0, 1080.0);
        // 右外へ完全に出ている。中心は display の上に無い。
        let saved = rect(3000.0, 100.0, 800.0, 600.0);
        let fitted = fit_into(saved, area);
        assert_eq!(fitted.center(), area.center());
        assert_eq!(fitted.size, saved.size);
    }

    /// work area より大きい window は、大きさを収めてから中央へ置く。
    #[test]
    fn an_oversized_off_screen_window_is_clamped_to_the_work_area() {
        let area = rect(0.0, 0.0, 1280.0, 720.0);
        let saved = rect(-2000.0, -2000.0, 3000.0, 2000.0);
        let fitted = fit_into(saved, area);
        assert_eq!(fitted.size, area.size);
        assert_eq!(fitted.center(), area.center());
    }

    /// 中心が入っている display を優先して選ぶ。
    #[test]
    fn the_display_containing_the_centre_wins() {
        let left = rect(0.0, 0.0, 1920.0, 1080.0);
        let right = rect(1920.0, 0.0, 1920.0, 1080.0);
        let areas = [left, right];

        // 左より右に深く掛かり、中心も右の上にある。
        assert_eq!(
            choose_work_area(rect(1800.0, 100.0, 800.0, 600.0), &areas),
            Some(1)
        );
        // 中心が境界の手前にある。重なりも左が大きい。
        assert_eq!(
            choose_work_area(rect(0.0, 0.0, 800.0, 600.0), &areas),
            Some(0)
        );
        assert_eq!(
            choose_work_area(rect(1000.0, 100.0, 800.0, 600.0), &areas),
            Some(0)
        );
        // どれとも重ならない。display も無い。
        assert_eq!(
            choose_work_area(rect(5000.0, 5000.0, 800.0, 600.0), &areas),
            None
        );
        assert_eq!(choose_work_area(left, &[]), None);
    }

    /// 壊れた保存値は受け取らない。
    #[test]
    fn broken_window_values_are_refused() {
        let ok = BoundsValue {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(rect_of(ok), Some(rect(10.0, 20.0, 800.0, 600.0)));
        assert_eq!(
            rect_of(BoundsValue {
                width: f32::NAN,
                ..ok
            }),
            None
        );
        assert_eq!(
            rect_of(BoundsValue {
                y: f32::INFINITY,
                ..ok
            }),
            None
        );
        // v5 の `load_window_bounds` と同じく、小さすぎる寸法は既定へ落とす。
        assert_eq!(
            rect_of(BoundsValue {
                width: MIN_WINDOW_WIDTH - 1.0,
                ..ok
            }),
            None
        );
        assert_eq!(
            rect_of(BoundsValue {
                height: MIN_WINDOW_HEIGHT - 1.0,
                ..ok
            }),
            None
        );
    }
}
