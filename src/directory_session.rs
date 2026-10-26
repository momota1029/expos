//! 一覧の読取ライフサイクル。GPUI にも filesystem にも依存しない。
//!
//! 以前は `main.rs` が `directory` / `listing_ready` / `listing_error` を別々に持ち、
//! 列挙は GPUI の asset だった。asset は path をキーにするので、同じ path の
//! 再読込が新しい読取にならない。ここでは location と request の寿命を分け、
//! 古い結果を commit できないようにする。
//!
//! この型は GPUI の event / entity / window を受け取らない。入力を処理して
//! [`SessionEffect`] を返し、application adapter が実行する。

use crate::location::Location;
use crate::navigation_history::{NavigationHistory, ViewBookmark};
use crate::{
    directory::{DirectoryError, EntryKind, Listing},
    everything::{FolderSizeCacheGeneration, FolderSizeProbe, FolderSizeResult, ProviderRevision},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// 読取の識別子。commit できるかを判定するために使う。
///
/// `LocationEpoch` は A → B → A の最初の A を拒否し、`FreshnessEpoch` は同じ
/// location の古い読取を拒否する。`ScanId` は同じ世代の重複 completion を拒否する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanTicket {
    pub location_epoch: u64,
    pub freshness_epoch: u64,
    pub scan_id: u64,
    pub path: PathBuf,
    /// 読取を始めた時点の `dirty_seq`。
    ///
    /// **`begin_scan` だけが書く。** 呼出し元が上書きすると、実行中 scan が捕捉した
    /// 値を失い、その間の変更が reconcile されない。
    pub dirty_at_start: u64,
}

/// Identity for one fresh archive read. Repeated reads of the same typed address
/// are distinct and a return to an older address cannot revive its request.
#[derive(Clone, Debug)]
pub struct ArchiveTicket {
    pub location_epoch: u64,
    pub freshness_epoch: u64,
    pub request_id: u64,
    pub location: Location,
    cancellation: Arc<AtomicBool>,
}

impl PartialEq for ArchiveTicket {
    fn eq(&self, other: &Self) -> bool {
        self.location_epoch == other.location_epoch
            && self.freshness_epoch == other.freshness_epoch
            && self.request_id == other.request_id
            && self.location == other.location
            && Arc::ptr_eq(&self.cancellation, &other.cancellation)
    }
}

impl Eq for ArchiveTicket {}

impl ArchiveTicket {
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancellation.clone()
    }

    fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

/// A committed listing に属する Everything metadata batch の identity。
///
/// filesystem listing の topology は `ScanTicket` が authority のまま保持する。これは
/// 外部 index の display-only enrichment だけを、その snapshot へ一度だけ足す ticket。
#[derive(Clone, Debug)]
pub struct FolderSizeTicket {
    pub listing: ScanTicket,
    pub request_id: u64,
    dirty_at_start: u64,
    pub provider_revision: ProviderRevision,
    pub cache_generation: FolderSizeCacheGeneration,
    _freshness_lease: Arc<crate::everything::FolderSizeCacheLease>,
    cancellation: Arc<AtomicBool>,
}

impl PartialEq for FolderSizeTicket {
    fn eq(&self, other: &Self) -> bool {
        self.listing == other.listing
            && self.request_id == other.request_id
            && self.dirty_at_start == other.dirty_at_start
            && self.provider_revision == other.provider_revision
            && self.cache_generation == other.cache_generation
    }
}

impl Eq for FolderSizeTicket {}

impl FolderSizeTicket {
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancellation.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSizeRequest {
    pub ticket: FolderSizeTicket,
    pub probes: Vec<FolderSizeProbe>,
    pub provider_revision: ProviderRevision,
    pub cache_generation: FolderSizeCacheGeneration,
    pub bypass_cache: bool,
}

/// Delayed retry identity, tied to one accepted listing and retry generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSizeRetryToken {
    listing: ScanTicket,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderSizeRetry {
    Immediate,
    Delayed {
        token: FolderSizeRetryToken,
        delay: std::time::Duration,
    },
}

#[derive(Clone, Debug, Default)]
struct FolderSizeRetryState {
    listing: Option<ScanTicket>,
    generation: u64,
    immediate_retries: u8,
    delayed_scheduled: bool,
    next_delay: std::time::Duration,
    delayed_request_id: Option<u64>,
    last_completed_request_id: Option<u64>,
}

const FOLDER_SIZE_IMMEDIATE_RETRIES: u8 = 2;
const FOLDER_SIZE_RETRY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const FOLDER_SIZE_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// path を持たない location の読込要求。Shell adapter はこの identity を返却時まで運ぶ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualTicket {
    pub location_epoch: u64,
    pub request_id: u64,
    pub location: Location,
}

/// 一覧の表示状態。空一覧と読取失敗を混ぜない。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ListingStatus {
    /// まだ一度も読めていない。
    #[default]
    InitialLoading,
    /// 現在の location の内容を表示している。
    Ready,
    /// 前の内容を残したまま読み直している。
    ///
    /// `refresh` から入る。B1b で F5 を接続するまで構築されない。
    #[allow(dead_code)]
    Refreshing,
    /// 一度も読めずに失敗した。
    Unavailable(DirectoryError),
    /// 前の内容を残したまま読み直しに失敗した。
    RefreshFailed(DirectoryError),
}

impl ListingStatus {
    /// 読取中か。UI の進行表示に使う。
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::InitialLoading | Self::Refreshing)
    }

    /// 表示すべきエラー。last-good を残している場合も理由を出す。
    pub fn error(&self) -> Option<&DirectoryError> {
        match self {
            Self::Unavailable(error) | Self::RefreshFailed(error) => Some(error),
            _ => None,
        }
    }
}

/// session が adapter へ渡す要求。effect 全体の汎用 framework は作らない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEffect {
    /// この ticket で列挙を開始する。
    Enumerate(ScanTicket),
    /// Read the current ZIP location through the bounded archive adapter.
    EnumerateArchive(ArchiveTicket),
    /// Shell namespace の行を解決する。filesystem 列挙や watch は開始しない。
    LoadVirtual(VirtualTicket),
    /// この path の監視を始める。
    ///
    /// adapter は `run_effect` の中で同期実行せず、background executor へ渡す。
    /// `RecommendedWatcher::watch` は切断されたネットワーク path で短く block し得る。
    ///
    /// `location_epoch` も渡す。callback から届いた hint を、購読した時点の
    /// location のものとして照合できるようにするため。
    SubscribeWatch {
        token: u64,
        location_epoch: u64,
        path: PathBuf,
    },
    /// この token の監視をやめる。
    UnsubscribeWatch { token: u64 },
}

/// 監視の状態。
///
/// `Degraded` を session の中だけに置くと、「変化がなかった」と「もう観測していない」を
/// ユーザーが区別できない。状態行へ出すのは adapter の役割。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WatchState {
    /// まだ登録していない。demo や、登録前の短い期間。
    #[default]
    NotWatched,
    /// 登録できている。
    Watching,
    /// 登録に失敗したか、callback がエラーを報告した。
    ///
    /// 自動更新は無いが F5 は残る。
    Degraded(String),
}

/// watch が dirty にした対象。
///
/// 上限を超えたら path を捨てて `All` へ畳む。overflow / `need_rescan` / path 不明も
/// `All` として扱う。
///
/// adapter の接続は次の段階。今はテストだけが `Paths` を作る。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WatchHint {
    All,
    Paths(Vec<PathBuf>),
}

/// subscription ごとの mailbox 上限。
///
/// 主たる境界は adapter の callback 側で、queue へ入れる前に適用する。
/// ここは二重の防御。
pub const WATCH_MAILBOX_PATHS: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewVisibility {
    #[default]
    Visible,
    Hidden,
}

/// 一覧の読取ライフサイクルの唯一の所有者。
#[derive(Default)]
pub struct DirectorySession {
    /// requested location の唯一の owner。ここから導出し、別に持たない。
    history: NavigationHistory,
    /// 初回の暗黙 startup effect は、failure 後も一度だけにする。
    startup_consumed: bool,
    location_epoch: u64,
    freshness_epoch: u64,
    scan_id: u64,
    archive_request_id: u64,
    pending_archive: Option<ArchiveTicket>,
    accepted_archive: Option<crate::archive_manifest::ArchiveManifest>,
    virtual_request_id: u64,
    /// 実行中の virtual load。完了を一度だけ受理し、同じ ticket の重複 terminal
    /// delivery で status/projection をもう一度書き換えない。
    pending_virtual: Option<VirtualTicket>,
    accepted_virtual: Option<VirtualTicket>,
    status: ListingStatus,
    /// 直近に commit できた内容。読取失敗のときも表示を保つために残す。
    ///
    /// ここが記録する location は読取の provenance であって、navigation の
    /// authority ではない。
    last_good: Option<Listing>,
    /// 実行中の読取。commit できるのはこれと一致する結果だけ。
    pending: Option<ScanTicket>,
    /// 現在の `last_good` を作った scan。external metadata はこれと完全一致するときだけ
    /// 足せる。path だけで古い結果を再入場へ混ぜない。
    accepted_listing: Option<ScanTicket>,
    /// accepted listing と同じ列挙結果から取得した親 directory identity。
    directory_identity: Option<crate::filesystem_identity::FilesystemIdentity>,
    folder_size_request_id: u64,
    pending_folder_sizes: Option<FolderSizeTicket>,
    folder_size_cache_bypass: bool,
    folder_size_retry: FolderSizeRetryState,
    #[cfg(test)]
    folder_size_requests_disabled_for_test: bool,
    /// 監視の subscription identity。作成・再接続のたびに進める。
    ///
    /// `location_epoch` とは役割が違う。こちらは「この subscription がまだ生きているか」、
    /// `location_epoch` は「この hint は今見ている location の話か」を答える。
    watch_token: u64,
    /// 受理した watch hint の数。`freshness_epoch` とは別に進める。
    ///
    /// 変更が続くたびに scan を無効化すると永久に一枚も表示できないため、
    /// hint は実行中 scan の freshness を進めない。
    dirty_seq: u64,
    /// follow-up scan が未処理か。
    ///
    /// 消せるのは `dirty_at_start` がその時点の `dirty_seq` と一致する scan が
    /// **開始した**ときだけ。
    pending_dirty: bool,
    /// Shell の workspace/tab state から渡される使い捨て可視性 projection。
    /// hidden session は dirty を保持し、表示に戻ると root を再列挙する。
    visibility: ViewVisibility,
    watch_state: WatchState,
}

impl DirectorySession {
    pub fn navigation_snapshot(&self) -> crate::navigation_history::NavigationHistorySnapshot {
        self.history.snapshot()
    }

    pub fn navigation_history(&self) -> &NavigationHistory {
        &self.history
    }

    pub fn from_navigation_snapshot(
        snapshot: crate::navigation_history::NavigationHistorySnapshot,
    ) -> Self {
        let mut session = Self::default();
        session.history = NavigationHistory::from_snapshot(snapshot);
        session
    }
    pub fn location(&self) -> Option<&Location> {
        self.history.location()
    }

