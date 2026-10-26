use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use windows_062::{
    core::{implement, Ref, HRESULT, PCWSTR},
    Win32::{
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        },
        UI::Shell::{
            FileOperation, IFileOperation, IFileOperationProgressSink, IShellItem,
            SHCreateItemFromParsingName, SHQueryRecycleBinW, FOFX_EARLYFAILURE,
            FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_NO_UI,
            SHQUERYRBINFO, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
        },
    },
};

use super::{DeleteCallbacks, RestoreDisposition, TrashError, TrashReceipt};

// Shell reports COPYENGINE_S_DONT_PROCESS_CHILDREN for a completed top-level move.
const COPYENGINE_S_DONT_PROCESS_CHILDREN: HRESULT = HRESULT(0x0027_0008);

#[derive(Clone, Copy)]
struct RestoreCallbackEvidence {
    exact: bool,
}

#[implement(IFileOperationProgressSink)]
struct RestoreSink {
    callbacks: Arc<Mutex<Vec<RestoreCallbackEvidence>>>,
    expected_source: OsString,
    expected_parent: PathBuf,
    expected_target: PathBuf,
    expected_name: OsString,
    #[cfg(test)]
    abort_in_pre_move: bool,
    #[cfg(test)]
    pre_move_called: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)]
    test_receipt: Option<TrashReceipt>,
}

fn os_string_from_wide_ptr(value: *const u16) -> Option<OsString> {
    if value.is_null() {
        return None;
    }
    let mut len = 0;
    unsafe {
        while *value.add(len) != 0 {
            len += 1;
        }
        Some(OsString::from_wide(std::slice::from_raw_parts(value, len)))
    }
}

fn shell_item_name(
    item: Option<&IShellItem>,
    kind: windows_062::Win32::UI::Shell::SIGDN,
) -> Option<OsString> {
    let item = item?;
    let display = unsafe { item.GetDisplayName(kind) }.ok()?;
    let value = os_string_from_wide_ptr(display.0);
    unsafe { CoTaskMemFree(Some(display.0.cast())) };
    value
}

fn callback_name(name: &PCWSTR) -> Option<OsString> {
    os_string_from_wide_ptr(name.0)
}

fn restore_callback_is_exact(
    expected_source: &OsStr,
    expected_parent: &Path,
    expected_target: &Path,
    expected_name: &OsStr,
    source_parsing_name: Option<&OsStr>,
    destination_parent_path: Option<&OsStr>,
    new_name: Option<&OsStr>,
    moved_path: Option<&Path>,
    result: HRESULT,
) -> bool {
    source_parsing_name == Some(expected_source)
        && destination_parent_path.is_some_and(|parent| Path::new(parent) == expected_parent)
        && new_name == Some(expected_name)
        && moved_path == Some(expected_target)
        && moved_path
            .and_then(Path::file_name)
            .is_some_and(|name| name == expected_name)
        // Shell returned S_OK or its observed completed top-level move status.
        && (result == HRESULT(0) || result == COPYENGINE_S_DONT_PROCESS_CHILDREN)
}

impl RestoreSink {
    fn record_move(
        &self,
        source: Option<&IShellItem>,
        destination_parent: Option<&IShellItem>,
        new_name: &PCWSTR,
        result: HRESULT,
        moved_item: Option<&IShellItem>,
    ) {
        let source = shell_item_name(source, SIGDN_DESKTOPABSOLUTEPARSING);
        let parent = shell_item_name(destination_parent, SIGDN_FILESYSPATH);
        let name = callback_name(new_name);
        let moved_path = shell_item_name(moved_item, SIGDN_FILESYSPATH).map(PathBuf::from);
        let exact = restore_callback_is_exact(
            &self.expected_source,
            &self.expected_parent,
            &self.expected_target,
            &self.expected_name,
            source.as_deref(),
            parent.as_deref(),
            name.as_deref(),
            moved_path.as_deref(),
            result,
        );
        #[cfg(test)]
        let suppressed = self.test_receipt.as_ref().is_some_and(|receipt| {
            receipt.observe_restore_callback_for_test(super::RestoreCallbackObservation {
                source: source
                    .as_deref()
                    .map(|value| value.to_string_lossy().into_owned()),
                parent: parent
                    .as_deref()
                    .map(|value| value.to_string_lossy().into_owned()),
                name: name
                    .as_deref()
                    .map(|value| value.to_string_lossy().into_owned()),
                moved_path: moved_path
                    .as_deref()
                    .map(|value| value.to_string_lossy().into_owned()),
                result: result.0,
                exact_success: exact,
                suppressed: false,
            })
        });
        #[cfg(not(test))]
        let suppressed = false;
        if !suppressed {
            self.callbacks
                .lock()
                .expect("restore callback mutex poisoned")
                .push(RestoreCallbackEvidence { exact });
        }
    }
}

impl windows_062::Win32::UI::Shell::IFileOperationProgressSink_Impl for RestoreSink_Impl {
    fn StartOperations(&self) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn FinishOperations(&self, _: HRESULT) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PreRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        #[cfg(test)]
        if self.abort_in_pre_move {
            if let Some(called) = &self.pre_move_called {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            return Err(windows_062::core::Error::from_hresult(HRESULT(
                0x8000_4004u32 as i32,
            )));
        }
        Ok(())
    }

    fn PostMoveItem(
        &self,
        _: u32,
        source: Ref<'_, IShellItem>,
        destination_parent: Ref<'_, IShellItem>,
        new_name: &PCWSTR,
        result: HRESULT,
        moved_item: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        self.record_move(
            source.as_ref(),
            destination_parent.as_ref(),
            new_name,
            result,
            moved_item.as_ref(),
        );
        Ok(())
    }

    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PreDeleteItem(&self, _: u32, _: Ref<'_, IShellItem>) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PreNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn UpdateProgress(&self, _: u32, _: u32) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn ResetTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn PauseTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }

    fn ResumeTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }
}

fn classify_restore_callbacks(
    callbacks: &[RestoreCallbackEvidence],
    aggregate_anomaly: Option<String>,
) -> Result<RestoreDisposition, TrashError> {
    if callbacks.len() != 1 || !callbacks[0].exact {
        return Err(TrashError::MutationUncertain(format!(
            "Shell did not report exactly one exact successful restore callback ({} callbacks)",
            callbacks.len()
        )));
    }
    Ok(match aggregate_anomaly {
        Some(anomaly) => RestoreDisposition::CompletedWithAnomaly(anomaly),
        None => RestoreDisposition::Completed,
    })
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, TrashError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(platform)?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[implement(IFileOperationProgressSink)]
struct DeleteSink {
    callbacks: Arc<Mutex<DeleteCallbacks>>,
    accepted_path: Option<PathBuf>,
    accepted_guard: Option<crate::operations::model::TrashSourceGuard>,
    accepted_tag: Option<u32>,
    #[cfg(test)]
    abort_in_pre_delete: bool,
    #[cfg(test)]
    pre_delete_called: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)]
    after_pre_delete_validation: Option<Arc<Mutex<Option<Box<dyn FnOnce() -> bool + Send>>>>>,
}

impl DeleteSink_Impl {
    fn record_delete(
        &self,
        result: HRESULT,
        item: Option<&IShellItem>,
    ) -> windows_062::core::Result<()> {
        let id = item.map(|item| {
            shell_item_name(Some(item), SIGDN_DESKTOPABSOLUTEPARSING)
                .ok_or_else(|| "could not read Shell parsing name".to_string())
        });
        self.callbacks
            .lock()
            .expect("trash callback mutex poisoned")
            .record(result.is_ok(), id);
        Ok(())
    }
}

