use std::path::{Path, PathBuf};

pub struct DisplayNames;

impl gpui::Asset for DisplayNames {
    /// location と、その名前を無効化した世代。世代が変われば別の asset になる。
    type Source = (PathBuf, u64);
    type Output = Vec<String>;

    fn load(
        (path, _epoch): Self::Source,
        cx: &mut gpui::App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        // Shell問い合わせはネットワークパスで待つことがあるため描画スレッドから外す。
        cx.background_executor().spawn(async move {
            ancestors(&path)
                .iter()
                .map(|path| display_name(path))
                .collect()
        })
    }
}

fn display_name(path: &Path) -> String {
    #[cfg(windows)]
    if let Some(name) = windows_display_name(path) {
        return name;
    }
    label(path)
}

#[cfg(windows)]
fn windows_display_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Storage::FileSystem::GetVolumeInformationW,
            UI::Shell::{SHGetLocalizedName, SHLoadIndirectString},
        },
        core::PCWSTR,
    };
    fn decode(buffer: &[u16]) -> Option<String> {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        (end > 0).then(|| String::from_utf16_lossy(&buffer[..end]))
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buffer = [0u16; 260];
    // v5と同じく、ドライブのラベルとdesktop.iniのローカライズ名をShellから得る。
    unsafe {
        if path.parent().is_none() {
            let root = path.to_string_lossy();
            if root.as_bytes().get(1) == Some(&b':') {
                GetVolumeInformationW(
                    PCWSTR(wide.as_ptr()),
                    Some(&mut buffer),
                    None,
                    None,
                    None,
                    None,
                )
                .ok()?;
                return Some(format!("{} ({})", decode(&buffer)?, &root[..2]));
            }
        }
        let mut resource_id = 0;
        SHGetLocalizedName(PCWSTR(wide.as_ptr()), &mut buffer, &mut resource_id).ok()?;
        let reference = format!("@{},-{}", decode(&buffer)?, resource_id);
        let reference: Vec<u16> = reference.encode_utf16().chain(Some(0)).collect();
        SHLoadIndirectString(PCWSTR(reference.as_ptr()), &mut buffer, None).ok()?;
    }
    decode(&buffer)
}

/// Pathの祖先を使い、ドライブやUNC共有の根元を途中で分割しない。
pub fn ancestors(path: &Path) -> Vec<PathBuf> {
    let mut paths = path
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.reverse();
    paths
}

pub fn label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// 一つ上の階層。
///
/// drive や UNC share の根元より上へは移らない。判定は [`ancestors`] と同じ
/// lossless な path の構成で行い、filesystem には問い合わせない。
///
/// `Up` の key 割り当ては v5 にも無いのでまだ接続していない。
#[allow(dead_code)]
pub fn parent_of(path: &Path) -> Option<PathBuf> {
    let mut chain = ancestors(path);
    // 末尾が自分自身。その手前が親。根元しか無ければ親は無い。
    chain.pop();
    chain.pop()
}

/// v5と同じく、アクティブなタブを閉じると直前のタブ（先頭なら次）へ移る。
pub fn active_after_close(ids: &[usize], active: usize, closing: usize) -> usize {
    if ids.len() <= 1 || closing != active {
        return active;
    }
    let Some(index) = ids.iter().position(|id| *id == closing) else {
        return active;
    };
    ids[if index == 0 { 1 } else { index - 1 }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_shell_name_falls_back_to_path_name() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("存在しないフォルダー");
        assert_eq!(display_name(&missing), "存在しないフォルダー");
    }

    #[test]
    fn closing_other_tab_keeps_active_identity() {
        assert_eq!(active_after_close(&[3, 7, 9], 7, 3), 7);
        assert_eq!(active_after_close(&[3, 7, 9], 7, 7), 3);
        assert_eq!(active_after_close(&[3, 7, 9], 3, 3), 7);
        assert_eq!(active_after_close(&[3], 3, 3), 3);
    }
    #[cfg(windows)]
    #[test]
    fn drive_and_unc_roots_stay_whole() {
        let paths = ancestors(Path::new(r"C:\Users\名前"));
        assert_eq!(
            paths,
            [
                PathBuf::from(r"C:\"),
                PathBuf::from(r"C:\Users"),
                PathBuf::from(r"C:\Users\名前")
            ]
        );
        let paths = ancestors(Path::new(r"\\server\share\folder"));
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from(r"\\server\share\"));
        assert_eq!(label(&paths[1]), "folder");
    }

    #[cfg(windows)]
    #[test]
    fn parent_of_stops_at_drive_and_unc_roots() {
        assert_eq!(
            parent_of(Path::new(r"C:\Users\名前")),
            Some(PathBuf::from(r"C:\Users"))
        );
        // drive の根元より上へは移らない。
        assert_eq!(parent_of(Path::new(r"C:\")), None);
        // UNC share の根元も同じ。
        assert_eq!(
            parent_of(Path::new(r"\\server\share\folder")),
            Some(PathBuf::from(r"\\server\share\"))
        );
        assert_eq!(parent_of(Path::new(r"\\server\share\")), None);
    }
}
