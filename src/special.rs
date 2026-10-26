//! Explorer の PC に並べる行。
//!
//! PC は path を持たない仮想の場所で、location/ticket の寿命は `DirectorySession` が持つ。
//! 行はここで組み立て、view は普通の一覧と同じ形で描く（1 行 = folder）。
//!
//! **shell への問い合わせは重い。** 切れた network drive への `is_dir`、`\\wsl.localhost` の
//! `read_dir`、volume label の取得はどれも数秒待ちうる。だから
//!
//! - 行の組み立て（[`rows`] とその内訳）は**解決済みの入力だけ**を受け取り、shell を触らない。
//!   順序とラベルはここで単体テストできる。
//! - shell を呼ぶのは `PcDrives` / `PcKnownFolders` / `PcWsl` の背景処理だけ。結果は asset と
//!   して届き、届いたら描き直される（`DisplayNames` と同じ形）。**3つを別々に走らせる**ので、
//!   遅い WSL の probe が drive の行を待たせない。
//!
//! 並べる順は v5 と同じ（known folders → drives → WSL）。v5 と違うのは、drives を
//! `GetLogicalDrives` の bitmask から取ること（v5 は `A:\` から `Z:\` へ `is_dir` していた）。

use crate::{
    directory::EntrySourceIdentity,
    location::{Location, ShellRoot},
};
use std::{collections::HashMap, path::PathBuf};

/// PC に並ぶ 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcEntry {
    /// 表示名。shell の名前が取れなかったときは path の形（`C:\`）にする。
    pub name: String,
    /// 開いたときに移る typed location。表示名から行き先を復元しない。
    pub target: Location,
    pub kind: PcEntryKind,
    /// 背景で観測した filesystem identity。差し替わった path へ古い icon を出さない。
    pub source_identity: Option<EntrySourceIdentity>,
}

/// 行の種類。named Shell root は resource catalog の semantic icon を使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcEntryKind {
    KnownFolder,
    Libraries,
    Network,
    RecycleBin,
    Drive,
    WslDistribution,
}

/// Shell namespace が返す virtual root の行。
///
/// `target` は表示名と別なので、ローカライズや rename で誤った item を開かない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellEntry {
    pub name: String,
    /// folder のときだけ開ける行き先。Recycle Bin の file は Shell の表示専用で、
    /// 元 path や display name を filesystem operation の target にしない。
    pub target: Option<Location>,
    /// Shell が発行した opaque identity。target が無い file 行も表示名ではなくこれで key を作る。
    pub identity: String,
    pub kind: ShellEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellEntryKind {
    Folder,
    File,
}

/// 解決済みの入力。shell を呼ぶ側が用意する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcSources {
    /// 存在する drive と、その volume label。label が無ければ名前は `C:\` の形になる。
    pub drives: Vec<(char, Option<String>)>,
    /// 解決済みの known folder（表示名, path）。順序は呼ぶ側が決める。
    pub known_folders: Vec<(String, PathBuf)>,
    /// Known-folder identities arrive independently so identity probes do not delay rows.
    pub known_folder_identities: HashMap<PathBuf, EntrySourceIdentity>,
    /// Drive の identity は letter の即時表示を待たせない別 asset で届く。
    pub drive_identities: HashMap<char, EntrySourceIdentity>,
    /// WSL distribution の表示名と、列挙できた UNC path。
    pub wsl_distributions: Vec<(String, PathBuf)>,
}

/// 並べる順に組み立てる。shell は触らない。
pub fn rows(sources: &PcSources) -> Vec<PcEntry> {
    let mut rows = known_folder_rows(&sources.known_folders, &sources.known_folder_identities);
    rows.push(PcEntry {
        name: "Libraries".into(),
        target: Location::ShellRoot(ShellRoot::Libraries),
        kind: PcEntryKind::Libraries,
        source_identity: None,
    });
    rows.push(PcEntry {
        name: "Network".into(),
        target: Location::ShellRoot(ShellRoot::Network),
        kind: PcEntryKind::Network,
        source_identity: None,
    });
    rows.push(PcEntry {
        name: "Recycle Bin".into(),
        target: Location::ShellRoot(ShellRoot::RecycleBin),
        kind: PcEntryKind::RecycleBin,
        source_identity: None,
    });
    rows.extend(drive_rows(&sources.drives, &sources.drive_identities));
    rows.extend(wsl_rows(&sources.wsl_distributions));
    rows
}

