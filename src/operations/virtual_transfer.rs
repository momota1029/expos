//! Native, process-local sessions for pathless Shell clipboard files.
//!
//! The dedicated STA is the only thread that owns the Shell `IDataObject` and
//! its `IStream`. It exposes bounded reads by token; it never knows where the
//! bytes will be published or whether the operation is cancelled.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransferToken(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualFileSource {
    pub token: TransferToken,
    pub name: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualFileSources {
    pub items: Vec<VirtualFileSource>,
}

impl VirtualFileSources {
    pub fn new(items: Vec<VirtualFileSource>) -> Option<Self> {
        (!items.is_empty()).then_some(Self { items })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFile {
    pub sources: Vec<VirtualFileSource>,
    pub effect: super::file_clipboard::ClipboardEffect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    Unsupported,
    InvalidData(String),
    Platform(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                f.write_str("virtual Shell clipboard transfer is only supported on Windows")
            }
            Self::InvalidData(message) | Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for TransferError {}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{OnceLock, mpsc},
        thread,
        time::Duration,
    };
    use windows_062::{
        Win32::{
            Foundation::HGLOBAL,
            System::{
                Com::{
                    DVASPECT_CONTENT, FORMATETC, IDataObject, IStream, STGMEDIUM, TYMED_HGLOBAL,
                    TYMED_ISTREAM,
                },
                DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW},
                Memory::{GlobalLock, GlobalSize, GlobalUnlock},
                Ole::{OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium},
            },
            UI::WindowsAndMessaging::{
                DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
            },
        },
        core::PCWSTR,
    };

    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    static STA: OnceLock<mpsc::Sender<Command>> = OnceLock::new();

    enum Command {
        Capture(
            u32,
            mpsc::Sender<Result<Option<CapturedFile>, TransferError>>,
        ),
        Read(
            TransferToken,
            usize,
            mpsc::Sender<Result<Vec<u8>, TransferError>>,
        ),
        Release(TransferToken),
        #[cfg(test)]
        InstallFake {
            name: String,
            size: Option<u64>,
            chunks: std::collections::VecDeque<Result<Vec<u8>, String>>,
            #[cfg(test)]
            block: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
            reply: mpsc::Sender<VirtualFileSource>,
        },
    }

    // STGMEDIUM contains a raw union and does not release its contents when
    // Rust drops the carrier. Keep it before IDataObject so it is released
    // first when the session leaves the owning STA.
    struct OwnedMedium(STGMEDIUM);

    impl Drop for OwnedMedium {
        fn drop(&mut self) {
            unsafe { ReleaseStgMedium(&mut self.0) };
        }
    }

    struct Session {
        stream: ContentStream,
        _contents_medium: Option<OwnedMedium>,
        _data_object: Option<IDataObject>,
    }

    enum ContentStream {
        Native(IStream),
        #[cfg(test)]
        Fake(FakeStream),
    }

    #[cfg(test)]
    struct FakeStream {
        chunks: std::collections::VecDeque<Result<Vec<u8>, String>>,
        block: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }

