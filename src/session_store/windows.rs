//! Windows での置換と、保存先のパス。
//!
//! `std::fs::rename` は `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` を呼ぶだけで、
//! `MOVEFILE_WRITE_THROUGH` を付けない。上書きはできても、crash durability を
//! 保証したことにはならない。ここでは内容を `sync_all` してから置換し、既存の
//! primary があるときは `ReplaceFileW` で直前の内容を backup に残す。
//!
//! **旧内容を先に消してから rename しない。** 失敗したら primary はそのまま残し、
//! 一時ファイルだけを片付ける。読み手に中途半端な JSON を見せないため、置換は
//! 同じ directory の一時ファイルから行う。

use super::MAX_SNAPSHOT_BYTES;
use std::path::{Path, PathBuf};

const REPLACE_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_millis(10),
    std::time::Duration::from_millis(20),
    std::time::Duration::from_millis(40),
];

/// 保存先の組。同じ directory に置き、別 volume をまたぐ置換をしない。
#[derive(Clone, Debug)]
pub(crate) struct SessionPaths {
    pub primary: PathBuf,
    pub backup: PathBuf,
}

impl SessionPaths {
    pub(crate) fn new(primary: PathBuf) -> Self {
        let name = primary
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session.json".to_string());
        let backup = primary.with_file_name(format!("{name}.bak"));
        Self { primary, backup }
    }

    /// `%LOCALAPPDATA%/gpui-filer/v6/session/session-v1.json`。
    ///
    /// v5 の設定ファイルを上書き・流用しない。
    pub(crate) fn default_for_app() -> Option<Self> {
        crate::app_paths::session_file().map(Self::new)
    }
}

/// 置換のどこで失敗したか。
///
/// stderr だけで成功扱いにしないための材料。呼び出し側が可視の診断に使う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    /// 一時ファイルの作成・書き込み・flush。
    Temporary(String),
    /// platform の置換。primary は変わっていない。
    Replace { code: Option<u32>, message: String },
    /// 一時ファイルを片付けられなかった。
    Cleanup(String),
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Temporary(reason) => write!(f, "temporary file: {reason}"),
            Self::Replace { code, message } => match code {
                Some(code) => write!(f, "replace failed ({code}): {message}"),
                None => write!(f, "replace failed: {message}"),
            },
            Self::Cleanup(reason) => write!(f, "cleanup: {reason}"),
        }
    }
}