    /// Shell の visible-pane/tab projection を受け取る。非表示から戻るたびに
    /// root を再検証し、既に scan 中なら dirty debt として一度だけ coalesce する。
    pub fn set_visible(&mut self, visible: bool) -> Option<SessionEffect> {
        let was_visible = self.visibility == ViewVisibility::Visible;
        self.visibility = if visible {
            ViewVisibility::Visible
        } else {
            ViewVisibility::Hidden
        };
        if !visible || was_visible {
            return None;
        }
        if !matches!(self.history.location(), Some(Location::Path(_))) {
            return None;
        }
        self.accept_dirty(WatchHint::All);
        self.take_follow_up()
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == ViewVisibility::Visible
    }

    /// 今の entry に保存されている表示状態（スクロールと選択）。
    ///
    /// Back / Forward の直後に view が読んで stash する。session 側に寿命を
    /// 持たせないので、follow-up scan や再読込に匂われない。
    pub fn current_bookmark(&self) -> Option<&ViewBookmark> {
        self.history.bookmark()
    }

    /// The current visible rows may refresh the navigation bookmark only when the accepted
    /// listing still belongs to the current path and location epoch.
    pub fn has_accepted_current_listing(&self) -> bool {
        match self.location() {
            Some(Location::Path(path)) => self.accepted_listing.as_ref().is_some_and(|ticket| {
                &ticket.path == path && ticket.location_epoch == self.location_epoch
            }),
            Some(
                location @ (Location::ThisPc | Location::ShellRoot(_) | Location::ShellItem { .. }),
            ) => self.accepted_virtual.as_ref().is_some_and(|ticket| {
                &ticket.location == location && ticket.location_epoch == self.location_epoch
            }),
            Some(Location::Archive { .. }) => self.accepted_archive.is_some(),
            None => false,
        }
    }

    pub fn set_current_bookmark(&mut self, bookmark: Option<ViewBookmark>) {
        self.history.set_bookmark(bookmark);
    }

    pub fn status(&self) -> &ListingStatus {
        &self.status
    }

    #[cfg(test)]
    pub(crate) fn archive_cancellation_for_test(&self) -> Option<Arc<AtomicBool>> {
        self.pending_archive
            .as_ref()
            .map(ArchiveTicket::cancellation)
    }

    /// 表示すべき内容。読取中や失敗中でも last-good を返す。
    pub fn listing(&self) -> Option<&Listing> {
        self.last_good.as_ref()
    }

    /// Accepted detached ZIP metadata. Presentation rows are rebuilt from this value.
    pub fn archive_manifest(&self) -> Option<&crate::archive_manifest::ArchiveManifest> {
        self.accepted_archive.as_ref()
    }

    /// 現在の accepted listing を供給した directory object。取得不能なら `None`。
    #[allow(dead_code)] // case-only rename の acceptance snapshot で使用する。
    pub fn directory_identity(&self) -> Option<&crate::filesystem_identity::FilesystemIdentity> {
        self.directory_identity.as_ref()
    }

    pub fn accepted_scan_ticket(&self) -> Option<&ScanTicket> {
        self.accepted_listing.as_ref()
    }

    /// 実行中の読取。
    ///
    /// B1b の F5 と、進行表示の判定で使う。
    #[allow(dead_code)]
    pub fn pending(&self) -> Option<&ScanTicket> {
        self.pending.as_ref()
    }

    /// demo の合成一覧を直接置く。列挙しないので location を持たない。
    ///
    /// 実ファイルを読む経路と混ざらないよう、`navigate` した時点で上書きされる。
    pub fn adopt_demo(&mut self, listing: Listing) {
        self.startup_consumed = true;
        if let Some(ticket) = self.pending_archive.take() {
            ticket.cancel();
        }
        self.cancel_folder_size_request();
        self.reset_folder_size_retry(None);
        self.last_good = Some(listing);
        self.accepted_listing = None;
        self.directory_identity = None;
        self.status = ListingStatus::Ready;
    }

    /// 起動時の場所を決める。列挙はまだ始めない。
    ///
    /// `new_at` は `Self` を組み立てるだけで effect を実行できないので、最初の
    /// 読取は adapter が `take_startup_effect` で拾う。
    pub fn start_at(&mut self, location: impl Into<Location>) {
        self.startup_consumed = false;
        self.history.navigate(location, None);
        self.status = ListingStatus::InitialLoading;
    }

    /// 起動時の読取要求を一度だけ取り出す。
    ///
    /// 監視の登録もここで一緒に要求する。列挙と watch 登録は共に background で
    /// 始まり、登録の完了を待って初回表示を止めない。
    pub fn take_startup_effects(&mut self) -> Vec<SessionEffect> {
        if self.startup_consumed {
            return Vec::new();
        }
        if self.history.is_empty() {
            return Vec::new();
        }
        self.startup_consumed = true;
        if self.pending.is_some() || self.last_good.is_some() {
            return Vec::new();
        }
        let mut effects = self.resubscribe_watch();
        match self.location() {
            Some(Location::Path(_)) if self.is_visible() => effects.push(self.begin_scan()),
            Some(Location::Path(_)) => {
                self.accept_dirty(WatchHint::All);
            }
            Some(Location::ThisPc | Location::ShellRoot(_) | Location::ShellItem { .. }) => {
                effects.push(self.begin_virtual())
            }
            Some(Location::Archive { .. }) => effects.push(self.begin_archive()),
            None => {}
        }
        effects
    }

    /// 別の場所へ移動する。同じ path は何もしない。
    ///
    /// 旧 snapshot を離し、新しい読取を要求する。旧 path の rows を新 path の
    /// 下へ置かないよう、`last_good` もここで落とす。
    ///
    /// 監視は path に結びつくので、token を進めて張り直す。古い token の
    /// callback はここで無効になる。
    ///
    /// `bookmark_for_current` は移動直前の一覧のスクロール位置。移動と同時に
    /// 渡さないと、遅れて届いた書込みが別の entry を指してしまう。
    pub fn navigate(
        &mut self,
        location: impl Into<Location>,
        bookmark_for_current: Option<ViewBookmark>,
    ) -> Vec<SessionEffect> {
        if !self.history.navigate(location, bookmark_for_current) {
            return Vec::new();
        }
        self.enter_current_location()
    }

    /// 履歴を一つ戻る。範囲外なら何もしない。
    pub fn back(&mut self, bookmark_for_current: Option<ViewBookmark>) -> Vec<SessionEffect> {
        if self.history.back(bookmark_for_current).is_none() {
            return Vec::new();
        }
        self.enter_current_location()
    }

    /// 履歴を一つ進む。範囲外なら何もしない。
    pub fn forward(&mut self, bookmark_for_current: Option<ViewBookmark>) -> Vec<SessionEffect> {
        if self.history.forward(bookmark_for_current).is_none() {
            return Vec::new();
        }
        self.enter_current_location()
    }

    /// 戻れる履歴があるか。UI の disabled 判定が読む。
    pub fn can_back(&self) -> bool {
        self.history.can_back()
    }

    /// 進める履歴があるか。
    pub fn can_forward(&self) -> bool {
        self.history.can_forward()
    }

    /// 新しい location を読みにいく準備。
    ///
    /// epoch を進めるので、移動前の結果はすべて commit 不可になる。旧 path の rows を
    /// 新 path の下へ置かないよう `last_good` も離す。
    fn enter_current_location(&mut self) -> Vec<SessionEffect> {
        self.startup_consumed = true;
        self.location_epoch = self.location_epoch.wrapping_add(1);
        self.cancel_folder_size_request();
        self.reset_folder_size_retry(None);
        self.last_good = None;
        self.accepted_archive = None;
        if let Some(ticket) = self.pending_archive.take() {
            ticket.cancel();
        }
        self.accepted_listing = None;
        self.directory_identity = None;
        self.pending_virtual = None;
        self.accepted_virtual = None;
        self.status = ListingStatus::InitialLoading;
        // 旧 location の dirty は持ち越さない。新しい scan が全部を読む。
        self.pending_dirty = false;
        let mut effects = self.resubscribe_watch();
        match self.location() {
            Some(Location::Path(_)) if self.is_visible() => effects.push(self.begin_scan()),
            Some(Location::Path(_)) => {
                self.accept_dirty(WatchHint::All);
            }
            Some(Location::ThisPc | Location::ShellRoot(_) | Location::ShellItem { .. }) => {
                effects.push(self.begin_virtual())
            }
            Some(Location::Archive { .. }) => effects.push(self.begin_archive()),
            None => {}
        }
        effects
    }

    /// 同じ場所を読み直す。F5 相当。
    ///
    /// navigation と別 intent なので no-op 判定を通さない。last-good があれば
    /// 残したまま refreshing にする。
    ///
    /// F5 は監視を張り直さない。`freshness_epoch` だけを進める。
    pub fn refresh(&mut self) -> Option<SessionEffect> {
        if matches!(self.history.location(), Some(Location::Archive { .. })) {
            self.freshness_epoch = self.freshness_epoch.wrapping_add(1);
            self.status = if self.accepted_archive.is_some() {
                ListingStatus::Refreshing
            } else {
                ListingStatus::InitialLoading
            };
            return Some(self.begin_archive());
        }
        if !matches!(self.history.location(), Some(Location::Path(_))) {
            return None;
        }
        self.freshness_epoch = self.freshness_epoch.wrapping_add(1);
        self.invalidate_folder_sizes();
        self.status = if self.last_good.is_some() {
            ListingStatus::Refreshing
        } else {
            ListingStatus::InitialLoading
        };
        Some(self.begin_scan())
    }

    /// PC の段階的 asset projection がこの virtual request に属するかを確認する。
    ///
    /// virtual root は refresh / watch を持たない。この commit は ticket の stale rejection
    /// と初回 loading state の終端だけを担う。
    pub fn commit_virtual(&mut self, ticket: &VirtualTicket) -> bool {
        if !self.accepts_virtual(ticket) {
            return false;
        }
        self.status = ListingStatus::Ready;
        self.accepted_virtual = Some(ticket.clone());
        true
    }

    /// 一回だけ返る Shell namespace 読込を終端する。
    ///
    /// PC は複数 asset が段階で届くため [`Self::commit_virtual`] を使い続ける。対して
    /// Shell enumeration は成功でも失敗でもこの ticket を消費し、重複 completion を拒む。
    pub fn complete_virtual(&mut self, ticket: &VirtualTicket) -> bool {
        if !self.accepts_virtual(ticket) {
            return false;
        }
        self.pending_virtual = None;
        self.status = ListingStatus::Ready;
        self.accepted_virtual = Some(ticket.clone());
        true
    }

    /// ZIP 読取を一度だけ確定し、source/route lease と旧値を破棄用に返す。
    /// 採否の判断後も custody を返却値に残し、呼出側の background executor で解放する。
    pub(crate) fn commit_archive_result(
        &mut self,
        ticket: &ArchiveTicket,
        result: Result<crate::archive_listing::ArchiveReadResult, DirectoryError>,
    ) -> ArchiveCommitOutcome {
        let (mut manifest, custody, mut failure) = match result {
            Ok(read) => {
                let (manifest, custody) = read.into_parts();
                (Some(manifest), Some(custody), None)
            }
            Err(error) => (None, None, Some(error)),
        };
        let displaced = self.commit_archive_parts(ticket, &mut manifest, &mut failure);
        let committed = displaced.is_some();
        ArchiveCommitOutcome {
            committed,
            disposal: ArchiveCommitDisposal {
                _incoming_manifest: manifest,
                _custody: custody,
                _failure: failure,
                _replaced_failure: None,
                _displaced: displaced,
            },
        }
    }