    fn sender() -> &'static mpsc::Sender<Command> {
        STA.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            thread::Builder::new()
                .name("gpui-filer-virtual-clipboard-sta".into())
                .spawn(move || run_sta(rx))
                .expect("start virtual clipboard STA");
            tx
        })
    }

    fn run_sta(receiver: mpsc::Receiver<Command>) {
        let initialized = unsafe { OleInitialize(None) }.is_ok();
        let mut sessions = HashMap::<TransferToken, Session>::new();
        loop {
            pump_messages();
            let command = match receiver.try_recv() {
                Ok(command) => command,
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            match command {
                Command::Capture(sequence, reply) => {
                    let result = if initialized {
                        capture_on_sta(sequence, &mut sessions)
                    } else {
                        Err(TransferError::Platform(
                            "could not initialize virtual transfer STA".into(),
                        ))
                    };
                    if let Err(result) = reply.send(result) {
                        if let Ok(Some(captured)) = result.0 {
                            for source in captured.sources {
                                sessions.remove(&source.token);
                            }
                        }
                    }
                }
                Command::Read(token, maximum, reply) => {
                    let result = if initialized {
                        read(&mut sessions, token, maximum)
                    } else {
                        Err(TransferError::Platform(
                            "virtual transfer STA is unavailable".into(),
                        ))
                    };
                    let _ = reply.send(result);
                }
                Command::Release(token) => {
                    sessions.remove(&token);
                }
                #[cfg(test)]
                Command::InstallFake {
                    name,
                    size,
                    chunks,
                    block,
                    reply,
                } => {
                    let token = TransferToken(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
                    sessions.insert(
                        token,
                        Session {
                            stream: ContentStream::Fake(FakeStream { chunks, block }),
                            _data_object: None,
                            _contents_medium: None,
                        },
                    );
                    let _ = reply.send(VirtualFileSource { token, name, size });
                }
            }
        }
        drop(sessions);
        if initialized {
            unsafe { OleUninitialize() };
        }
    }

    fn pump_messages() {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn capture_on_sta(
        sequence: u32,
        sessions: &mut HashMap<TransferToken, Session>,
    ) -> Result<Option<CapturedFile>, TransferError> {
        if unsafe { GetClipboardSequenceNumber() } != sequence {
            return Ok(None);
        }
        let data = unsafe { OleGetClipboard() }.map_err(platform_error)?;
        capture_from_data_object(sequence, sessions, data)
    }

    fn capture_from_data_object(
        sequence: u32,
        sessions: &mut HashMap<TransferToken, Session>,
        data: IDataObject,
    ) -> Result<Option<CapturedFile>, TransferError> {
        capture_from_data_object_with_sequence(sequence, sessions, data, || unsafe {
            GetClipboardSequenceNumber()
        })
    }

    fn capture_from_data_object_with_sequence(
        sequence: u32,
        sessions: &mut HashMap<TransferToken, Session>,
        data: IDataObject,
        mut current_sequence: impl FnMut() -> u32,
    ) -> Result<Option<CapturedFile>, TransferError> {
        if current_sequence() != sequence {
            return Ok(None);
        }
        let descriptor_format = register("FileGroupDescriptorW")?;
        let contents_format = register("FileContents")?;
        let descriptor = get_medium(&data, descriptor_format, -1, TYMED_HGLOBAL.0 as u32)?;
        if descriptor.tymed != TYMED_HGLOBAL.0 as u32 {
            unsafe { ReleaseStgMedium((&descriptor as *const STGMEDIUM).cast_mut()) };
            return Err(TransferError::InvalidData(
                "FILEGROUPDESCRIPTORW must use HGLOBAL".into(),
            ));
        }
        let descriptor_data = unsafe { descriptor.u.hGlobal };
        let descriptor_result = unsafe { parse_descriptor(descriptor_data) };
        let mut descriptor = descriptor;
        unsafe { ReleaseStgMedium(&mut descriptor) };
        let descriptors = descriptor_result?;

        let preferred_format = register("Preferred DropEffect")?;
        let effect_value = match get_medium(&data, preferred_format, -1, TYMED_HGLOBAL.0 as u32) {
            Ok(medium) if medium.tymed == TYMED_HGLOBAL.0 as u32 => {
                let handle = unsafe { medium.u.hGlobal };
                let value = unsafe { read_u32(handle) };
                let mut medium = medium;
                unsafe { ReleaseStgMedium(&mut medium) };
                let value = value.ok_or_else(|| {
                    TransferError::InvalidData("malformed Preferred DropEffect".into())
                })?;
                Some(value)
            }
            Ok(mut medium) => {
                unsafe { ReleaseStgMedium(&mut medium) };
                return Err(TransferError::InvalidData(
                    "Preferred DropEffect must use HGLOBAL".into(),
                ));
            }
            Err(_) => None,
        };
        let effect = preferred_copy_effect(effect_value)?;

        // 全descriptorのmediumを一時保持し、sequence確定後にだけsession tokenを公開する。
        let mut acquired = Vec::with_capacity(descriptors.len());
        for (index, (name, size)) in descriptors.into_iter().enumerate() {
            let medium = get_medium(&data, contents_format, index as i32, TYMED_ISTREAM.0 as u32)?;
            if medium.tymed != TYMED_ISTREAM.0 as u32 {
                let mut medium = medium;
                unsafe { ReleaseStgMedium(&mut medium) };
                return Err(TransferError::InvalidData(
                    "FILECONTENTS must provide TYMED_ISTREAM".into(),
                ));
            }
            let stream = unsafe { medium.u.pstm.as_ref() }.cloned();
            let Some(stream) = stream else {
                let mut medium = medium;
                unsafe { ReleaseStgMedium(&mut medium) };
                return Err(TransferError::InvalidData(
                    "FILECONTENTS returned a null stream".into(),
                ));
            };
            acquired.push((name, size, stream, OwnedMedium(medium)));
        }
        if current_sequence() != sequence {
            return Ok(None);
        }
        let sources = acquired
            .into_iter()
            .map(|(name, size, stream, medium)| {
                let token = TransferToken(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
                sessions.insert(
                    token,
                    Session {
                        stream: ContentStream::Native(stream),
                        _data_object: Some(data.clone()),
                        _contents_medium: Some(medium),
                    },
                );
                VirtualFileSource { token, name, size }
            })
            .collect();
        Ok(Some(CapturedFile { sources, effect }))
    }

    unsafe fn parse_descriptor(
        handle: HGLOBAL,
    ) -> Result<Vec<(String, Option<u64>)>, TransferError> {
        let size = unsafe { GlobalSize(handle) };
        if size < 4 + 592 {
            return Err(TransferError::InvalidData(
                "truncated FILEGROUPDESCRIPTORW".into(),
            ));
        }
        let pointer = unsafe { GlobalLock(handle) }.cast::<u8>();
        if pointer.is_null() {
            return Err(TransferError::InvalidData(
                "could not lock FILEGROUPDESCRIPTORW".into(),
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer, size) };
        let result = parse_descriptor_bytes(bytes);
        let _ = unsafe { GlobalUnlock(handle) };
        result
    }

    fn parse_descriptor_bytes(bytes: &[u8]) -> Result<Vec<(String, Option<u64>)>, TransferError> {
        const FILEDESCRIPTORW_SIZE: usize = 592;
        const FD_ATTRIBUTES: u32 = 0x0000_0004;
        const FD_FILESIZE: u32 = 0x0000_0040;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if bytes.len() < 4 + FILEDESCRIPTORW_SIZE {
            return Err(TransferError::InvalidData(
                "truncated FILEGROUPDESCRIPTORW".into(),
            ));
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if count == 0 || count > 1024 {
            return Err(TransferError::InvalidData(
                "invalid virtual clipboard file count".into(),
            ));
        }
        let byte_count = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(FILEDESCRIPTORW_SIZE))
            .and_then(|count| count.checked_add(4))
            .filter(|count| *count <= bytes.len())
            .ok_or_else(|| TransferError::InvalidData("truncated FILEGROUPDESCRIPTORW".into()))?;
        let mut parsed = Vec::with_capacity(count as usize);
        for descriptor in bytes[4..byte_count].chunks_exact(FILEDESCRIPTORW_SIZE) {
            let flags = u32::from_le_bytes(descriptor[0..4].try_into().unwrap());
            let attributes = u32::from_le_bytes(descriptor[36..40].try_into().unwrap());
            if flags & FD_ATTRIBUTES != 0
                && attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return Err(TransferError::InvalidData(
                    "virtual clipboard item claims to be a directory or reparse point".into(),
                ));
            }
            let mut units = [0_u16; 260];
            for (index, pair) in descriptor[72..592].chunks_exact(2).enumerate() {
                units[index] = u16::from_le_bytes([pair[0], pair[1]]);
                if units[index] == 0 {
                    break;
                }
            }
            let end = units.iter().position(|unit| *unit == 0).ok_or_else(|| {
                TransferError::InvalidData("unterminated virtual filename".into())
            })?;
            let name = String::from_utf16(&units[..end]).map_err(|_| {
                TransferError::InvalidData("invalid UTF-16 virtual filename".into())
            })?;
            if !valid_leaf_name(&name) {
                return Err(TransferError::InvalidData(
                    "virtual filename is not a valid ordinary leaf name".into(),
                ));
            }
            let file_size = u64::from(u32::from_le_bytes(descriptor[64..68].try_into().unwrap()))
                << 32
                | u64::from(u32::from_le_bytes(descriptor[68..72].try_into().unwrap()));
            parsed.push((name, (flags & FD_FILESIZE != 0).then_some(file_size)));
        }
        Ok(parsed)
    }

    fn preferred_copy_effect(
        value: Option<u32>,
    ) -> Result<super::super::file_clipboard::ClipboardEffect, TransferError> {
        let Some(value) = value else {
            return Ok(super::super::file_clipboard::ClipboardEffect::Copy);
        };
        if value & 2 != 0 {
            return Err(TransferError::InvalidData(
                "virtual clipboard Move is not supported".into(),
            ));
        }
        if value & 1 == 0 {
            return Err(TransferError::InvalidData(
                "Preferred DropEffect does not request Copy".into(),
            ));
        }
        Ok(super::super::file_clipboard::ClipboardEffect::Copy)
    }

    fn valid_leaf_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.ends_with([' ', '.'])
            && !name
                .chars()
                .any(|ch| ch < ' ' || "<>:\"/\\|?*".contains(ch))
            && super::super::rename::is_single_name(name)
    }

    unsafe fn read_u32(handle: HGLOBAL) -> Option<u32> {
        if unsafe { GlobalSize(handle) } < 4 {
            return None;
        }
        let pointer = unsafe { GlobalLock(handle) }.cast::<u32>();
        if pointer.is_null() {
            return None;
        }
        let value = unsafe { *pointer };
        let _ = unsafe { GlobalUnlock(handle) };
        Some(value)
    }

    fn register(name: &str) -> Result<u16, TransferError> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
        (id != 0).then_some(id as u16).ok_or_else(|| {
            TransferError::Platform(format!("could not register clipboard format {name}"))
        })
    }

    fn get_medium(
        data: &windows_062::Win32::System::Com::IDataObject,
        format: u16,
        index: i32,
        tymed: u32,
    ) -> Result<STGMEDIUM, TransferError> {
        let request = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index,
            tymed,
        };
        unsafe { data.GetData(&request) }.map_err(platform_error)
    }

    fn read(
        sessions: &mut HashMap<TransferToken, Session>,
        token: TransferToken,
        maximum: usize,
    ) -> Result<Vec<u8>, TransferError> {
        let session = sessions
            .get_mut(&token)
            .ok_or_else(|| TransferError::InvalidData("virtual transfer session expired".into()))?;
        let mut buffer = vec![0; maximum.min(64 * 1024)];
        let mut read = 0_u32;
        match &mut session.stream {
            ContentStream::Native(stream) => unsafe {
                stream.Read(
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    Some(&mut read),
                )
            }
            .ok()
            .map_err(platform_error)?,
            #[cfg(test)]
            ContentStream::Fake(fake) => {
                if let Some((started, release)) = fake.block.take() {
                    let _ = started.send(());
                    let _ = release.recv();
                }
                let next = fake
                    .chunks
                    .pop_front()
                    .transpose()
                    .map_err(TransferError::Platform)?;
                let Some(bytes) = next else {
                    return Ok(Vec::new());
                };
                let copied = bytes.len().min(buffer.len());
                buffer[..copied].copy_from_slice(&bytes[..copied]);
                if copied < bytes.len() {
                    fake.chunks.push_front(Ok(bytes[copied..].to_vec()));
                }
                buffer.truncate(copied);
                return Ok(buffer);
            }
        }
        buffer.truncate(read as usize);
        Ok(buffer)
    }

    fn platform_error(error: windows_062::core::Error) -> TransferError {
        TransferError::Platform(error.to_string())
    }

    pub(super) fn capture(sequence: u32) -> Result<Option<CapturedFile>, TransferError> {
        let (tx, rx) = mpsc::channel();
        sender()
            .send(Command::Capture(sequence, tx))
            .map_err(|_| TransferError::Platform("virtual transfer STA stopped".into()))?;
        rx.recv()
            .map_err(|_| TransferError::Platform("virtual transfer STA stopped".into()))?
    }

    pub(super) fn read_chunk(
        token: TransferToken,
        maximum: usize,
    ) -> Result<Vec<u8>, TransferError> {
        let (tx, rx) = mpsc::channel();
        sender()
            .send(Command::Read(token, maximum, tx))
            .map_err(|_| TransferError::Platform("virtual transfer STA stopped".into()))?;
        rx.recv()
            .map_err(|_| TransferError::Platform("virtual transfer STA stopped".into()))?
    }

    pub(super) fn release(token: TransferToken) {
        let _ = sender().send(Command::Release(token));
    }

    #[cfg(test)]
    pub(super) fn install_fake(
        name: String,
        size: Option<u64>,
        chunks: std::collections::VecDeque<Result<Vec<u8>, String>>,
    ) -> VirtualFileSource {
        let (tx, rx) = mpsc::channel();
        sender()
            .send(Command::InstallFake {
                name,
                size,
                chunks,
                block: None,
                reply: tx,
            })
            .unwrap();
        rx.recv().unwrap()
    }

    #[cfg(test)]
    pub(super) fn install_blocked_fake(
        name: String,
        size: Option<u64>,
        chunks: std::collections::VecDeque<Result<Vec<u8>, String>>,
    ) -> (VirtualFileSource, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        sender()
            .send(Command::InstallFake {
                name,
                size,
                chunks,
                block: Some((started_tx, release_rx)),
                reply: reply_tx,
            })
            .unwrap();
        (reply_rx.recv().unwrap(), started_rx, release_tx)
    }

    #[cfg(test)]
    #[test]
    fn captures_a_single_file_from_an_i_data_object_on_an_ole_sta() {
        use std::mem::ManuallyDrop;
        use windows_062::{
            Win32::{
                Foundation::{DV_E_FORMATETC, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED},
                System::{
                    Com::{
                        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
                        IStream, STGMEDIUM, STGMEDIUM_0, StructuredStorage::CreateStreamOnHGlobal,
                        TYMED_HGLOBAL, TYMED_ISTREAM,
                    },
                    DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW},
                    Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
                    Ole::{OleInitialize, OleUninitialize},
                },
            },
            core::{BOOL, Error, PCWSTR, Ref, implement},
        };

        struct OleApartmentGuard;

        impl Drop for OleApartmentGuard {
            fn drop(&mut self) {
                unsafe { OleUninitialize() };
            }
        }

        #[implement(IDataObject)]
        struct VirtualFileDataObject {
            descriptor_format: u16,
            contents_format: u16,
            stream: IStream,
        }

        impl VirtualFileDataObject {
            fn format(name: &str) -> u16 {
                let wide = name.encode_utf16().chain([0]).collect::<Vec<_>>();
                unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) as u16 }
            }

            fn descriptor() -> HGLOBAL {
                const DESCRIPTOR_SIZE: usize = 592;
                let mut bytes = vec![0u8; 4 + DESCRIPTOR_SIZE];
                bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
                let item = &mut bytes[4..];
                item[0..4].copy_from_slice(&(0x4u32 | 0x40).to_le_bytes());
                item[36..40].copy_from_slice(&0x80u32.to_le_bytes());
                item[68..72].copy_from_slice(&5u32.to_le_bytes());
                for (index, unit) in "sample.txt".encode_utf16().enumerate() {
                    let offset = 72 + index * 2;
                    item[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
                }

                let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
                    .expect("allocate descriptor HGLOBAL");
                let pointer = unsafe { GlobalLock(handle) }.cast::<u8>();
                assert!(!pointer.is_null());
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
                let _ = unsafe { GlobalUnlock(handle) };
                handle
            }

            fn new(contents: &[u8]) -> Self {
                let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, contents.len()) }
                    .expect("allocate stream HGLOBAL");
                let pointer = unsafe { GlobalLock(handle) }.cast::<u8>();
                assert!(!pointer.is_null());
                unsafe {
                    std::ptr::copy_nonoverlapping(contents.as_ptr(), pointer, contents.len())
                };
                let _ = unsafe { GlobalUnlock(handle) };
                let stream = unsafe { CreateStreamOnHGlobal(handle, true) }
                    .expect("create stream for FILECONTENTS");
                Self {
                    descriptor_format: Self::format("FileGroupDescriptorW"),
                    contents_format: Self::format("FileContents"),
                    stream,
                }
            }
        }

        impl IDataObject_Impl for VirtualFileDataObject_Impl {
            fn GetData(
                &self,
                request: *const windows_062::Win32::System::Com::FORMATETC,
            ) -> windows_062::core::Result<STGMEDIUM> {
                if request.is_null() {
                    return Err(Error::from(DV_E_FORMATETC));
                }
                let request = unsafe { &*request };
                if request.cfFormat == self.descriptor_format
                    && request.lindex == -1
                    && request.tymed & TYMED_HGLOBAL.0 as u32 != 0
                {
                    return Ok(STGMEDIUM {
                        tymed: TYMED_HGLOBAL.0 as u32,
                        u: STGMEDIUM_0 {
                            hGlobal: VirtualFileDataObject::descriptor(),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }
                if request.cfFormat == self.contents_format
                    && request.lindex == 0
                    && request.tymed & TYMED_ISTREAM.0 as u32 != 0
                {
                    return Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 {
                            pstm: ManuallyDrop::new(Some(self.stream.clone())),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }
                Err(Error::from(DV_E_FORMATETC))
            }

            fn GetDataHere(
                &self,
                _request: *const windows_062::Win32::System::Com::FORMATETC,
                _medium: *mut STGMEDIUM,
            ) -> windows_062::core::Result<()> {
                Err(Error::from(E_NOTIMPL))
            }

            fn QueryGetData(
                &self,
                _request: *const windows_062::Win32::System::Com::FORMATETC,
            ) -> windows_062::core::HRESULT {
                E_NOTIMPL
            }

            fn GetCanonicalFormatEtc(
                &self,
                _input: *const windows_062::Win32::System::Com::FORMATETC,
                output: *mut windows_062::Win32::System::Com::FORMATETC,
            ) -> windows_062::core::HRESULT {
                if !output.is_null() {
                    unsafe { (*output).ptd = std::ptr::null_mut() };
                }
                E_NOTIMPL
            }

            fn SetData(
                &self,
                _request: *const windows_062::Win32::System::Com::FORMATETC,
                _medium: *const STGMEDIUM,
                _release: BOOL,
            ) -> windows_062::core::Result<()> {
                Err(Error::from(E_NOTIMPL))
            }

            fn EnumFormatEtc(&self, _direction: u32) -> windows_062::core::Result<IEnumFORMATETC> {
                Err(Error::from(E_NOTIMPL))
            }

            fn DAdvise(
                &self,
                _request: *const windows_062::Win32::System::Com::FORMATETC,
                _flags: u32,
                _sink: Ref<'_, IAdviseSink>,
            ) -> windows_062::core::Result<u32> {
                Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
            }

            fn DUnadvise(&self, _connection: u32) -> windows_062::core::Result<()> {
                Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
            }

            fn EnumDAdvise(&self) -> windows_062::core::Result<IEnumSTATDATA> {
                Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
            }
        }

        assert!(unsafe { OleInitialize(None) }.is_ok());
        let _ole_apartment = OleApartmentGuard;
        let current_sequence = unsafe { GetClipboardSequenceNumber() };
        let mut sessions = HashMap::new();
        // 注入したCOM objectのテストをOS clipboardの変化から切り離し、sequenceの境界だけを検証する。
        let stale_object: IDataObject = VirtualFileDataObject::new(b"stale").into();
        let mut sequence_reads = 0;
        assert!(
            capture_from_data_object_with_sequence(
                current_sequence,
                &mut sessions,
                stale_object,
                || {
                    sequence_reads += 1;
                    if sequence_reads == 1 {
                        current_sequence
                    } else {
                        current_sequence.wrapping_add(1)
                    }
                },
            )
            .unwrap()
            .is_none()
        );

        let data_object: IDataObject = VirtualFileDataObject::new(b"hello").into();
        let captured = capture_from_data_object_with_sequence(
            current_sequence,
            &mut sessions,
            data_object,
            || current_sequence,
        )
        .expect("capture fake Shell IDataObject")
        .expect("clipboard sequence remains current");
        let source = &captured.sources[0];
        assert_eq!(source.name, "sample.txt");
        assert_eq!(source.size, Some(5));
        assert_eq!(read(&mut sessions, source.token, 16).unwrap(), b"hello");
        assert!(read(&mut sessions, source.token, 16).unwrap().is_empty());
        sessions.remove(&source.token);
    }

    #[cfg(test)]
    #[test]
    fn descriptor_parser_accepts_one_ordinary_file_and_rejects_invalid_claims() {
        fn descriptor(name: &str, count: u32, attributes: u32) -> Vec<u8> {
            let mut bytes = vec![0; 4 + 592];
            bytes[0..4].copy_from_slice(&count.to_le_bytes());
            let item = &mut bytes[4..];
            item[0..4].copy_from_slice(&(0x4_u32 | 0x40).to_le_bytes());
            item[36..40].copy_from_slice(&attributes.to_le_bytes());
            item[64..68].copy_from_slice(&0_u32.to_le_bytes());
            item[68..72].copy_from_slice(&17_u32.to_le_bytes());
            for (index, unit) in name.encode_utf16().enumerate() {
                let offset = 72 + index * 2;
                if offset + 1 >= 592 {
                    break;
                }
                item[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }
            bytes
        }

        let good = descriptor("ordinary.txt", 1, 0x80);
        assert_eq!(
            parse_descriptor_bytes(&good).unwrap(),
            vec![("ordinary.txt".into(), Some(17))]
        );
        assert!(parse_descriptor_bytes(&good[..100]).is_err());
        assert!(parse_descriptor_bytes(&descriptor("x.txt", 2, 0)).is_err());
        assert!(parse_descriptor_bytes(&descriptor("../outside", 1, 0)).is_err());
        assert!(parse_descriptor_bytes(&descriptor("CON", 1, 0)).is_err());
        assert!(parse_descriptor_bytes(&descriptor("folder", 1, 0x10)).is_err());
        assert!(parse_descriptor_bytes(&descriptor("link", 1, 0x400)).is_err());
        let mut unterminated = descriptor("", 1, 0);
        unterminated[76..596].fill(0x41);
        assert!(parse_descriptor_bytes(&unterminated).is_err());
        assert_eq!(
            preferred_copy_effect(None).unwrap(),
            super::super::file_clipboard::ClipboardEffect::Copy
        );
        assert_eq!(
            preferred_copy_effect(Some(1)).unwrap(),
            super::super::file_clipboard::ClipboardEffect::Copy
        );
        assert!(preferred_copy_effect(Some(2)).is_err());
        assert!(preferred_copy_effect(Some(3)).is_err());
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::*;
    pub(super) fn capture(_: u32) -> Result<Option<CapturedFile>, TransferError> {
        Err(TransferError::Unsupported)
    }
    pub(super) fn read_chunk(_: TransferToken, _: usize) -> Result<Vec<u8>, TransferError> {
        Err(TransferError::Unsupported)
    }
    pub(super) fn release(_: TransferToken) {}
}

pub fn capture(sequence: u32) -> Result<Option<CapturedFile>, TransferError> {
    platform::capture(sequence)
}

pub fn read_chunk(token: TransferToken, maximum: usize) -> Result<Vec<u8>, TransferError> {
    platform::read_chunk(token, maximum)
}

pub fn release(token: TransferToken) {
    platform::release(token)
}

#[cfg(all(test, target_os = "windows"))]
pub fn install_fake_data_object_session(
    name: String,
    size: Option<u64>,
    chunks: impl IntoIterator<Item = Result<Vec<u8>, String>>,
) -> VirtualFileSources {
    VirtualFileSources::new(vec![platform::install_fake(
        name,
        size,
        chunks.into_iter().collect(),
    )])
    .expect("single fake session creates a non-empty source batch")
}

#[cfg(all(test, target_os = "windows"))]
pub fn install_blocked_fake_data_object_session(
    name: String,
    size: Option<u64>,
    chunks: impl IntoIterator<Item = Result<Vec<u8>, String>>,
) -> (
    VirtualFileSources,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (source, started, release) =
        platform::install_blocked_fake(name, size, chunks.into_iter().collect());
    (
        VirtualFileSources::new(vec![source])
            .expect("single fake session creates a non-empty source batch"),
        started,
        release,
    )
}
