//! Recycle-bin adapter with opaque restore receipts.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[cfg(not(windows))]
use std::collections::HashSet;

#[cfg(windows)]
mod windows;
#[derive(Clone)]
pub struct TrashReceipt {
    item: ::trash::TrashItem,
    #[cfg(all(test, windows))]
    restore_probe: std::sync::Arc<std::sync::Mutex<RestoreProbeState>>,
}

impl std::fmt::Debug for TrashReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep opaque receipt evidence stable when test-only callback probes exist.
        f.debug_struct("TrashReceipt")
            .field("item", &self.item)
            .finish()
    }
}

#[cfg(all(test, windows))]
#[derive(Default)]
struct RestoreProbeState {
    suppress_next_exact_success: bool,
    observations: Vec<RestoreCallbackObservation>,
    classifier_results: Vec<RestoreClassifierOutcome>,
    preflight_collisions: Vec<String>,
}

#[cfg(all(test, windows))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestoreClassifierOutcome {
    Completed,
    CompletedWithAnomaly,
    MutationUncertain,
    RestoreCollision,
    OtherError,
}

#[cfg(all(test, windows))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestoreCallbackObservation {
    pub source: Option<String>,
    pub parent: Option<String>,
    pub name: Option<String>,
    pub moved_path: Option<String>,
    pub result: i32,
    pub exact_success: bool,
    pub suppressed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreDisposition {
    Completed,
    CompletedWithAnomaly(String),
}