    #[cfg(test)]
    pub fn commit_archive(
        &mut self,
        ticket: &ArchiveTicket,
        result: Result<crate::archive_listing::ArchiveReadResult, DirectoryError>,
    ) -> bool {
        let outcome = self.commit_archive_result(ticket, result);
        let committed = outcome.committed;
        drop(outcome);
        committed
    }

    /// detached manifest と failure を ticket 検証後に move する。
    /// provider gate 内では旧値を返し、呼出側が lock 解放後に drop する。
    pub(crate) fn commit_archive_parts(
        &mut self,
        ticket: &ArchiveTicket,
        manifest: &mut Option<crate::archive_manifest::ArchiveManifest>,
        failure: &mut Option<DirectoryError>,
    ) -> Option<ArchiveCommitDisplaced> {
        if !self.accepts_archive(ticket) || (manifest.is_none() == failure.is_none()) {
            return None;
        }
        let old_manifest = manifest
            .take()
            .map(|manifest| self.accepted_archive.replace(manifest))
            .unwrap_or(None);
        let old_pending = self.pending_archive.take();
        let new_status = if let Some(error) = failure.take() {
            if self.accepted_archive.is_some() {
                ListingStatus::RefreshFailed(error)
            } else {
                ListingStatus::Unavailable(error)
            }
        } else {
            ListingStatus::Ready
        };
        let old_status = std::mem::replace(&mut self.status, new_status);
        Some(ArchiveCommitDisplaced {
            _manifest: old_manifest,
            _pending: old_pending,
            _status: old_status,
        })
    }

    #[cfg(windows)]
    pub(crate) fn commit_archive_7z(
        &mut self,
        owner: &crate::archive_provider::ArchiveProviderOwner,
        capability: Option<&crate::archive_provider::Archive7zReadCapability>,
        ticket: &ArchiveTicket,
        result: Result<crate::archive_listing::ArchiveReadResult, DirectoryError>,
    ) -> ArchiveCommitOutcome {
        // provider 不一致時は gate の外で incoming manifest を捨て、current ticket だけを failure にする。
        let (mut manifest, custody, mut failure) = match result {
            Ok(read) => {
                let (manifest, custody) = read.into_parts();
                (Some(manifest), Some(custody), None)
            }
            Err(error) => (None, None, Some(error)),
        };
        let mut displaced = None;
        let transition = capability.and_then(|capability| {
            owner.with_current_7z_read(capability, || {
                displaced = self.commit_archive_parts(ticket, &mut manifest, &mut failure);
                displaced.is_some()
            })
        });
        let mut rejected_manifest = None;
        let mut replaced_failure = None;
        let committed = match transition {
            Some(committed) => committed,
            None => {
                std::mem::swap(&mut rejected_manifest, &mut manifest);
                replaced_failure = failure.take();
                failure = Some(if capability.is_some() {
                    DirectoryError(
                        "7z provider changed before the listing could be committed".into(),
                    )
                } else {
                    DirectoryError("current 7z read capability is unavailable".into())
                });
                displaced = self.commit_archive_parts(ticket, &mut manifest, &mut failure);
                displaced.is_some()
            }
        };
        ArchiveCommitOutcome {
            committed,
            disposal: ArchiveCommitDisposal {
                _incoming_manifest: rejected_manifest.or(manifest),
                _custody: custody,
                _failure: failure,
                _replaced_failure: replaced_failure,
                _displaced: displaced,
            },
        }
    }

    /// 読取の結果を受け取る。commit できない結果は捨てる。
    ///
    /// 戻り値は「commit したか」。捨てた場合も worker slot は解放される。
    ///
    /// dirty の判定は `accepts` を通った後だけ行う。捨てる結果が dirty 状態へ
    /// 書き込んではならない。
    #[cfg(test)]
    pub fn commit(&mut self, ticket: &ScanTicket, result: Result<Listing, DirectoryError>) -> bool {
        self.commit_enumerated(ticket, result.map(Into::into))
    }

