//! Explorer-compatible Windows file clipboard adapter.
//!
//! This is an adapter, not an operation owner: it only exchanges an immutable
//! paste request (`CF_HDROP` plus `Preferred DropEffect`) with other programs.
//! The shell decides whether a read becomes a copy or move job.

use std::path::PathBuf;

/// Explorer's preferred effect values for `Preferred DropEffect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardEffect {
    Copy,
    Move,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFiles {
    pub paths: Vec<PathBuf>,
    pub effect: ClipboardEffect,
}

/// Removes lexically duplicate paths while preserving the first occurrence and order.
pub fn deduplicate_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::with_capacity(paths.len());
    paths
        .iter()
        .filter(|path| seen.insert((*path).clone()))
        .cloned()
        .collect()
}

/// Detached CF_HDROP bytes prepared before the publication gate is acquired.
pub struct PreparedFileClipboard {
    payload: Option<Vec<u8>>,
    effect: ClipboardEffect,
}

/// Clipboard generation captured before background CF_HDROP enumeration.
pub type ClipboardSequence = u32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    Unsupported,
    Platform(String),
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("file clipboard is only supported on Windows"),
            Self::Platform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ClipboardError {}

#[cfg(target_os = "windows")]
pub fn prepare(
    paths: &[PathBuf],
    effect: ClipboardEffect,
) -> Result<PreparedFileClipboard, ClipboardError> {
    Ok(PreparedFileClipboard {
        payload: (!paths.is_empty()).then(|| windows::dropfiles_payload(paths)),
        effect,
    })
}

#[cfg(not(target_os = "windows"))]
pub fn prepare(
    _paths: &[PathBuf],
    _effect: ClipboardEffect,
) -> Result<PreparedFileClipboard, ClipboardError> {
    Err(ClipboardError::Unsupported)
}

#[cfg(target_os = "windows")]
pub fn publish_prepared(
    prepared: PreparedFileClipboard,
    owner: std::ptr::NonNull<std::ffi::c_void>,
) -> Result<(), ClipboardError> {
    let Some(payload) = prepared.payload else {
        // Copying no selection must not erase another application's data.
        return Ok(());
    };
    windows::publish(&payload, prepared.effect, owner)
}

#[cfg(not(target_os = "windows"))]
pub fn publish_prepared(_prepared: PreparedFileClipboard) -> Result<(), ClipboardError> {
    Err(ClipboardError::Unsupported)
}

#[cfg(target_os = "windows")]
pub fn read() -> Result<Option<ClipboardFiles>, ClipboardError> {
    windows::read()
}

#[cfg(target_os = "windows")]
pub fn sequence_number() -> Result<ClipboardSequence, ClipboardError> {
    Ok(windows::sequence_number())
}