/// 置換の手順。`persistence.rs` とテストから使う。
pub(crate) struct SessionFile {
    paths: SessionPaths,
    #[cfg(test)]
    // テスト対象の SessionFile だけに障害を届け、並列 writer へ漏らさない。
    partial_write_fault: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SessionFile {
    pub(crate) fn new(paths: SessionPaths) -> Self {
        Self {
            paths,
            #[cfg(test)]
            partial_write_fault: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_partial_write_fault(
        paths: SessionPaths,
        fault: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            paths,
            partial_write_fault: fault,
        }
    }

    #[allow(dead_code)] // 置換先をテストから直接確かめる。
    pub(crate) fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    /// 全内容を一時ファイルへ書いてから primary を置換する。
    pub(crate) fn replace(&self, bytes: &[u8]) -> Result<(), WriteFailure> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(WriteFailure::Temporary("snapshot is too large".into()));
        }
        let directory = self
            .paths
            .primary
            .parent()
            .ok_or_else(|| WriteFailure::Temporary("primary has no directory".into()))?;
        std::fs::create_dir_all(directory)
            .map_err(|error| WriteFailure::Temporary(format!("create {directory:?}: {error}")))?;

        let temporary = self.temporary_path();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| WriteFailure::Temporary(format!("create {temporary:?}: {error}")))?;
        #[cfg(test)]
        let write_result = write_and_flush(
            &mut file,
            &temporary,
            bytes,
            Some(&self.partial_write_fault),
        );
        #[cfg(not(test))]
        let write_result = write_and_flush(&mut file, &temporary, bytes, None);
        drop(file);
        if let Err(failure) = write_result {
            return cleanup_owned_temporary(&temporary, failure);
        }
        let mut retries = REPLACE_RETRY_DELAYS.iter();
        let publish_result = loop {
            match self.publish(&temporary) {
                Err(failure) if is_retryable_replace_failure(&failure) => {
                    if let Some(delay) = retries.next() {
                        // Windows の短時間な delete-share 競合だけをwriter thread上で再試行する。
                        std::thread::sleep(*delay);
                    } else {
                        break Err(failure);
                    }
                }
                result => break result,
            }
        };
        if let Err(failure) = publish_result {
            // 失敗した一時ファイルは残さない。primary には触れていない。
            // 片付けにも失敗したら、置換の理由と一緒に残す。黙って消さない。
            if let Err(cleanup) = std::fs::remove_file(&temporary) {
                return Err(WriteFailure::Cleanup(format!(
                    "{failure}; and {temporary:?} could not be removed: {cleanup}"
                )));
            }
            return Err(failure);
        }
        Ok(())
    }

    /// primary を読む。無ければ `None`。
    pub(crate) fn read_primary(&self) -> Result<Option<Vec<u8>>, String> {
        read_if_exists(&self.paths.primary)
    }

    /// backup を読む。無ければ `None`。
    pub(crate) fn read_backup(&self) -> Result<Option<Vec<u8>>, String> {
        read_if_exists(&self.paths.backup)
    }

    fn temporary_path(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(path) = TEST_TEMPORARY_PATH.with(|temporary| temporary.take()) {
            return path;
        }

        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = self
            .paths
            .primary
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session.json".to_string());
        // 同じ directory に置く。別 volume をまたぐと置換が atomic にならない。
        self.paths.primary.with_file_name(format!(
            "{name}.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn publish(&self, temporary: &Path) -> Result<(), WriteFailure> {
        if self.paths.primary.exists() {
            // 既存があるときは backup を残せる `ReplaceFileW`。
            match replace_file(&self.paths.primary, temporary, Some(&self.paths.backup)) {
                Err(failure) if failure.is_missing_target() => {
                    // 直前の確認と置換の間に消えた。初回と同じ扱いへ落とす。
                    move_file(temporary, &self.paths.primary)
                }
                result => result,
            }
        } else {
            // 初回は置換先が無いので `MoveFileExW`。
            move_file(temporary, &self.paths.primary)
        }
    }
}

fn is_retryable_replace_failure(failure: &WriteFailure) -> bool {
    let WriteFailure::Replace {
        code: Some(code), ..
    } = failure
    else {
        return false;
    };
    // HRESULT_FROM_WIN32 for sharing/lock violation and ERROR_UNABLE_TO_REMOVE_REPLACED.
    // 1176/1177 can leave namespace names in a different arrangement, so they stay fail-closed.
    matches!(*code, 0x8007_0020 | 0x8007_0021 | 0x8007_0497)
}

impl super::SessionSink for SessionFile {
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.replace(bytes).map_err(|failure| failure.to_string())
    }
}

impl WriteFailure {
    /// 置換先が無かった失敗か。初回作成と同じ経路へ落とす判断に使う。
    fn is_missing_target(&self) -> bool {
        matches!(self, Self::Replace { code: Some(code), .. } if *code == ERROR_FILE_NOT_FOUND)
    }
}

const ERROR_FILE_NOT_FOUND: u32 = 2;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestWriteFault {
    PartialWrite,
    Sync,
}

#[cfg(test)]
thread_local! {
    static TEST_WRITE_FAULT: std::cell::Cell<Option<TestWriteFault>> = const { std::cell::Cell::new(None) };
    static TEST_CLEANUP_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_TEMPORARY_PATH: std::cell::Cell<Option<PathBuf>> = const { std::cell::Cell::new(None) };
}