fn shell_item_matches_accepted_source(
    item: Option<&IShellItem>,
    accepted_path: &Path,
    accepted: &crate::operations::model::TrashSourceGuard,
    accepted_tag: Option<u32>,
) -> bool {
    let Some(item) = item else {
        return false;
    };
    let Some(effective_path) = shell_item_name(Some(item), SIGDN_FILESYSPATH) else {
        return false;
    };
    let effective_path = PathBuf::from(effective_path);
    if effective_path != accepted_path {
        return false;
    }
    probe_source_handle(&effective_path).is_some_and(|(identity, tag)| {
        accepted_source_facts_match(accepted, accepted_tag, &identity, tag)
    })
}

const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

fn trash_tag_is_admitted(candidate: bool, tag: u32) -> bool {
    if candidate {
        tag == IO_REPARSE_TAG_MOUNT_POINT || tag == IO_REPARSE_TAG_SYMLINK
    } else {
        tag == 0
    }
}

fn accepted_source_facts_match(
    accepted: &crate::operations::model::TrashSourceGuard,
    accepted_tag: Option<u32>,
    observed_identity: &crate::filesystem_identity::FilesystemIdentity,
    observed_tag: u32,
) -> bool {
    observed_identity == &accepted.identity.filesystem_identity()
        && observed_tag == accepted_tag.unwrap_or(0)
        && trash_tag_is_admitted(accepted.reparse_candidate, observed_tag)
}

fn probe_source_handle(path: &Path) -> Option<(crate::filesystem_identity::FilesystemIdentity, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_062::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, GetFileInformationByHandleEx,
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FileAttributeTagInfo, OPEN_EXISTING,
        },
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .ok()?
    };
    struct Owned(HANDLE);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    let handle = Owned(handle);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle.0, &mut info).ok()? };
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
        .ok()?
    };
    let identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    );
    Some((identity, tag.ReparseTag))
}

impl windows_062::Win32::UI::Shell::IFileOperationProgressSink_Impl for DeleteSink_Impl {
    fn StartOperations(&self) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, _: u32, item: Ref<'_, IShellItem>) -> windows_062::core::Result<()> {
        #[cfg(test)]
        if self.abort_in_pre_delete {
            if let Some(called) = &self.pre_delete_called {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            return Err(windows_062::core::Error::from_hresult(HRESULT(
                0x8000_4004u32 as i32,
            )));
        }
        let accepted = self
            .accepted_path
            .as_ref()
            .zip(self.accepted_guard.as_ref())
            .is_some_and(|(accepted_path, accepted_guard)| {
                shell_item_matches_accepted_source(
                    item.as_ref(),
                    accepted_path,
                    accepted_guard,
                    self.accepted_tag,
                )
            });
        if accepted {
            #[cfg(test)]
            if let Some(hook) = &self.after_pre_delete_validation {
                let hook = hook
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if hook.is_some_and(|hook| !hook()) {
                    return Err(windows_062::core::Error::from_hresult(HRESULT(
                        0x8000_4004u32 as i32,
                    )));
                }
            }
            Ok(())
        } else {
            Err(windows_062::core::Error::from_hresult(HRESULT(
                0x8000_4004u32 as i32,
            )))
        }
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        result: HRESULT,
        new_item: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        self.record_delete(result, new_item.as_ref())
    }
    fn PreNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows_062::core::Result<()> {
        Ok(())
    }
}

pub(super) fn restore(receipt: TrashReceipt) -> Result<RestoreDisposition, TrashError> {
    restore_inner(receipt, false, None)
}

#[cfg(test)]
pub(super) fn restore_aborting_in_pre_move_for_test(
    receipt: TrashReceipt,
    pre_move_called: Arc<std::sync::atomic::AtomicBool>,
) -> Result<RestoreDisposition, TrashError> {
    restore_inner(receipt, true, Some(pre_move_called))
}

fn restore_inner(
    receipt: TrashReceipt,
    #[cfg_attr(not(test), allow(unused_variables))] abort_in_pre_move: bool,
    #[cfg_attr(not(test), allow(unused_variables))] pre_move_called: Option<
        Arc<std::sync::atomic::AtomicBool>,
    >,
) -> Result<RestoreDisposition, TrashError> {
    let target = receipt.item.original_path();
    let parent = receipt.item.original_parent.clone();
    let name = receipt.item.name.clone();
    let _apartment = ComApartment::initialize()?;
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
            .map_err(platform)?;
    unsafe { operation.SetOperationFlags(FOF_NO_UI | FOFX_EARLYFAILURE) }.map_err(platform)?;

    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let sink: IFileOperationProgressSink = RestoreSink {
        callbacks: callbacks.clone(),
        expected_source: receipt.item.id.clone(),
        expected_parent: parent.clone(),
        expected_target: target,
        expected_name: name.clone(),
        #[cfg(test)]
        abort_in_pre_move,
        #[cfg(test)]
        pre_move_called,
        #[cfg(test)]
        test_receipt: Some(receipt.clone()),
    }
    .into();
    let source_wide = receipt
        .item
        .id
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let source: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(source_wide.as_ptr()), None) }
            .map_err(platform)?;
    let parent_wide = parent
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination_parent: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(parent_wide.as_ptr()), None) }
            .map_err(platform)?;
    let name_wide = name.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    unsafe {
        operation
            .MoveItem(
                &source,
                &destination_parent,
                PCWSTR(name_wide.as_ptr()),
                Some(&sink),
            )
            .map_err(platform)?;
    }

    let performed = unsafe { operation.PerformOperations() };
    // GetAnyOperationsAborted is a separate fact and must be queried even when PerformOperations fails.
    let aborted = unsafe { operation.GetAnyOperationsAborted() };
    let aggregate_anomaly = match (performed, aborted) {
        (Ok(()), Ok(aborted)) if !aborted.as_bool() => None,
        (performed, aborted) => Some(format!(
            "PerformOperations={performed:?}; GetAnyOperationsAborted={aborted:?}"
        )),
    };
    let callbacks =
        std::mem::take(&mut *callbacks.lock().expect("restore callback mutex poisoned"));
    let result = classify_restore_callbacks(&callbacks, aggregate_anomaly);
    #[cfg(test)]
    receipt.record_restore_classifier_result_for_test(&result);
    result
}

pub(super) fn move_to_trash_with_identity(
    path: &Path,
    accepted: &crate::directory::EntrySourceIdentity,
) -> Result<TrashReceipt, TrashError> {
    let guard = crate::operations::model::TrashSourceGuard {
        identity: accepted.clone(),
        reparse_candidate: false,
    };
    move_to_trash_with_guard(path, &guard, None)
}

pub(super) fn admit_source(
    path: &Path,
    guard: &crate::operations::model::TrashSourceGuard,
) -> Result<Option<u32>, TrashError> {
    let Some((identity, tag)) = probe_source_handle(path) else {
        return Err(TrashError::Platform(
            "no-follow source probe failed".into(),
        ));
    };
    if identity != guard.identity.filesystem_identity() {
        return Err(TrashError::Platform(
            "Trash source identity changed".into(),
        ));
    }
    if guard.reparse_candidate {
        if !trash_tag_is_admitted(true, tag) {
            return Err(TrashError::Platform(
                "only symbolic-link or mount-point junction Trash is supported".into(),
            ));
        }
        Ok(Some(tag))
    } else if trash_tag_is_admitted(false, tag) {
        Ok(None)
    } else {
        Err(TrashError::Platform(
            "ordinary source was replaced by a reparse point".into(),
        ))
    }
}