/// Checks whether a filesystem-drop clipboard format is available without enumerating paths.
pub fn has_files() -> Result<bool, ClipboardError> {
    #[cfg(target_os = "windows")]
    {
        windows::has_files()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(false)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn sequence_number() -> Result<ClipboardSequence, ClipboardError> {
    Ok(0)
}

/// Reads only if the clipboard still has the generation captured at dispatch.
/// A replacement clipboard cannot retarget delayed background enumeration.
#[cfg(target_os = "windows")]
pub fn read_if_unchanged(
    sequence: ClipboardSequence,
) -> Result<Option<ClipboardFiles>, ClipboardError> {
    windows::read_if_unchanged(sequence)
}

#[cfg(not(target_os = "windows"))]
pub fn read_if_unchanged(
    _sequence: ClipboardSequence,
) -> Result<Option<ClipboardFiles>, ClipboardError> {
    read()
}

#[cfg(not(target_os = "windows"))]
pub fn read() -> Result<Option<ClipboardFiles>, ClipboardError> {
    Ok(None)
}

#[cfg(target_os = "windows")]
mod windows {
    use std::{
        ffi::OsString,
        mem::size_of,
        os::windows::ffi::{OsStrExt as _, OsStringExt as _},
        path::PathBuf,
    };

    use ::windows::{
        Win32::{
            Foundation::{BOOL, GlobalFree, HANDLE, HGLOBAL, HWND},
            System::{
                DataExchange::{
                    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
                    SetClipboardData,
                },
                Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
                Ole::CF_HDROP,
            },
            UI::Shell::{DROPFILES, DragQueryFileW, HDROP},
        },
        core::PCWSTR,
    };

    use super::{ClipboardEffect, ClipboardError, ClipboardFiles};

    const DROPEFFECT_COPY: u32 = 1;
    const DROPEFFECT_MOVE: u32 = 2;

    pub(super) fn dropfiles_payload(paths: &[PathBuf]) -> Vec<u8> {
        let mut names = Vec::new();
        for path in paths {
            names.extend(path.as_os_str().encode_wide());
            names.push(0);
        }
        names.push(0);

        let header_size = size_of::<DROPFILES>();
        let mut payload = vec![0; header_size];
        let p_files = (header_size as u32).to_ne_bytes();
        let point = 0i32.to_ne_bytes();
        let f_nc: BOOL = false.into();
        let f_wide: BOOL = true.into();
        payload[std::mem::offset_of!(DROPFILES, pFiles)..][..p_files.len()]
            .copy_from_slice(&p_files);
        payload[std::mem::offset_of!(DROPFILES, pt.x)..][..point.len()].copy_from_slice(&point);
        payload[std::mem::offset_of!(DROPFILES, pt.y)..][..point.len()].copy_from_slice(&point);
        payload[std::mem::offset_of!(DROPFILES, fNC)..][..size_of::<BOOL>()]
            .copy_from_slice(&f_nc.0.to_ne_bytes());
        payload[std::mem::offset_of!(DROPFILES, fWide)..][..size_of::<BOOL>()]
            .copy_from_slice(&f_wide.0.to_ne_bytes());
        payload.reserve(std::mem::size_of_val(names.as_slice()));
        for name in names {
            payload.extend_from_slice(&name.to_ne_bytes());
        }
        payload
    }

    fn preferred_effect_value(effect: ClipboardEffect) -> u32 {
        match effect {
            ClipboardEffect::Copy => DROPEFFECT_COPY,
            ClipboardEffect::Move => DROPEFFECT_MOVE,
        }
    }

    pub(super) fn publish(
        payload: &[u8],
        effect: ClipboardEffect,
        owner: std::ptr::NonNull<std::ffi::c_void>,
    ) -> Result<(), ClipboardError> {
        let files = FileData::from_payload(payload)?;
        let preferred = EffectData::new(effect)?;
        let format = preferred_format()?;
        unsafe {
            OpenClipboard(HWND(owner.as_ptr())).map_err(platform)?;
        }
        let _clipboard = ClipboardGuard;
        unsafe {
            EmptyClipboard().map_err(platform)?;
        }
        // On success the system owns each HGLOBAL.  Keeping each allocation in
        // an owner until then prevents a pre-clipboard allocation error from
        // leaving a leaked global handle.
        publish_formats(
            || preferred.publish(format),
            || files.publish(CF_HDROP.0 as u32),
        )
    }

    // CF_HDROP is the actionable part of this request, so publish it only
    // after its matching effect has been accepted by the clipboard.
    fn publish_formats(
        publish_effect: impl FnOnce() -> Result<(), ClipboardError>,
        publish_files: impl FnOnce() -> Result<(), ClipboardError>,
    ) -> Result<(), ClipboardError> {
        publish_effect()?;
        publish_files()
    }

    pub(super) fn sequence_number() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    pub(super) fn has_files() -> Result<bool, ClipboardError> {
        unsafe {
            OpenClipboard(HWND::default()).map_err(platform)?;
            let _clipboard = ClipboardGuard;
            Ok(IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok())
        }
    }

    pub(super) fn read() -> Result<Option<ClipboardFiles>, ClipboardError> {
        read_inner(None)
    }

    pub(super) fn read_if_unchanged(
        sequence: u32,
    ) -> Result<Option<ClipboardFiles>, ClipboardError> {
        read_inner(Some(sequence))
    }

    // Clipboard列挙の前後で同じ世代か確かめ、遅延Pasteが置換後の一覧へ振り替わるのを防ぐ。
    fn read_under_sequence_guard<T>(
        expected_sequence: Option<u32>,
        mut current_sequence: impl FnMut() -> u32,
        read: impl FnOnce() -> Result<Option<T>, ClipboardError>,
    ) -> Result<Option<T>, ClipboardError> {
        if expected_sequence.is_some_and(|expected| current_sequence() != expected) {
            return Ok(None);
        }
        let result = read()?;
        if expected_sequence.is_some_and(|expected| current_sequence() != expected) {
            return Ok(None);
        }
        Ok(result)
    }

    fn read_inner(
        expected_sequence: Option<u32>,
    ) -> Result<Option<ClipboardFiles>, ClipboardError> {
        unsafe {
            OpenClipboard(HWND::default()).map_err(platform)?;
        }
        let _clipboard = ClipboardGuard;
        read_under_sequence_guard(expected_sequence, sequence_number, || unsafe {
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
                return Ok(None);
            }
            let handle = GetClipboardData(CF_HDROP.0 as u32).map_err(platform)?;
            let hdrop = HDROP(handle.0);
            let paths = paths_from_hdrop(hdrop)?;
            if paths.is_empty() {
                return Ok(None);
            }
            let files = ClipboardFiles {
                paths,
                effect: read_effect().unwrap_or(ClipboardEffect::Copy),
            };
            Ok(Some(files))
        })
    }

    fn paths_from_hdrop(hdrop: HDROP) -> Result<Vec<PathBuf>, ClipboardError> {
        unsafe {
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            let mut paths = Vec::new();
            reserve_clipboard_capacity(&mut paths, count as usize)?;
            for index in 0..count {
                let length = DragQueryFileW(hdrop, index, None) as usize;
                let capacity = length.checked_add(1).ok_or_else(|| {
                    ClipboardError::Platform("clipboard path length exceeds addressable memory".into())
                })?;
                let mut buffer = Vec::new();
                reserve_clipboard_capacity(&mut buffer, capacity)?;
                buffer.resize(capacity, 0_u16);
                DragQueryFileW(hdrop, index, Some(&mut buffer));
                paths.push(PathBuf::from(OsString::from_wide(&buffer[..length])));
            }
            Ok(paths)
        }
    }

    fn reserve_clipboard_capacity<T>(
        values: &mut Vec<T>,
        capacity: usize,
    ) -> Result<(), ClipboardError> {
        values.try_reserve_exact(capacity).map_err(|_| {
            ClipboardError::Platform("clipboard path list exceeds available memory".into())
        })
    }

    struct FileData(HGLOBAL);

    impl FileData {
        fn from_payload(payload: &[u8]) -> Result<Self, ClipboardError> {
            let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len()).map_err(platform)? };
            let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
            if pointer.is_null() {
                unsafe {
                    let _ = GlobalFree(memory);
                }
                return Err(ClipboardError::Platform(
                    "GlobalLock(CF_HDROP) returned null".into(),
                ));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, payload.len());
                let _ = GlobalUnlock(memory);
            }
            Ok(Self(memory))
        }

        fn publish(mut self, format: u32) -> Result<(), ClipboardError> {
            let handle = self.0;
            unsafe {
                SetClipboardData(format, HANDLE(handle.0)).map_err(platform)?;
            }
            self.0 = HGLOBAL::default();
            Ok(())
        }
    }

    impl Drop for FileData {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = GlobalFree(self.0);
                }
            }
        }
    }

    struct EffectData(FileData);

    impl EffectData {
        fn new(effect: ClipboardEffect) -> Result<Self, ClipboardError> {
            let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of::<u32>()).map_err(platform)? };
            let pointer = unsafe { GlobalLock(memory) }.cast::<u32>();
            if pointer.is_null() {
                unsafe {
                    let _ = GlobalFree(memory);
                }
                return Err(ClipboardError::Platform(
                    "GlobalLock(Preferred DropEffect) returned null".into(),
                ));
            }
            unsafe {
                *pointer = preferred_effect_value(effect);
                let _ = GlobalUnlock(memory);
            }
            Ok(Self(FileData(memory)))
        }

        fn publish(self, format: u32) -> Result<(), ClipboardError> {
            self.0.publish(format)
        }
    }

    fn preferred_format() -> Result<u32, ClipboardError> {
        let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
        let format = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
        (format != 0)
            .then_some(format)
            .ok_or_else(|| ClipboardError::Platform("RegisterClipboardFormatW failed".into()))
    }

    fn read_effect() -> Option<ClipboardEffect> {
        let format = preferred_format().ok()?;
        let handle = unsafe { GetClipboardData(format).ok()? };
        let memory = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(memory) };
        read_preferred_effect(size, || {
            let pointer = unsafe { GlobalLock(memory) }.cast::<u32>();
            if pointer.is_null() {
                return None;
            }
            let _unlock = GlobalMemoryUnlockGuard(memory);
            Some(unsafe { *pointer })
        })
    }

    // 外部clipboardのHGLOBALは、DWORDを読む前に確保範囲を確認する。
    fn read_preferred_effect(
        size: usize,
        read_value: impl FnOnce() -> Option<u32>,
    ) -> Option<ClipboardEffect> {
        if size < size_of::<u32>() {
            return None;
        }
        read_value().map(|value| match value {
            DROPEFFECT_MOVE => ClipboardEffect::Move,
            _ => ClipboardEffect::Copy,
        })
    }

    struct GlobalMemoryUnlockGuard(HGLOBAL);

    impl Drop for GlobalMemoryUnlockGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = GlobalUnlock(self.0);
            }
        }
    }

    fn platform(error: impl std::fmt::Display) -> ClipboardError {
        ClipboardError::Platform(error.to_string())
    }

    struct ClipboardGuard;

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn preferred_drop_effect_rejects_hglobal_smaller_than_a_dword_before_locking() {
            for size in 0..size_of::<u32>() {
                let mut locked = false;
                assert_eq!(
                    read_preferred_effect(size, || {
                        locked = true;
                        Some(DROPEFFECT_MOVE)
                    }),
                    None,
                    "a truncated Preferred DropEffect is ignored"
                );
                assert!(!locked, "short HGLOBAL is rejected before GlobalLock");
            }

            assert_eq!(
                read_preferred_effect(size_of::<u32>(), || Some(DROPEFFECT_MOVE)),
                Some(ClipboardEffect::Move)
            );
            assert_eq!(
                read_preferred_effect(size_of::<u32>(), || Some(DROPEFFECT_COPY)),
                Some(ClipboardEffect::Copy)
            );
        }

        #[test]
        fn hdrop_capacity_overflow_returns_an_error_without_panicking() {
            let mut paths = Vec::<PathBuf>::new();
            assert!(reserve_clipboard_capacity(&mut paths, usize::MAX).is_err());
            assert!(paths.is_empty());
        }

        #[test]
        fn dropfiles_payload_has_wide_multistring_header_and_unicode_paths() {
            let paths = [
                PathBuf::from(r"C:\files\parent"),
                PathBuf::from(r"C:\files\parent\child.txt"),
                PathBuf::from(r"C:\files\parent"),
                PathBuf::from("C:\\日本語\\画像📁.png"),
            ];
            let payload = dropfiles_payload(&paths);
            let header_size = size_of::<DROPFILES>();
            let header = unsafe { payload.as_ptr().cast::<DROPFILES>().read_unaligned() };
            let p_files = header.pFiles;
            let (x, y) = (header.pt.x, header.pt.y);
            let f_nc = header.fNC.0;
            let f_wide = header.fWide.0;

            assert_eq!(p_files as usize, header_size);
            assert_eq!((x, y), (0, 0));
            assert_eq!(f_nc, 0);
            assert_eq!(f_wide, 1);

            let names = payload[header_size..]
                .chunks_exact(size_of::<u16>())
                .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
                .collect::<Vec<_>>();
            let mut expected = Vec::new();
            for path in &paths {
                expected.extend(path.as_os_str().encode_wide());
                expected.push(0);
            }
            expected.push(0);
            assert_eq!(names, expected);
            assert_eq!(&names[names.len() - 2..], &[0, 0]);

            let data = FileData::from_payload(&payload).unwrap();
            let hdrop = HDROP(data.0.0);
            assert_eq!(paths_from_hdrop(hdrop).unwrap(), paths);
        }

        #[test]
        fn dropfiles_move_sources_reach_shell_preparation_in_order_after_first_wins_deduplication()
        {
            let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let parent = directory.path().join("parent");
            let child = parent.join("child.txt");
            let unicode = directory.path().join("日本語-📁.txt");
            let destination = directory.path().join("destination");
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&child, b"child bytes").unwrap();
            std::fs::write(&unicode, b"unicode bytes").unwrap();
            std::fs::create_dir(&destination).unwrap();

            let supplied_paths = vec![
                parent.clone(),
                child.clone(),
                parent.clone(),
                unicode.clone(),
            ];
            let payload = dropfiles_payload(&supplied_paths);
            let data = FileData::from_payload(&payload).unwrap();
            let decoded_paths = paths_from_hdrop(HDROP(data.0.0)).unwrap();
            assert_eq!(decoded_paths, supplied_paths);

            let destination_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
                &destination,
                crate::directory::EntryKind::Folder,
            )
            .unwrap()
            .filesystem_identity();
            let (files, replace_evidence, move_evidence) =
                crate::shell::FilerShell::prepare_clipboard_file_request(
                    ClipboardFiles {
                        paths: decoded_paths,
                        effect: ClipboardEffect::Move,
                    },
                    &destination_identity,
                )
                .unwrap();

            assert_eq!(
                files.paths,
                vec![parent.clone(), child.clone(), unicode.clone()]
            );
            assert!(replace_evidence.is_none());
            let move_evidence = move_evidence.unwrap();
            assert_eq!(
                move_evidence
                    .iter()
                    .map(|evidence| evidence.source.clone())
                    .collect::<Vec<_>>(),
                files.paths
            );
            assert_eq!(
                std::fs::read(&child).unwrap(),
                b"child bytes",
                "capturing Move evidence does not modify clipboard sources"
            );
        }

        #[test]
        fn clipboard_sequence_guard_discards_stale_snapshots_before_or_during_read() {
            let mut stale_before = [8_u32].into_iter();
            let mut read_called = false;
            let result = read_under_sequence_guard(
                Some(7),
                || stale_before.next().unwrap(),
                || {
                    read_called = true;
                    Ok(Some("stale"))
                },
            )
            .unwrap();
            assert_eq!(result, None);
            assert!(!read_called);

            let mut changes_during_read = [7_u32, 8].into_iter();
            let result = read_under_sequence_guard(
                Some(7),
                || changes_during_read.next().unwrap(),
                || Ok(Some("stale")),
            )
            .unwrap();
            assert_eq!(result, None);

            let mut unchanged = [7_u32, 7].into_iter();
            let result = read_under_sequence_guard(
                Some(7),
                || unchanged.next().unwrap(),
                || Ok(Some("current")),
            )
            .unwrap();
            assert_eq!(result, Some("current"));
        }

        #[test]
        fn preferred_drop_effect_encodes_copy_and_move_dwords() {
            assert_eq!(
                preferred_effect_value(ClipboardEffect::Copy).to_le_bytes(),
                [1, 0, 0, 0]
            );
            assert_eq!(
                preferred_effect_value(ClipboardEffect::Move).to_le_bytes(),
                [2, 0, 0, 0]
            );
        }

        #[test]
        fn publication_failures_never_leave_actionable_files_and_release_untransferred_data() {
            use std::cell::RefCell;
            use std::rc::Rc;

            #[derive(Debug, Default)]
            struct ClipboardModel {
                formats: Vec<&'static str>,
                transferred: usize,
                freed: usize,
            }

            struct PreparedHandle {
                model: Rc<RefCell<ClipboardModel>>,
                transferred: bool,
            }
            impl PreparedHandle {
                fn publish(
                    mut self,
                    format: &'static str,
                    fail: bool,
                ) -> Result<(), ClipboardError> {
                    if fail {
                        return Err(ClipboardError::Platform(format!("failed {format}")));
                    }
                    {
                        let mut model = self.model.borrow_mut();
                        model.formats.push(format);
                        model.transferred += 1;
                    }
                    self.transferred = true;
                    Ok(())
                }
            }
            impl Drop for PreparedHandle {
                fn drop(&mut self) {
                    if !self.transferred {
                        self.model.borrow_mut().freed += 1;
                    }
                }
            }

            fn run(
                effect_fails: bool,
                files_fails: bool,
            ) -> (Result<(), ClipboardError>, ClipboardModel) {
                let model = Rc::new(RefCell::new(ClipboardModel::default()));
                let effect = PreparedHandle {
                    model: Rc::clone(&model),
                    transferred: false,
                };
                let files = PreparedHandle {
                    model: Rc::clone(&model),
                    transferred: false,
                };
                let result = publish_formats(
                    || effect.publish("Preferred DropEffect", effect_fails),
                    || files.publish("CF_HDROP", files_fails),
                );
                let model = Rc::try_unwrap(model).unwrap().into_inner();
                (result, model)
            }

            let (result, first_failure) = run(true, false);
            assert!(result.is_err());
            assert!(first_failure.formats.is_empty());
            assert_eq!((first_failure.transferred, first_failure.freed), (0, 2));

            let (result, second_failure) = run(false, true);
            assert!(result.is_err());
            assert_eq!(second_failure.formats, ["Preferred DropEffect"]);
            assert_eq!((second_failure.transferred, second_failure.freed), (1, 1));
            assert!(!second_failure.formats.contains(&"CF_HDROP"));

            let (result, success) = run(false, false);
            assert!(result.is_ok());
            assert_eq!(success.formats, ["Preferred DropEffect", "CF_HDROP"]);
            assert_eq!((success.transferred, success.freed), (2, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_paths_keep_the_first_occurrence_and_request_order() {
        let first = PathBuf::from(r"C:\source\first.txt");
        let second = PathBuf::from(r"C:\source\second.txt");
        let third = PathBuf::from(r"C:\source\third.txt");

        assert_eq!(
            deduplicate_paths(&[
                first.clone(),
                second.clone(),
                first.clone(),
                third.clone(),
                second.clone(),
            ]),
            vec![first, second, third],
        );
    }

    #[test]
    fn effects_are_explicit() {
        assert_ne!(ClipboardEffect::Copy, ClipboardEffect::Move);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_read_is_empty_and_write_is_rejected() {
        assert_eq!(read().unwrap(), None);
        assert_eq!(
            copy(&[PathBuf::from("a")]),
            Err(ClipboardError::Unsupported)
        );
    }
}