/// known folder の行。表示名は shell が返したものをそのまま使う。
pub fn known_folder_rows(
    folders: &[(String, PathBuf)],
    identities: &HashMap<PathBuf, EntrySourceIdentity>,
) -> Vec<PcEntry> {
    folders
        .iter()
        .map(|(name, path)| PcEntry {
            name: name.clone(),
            target: Location::Path(path.clone()),
            kind: PcEntryKind::KnownFolder,
            source_identity: identities.get(path).cloned(),
        })
        .collect()
}

/// drive の行。label があれば `OS (C:)`、無ければ `C:\`。
pub fn drive_rows(
    drives: &[(char, Option<String>)],
    identities: &HashMap<char, EntrySourceIdentity>,
) -> Vec<PcEntry> {
    drives
        .iter()
        .map(|(letter, label)| {
            let root = format!("{letter}:\\");
            PcEntry {
                name: label.clone().unwrap_or_else(|| root.clone()),
                target: Location::Path(PathBuf::from(root)),
                kind: PcEntryKind::Drive,
                source_identity: identities.get(letter).cloned(),
            }
        })
        .collect()
}

/// WSL distribution の行。列挙時に見つかった UNC path をそのまま開く。
pub fn wsl_rows(distributions: &[(String, PathBuf)]) -> Vec<PcEntry> {
    distributions
        .iter()
        .map(|(name, path)| PcEntry {
            name: name.clone(),
            // v5 と同じく列挙で成功した alias を保持する。環境によっては旧 `\\wsl$` だけが使える。
            target: Location::Path(path.clone()),
            kind: PcEntryKind::WslDistribution,
            source_identity: None,
        })
        .collect()
}

/// drive の letter と label を合わせて [`PcSources`] の形にする。
///
/// letter は `GetLogicalDrives` から即時に取れるが、label はあとから届く。遅い drive を
/// 待たないために分けているので、届いていない label は `None` のままにして、行は `C:\` の
/// 形で先に出す。
pub fn drives_with_labels(
    letters: &[char],
    labels: &[(char, String)],
) -> Vec<(char, Option<String>)> {
    letters
        .iter()
        .map(|letter| {
            let label = labels
                .iter()
                .find(|(labeled, _)| labeled == letter)
                .map(|(_, name)| name.clone());
            (*letter, label)
        })
        .collect()
}

/// PC の drive 行を背景で解決する asset。
///
/// 3つに分けているのは、**遅いものが速いものを待たせないため**。WSL の probe が数秒かかっても、
/// drive と known folder の行は先に出る。view は揃ったものから [`rows`] で並べる。
///
/// この asset は letter だけを返す（メディアに触らないので即時）。label は [`PcDriveLabels`]。
pub struct PcDrives;

impl gpui::Asset for PcDrives {
    type Source = ();
    type Output = Vec<char>;

    fn load(
        _source: Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor().spawn(async move {
            #[cfg(windows)]
            {
                shell::drive_letters()
            }
            #[cfg(not(windows))]
            {
                Vec::new()
            }
        })
    }
}

/// drive の volume label を背景で解決する asset。
///
/// **行の表示をこれで待たせない。** label は drive に触るので、メディアの無いドライブや
/// 切断された network drive では数分返ってこない（この機械の D: は 210 秒だった）。
/// だから [`shell::label_is_safe`] が真の種類だけを聞き、他の行は `C:\` の名前のままになる。
pub struct PcDriveLabels;

impl gpui::Asset for PcDriveLabels {
    type Source = ();
    type Output = Vec<(char, String)>;

    fn load(
        _source: Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor().spawn(async move {
            #[cfg(windows)]
            {
                shell::drive_labels()
            }
            #[cfg(not(windows))]
            {
                Vec::new()
            }
        })
    }
}

/// PC の known folder 行を背景で解決する asset。
pub struct PcKnownFolders;

impl gpui::Asset for PcKnownFolders {
    type Source = ();
    type Output = Vec<(String, PathBuf)>;

    fn load(
        _source: Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor().spawn(async move {
            #[cfg(windows)]
            {
                shell::known_folders()
            }
            #[cfg(not(windows))]
            {
                Vec::new()
            }
        })
    }
}

