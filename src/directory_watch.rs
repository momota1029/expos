//! フォルダ変更の監視。`notify` を知っている唯一の場所。
//!
//! 役割は三つだけ。
//!
//! - view ごとに一つの `RecommendedWatcher` を持ち、登録・解除を行う
//! - callback を bounded な mailbox へ畳み、debounce してから session へ渡す
//! - 登録の成否と callback のエラーを session へ報告する
//!
//! filesystem の snapshot は持たない。dirty を伝えるだけで、再列挙は session が
//! 要求する。`notify` の callback は専用 thread で走るので、その中では stat も
//! canonicalize も再列挙もしない。
//!
//! **callback は GPUI に触れない。** `notify` の callback には `Send` が要るが、
//! GPUI の `WeakEntity` は `Send` ではない。callback は mailbox へ積むだけにして、
//! GPUI 側のタスクが timer で拾いに行く。
//!
//! 初版は view ごとに watcher を一つ持つ。共有 transport にすると `unwatch` が
//! 同じ path を開く別タブの登録まで消してしまう。

use crate::directory_session::{SessionEffect, WATCH_MAILBOX_PATHS, WatchHint};
use gpui::{App, Context, WeakEntity};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// trailing debounce。変更が続く間は待ち続ける。
const DEBOUNCE: Duration = Duration::from_millis(150);
/// 最初の dirty から数えた上限。変更が止まらなくてもここで一度読む。
const DEBOUNCE_CAP: Duration = Duration::from_millis(750);

/// callback から GPUI 側へ渡す mailbox。
///
/// **上限はここで適用する。** `notify` の callback は専用 thread で走るので、
/// debounce が発火するまで session へは届かない。session 側の上限は二重の防御で、
/// 主たる境界はこの型。
///
/// callback は `Send` な thread から触るので、`Mutex` と atomic だけで組む。
#[derive(Default)]
struct Mailbox {
    /// path を数えきれなくなった、または path が分からない event があった。
    all: AtomicBool,
    /// 畳んだ path の数。上限判定だけに使う。
    count: AtomicU64,
    /// callback がエラーを報告した。GPUI 側が拾って degraded にする。
    error: Mutex<Option<String>>,
    paths: Mutex<HashSet<PathBuf>>,
}

impl Mailbox {
    /// callback から呼ぶ。ここでは何も block しない。
    fn push(&self, paths: &[PathBuf], all: bool) {
        if all {
            self.all.store(true, Ordering::Relaxed);
            return;
        }
        if self.all.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut set) = self.paths.lock() else {
            // lock が壊れているなら、取りこぼすより全部読み直す方が安全。
            self.all.store(true, Ordering::Relaxed);
            return;
        };
        for path in paths {
            if set.len() >= WATCH_MAILBOX_PATHS {
                // 上限を超えたら path を捨てて all へ畳む。
                set.clear();
                self.count.store(0, Ordering::Relaxed);
                self.all.store(true, Ordering::Relaxed);
                return;
            }
            set.insert(path.clone());
        }
        self.count.store(set.len() as u64, Ordering::Relaxed);
    }

    /// callback のエラーを記録する。既に記録があれば上書きしない。
    fn push_error(&self, reason: String) {
        if let Ok(mut slot) = self.error.lock()
            && slot.is_none()
        {
            *slot = Some(reason);
        }
    }

    /// GPUI 側から呼ぶ。溜まった分を取り出して空にする。
    fn take(&self) -> Option<WatchHint> {
        if self.all.swap(false, Ordering::Relaxed) {
            if let Ok(mut set) = self.paths.lock() {
                set.clear();
            }
            self.count.store(0, Ordering::Relaxed);
            return Some(WatchHint::All);
        }
        let Ok(mut set) = self.paths.lock() else {
            return Some(WatchHint::All);
        };
        if set.is_empty() {
            return None;
        }
        let paths = set.drain().collect::<Vec<_>>();
        self.count.store(0, Ordering::Relaxed);
        Some(WatchHint::Paths(paths))
    }

    /// 記録されたエラーを取り出す。
    fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }

    fn is_empty(&self) -> bool {
        !self.all.load(Ordering::Relaxed) && self.count.load(Ordering::Relaxed) == 0
    }
}