pub(super) fn move_to_trash_with_guard(
    path: &Path,
    guard: &crate::operations::model::TrashSourceGuard,
    tag: Option<u32>,
) -> Result<TrashReceipt, TrashError> {
    if guard.reparse_candidate != tag.is_some()
        || tag.is_some_and(|tag| !trash_tag_is_admitted(true, tag))
    {
        return Err(TrashError::Platform(
            "Trash reparse tag admission is invalid".into(),
        ));
    }
    #[cfg(test)]
    {
        move_to_trash_inner(path, Some((guard, tag)), false, None, None, None)
    }
    #[cfg(not(test))]
    {
        move_to_trash_inner(path, Some((guard, tag)))
    }
}

#[cfg(test)]
pub(super) fn move_to_trash_aborting_in_pre_delete(
    path: &Path,
    pre_delete_called: Arc<std::sync::atomic::AtomicBool>,
) -> Result<TrashReceipt, TrashError> {
    move_to_trash_inner(path, None, true, Some(pre_delete_called), None, None)
}

#[cfg(test)]
pub(super) fn move_to_trash_with_reparse_identity_for_diagnostic(
    path: &Path,
    accepted: &crate::directory::EntrySourceIdentity,
) -> Result<TrashReceipt, TrashError> {
    let guard = crate::operations::model::TrashSourceGuard {
        identity: accepted.clone(),
        reparse_candidate: true,
    };
    let tag = admit_source(path, &guard)?;
    move_to_trash_with_guard(path, &guard, tag)
}

#[cfg(test)]
fn move_to_trash_with_queued_item_hook_for_test(
    path: &Path,
    accepted: &crate::directory::EntrySourceIdentity,
    before_perform: impl FnOnce() + 'static,
) -> Result<TrashReceipt, TrashError> {
    move_to_trash_inner(
        path,
        Some((
            &crate::operations::model::TrashSourceGuard {
                identity: accepted.clone(),
                reparse_candidate: false,
            },
            None,
        )),
        false,
        None,
        Some(Box::new(before_perform)),
        None,
    )
}

#[cfg(test)]
fn move_to_trash_with_post_validation_hook_for_test(
    path: &Path,
    accepted: &crate::directory::EntrySourceIdentity,
    hook: impl FnOnce() -> bool + Send + 'static,
) -> Result<TrashReceipt, TrashError> {
    let guard = crate::operations::model::TrashSourceGuard {
        identity: accepted.clone(),
        reparse_candidate: false,
    };
    move_to_trash_inner(
        path,
        Some((&guard, None)),
        false,
        None,
        None,
        Some(Arc::new(Mutex::new(Some(Box::new(hook))))),
    )
}

fn move_to_trash_inner(
    path: &Path,
    accepted: Option<(&crate::operations::model::TrashSourceGuard, Option<u32>)>,
    #[cfg(test)] abort_in_pre_delete: bool,
    #[cfg(test)] pre_delete_called: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)] before_perform: Option<Box<dyn FnOnce()>>,
    #[cfg(test)] after_pre_delete_validation: Option<
        Arc<Mutex<Option<Box<dyn FnOnce() -> bool + Send>>>>,
    >,
) -> Result<TrashReceipt, TrashError> {
    if path.file_name().is_none() || path.parent().is_none() {
        return Err(TrashError::Platform(
            "source must have a name and parent directory".into(),
        ));
    }
    query_source_recycle_bin(path)?;
    let _apartment = ComApartment::initialize()?;
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
            .map_err(platform)?;
    unsafe {
        operation
            .SetOperationFlags(
                FOF_ALLOWUNDO
                    | FOF_NOCONFIRMATION
                    | FOF_NOERRORUI
                    | FOFX_EARLYFAILURE
                    | FOFX_RECYCLEONDELETE,
            )
            .map_err(platform)?;
    }

    let callbacks = Arc::new(Mutex::new(DeleteCallbacks::default()));
    let sink: IFileOperationProgressSink = DeleteSink {
        callbacks: callbacks.clone(),
        accepted_path: accepted.map(|_| path.to_path_buf()),
        accepted_guard: accepted.map(|(guard, _)| guard.clone()),
        accepted_tag: accepted.and_then(|(_, tag)| tag),
        #[cfg(test)]
        abort_in_pre_delete,
        #[cfg(test)]
        pre_delete_called,
        #[cfg(test)]
        after_pre_delete_validation,
    }
    .into();
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None) }
        .map_err(platform)?;
    unsafe { operation.DeleteItem(&item, Some(&sink)) }.map_err(platform)?;
    #[cfg(test)]
    if let Some(before_perform) = before_perform {
        before_perform();
    }

    let performed = unsafe { operation.PerformOperations() };
    let aborted = unsafe { operation.GetAnyOperationsAborted() };
    let (Ok(()), Ok(aborted)) = (performed, aborted) else {
        return Err(TrashError::MutationUncertain(
            "Shell could not confirm the recycle operation result".into(),
        ));
    };
    if aborted.as_bool() {
        return Err(TrashError::MutationUncertain(
            "Shell reported that the recycle operation was aborted".into(),
        ));
    }

    let id = std::mem::take(&mut *callbacks.lock().expect("trash callback mutex poisoned"))
        .receipt_id()?;
    TrashReceipt::from_parts(id, path)
}

pub(super) fn query_source_recycle_bin(path: &Path) -> Result<(), TrashError> {
    const MAX_PATH_UNITS: usize = 32_767;
    const SH_QUERY_MAX_PATH_UNITS: usize = 260;
    if !path.is_absolute() {
        return Err(TrashError::Platform(
            "source path is not absolute and has no safe Recycle Bin query root".into(),
        ));
    }
    let source = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    if source.len() > MAX_PATH_UNITS {
        return Err(TrashError::Platform(
            "source path is too long to query its Recycle Bin".into(),
        ));
    }

    let mut volume_root = vec![0u16; MAX_PATH_UNITS + 1];
    // GetVolumePathNameW resolves mounted-folder paths to their actual volume root.
    // SAFETY: both buffers are NUL-terminated / writable UTF-16 buffers of the declared size.
    let ok = unsafe {
        GetVolumePathNameW(
            source.as_ptr(),
            volume_root.as_mut_ptr(),
            volume_root.len() as u32,
        )
    };
    if ok == 0 {
        return Err(platform(std::io::Error::last_os_error()));
    }
    let Some(end) = volume_root.iter().position(|unit| *unit == 0) else {
        return Err(TrashError::Platform(
            "Windows returned an unterminated volume query root".into(),
        ));
    };
    // SHQueryRecycleBinW accepts drive, folder, and subfolder paths, but its
    // input is limited to MAX_PATH UTF-16 units including the terminator.
    if end == 0 || end + 1 > SH_QUERY_MAX_PATH_UNITS {
        return Err(TrashError::Platform(
            "source volume has no supported Recycle Bin query root".into(),
        ));
    }

    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // A successful query is only a preflight signal; receipt validation after
    // the operation remains necessary because policy and volume state can race.
    unsafe { SHQueryRecycleBinW(PCWSTR(volume_root.as_ptr()), &mut info) }.map_err(platform)
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetVolumePathNameW(
        file_name: *const u16,
        volume_path_name: *mut u16,
        buffer_length: u32,
    ) -> i32;
}

fn platform(error: impl std::fmt::Display) -> TrashError {
    TrashError::Platform(error.to_string())
}

#[cfg(test)]
mod restore_classifier_tests {
    use super::*;