/// Capture one known-folder identity independently from row resolution.
pub struct PcKnownFolderIdentity;

impl gpui::Asset for PcKnownFolderIdentity {
    type Source = (PathBuf, u64);
    type Output = Option<EntrySourceIdentity>;

    fn load(
        (path, _generation): Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor()
            .spawn(async move { crate::shell_icons::capture_identity(&path).await })
    }
}

/// Drive roots receive identity independently so a slow/disconnected root never delays its row.
pub struct PcDriveIdentity;

impl gpui::Asset for PcDriveIdentity {
    type Source = (char, u64);
    type Output = Option<EntrySourceIdentity>;

    fn load(
        (letter, _generation): Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor().spawn(async move {
            let path = PathBuf::from(format!("{letter}:\\"));
            crate::shell_icons::capture_identity(&path).await
        })
    }
}

/// PC の WSL distribution 行を背景で解決する asset。
///
/// 一番遅くなりやすいので、他の2つと別に走らせる。
pub struct PcWsl;

impl gpui::Asset for PcWsl {
    type Source = ();
    type Output = Vec<(String, PathBuf)>;

    fn load(
        _source: Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        cx.background_executor().spawn(async move {
            #[cfg(windows)]
            {
                shell::wsl_distributions()
            }
            #[cfg(not(windows))]
            {
                Vec::new()
            }
        })
    }
}

/// virtual Shell root またはその child を Shell namespace から列挙する。
///
/// 呼び出し側は必ず background executor から呼ぶ。Shell item の parsing identity は
/// `Location` に閉じ込め、表示名を再解析して行き先を作らない。
pub fn shell_rows(location: &Location) -> Result<Vec<ShellEntry>, String> {
    #[cfg(windows)]
    {
        shell::shell_rows(location)
    }
    #[cfg(not(windows))]
    {
        let _ = location;
        Err("Windows Shell namespace is unavailable on this platform".into())
    }
}

/// shell を呼ぶ側。**すべて背景で走る。**
#[cfg(windows)]
mod shell {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::Win32::System::Com::{
        COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
    };
    use windows::Win32::System::SystemServices::SFGAO_FOLDER;
    use windows::Win32::UI::Shell::{
        BHID_SFObject, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
        FOLDERID_Libraries, FOLDERID_Music, FOLDERID_NetworkFolder, FOLDERID_Pictures,
        FOLDERID_Profile, FOLDERID_RecycleBinFolder, FOLDERID_Startup, FOLDERID_Videos,
        IEnumIDList, IShellFolder, IShellItem, KF_FLAG_DEFAULT, SHCONTF_FOLDERS,
        SHCONTF_NONFOLDERS, SHCreateItemFromIDList, SHCreateItemWithParent,
        SHGetKnownFolderIDList, SHGetKnownFolderPath, SHParseDisplayName,
        SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY,
    };
    use windows::core::{GUID, PCWSTR, PWSTR};

    /// PC に並べる known folder。Explorer の PC に近い並びにする。
    // なぜ: v5 の PC 一覧では Home / Startup を固定名で表示していたため、profile / startup
    // だけその名前を保ち、残りは Shell が返すローカライズ済み leaf を使う。
    const KNOWN_FOLDERS: &[(&GUID, Option<&str>)] = &[
        (&FOLDERID_Profile, Some("Home")),
        (&FOLDERID_Desktop, None),
        (&FOLDERID_Downloads, None),
        (&FOLDERID_Documents, None),
        (&FOLDERID_Startup, Some("Startup")),
        (&FOLDERID_Pictures, None),
        (&FOLDERID_Music, None),
        (&FOLDERID_Videos, None),
    ];