    /// 列挙結果と親 identity を accepted ticket と一緒に commit する。
    pub fn commit_enumerated(
        &mut self,
        ticket: &ScanTicket,
        result: Result<crate::directory::EnumeratedListing, DirectoryError>,
    ) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        self.pending = None;
        match result {
            Ok(listing) => {
                self.cancel_folder_size_request();
                self.reset_folder_size_retry(Some(ticket));
                self.last_good = Some(listing.entries);
                self.accepted_listing = Some(ticket.clone());
                self.directory_identity = listing.directory_identity;
                self.status = ListingStatus::Ready;
            }
            Err(error) => {
                // last-good は残す。初回なら unavailable。
                self.status = if self.last_good.is_some() {
                    ListingStatus::RefreshFailed(error)
                } else {
                    ListingStatus::Unavailable(error)
                };
            }
        }
        // 読取が始まった後に来た変更は、この結果に含まれていない。
        // `!=` で見るのは、`wrapping_add` を使う以上 `>` がラップアラウンドで壊れるため。
        if self.dirty_seq != ticket.dirty_at_start {
            self.pending_dirty = true;
        }
        true
    }

    /// Current listing の folder path だけを detached Everything request に写す。
    ///
    /// request 中の watch/F5/navigation は `pending_folder_sizes` を直ちに無効にする。
    /// IPC の cooperative cancellation を待たず、遅い結果を commit 側で捨てる。
    pub fn request_folder_sizes(&mut self) -> Option<FolderSizeRequest> {
        #[cfg(test)]
        if self.folder_size_requests_disabled_for_test {
            return None;
        }
        if self.pending.is_some() || self.pending_folder_sizes.is_some() {
            return None;
        }
        let listing = self.last_good.as_ref()?;
        let listing_ticket = self.accepted_listing.clone()?;
        if self.dirty_seq != listing_ticket.dirty_at_start {
            return None;
        }
        let (cache_generation, freshness_lease) =
            crate::everything::begin_folder_size_cache_lease();
        let probes: Vec<_> = listing
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::Folder && !entry.is_link() && entry.folder_size.is_none()
            })
            .filter_map(|entry| {
                let path = entry.path.clone()?;
                Some(FolderSizeProbe {
                    key: entry.key.clone(),
                    freshness: freshness_lease.register(&path),
                    path,
                    source_identity: entry.source_identity.clone()?,
                    cache_generation,
                })
            })
            .collect();
        if probes.is_empty() {
            return None;
        }
        self.folder_size_request_id = self.folder_size_request_id.wrapping_add(1);
        let ticket = FolderSizeTicket {
            listing: listing_ticket,
            request_id: self.folder_size_request_id,
            dirty_at_start: self.dirty_seq,
            cancellation: Arc::new(AtomicBool::new(false)),
            provider_revision: ProviderRevision::current(),
            cache_generation,
            _freshness_lease: freshness_lease,
        };
        let provider_revision = ticket.provider_revision;
        self.pending_folder_sizes = Some(ticket.clone());
        Some(FolderSizeRequest {
            ticket,
            probes,
            provider_revision,
            cache_generation,
            bypass_cache: self.folder_size_cache_bypass,
        })
    }

    /// Everything index size を current listing へ足す。listing の entry/path/kind が一致する
    /// 成功分だけを受理し、provider failure を directory load error にはしない。
    pub fn commit_folder_sizes(
        &mut self,
        ticket: &FolderSizeTicket,
        results: Vec<FolderSizeResult>,
    ) -> bool {
        if self.pending_folder_sizes.as_ref() != Some(ticket)
            || self.accepted_listing.as_ref() != Some(&ticket.listing)
            || self.pending.is_some()
            || self.dirty_seq != ticket.dirty_at_start
        {
            return false;
        }
        self.pending_folder_sizes = None;
        self.folder_size_cache_bypass = false;
        self.folder_size_retry.last_completed_request_id = Some(ticket.request_id);
        // Provider failure may have advanced its epoch.  Consume this metadata
        // request without touching the last-good listing in that case.
        if ProviderRevision::current() != ticket.provider_revision
            || results.iter().any(|result| {
                result.revision != ticket.provider_revision
                    || result.cache_generation != ticket.cache_generation
            })
        {
            return false;
        }
        let results: Vec<_> = results
            .into_iter()
            .filter(|result| result.freshness.is_current())
            .collect();
        let Some(listing) = self.last_good.as_ref() else {
            return false;
        };
        let results: std::collections::HashMap<_, _> = results
            .into_iter()
            .map(|result| (result.key, (result.path, result.fact)))
            .collect();
        let mut changed = false;
        let entries = listing
            .iter()
            .cloned()
            .map(|mut entry| {
                if let Some((path, fact)) = results.get(&entry.key)
                    && entry.path.as_ref() == Some(path)
                    && entry.kind == EntryKind::Folder
                {
                    entry.folder_size = Some(*fact);
                    changed = true;
                }
                entry
            })
            .collect();
        if changed {
            self.last_good = Some(std::sync::Arc::new(entries));
        }
        changed
    }

    /// この結果を commit してよいか。
    ///
    /// owner が生存していることは呼び出し側が保証する。ここでは active ticket と
    /// scan ID の一致、location と freshness epoch の一致だけを見る。
    /// キャンセルできたかどうかは commit 条件にしない。
    fn accepts(&self, ticket: &ScanTicket) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        pending == ticket
            && self.location_epoch == ticket.location_epoch
            && self.freshness_epoch == ticket.freshness_epoch
            && self.history.location() == Some(&Location::Path(ticket.path.clone()))
    }

    fn accepts_archive(&self, ticket: &ArchiveTicket) -> bool {
        self.pending_archive.as_ref() == Some(ticket)
            && self.location_epoch == ticket.location_epoch
            && self.freshness_epoch == ticket.freshness_epoch
            && self.history.location() == Some(&ticket.location)
            && !ticket.cancellation.load(Ordering::Acquire)
    }

    /// 新しい読取を始める。実行中の要求は最新の一つへ集約する。
    ///
    /// `dirty_at_start` はここだけで書く。呼出し元が上書きすると、実行中 scan が
    /// 捕捉した値を失い、その間の変更が reconcile されない。
    fn begin_scan(&mut self) -> SessionEffect {
        self.startup_consumed = true;
        self.scan_id = self.scan_id.wrapping_add(1);
        let ticket = ScanTicket {
            location_epoch: self.location_epoch,
            freshness_epoch: self.freshness_epoch,
            scan_id: self.scan_id,
            path: self
                .history
                .location()
                .and_then(Location::path)
                .map(PathBuf::from)
                .expect("begin_scan is only called with a path location"),
            dirty_at_start: self.dirty_seq,
        };
        self.pending = Some(ticket.clone());
        // この scan が今の dirty を全部読むので、follow-up の借りはここで返す。
        self.pending_dirty = false;
        SessionEffect::Enumerate(ticket)
    }

    fn begin_virtual(&mut self) -> SessionEffect {
        self.startup_consumed = true;
        self.virtual_request_id = self.virtual_request_id.wrapping_add(1);
        let ticket = VirtualTicket {
            location_epoch: self.location_epoch,
            request_id: self.virtual_request_id,
            location: self
                .location()
                .cloned()
                .expect("virtual location is current"),
        };
        self.pending_virtual = Some(ticket.clone());
        SessionEffect::LoadVirtual(ticket)
    }

    fn begin_archive(&mut self) -> SessionEffect {
        self.startup_consumed = true;
        if let Some(previous) = self.pending_archive.take() {
            previous.cancel();
        }
        self.archive_request_id = self.archive_request_id.wrapping_add(1);
        let ticket = ArchiveTicket {
            location_epoch: self.location_epoch,
            freshness_epoch: self.freshness_epoch,
            request_id: self.archive_request_id,
            location: self
                .location()
                .cloned()
                .expect("archive location is current"),
            cancellation: Arc::new(AtomicBool::new(false)),
        };
        self.pending_archive = Some(ticket.clone());
        SessionEffect::EnumerateArchive(ticket)
    }

    /// Shell adapter が取得した仮想行を適用してよいか。
    pub fn accepts_virtual(&self, ticket: &VirtualTicket) -> bool {
        self.pending_virtual.as_ref() == Some(ticket)
            && ticket.location_epoch == self.location_epoch
            && ticket.request_id == self.virtual_request_id
            && self.location() == Some(&ticket.location)
    }

    /// 監視を張り直す。旧 token の callback はここで無効になる。
    ///
    /// 再接続も新しい token を発行する。同じ token を再送すると catch-up の
    /// 述語が決まらなくなる。
    pub fn resubscribe_watch(&mut self) -> Vec<SessionEffect> {
        let mut effects = Vec::new();
        // 登録完了前でも token は既に callback へ渡っている。Path → PC では
        // `watch_ready` の遅着を受理しないため、state ではなく token を失効させる。
        if self.watch_token != 0 {
            effects.push(SessionEffect::UnsubscribeWatch {
                token: self.watch_token,
            });
        }
        self.watch_token = self.watch_token.wrapping_add(1);
        self.watch_state = WatchState::NotWatched;
        let Some(path) = self
            .history
            .location()
            .and_then(Location::path)
            .map(PathBuf::from)
        else {
            return effects;
        };
        effects.push(SessionEffect::SubscribeWatch {
            token: self.watch_token,
            location_epoch: self.location_epoch,
            path,
        });
        effects
    }

    /// 監視の状態。
    ///
    /// adapter の接続は次の段階。今はテストだけが読む。
    #[allow(dead_code)]
    pub fn watch_state(&self) -> &WatchState {
        &self.watch_state
    }

    /// 実行中の subscription の token。
    #[allow(dead_code)]
    pub fn watch_token(&self) -> u64 {
        self.watch_token
    }

    /// 今の location の世代。
    ///
    /// watch hint の照合は adapter が購読時の値を運ぶので、ここはテストが読む。
    #[allow(dead_code)]
    pub fn location_epoch(&self) -> u64 {
        self.location_epoch
    }

    /// 登録に成功したことを受け取る。
    ///
    /// 同じ token の二度目は no-op。
    ///
    /// 登録より前に始まった scan は、登録が有効になるまでの変更を見ていない可能性がある。
    /// 実行中かどうかでは区別できないので、登録が済んだら一度読み直す。この scan は
    /// 登録後に始まるので、必ず登録以降を見ている。
    ///
    /// 登録の完了を待って初回表示を止めない代償として、移動のたびに列挙が一度多く走る。
    /// 無制限には増えない（登録一回につき一回）。
    #[allow(dead_code)]
    pub fn watch_ready(&mut self, token: u64) -> Option<SessionEffect> {
        if token != self.watch_token || self.watch_state == WatchState::Watching {
            return None;
        }
        self.watch_state = WatchState::Watching;
        if !matches!(self.history.location(), Some(Location::Path(_))) {
            return None;
        }
        self.reset_folder_size_retry(None);
        if !self.is_visible() {
            self.accept_dirty(WatchHint::All);
            return None;
        }
        Some(self.begin_scan())
    }

    /// 登録に失敗したことを受け取る。
    ///
    /// 自動更新は無いが F5 は残る。
    #[allow(dead_code)]
    pub fn watch_failed(&mut self, token: u64, reason: String) {
        if token != self.watch_token {
            return;
        }
        self.watch_state = WatchState::Degraded(reason);
    }

    /// callback がエラーや overflow を報告した。
    ///
    /// 登録は生きているが、取りこぼした可能性がある。
    #[allow(dead_code)]
    pub fn watch_degraded(&mut self, token: u64, reason: String) {
        if token != self.watch_token {
            return;
        }
        self.watch_state = WatchState::Degraded(reason);
    }

    /// watch が dirty にした。
    ///
    /// `location_epoch` も照合する。token は「この subscription が生きているか」、
    /// epoch は「この hint は今見ている location の話か」を答える。
    ///
    /// hint は `dirty_seq` と `pending_dirty` だけを動かし、`status` は変えない。
    /// `Refreshing` への遷移は follow-up scan の `begin_scan` が行う。
    /// 実行中 scan の `freshness_epoch` も進めない。
    #[allow(dead_code)]
    pub fn watch_dirty(&mut self, token: u64, location_epoch: u64, hint: WatchHint) -> bool {
        if token != self.watch_token || location_epoch != self.location_epoch {
            return false;
        }
        // 上限は adapter 側が主たる境界。ここは二重の防御。
        if let WatchHint::Paths(paths) = &hint
            && paths.len() > WATCH_MAILBOX_PATHS
        {
            return self.accept_dirty(WatchHint::All);
        }
        self.accept_dirty(hint)
    }

    /// Operation terminal report からの detached change hint を受ける。
    ///
    /// operation は view の rows を直接変更しない。現在の path と完全一致する
    /// session だけを dirty にし、watch と同じ follow-up coalescing を通す。
    pub fn operation_dirty(&mut self, directory: &Path) -> bool {
        if self.history.location().and_then(Location::path) != Some(directory) {
            return false;
        }
        self.accept_dirty(WatchHint::All)
    }

    #[allow(dead_code)]
    fn accept_dirty(&mut self, _hint: WatchHint) -> bool {
        self.dirty_seq = self.dirty_seq.wrapping_add(1);
        self.invalidate_folder_sizes();
        self.pending_dirty = true;
        true
    }

    /// IPC は強制停止できないので、待機中・実行中の probe に cancellation を通知する。
    /// completion 側の ticket 判定も残し、race しても old result は表示へ戻さない。
    fn cancel_folder_size_request(&mut self) -> bool {
        if let Some(ticket) = self.pending_folder_sizes.take() {
            ticket.cancellation.store(true, Ordering::Release);
            crate::everything::invalidate_folder_size_cache_under(&ticket.listing.path);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn disable_folder_size_requests_for_test(&mut self) {
        self.folder_size_requests_disabled_for_test = true;
        self.cancel_folder_size_request();
        self.reset_folder_size_retry(None);
    }

    fn reset_folder_size_retry(&mut self, listing: Option<&ScanTicket>) {
        self.folder_size_retry.generation = self.folder_size_retry.generation.wrapping_add(1);
        self.folder_size_retry.listing = listing.cloned();
        self.folder_size_retry.immediate_retries = 0;
        self.folder_size_retry.delayed_scheduled = false;
        self.folder_size_retry.next_delay = FOLDER_SIZE_RETRY_INITIAL_DELAY;
        self.folder_size_retry.delayed_request_id = None;
        self.folder_size_retry.last_completed_request_id = None;
    }

    /// Completed stale probes retry immediately at first, then at a capped backoff rate.
    pub fn folder_size_retry_after_invalidation(
        &mut self,
        ticket: &FolderSizeTicket,
    ) -> Option<FolderSizeRetry> {
        let state = &mut self.folder_size_retry;
        if self.accepted_listing.as_ref() != Some(&ticket.listing)
            || state.listing.as_ref() != Some(&ticket.listing)
            || self.dirty_seq != ticket.dirty_at_start
            || self.pending_folder_sizes.is_some()
            || state.last_completed_request_id != Some(ticket.request_id)
        {
            return None;
        }
        state.last_completed_request_id = None;
        if state.immediate_retries < FOLDER_SIZE_IMMEDIATE_RETRIES {
            state.immediate_retries += 1;
            return Some(FolderSizeRetry::Immediate);
        }
        if state.delayed_scheduled {
            return None;
        }
        if state.delayed_request_id == Some(ticket.request_id) {
            state.next_delay = (state.next_delay * 2).min(FOLDER_SIZE_RETRY_MAX_DELAY);
        }
        state.delayed_scheduled = true;
        Some(FolderSizeRetry::Delayed {
            token: FolderSizeRetryToken {
                listing: ticket.listing.clone(),
                generation: state.generation,
            },
            delay: state.next_delay,
        })
    }

    /// Timer wake-up goes through the session before creating a normal provider request.
    pub fn folder_size_retry_request(
        &mut self,
        token: &FolderSizeRetryToken,
    ) -> Option<FolderSizeRequest> {
        let state = &mut self.folder_size_retry;
        if !state.delayed_scheduled
            || state.generation != token.generation
            || state.listing.as_ref() != Some(&token.listing)
            || self.accepted_listing.as_ref() != Some(&token.listing)
            || self.dirty_seq != token.listing.dirty_at_start
            || self.pending.is_some()
            || self.pending_folder_sizes.is_some()
        {
            return None;
        }
        state.delayed_scheduled = false;
        let request = self.request_folder_sizes();
        if let Some(request) = request.as_ref() {
            self.folder_size_retry.delayed_request_id = Some(request.ticket.request_id);
        } else {
            // No missing folder metadata (or no requestable probes) settles this listing.
            self.folder_size_retry.listing = None;
        }
        request
    }

    /// F5/watch を受けた瞬間にindex sizeを取り去る。次の listing が届くまで古い index 値を
    /// 「現在の folder サイズ」に見せないための表示上の invalidation でもある。
    fn invalidate_folder_sizes(&mut self) {
        self.reset_folder_size_retry(None);
        if !self.cancel_folder_size_request() {
            if let Some(path) = self
                .accepted_listing
                .as_ref()
                .map(|ticket| ticket.path.as_path())
                .or_else(|| self.history.location().and_then(Location::path))
            {
                crate::everything::invalidate_folder_size_cache_under(path);
            }
        }
        self.folder_size_cache_bypass = true;
        let Some(listing) = self.last_good.as_ref() else {
            return;
        };
        if !listing.iter().any(|entry| entry.folder_size.is_some()) {
            return;
        }
        let entries = listing
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.folder_size = None;
                entry
            })
            .collect();
        self.last_good = Some(std::sync::Arc::new(entries));
    }

    /// follow-up scan の借りがあれば、その要求を返す。
    ///
    /// dispatch 時に呼ぶ。事前に組み立てると、その間の dirty を古い
    /// `dirty_at_start` と比べて落としてしまう。
    pub fn take_follow_up(&mut self) -> Option<SessionEffect> {
        if !self.is_visible() || !self.pending_dirty || self.pending.is_some() {
            return None;
        }
        self.history.location()?;
        if self.last_good.is_some() {
            self.status = ListingStatus::Refreshing;
        }
        Some(self.begin_scan())
    }
}