fn cleanup_owned_temporary(path: &Path, failure: WriteFailure) -> Result<(), WriteFailure> {
    #[cfg(test)]
    if TEST_CLEANUP_FAILURE.with(|fail| fail.replace(false)) {
        return Err(WriteFailure::Cleanup(format!(
            "{failure}; and {path:?} could not be removed: injected cleanup failure"
        )));
    }

    match std::fs::remove_file(path) {
        Ok(()) => Err(failure),
        Err(cleanup) => Err(WriteFailure::Cleanup(format!(
            "{failure}; and {path:?} could not be removed: {cleanup}"
        ))),
    }
}

fn write_and_flush(
    file: &mut std::fs::File,
    path: &Path,
    bytes: &[u8],
    _partial_write_fault: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), WriteFailure> {
    use std::io::Write as _;
    #[cfg(test)]
    let injected = TEST_WRITE_FAULT.with(|fault| fault.replace(None));

    #[cfg(test)]
    // replace が create_new を通過した後にだけ、このインスタンス固有の fault を消費する。
    if _partial_write_fault
        .is_some_and(|fault| fault.swap(false, std::sync::atomic::Ordering::AcqRel))
    {
        let prefix_len = bytes.len().min(2);
        file.write_all(&bytes[..prefix_len])
            .map_err(|error| WriteFailure::Temporary(format!("write {path:?}: {error}")))?;
        return Err(WriteFailure::Temporary(format!(
            "write {path:?}: injected partial write failure"
        )));
    }

    #[cfg(test)]
    if injected == Some(TestWriteFault::PartialWrite) {
        let prefix_len = bytes.len().min(2);
        file.write_all(&bytes[..prefix_len])
            .map_err(|error| WriteFailure::Temporary(format!("write {path:?}: {error}")))?;
        return Err(WriteFailure::Temporary(format!(
            "write {path:?}: injected partial write failure"
        )));
    }

    file.write_all(bytes)
        .map_err(|error| WriteFailure::Temporary(format!("write {path:?}: {error}")))?;
    // 内容を disk へ寄せてから置換する。これが無いと、置換だけが先に載って
    // 中身の怪しい primary が残り得る。
    #[cfg(test)]
    if injected == Some(TestWriteFault::Sync) {
        return Err(WriteFailure::Temporary(format!(
            "flush {path:?}: injected sync failure"
        )));
    }
    file.sync_all()
        .map_err(|error| WriteFailure::Temporary(format!("flush {path:?}: {error}")))?;
    Ok(())
}

fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {path:?}: {error}")),
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// 既存の primary を置換し、直前の内容を backup へ残す。
fn replace_file(
    primary: &Path,
    replacement: &Path,
    backup: Option<&Path>,
) -> Result<(), WriteFailure> {
    use windows::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};
    use windows::core::PCWSTR;

    let primary = wide(primary);
    let replacement = wide(replacement);
    let backup = backup.map(wide);
    let backup = backup
        .as_ref()
        .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()));
    let result = unsafe {
        ReplaceFileW(
            PCWSTR(primary.as_ptr()),
            PCWSTR(replacement.as_ptr()),
            backup,
            REPLACEFILE_WRITE_THROUGH,
            None,
            None,
        )
    };
    result.map_err(|error| WriteFailure::Replace {
        code: Some(error.code().0 as u32),
        message: error.message().to_string(),
    })
}

