use std::path::PathBuf;

/// 保存先の組。同じ directory に置き、別 volume をまたぐ置換をしない。
#[derive(Clone, Debug)]
#[cfg_attr(not(windows), allow(dead_code))] // 非対応 backend はこれらのパスで I/O しない。
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
    #[cfg(windows)]
    Temporary(String),
    /// platform の置換。primary は変わっていない。
    #[cfg(windows)]
    Replace { code: Option<u32>, message: String },
    /// 一時ファイルを片付けられなかった。
    #[cfg(windows)]
    Cleanup(String),
    #[cfg(not(windows))]
    Unsupported,
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(windows)]
            Self::Temporary(reason) => write!(f, "temporary file: {reason}"),
            #[cfg(windows)]
            Self::Replace { code, message } => match code {
                Some(code) => write!(f, "replace failed ({code}): {message}"),
                None => write!(f, "replace failed: {message}"),
            },
            #[cfg(windows)]
            Self::Cleanup(reason) => write!(f, "cleanup: {reason}"),
            #[cfg(not(windows))]
            Self::Unsupported => write!(f, "durable file writes are unsupported on this platform"),
        }
    }
}