    /// 存在する drive と、その volume label。
    ///
    /// 列挙は `GetLogicalDrives` の bitmask だけで、**メディアに触らない**。v5 の
    /// `A:\`〜`Z:\` への `is_dir` は、切れた network drive や空のカードリーダーで固まりうる。
    /// label の取得だけは drive に触るので、ここは背景でしか呼ばない。
    /// 存在する drive letter。`GetLogicalDrives` の bitmask だけで、メディアに触らない。
    pub(super) fn drive_letters() -> Vec<char> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u32)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| (b'A' + bit as u8) as char)
            .collect()
    }

    /// 存在する drive の label。**触ってよい種類だけ**聞く（[`label_is_safe`]）。
    pub(super) fn drive_labels() -> Vec<(char, String)> {
        drive_letters()
            .into_iter()
            .filter(|letter| label_is_safe(*letter))
            .filter_map(|letter| Some((letter, drive_name(letter)?)))
            .collect()
    }

    /// drive の種類。`GetDriveTypeW` はメディアに触らない。
    pub(super) fn drive_type(letter: char) -> u32 {
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) }
    }

    /// label を聞いてもよい drive か。
    ///
    /// `GetVolumeInformationW` はメディアに触る。メディアの無いドライブや切断された
    /// network drive では**数分**返ってこないことがある（この機械の D: は 210 秒だった）ので、
    /// 応答が速いと分かっている種類だけにする。
    pub(super) fn label_is_safe(letter: char) -> bool {
        const DRIVE_FIXED: u32 = 3;
        drive_type(letter) == DRIVE_FIXED
    }

    /// drive の volume label。空なら `None`（呼ぶ側が `C:\` の形へ落とす）。
    pub(super) fn drive_name(letter: char) -> Option<String> {
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        let mut buffer = [0u16; 260];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut buffer),
                None,
                None,
                None,
                None,
            )
            .ok()?;
        }
        let label = decode(&buffer)?;
        (!label.is_empty()).then(|| format!("{label} ({letter}:)"))
    }

    /// known folder を shell から解決する。名前は解決した path の末尾を使う。
    ///
    /// shell が返す path は表示名でもある（Windows は新しい profile でローカライズした
    /// フォルダ名を作る）ので、名前のために shell をもう一度叩かない。
    pub(super) fn known_folders() -> Vec<(String, PathBuf)> {
        KNOWN_FOLDERS
            .iter()
            .filter_map(|(id, name)| {
                let (shell_name, path) = known_folder(id)?;
                Some((name.unwrap_or(&shell_name).to_string(), path))
            })
            .collect()
    }

    fn known_folder(id: &GUID) -> Option<(String, PathBuf)> {
        let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok()? };
        let text = unsafe { raw.to_string().ok()? };
        // shell が CoTaskMemAlloc で返す。読んだら返す。
        unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
        let path = PathBuf::from(text);
        // 存在しない known folder は出さない（redirect された先も含めて、背景で1回 stat する）。
        if !path.is_dir() {
            return None;
        }
        let name = path.file_name()?.to_string_lossy().into_owned();
        Some((name, path))
    }

    /// WSL distribution を列挙し、成功した UNC root を含む path を返す。
    /// `\\wsl.localhost` を読み、無ければ旧 `\\wsl$` を試す。
    ///
    /// `wsl.exe` は起動しない（process を起こすのは重く、`read_dir` で足りる環境が今の
    /// 対象）。無ければ空を返すだけで、他を待たせない。
    pub(super) fn wsl_distributions() -> Vec<(String, PathBuf)> {
        for root in [r"\\wsl.localhost", r"\\wsl$"] {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut distributions = entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_str()?.to_owned();
                    Some((name, entry.path()))
                })
                .collect::<Vec<_>>();
            distributions.sort_by(|a, b| a.0.cmp(&b.0));
            if !distributions.is_empty() {
                return distributions;
            }
        }
        Vec::new()
    }

    pub(super) fn shell_rows(location: &Location) -> Result<Vec<ShellEntry>, String> {
        // Shell namespace object は apartment 初期化済みの worker でだけ触る。GPUI の
        // render / input thread に COM や network-backed library の待ちを持ち込まない。
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| format!("initialize COM: {error}"))?;
        let result = unsafe { shell_rows_inner(location) };
        unsafe { CoUninitialize() };
        result
    }

    unsafe fn shell_rows_inner(location: &Location) -> Result<Vec<ShellEntry>, String> {
        let pidl = match location {
            Location::ShellRoot(root) => unsafe {
                let folder_id = match root {
                    ShellRoot::Libraries => &FOLDERID_Libraries,
                    ShellRoot::Network => &FOLDERID_NetworkFolder,
                    ShellRoot::RecycleBin => &FOLDERID_RecycleBinFolder,
                };
                SHGetKnownFolderIDList(folder_id, KF_FLAG_DEFAULT.0 as u32, HANDLE::default())
                    .map_err(|error| format!("resolve {}: {error}", root.display_name()))?
            },
            Location::ShellItem { parsing_name, .. } => {
                let wide: Vec<u16> = parsing_name.encode_utf16().chain(Some(0)).collect();
                let mut parsed = std::ptr::null_mut();
                unsafe {
                    SHParseDisplayName(
                        PCWSTR(wide.as_ptr()),
                        None::<&IBindCtx>,
                        &mut parsed,
                        0,
                        None,
                    )
                    .map_err(|error| format!("resolve Shell item: {error}"))?;
                }
                parsed
            }
            Location::Path(_) | Location::Archive { .. } | Location::ThisPc => {
                return Err("Shell adapter received a non-Shell location".into());
            }
        };
        let result = (|| unsafe {
            let item: IShellItem = SHCreateItemFromIDList(pidl)
                .map_err(|error| format!("create Shell item: {error}"))?;
            let folder: IShellFolder = item
                .BindToHandler(None::<&IBindCtx>, &BHID_SFObject)
                .map_err(|error| format!("bind Shell folder: {error}"))?;
            let mut enumerator: Option<IEnumIDList> = None;
            folder
                .EnumObjects(
                    windows::Win32::Foundation::HWND::default(),
                    (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32,
                    &mut enumerator,
                )
                .ok()
                .map_err(|error| format!("enumerate Shell folder: {error}"))?;
            let Some(enumerator) = enumerator else {
                return Ok(Vec::new());
            };
            let mut rows = Vec::new();
            loop {
                let mut child = std::ptr::null_mut();
                let mut fetched = 0;
                enumerator
                    .Next(std::slice::from_mut(&mut child), Some(&mut fetched))
                    .ok()
                    .map_err(|error| format!("iterate Shell folder: {error}"))?;
                if fetched == 0 || child.is_null() {
                    break;
                }
                let row = shell_row(&folder, pidl, child, location);
                CoTaskMemFree(Some(child.cast()));
                if let Some(row) = row {
                    rows.push(row);
                }
            }
            Ok(rows)
        })();
        unsafe { CoTaskMemFree(Some(pidl.cast())) };
        result
    }

    unsafe fn shell_row(
        parent: &IShellFolder,
        parent_pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST,
        child_pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST,
        location: &Location,
    ) -> Option<ShellEntry> {
        let item: IShellItem =
            unsafe { SHCreateItemWithParent(Some(parent_pidl), parent, child_pidl).ok()? };
        let name = unsafe { shell_name(&item, SIGDN_NORMALDISPLAY) }.ok()?;
        let is_folder = unsafe { item.GetAttributes(SFGAO_FOLDER) }
            .map(|attributes| attributes.0 & SFGAO_FOLDER.0 != 0)
            .unwrap_or(false);
        let root = shell_root_for(location)?;
        let identity = unsafe { shell_name(&item, SIGDN_DESKTOPABSOLUTEPARSING) }.ok()?;
        let target = if root == ShellRoot::RecycleBin {
            // Recycle Bin の filesystem path は削除前の original location を指し得る。
            // ここで渡すと row が削除済み object ではなく元 path を操作してしまう。
            is_folder.then(|| Location::ShellItem {
                root,
                parsing_name: identity.clone(),
                display_name: name.clone(),
            })
        } else {
            match unsafe { shell_name(&item, SIGDN_FILESYSPATH) } {
                Ok(path) if !path.is_empty() => Some(Location::Path(PathBuf::from(path))),
                _ if is_folder => Some(Location::ShellItem {
                    root,
                    parsing_name: identity.clone(),
                    display_name: name.clone(),
                }),
                // Shell-only files need a later Shell execute capability; never turn their name into
                // a path or claim they can be opened by the filesystem adapter.
                _ => None,
            }
        };
        Some(ShellEntry {
            name,
            target,
            identity,
            kind: if is_folder {
                ShellEntryKind::Folder
            } else {
                ShellEntryKind::File
            },
        })
    }

    fn shell_root_for(location: &Location) -> Option<ShellRoot> {
        match location {
            Location::ShellRoot(root) | Location::ShellItem { root, .. } => Some(*root),
            Location::Path(_) | Location::Archive { .. } | Location::ThisPc => None,
        }
    }

    unsafe fn shell_name(
        item: &IShellItem,
        format: windows::Win32::UI::Shell::SIGDN,
    ) -> Result<String, String> {
        let value: PWSTR = unsafe { item.GetDisplayName(format) }
            .map_err(|error| format!("read Shell item name: {error}"))?;
        let text = unsafe { value.to_string() }
            .map_err(|error| format!("decode Shell item name: {error}"));
        unsafe { CoTaskMemFree(Some(value.0.cast())) };
        text
    }

    fn decode(buffer: &[u16]) -> Option<String> {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        (end > 0).then(|| String::from_utf16_lossy(&buffer[..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> (String, PathBuf) {
        (
            name.to_string(),
            PathBuf::from(format!(r"C:\Users\me\{name}")),
        )
    }

    /// 並びは v5 と同じ。known folders → drives → WSL。
    #[test]
    fn rows_follow_v5_order() {
        let sources = PcSources {
            drives: vec![('C', None), ('D', None)],
            known_folders: vec![folder("Desktop")],
            known_folder_identities: HashMap::new(),
            drive_identities: HashMap::new(),
            wsl_distributions: vec![(
                "Ubuntu".into(),
                PathBuf::from(r"\\wsl.localhost\Ubuntu"),
            )],
        };
        let kinds: Vec<_> = rows(&sources).iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PcEntryKind::KnownFolder,
                PcEntryKind::Libraries,
                PcEntryKind::Network,
                PcEntryKind::RecycleBin,
                PcEntryKind::Drive,
                PcEntryKind::Drive,
                PcEntryKind::WslDistribution,
            ]
        );
    }

    /// 何も解決できなくても落ちない。PC は空の一覧になる。
    #[test]
    fn empty_sources_still_expose_typed_shell_roots() {
        let rows = rows(&PcSources::default());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].target, Location::ShellRoot(ShellRoot::Libraries));
        assert_eq!(rows[1].target, Location::ShellRoot(ShellRoot::Network));
        assert_eq!(rows[2].target, Location::ShellRoot(ShellRoot::RecycleBin));
    }

    /// drive は `C:\` の形で、開くとその path へ移る。label があればそちらを使う。
    #[test]
    fn drives_carry_their_root_path() {
        let rows = drive_rows(&[('C', None)], &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, r"C:\");
        assert_eq!(rows[0].target, Location::Path(PathBuf::from(r"C:\")));

        let rows = drive_rows(&[('C', Some("OS (C:)".into()))], &HashMap::new());
        assert_eq!(rows[0].name, "OS (C:)");
    }

    /// letter は即時、label はあとから。届いていない label は `None` のままにする。
    #[test]
    fn labels_are_merged_when_they_arrive() {
        let letters = ['C', 'D'];
        let merged = drives_with_labels(&letters, &[]);
        assert_eq!(merged, vec![('C', None), ('D', None)]);

        let merged = drives_with_labels(&letters, &[('D', "SD (D:)".into())]);
        assert_eq!(
            merged,
            vec![('C', None), ('D', Some("SD (D:)".into()))],
            "届いた label だけを載せる"
        );
    }

    /// known folder は shell が返した名前と path をそのまま運ぶ。
    #[test]
    fn known_folders_keep_the_resolved_name_and_path() {
        let rows = known_folder_rows(&[folder("Desktop")], &HashMap::new());
        assert_eq!(rows[0].name, "Desktop");
        assert_eq!(
            rows[0].target,
            Location::Path(PathBuf::from(r"C:\Users\me\Desktop"))
        );
    }

    /// WSL は UNC として開ける path になる。
    #[test]
    fn wsl_rows_open_the_unc_path() {
        let rows = wsl_rows(&[("Ubuntu".into(), PathBuf::from(r"\\wsl$\Ubuntu"))]);
        assert_eq!(rows[0].name, "Ubuntu");
        assert_eq!(
            rows[0].target,
            Location::Path(PathBuf::from(r"\\wsl$\Ubuntu"))
        );
    }

    #[test]
    fn pc_exposes_typed_virtual_shell_roots() {
        let rows = rows(&PcSources::default());
        for root in [
            ShellRoot::Libraries,
            ShellRoot::Network,
            ShellRoot::RecycleBin,
        ] {
            assert!(
                rows.iter()
                    .any(|row| row.target == Location::ShellRoot(root)),
                "missing {root:?}"
            );
        }
    }
}