impl TrashReceipt {
    /// Undo UI が表示・再照合できる元の復元先。opaque recycle ID は adapter の外へ出さない。
    pub fn original_path(&self) -> PathBuf {
        self.item.original_path()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn suppress_next_exact_restore_callback_for_test(&self) {
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .suppress_next_exact_success = true;
    }

    #[cfg(all(test, windows))]
    pub(crate) fn restore_callback_observations_for_test(&self) -> Vec<RestoreCallbackObservation> {
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .observations
            .clone()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn restore_expected_source_for_test(&self) -> String {
        self.item.id.to_string_lossy().into_owned()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn restore_classifier_results_for_test(&self) -> Vec<RestoreClassifierOutcome> {
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .classifier_results
            .clone()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn restore_preflight_collisions_for_test(&self) -> Vec<String> {
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .preflight_collisions
            .clone()
    }

    #[cfg(all(test, windows))]
    pub(crate) fn observe_restore_callback_for_test(
        &self,
        mut observation: RestoreCallbackObservation,
    ) -> bool {
        let mut state = self.restore_probe.lock().expect("restore probe poisoned");
        observation.suppressed = observation.exact_success && state.suppress_next_exact_success;
        if observation.suppressed {
            state.suppress_next_exact_success = false;
        }
        state.observations.push(observation.clone());
        observation.suppressed
    }

    #[cfg(all(test, windows))]
    pub(crate) fn record_restore_classifier_result_for_test(
        &self,
        result: &Result<RestoreDisposition, TrashError>,
    ) {
        use RestoreClassifierOutcome as Outcome;
        let outcome = match result {
            Ok(RestoreDisposition::Completed) => Outcome::Completed,
            Ok(RestoreDisposition::CompletedWithAnomaly(_)) => Outcome::CompletedWithAnomaly,
            Err(TrashError::MutationUncertain(_)) => Outcome::MutationUncertain,
            Err(TrashError::RestoreCollision(_)) => Outcome::RestoreCollision,
            Err(_) => Outcome::OtherError,
        };
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .classifier_results
            .push(outcome);
    }

    #[cfg(all(test, windows))]
    fn record_restore_preflight_collision_for_test(&self, target: &Path) {
        self.restore_probe
            .lock()
            .expect("restore probe poisoned")
            .preflight_collisions
            .push(target.to_string_lossy().into_owned());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrashError {
    Platform(String),
    ReceiptMissing,
    MutationUncertain(String),
    RestoreCollision(PathBuf),
}

#[cfg(not(windows))]
pub fn move_to_trash(path: &Path) -> Result<TrashReceipt, TrashError> {
    let before: HashSet<OsString> = ::trash::os_limited::list()
        .map_err(platform)?
        .into_iter()
        .map(|item| item.id)
        .collect();
    let source = path.to_path_buf();
    ::trash::delete(&source).map_err(platform)?;
    let mut matches = ::trash::os_limited::list()
        .map_err(platform)?
        .into_iter()
        .filter(|item| !before.contains(&item.id) && item.original_path() == source);
    let item = matches.next().ok_or(TrashError::ReceiptMissing)?;
    if matches.next().is_some() {
        return Err(TrashError::ReceiptMissing);
    }
    Ok(TrashReceipt { item })
}

pub fn restore(receipt: TrashReceipt) -> Result<RestoreDisposition, TrashError> {
    let target = receipt.item.original_path();
    #[cfg(test)]
    let probe_receipt = receipt.clone();
    let result = restore_if_target_missing(&target, || {
        #[cfg(windows)]
        {
            windows::restore(receipt)
        }
        #[cfg(not(windows))]
        {
            ::trash::os_limited::restore_all([receipt.item])
                .map(|()| RestoreDisposition::Completed)
                .map_err(platform)
        }
    });
    // このテスト観測は、production preflight が返した exact collision のみ記録する。
    #[cfg(all(test, windows))]
    if let Err(TrashError::RestoreCollision(target)) = &result {
        probe_receipt.record_restore_preflight_collision_for_test(target);
    }
    result
}

/// Move the accepted filesystem object to Trash, carrying the admission identity to Shell's
/// final pre-delete callback on Windows.
pub fn move_to_trash_with_identity(
    path: &Path,
    accepted: &crate::directory::EntrySourceIdentity,
) -> Result<TrashReceipt, TrashError> {
    #[cfg(windows)]
    {
        windows::move_to_trash_with_identity(path, accepted)
    }
    #[cfg(not(windows))]
    {
        let _ = accepted;
        move_to_trash(path)
    }
}

pub(crate) fn admit_source(
    path: &Path,
    guard: &super::model::TrashSourceGuard,
) -> Result<Option<u32>, TrashError> {
    #[cfg(windows)]
    {
        windows::admit_source(path, guard)
    }
    #[cfg(not(windows))]
    {
        if guard.reparse_candidate {
            return Err(TrashError::Platform(
                "reparse Trash is unsupported".into(),
            ));
        }
        let current = crate::directory::EntrySourceIdentity::from_path_no_follow(
            path,
            guard.identity.kind,
        );
        if current.as_ref() == Some(&guard.identity) {
            Ok(None)
        } else {
            Err(TrashError::Platform(
                "Trash source identity changed or is unsupported".into(),
            ))
        }
    }
}

pub(crate) fn move_to_trash_with_guard(
    path: &Path,
    guard: &super::model::TrashSourceGuard,
    reparse_tag: Option<u32>,
) -> Result<TrashReceipt, TrashError> {
    #[cfg(windows)]
    {
        windows::move_to_trash_with_guard(path, guard, reparse_tag)
    }
    #[cfg(not(windows))]
    {
        if guard.reparse_candidate || reparse_tag.is_some() {
            return Err(TrashError::Platform(
                "reparse Trash is unsupported".into(),
            ));
        }
        move_to_trash_with_identity(path, &guard.identity)
    }
}

#[cfg(all(test, windows))]
pub(crate) fn restore_aborting_in_pre_move_for_test(
    receipt: TrashReceipt,
    pre_move_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<RestoreDisposition, TrashError> {
    let target = receipt.item.original_path();
    restore_if_target_missing(&target, || {
        windows::restore_aborting_in_pre_move_for_test(receipt, pre_move_called)
    })
}

#[cfg(all(test, windows))]
pub(crate) fn move_to_trash_aborting_in_pre_delete_for_test(
    path: &Path,
    pre_delete_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<TrashReceipt, TrashError> {
    windows::move_to_trash_aborting_in_pre_delete(path, pre_delete_called)
}

#[cfg(all(test, windows))]
pub(crate) fn restore_recovered_item_for_test(
    id: OsString,
    original_path: &Path,
) -> Result<(), TrashError> {
    restore(TrashReceipt::from_parts(id, original_path)?).map(|_| ())
}

impl TrashReceipt {
    #[cfg(windows)]
    fn from_parts(id: OsString, original_path: &Path) -> Result<Self, TrashError> {
        let name = original_path
            .file_name()
            .ok_or_else(|| TrashError::Platform("source has no file name".into()))?
            .to_os_string();
        let original_parent = original_path
            .parent()
            .ok_or_else(|| TrashError::Platform("source has no parent directory".into()))?
            .to_path_buf();
        Ok(Self {
            item: ::trash::TrashItem {
                id,
                name,
                original_parent,
                time_deleted: 0,
            },
            #[cfg(all(test, windows))]
            restore_probe: std::sync::Arc::default(),
        })
    }
}

#[cfg(windows)]
#[derive(Default)]
struct DeleteCallbacks {
    matching: usize,
    successful_ids: Vec<Result<OsString, String>>,
}

#[cfg(windows)]
impl DeleteCallbacks {
    fn record(&mut self, succeeded: bool, id: Option<Result<OsString, String>>) {
        self.matching += 1;
        if succeeded {
            self.successful_ids
                .push(id.unwrap_or_else(|| Err("callback returned no new item".into())));
        }
    }

    fn receipt_id(self) -> Result<OsString, TrashError> {
        if self.matching != 1 || self.successful_ids.len() != 1 {
            return Err(TrashError::MutationUncertain(
                "Shell did not provide exactly one matching successful delete callback".into(),
            ));
        }
        match self
            .successful_ids
            .into_iter()
            .next()
            .expect("one callback")
        {
            Ok(id) if !id.is_empty() => Ok(id),
            Ok(_) => Err(TrashError::MutationUncertain(
                "Shell returned an empty recycle item identifier".into(),
            )),
            Err(reason) => Err(TrashError::MutationUncertain(reason)),
        }
    }
}

fn restore_if_target_missing<T>(
    target: &Path,
    restore_receipt: impl FnOnce() -> Result<T, TrashError>,
) -> Result<T, TrashError> {
    // リンク先が無くても名前は占有されているため、exists() では空き判定しない。
    match std::fs::symlink_metadata(target) {
        Ok(_) => Err(TrashError::RestoreCollision(target.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => restore_receipt(),
        Err(error) => Err(platform(error)),
    }
}

fn platform(error: impl std::fmt::Display) -> TrashError {
    TrashError::Platform(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    #[cfg(windows)]
    struct RestoreTrashFixtureOnDrop {
        path: PathBuf,
        receipt: Option<TrashReceipt>,
        adapter_may_have_mutated: bool,
    }

    #[cfg(windows)]
    impl RestoreTrashFixtureOnDrop {
        fn new(path: PathBuf) -> Self {
            Self {
                path,
                receipt: None,
                adapter_may_have_mutated: false,
            }
        }

        fn record_receipt(&mut self, receipt: &TrashReceipt) {
            self.receipt = Some(receipt.clone());
        }

        fn arm_exact_path_recovery(&mut self) {
            self.adapter_may_have_mutated = true;
        }

        fn clear_receipt(&mut self) {
            self.receipt = None;
        }
    }

    #[cfg(windows)]
    impl Drop for RestoreTrashFixtureOnDrop {
        fn drop(&mut self) {
            match fs::symlink_metadata(&self.path) {
                Ok(_) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    report_trash_fixture_cleanup_error(format!(
                        "could not check fixture path {}: {error}",
                        self.path.display()
                    ));
                    return;
                }
            }

            let receipt = match self.receipt.take() {
                Some(receipt) => Some(receipt),
                None if self.adapter_may_have_mutated => {
                    let items = match ::trash::os_limited::list() {
                        Ok(items) => items,
                        Err(error) => {
                            report_trash_fixture_cleanup_error(format!(
                                "could not inspect recycle items for {}: {error}",
                                self.path.display()
                            ));
                            return;
                        }
                    };
                    let mut matches = items
                        .into_iter()
                        .filter(|item| item.original_path() == self.path);
                    let Some(item) = matches.next() else {
                        return;
                    };
                    if matches.next().is_some() {
                        report_trash_fixture_cleanup_error(format!(
                            "multiple recycle items match fixture path {}; leaving them untouched",
                            self.path.display()
                        ));
                        return;
                    }
                    Some(TrashReceipt {
                        item,
                        #[cfg(all(test, windows))]
                        restore_probe: std::sync::Arc::default(),
                    })
                }
                None => return,
            };

            if let Some(receipt) = receipt {
                if let Err(error) = super::restore(receipt) {
                    report_trash_fixture_cleanup_error(format!(
                        "could not restore fixture {}: {error:?}",
                        self.path.display()
                    ));
                }
            }
        }
    }

    #[cfg(windows)]
    fn report_trash_fixture_cleanup_error(message: String) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "trash fixture cleanup: {message}");
    }

    #[cfg(windows)]
    struct TrashLinkDiagnosticFixture {
        root: Option<tempfile::TempDir>,
        receipt: Option<TrashReceipt>,
        dispatched: bool,
        complete: bool,
    }

    #[cfg(windows)]
    impl TrashLinkDiagnosticFixture {
        fn new(root: tempfile::TempDir) -> Self {
            Self {
                root: Some(root),
                receipt: None,
                dispatched: false,
                complete: false,
            }
        }

        fn arm_before_dispatch(&mut self) {
            self.dispatched = true;
        }

        fn record_receipt(&mut self, receipt: TrashReceipt) {
            self.receipt = Some(receipt);
        }

        fn restore_exact_receipt(&mut self) -> Result<(), TrashError> {
            let receipt = self
                .receipt
                .take()
                .expect("Trash diagnostic has an exact Shell receipt");
            super::restore(receipt).map(|_| ())
        }

        fn complete(&mut self) {
            self.complete = true;
        }
    }

    #[cfg(windows)]
    impl Drop for TrashLinkDiagnosticFixture {
        fn drop(&mut self) {
            if self.complete || !self.dispatched {
                return;
            }

            if let Some(receipt) = self.receipt.take() {
                match super::restore(receipt) {
                    Ok(_) => eprintln!(
                        "Trash link diagnostic failure cleanup restored its exact Shell receipt"
                    ),
                    Err(error) => {
                        eprintln!("Trash link diagnostic exact receipt recovery failed: {error:?}")
                    }
                }
            }
            if let Some(root) = self.root.take() {
                let preserved = root.keep();
                eprintln!(
                    "Trash link diagnostic did not complete; preserving fixture root {}",
                    preserved.display()
                );
            }
        }
    }

    #[cfg(windows)]
    fn assert_local_fixed_ntfs(path: &Path) {
        use std::os::windows::ffi::OsStrExt as _;

        let volume_root = path.ancestors().last().expect("volume root");
        let wide: Vec<u16> = volume_root
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let drive_type = unsafe {
            windows_062::Win32::Storage::FileSystem::GetDriveTypeW(windows_062::core::PCWSTR(
                wide.as_ptr(),
            ))
        };
        let mut filesystem = [0u16; 32];
        let volume_info_ok = unsafe {
            windows_062::Win32::Storage::FileSystem::GetVolumeInformationW(
                windows_062::core::PCWSTR(wide.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
        }
        .is_ok();
        assert!(
            !path.to_string_lossy().starts_with(r"\\")
                && drive_type == 3
                && volume_info_ok
                && String::from_utf16_lossy(&filesystem)
                    .trim_end_matches('\0')
                    .eq_ignore_ascii_case("NTFS"),
            "Trash link diagnostic requires a local fixed NTFS volume: {}",
            path.display()
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; invoke explicitly on a disposable fixture"]
    fn windows_shell_adapter_trash_round_trip_restores_exact_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gpui-filer-trash-adapter-round-trip.txt");
        let expected = b"trash adapter round-trip fixture";
        fs::write(&path, expected).unwrap();
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &path,
            crate::directory::EntryKind::File,
        )
        .unwrap();
        let mut cleanup = RestoreTrashFixtureOnDrop::new(path.clone());

        cleanup.arm_exact_path_recovery();
        let receipt = match super::move_to_trash_with_identity(&path, &accepted) {
            Ok(receipt) => receipt,
            Err(error) => panic!("Shell trash adapter failed: {error:?}"),
        };
        cleanup.record_receipt(&receipt);

        assert_eq!(receipt.original_path(), path);
        assert!(
            matches!(
                fs::symlink_metadata(&path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "source should be absent while the fixture is in the Recycle Bin"
        );
        super::restore(receipt).expect("restore exact Shell adapter receipt");
        cleanup.clear_receipt();

        assert!(
            fs::symlink_metadata(&path).is_ok(),
            "original path restored"
        );
        assert_eq!(fs::read(&path).unwrap(), expected);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "uses the current Windows account Recycle Bin; invoke explicitly on a disposable fixture"]
    fn windows_shell_adapter_restore_pre_move_abort_retains_exact_receipt_for_retry() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("gpui-filer-restore-abort-")
            .suffix(".txt")
            .tempfile_in(temp.path())
            .unwrap();
        let (_file, path) = fixture.keep().unwrap();
        drop(_file);
        let expected = b"restore PreMoveItem abort retry fixture";
        fs::write(&path, expected).unwrap();
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &path,
            crate::directory::EntryKind::File,
        )
        .unwrap();
        let mut cleanup = RestoreTrashFixtureOnDrop::new(path.clone());
        cleanup.arm_exact_path_recovery();
        let receipt = super::move_to_trash_with_identity(&path, &accepted)
            .expect("recycle restore abort fixture");
        cleanup.record_receipt(&receipt);
        let pre_move_called = Arc::new(AtomicBool::new(false));

        let aborted =
            super::restore_aborting_in_pre_move_for_test(receipt.clone(), pre_move_called.clone());
        assert!(
            pre_move_called.load(Ordering::SeqCst),
            "Shell must reach PreMoveItem"
        );
        assert!(
            matches!(aborted, Err(TrashError::MutationUncertain(_))),
            "an aborted restore must remain uncertain: {aborted:?}"
        );
        assert!(
            matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "PreMoveItem abort must leave the destination absent"
        );

        let disposition = super::restore(receipt).expect("retry same exact receipt");
        assert!(matches!(disposition, RestoreDisposition::Completed));
        cleanup.clear_receipt();
        assert_eq!(fs::read(&path).unwrap(), expected);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; invoke explicitly on a disposable fixture"]
    fn windows_shell_adapter_pre_delete_abort_is_uncertain_and_keeps_exact_fixture() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("gpui-filer-trash-abort-")
            .suffix(".txt")
            .tempfile_in(temp.path())
            .unwrap();
        let (_file, path) = fixture.keep().unwrap();
        drop(_file);
        let expected = b"trash pre-delete abort fixture";
        fs::write(&path, expected).unwrap();
        let mut cleanup = RestoreTrashFixtureOnDrop::new(path.clone());
        cleanup.arm_exact_path_recovery();
        let pre_delete_called = Arc::new(AtomicBool::new(false));

        let result =
            windows::move_to_trash_aborting_in_pre_delete(&path, pre_delete_called.clone());

        assert!(
            pre_delete_called.load(Ordering::SeqCst),
            "Shell must reach PreDeleteItem"
        );
        assert!(
            matches!(result, Err(TrashError::MutationUncertain(_))),
            "an aborted Shell operation must not produce a success receipt: {result:?}"
        );
        assert!(
            fs::symlink_metadata(&path).is_ok(),
            "original fixture path remains"
        );
        assert_eq!(fs::read(&path).unwrap(), expected);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the Y: exFAT fixture with TEMP and TMP set to Y:\\TEMP"]
    fn windows_shell_adapter_refuses_volume_when_recycle_bin_query_fails() {
        use ::windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        use std::os::windows::io::AsRawHandle;

        fn file_identity(path: &Path) -> (u32, u32, u32) {
            let file = fs::File::open(path).unwrap();
            let handle = HANDLE(file.as_raw_handle() as _);
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe { GetFileInformationByHandle(handle, &mut info) }.unwrap();
            (
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
            )
        }

        let temp_root = std::env::temp_dir();
        assert!(
            temp_root.to_string_lossy().starts_with(r"Y:\TEMP"),
            "run with TEMP and TMP set to Y:\\TEMP, got {}",
            temp_root.display()
        );
        let temp = tempfile::tempdir_in(temp_root).unwrap();
        let path = temp.path().join("gpui-filer-trash-query-refusal.txt");
        let expected = b"preserve this exFAT fixture";
        fs::write(&path, expected).unwrap();
        let mut cleanup = RestoreTrashFixtureOnDrop::new(path.clone());
        cleanup.arm_exact_path_recovery();
        let identity_before = file_identity(&path);

        let query_error = match windows::query_source_recycle_bin(&path) {
            Err(error @ TrashError::Platform(_)) => error,
            result => panic!("resolved Y: query root should fail: {result:?}"),
        };
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &path,
            crate::directory::EntryKind::File,
        )
        .unwrap();
        let result = super::move_to_trash_with_identity(&path, &accepted);

        assert_eq!(result.unwrap_err(), query_error);
        assert_eq!(
            file_identity(&path),
            identity_before,
            "source identity is unchanged"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            expected,
            "source bytes are unchanged"
        );

        // Directory paths take a different route through volume-root resolution; refusal must
        // still happen before IFileOperation can mutate the directory tree.
        let folder = temp.path().join("gpui-filer-trash-query-refusal-folder");
        let child = folder.join("nested").join("preserve.txt");
        let child_expected = b"preserve this exFAT folder fixture";
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        fs::write(&child, child_expected).unwrap();
        let mut folder_cleanup = RestoreTrashFixtureOnDrop::new(folder.clone());
        folder_cleanup.arm_exact_path_recovery();
        let child_identity_before = file_identity(&child);

        let folder_query_error = match windows::query_source_recycle_bin(&folder) {
            Err(error @ TrashError::Platform(_)) => error,
            result => panic!("resolved Y: folder query root should fail: {result:?}"),
        };
        let folder_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &folder,
            crate::directory::EntryKind::Folder,
        )
        .unwrap();
        let folder_result = super::move_to_trash_with_identity(&folder, &folder_identity);

        assert_eq!(folder_result.unwrap_err(), folder_query_error);
        assert!(
            fs::symlink_metadata(&folder).is_ok(),
            "source directory is unchanged"
        );
        assert_eq!(
            file_identity(&child),
            child_identity_before,
            "nested file identity is unchanged"
        );
        assert_eq!(
            fs::read(&child).unwrap(),
            child_expected,
            "nested file bytes are unchanged"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; invoke explicitly on a disposable fixture"]
    fn windows_shell_adapter_folder_trash_round_trip_restores_exact_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gpui-filer-trash-folder-round-trip");
        let child = path.join("nested").join("fixture.txt");
        let child_expected = b"nested file fixture";
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        fs::write(&child, child_expected).unwrap();
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &path,
            crate::directory::EntryKind::Folder,
        )
        .unwrap();
        let mut cleanup = RestoreTrashFixtureOnDrop::new(path.clone());

        cleanup.arm_exact_path_recovery();
        let receipt = match super::move_to_trash_with_identity(&path, &accepted) {
            Ok(receipt) => receipt,
            Err(error) => panic!("Shell trash adapter failed: {error:?}"),
        };
        cleanup.record_receipt(&receipt);

        assert_eq!(receipt.original_path(), path);
        assert!(
            matches!(
                fs::symlink_metadata(&path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "source directory should be absent while the fixture is in the Recycle Bin"
        );
        super::restore(receipt).expect("restore exact Shell adapter receipt");
        cleanup.clear_receipt();

        assert!(
            fs::symlink_metadata(&path).is_ok(),
            "original directory restored"
        );
        assert_eq!(fs::read(&child).unwrap(), child_expected);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; explicitly diagnoses IFileOperation behavior for a junction fixture"]
    fn windows_shell_adapter_junction_trash_preserves_target_and_restores_link() {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert_local_fixed_ntfs(temp.path());
        let target = temp.path().join("target-directory");
        let link = temp.path().join("junction-link");
        fs::create_dir(&target).unwrap();
        let marker = target.join("marker.bin");
        let marker_bytes = b"junction target must remain unchanged";
        fs::write(&marker, marker_bytes).unwrap();

        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .expect("launch mklink for the disposable junction fixture");
        assert!(status.success(), "mklink /J must create fixture junction");

        let raw_target = fs::read_link(&link).expect("read junction target without following it");
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &link,
            crate::directory::EntryKind::Folder,
        )
        .expect("capture no-follow junction identity");
        let target_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &target,
            crate::directory::EntryKind::Folder,
        )
        .expect("capture target directory identity");
        let marker_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &marker,
            crate::directory::EntryKind::File,
        )
        .expect("capture target content identity");
        eprintln!(
            "junction diagnostic pre-trash: link_identity={accepted:?}, target_identity={target_identity:?}, marker_identity={marker_identity:?}, raw_target={raw_target:?}"
        );
        let mut fixture = TrashLinkDiagnosticFixture::new(temp);
        fixture.arm_before_dispatch();

        let guard = crate::operations::model::TrashSourceGuard {
            identity: accepted.clone(),
            reparse_candidate: true,
        };
        let admitted_tag = windows::admit_source(&link, &guard)
            .expect("production worker admission must accept this junction");
        let receipt =
            match windows::move_to_trash_with_guard(&link, &guard, admitted_tag) {
                Ok(receipt) => receipt,
                Err(error) => panic!("Shell returned no exact junction receipt: {error:?}"),
            };
        let receipt_origin = receipt.original_path();
        fixture.record_receipt(receipt);
        assert_eq!(receipt_origin, link);
        assert!(
            matches!(fs::symlink_metadata(&link), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "junction link object disappeared"
        );
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &target,
                crate::directory::EntryKind::Folder,
            ),
            Some(target_identity.clone()),
            "target directory identity is unchanged after Trash"
        );
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &marker,
                crate::directory::EntryKind::File,
            ),
            Some(marker_identity.clone()),
            "target marker identity is unchanged after Trash"
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_bytes);

        fixture
            .restore_exact_receipt()
            .expect("restore exact Shell receipt");
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &link,
                crate::directory::EntryKind::Folder,
            ),
            Some(accepted),
            "restored link has the same no-follow identity"
        );
        eprintln!(
            "junction diagnostic post-restore: link_identity={:?}, target_identity={target_identity:?}, marker_identity={marker_identity:?}, raw_target={:?}",
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &link,
                crate::directory::EntryKind::Folder,
            ),
            fs::read_link(&link).unwrap()
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            raw_target,
            "raw junction target is restored"
        );
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &target,
                crate::directory::EntryKind::Folder,
            ),
            Some(target_identity),
            "target identity remains unchanged after restore"
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_bytes);
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &marker,
                crate::directory::EntryKind::File,
            ),
            Some(marker_identity),
            "target content identity remains unchanged after restore"
        );
        fs::remove_dir(&link).expect("remove the verified junction link before fixture cleanup");
        fixture.complete();
    }

    #[cfg(windows)]
    fn run_symbolic_link_trash_diagnostic(directory_link: bool, dangling: bool) {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert_local_fixed_ntfs(temp.path());
        let target = temp.path().join(if directory_link {
            "target-directory"
        } else {
            "target-file.bin"
        });
        let link = temp.path().join(if dangling {
            "dangling-link"
        } else if directory_link {
            "directory-link"
        } else {
            "file-link"
        });
        let target_bytes = b"symbolic link target must remain unchanged";
        let marker = directory_link.then(|| target.join("marker.bin"));

        if !dangling {
            if directory_link {
                fs::create_dir(&target).unwrap();
                fs::write(marker.as_ref().unwrap(), target_bytes).unwrap();
            } else {
                fs::write(&target, target_bytes).unwrap();
            }
        }

        let create_result = if directory_link {
            symlink_dir(&target, &link)
        } else {
            symlink_file(&target, &link)
        };
        if let Err(error) = create_result {
            if error.raw_os_error() == Some(1314) {
                panic!(
                    "Windows returned privilege-not-held (1314) creating {} symbolic link; Shell Trash was not dispatched and behavior was not verified",
                    if directory_link { "directory" } else { "file" }
                );
            }
            panic!("create ordinary symbolic link fixture: {error}");
        }

        let kind = if directory_link {
            crate::directory::EntryKind::Folder
        } else {
            crate::directory::EntryKind::File
        };
        let raw_target = fs::read_link(&link).expect("capture symbolic link raw target");
        let accepted = crate::directory::EntrySourceIdentity::from_path_no_follow(&link, kind)
            .expect("capture no-follow symbolic link identity");
        let target_identity = if dangling {
            assert!(matches!(
                fs::symlink_metadata(&target),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ));
            None
        } else {
            Some(
                crate::directory::EntrySourceIdentity::from_path_no_follow(
                    &target,
                    if directory_link {
                        crate::directory::EntryKind::Folder
                    } else {
                        crate::directory::EntryKind::File
                    },
                )
                .expect("capture target identity"),
            )
        };
        let marker_identity = marker.as_ref().map(|marker| {
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                marker,
                crate::directory::EntryKind::File,
            )
            .expect("capture target marker identity")
        });
        eprintln!(
            "symbolic link diagnostic pre-trash: directory_link={directory_link}, dangling={dangling}, link_identity={accepted:?}, target_identity={target_identity:?}, marker_identity={marker_identity:?}, raw_target={raw_target:?}"
        );

        let mut fixture = TrashLinkDiagnosticFixture::new(temp);
        fixture.arm_before_dispatch();
        let receipt =
            match windows::move_to_trash_with_reparse_identity_for_diagnostic(&link, &accepted) {
                Ok(receipt) => receipt,
                Err(error) => panic!("Shell returned no exact symbolic-link receipt: {error:?}"),
            };
        let receipt_origin = receipt.original_path();
        fixture.record_receipt(receipt);
        assert_eq!(receipt_origin, link);
        assert!(
            matches!(
                fs::symlink_metadata(&link),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "only the symbolic link object should disappear"
        );

        if dangling {
            assert!(
                matches!(
                    fs::symlink_metadata(&target),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ),
                "dangling target remains absent while link is trashed"
            );
        } else {
            let target_kind = if directory_link {
                crate::directory::EntryKind::Folder
            } else {
                crate::directory::EntryKind::File
            };
            assert_eq!(
                crate::directory::EntrySourceIdentity::from_path_no_follow(&target, target_kind),
                target_identity,
                "target identity is unchanged while link is trashed"
            );
            if directory_link {
                assert_eq!(fs::read(marker.as_ref().unwrap()).unwrap(), target_bytes);
            } else {
                assert_eq!(fs::read(&target).unwrap(), target_bytes);
            }
            if let (Some(marker), Some(expected_identity)) = (&marker, &marker_identity) {
                assert_eq!(
                    crate::directory::EntrySourceIdentity::from_path_no_follow(
                        marker,
                        crate::directory::EntryKind::File,
                    ),
                    Some(expected_identity.clone()),
                    "directory target marker identity is unchanged"
                );
            }
        }

        fixture
            .restore_exact_receipt()
            .expect("restore exact Shell symbolic-link receipt");
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(&link, kind),
            Some(accepted),
            "restored symbolic link has the same no-follow identity"
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            raw_target,
            "restored symbolic link has the same raw target"
        );
        if dangling {
            assert!(
                matches!(
                    fs::symlink_metadata(&target),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ),
                "dangling target remains absent after restore"
            );
        } else {
            let target_kind = if directory_link {
                crate::directory::EntryKind::Folder
            } else {
                crate::directory::EntryKind::File
            };
            assert_eq!(
                crate::directory::EntrySourceIdentity::from_path_no_follow(&target, target_kind),
                target_identity,
                "target identity is unchanged after restore"
            );
            if directory_link {
                assert_eq!(fs::read(marker.as_ref().unwrap()).unwrap(), target_bytes);
            } else {
                assert_eq!(fs::read(&target).unwrap(), target_bytes);
            }
            if let (Some(marker), Some(expected_identity)) = (&marker, &marker_identity) {
                assert_eq!(
                    crate::directory::EntrySourceIdentity::from_path_no_follow(
                        marker,
                        crate::directory::EntryKind::File,
                    ),
                    Some(expected_identity.clone()),
                    "directory target marker identity is unchanged after restore"
                );
            }
        }
        eprintln!(
            "symbolic link diagnostic post-restore: directory_link={directory_link}, dangling={dangling}, link_identity={:?}, raw_target={:?}",
            crate::directory::EntrySourceIdentity::from_path_no_follow(&link, kind),
            fs::read_link(&link).unwrap()
        );

        if directory_link {
            fs::remove_dir(&link).expect("remove verified directory symbolic link");
        } else {
            fs::remove_file(&link).expect("remove verified file symbolic link");
        }
        fixture.complete();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; explicitly diagnoses IFileOperation behavior for a file symbolic link"]
    fn windows_shell_adapter_file_symlink_trash_preserves_target_and_restores_link() {
        run_symbolic_link_trash_diagnostic(false, false);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; explicitly diagnoses IFileOperation behavior for a directory symbolic link"]
    fn windows_shell_adapter_directory_symlink_trash_preserves_target_and_restores_link() {
        run_symbolic_link_trash_diagnostic(true, false);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; explicitly diagnoses IFileOperation behavior for a dangling symbolic link"]
    fn windows_shell_adapter_dangling_symlink_trash_preserves_target_and_restores_link() {
        run_symbolic_link_trash_diagnostic(false, true);
    }

    #[cfg(windows)]
    #[test]
    fn delete_callback_receipt_requires_exactly_one_successful_nonempty_id() {
        let expected = OsString::from("recycle-item");
        let mut callbacks = DeleteCallbacks::default();
        callbacks.record(true, Some(Ok(expected.clone())));
        assert_eq!(callbacks.receipt_id(), Ok(expected));

        for callbacks in [
            DeleteCallbacks::default(),
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(true, Some(Ok(OsString::from("same-id"))));
                callbacks.record(true, Some(Ok(OsString::from("same-id"))));
                callbacks
            },
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(false, Some(Ok(OsString::from("ignored-id"))));
                callbacks
            },
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(true, None);
                callbacks
            },
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(true, Some(Ok(OsString::new())));
                callbacks
            },
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(true, Some(Err("receipt extraction failed".into())));
                callbacks
            },
        ] {
            assert!(matches!(
                callbacks.receipt_id(),
                Err(TrashError::MutationUncertain(_))
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn delete_callback_receipt_rejects_mixed_success_and_failure_orderings() {
        let mixed_callbacks = [
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(true, Some(Ok(OsString::from("recycle-item"))));
                callbacks.record(false, None);
                callbacks
            },
            {
                let mut callbacks = DeleteCallbacks::default();
                callbacks.record(false, None);
                callbacks.record(true, Some(Ok(OsString::from("recycle-item"))));
                callbacks
            },
        ];

        for callbacks in mixed_callbacks {
            assert!(matches!(
                callbacks.receipt_id(),
                Err(TrashError::MutationUncertain(_))
            ));
        }
    }

    #[test]
    fn restore_preflight_rejects_existing_file_and_directory_without_calling_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("existing-file");
        fs::write(&file, b"keep this file").unwrap();
        let directory = temp.path().join("existing-directory");
        fs::create_dir(&directory).unwrap();
        let child = directory.join("child");
        fs::write(&child, b"keep this child").unwrap();

        for target in [&file, &directory] {
            let restore_called = Cell::new(false);
            let result = restore_if_target_missing(target, || {
                restore_called.set(true);
                Ok(())
            });
            assert_eq!(
                result,
                Err(TrashError::RestoreCollision(target.to_path_buf()))
            );
            assert!(!restore_called.get(), "opaque trash receipt was not used");
        }

        assert_eq!(fs::read(&file).unwrap(), b"keep this file");
        assert_eq!(fs::read(&child).unwrap(), b"keep this child");
    }

    #[test]
    fn restore_preflight_calls_receipt_only_for_a_missing_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let restore_called = Cell::new(false);
        let result = restore_if_target_missing(&missing, || {
            restore_called.set(true);
            Ok("restored")
        });
        assert_eq!(result, Ok("restored"));
        assert!(restore_called.get());
    }

    #[test]
    fn restore_preflight_maps_non_not_found_metadata_errors_to_platform() {
        let invalid = Path::new("invalid\0leaf");
        let restore_called = Cell::new(false);
        let result = restore_if_target_missing(invalid, || {
            restore_called.set(true);
            Ok(())
        });
        assert!(matches!(result, Err(TrashError::Platform(_))));
        assert!(!restore_called.get());
    }

    #[cfg(windows)]
    #[test]
    fn restore_preflight_rejects_dangling_junction_without_changing_it() {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let target = temp.path().join("junction");
        let backing = temp.path().join("backing");
        fs::create_dir(&backing).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&target)
            .arg(&backing)
            .status()
            .unwrap();
        assert!(status.success(), "junction fixture creation failed");
        let _remove_link_before_tempdir = RemoveJunction(target.clone());
        let raw_target = fs::read_link(&target).unwrap();
        fs::remove_dir(&backing).unwrap();
        assert!(!target.exists(), "fixture must be dangling");

        let restore_called = Cell::new(false);
        let result = restore_if_target_missing(&target, || {
            restore_called.set(true);
            Ok(())
        });
        assert_eq!(
            result,
            Err(TrashError::RestoreCollision(target.to_path_buf()))
        );
        assert!(!restore_called.get(), "opaque trash receipt was not used");
        assert_eq!(fs::read_link(&target).unwrap(), raw_target);
        assert!(fs::symlink_metadata(&target).is_ok());

        // 一時ディレクトリの後始末より先に、junction 自体を明示的に外す。
        fs::remove_dir(&target).unwrap();
    }

    #[cfg(windows)]
    struct RemoveJunction(PathBuf);

    #[cfg(windows)]
    impl Drop for RemoveJunction {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }
}