/// adapter が session へ配送する先。
///
/// `DirectoryListDemo` が実装する。adapter は `WeakEntity` だけを持ち、
/// 閉じたタブの view を生かし続けない。
pub trait WatchOwner: Sized {
    fn watch_ready(&mut self, token: u64, cx: &mut Context<Self>);
    fn watch_failed(&mut self, token: u64, reason: String, cx: &mut Context<Self>);
    fn watch_degraded(&mut self, token: u64, reason: String, cx: &mut Context<Self>);
    /// hint を届ける。
    ///
    /// `token` と `location_epoch` は**購読したときに固定された値**を渡す。
    /// 今の値で貼り直すと、古い subscription の event が現在の view のものとして
    /// 通ってしまう。
    fn watch_dirty(
        &mut self,
        token: u64,
        location_epoch: u64,
        hint: WatchHint,
        cx: &mut Context<Self>,
    );
    /// debounce が明けた。溜まった hint を session へ渡す。
    fn watch_flush(&mut self, cx: &mut Context<Self>);
}

/// 一つの view の監視。
///
/// `T` は配送先の view 型。`WeakEntity` だけを保持するので、閉じたタブの
/// view を生かし続けない。
pub struct DirectoryWatch<T: WatchOwner + 'static> {
    watcher: Option<RecommendedWatcher>,
    /// 今登録している path。解除に使う。
    watched: Option<PathBuf>,
    /// 今登録している subscription の identity。
    ///
    /// 解除要求は token で照合する。effect は一つずつ別の task で実行されるので、
    /// 古い Unsubscribe が新しい Subscribe の後へ届き得る。
    watched_token: Option<u64>,
    /// 購読した時点の location の世代。hint の照合に使う。
    watched_epoch: u64,
    mailbox: Arc<Mailbox>,
    /// debounce 役が動いているか。二重に走らせない。
    debouncing: bool,
    view: WeakEntity<T>,
}