    const COPYENGINE_S_USER_RETRY: HRESULT = HRESULT(0x0027_0004);
    const COPYENGINE_S_USER_IGNORED: HRESULT = HRESULT(0x0027_0005);
    const COPYENGINE_S_PENDING: HRESULT = HRESULT(0x0027_000b);

    #[test]
    fn restore_evidence_uses_literal_filesystem_name_when_display_label_differs() {
        use std::os::windows::ffi::OsStringExt;

        let parent = Path::new(r"C:\restore-test");
        let name = OsString::from("report.txt");
        let target = parent.join(&name);
        let presentation_label = OsString::from("report");
        assert_ne!(presentation_label, name);
        let exact_with_result = |result| {
            restore_callback_is_exact(
                OsStr::new(r"::{recycle-item}"),
                parent,
                &target,
                &name,
                Some(OsStr::new(r"::{recycle-item}")),
                Some(parent.as_os_str()),
                Some(name.as_os_str()),
                Some(target.as_path()),
                result,
            )
        };
        assert!(exact_with_result(HRESULT(0)));
        assert!(exact_with_result(COPYENGINE_S_DONT_PROCESS_CHILDREN));
        assert!(!exact_with_result(COPYENGINE_S_USER_RETRY));
        assert!(!exact_with_result(COPYENGINE_S_USER_IGNORED));
        assert!(!exact_with_result(COPYENGINE_S_PENDING));
        assert!(!exact_with_result(HRESULT(0x8000_4005u32 as i32)));

        let unpaired_surrogate = [0xD800, 0];
        assert_eq!(
            os_string_from_wide_ptr(unpaired_surrogate.as_ptr()),
            Some(OsString::from_wide(&unpaired_surrogate[..1]))
        );
    }

    #[test]
    fn restore_classifier_requires_one_exact_callback_and_keeps_aggregate_anomaly_separate() {
        let exact = RestoreCallbackEvidence { exact: true };
        let mismatch = RestoreCallbackEvidence { exact: false };

        assert_eq!(
            classify_restore_callbacks(&[exact], None),
            Ok(RestoreDisposition::Completed)
        );
        assert_eq!(
            classify_restore_callbacks(&[exact], Some("aggregate query failed".into())),
            Ok(RestoreDisposition::CompletedWithAnomaly(
                "aggregate query failed".into()
            ))
        );
        assert!(matches!(
            classify_restore_callbacks(&[], None),
            Err(TrashError::MutationUncertain(_))
        ));
        assert!(matches!(
            classify_restore_callbacks(&[exact, exact], None),
            Err(TrashError::MutationUncertain(_))
        ));
        assert!(matches!(
            classify_restore_callbacks(&[mismatch], None),
            Err(TrashError::MutationUncertain(_))
        ));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn trash_tag_admission_accepts_only_ordinary_symlink_or_junction_tags() {
        assert!(super::trash_tag_is_admitted(false, 0));
        assert!(!super::trash_tag_is_admitted(
            false,
            super::IO_REPARSE_TAG_MOUNT_POINT
        ));
        assert!(!super::trash_tag_is_admitted(true, 0));
        assert!(super::trash_tag_is_admitted(
            true,
            super::IO_REPARSE_TAG_MOUNT_POINT
        ));
        assert!(super::trash_tag_is_admitted(
            true,
            super::IO_REPARSE_TAG_SYMLINK
        ));
        assert!(!super::trash_tag_is_admitted(true, 0xA000_0027));
    }

    #[test]
    fn callback_source_facts_require_exact_identity_and_admitted_tag() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        let other_file = root.path().join("other-file");
        fs::write(&file, b"a").unwrap();
        fs::write(&other_file, b"b").unwrap();
        let file_identity =
            EntrySourceIdentity::from_path_no_follow(&file, EntryKind::File).unwrap();
        let other_identity =
            EntrySourceIdentity::from_path_no_follow(&other_file, EntryKind::File).unwrap();
        let observed_identity = file_identity.filesystem_identity();

        let ordinary = crate::operations::model::TrashSourceGuard {
            identity: file_identity.clone(),
            reparse_candidate: false,
        };
        assert!(accepted_source_facts_match(
            &ordinary,
            None,
            &observed_identity,
            0
        ));
        assert!(!accepted_source_facts_match(
            &ordinary,
            None,
            &observed_identity,
            IO_REPARSE_TAG_SYMLINK,
        ));

        for tag in [IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK] {
            let candidate = crate::operations::model::TrashSourceGuard {
                identity: file_identity.clone(),
                reparse_candidate: true,
            };
            assert!(accepted_source_facts_match(
                &candidate,
                Some(tag),
                &observed_identity,
                tag,
            ));
            assert!(!accepted_source_facts_match(
                &candidate,
                Some(tag),
                &observed_identity,
                if tag == IO_REPARSE_TAG_SYMLINK {
                    IO_REPARSE_TAG_MOUNT_POINT
                } else {
                    IO_REPARSE_TAG_SYMLINK
                },
            ));
            assert!(!accepted_source_facts_match(
                &candidate,
                Some(tag),
                &observed_identity,
                0
            ));
            assert!(!accepted_source_facts_match(
                &candidate,
                Some(tag),
                &observed_identity,
                0xA000_0027
            ));
            assert!(!accepted_source_facts_match(
                &candidate,
                Some(tag),
                &other_identity.filesystem_identity(),
                tag,
            ));
        }
    }