pub(crate) struct ArchiveCommitDisplaced {
    _manifest: Option<crate::archive_manifest::ArchiveManifest>,
    _pending: Option<ArchiveTicket>,
    _status: ListingStatus,
}

/// UI transition から外し、background executor で破棄する archive 読込の所有物。
pub(crate) struct ArchiveCommitDisposal {
    _incoming_manifest: Option<crate::archive_manifest::ArchiveManifest>,
    _custody: Option<crate::archive_listing::ArchiveReadCustody>,
    _failure: Option<DirectoryError>,
    _replaced_failure: Option<DirectoryError>,
    _displaced: Option<ArchiveCommitDisplaced>,
}

/// commit 結果と、UI thread 外で破棄すべき値を一緒に返す。
pub(crate) struct ArchiveCommitOutcome {
    pub(crate) committed: bool,
    disposal: ArchiveCommitDisposal,
}

impl ArchiveCommitOutcome {
    pub(crate) fn into_parts(self) -> (bool, ArchiveCommitDisposal) {
        (self.committed, self.disposal)
    }
}

impl Drop for DirectorySession {
    fn drop(&mut self) {
        // tab close / view shutdown は navigation effect を通らない。WeakEntity の失敗を
        // 待つだけにせず、slot 待ち・次probeへ進む前の Everything request を止める。
        self.cancel_folder_size_request();
        if let Some(ticket) = self.pending_archive.take() {
            ticket.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder_size_cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        GUARD
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn location(name: &str) -> Location {
        Location::Path(path(name))
    }

    fn listing(names: &[&str]) -> Listing {
        Arc::new(
            names
                .iter()
                .map(|name| Entry {
                    is_hidden: false,
                    key: crate::entry_identity::EntryKey::path(path(name), EntryKind::File),
                    name: name.to_string(),
                    path: Some(path(name)),
                    kind: EntryKind::File,
                    link: None,
                    link_target: None,
                    source_identity: None,
                    size: Some(0),
                    folder_size: None,
                    modified: None,
                })
                .collect(),
        )
    }

    fn folder_listing(names: &[&str]) -> Listing {
        Arc::new(
            names
                .iter()
                .map(|name| Entry {
                    is_hidden: false,
                    key: crate::entry_identity::EntryKey::path(path(name), EntryKind::Folder),
                    name: name.to_string(),
                    path: Some(path(name)),
                    kind: EntryKind::Folder,
                    link: None,
                    link_target: None,
                    source_identity: Some(crate::directory::EntrySourceIdentity::test_folder()),
                    size: None,
                    folder_size: None,
                    modified: None,
                })
                .collect(),
        )
    }

    fn folder_size(name: &str, bytes: u64) -> FolderSizeResult {
        let path = path(name);
        FolderSizeResult {
            key: crate::entry_identity::EntryKey::path(path.clone(), EntryKind::Folder),
            path,
            fact: crate::directory::FolderSizeFact {
                bytes,
                provenance: crate::directory::FolderSizeProvenance::EverythingIndex,
            },
            revision: crate::everything::ProviderRevision::current(),
            cache_generation: crate::everything::current_folder_size_cache_generation(),
            freshness: crate::everything::FolderSizeFreshness::new(),
        }
    }

    fn accepted_folder_listing(session: &mut DirectorySession, name: &str) {
        let scan = navigate(session, name);
        assert!(session.commit(&scan, Ok(folder_listing(&[&format!("{name}\\folder")]))));
    }

    fn finish_stale_folder_size_request(session: &mut DirectorySession) -> FolderSizeTicket {
        let request = session.request_folder_sizes().expect("folder-size request");
        let ticket = request.ticket;
        assert!(!session.commit_folder_sizes(&ticket, Vec::new()));
        assert!(session.pending_folder_sizes.is_none());
        assert!(!session.commit_folder_sizes(&ticket, Vec::new()));
        ticket
    }

    fn delayed_retry(retry: FolderSizeRetry) -> (FolderSizeRetryToken, std::time::Duration) {
        match retry {
            FolderSizeRetry::Delayed { token, delay } => (token, delay),
            FolderSizeRetry::Immediate => panic!("expected delayed retry"),
        }
    }

    #[test]
    fn folder_size_retries_coalesce_and_use_capped_backoff() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        accepted_folder_listing(&mut session, "A");

        let first = finish_stale_folder_size_request(&mut session);
        assert_eq!(
            session.folder_size_retry_after_invalidation(&first),
            Some(FolderSizeRetry::Immediate)
        );
        assert_eq!(session.folder_size_retry_after_invalidation(&first), None);
        assert_eq!(session.request_folder_sizes().unwrap().probes.len(), 1);
        assert!(session.request_folder_sizes().is_none());
        // Finish the retry opened above; request ids are consumed exactly once.
        let second_request = session.pending_folder_sizes.clone().unwrap();
        assert!(!session.commit_folder_sizes(&second_request, Vec::new()));
        assert!(session.pending_folder_sizes.is_none());
        assert_eq!(
            session.folder_size_retry_after_invalidation(&second_request),
            Some(FolderSizeRetry::Immediate)
        );
        assert_eq!(
            session.folder_size_retry_after_invalidation(&second_request),
            None
        );

        let delays = [1, 2, 4, 8, 16, 30, 30];
        let third = finish_stale_folder_size_request(&mut session);
        let (mut token, delay) = delayed_retry(
            session
                .folder_size_retry_after_invalidation(&third)
                .unwrap(),
        );
        assert_eq!(delay, std::time::Duration::from_secs(delays[0]));
        assert!(
            session
                .folder_size_retry_after_invalidation(&third)
                .is_none()
        );

        for expected in delays.iter().skip(1) {
            let request = session
                .folder_size_retry_request(&token)
                .expect("timer creates retry request");
            assert!(session.folder_size_retry_request(&token).is_none());
            assert!(!session.commit_folder_sizes(&request.ticket, Vec::new()));
            assert!(session.pending_folder_sizes.is_none());
            assert!(!session.commit_folder_sizes(&request.ticket, Vec::new()));
            let (next_token, actual_delay) = delayed_retry(
                session
                    .folder_size_retry_after_invalidation(&request.ticket)
                    .expect("next delayed retry"),
            );
            assert_eq!(actual_delay, std::time::Duration::from_secs(*expected));
            assert!(
                session
                    .folder_size_retry_after_invalidation(&request.ticket)
                    .is_none()
            );
            token = next_token;
        }
    }

    #[test]
    fn provider_revision_change_at_commit_rejects_facts_and_coalesces_retry() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        accepted_folder_listing(&mut session, "revision-race");
        let request = session
            .request_folder_sizes()
            .expect("accepted listing requests folder metadata");
        let old_revision = request.ticket.provider_revision;

        // detached adapterの結果ができた後、DirectorySessionが受け取る前のraceを作る。
        crate::everything::invalidate(old_revision);
        assert!(!session.commit_folder_sizes(
            &request.ticket,
            vec![folder_size("revision-race\\folder", 42)],
        ));
        assert_eq!(session.listing().unwrap()[0].folder_size, None);
        assert_eq!(
            session.folder_size_retry_after_invalidation(&request.ticket),
            Some(FolderSizeRetry::Immediate)
        );
        assert_eq!(
            session.folder_size_retry_after_invalidation(&request.ticket),
            None
        );

        let retry = session
            .request_folder_sizes()
            .expect("current listing may retry at the new provider revision");
        assert_ne!(retry.ticket.provider_revision, old_revision);
    }