impl<T: WatchOwner + 'static> DirectoryWatch<T> {
    pub fn new(view: WeakEntity<T>) -> Self {
        Self {
            watcher: None,
            watched: None,
            watched_token: None,
            watched_epoch: 0,
            mailbox: Arc::new(Mailbox::default()),
            debouncing: false,
            view,
        }
    }

    /// session が返した監視の effect を実行する。
    ///
    /// `RecommendedWatcher::watch` は切断されたネットワーク path で短く block し得る。
    /// 呼び出し側は background executor へ渡すこと。
    pub fn apply(&mut self, effect: &SessionEffect, cx: &mut App) {
        match effect {
            SessionEffect::SubscribeWatch {
                token,
                location_epoch,
                path,
            } => {
                self.subscribe(*token, *location_epoch, path.clone(), cx);
            }
            SessionEffect::UnsubscribeWatch { token } => {
                self.unsubscribe(*token);
            }
            SessionEffect::Enumerate(_) => {}
            SessionEffect::EnumerateArchive(_) => {}
            SessionEffect::LoadVirtual(_) => {}
        }
    }

    fn subscribe(&mut self, token: u64, location_epoch: u64, path: PathBuf, cx: &mut App) {
        // 同じ view の前の登録は必ず外す。path が変わっていれば特に。
        self.unsubscribe(self.watched_token.unwrap_or_else(|| token.wrapping_sub(1)));
        // 購読ごとに mailbox を新しくする。前の subscription に届いた hint を
        // 新しい location のものとして読まないため。
        self.mailbox = Arc::new(Mailbox::default());
        // 古い debounce 役は自分の mailbox が空になった時点で終わる。
        self.debouncing = false;
        let mailbox = self.mailbox.clone();
        let watcher =
            notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
                match result {
                    Ok(event) => {
                        // callback 内では stat も canonicalize も再列挙もしない。
                        // 変更・rename の両端・未知 event は保守的に dirty にする。
                        let all = event.need_rescan() || event.paths.is_empty();
                        mailbox.push(&event.paths, all);
                    }
                    Err(error) => {
                        // callback error はログだけで消さず degraded に反映する。
                        mailbox.push_error(error.to_string());
                    }
                }
            });
        match watcher {
            Ok(mut watcher) => {
                if let Err(error) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                    let reason = error.to_string();
                    self.deliver(cx, move |owner, cx| owner.watch_failed(token, reason, cx));
                    return;
                }
                self.watcher = Some(watcher);
                self.watched = Some(path);
                self.watched_token = Some(token);
                self.watched_epoch = location_epoch;
                self.deliver(cx, move |owner, cx| owner.watch_ready(token, cx));
            }
            Err(error) => {
                let reason = error.to_string();
                self.deliver(cx, move |owner, cx| owner.watch_failed(token, reason, cx));
            }
        }
    }

    /// view へ配送する。
    ///
    /// `subscribe` は `view.update` の中から呼ばれることがある。そこで直接
    /// `view.update` すると二重更新で panic するので、配送は次のフレームへ回す。
    fn deliver<F>(&self, cx: &mut App, deliver: F)
    where
        F: FnOnce(&mut T, &mut Context<T>) + 'static,
    {
        let view = self.view.clone();
        cx.defer(move |cx| {
            let _ = view.update(cx, |owner, cx| deliver(owner, cx));
        });
    }

    /// 今の登録を外す。
    ///
    /// token が今のものでなければ何もしない。effect は一つずつ別の task で実行される
    /// ので、古い Unsubscribe が新しい Subscribe の後に届くことがある。
    fn unsubscribe(&mut self, token: u64) {
        if self.watched_token != Some(token) {
            return;
        }
        if let (Some(watcher), Some(path)) = (&mut self.watcher, self.watched.take()) {
            // 失敗しても登録は捨てる。古い path を握り続けない。
            let _ = watcher.unwatch(&path);
        }
        self.watcher = None;
        self.watched_token = None;
    }

    /// 監視をやめる。view を閉じるときに呼ぶ。
    pub fn shutdown(&mut self) {
        if let Some(token) = self.watched_token {
            self.unsubscribe(token);
        }
    }

    /// debounce のタイマーを回す。既に走っていれば何もしない。
    ///
    /// trailing 150ms、最初の dirty から最大 750ms。session を純粋に保つため、
    /// 時間の管理はここだけが持つ。
    ///
    /// callback は GPUI に触れないので、ここで timer を回して mailbox を拾いに行く。
    pub fn start_debounce(&mut self, cx: &mut App) {
        if self.debouncing {
            return;
        }
        self.debouncing = true;
        let mailbox = self.mailbox.clone();
        let view = self.view.clone();
        cx.spawn(async move |cx| {
            let mut waited = Duration::ZERO;
            loop {
                cx.background_executor().timer(DEBOUNCE).await;
                waited += DEBOUNCE;
                if mailbox.is_empty() || waited >= DEBOUNCE_CAP {
                    break;
                }
            }
            // ここは `view.update` の外なので、直接配送してよい。
            let _ = view.update(cx, |owner, cx| {
                owner.watch_flush(cx);
            });
        })
        .detach();
    }

    /// debounce が明けた。溜まった hint を session へ渡す。
    ///
    /// 渡したあとにタイマーを回し直す。callback は GPUI に触れないので、
    /// 次の dirty を拾うにはここで待ち続けるしかない。
    pub fn flush(&mut self, cx: &mut App) {
        self.debouncing = false;
        // 購読していなければ渡す先が無い。
        let Some(token) = self.watched_token else {
            return;
        };
        // 購読した時点の値で渡す。今の値で貼り直すと、古い subscription の
        // event が現在の location のものとして通ってしまう。
        let epoch = self.watched_epoch;
        if let Some(reason) = self.mailbox.take_error() {
            self.deliver(cx, move |owner, cx| owner.watch_degraded(token, reason, cx));
        }
        if let Some(hint) = self.mailbox.take() {
            self.deliver(cx, move |owner, cx| {
                owner.watch_dirty(token, epoch, hint, cx)
            });
        }
        // 監視が生きている間はタイマーを回し続ける。
        if self.watcher.is_some() {
            self.start_debounce(cx);
        }
    }
}