    use super::*;
    use crate::directory::{EntryKind, EntrySourceIdentity};
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    use windows::{
        Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW, MoveFileW},
        core::PCWSTR,
    };

    const BYTES_A: &[u8] = b"queued Shell object A";
    const BYTES_B: &[u8] = b"replacement object B";

    #[derive(Clone, Debug)]
    struct RecycleCandidate {
        id: std::ffi::OsString,
        original_path: PathBuf,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ObservedIdentity {
        A,
        B,
        Uncertain,
    }

    struct QueuedDeleteFixture {
        root: Option<tempfile::TempDir>,
        root_path: PathBuf,
        source: PathBuf,
        accepted_a: PathBuf,
        relocated_b: PathBuf,
        identity_a: EntrySourceIdentity,
        identity_b: Arc<Mutex<Option<EntrySourceIdentity>>>,
        armed: bool,
    }

    impl QueuedDeleteFixture {
        fn identity(path: &Path) -> Result<Option<EntrySourceIdentity>, String> {
            match fs::symlink_metadata(path) {
                Ok(_) => EntrySourceIdentity::from_path_no_follow(path, EntryKind::File)
                    .map(Some)
                    .ok_or_else(|| {
                        format!("identity unavailable for existing path {}", path.display())
                    }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("inspect {}: {error}", path.display())),
            }
        }

        fn move_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
            let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
            let destination_wide: Vec<u16> = destination
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            unsafe {
                MoveFileW(
                    PCWSTR(source_wide.as_ptr()),
                    PCWSTR(destination_wide.as_ptr()),
                )
            }
            .map_err(|error| {
                format!(
                    "no-replace move {} -> {} failed: {error}",
                    source.display(),
                    destination.display()
                )
            })
        }

        fn relocate_known_source_occupant(&self) -> Result<(), String> {
            let identity_b = self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            match Self::identity(&self.source)? {
                None => Ok(()),
                Some(identity) if identity_b.as_ref() == Some(&identity) => {
                    if !self.relocated_b.starts_with(&self.root_path) {
                        return Err("B relocation escaped the disposable fixture".into());
                    }
                    Self::move_no_replace(&self.source, &self.relocated_b)?;
                    if Self::identity(&self.relocated_b)? != identity_b {
                        return Err(format!(
                            "B identity changed during relocation to {}",
                            self.relocated_b.display()
                        ));
                    }
                    Ok(())
                }
                Some(identity) if identity == self.identity_a => {
                    if !self.accepted_a.starts_with(&self.root_path) {
                        return Err("A relocation escaped the disposable fixture".into());
                    }
                    Self::move_no_replace(&self.source, &self.accepted_a)?;
                    if Self::identity(&self.accepted_a)? != Some(self.identity_a.clone()) {
                        return Err(format!(
                            "A identity changed during relocation to {}",
                            self.accepted_a.display()
                        ));
                    }
                    Ok(())
                }
                Some(identity) => Err(format!(
                    "unproven occupant {:?} at {}; left it untouched",
                    identity,
                    self.source.display()
                )),
            }
        }

        fn verify_both_objects(&self) -> Result<(), String> {
            let identity_b = self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let Some(identity_b) = identity_b.as_ref() else {
                let paths = [&self.source, &self.accepted_a, &self.relocated_b];
                let mut a_paths = Vec::new();
                for path in paths {
                    match Self::identity(path)? {
                        Some(identity) if identity == self.identity_a => a_paths.push(path),
                        Some(identity) => {
                            return Err(format!(
                                "unproven identity {:?} at {}; preserving fixture root",
                                identity,
                                path.display()
                            ));
                        }
                        None => {}
                    }
                }
                if a_paths.len() != 1 {
                    return Err(format!(
                        "without B identity, expected A exactly once across source/accepted_a/relocated_b; found {}; preserving fixture root",
                        a_paths.len()
                    ));
                }
                if fs::read(a_paths[0]).map_err(|error| error.to_string())? != BYTES_A {
                    return Err(format!(
                        "A bytes changed at {}; preserving fixture root",
                        a_paths[0].display()
                    ));
                }
                return Ok(());
            };
            if identity_b == &self.identity_a {
                return Err("A and B unexpectedly share one filesystem identity".into());
            }
            let paths = [&self.source, &self.accepted_a, &self.relocated_b];
            let mut a_paths = Vec::new();
            let mut b_paths = Vec::new();
            for path in paths {
                match Self::identity(path)? {
                    Some(identity) if identity == self.identity_a => a_paths.push(path),
                    Some(identity) if &identity == identity_b => b_paths.push(path),
                    Some(identity) => {
                        return Err(format!(
                            "unproven identity {:?} at {}; preserving fixture root",
                            identity,
                            path.display()
                        ));
                    }
                    None => {}
                }
            }
            if a_paths.len() != 1 || b_paths.len() != 1 {
                return Err(format!(
                    "could not account for A/B exactly once inside fixture (A paths: {}, B paths: {}); preserving fixture root",
                    a_paths.len(),
                    b_paths.len()
                ));
            }
            if fs::read(a_paths[0]).map_err(|error| error.to_string())? != BYTES_A {
                return Err(format!(
                    "A bytes changed at {}; preserving fixture root",
                    a_paths[0].display()
                ));
            }
            if fs::read(b_paths[0]).map_err(|error| error.to_string())? != BYTES_B {
                return Err(format!(
                    "B bytes changed at {}; preserving fixture root",
                    b_paths[0].display()
                ));
            }
            Ok(())
        }

        fn restore_candidate(&mut self, candidate: RecycleCandidate) -> Result<(), String> {
            if candidate.original_path != self.source && candidate.original_path != self.accepted_a
            {
                return Err(format!(
                    "candidate origin {} is outside fixture origins; preserving root",
                    candidate.original_path.display()
                ));
            }
            if candidate.original_path == self.source {
                self.relocate_known_source_occupant()?;
            } else if Self::identity(&self.accepted_a)?.is_some() {
                return Err(format!(
                    "candidate restore origin {} is occupied; preserving root",
                    self.accepted_a.display()
                ));
            }
            crate::operations::trash::restore_recovered_item_for_test(
                candidate.id,
                &candidate.original_path,
            )
            .map_err(|error| format!("restore exact Recycle Bin item: {error:?}"))?;
            if candidate.original_path == self.accepted_a
                && Self::identity(&self.accepted_a)? == Some(self.identity_a.clone())
                && Self::identity(&self.source)?.is_none()
            {
                Self::move_no_replace(&self.accepted_a, &self.source)?;
                if Self::identity(&self.source)? != Some(self.identity_a.clone()) {
                    return Err("A identity changed while moving recovered item to source".into());
                }
            }
            self.verify_both_objects()
        }

        fn matching_candidates(&self) -> Result<Vec<RecycleCandidate>, String> {
            ::trash::os_limited::list()
                .map_err(|error| error.to_string())
                .map(|items| {
                    items
                        .into_iter()
                        .filter_map(|item| {
                            let original_path = item.original_path();
                            (original_path == self.source || original_path == self.accepted_a)
                                .then_some(RecycleCandidate {
                                    id: item.id,
                                    original_path,
                                })
                        })
                        .collect()
                })
        }

        fn recover(&mut self) -> Result<(), String> {
            let candidates = self.matching_candidates()?;
            match candidates.as_slice() {
                [candidate] => self.restore_candidate(candidate.clone())?,
                [] => self.verify_both_objects()?,
                _ => {
                    return Err(format!(
                        "multiple Recycle Bin items match fixture origins; left untouched: {}",
                        candidates
                            .iter()
                            .map(|candidate| format!(
                                "id={} origin={}",
                                candidate.id.to_string_lossy(),
                                candidate.original_path.display()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            self.armed = false;
            Ok(())
        }

        fn restore_receipt(&mut self, receipt: TrashReceipt) -> Result<(), String> {
            self.relocate_known_source_occupant()?;
            super::super::restore(receipt)
                .map_err(|error| format!("restore exact Shell receipt: {error:?}"))?;
            self.verify_both_objects()
        }

        fn restored_identity(&self) -> Result<ObservedIdentity, String> {
            self.identity_at(&self.source)
        }

        fn identity_at(&self, path: &Path) -> Result<ObservedIdentity, String> {
            let identity = Self::identity(path)?
                .ok_or_else(|| format!("restored no object at {}", path.display()))?;
            if identity == self.identity_a {
                Ok(ObservedIdentity::A)
            } else if self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(&identity)
            {
                Ok(ObservedIdentity::B)
            } else {
                Err(format!(
                    "restored object at {} has unproven identity {:?}",
                    path.display(),
                    identity
                ))
            }
        }
    }

    impl Drop for QueuedDeleteFixture {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            if let Err(error) = self.recover() {
                let preserved = self.root.take().map(|root| root.keep());
                eprintln!(
                    "queued Shell Trash fixture recovery failed: {error}; preserved root {}",
                    preserved.as_ref().map_or_else(
                        || self.root_path.display().to_string(),
                        |path| path.display().to_string()
                    )
                );
            }
        }
    }

    struct QueuedDirectoryFixture {
        root: Option<tempfile::TempDir>,
        root_path: PathBuf,
        source: PathBuf,
        accepted_a: PathBuf,
        relocated_b: PathBuf,
        identity_a: EntrySourceIdentity,
        identity_b: Arc<Mutex<Option<EntrySourceIdentity>>>,
        armed: bool,
    }

    impl QueuedDirectoryFixture {
        fn identity(path: &Path) -> Result<Option<EntrySourceIdentity>, String> {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    EntrySourceIdentity::from_path_no_follow(path, EntryKind::Folder)
                        .map(Some)
                        .ok_or_else(|| {
                            format!("directory identity unavailable at {}", path.display())
                        })
                }
                Ok(_) => Err(format!("non-directory occupant at {}", path.display())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("inspect {}: {error}", path.display())),
            }
        }

        fn marker(path: &Path) -> Result<Vec<u8>, String> {
            fs::read(path.join("marker.bin"))
                .map_err(|error| format!("read directory marker in {}: {error}", path.display()))
        }

        fn verify_directory_contents(path: &Path, expected_marker: &[u8]) -> Result<(), String> {
            let mut entries = fs::read_dir(path)
                .map_err(|error| format!("enumerate {}: {error}", path.display()))?;
            let marker = entries
                .next()
                .ok_or_else(|| format!("missing marker in {}; preserving root", path.display()))?
                .map_err(|error| error.to_string())?;
            if marker.file_name() != "marker.bin"
                || !marker
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_file()
                || entries.next().is_some()
            {
                return Err(format!(
                    "unproven directory contents at {}; preserving root",
                    path.display()
                ));
            }
            if Self::marker(path)? != expected_marker {
                return Err(format!(
                    "marker bytes changed at {}; preserving root",
                    path.display()
                ));
            }
            Ok(())
        }

        fn move_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
            QueuedDeleteFixture::move_no_replace(source, destination)
        }

        fn relocate_source_occupant(&self) -> Result<(), String> {
            let identity_b = self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            match Self::identity(&self.source)? {
                None => Ok(()),
                Some(identity) if identity_b.as_ref() == Some(&identity) => {
                    if !self.relocated_b.starts_with(&self.root_path) {
                        return Err("directory B relocation escaped fixture".into());
                    }
                    Self::move_no_replace(&self.source, &self.relocated_b)?;
                    if Self::identity(&self.relocated_b)? != identity_b
                        || Self::marker(&self.relocated_b)? != BYTES_B
                    {
                        return Err("directory B changed during recovery relocation".into());
                    }
                    Ok(())
                }
                Some(identity) if identity == self.identity_a => Err(format!(
                    "A still occupies {}; receipt recovery will not move or overwrite it",
                    self.source.display()
                )),
                Some(identity) => Err(format!(
                    "unproven directory identity {:?} at {}; left untouched",
                    identity,
                    self.source.display()
                )),
            }
        }

        fn verify_both(&self) -> Result<(), String> {
            let expected_leaves = [&self.source, &self.accepted_a, &self.relocated_b];
            for entry in fs::read_dir(&self.root_path)
                .map_err(|error| format!("enumerate fixture root: {error}"))?
            {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                if !expected_leaves.contains(&&path) {
                    return Err(format!(
                        "unproven entry {} at fixture root; preserving root",
                        path.display()
                    ));
                }
            }
            let identity_b = self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let leaves = [&self.source, &self.accepted_a, &self.relocated_b];
            let mut a_paths = Vec::new();
            let mut b_paths = Vec::new();
            for path in leaves {
                match Self::identity(path)? {
                    Some(identity) if identity == self.identity_a => a_paths.push(path),
                    Some(identity) if identity_b.as_ref() == Some(&identity) => b_paths.push(path),
                    Some(identity) => {
                        return Err(format!(
                            "unproven directory identity {:?} at {}; preserving fixture root",
                            identity,
                            path.display()
                        ));
                    }
                    None => {}
                }
            }
            if let Some(identity_b) = identity_b {
                if identity_b == self.identity_a || a_paths.len() != 1 || b_paths.len() != 1 {
                    return Err(format!(
                        "directory identities are not accounted exactly once (A {}, B {}); preserving fixture root",
                        a_paths.len(),
                        b_paths.len()
                    ));
                }
                Self::verify_directory_contents(a_paths[0], BYTES_A)?;
                Self::verify_directory_contents(b_paths[0], BYTES_B)?;
            } else {
                if a_paths.len() != 1 {
                    return Err(format!(
                        "without B identity, expected only A exactly once across all fixture leaves; found {}; preserving fixture root",
                        a_paths.len()
                    ));
                }
                Self::verify_directory_contents(a_paths[0], BYTES_A)?;
            }
            Ok(())
        }

        fn candidates(&self) -> Result<Vec<RecycleCandidate>, String> {
            ::trash::os_limited::list()
                .map_err(|error| error.to_string())
                .map(|items| {
                    items
                        .into_iter()
                        .filter_map(|item| {
                            let original_path = item.original_path();
                            (original_path == self.source || original_path == self.accepted_a)
                                .then_some(RecycleCandidate {
                                    id: item.id,
                                    original_path,
                                })
                        })
                        .collect()
                })
        }

        fn restore_candidate(&mut self, candidate: RecycleCandidate) -> Result<(), String> {
            if candidate.original_path == self.source {
                self.relocate_source_occupant()?;
            } else if candidate.original_path == self.accepted_a {
                if Self::identity(&self.accepted_a)?.is_some() {
                    return Err(format!(
                        "candidate origin {} is occupied; preserving fixture root",
                        self.accepted_a.display()
                    ));
                }
            } else {
                return Err("Recycle Bin candidate origin is outside fixture".into());
            }
            crate::operations::trash::restore_recovered_item_for_test(
                candidate.id,
                &candidate.original_path,
            )
            .map_err(|error| format!("restore exact directory Recycle Bin item: {error:?}"))?;
            if candidate.original_path == self.accepted_a
                && Self::identity(&self.source)?.is_none()
                && Self::identity(&self.accepted_a)? == Some(self.identity_a.clone())
            {
                Self::move_no_replace(&self.accepted_a, &self.source)?;
                if Self::identity(&self.source)? != Some(self.identity_a.clone()) {
                    return Err(
                        "A identity changed while returning recovered directory to source".into(),
                    );
                }
            }
            self.verify_both()
        }

        fn restore_receipt(&mut self, receipt: TrashReceipt) -> Result<(), String> {
            self.relocate_source_occupant()?;
            super::super::restore(receipt)
                .map_err(|error| format!("restore exact directory Shell receipt: {error:?}"))?;
            self.verify_both()
        }

        fn identity_at_source(&self) -> Result<ObservedIdentity, String> {
            let identity = Self::identity(&self.source)?
                .ok_or_else(|| format!("no restored directory at {}", self.source.display()))?;
            if identity == self.identity_a {
                Ok(ObservedIdentity::A)
            } else if self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(&identity)
            {
                Ok(ObservedIdentity::B)
            } else {
                Err(format!(
                    "unproven restored directory identity at {}",
                    self.source.display()
                ))
            }
        }

        fn identity_at(&self, path: &Path) -> Result<ObservedIdentity, String> {
            let identity = Self::identity(path)?
                .ok_or_else(|| format!("no recovered directory at {}", path.display()))?;
            if identity == self.identity_a {
                Ok(ObservedIdentity::A)
            } else if self
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(&identity)
            {
                Ok(ObservedIdentity::B)
            } else {
                Err(format!(
                    "unproven recovered directory identity at {}",
                    path.display()
                ))
            }
        }

        fn recover(&mut self) -> Result<(), String> {
            let candidates = self.candidates()?;
            match candidates.as_slice() {
                [candidate] => self.restore_candidate(candidate.clone())?,
                [] => self.verify_both()?,
                _ => {
                    return Err(format!(
                        "multiple directory recycle candidates left untouched: {}",
                        candidates
                            .iter()
                            .map(|candidate| format!(
                                "id={} origin={}",
                                candidate.id.to_string_lossy(),
                                candidate.original_path.display()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            self.armed = false;
            Ok(())
        }
    }

    impl Drop for QueuedDirectoryFixture {
        fn drop(&mut self) {
            if self.armed {
                if let Err(error) = self.recover() {
                    let preserved = self.root.take().map(|root| root.keep());
                    eprintln!(
                        "queued directory Shell Trash recovery failed: {error}; preserved root {}",
                        preserved.as_ref().map_or_else(
                            || self.root_path.display().to_string(),
                            |path| path.display().to_string()
                        )
                    );
                }
            }
        }
    }

    fn assert_local_fixed_ntfs(path: &Path) {
        let volume_root = path.ancestors().last().expect("volume root");
        let wide: Vec<u16> = volume_root
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        const DRIVE_FIXED: u32 = 3;
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        let mut filesystem = [0u16; 32];
        let volume_ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
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
                && drive_type == DRIVE_FIXED
                && volume_ok
                && String::from_utf16_lossy(&filesystem)
                    .trim_end_matches('\0')
                    .eq_ignore_ascii_case("NTFS"),
            "queued Shell Trash test requires a local fixed NTFS volume: {}",
            path.display()
        );
    }

    #[test]
    #[ignore = "modifies the current user's Recycle Bin; probes queued Shell item identity on a disposable fixture"]
    fn ignored_queued_shell_delete_item_identity_after_path_reuse() {
        let root = tempfile::Builder::new()
            .prefix("gpui-filer-trash-queued-item-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        assert_local_fixed_ntfs(root.path());

        let source = root.path().join("fixture.txt");
        let accepted_a = root.path().join("accepted-a.txt");
        let relocated_b = root.path().join("relocated-b.txt");
        fs::write(&source, BYTES_A).unwrap();
        let identity_a =
            EntrySourceIdentity::from_path_no_follow(&source, EntryKind::File).unwrap();
        let mut fixture = QueuedDeleteFixture {
            root_path: root.path().to_path_buf(),
            root: Some(root),
            source: source.clone(),
            accepted_a: accepted_a.clone(),
            relocated_b,
            identity_a: identity_a.clone(),
            identity_b: Arc::new(Mutex::new(None)),
            armed: true,
        };
        let hook_called = Arc::new(AtomicBool::new(false));
        let hook_called_by_shell = hook_called.clone();
        let identity_b_by_shell = fixture.identity_b.clone();
        let hook_source = source.clone();
        let hook_accepted_a = accepted_a.clone();

        let accepted_guard = identity_a.clone();
        let adapter_result =
            move_to_trash_with_queued_item_hook_for_test(&source, &accepted_guard, move || {
                fs::rename(&hook_source, &hook_accepted_a).unwrap();
                assert_eq!(
                    EntrySourceIdentity::from_path_no_follow(&hook_accepted_a, EntryKind::File),
                    Some(identity_a.clone()),
                    "A identity after rename"
                );
                fs::write(&hook_source, BYTES_B).unwrap();
                let identity_b =
                    EntrySourceIdentity::from_path_no_follow(&hook_source, EntryKind::File)
                        .unwrap();
                *identity_b_by_shell
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity_b.clone());
                assert_ne!(
                    identity_a, identity_b,
                    "A and B must be distinct filesystem objects"
                );
                assert_eq!(fs::read(&hook_accepted_a).unwrap(), BYTES_A);
                assert_eq!(fs::read(&hook_source).unwrap(), BYTES_B);
                hook_called_by_shell.store(true, Ordering::SeqCst);
            });
        assert!(
            hook_called.load(Ordering::SeqCst),
            "test hook must run after DeleteItem was queued and before PerformOperations"
        );
        assert!(
            fixture
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "capture B identity at queued boundary"
        );
        assert!(
            matches!(&adapter_result, Err(TrashError::MutationUncertain(_))),
            "reused-path identity mismatch must abort without a success receipt: {adapter_result:?}"
        );

        let observed = match adapter_result {
            Ok(receipt) => {
                fixture
                    .restore_receipt(receipt)
                    .expect("restore exact Shell receipt");
                fixture
                    .restored_identity()
                    .expect("classify restored receipt")
            }
            Err(error) => {
                eprintln!("Shell adapter returned uncertain result: {error:?}");
                let candidates = fixture.matching_candidates().unwrap();
                match candidates.as_slice() {
                    [candidate] => {
                        let restored_origin = candidate.original_path.clone();
                        fixture
                            .restore_candidate(candidate.clone())
                            .expect("restore uniquely matching exact Shell item");
                        fixture
                            .identity_at(&restored_origin)
                            .expect("classify recovered item")
                    }
                    [] => {
                        fixture
                            .verify_both_objects()
                            .expect("both fixture objects remain local when no item is receipted");
                        ObservedIdentity::Uncertain
                    }
                    _ => panic!(
                        "multiple exact fixture Recycle Bin items match fixture origins; recovery guard will preserve the root: {}",
                        candidates
                            .iter()
                            .map(|candidate| format!(
                                "id={} origin={}",
                                candidate.id.to_string_lossy(),
                                candidate.original_path.display()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };

        match observed {
            ObservedIdentity::A => eprintln!(
                "Queued Shell Trash characterization: recovered item was A; B remains in the disposable fixture. No identity custody guarantee is implied."
            ),
            ObservedIdentity::B => eprintln!(
                "Queued Shell Trash characterization: B was restored at P; A remains in the disposable fixture. No identity custody guarantee is implied."
            ),
            ObservedIdentity::Uncertain => eprintln!(
                "Queued Shell Trash characterization: adapter result was uncertain and no exact recycle item identified A or B."
            ),
        }
        fixture
            .verify_both_objects()
            .expect("A and B identities and bytes remain in the disposable fixture");
        fixture.armed = false;
    }

    #[test]
    #[ignore = "modifies the current user's Recycle Bin; probes Shell identity after validated PreDeleteItem on a disposable fixture"]
    fn ignored_pre_delete_callback_path_substitution_identity() {
        let root = tempfile::Builder::new()
            .prefix("gpui-filer-trash-pre-delete-callback-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        assert_local_fixed_ntfs(root.path());

        let source = root.path().join("fixture.txt");
        let accepted_a = root.path().join("accepted-a.txt");
        let relocated_b = root.path().join("relocated-b.txt");
        fs::write(&source, BYTES_A).unwrap();
        let identity_a =
            EntrySourceIdentity::from_path_no_follow(&source, EntryKind::File).unwrap();
        let mut fixture = QueuedDeleteFixture {
            root_path: root.path().to_path_buf(),
            root: Some(root),
            source: source.clone(),
            accepted_a: accepted_a.clone(),
            relocated_b,
            identity_a: identity_a.clone(),
            identity_b: Arc::new(Mutex::new(None)),
            armed: true,
        };

        let (validated_tx, validated_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let thread_source = source.clone();
        let thread_accepted_a = accepted_a.clone();
        let thread_identity_a = identity_a.clone();
        let thread_identity_b = fixture.identity_b.clone();
        let substitution = std::thread::spawn(move || {
            let result = validated_rx.recv().is_ok().then(|| {
                fs::rename(&thread_source, &thread_accepted_a)
                    .map_err(|error| error.to_string())?;
                let identity_at_a =
                    EntrySourceIdentity::from_path_no_follow(&thread_accepted_a, EntryKind::File)
                        .ok_or_else(|| "A identity unavailable after relocation".to_string())?;
                if identity_at_a != thread_identity_a
                    || fs::read(&thread_accepted_a).map_err(|error| error.to_string())? != BYTES_A
                {
                    return Err("accepted A identity or bytes changed".to_string());
                }
                fs::write(&thread_source, BYTES_B).map_err(|error| error.to_string())?;
                let identity_b =
                    EntrySourceIdentity::from_path_no_follow(&thread_source, EntryKind::File)
                        .ok_or_else(|| "B identity unavailable at substituted path".to_string())?;
                if identity_b == thread_identity_a
                    || fs::read(&thread_source).map_err(|error| error.to_string())? != BYTES_B
                {
                    return Err("replacement B was not distinct with expected bytes".to_string());
                }
                *thread_identity_b
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity_b);
                Ok::<(), String>(())
            });
            let succeeded = matches!(&result, Some(Ok(())));
            let _ = continue_tx.send(succeeded);
            result.unwrap_or_else(|| Err("PreDeleteItem validation barrier was not reached".into()))
        });

        let adapter_result =
            move_to_trash_with_post_validation_hook_for_test(&source, &identity_a, move || {
                validated_tx.send(()).is_ok() && continue_rx.recv().unwrap_or(false)
            });
        substitution
            .join()
            .expect("path substitution worker must not panic")
            .expect("path substitution must complete after callback validation");
        assert!(
            fixture
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "replacement B identity must be captured"
        );

        let observed = match adapter_result {
            Ok(receipt) => {
                fixture
                    .restore_receipt(receipt)
                    .expect("restore exact Shell receipt and verify both objects");
                fixture
                    .restored_identity()
                    .expect("classify exact restored callback receipt")
            }
            Err(error) => {
                eprintln!("Shell adapter returned uncertain result: {error:?}");
                let candidates = fixture.matching_candidates().unwrap();
                match candidates.as_slice() {
                    [candidate] => {
                        let original_path = candidate.original_path.clone();
                        fixture
                            .restore_candidate(candidate.clone())
                            .expect("restore uniquely matching exact Shell item");
                        fixture
                            .identity_at(&original_path)
                            .expect("classify uniquely recovered item")
                    }
                    [] => {
                        fixture
                            .verify_both_objects()
                            .expect("verify A and B remain in fixture when no item is receipted");
                        ObservedIdentity::Uncertain
                    }
                    _ => panic!(
                        "multiple exact fixture Recycle Bin items match; preserving root: {}",
                        candidates
                            .iter()
                            .map(|candidate| format!(
                                "id={} origin={}",
                                candidate.id.to_string_lossy(),
                                candidate.original_path.display()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };

        match observed {
            ObservedIdentity::A => eprintln!(
                "PreDeleteItem callback characterization: exact receipt restored A; B remains in the fixture. Host observation only; Shell identity custody is not an API guarantee."
            ),
            ObservedIdentity::B => eprintln!(
                "PreDeleteItem callback characterization: exact receipt restored replacement B at P; accepted A remains at its retained sibling. Host observation only; Shell identity custody is not an API guarantee."
            ),
            ObservedIdentity::Uncertain => eprintln!(
                "PreDeleteItem callback characterization: outcome uncertain; no exact receipt identified A or B. No Shell safety guarantee is inferred."
            ),
        }
        fixture
            .verify_both_objects()
            .expect("verify exact A/B identities and contents after receipt recovery");
        fixture.armed = false;
    }

    #[test]
    #[ignore = "modifies the current user's Recycle Bin; probes queued Shell directory identity on a disposable fixture"]
    fn ignored_queued_shell_directory_identity_after_path_reuse() {
        let root = tempfile::Builder::new()
            .prefix("gpui-filer-trash-queued-directory-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        assert_local_fixed_ntfs(root.path());

        let source = root.path().join("fixture-directory");
        let accepted_a = root.path().join("accepted-directory-a");
        let relocated_b = root.path().join("relocated-directory-b");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("marker.bin"), BYTES_A).unwrap();
        let identity_a =
            EntrySourceIdentity::from_path_no_follow(&source, EntryKind::Folder).unwrap();
        let mut fixture = QueuedDirectoryFixture {
            root_path: root.path().to_path_buf(),
            root: Some(root),
            source: source.clone(),
            accepted_a: accepted_a.clone(),
            relocated_b,
            identity_a: identity_a.clone(),
            identity_b: Arc::new(Mutex::new(None)),
            armed: true,
        };
        let hook_called = Arc::new(AtomicBool::new(false));
        let hook_called_by_shell = hook_called.clone();
        let identity_b_by_shell = fixture.identity_b.clone();
        let hook_source = source.clone();
        let hook_accepted_a = accepted_a.clone();

        let accepted_guard = identity_a.clone();
        let adapter_result =
            move_to_trash_with_queued_item_hook_for_test(&source, &accepted_guard, move || {
                fs::rename(&hook_source, &hook_accepted_a).unwrap();
                assert_eq!(
                    EntrySourceIdentity::from_path_no_follow(&hook_accepted_a, EntryKind::Folder),
                    Some(identity_a.clone()),
                    "directory A identity after rename"
                );
                assert_eq!(
                    QueuedDirectoryFixture::marker(&hook_accepted_a).unwrap(),
                    BYTES_A,
                    "directory A marker after rename"
                );

                fs::create_dir(&hook_source).unwrap();
                fs::write(hook_source.join("marker.bin"), BYTES_B).unwrap();
                let identity_b =
                    EntrySourceIdentity::from_path_no_follow(&hook_source, EntryKind::Folder)
                        .unwrap();
                *identity_b_by_shell
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity_b.clone());
                assert_ne!(identity_a, identity_b, "directory A and B must be distinct");
                assert_eq!(
                    QueuedDirectoryFixture::marker(&hook_source).unwrap(),
                    BYTES_B,
                    "directory B marker at reused path"
                );
                hook_called_by_shell.store(true, Ordering::SeqCst);
            });

        assert!(
            hook_called.load(Ordering::SeqCst),
            "directory test hook must run after DeleteItem queue and before PerformOperations"
        );
        assert!(
            fixture
                .identity_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "capture directory B identity at queued boundary"
        );
        assert!(
            matches!(&adapter_result, Err(TrashError::MutationUncertain(_))),
            "reused directory identity must abort without a success receipt: {adapter_result:?}"
        );

        let observed = match adapter_result {
            Ok(receipt) => {
                fixture
                    .restore_receipt(receipt)
                    .expect("restore exact Shell directory receipt");
                fixture
                    .identity_at_source()
                    .expect("classify exact restored directory")
            }
            Err(error) => {
                eprintln!("Shell directory adapter returned uncertain result: {error:?}");
                let candidates = fixture.candidates().expect("enumerate fixture candidates");
                match candidates.as_slice() {
                    [candidate] => {
                        let restored_origin = candidate.original_path.clone();
                        fixture
                            .restore_candidate(candidate.clone())
                            .expect("restore uniquely matching exact directory item");
                        fixture
                            .identity_at(&restored_origin)
                            .expect("classify recovered directory")
                    }
                    [] => {
                        fixture
                            .verify_both()
                            .expect("both directories remain local without a receipt");
                        ObservedIdentity::Uncertain
                    }
                    _ => panic!(
                        "multiple exact directory candidates; recovery guard preserves fixture: {}",
                        candidates
                            .iter()
                            .map(|candidate| format!(
                                "id={} origin={}",
                                candidate.id.to_string_lossy(),
                                candidate.original_path.display()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };

        eprintln!(
            "Queued Shell directory Trash characterization: observed {observed:?}; this is path-reuse evidence, not a safety guarantee."
        );
        fixture
            .verify_both()
            .expect("both directory identities and marker bytes remain inside fixture");
        fixture.armed = false;
    }
}