/// 初回作成と、置換先が消えていた場合の退避経路。
fn move_file(from: &Path, to: &Path) -> Result<(), WriteFailure> {
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let from = wide(from);
    let to = wide(to);
    let result = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    result.map_err(|error| WriteFailure::Replace {
        code: Some(error.code().0 as u32),
        message: error.message().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionSink;

    struct WriteFaultGuard;

    impl WriteFaultGuard {
        fn set(fault: TestWriteFault) -> Self {
            TEST_WRITE_FAULT.with(|current| {
                assert!(current.replace(Some(fault)).is_none());
            });
            Self
        }
    }

    impl Drop for WriteFaultGuard {
        fn drop(&mut self) {
            TEST_WRITE_FAULT.with(|current| current.set(None));
        }
    }

    struct CleanupFailureGuard;

    impl CleanupFailureGuard {
        fn set() -> Self {
            TEST_CLEANUP_FAILURE.with(|current| {
                assert!(!current.replace(true));
            });
            Self
        }
    }

    impl Drop for CleanupFailureGuard {
        fn drop(&mut self) {
            TEST_CLEANUP_FAILURE.with(|current| current.set(false));
        }
    }

    struct TemporaryPathGuard;

    impl TemporaryPathGuard {
        fn set(path: PathBuf) -> Self {
            TEST_TEMPORARY_PATH.with(|current| {
                assert!(current.replace(Some(path)).is_none());
            });
            Self
        }
    }

    impl Drop for TemporaryPathGuard {
        fn drop(&mut self) {
            TEST_TEMPORARY_PATH.with(|current| current.set(None));
        }
    }

    fn file_for(directory: &Path) -> SessionFile {
        SessionFile::new(SessionPaths::new(directory.join("session-v1.json")))
    }

    /// 一時ファイルが残っていないこと。失敗時も片付ける。
    fn temporary_files(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_first_write_creates_the_primary_without_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());

        file.replace(b"first").unwrap();

        assert_eq!(file.read_primary().unwrap(), Some(b"first".to_vec()));
        // 置換していないので backup は無い。
        assert_eq!(file.read_backup().unwrap(), None);
        assert!(temporary_files(directory.path()).is_empty());
    }

    #[test]
    fn replacing_keeps_the_previous_content_as_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());

        file.replace(b"first").unwrap();
        file.replace(b"second").unwrap();

        assert_eq!(file.read_primary().unwrap(), Some(b"second".to_vec()));
        // 直前の valid snapshot が backup に残る。
        assert_eq!(file.read_backup().unwrap(), Some(b"first".to_vec()));
        assert!(temporary_files(directory.path()).is_empty());
    }

    /// 読み手が排他で開いている間の置換は失敗し、primary は変わらない。
    #[test]
    fn a_sharing_violation_leaves_the_primary_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        file.replace(b"first").unwrap();

        let held = open_exclusive(&file.paths().primary);
        let failure = file.replace(b"second").unwrap_err();
        assert!(
            matches!(failure, WriteFailure::Replace { .. }),
            "{failure:?}"
        );
        // 失敗した一時ファイルは残さない。
        assert!(temporary_files(directory.path()).is_empty());

        // 排他を離してから中身を見る。置換に失敗したので前のまま。
        drop(held);
        assert_eq!(file.read_primary().unwrap(), Some(b"first".to_vec()));
    }

    #[test]
    fn a_preexisting_temporary_candidate_is_not_modified_or_removed() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        let temporary = directory.path().join("session-v1.json.preexisting.tmp");
        std::fs::write(&temporary, b"owned by someone else").unwrap();
        let _temporary_path = TemporaryPathGuard::set(temporary.clone());

        let failure = file.replace(b"new session").unwrap_err();

        assert!(matches!(failure, WriteFailure::Temporary(_)), "{failure:?}");
        assert_eq!(std::fs::read(&temporary).unwrap(), b"owned by someone else");
        assert_eq!(file.read_primary().unwrap(), None);
        assert_eq!(file.read_backup().unwrap(), None);
    }

    #[test]
    fn a_partial_write_failure_cleans_the_owned_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        file.replace(b"previous").unwrap();
        let _fault = WriteFaultGuard::set(TestWriteFault::PartialWrite);

        let failure = file.replace(b"new session").unwrap_err();

        assert!(
            matches!(&failure, WriteFailure::Temporary(message) if message.contains("injected partial write")),
            "{failure:?}"
        );
        assert!(temporary_files(directory.path()).is_empty());
        assert_eq!(file.read_primary().unwrap(), Some(b"previous".to_vec()));
        assert_eq!(file.read_backup().unwrap(), None);
    }

    #[test]
    fn partial_write_fault_is_scoped_to_its_session_file_instance() {
        let first_directory = tempfile::tempdir().unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let fault = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let first = SessionFile::new_with_partial_write_fault(
            SessionPaths::new(first_directory.path().join("session-v1.json")),
            fault.clone(),
        );
        let second = file_for(second_directory.path());

        let failure = first.replace(b"first session").unwrap_err();
        assert!(
            matches!(&failure, WriteFailure::Temporary(message) if message.contains("injected partial write")),
            "{failure:?}"
        );
        assert!(!fault.load(std::sync::atomic::Ordering::Acquire));
        second.replace(b"second session").unwrap();
        assert_eq!(
            second.read_primary().unwrap(),
            Some(b"second session".to_vec())
        );
        assert!(temporary_files(first_directory.path()).is_empty());
        assert!(temporary_files(second_directory.path()).is_empty());
    }

    #[test]
    fn a_sync_failure_cleans_the_owned_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        file.replace(b"previous").unwrap();
        let _fault = WriteFaultGuard::set(TestWriteFault::Sync);

        let failure = file.replace(b"new session").unwrap_err();

        assert!(
            matches!(&failure, WriteFailure::Temporary(message) if message.contains("injected sync failure")),
            "{failure:?}"
        );
        assert!(temporary_files(directory.path()).is_empty());
        assert_eq!(file.read_primary().unwrap(), Some(b"previous".to_vec()));
        assert_eq!(file.read_backup().unwrap(), None);
    }

    #[test]
    fn cleanup_failure_retains_the_temporary_and_reports_both_failures() {
        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        file.replace(b"previous").unwrap();
        let _write_fault = WriteFaultGuard::set(TestWriteFault::PartialWrite);
        let _cleanup_fault = CleanupFailureGuard::set();

        let failure = file.replace(b"new session").unwrap_err();

        assert!(
            matches!(&failure, WriteFailure::Cleanup(message) if message.contains("injected partial write") && message.contains("injected cleanup failure")),
            "{failure:?}"
        );
        let temporary = temporary_files(directory.path());
        assert_eq!(temporary.len(), 1);
        assert_eq!(std::fs::read(&temporary[0]).unwrap(), b"ne");
        assert_eq!(file.read_primary().unwrap(), Some(b"previous".to_vec()));
        assert_eq!(file.read_backup().unwrap(), None);
    }

    #[test]
    fn a_short_sharing_violation_is_retried_without_losing_the_backup() {
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let file = file_for(directory.path());
        file.replace(b"first").unwrap();

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let primary = file.paths().primary.clone();
        let lock = std::thread::spawn(move || {
            let _held = open_exclusive(&primary);
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            release_tx.send(()).unwrap();
        });
        file.replace(b"second").unwrap();
        releaser.join().unwrap();
        lock.join().unwrap();

        assert_eq!(file.read_primary().unwrap(), Some(b"second".to_vec()));
        assert_eq!(file.read_backup().unwrap(), Some(b"first".to_vec()));
        assert!(temporary_files(directory.path()).is_empty());
    }

    #[test]
    fn an_unwritable_location_is_reported_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        // 親が通常ファイルなので directory を作れない。
        let blocker = directory.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let file = SessionFile::new(SessionPaths::new(blocker.join("session-v1.json")));

        let failure = file.replace(b"data").unwrap_err();
        assert!(matches!(failure, WriteFailure::Temporary(_)), "{failure:?}");
        assert_eq!(file.read_primary().unwrap(), None);
    }

    #[test]
    fn the_sink_reports_the_failure_reason() {
        let directory = tempfile::tempdir().unwrap();
        let mut file = file_for(directory.path());
        file.write(b"ok").unwrap();
        assert_eq!(file.read_primary().unwrap(), Some(b"ok".to_vec()));
    }

    /// 置換先を排他で開く。`CreateFileW` の共有モードを 0 にする。
    struct ExclusivelyOpen(windows::Win32::Foundation::HANDLE);

    impl Drop for ExclusivelyOpen {
        fn drop(&mut self) {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    fn open_exclusive(path: &Path) -> ExclusivelyOpen {
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING,
        };
        // GENERIC_READ。共有モード 0 なので、他からの置換を拒否させる。
        const GENERIC_READ: u32 = 0x8000_0000;
        let wide = wide(path);
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                GENERIC_READ,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .expect("open the primary exclusively");
        ExclusivelyOpen(handle)
    }
}