    #[test]
    fn stale_retry_timer_and_result_are_rejected_after_invalidations() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        for invalidation in ["refresh", "watch", "navigation"] {
            let mut session = DirectorySession::default();
            accepted_folder_listing(&mut session, "A");
            let first = finish_stale_folder_size_request(&mut session);
            assert_eq!(
                session.folder_size_retry_after_invalidation(&first),
                Some(FolderSizeRetry::Immediate)
            );
            let second = finish_stale_folder_size_request(&mut session);
            assert_eq!(
                session.folder_size_retry_after_invalidation(&second),
                Some(FolderSizeRetry::Immediate)
            );
            let third = finish_stale_folder_size_request(&mut session);
            let (old_token, _) = delayed_retry(
                session
                    .folder_size_retry_after_invalidation(&third)
                    .unwrap(),
            );
            let old_request = session.request_folder_sizes().unwrap();
            match invalidation {
                "refresh" => {
                    assert!(session.refresh().is_some());
                    let refreshed = session.pending().unwrap().clone();
                    assert!(session.commit(&refreshed, Ok(folder_listing(&["A\\folder"]))));
                }
                "watch" => {
                    let epoch = session.location_epoch();
                    assert!(session.watch_dirty(session.watch_token(), epoch, WatchHint::All));
                    let followup = ticket_of(session.take_follow_up());
                    assert!(session.commit(&followup, Ok(folder_listing(&["A\\folder"]))));
                }
                "navigation" => {
                    accepted_folder_listing(&mut session, "B");
                    accepted_folder_listing(&mut session, "A");
                }
                _ => unreachable!(),
            }

            assert!(session.folder_size_retry_request(&old_token).is_none());
            assert!(!session.commit_folder_sizes(&old_request.ticket, Vec::new()));
            let replacement = session.request_folder_sizes().unwrap();
            assert_eq!(
                session.pending_folder_sizes.as_ref(),
                Some(&replacement.ticket)
            );
            assert!(!session.commit_folder_sizes(&old_request.ticket, Vec::new()));
            assert_eq!(
                session.pending_folder_sizes.as_ref(),
                Some(&replacement.ticket)
            );
            assert!(!session.commit_folder_sizes(&replacement.ticket, Vec::new()));
            assert!(session.pending_folder_sizes.is_none());
        }
    }

    use crate::directory::{Entry, EntryKind};
    use std::sync::Arc;

    fn ticket_of(effect: Option<SessionEffect>) -> ScanTicket {
        match effect {
            Some(SessionEffect::Enumerate(ticket)) => ticket,
            Some(other) => panic!("expected an enumerate effect, got {other:?}"),
            None => panic!("expected an enumerate effect"),
        }
    }

    #[test]
    fn folder_size_result_commits_only_to_its_accepted_listing() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let scan = navigate(&mut session, "A");
        assert!(session.commit(&scan, Ok(folder_listing(&["A\\folder"]))));
        let request = session
            .request_folder_sizes()
            .expect("folder metadata request");
        assert_eq!(request.probes.len(), 1);
        assert!(session.commit_folder_sizes(&request.ticket, vec![folder_size("A\\folder", 42)]));
        assert_eq!(
            session.listing().unwrap()[0]
                .folder_size
                .map(|fact| fact.bytes),
            Some(42)
        );
        // terminal ticket is consumed exactly once.
        assert!(!session.commit_folder_sizes(&request.ticket, vec![folder_size("A\\folder", 42)]));
    }

    #[test]
    fn late_folder_size_result_is_rejected_after_refresh_or_navigation() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let scan = navigate(&mut session, "A");
        assert!(session.commit(&scan, Ok(folder_listing(&["A\\folder"]))));
        let after_refresh = session.request_folder_sizes().unwrap();
        session.refresh();
        assert!(
            !session.commit_folder_sizes(&after_refresh.ticket, vec![folder_size("A\\folder", 1)])
        );

        let refreshed = session.pending().cloned().expect("refresh scan");
        assert!(session.commit(&refreshed, Ok(folder_listing(&["A\\folder"]))));
        let after_navigation = session.request_folder_sizes().unwrap();
        session.navigate(location("B"), None);
        assert!(
            !session
                .commit_folder_sizes(&after_navigation.ticket, vec![folder_size("A\\folder", 1)],)
        );
    }

    #[test]
    fn refresh_and_watch_clear_committed_folder_sizes_immediately() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let scan = navigate(&mut session, "A");
        assert!(session.commit(&scan, Ok(folder_listing(&["A\\folder"]))));
        let request = session.request_folder_sizes().unwrap();
        assert!(session.commit_folder_sizes(&request.ticket, vec![folder_size("A\\folder", 42)]));
        assert!(session.refresh().is_some());
        assert_eq!(session.listing().unwrap()[0].folder_size, None);

        let refreshed = session.pending().cloned().expect("refresh scan");
        assert!(session.commit(&refreshed, Ok(folder_listing(&["A\\folder"]))));
        let request = session.request_folder_sizes().unwrap();
        assert!(session.commit_folder_sizes(&request.ticket, vec![folder_size("A\\folder", 42)]));
        assert!(session.watch_dirty(
            session.watch_token(),
            session.location_epoch(),
            WatchHint::All
        ));
        assert_eq!(session.listing().unwrap()[0].folder_size, None);
    }

    #[test]
    fn dropping_a_session_cancels_its_pending_folder_size_request() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let scan = navigate(&mut session, "A");
        assert!(session.commit(&scan, Ok(folder_listing(&["A\\folder"]))));
        let request = session.request_folder_sizes().unwrap();
        let cancellation = request.ticket.cancellation();
        drop(session);
        assert!(cancellation.load(Ordering::Acquire));
    }

    #[test]
    fn dropping_a_session_cancels_its_pending_archive_read() {
        let mut session = DirectorySession::default();
        let effect = session.navigate(
            Location::Archive {
                source: path("A.zip"),
                components: Vec::new(),
            },
            None,
        );
        let ticket = effect
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::EnumerateArchive(ticket) => Some(ticket),
                _ => None,
            })
            .unwrap();
        let cancellation = ticket.cancellation();
        drop(session);
        assert!(cancellation.load(Ordering::Acquire));
    }

    #[test]
    fn watch_dirty_invalidates_folder_size_before_reconciliation() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let scan = navigate(&mut session, "A");
        assert!(session.commit(&scan, Ok(folder_listing(&["A\\folder"]))));
        let request = session.request_folder_sizes().unwrap();
        let watch = session.watch_token();
        let epoch = session.location_epoch();
        assert!(session.watch_dirty(watch, epoch, WatchHint::All));
        assert!(!session.commit_folder_sizes(&request.ticket, vec![folder_size("A\\folder", 1)]));
    }

    /// `navigate` が返す effect 列から列挙の ticket だけを取り出す。
    fn navigate(session: &mut DirectorySession, name: &str) -> ScanTicket {
        let effects = session.navigate(path(name), None);
        ticket_of(
            effects
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        )
    }

    /// 監視を登録済みにし、登録後に始まる catch-up scan の ticket を返す。
    ///
    /// `watch_ready` は必ず一度読み直す。以後の dirty はこの ticket に対して見る。
    fn watch(session: &mut DirectorySession) -> ScanTicket {
        let token = session.watch_token();
        ticket_of(session.watch_ready(token))
    }

    fn bookmark(name: &str) -> ViewBookmark {
        ViewBookmark {
            key: crate::entry_identity::EntryKey::path(path(name), EntryKind::File),
            offset: 8.0,
            selection: crate::selection::SelectionSnapshot::default(),
        }
    }

    #[test]
    fn navigation_starts_a_scan_and_clears_the_previous_listing() {
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        assert!(session.commit(&first, Ok(listing(&["a"]))));
        assert_eq!(session.status(), &ListingStatus::Ready);

        let second = navigate(&mut session, "B");
        // 旧 path の rows を新 path の下へ置かない。
        assert!(session.listing().is_none());
        assert_eq!(session.status(), &ListingStatus::InitialLoading);
        assert_ne!(first, second);
    }

    #[test]
    fn the_same_path_is_a_no_op() {
        let mut session = DirectorySession::default();
        session.navigate(path("A"), None);
        assert!(session.navigate(path("A"), None).is_empty());
    }

    #[test]
    fn a_stale_location_result_is_discarded() {
        // B-T01: A 読取中 → B → A → 最初の A 完了。
        let mut session = DirectorySession::default();
        let first_a = navigate(&mut session, "A");
        session.navigate(path("B"), None);
        let second_a = navigate(&mut session, "A");

        // 最初の A は同じ path だが location epoch が古い。
        assert!(!session.commit(&first_a, Ok(listing(&["stale"]))));
        assert!(session.listing().is_none());

        assert!(session.commit(&second_a, Ok(listing(&["fresh"]))));
        assert_eq!(session.listing().unwrap().len(), 1);
        assert_eq!(session.listing().unwrap()[0].name, "fresh");
    }

    #[test]
    fn a_scan_cannot_commit_after_entering_this_pc() {
        let mut session = DirectorySession::default();
        let a = navigate(&mut session, "A");
        let effects = session.navigate(Location::ThisPc, None);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SessionEffect::LoadVirtual(_)))
        );
        assert!(!session.commit(&a, Ok(listing(&["late-a"]))));
        assert_eq!(session.location(), Some(&Location::ThisPc));
    }

    #[test]
    fn leaving_and_reentering_a_rejects_the_first_a_scan() {
        let mut session = DirectorySession::default();
        let first_a = navigate(&mut session, "A");
        session.navigate(Location::ThisPc, None);
        let second_a = navigate(&mut session, "A");
        assert!(!session.commit(&first_a, Ok(listing(&["stale"]))));
        assert!(session.commit(&second_a, Ok(listing(&["fresh"]))));
    }

    #[test]
    fn archive_requests_are_single_use_across_a_b_a_refresh_and_back() {
        let mut session = DirectorySession::default();
        let a = Location::Archive {
            source: path("A.zip"),
            components: vec![],
        };
        let b = Location::Archive {
            source: path("B.zip"),
            components: vec![],
        };
        let first_a = archive_ticket(session.navigate(a.clone(), None));
        let _b = archive_ticket(session.navigate(b, None));
        let second_a = archive_ticket(session.navigate(a.clone(), None));
        assert!(!session.commit_archive(&first_a, Err(DirectoryError("late".into()))));
        assert!(first_a.cancellation().load(Ordering::Acquire));
        assert!(session.commit_archive(&second_a, Err(DirectoryError("current".into()))));
        assert!(!session.commit_archive(&second_a, Err(DirectoryError("duplicate".into()))));

        let refresh_one = archive_ticket(vec![session.refresh().unwrap()]);
        let refresh_two = archive_ticket(vec![session.refresh().unwrap()]);
        assert!(refresh_one.cancellation().load(Ordering::Acquire));
        assert!(!session.commit_archive(&refresh_one, Err(DirectoryError("late refresh".into()))));
        assert!(session.commit_archive(&refresh_two, Err(DirectoryError("refresh failed".into()))));

        session.navigate(
            Location::Archive {
                source: path("C.zip"),
                components: vec![],
            },
            None,
        );
        let back = session.back(None);
        assert!(
            back.iter()
                .any(|effect| matches!(effect, SessionEffect::EnumerateArchive(_)))
        );
    }

    fn archive_ticket(effects: Vec<SessionEffect>) -> ArchiveTicket {
        effects
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::EnumerateArchive(ticket) => Some(ticket),
                _ => None,
            })
            .expect("archive read effect")
    }

    #[test]
    fn this_pc_neither_subscribes_a_watch_nor_refreshes() {
        let mut session = DirectorySession::default();
        session.navigate(path("A"), None);
        let old_watch = session.watch_token();
        let effects = session.navigate(Location::ThisPc, None);
        assert!(effects.iter().any(|effect| matches!(effect, SessionEffect::UnsubscribeWatch { token } if *token == old_watch)));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, SessionEffect::SubscribeWatch { .. }))
        );
        assert!(session.watch_ready(old_watch).is_none());
        assert!(session.refresh().is_none());
    }

    #[test]
    fn shell_roots_use_a_virtual_ticket_without_watch_or_refresh() {
        let mut session = DirectorySession::default();
        let root = Location::ShellRoot(crate::location::ShellRoot::Network);
        let effects = session.navigate(root.clone(), None);
        let ticket = effects.into_iter().find_map(|effect| match effect {
            SessionEffect::LoadVirtual(ticket) => Some(ticket),
            _ => None,
        });
        let ticket = ticket.expect("Shell root requests a virtual load");
        assert_eq!(ticket.location, root);
        assert!(session.refresh().is_none());
        assert!(session.commit_virtual(&ticket));
        assert_eq!(session.status(), &ListingStatus::Ready);
    }

    #[test]
    fn a_late_shell_result_is_rejected_after_navigation() {
        let mut session = DirectorySession::default();
        let ticket = session
            .navigate(
                Location::ShellRoot(crate::location::ShellRoot::RecycleBin),
                None,
            )
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::LoadVirtual(ticket) => Some(ticket),
                _ => None,
            })
            .expect("Recycle Bin requests a virtual load");
        navigate(&mut session, "A");
        assert!(!session.commit_virtual(&ticket));
    }

    #[test]
    fn a_terminal_shell_completion_is_consumed_once() {
        let mut session = DirectorySession::default();
        let ticket = session
            .navigate(
                Location::ShellRoot(crate::location::ShellRoot::Network),
                None,
            )
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::LoadVirtual(ticket) => Some(ticket),
                _ => None,
            })
            .expect("Network requests a virtual load");
        assert!(session.complete_virtual(&ticket));
        assert!(!session.complete_virtual(&ticket));
        assert!(session.refresh().is_none());
    }

    #[test]
    fn refresh_keeps_the_last_good_listing() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        session.commit(&first, Ok(listing(&["a"])));

        let refresh = ticket_of(session.refresh());
        assert_eq!(session.status(), &ListingStatus::Refreshing);
        // 読み直し中も前の内容を表示する。
        assert_eq!(session.listing().unwrap().len(), 1);

        session.commit(&refresh, Ok(listing(&["a", "b"])));
        assert_eq!(session.status(), &ListingStatus::Ready);
        assert_eq!(session.listing().unwrap().len(), 2);
    }

    #[test]
    fn a_stale_refresh_result_is_discarded() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T02: scan → F5×2 → 古い結果を捨てる。
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        session.commit(&first, Ok(listing(&["a"])));

        let old_refresh = ticket_of(session.refresh());
        let new_refresh = ticket_of(session.refresh());
        assert!(!session.commit(&old_refresh, Ok(listing(&["stale"]))));
        assert!(session.commit(&new_refresh, Ok(listing(&["fresh"]))));
        assert_eq!(session.listing().unwrap()[0].name, "fresh");
    }

    #[test]
    fn a_failed_first_scan_is_unavailable_not_empty() {
        // B-T10: 初回失敗は空一覧にしない。
        let mut session = DirectorySession::default();
        let ticket = navigate(&mut session, "A");
        session.commit(&ticket, Err(DirectoryError("access denied".into())));

        assert!(matches!(session.status(), ListingStatus::Unavailable(_)));
        assert!(session.listing().is_none());
        assert_eq!(
            session.status().error().map(|error| error.0.as_str()),
            Some("access denied")
        );
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_listing() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T10: last-good + error。
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        session.commit(&first, Ok(listing(&["a"])));

        let refresh = ticket_of(session.refresh());
        session.commit(&refresh, Err(DirectoryError("gone".into())));

        assert!(matches!(session.status(), ListingStatus::RefreshFailed(_)));
        assert_eq!(session.listing().unwrap().len(), 1);
    }

    #[test]
    fn a_result_without_a_pending_scan_is_discarded() {
        let mut session = DirectorySession::default();
        let ticket = navigate(&mut session, "A");
        session.commit(&ticket, Ok(listing(&["a"])));
        // 同じ ticket をもう一度届けても、pending は既に無い。
        assert!(!session.commit(&ticket, Ok(listing(&["again"]))));
        assert_eq!(session.listing().unwrap()[0].name, "a");
    }

    #[test]
    fn the_startup_effects_are_taken_once() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));
        let effects = session.take_startup_effects();
        // 監視の登録と列挙が一度ずつ出る。
        assert!(matches!(
            effects.first(),
            Some(SessionEffect::SubscribeWatch { .. })
        ));
        let ticket = ticket_of(
            effects
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        );
        // 二度目は無い。
        assert!(session.take_startup_effects().is_empty());
        session.commit(&ticket, Ok(listing(&["a"])));
        assert!(session.take_startup_effects().is_empty());
    }

    #[test]
    fn failed_initial_startup_waits_for_explicit_refresh() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));
        let startup = session.take_startup_effects();
        let initial = ticket_of(
            startup
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        );

        assert!(session.commit(
            &initial,
            Err(DirectoryError("directory unavailable".into()))
        ));
        assert!(matches!(session.status(), ListingStatus::Unavailable(_)));
        assert!(session.take_startup_effects().is_empty());

        let refresh = ticket_of(session.refresh());
        assert!(session.commit(&refresh, Ok(listing(&["A", "A\\file"]))));
        assert!(matches!(session.status(), ListingStatus::Ready));
        assert_eq!(session.listing().unwrap().len(), 2);
    }

    #[test]
    fn navigation_during_initial_scan_rejects_old_ticket_and_commits_current_scan() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));

        let initial = ticket_of(
            session
                .take_startup_effects()
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        );
        let effects = session.navigate(path("B"), None);
        let scan = ticket_of(
            effects
                .iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_)))
                .cloned(),
        );
        assert_eq!(scan.path, path("B"));
        assert_eq!(session.location(), Some(&location("B")));
        assert!(session.commit(&scan, Ok(listing(&["b-child"]))));
        assert!(!session.commit(&initial, Err(DirectoryError("stale".into()))));
        assert!(session.take_startup_effects().is_empty());
    }

    #[test]
    fn hidden_startup_subscribes_once_and_scans_once_when_visible() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));
        assert!(session.set_visible(false).is_none());

        let effects = session.take_startup_effects();
        let token = effects
            .iter()
            .find_map(|effect| match effect {
                SessionEffect::SubscribeWatch { token, .. } => Some(*token),
                _ => None,
            })
            .expect("hidden startup still subscribes to the watch");
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, SessionEffect::SubscribeWatch { .. }))
                .count(),
            1
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, SessionEffect::Enumerate(_)))
        );
        assert!(session.watch_ready(token).is_none());
        assert!(session.take_startup_effects().is_empty());

        let visible_scan = session.set_visible(true).expect("becoming visible scans");
        let ticket = ticket_of(Some(visible_scan));
        assert_eq!(ticket.dirty_at_start, session.dirty_seq);
        assert!(session.set_visible(true).is_none());
        assert!(session.take_startup_effects().is_empty());
    }

    #[test]
    fn initial_virtual_ticket_is_not_replaced_by_another_startup_poll() {
        let mut session = DirectorySession::default();
        session.start_at(Location::ThisPc);

        let effects = session.take_startup_effects();
        let ticket = effects
            .iter()
            .find_map(|effect| match effect {
                SessionEffect::LoadVirtual(ticket) => Some(ticket.clone()),
                _ => None,
            })
            .expect("virtual startup requests one ticket");
        assert!(session.accepts_virtual(&ticket));
        assert!(session.take_startup_effects().is_empty());
        assert!(session.accepts_virtual(&ticket));
        assert!(session.commit_virtual(&ticket));
    }

    #[test]
    fn empty_history_can_start_later_and_demo_adoption_consumes_startup() {
        let mut empty = DirectorySession::default();
        assert!(empty.take_startup_effects().is_empty());
        empty.start_at(path("A"));
        assert!(
            empty
                .take_startup_effects()
                .iter()
                .any(|effect| matches!(effect, SessionEffect::Enumerate(_)))
        );

        let mut demo = DirectorySession::default();
        demo.adopt_demo(listing(&["demo"]));
        assert!(demo.take_startup_effects().is_empty());
        assert_eq!(demo.status(), &ListingStatus::Ready);
    }

    #[test]
    fn explicit_refresh_before_startup_poll_does_not_reissue_after_failure() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));
        let refresh = ticket_of(session.refresh());
        assert!(session.commit(&refresh, Err(DirectoryError("gone".into()))));
        assert!(matches!(session.status(), ListingStatus::Unavailable(_)));
        assert!(session.take_startup_effects().is_empty());
    }

    #[test]
    fn start_at_rearms_a_failed_startup_once_for_the_new_location() {
        let mut session = DirectorySession::default();
        session.start_at(path("A"));
        let initial = ticket_of(
            session
                .take_startup_effects()
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        );
        assert!(session.commit(&initial, Err(DirectoryError("gone".into()))));
        assert!(session.take_startup_effects().is_empty());

        session.start_at(path("B"));
        let restarted = session.take_startup_effects();
        let ticket = ticket_of(
            restarted
                .into_iter()
                .find(|effect| matches!(effect, SessionEffect::Enumerate(_))),
        );
        assert_eq!(ticket.path, path("B"));
        assert!(session.take_startup_effects().is_empty());
    }

    #[test]
    fn a_demo_session_has_no_startup_effect() {
        let mut session = DirectorySession::default();
        session.adopt_demo(listing(&["demo"]));
        assert!(session.take_startup_effects().is_empty());
        assert_eq!(session.status(), &ListingStatus::Ready);
    }

    // --- C1: navigation history ---

    /// C-T01: A → B → C、Back、D へ移動。Forward で C へ戻らない。
    #[test]
    fn a_branch_after_back_drops_the_forward_entries() {
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        navigate(&mut session, "B");
        navigate(&mut session, "C");

        assert!(!session.back(None).is_empty());
        assert_eq!(session.location(), Some(&location("B")));
        navigate(&mut session, "D");

        assert_eq!(session.location(), Some(&location("D")));
        assert!(session.forward(None).is_empty());
        assert_eq!(session.history.len(), 3);
    }

    /// C-T02: Back を連打しても、古い scan の success / error は cursor を動かさない。
    #[test]
    fn repeated_back_ignores_old_completions() {
        let mut session = DirectorySession::default();
        let a = navigate(&mut session, "A");
        session.commit(&a, Ok(listing(&["a"])));
        let b = navigate(&mut session, "B");
        session.commit(&b, Ok(listing(&["b"])));
        let c = navigate(&mut session, "C");
        session.commit(&c, Ok(listing(&["c"])));

        session.back(None);
        assert!(!session.back(None).is_empty());
        assert_eq!(session.location(), Some(&location("A")));
        assert_eq!(session.history.cursor(), 0);

        // 移動前の結果が遅れて届いても、cursor も表示も動かない。
        assert!(!session.commit(&c, Ok(listing(&["late-c"]))));
        assert!(!session.commit(&b, Err(DirectoryError("late".into()))));
        assert_eq!(session.location(), Some(&location("A")));
        assert_eq!(session.history.cursor(), 0);

        // 端では何も起きない。
        assert!(session.back(None).is_empty());
        assert_eq!(session.location(), Some(&location("A")));
    }

    /// C-T03: F5 / sort / watch / 同 path navigate で履歴件数が変わらない。
    #[test]
    fn refresh_and_watch_hints_do_not_change_the_history() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        navigate(&mut session, "B");
        let (len, cursor) = (session.history.len(), session.history.cursor());

        session.refresh();
        let ticket = watch(&mut session);
        let epoch = session.location_epoch();
        session.watch_dirty(session.watch_token(), epoch, WatchHint::All);
        session.commit(&ticket, Ok(listing(&["b"])));
        let _ = session.take_follow_up();
        assert!(session.navigate(path("B"), None).is_empty());

        assert_eq!(session.history.len(), len);
        assert_eq!(session.history.cursor(), cursor);
    }

    /// 読取失敗でも cursor は戻さず、Back は使えるままにする。
    #[test]
    fn a_failed_read_keeps_the_cursor_so_back_still_works() {
        let mut session = DirectorySession::default();
        let a = navigate(&mut session, "A");
        session.commit(&a, Ok(listing(&["a"])));
        let b = navigate(&mut session, "B");
        session.commit(&b, Err(DirectoryError("gone".into())));

        assert_eq!(session.location(), Some(&location("B")));
        assert!(matches!(session.status(), ListingStatus::Unavailable(_)));
        assert!(!session.back(None).is_empty());
        assert_eq!(session.location(), Some(&location("A")));
    }

    /// Back / Forward で着地した entry の表示状態を view が読める。
    #[test]
    fn back_and_forward_expose_the_bookmark_of_the_entry_they_land_on() {
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        navigate(&mut session, "B");

        session.back(Some(bookmark("in-b")));
        assert_eq!(session.location(), Some(&location("A")));
        assert_eq!(session.current_bookmark(), None);

        session.forward(Some(bookmark("in-a")));
        assert_eq!(session.location(), Some(&location("B")));
        assert_eq!(session.current_bookmark(), Some(&bookmark("in-b")));
    }

    /// bookmark は選択とフォーカスも運ぶ。
    #[test]
    fn a_bookmark_carries_the_selection_snapshot() {
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        navigate(&mut session, "B");

        // 実物の snapshot を組み立てて履歴に入れる。
        let keys = [
            crate::entry_identity::EntryKey::path(path("a"), EntryKind::File),
            crate::entry_identity::EntryKey::path(path("b"), EntryKind::File),
        ];
        let mut selection = crate::selection::Selection::default();
        selection.apply(
            crate::selection::SelectionIntent::Pick {
                index: 1,
                toggle: false,
                range: false,
            },
            keys.len(),
        );
        let snapshot = selection.snapshot(&keys);
        session.back(Some(ViewBookmark {
            key: keys[0].clone(),
            offset: 4.0,
            selection: snapshot.clone(),
        }));
        session.forward(Some(bookmark("in-a")));

        assert_eq!(session.current_bookmark().unwrap().selection, snapshot);
    }

    // --- B2: watcher ---

    #[test]
    fn navigation_resubscribes_the_watch_with_a_new_token() {
        let mut session = DirectorySession::default();
        let first = session.navigate(path("A"), None);
        let first_token = session.watch_token();
        // 最初は購読解除の相手がいないので、購読だけが出る。
        assert!(matches!(
            first.first(),
            Some(SessionEffect::SubscribeWatch { token, path: watched, .. })
                if *token == first_token && watched == &path("A")
        ));

        // 登録が済んでから移動すると、旧 token の解除が先に出る。
        session.watch_ready(first_token);
        let second = session.navigate(path("B"), None);
        assert!(matches!(
            second.first(),
            Some(SessionEffect::UnsubscribeWatch { token }) if *token == first_token
        ));
        assert!(matches!(
            second.get(1),
            Some(SessionEffect::SubscribeWatch { token, path: watched, .. })
                if *token == session.watch_token() && watched == &path("B")
        ));
        assert_ne!(first_token, session.watch_token());
    }

    #[test]
    fn refresh_does_not_touch_the_watch_token() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let token = session.watch_token();
        session.refresh();
        assert_eq!(session.watch_token(), token);
    }

    #[test]
    fn a_stale_watch_token_is_rejected() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let stale = session.watch_token();
        let stale_epoch = session.location_epoch();
        navigate(&mut session, "B");

        assert!(!session.watch_dirty(stale, stale_epoch, WatchHint::All));
        assert!(session.watch_dirty(
            session.watch_token(),
            session.location_epoch(),
            WatchHint::All
        ));
    }

    #[test]
    fn a_watch_hint_for_an_old_location_is_rejected() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // token が生きていても、epoch が違えば今の view の話ではない。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let token = session.watch_token();
        let stale_epoch = session.location_epoch();
        navigate(&mut session, "B");

        assert!(!session.watch_dirty(token, stale_epoch, WatchHint::All));
        assert!(session.watch_dirty(
            session.watch_token(),
            session.location_epoch(),
            WatchHint::All
        ));
    }

    #[test]
    fn a_watch_hint_does_not_invalidate_a_running_scan() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T03: scan 中に dirty が来ても、実行中の scan は commit できる。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let ticket = watch(&mut session);
        let epoch = session.location_epoch();
        assert!(session.watch_dirty(session.watch_token(), epoch, WatchHint::All));

        // freshness epoch は進んでいないので、同じ ticket がまだ commit できる。
        assert!(session.commit(&ticket, Ok(listing(&["a"]))));
        assert_eq!(session.status(), &ListingStatus::Ready);
    }

    #[test]
    fn a_dirty_during_a_scan_owes_exactly_one_follow_up() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T03: 最新 dirty を消さず、次の scan が必ず走る。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let ticket = watch(&mut session);
        let epoch = session.location_epoch();
        session.watch_dirty(session.watch_token(), epoch, WatchHint::All);

        session.commit(&ticket, Ok(listing(&["a"])));
        // 読取中に来た変更はこの結果に含まれていない。
        let follow_up = ticket_of(session.take_follow_up());
        assert_ne!(follow_up.scan_id, ticket.scan_id);
        // 二度目は無い。
        assert!(session.take_follow_up().is_none());
    }

    #[test]
    fn a_dirty_arriving_after_the_commit_owes_another_follow_up() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T03: 古い scan が新しい dirty を clear してはいけない。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let ticket = watch(&mut session);
        let epoch = session.location_epoch();
        session.watch_dirty(session.watch_token(), epoch, WatchHint::All);
        session.commit(&ticket, Ok(listing(&["a"])));

        let follow_up = ticket_of(session.take_follow_up());
        // follow-up が始まる前に、さらに変更が来る。
        session.watch_dirty(session.watch_token(), epoch, WatchHint::All);
        session.commit(&follow_up, Ok(listing(&["a", "b"])));

        // follow-up が捕捉した dirty より新しいので、もう一度借りが残る。
        assert!(session.take_follow_up().is_some());
    }

    #[test]
    fn a_stale_scan_does_not_clear_newer_dirty() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // 捨てる結果が dirty 状態へ書き込まない。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let stale = watch(&mut session);
        let epoch = session.location_epoch();
        // F5 で freshness を進め、古い ticket を commit 不可にする。
        let current = ticket_of(session.refresh());
        session.watch_dirty(session.watch_token(), epoch, WatchHint::All);

        assert!(!session.commit(&stale, Ok(listing(&["stale"]))));
        // 捨てた結果は dirty の借りを消さない。
        session.commit(&current, Ok(listing(&["fresh"])));
        assert!(session.take_follow_up().is_some());
    }

    #[test]
    fn registration_starts_exactly_one_catch_up() {
        // B-T04: 登録より前に始まった scan は、登録が有効になるまでの変更を見ていない。
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        session.commit(&first, Ok(listing(&["a"])));

        let catch_up = watch(&mut session);
        assert_ne!(catch_up.scan_id, first.scan_id);
        // 同じ token の二度目は no-op。
        assert!(session.watch_ready(session.watch_token()).is_none());
    }

    #[test]
    fn a_reconnect_mints_a_new_token_and_asks_for_a_catch_up() {
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let ticket = watch(&mut session);
        session.commit(&ticket, Ok(listing(&["a"])));

        // 再接続は新しい token を発行する。
        let old_token = session.watch_token();
        let effects = session.resubscribe_watch();
        let token = session.watch_token();
        assert_ne!(old_token, token);
        assert!(matches!(
            effects.first(),
            Some(SessionEffect::UnsubscribeWatch { token: t }) if *t == old_token
        ));
        assert!(matches!(
            effects.get(1),
            Some(SessionEffect::SubscribeWatch { token: t, .. }) if *t == token
        ));
        // 新しい token の登録完了は catch-up を要求する。
        assert!(session.watch_ready(token).is_some());
    }

    #[test]
    fn a_failed_registration_is_degraded_and_keeps_f5_working() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        session.watch_failed(session.watch_token(), "access denied".into());

        assert_eq!(
            session.watch_state(),
            &WatchState::Degraded("access denied".into())
        );
        // F5 は残る。
        assert!(session.refresh().is_some());
    }

    #[test]
    fn a_stale_watch_failure_is_ignored() {
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let stale = session.watch_token();
        navigate(&mut session, "B");

        session.watch_failed(stale, "old".into());
        assert_eq!(session.watch_state(), &WatchState::NotWatched);
    }

    #[test]
    fn an_oversized_hint_collapses_to_all() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T05: mailbox 上限を超えたら path を捨てて all へ畳む。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        let ticket = watch(&mut session);
        let epoch = session.location_epoch();
        let paths = (0..WATCH_MAILBOX_PATHS + 1)
            .map(|index| path(&format!("f{index}")))
            .collect();

        assert!(session.watch_dirty(session.watch_token(), epoch, WatchHint::Paths(paths)));
        // 畳んでも dirty の借りは立つ。
        session.commit(&ticket, Ok(listing(&["a"])));
        assert!(session.take_follow_up().is_some());
    }

    #[test]
    fn a_degraded_watch_is_distinguishable_from_no_changes() {
        // B-T05: degraded を明示できること。
        let mut session = DirectorySession::default();
        navigate(&mut session, "A");
        assert_eq!(session.watch_state(), &WatchState::NotWatched);
        let _ = watch(&mut session);
        assert_eq!(session.watch_state(), &WatchState::Watching);
        session.watch_degraded(session.watch_token(), "overflow".into());
        assert_eq!(
            session.watch_state(),
            &WatchState::Degraded("overflow".into())
        );
    }

    #[test]
    fn a_closed_owner_ignores_late_events() {
        let _cache_generation_guard = folder_size_cache_test_guard();
        // B-T06: 同じ folder を開く2タブのうち片方を閉じても、残った owner は
        // 自分の hint を受け取り続ける。閉じた owner の session はもう存在しない。
        let mut surviving = DirectorySession::default();
        navigate(&mut surviving, "A");
        let surviving_token = surviving.watch_token();

        // 閉じた owner が使っていた token は、残った owner のものではない。
        // 別の view の token は独立に採番されるので、ここでは進んだ値で代表させる。
        let closed_token = surviving_token.wrapping_add(1);
        assert!(!surviving.watch_dirty(closed_token, surviving.location_epoch(), WatchHint::All));
        // 残った owner は自分の hint を受け取る。
        assert!(surviving.watch_dirty(surviving_token, surviving.location_epoch(), WatchHint::All));
    }

    #[test]
    fn operation_hint_coalesces_with_an_in_flight_scan_for_its_current_path() {
        let mut session = DirectorySession::default();
        let ticket = navigate(&mut session, "A");
        assert!(session.operation_dirty(Path::new("A")));
        assert!(!session.operation_dirty(Path::new("B")));
        session.commit(&ticket, Ok(listing(&["a"])));
        assert!(session.take_follow_up().is_some());
        assert!(session.take_follow_up().is_none());
    }

    #[test]
    fn hidden_view_coalesces_hints_until_reactivated() {
        let mut session = DirectorySession::default();
        let initial = navigate(&mut session, "A");
        assert!(session.commit(&initial, Ok(listing(&["last-good"]))));
        assert_eq!(session.listing().unwrap().len(), 1);

        assert!(session.set_visible(false).is_none());
        assert!(session.operation_dirty(Path::new("A")));
        assert!(session.watch_dirty(
            session.watch_token(),
            session.location_epoch(),
            WatchHint::All
        ));
        assert!(session.take_follow_up().is_none());
        assert_eq!(session.listing().unwrap()[0].name, "last-good");

        let catch_up = ticket_of(session.set_visible(true));
        assert!(session.take_follow_up().is_none());
        assert!(session.set_visible(true).is_none());
        assert!(session.commit(&catch_up, Ok(listing(&["current"]))));
        assert_eq!(session.listing().unwrap()[0].name, "current");
    }

    #[test]
    fn hidden_in_flight_scan_defers_its_single_follow_up() {
        let mut session = DirectorySession::default();
        let first = navigate(&mut session, "A");
        assert!(session.set_visible(false).is_none());
        assert!(session.watch_dirty(
            session.watch_token(),
            session.location_epoch(),
            WatchHint::All
        ));
        assert!(session.commit(&first, Ok(listing(&["first-result"]))));
        assert!(session.take_follow_up().is_none());

        let catch_up = ticket_of(session.set_visible(true));
        assert_ne!(catch_up.scan_id, first.scan_id);
        assert!(session.take_follow_up().is_none());
    }
}
