use crate::operations::model::SubmitError;
use chrono::{DateTime, Local};
use std::borrow::Cow;
use std::time::SystemTime;

/// The locale selected by the application localization owner.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Locale {
    Japanese,
    English,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LanguageOverride {
    #[default]
    System,
    Japanese,
    English,
    #[cfg(debug_assertions)]
    Pseudo,
}

impl LanguageOverride {
    pub(crate) fn startup(debug_pseudo_requested: bool) -> Self {
        #[cfg(debug_assertions)]
        if debug_pseudo_requested {
            return Self::Pseudo;
        }
        let _ = debug_pseudo_requested;
        Self::System
    }

    pub(crate) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::System,
            "ja" | "ja-jp" | "japanese" => Self::Japanese,
            "en" | "en-us" | "english" => Self::English,
            _ => Self::Japanese,
        }
    }

    pub(crate) fn persisted(self) -> &'static str {
        match self {
            Self::System => "auto",
            Self::Japanese => "ja",
            Self::English => "en",
            #[cfg(debug_assertions)]
            Self::Pseudo => "auto",
        }
    }

    pub(crate) fn resolve(self, system: Locale) -> Locale {
        match self {
            Self::System => system,
            Self::Japanese => Locale::Japanese,
            Self::English => Locale::English,
            #[cfg(debug_assertions)]
            Self::Pseudo => Locale::Japanese,
        }
    }

    pub(crate) fn translator(self, system: Locale) -> Translator {
        match self {
            #[cfg(debug_assertions)]
            Self::Pseudo => Translator::pseudo(Locale::Japanese),
            _ => Translator::new(self.resolve(system)),
        }
    }
}

pub(crate) fn pseudo_startup_requested() -> bool {
    #[cfg(debug_assertions)]
    {
        return std::env::var_os("GPUI_FILER_PSEUDO_LOCALE")
            .is_some_and(|value| value == std::ffi::OsStr::new("1"));
    }
    #[cfg(not(debug_assertions))]
    false
}

pub(crate) fn system_locale() -> Locale {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::GetUserDefaultUILanguage;
        let language = unsafe { GetUserDefaultUILanguage() };
        return locale_from_windows_language_id(language);
    }
    #[cfg(not(windows))]
    Locale::Japanese
}

fn locale_from_windows_language_id(language: u16) -> Locale {
    // 未対応言語は日本語へ戻す。これは RTL の表示方向を保証しない。
    match language & 0x03ff {
        0x0009 => Locale::English,
        _ => Locale::Japanese,
    }
}

/// Application-owned messages that have already crossed the component boundary.
///
/// Reusable UI crates never see this enum; they receive the resolved text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    NewTab,
    NoClosedWorkspaces,
    ClosedWorkspaces,
    DuplicateWorkspace,
    WorkspaceCopyPrefix,
    WorkspaceCopySuffix,
    WorkspaceCapacityReached,
    WorkspaceDuplicateFailed,
    NoClosedTabs,
    ClosedTabHistoryLimit,
    ClosedTabHistoryRecordTooLarge,
    ClearClosedTabHistoryAndRetry,
    SampleList,
    /// 一覧の列見出し。v5 の `ColumnKind::label()` と同じ語を使う。
    ColumnName,
    ColumnKind,
    ColumnSize,
    ColumnModified,
    /// 一覧の状態表示。
    Loading,
    LoadFailed,
    Refreshing,
    RefreshFailed,
    Retry,
    ClearSelection,
    MultipleSelection,
    /// フォルダ変更の監視が動いていない。
    WatchDegraded,
    /// セッションの保存が止まっている。
    SessionNotSaved,
    /// 受理済みファイル操作を開始できなかった。
    OperationCapacityReached,
    OperationRetryRejected,
    UndoOperationRejected,
    UndoOperationPartial,
    MoveInspectionPending,
    MoveInspectionReview,
    CopyInspectionPending,
    CopyInspectionReview,
    CopyInspectionRetry,
    CopyInspectionDeleteStage,
    CopyInspectionDeleteRejected,
    CopyStageCleanupRunning,
    CopyCleanupExplanation,
    CopyCleanupStagePath,
    CopyPublicationHistory,
    CopyPublicationNotPublished,
    CopyPublicationPublished,
    CopyPublicationUnknown,
    CopyObservationSource,
    CopyObservationParent,
    CopyObservationDestination,
    CopyObservationStage,
    CopyObservationStageContent,
    CopyObservationMatches,
    CopyObservationChanged,
    CopyObservationAbsent,
    CopyObservationIndeterminate,
    MoveRetryVerified,
    MoveInspectionUnavailable,
    MoveSourceIdentity,
    MoveDestinationParentIdentity,
    MoveIdentityMatches,
    MoveIdentityMismatch,
    MoveDestinationState,
    MoveDestinationAbsent,
    MoveDestinationPresent,
    MoveDestinationIndeterminate,
    PublishedMoveReview,
    DirectoryMoveReview,
    DirectoryMoveChildSummary,
    DirectoryMovePendingChild,
    DirectoryMovePendingRoot,
    DirectoryMoveRootSummary,
    DirectoryMoveCleanupSummary,
    DirectoryMovePending,
    DirectoryMoveNotPending,
    DirectoryMoveRecorded,
    DirectoryMoveNotRecorded,
    DirectoryMoveUnavailable,
    CrossVolumeMoveRetryVerified,
    CrossVolumeMoveRetrySummary,
    PublishedMoveSourceRetained,
    PublishedMoveDispositionAccepted,
    PublishedMoveDestinationMatches,
    PublishedMoveDestinationChanged,
    PublishedMoveDestinationAbsent,
    PublishedMoveDestinationIndeterminate,
    MoveCandidateVerified,
    MoveCandidateUnavailable,
    OperationIdUnavailable,
    OperationIdentityUnavailable,
    OperationInvalidRequest,
    /// OLE が Move を報告したが、source cleanup を queue できなかった。
    ExternalMoveCleanupQueueRejected,
    BatchRenamePreviewStale,
    BatchRenameUndoNotQueued,
    ReplaceRecoveryIntentNotSaved,
    CaseRenameRecoveryIntentNotSaved,
    BatchRenameItemNotSaved,
    BatchRenamePlanNotSaved,
    RecoveryRecordsCouldNotDismiss,
    RecoveryKindCopy,
    RecoveryKindMove,
    RecoveryKindSymbolicLink,
    RecoveryKindJunction,
    RecoveryKindNewFolder,
    RecoveryKindNewFile,
    RecoveryKindCreateShortcut,
    OperationCreatingShortcut,
    RecoveryKindTrash,
    RecoveryKindRename,
    RecoveryKindBatchRename,
    RecoveryKindBatchRenameUndo,
    RecoveryKindCreateZip,
    RecoveryKindCreate7z,
    RecoveryKindExtractZip,
    RecoveryHeading,
    RecoveryHistoricalWarning,
    RecoveryRecordedSources,
    RecoveryRecordedDestination,
    RecoveryCreatedFolder,
    RecoveryPlanDestinations,
    RecoveryRecordedTerminalOutcome,
    RecoverySourceRootNow,
    RecoveryDestinationRootNow,
    RecoveryOutcomeSucceeded,
    RecoveryOutcomeFailed,
    RecoveryOutcomeCancelled,
    RecoveryOutcomePartial,
    RecoveryOutcomeUnknown,
    RecoveryCompletedRenameReceipts,
    RecoveryHistoricalReplaceBackup,
    RecoveryHistoricalCrossVolumeMoveReplace,
    RecoveryHistoricalZipCreate,
    RecoveryHistoricalZipExtract,
    RecoveryZipStagePath,
    RecoveryZipPhase,
    RecoveryMoveStagePath,
    RecoveryMovePhase,
    RecoveryBackupSource,
    RecoveryBackupDestination,
    RecoveryBackupPath,
    RecoveryHistoricalRenameStage,
    RecoveryRenameStageSource,
    RecoveryRenameStagePath,
    RecoveryRenameStageDestination,
    RecoveryRenameStagePhase,
    RecoveryHistoricalDirectoryMoveReplace,
    RecoveryDirectoryMovePhase,
    RecoverySourceLocation,
    RecoveryTargetLocation,
    RecoveryLocationOriginal,
    RecoveryLocationPublished,
    RecoveryLocationDestination,
    RecoveryLocationBackup,
    RecoveryLocationDisposed,
    RecoveryLocationUncertain,
    RecoveryCleanupSummary,
    RecoveryCleanupChildren,
    RecoveryBackupRootDisposal,
    RecoveryBackupRootRecorded,
    RecoveryBackupRootNotRecorded,
    RecoveryPendingCleanup,
    RecoveryCleanupOrdinal,
    RecoveryCleanupNodeKind,
    RecoveryNodeFile,
    RecoveryNodeDirectory,
    RecoveryNodeUnknown,
    RecoveryBackupRoot,
    RecoveryRelativePath,
    RecoveryMovePhasePrepared,
    RecoveryMovePhaseNoEffect,
    RecoveryMovePhaseDisplaceIntent,
    RecoveryMovePhaseTargetDisplaced,
    RecoveryMovePhasePublishIntent,
    RecoveryMovePhaseDestinationPublished,
    RecoveryMovePhaseBackupCleanup,
    RecoveryMovePhaseBackupComplete,
    RecoveryMovePhaseComplete,
    RecoveryNoneRecorded,
    RecoveryDetails,
    RecoveryHideDetails,
    RecoveryDismiss,
    RecoveryNewCopy,
    RecoveryNewCopyFailed,
    RecoverySaving,
    RecoveryPreviousPage,
    RecoveryNextPage,
    RecoveryDetailPage,
    RecoveryMoreRecords,
    RecoveryPreviousRecords,
    RecoveryNextRecords,
    RecoverySourceSummary,
    RecoveryShortcutHint,
    OperationJournalPreserved,
    FolderPreferencesNotSaved,
    SavedPaneRestoreIncomplete,
    Dismiss,
    /// 外部 keymap を読み込めなかった。
    KeymapNotLoaded,
    /// 通知が多くて畳んだ分がある。
    MoreNotices,
    /// 表示形式の切り替え。
    LayoutRows,
    LayoutTiles,
    LayoutIcons,
    GroupNone,
    GroupByDate,
    GroupByKind,
    GroupDateToday,
    GroupDateYesterday,
    GroupDateThisWeek,
    GroupDateLastWeek,
    GroupDateThisMonth,
    GroupDateLastMonth,
    GroupDateThisYear,
    GroupDateLastYear,
    GroupDateEarlier,
    GroupDateUnknown,
    ToggleColumnKind,
    ToggleColumnSize,
    ToggleColumnModified,
    MoveNameLeft,
    MoveNameRight,
    MoveKindLeft,
    MoveKindRight,
    MoveSizeLeft,
    MoveSizeRight,
    MoveModifiedLeft,
    MoveModifiedRight,
    ToggleFileInfoPane,
    ShowHiddenItems,
    ItemCheckBoxes,
    FileInfoKind,
    FileInfoSize,
    FileInfoModified,
    FileInfoLocation,
    FileInfoLinkTarget,
    FileInfoEmpty,
    FileInfoMultiple,
    FileInfoItems,
    FileInfoFiles,
    FileInfoFolders,
    FileInfoTotalSize,
    FileInfoCalculating,
    FileInfoSizeUnknown,
    FileInfoCopyName,
    FileInfoCopyLocation,
    FileInfoCopyNames,
    FileInfoCopyPaths,
    FileInfoHash,
    FileInfoHashLoading,
    FileInfoHashFailed,
    FileInfoHashUnavailable,
    FileInfoExif,
    FileInfoExifLoading,
    FileInfoExifNone,
    FileInfoExifFailed,
    FileInfoExifMake,
    FileInfoExifModel,
    FileInfoExifDateTimeOriginal,
    FileInfoExifLensModel,
    FileInfoExifExposureTime,
    FileInfoExifFNumber,
    FileInfoExifPhotographicSensitivity,
    FileInfoExifFocalLength,
    FileInfoShortcut,
    FileInfoShortcutTarget,
    FileInfoShortcutStatus,
    FileInfoShortcutValid,
    FileInfoShortcutBroken,
    FileInfoShortcutWorkingDirectory,
    FileInfoShortcutArguments,
    FileInfoShortcutLoading,
    FileInfoShortcutFailed,
    /// メニューバーの見出し。
    MenuFile,
    MenuEdit,
    MenuView,
    MenuTheme,
    MenuLanguage,
    MenuLayout,
    MenuColumns,
    MenuCompression,
    MenuCreation,
    SevenZipProvider,
    SevenZipUnknown,
    SevenZipDiscovering,
    SevenZipUnavailable,
    SevenZipAvailable,
    SevenZipFormatCreate,
    LanguageSystem,
    LanguageJapanese,
    LanguageEnglish,
    #[cfg(debug_assertions)]
    LanguagePseudo,
    SearchTitle,
    SearchTypeToSearch,
    SearchUsage,
    SearchExamples,
    SearchActivationHint,
    SearchSearching,
    SearchFailed,
    SearchProviderChanged,
    BatchRenameTitle,
    BatchRenamePattern,
    BatchRenameFind,
    BatchRenameReplace,
    BatchRenameCancel,
    BatchRenameSubmit,
    BatchRenameNoChanges,
    BatchRenameListingChanged,
    ConflictHeading,
    ConflictAlreadyExists,
    ConflictSkip,
    ConflictKeepBoth,
    ConflictReplace,
    ConflictApplyToRemaining,
    ConflictCancel,
    ConflictWaiting,
    MenuGo,
    NewWindow,
    NewWindowFailed,
    /// パンくずの先頭に出す仮想のルート（Explorer の PC）。
    ThisPc,
    /// メニューに出すコマンド名。
    Refresh,
    NavigateBack,
    NavigateForward,
    /// 項目を開く。
    Activate,
    ActivateNewTab,
    OpenInNewPane,
    CopyPaths,
    CopyNames,
    CreateZip,
    CreateShortcut,
    Create7z,
    ExtractToFolder,
    OpenInWindowsTerminal,
    WindowsTerminalLaunchFailed,
    Rename,
    Properties,
    PasteLinkHere,
    PasteJunctionHere,
    CreateFolder,
    CreateFile,
    SelectExtensionOnRename,
    ReloadKeymap,
    TabDuplicate,
    TabClose,
    TabCloseOthers,
    TabCloseToRight,
    TabLock,
    TabUnlock,
    QuickPreviewLoading,
    QuickPreviewFailed,
    QuickPreviewFallback,
    QuickPreviewEncoding,
    QuickPreviewLossy,
    QuickPreviewTruncated,
    QuickPreviewDuration,
    QuickPreviewDimensions,
    QuickPreviewSize,
    ThumbnailGenerationUnavailable,
    ThumbnailProcessingFailed,
    ThumbnailFolderUnavailable,
    ThumbnailFolderNoPreview,
}

/// Small application-side localization facade.
///
/// The facade is deliberately backend-free. Keeping call sites behind this API
/// lets a later Fluent (or other) backend replace the source table without
/// teaching presentation components about locale selection or message IDs.
#[derive(Clone)]
pub(crate) struct Translator {
    locale: Locale,
    #[cfg(debug_assertions)]
    pseudo: bool,
}

impl Translator {
    pub(crate) fn new(locale: Locale) -> Self {
        Self {
            locale,
            #[cfg(debug_assertions)]
            pseudo: false,
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn pseudo(locale: Locale) -> Self {
        Self {
            locale,
            pseudo: true,
        }
    }

    fn expand(&self, text: impl Into<String>) -> String {
        let text = text.into();
        #[cfg(debug_assertions)]
        if self.pseudo {
            return format!("⟦!! {text} ·· {text} !!⟧");
        }
        text
    }

    pub(crate) fn text(&self, message: Message) -> Cow<'static, str> {
        let japanese = match message {
            Message::NewTab => "新しいタブ",
            Message::NoClosedWorkspaces => "閉じたワークスペースはありません",
            Message::ClosedWorkspaces => "閉じたワークスペース",
            Message::DuplicateWorkspace => "複製",
            Message::WorkspaceCopyPrefix => "",
            Message::WorkspaceCopySuffix => " のコピー",
            Message::WorkspaceCapacityReached => {
                "ワークスペースを追加できません。ワークスペース、ペイン、またはタブの上限に達しています"
            }
            Message::WorkspaceDuplicateFailed => "ワークスペースを複製できませんでした",
            Message::NoClosedTabs => "閉じたタブはありません",
            Message::ClosedTabHistoryLimit => {
                "閉じたタブ履歴が上限に達したため、古い履歴を削除しました"
            }
            Message::ClosedTabHistoryRecordTooLarge => {
                "このタブは履歴の保存上限を超えるため、履歴に追加できませんでした"
            }
            Message::ClearClosedTabHistoryAndRetry => {
                "閉じたタブとワークスペースの履歴を消去して再試行"
            }
            Message::SampleList => "サンプル一覧",
            Message::ColumnName => "Name",
            Message::ColumnKind => "Kind",
            Message::ColumnSize => "Size",
            Message::ColumnModified => "Date modified",
            Message::Loading => "読み込み中",
            Message::LoadFailed => "読み込み失敗",
            Message::Refreshing => "読み直し中",
            Message::RefreshFailed => "読み直し失敗",
            Message::Retry => "再試行",
            Message::ClearSelection => "選択解除",
            Message::MultipleSelection => "複数選択中",
            Message::WatchDegraded => "自動更新なし",
            Message::SessionNotSaved => "セッションを保存できません",
            Message::OperationCapacityReached => {
                "操作が多すぎるため、このファイル操作を開始できませんでした"
            }
            Message::OperationRetryRejected => "このファイル操作は再試行できません",
            Message::UndoOperationRejected => {
                "取り消し結果を確認できませんでした。記録は保持しています"
            }
            Message::UndoOperationPartial => {
                "復元を公開しましたが、取り消しを完了できませんでした。移動先の一部が残っている可能性があります"
            }
            Message::MoveInspectionPending => "移動結果を確認中…",
            Message::MoveInspectionReview => "確認",
            Message::CopyInspectionPending => "コピー結果を確認中…",
            Message::CopyInspectionReview => "確認",
            Message::CopyInspectionRetry => "未公開項目を再試行",
            Message::CopyInspectionDeleteStage => "保持されたステージを削除",
            Message::CopyInspectionDeleteRejected => "保持ステージを削除できませんでした",
            Message::CopyStageCleanupRunning => "保持されたコピー用ステージを削除中…",
            Message::CopyCleanupExplanation => {
                "保持された一時ステージにはコピー途中のデータが含まれる場合があります。削除対象はこのステージだけで、元ファイルと移動先は削除しません。"
            }
            Message::CopyCleanupStagePath => "ステージのパス",
            Message::CopyPublicationHistory => "公開記録",
            Message::CopyPublicationNotPublished => "未公開",
            Message::CopyPublicationPublished => "公開済み",
            Message::CopyPublicationUnknown => "公開状態不明",
            Message::CopyObservationSource => "検査時の元ファイル",
            Message::CopyObservationParent => "現在の移動先親",
            Message::CopyObservationDestination => "検査時の移動先",
            Message::CopyObservationStage => "検査時のステージ",
            Message::CopyObservationStageContent => "ステージ内容（未比較）",
            Message::CopyObservationMatches => "一致",
            Message::CopyObservationChanged => "変更あり",
            Message::CopyObservationAbsent => "存在しない",
            Message::CopyObservationIndeterminate => "判定できない",
            Message::MoveRetryVerified => "確認済み項目を再試行",
            Message::MoveInspectionUnavailable => "再試行できません: 安全条件を満たしていません",
            Message::MoveSourceIdentity => "元ファイルのidentity",
            Message::MoveDestinationParentIdentity => "移動先親のidentity",
            Message::MoveIdentityMatches => "一致",
            Message::MoveIdentityMismatch => "不一致",
            Message::MoveDestinationState => "移動先",
            Message::MoveDestinationAbsent => "存在しない",
            Message::MoveDestinationPresent => "存在する",
            Message::MoveDestinationIndeterminate => "確認できない",
            Message::PublishedMoveReview => "移動結果を確認",
            Message::DirectoryMoveReview => "移動記録を確認",
            Message::DirectoryMoveChildSummary => "記録済みの子項目処理",
            Message::DirectoryMovePendingChild => "未解決の子項目処理あり",
            Message::DirectoryMovePendingRoot => "未解決のルート処理あり",
            Message::DirectoryMoveRootSummary => "移動元ルートの処理記録",
            Message::DirectoryMoveCleanupSummary => "記録上のクリーンアップ完了",
            Message::DirectoryMovePending => "未解決あり",
            Message::DirectoryMoveNotPending => "未解決なし",
            Message::DirectoryMoveRecorded => "記録済み",
            Message::DirectoryMoveNotRecorded => "未記録",
            Message::DirectoryMoveUnavailable => "利用できる通常ディレクトリ移動記録はありません",
            Message::CrossVolumeMoveRetryVerified => "確認済みのcross-volume移動を再試行",
            Message::CrossVolumeMoveRetrySummary => {
                "cross-volume移動: 元ファイルと移動先親のidentityが一致し、移動先が存在しない項目だけ再試行できる"
            }
            Message::PublishedMoveSourceRetained => "移動先を公開し、移動元を保持",
            Message::PublishedMoveDispositionAccepted => {
                "移動先と移動元の処理を受け付け済み（現在の状態を確認）"
            }
            Message::PublishedMoveDestinationMatches => "公開済みの移動先と一致",
            Message::PublishedMoveDestinationChanged => "移動先が変更済み",
            Message::PublishedMoveDestinationAbsent => "移動先が存在しない",
            Message::PublishedMoveDestinationIndeterminate => "移動先を確認できない",
            Message::MoveCandidateVerified => "変更なしを確認済み。再試行できます",
            Message::MoveCandidateUnavailable => {
                "receiptまたはidentityの確認が安全でないため再試行できません"
            }
            Message::OperationIdUnavailable => {
                "操作を開始できませんでした。アプリを再起動してください"
            }
            Message::OperationIdentityUnavailable => {
                "安全な操作 ID を作成できないため、ファイル操作を開始できません"
            }
            Message::OperationInvalidRequest => "ファイル操作の要求が正しくありません",
            Message::ExternalMoveCleanupQueueRejected => {
                "OLE は移動を報告しましたが、元ファイルの後処理を予約できませんでした。元ファイルは残っています"
            }
            Message::BatchRenamePreviewStale => "一括名前変更の一覧が古くなっています",
            Message::BatchRenameUndoNotQueued => "一括名前変更の取り消しを開始できませんでした",
            Message::ReplaceRecoveryIntentNotSaved => {
                "復元情報を保存できなかったため、置き換えを中止しました"
            }
            Message::CaseRenameRecoveryIntentNotSaved => {
                "復元情報を保存できなかったため、名前変更を中止しました"
            }
            Message::BatchRenameItemNotSaved => {
                "完了した項目を保存できなかったため、一括名前変更を停止しました"
            }
            Message::BatchRenamePlanNotSaved => {
                "計画を保存できなかったため、一括名前変更を中止しました"
            }
            Message::RecoveryRecordsCouldNotDismiss => "復旧記録を閉じられませんでした",
            Message::RecoveryKindCopy => "コピー",
            Message::RecoveryKindMove => "移動",
            Message::RecoveryKindSymbolicLink => "シンボリックリンク作成",
            Message::RecoveryKindJunction => "ジャンクション作成",
            Message::RecoveryKindNewFolder => "フォルダー作成",
            Message::RecoveryKindNewFile => "ファイル作成",
            Message::RecoveryKindCreateShortcut => "ショートカット作成",
            Message::OperationCreatingShortcut => "ショートカットを作成中…",
            Message::RecoveryKindTrash => "ごみ箱へ移動",
            Message::RecoveryKindRename => "名前変更",
            Message::RecoveryKindBatchRename => "一括名前変更",
            Message::RecoveryKindBatchRenameUndo => "一括名前変更の取り消し",
            Message::RecoveryKindCreateZip => "ZIP 作成",
            Message::RecoveryKindCreate7z => "7z 作成",
            Message::RecoveryKindExtractZip => "ZIP 展開",
            Message::RecoveryHeading => "中断されたファイル操作",
            Message::RecoveryHistoricalWarning => {
                "記録されたパスは過去の情報で、現在のファイルシステム上の対象を示すものではありません"
            }
            Message::RecoveryRecordedSources => "記録された元のパス（過去の情報）",
            Message::RecoveryRecordedDestination => "記録された移動先（過去の情報）",
            Message::RecoveryCreatedFolder => "作成したフォルダー（過去の情報）",
            Message::RecoveryPlanDestinations => "一括名前変更計画の移動先（過去の情報）",
            Message::RecoveryRecordedTerminalOutcome => "記録された終端結果",
            Message::RecoverySourceRootNow => "元ルートの現在の観測",
            Message::RecoveryDestinationRootNow => "移動先ルートの現在の観測",
            Message::RecoveryOutcomeSucceeded => "成功",
            Message::RecoveryOutcomeFailed => "失敗",
            Message::RecoveryOutcomeCancelled => "キャンセル",
            Message::RecoveryOutcomePartial => "一部完了",
            Message::RecoveryOutcomeUnknown => "不明",
            Message::RecoveryCompletedRenameReceipts => "完了した一括名前変更の記録（過去の情報）",
            Message::RecoveryHistoricalReplaceBackup => "置き換えバックアップ情報（過去の記録）",
            Message::RecoveryHistoricalCrossVolumeMoveReplace => {
                "volume をまたぐ Move Replace の記録（過去の情報）"
            }
            Message::RecoveryHistoricalZipCreate => "ZIP 作成の記録（過去の情報）",
            Message::RecoveryHistoricalZipExtract => "ZIP 展開の記録（過去の情報）",
            Message::RecoveryZipStagePath => "一時パス",
            Message::RecoveryZipPhase => "記録された段階",
            Message::RecoveryMoveStagePath => "ステージ先",
            Message::RecoveryMovePhase => "最後に記録された段階",
            Message::RecoveryBackupSource => "元のパス",
            Message::RecoveryBackupDestination => "移動先",
            Message::RecoveryBackupPath => "バックアップ先",
            Message::RecoveryHistoricalRenameStage => "名前変更の段階情報（過去の記録）",
            Message::RecoveryRenameStageSource => "元のパス",
            Message::RecoveryRenameStagePath => "一時パス",
            Message::RecoveryRenameStageDestination => "最終パス",
            Message::RecoveryRenameStagePhase => "記録された段階",
            Message::RecoveryHistoricalDirectoryMoveReplace => {
                "同一ボリュームのフォルダー置き換え移動（過去の記録）"
            }
            Message::RecoveryDirectoryMovePhase => "記録された段階",
            Message::RecoverySourceLocation => "元データの記録位置",
            Message::RecoveryTargetLocation => "置き換え対象の記録位置",
            Message::RecoveryLocationOriginal => "元の場所",
            Message::RecoveryLocationPublished => "移動先に公開済み",
            Message::RecoveryLocationDestination => "移動先",
            Message::RecoveryLocationBackup => "バックアップ",
            Message::RecoveryLocationDisposed => "処分済みと記録",
            Message::RecoveryLocationUncertain => "不確実",
            Message::RecoveryCleanupSummary => "記録された後片付けの概要",
            Message::RecoveryCleanupChildren => "子項目の処分記録",
            Message::RecoveryBackupRootDisposal => "バックアップルートの処分記録",
            Message::RecoveryBackupRootRecorded => "記録あり",
            Message::RecoveryBackupRootNotRecorded => "記録なし",
            Message::RecoveryPendingCleanup => "保留中の後片付け意図",
            Message::RecoveryCleanupOrdinal => "順序番号",
            Message::RecoveryCleanupNodeKind => "項目の種類",
            Message::RecoveryNodeFile => "ファイル",
            Message::RecoveryNodeDirectory => "フォルダー",
            Message::RecoveryNodeUnknown => "不明",
            Message::RecoveryBackupRoot => "バックアップルート",
            Message::RecoveryRelativePath => "相対パス",
            Message::RecoveryMovePhasePrepared => "準備済み",
            Message::RecoveryMovePhaseNoEffect => "効果なし",
            Message::RecoveryMovePhaseDisplaceIntent => "対象退避の意図",
            Message::RecoveryMovePhaseTargetDisplaced => "対象退避済み",
            Message::RecoveryMovePhasePublishIntent => "公開の意図",
            Message::RecoveryMovePhaseDestinationPublished => "移動先へ公開済み",
            Message::RecoveryMovePhaseBackupCleanup => "バックアップ後片付け",
            Message::RecoveryMovePhaseBackupComplete => "バックアップ完了",
            Message::RecoveryMovePhaseComplete => "完了",
            Message::RecoveryNoneRecorded => "記録なし",
            Message::RecoveryDetails => "詳細",
            Message::RecoveryHideDetails => "詳細を閉じる",
            Message::RecoveryDismiss => "破棄",
            Message::RecoveryNewCopy => "現在の元データから新しいコピーを開始",
            Message::RecoveryNewCopyFailed => {
                "元データまたは保存先を確認できないため、新しいコピーを開始できませんでした"
            }
            Message::RecoverySaving => "保存中…",
            Message::RecoveryPreviousPage => "前",
            Message::RecoveryNextPage => "次",
            Message::RecoveryDetailPage => "詳細ページ",
            Message::RecoveryMoreRecords => "このページより後にも記録があります",
            Message::RecoveryPreviousRecords => "前",
            Message::RecoveryNextRecords => "次",
            Message::RecoverySourceSummary => "記録された元のパス",
            Message::RecoveryShortcutHint => "Alt+Rで復旧操作へ移動",
            Message::OperationJournalPreserved => "ファイル操作の記録を保持しました",
            Message::FolderPreferencesNotSaved => "フォルダー設定を保存できませんでした",
            Message::SavedPaneRestoreIncomplete => {
                "保存されたペインをすべて復元できないため、セッションの保存を停止しています"
            }
            Message::Dismiss => "閉じる",
            Message::KeymapNotLoaded => "キーボード割り当てを読み込めません",
            Message::MoreNotices => "ほかの通知",
            Message::LayoutRows => "行",
            Message::LayoutTiles => "タイル",
            Message::LayoutIcons => "アイコン",
            Message::GroupNone => "整理なし",
            Message::GroupByDate => "日付で整理",
            Message::GroupByKind => "種類で整理",
            Message::GroupDateToday => "今日",
            Message::GroupDateYesterday => "昨日",
            Message::GroupDateThisWeek => "今週",
            Message::GroupDateLastWeek => "先週",
            Message::GroupDateThisMonth => "今月",
            Message::GroupDateLastMonth => "先月",
            Message::GroupDateThisYear => "今年",
            Message::GroupDateLastYear => "去年",
            Message::GroupDateEarlier => "かなり前",
            Message::GroupDateUnknown => "-",
            Message::ToggleColumnKind => "種類",
            Message::ToggleColumnSize => "サイズ",
            Message::ToggleColumnModified => "更新日時",
            Message::MoveNameLeft => "名前を左へ",
            Message::MoveNameRight => "名前を右へ",
            Message::MoveKindLeft => "種類を左へ",
            Message::MoveKindRight => "種類を右へ",
            Message::MoveSizeLeft => "サイズを左へ",
            Message::MoveSizeRight => "サイズを右へ",
            Message::MoveModifiedLeft => "更新日時を左へ",
            Message::MoveModifiedRight => "更新日時を右へ",
            Message::ToggleFileInfoPane => "ファイル情報",
            Message::ShowHiddenItems => "隠し項目を表示",
            Message::ItemCheckBoxes => "項目のチェックボックス",
            Message::FileInfoKind => "種類",
            Message::FileInfoSize => "サイズ",
            Message::FileInfoModified => "更新日時",
            Message::FileInfoLocation => "場所",
            Message::FileInfoLinkTarget => "リンク先",
            Message::FileInfoEmpty => "対象がありません",
            Message::FileInfoMultiple => "複数選択",
            Message::FileInfoItems => "項目数",
            Message::FileInfoFiles => "ファイル数",
            Message::FileInfoFolders => "フォルダ数",
            Message::FileInfoTotalSize => "合計サイズ",
            Message::FileInfoCalculating => "集計中…",
            Message::FileInfoSizeUnknown => "不明",
            Message::FileInfoCopyName => "名前をコピー",
            Message::FileInfoCopyLocation => "場所をコピー",
            Message::FileInfoCopyNames => "名前をコピー",
            Message::FileInfoCopyPaths => "パスをコピー",
            Message::FileInfoHash => "SHA-256",
            Message::FileInfoHashLoading => "計算中…",
            Message::FileInfoHashFailed => "取得できませんでした",
            Message::FileInfoHashUnavailable => "利用できません",
            Message::FileInfoExif => "EXIF",
            Message::FileInfoExifLoading => "読み込み中…",
            Message::FileInfoExifNone => "なし",
            Message::FileInfoExifFailed => "取得できませんでした",
            Message::FileInfoExifMake => "メーカー",
            Message::FileInfoExifModel => "モデル",
            Message::FileInfoExifDateTimeOriginal => "撮影日時",
            Message::FileInfoExifLensModel => "レンズ",
            Message::FileInfoExifExposureTime => "露出時間",
            Message::FileInfoExifFNumber => "F値",
            Message::FileInfoExifPhotographicSensitivity => "ISO",
            Message::FileInfoExifFocalLength => "焦点距離",
            Message::FileInfoShortcut => "ショートカット",
            Message::FileInfoShortcutTarget => "リンク先",
            Message::FileInfoShortcutStatus => "状態",
            Message::FileInfoShortcutValid => "有効",
            Message::FileInfoShortcutBroken => "リンク切れ",
            Message::FileInfoShortcutWorkingDirectory => "作業フォルダー",
            Message::FileInfoShortcutArguments => "引数",
            Message::FileInfoShortcutLoading => "読み込み中…",
            Message::FileInfoShortcutFailed => "取得できませんでした",
            Message::MenuFile => "ファイル",
            Message::MenuEdit => "編集",
            Message::MenuView => "表示",
            Message::MenuTheme => "テーマ",
            Message::MenuLanguage => "言語",
            Message::MenuLayout => "レイアウト",
            Message::MenuColumns => "列",
            Message::MenuCompression => "圧縮",
            Message::MenuCreation => "作成",
            Message::SevenZipProvider => "7-Zip",
            Message::SevenZipUnknown => "状態不明",
            Message::SevenZipDiscovering => "確認中…",
            Message::SevenZipUnavailable => "利用できません",
            Message::SevenZipAvailable => "利用可能",
            Message::SevenZipFormatCreate => "作成対応",
            Message::LanguageSystem => "システム設定",
            Message::LanguageJapanese => "日本語",
            Message::LanguageEnglish => "英語",
            #[cfg(debug_assertions)]
            Message::LanguagePseudo => "疑似ロケール（デバッグ）",
            Message::SearchTitle => "Everything 検索",
            Message::SearchTypeToSearch => "入力して検索",
            Message::SearchUsage => "ファイル名やパスの一部を入力して検索します",
            Message::SearchExamples => "例: report / *.png / C:\\Users\\ / folder:src",
            Message::SearchActivationHint => {
                "フォルダーは現在のタブで開き、ファイルは既定のアプリで開きます"
            },
            Message::SearchSearching => "検索中…",
            Message::SearchFailed => "検索に失敗しました",
            Message::SearchProviderChanged => "検索中に Everything の索引が変更されました",
            Message::BatchRenameTitle => "一括名前変更",
            Message::BatchRenamePattern => "名前のパターン",
            Message::BatchRenameFind => "検索文字列",
            Message::BatchRenameReplace => "置換文字列",
            Message::BatchRenameCancel => "キャンセル",
            Message::BatchRenameSubmit => "名前を変更",
            Message::BatchRenameNoChanges => "変更なし",
            Message::BatchRenameListingChanged => {
                "一覧が変わりました。プレビューを閉じて開き直してください。"
            }
            Message::ConflictHeading => "同じ名前の項目が既にあります",
            Message::ConflictAlreadyExists => "は既にあります",
            Message::ConflictSkip => "スキップ",
            Message::ConflictKeepBoth => "両方保持",
            Message::ConflictReplace => "置き換え",
            Message::ConflictApplyToRemaining => "残りにも適用",
            Message::ConflictCancel => "キャンセル",
            Message::ConflictWaiting => "競合の解決を待っています…",
            Message::MenuGo => "移動",
            Message::NewWindow => "新しいウィンドウ",
            Message::NewWindowFailed => "新しいウィンドウを開けませんでした",
            Message::ThisPc => "PC",
            Message::Refresh => "読み直し",
            Message::NavigateBack => "戻る",
            Message::NavigateForward => "進む",
            Message::Activate => "開く",
            Message::ActivateNewTab => "新しいタブで開く",
            Message::OpenInNewPane => "新しいペインで開く",
            Message::CopyPaths => "パスをコピー",
            Message::CopyNames => "名前をコピー",
            Message::CreateZip => "ZIP に圧縮",
            Message::CreateShortcut => "ショートカットを作成",
            Message::Create7z => "7z に圧縮",
            Message::ExtractToFolder => "フォルダーに展開",
            Message::OpenInWindowsTerminal => "Windows Terminal で開く",
            Message::WindowsTerminalLaunchFailed => "Windows Terminal を起動できませんでした",
            Message::Rename => "名前を変更",
            Message::Properties => "プロパティ",
            Message::PasteLinkHere => "ここにシンボリックリンクを作成",
            Message::PasteJunctionHere => "ここにジャンクションを作成",
            Message::CreateFolder => "新しいフォルダー",
            Message::CreateFile => "新しいファイル",
            Message::SelectExtensionOnRename => "名前変更時に拡張子も選択",
            Message::ReloadKeymap => "キーボード割り当てを再読み込み",
            Message::TabDuplicate => "タブを複製",
            Message::TabClose => "閉じる",
            Message::TabCloseOthers => "他のタブを閉じる",
            Message::TabCloseToRight => "下のタブを閉じる",
            Message::TabLock => "タブをロック",
            Message::TabUnlock => "タブのロックを解除",
            Message::QuickPreviewLoading => "プレビューを読み込み中",
            Message::QuickPreviewFailed => "プレビューを読み込めません",
            Message::QuickPreviewFallback => "プレビュー",
            Message::QuickPreviewEncoding => "文字コード",
            Message::QuickPreviewLossy => "置換文字あり",
            Message::QuickPreviewTruncated => "表示を省略しました",
            Message::QuickPreviewDuration => "長さ",
            Message::QuickPreviewDimensions => "サイズ",
            Message::QuickPreviewSize => "ファイルサイズ",
            Message::ThumbnailGenerationUnavailable => "生成不可",
            Message::ThumbnailProcessingFailed => "生成失敗",
            Message::ThumbnailFolderUnavailable => "読込失敗",
            Message::ThumbnailFolderNoPreview => "見本なし",
        };
        let text = match self.locale {
            Locale::Japanese => japanese,
            Locale::English => english(message).unwrap_or(japanese),
        };
        #[cfg(debug_assertions)]
        if self.pseudo {
            return Cow::Owned(self.expand(text));
        }
        Cow::Borrowed(text)
    }

    pub(crate) fn item_count(&self, count: usize) -> String {
        self.expand(match self.locale {
            Locale::Japanese => format!("{} 個", group_source_decimal(count)),
            Locale::English => format!("{} items", group_source_decimal(count)),
        })
    }

    pub(crate) fn search_result_count(&self, count: usize) -> String {
        self.expand(match self.locale {
            Locale::Japanese => format!("検索結果: {} 件", group_source_decimal(count)),
            Locale::English => format!(
                "{} {}",
                group_source_decimal(count),
                if count == 1 { "result" } else { "results" }
            ),
        })
    }

    pub(crate) fn multiple_selection_rows(
        &self,
        selected_count: usize,
        aggregate: Option<(usize, usize, Option<u64>, usize)>,
    ) -> Vec<String> {
        let mut rows = vec![format!(
            "{}: {}",
            self.text(Message::FileInfoItems),
            group_source_decimal(selected_count)
        )];
        for (label, count) in [
            (Message::FileInfoFiles, aggregate.map(|value| value.0)),
            (Message::FileInfoFolders, aggregate.map(|value| value.1)),
        ] {
            let value = count.map_or_else(
                || self.text(Message::FileInfoCalculating).into_owned(),
                group_source_decimal,
            );
            rows.push(format!("{}: {value}", self.text(label)));
        }
        let total = match aggregate {
            None => self.text(Message::FileInfoCalculating).into_owned(),
            Some((_, _, Some(size), _)) => self.size(Some(size)),
            Some((_, _, None, unknown)) if unknown > 0 => match self.locale {
                Locale::Japanese => format!(
                    "{}（{} 件のサイズ未取得）",
                    self.text(Message::FileInfoSizeUnknown),
                    group_source_decimal(unknown)
                ),
                Locale::English => format!(
                    "{} ({} sizes unavailable)",
                    self.text(Message::FileInfoSizeUnknown),
                    group_source_decimal(unknown)
                ),
            },
            Some((_, _, None, _)) => self.text(Message::FileInfoSizeUnknown).into_owned(),
        };
        rows.push(format!(
            "{}: {total}",
            self.text(Message::FileInfoTotalSize)
        ));
        rows.into_iter().map(|row| self.expand(row)).collect()
    }

    /// 一覧の件数と選択数の行。v5 と同じく、選択モード中だけその旨を足す。
    pub(crate) fn list_status(&self, total: usize, selected: usize, multiple: bool) -> String {
        let mut text = match self.locale {
            Locale::Japanese => format!(
                "{} 個 · {} 件を選択",
                group_source_decimal(total),
                group_source_decimal(selected)
            ),
            Locale::English => format!(
                "{} items · {} selected",
                group_source_decimal(total),
                group_source_decimal(selected)
            ),
        };
        if multiple {
            text.push_str(" · ");
            text.push_str(&self.text(Message::MultipleSelection));
        }
        self.expand(text)
    }

    /// 一覧の状態行。読取中と失敗を区別し、失敗時は last-good を残しているかも伝える。
    ///
    /// 空一覧と読取失敗を混ぜないため、失敗は必ず理由付きで出す。
    pub(crate) fn listing_status(
        &self,
        loading: bool,
        error: Option<&str>,
        has_last_good: bool,
    ) -> Option<String> {
        match (loading, error) {
            (_, Some(error)) => {
                let label = if has_last_good {
                    self.text(Message::RefreshFailed)
                } else {
                    self.text(Message::LoadFailed)
                };
                Some(format!("{label}: {error}"))
            }
            (true, None) if has_last_good => Some(self.text(Message::Refreshing).into_owned()),
            (true, None) => Some(self.text(Message::Loading).into_owned()),
            (false, None) => None,
        }
    }

    /// 自動更新が無いことを伝える。
    ///
    /// `Degraded` を session の中だけに置くと、「変化がなかった」と「もう観測して
    /// いない」をユーザーが区別できない。F5 は残るので、その旨も添える。
    pub(crate) fn watch_degraded(&self, reason: &str) -> String {
        format!("{}: {reason}", self.text(Message::WatchDegraded))
    }

    /// セッションの保存が止まっていることを伝える。
    ///
    /// 理由を出さないと、次に起動したときタブが戻らない原因が分からない。
    pub(crate) fn session_not_saved(&self, reason: &str) -> String {
        format!("{}: {reason}", self.text(Message::SessionNotSaved))
    }

    pub(crate) fn session_not_saved_detail(&self, message: Message, detail: &str) -> String {
        self.session_not_saved(&format!("{}: {detail}", self.text(message)))
    }

    /// file-operation runtime が要求を受理できなかったことを伝える。
    #[cfg(test)]
    pub(crate) fn operation_submit_failed(&self, error: SubmitError) -> String {
        self.text(Self::operation_submit_message(error))
            .into_owned()
    }

    pub(crate) fn operation_submit_message(error: SubmitError) -> Message {
        match error {
            SubmitError::CapacityReached => Message::OperationCapacityReached,
            SubmitError::IdExhausted => Message::OperationIdUnavailable,
            SubmitError::IdentityUnavailable => Message::OperationIdentityUnavailable,
            SubmitError::InvalidRequest => Message::OperationInvalidRequest,
        }
    }

    /// keymap を読めなかったことを伝える。
    ///
    /// 既定の binding で動いているので、理由を示して直せるようにする。
    pub(crate) fn keymap_not_loaded(&self, reason: &str) -> String {
        format!("{}: {reason}", self.text(Message::KeymapNotLoaded))
    }

    /// 畳んだ通知の件数。全部は起動ログにある。
    pub(crate) fn more_notices(&self, count: usize) -> String {
        self.expand(match self.locale {
            Locale::Japanese => format!(
                "{} {count} 件（起動ログに出力）",
                self.text(Message::MoreNotices)
            ),
            Locale::English => format!(
                "{} ({count} in startup log)",
                self.text(Message::MoreNotices)
            ),
        })
    }

    /// ファイルサイズを整形する。v5 の `format_size_bytes` と同じ 1024 進と桁数。
    /// フォルダは Everything 連携まで未取得なので空文字にする。
    pub(crate) fn size(&self, size: Option<u64>) -> String {
        let Some(size) = size else {
            return String::new();
        };
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * 1024.0;
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        let size_f = size as f64;
        self.expand(if size_f < KB {
            format!("{size} B")
        } else if size_f < MB {
            format!("{:.1} KB", size_f / KB)
        } else if size_f < GB {
            format!("{:.1} MB", size_f / MB)
        } else {
            format!("{:.1} GB", size_f / GB)
        })
    }

    /// 更新日時を整形する。v5 の `format_modified_time` と同じ `YYYY-MM-DD HH:MM`。
    /// 取得できなかった項目は v5 と同じく `-` にする。
    pub(crate) fn modified(&self, modified: Option<SystemTime>) -> String {
        let Some(modified) = modified else {
            return self.expand("-");
        };
        let local: DateTime<Local> = DateTime::from(modified);
        self.expand(local.format("%Y-%m-%d %H:%M").to_string())
    }
}

/// 英語カタログ。未登録の項目は既存の日本語文面へ戻る。
fn english(message: Message) -> Option<&'static str> {
    Some(match message {
        Message::NewTab => "New tab",
        Message::NoClosedWorkspaces => "No closed workspaces",
        Message::ClosedWorkspaces => "Closed workspaces",
        Message::DuplicateWorkspace => "Duplicate",
        Message::WorkspaceCopyPrefix => "Copy of ",
        Message::WorkspaceCopySuffix => "",
        Message::WorkspaceCapacityReached => {
            "Cannot add a workspace: the workspace, pane, or tab limit has been reached"
        }
        Message::WorkspaceDuplicateFailed => "Could not duplicate the workspace",
        Message::NoClosedTabs => "No closed tabs",
        Message::ClosedTabHistoryLimit => {
            "Closed tab history reached its limit; the oldest item was removed"
        }
        Message::ClosedTabHistoryRecordTooLarge => {
            "This tab exceeds the history storage limit and could not be saved"
        }
        Message::ClearClosedTabHistoryAndRetry => {
            "Clear closed tab and workspace history and retry"
        }
        Message::SampleList => "Sample list",
        Message::ColumnName => "Name",
        Message::ColumnKind => "Kind",
        Message::ColumnSize => "Size",
        Message::ColumnModified => "Date modified",
        Message::Loading => "Loading",
        Message::LoadFailed => "Load failed",
        Message::Refreshing => "Refreshing",
        Message::RefreshFailed => "Refresh failed",
        Message::Retry => "Retry",
        Message::ClearSelection => "Clear selection",
        Message::MultipleSelection => "Multiple selection",
        Message::WatchDegraded => "Automatic updates unavailable",
        Message::SessionNotSaved => "Session could not be saved",
        Message::OperationCapacityReached => {
            "Too many operations are active; this file operation could not start"
        }
        Message::OperationRetryRejected => "This file operation could not be retried",
        Message::UndoOperationRejected => "Undo could not be confirmed; its record was kept",
        Message::UndoOperationPartial => {
            "Undo published a restore but could not finish; moved items may remain"
        }
        Message::MoveInspectionPending => "Checking move results…",
        Message::MoveInspectionReview => "Review",
        Message::CopyInspectionPending => "Checking copy results…",
        Message::CopyInspectionReview => "Review",
        Message::CopyInspectionRetry => "Retry unpublished items",
        Message::CopyInspectionDeleteStage => "Delete retained stage",
        Message::CopyInspectionDeleteRejected => "Could not delete retained stage",
        Message::CopyStageCleanupRunning => "Deleting retained Copy stage…",
        Message::CopyCleanupExplanation => {
            "This retained temporary stage may contain partially copied data. This action deletes only this exact stage; it does not delete the source or destination."
        }
        Message::CopyCleanupStagePath => "Stage path",
        Message::CopyPublicationHistory => "Publication history",
        Message::CopyPublicationNotPublished => "Not published",
        Message::CopyPublicationPublished => "Published",
        Message::CopyPublicationUnknown => "Publication unknown",
        Message::CopyObservationSource => "Source at inspection",
        Message::CopyObservationParent => "Current destination parent",
        Message::CopyObservationDestination => "Destination at inspection",
        Message::CopyObservationStage => "Stage at inspection",
        Message::CopyObservationStageContent => "Stage content (not compared)",
        Message::CopyObservationMatches => "matches",
        Message::CopyObservationChanged => "changed",
        Message::CopyObservationAbsent => "absent",
        Message::CopyObservationIndeterminate => "indeterminate",
        Message::MoveRetryVerified => "Retry verified items",
        Message::MoveInspectionUnavailable => "Unavailable: safety conditions are not met",
        Message::MoveSourceIdentity => "Source identity",
        Message::MoveDestinationParentIdentity => "Destination parent identity",
        Message::MoveIdentityMatches => "matches",
        Message::MoveIdentityMismatch => "does not match",
        Message::MoveDestinationState => "Destination",
        Message::MoveDestinationAbsent => "absent",
        Message::MoveDestinationPresent => "present",
        Message::MoveDestinationIndeterminate => "indeterminate",
        Message::PublishedMoveReview => "Review move result",
        Message::DirectoryMoveReview => "Review move record",
        Message::DirectoryMoveChildSummary => "Recorded child dispositions",
        Message::DirectoryMovePendingChild => "A child disposition is pending",
        Message::DirectoryMovePendingRoot => "The root disposition is pending",
        Message::DirectoryMoveRootSummary => "Move root disposition",
        Message::DirectoryMoveCleanupSummary => "Recorded cleanup completion",
        Message::DirectoryMovePending => "pending",
        Message::DirectoryMoveNotPending => "none pending",
        Message::DirectoryMoveRecorded => "recorded",
        Message::DirectoryMoveNotRecorded => "not recorded",
        Message::DirectoryMoveUnavailable => {
            "No ordinary directory Move ledger is available for this review."
        }
        Message::CrossVolumeMoveRetryVerified => "Retry verified cross-volume Move",
        Message::CrossVolumeMoveRetrySummary => {
            "Cross-volume Move: retry is limited to items whose source and destination parent identities match and whose destination is absent."
        }
        Message::PublishedMoveSourceRetained => "Destination published; source retained",
        Message::PublishedMoveDispositionAccepted => {
            "Worker accepted publication and source disposition; inspect current state"
        }
        Message::PublishedMoveDestinationMatches => "matches published destination",
        Message::PublishedMoveDestinationChanged => "destination changed",
        Message::PublishedMoveDestinationAbsent => "destination absent",
        Message::PublishedMoveDestinationIndeterminate => "destination indeterminate",
        Message::MoveCandidateVerified => "Verified no effect; retry is available",
        Message::MoveCandidateUnavailable => {
            "Unavailable: receipt or identity checks are unsafe or indeterminate"
        }
        Message::OperationIdUnavailable => {
            "Could not start the operation. Restart the application and try again"
        }
        Message::OperationIdentityUnavailable => {
            "Could not create a safe operation ID; the file operation cannot start"
        }
        Message::OperationInvalidRequest => "The file operation request is invalid",
        Message::ExternalMoveCleanupQueueRejected => {
            "OLE reported a move, but source cleanup could not be queued. The source files remain"
        }
        Message::BatchRenamePreviewStale => "The batch rename preview is stale",
        Message::BatchRenameUndoNotQueued => "Could not queue batch rename undo",
        Message::ReplaceRecoveryIntentNotSaved => {
            "Replace was cancelled because its recovery intent could not be saved"
        }
        Message::CaseRenameRecoveryIntentNotSaved => {
            "Case-only rename was cancelled because its recovery intent could not be saved"
        }
        Message::BatchRenameItemNotSaved => {
            "Batch rename stopped because its completed item could not be saved"
        }
        Message::BatchRenamePlanNotSaved => {
            "Batch rename was cancelled because its plan could not be saved"
        }
        Message::RecoveryRecordsCouldNotDismiss => "Recovery records could not be dismissed",
        Message::RecoveryKindCopy => "Copy",
        Message::RecoveryKindMove => "Move",
        Message::RecoveryKindSymbolicLink => "Create symbolic link",
        Message::RecoveryKindJunction => "Create junction",
        Message::RecoveryKindNewFolder => "Create folder",
        Message::RecoveryKindNewFile => "Create file",
        Message::RecoveryKindCreateShortcut => "Create shortcut",
        Message::OperationCreatingShortcut => "Creating shortcut…",
        Message::RecoveryKindTrash => "Trash",
        Message::RecoveryKindRename => "Rename",
        Message::RecoveryKindBatchRename => "Batch rename",
        Message::RecoveryKindBatchRenameUndo => "Batch rename undo",
        Message::RecoveryKindCreateZip => "Create ZIP",
        Message::RecoveryKindCreate7z => "Create 7z archive",
        Message::RecoveryKindExtractZip => "Extract ZIP",
        Message::RecoveryHeading => "Interrupted file operations",
        Message::RecoveryHistoricalWarning => {
            "Recorded paths are historical details and do not identify current filesystem objects"
        }
        Message::RecoveryRecordedSources => "Recorded source paths (historical)",
        Message::RecoveryRecordedDestination => "Recorded destination (historical)",
        Message::RecoveryCreatedFolder => "Created folder (historical)",
        Message::RecoveryPlanDestinations => "Batch rename plan destinations (historical)",
        Message::RecoveryRecordedTerminalOutcome => "Recorded terminal outcome",
        Message::RecoverySourceRootNow => "Current source root observation",
        Message::RecoveryDestinationRootNow => "Current destination root observation",
        Message::RecoveryOutcomeSucceeded => "Succeeded",
        Message::RecoveryOutcomeFailed => "Failed",
        Message::RecoveryOutcomeCancelled => "Cancelled",
        Message::RecoveryOutcomePartial => "Partial",
        Message::RecoveryOutcomeUnknown => "Unknown",
        Message::RecoveryCompletedRenameReceipts => "Completed batch rename receipts (historical)",
        Message::RecoveryHistoricalReplaceBackup => "Replace backup intent (historical)",
        Message::RecoveryHistoricalCrossVolumeMoveReplace => {
            "Cross-volume Move Replace (historical)"
        }
        Message::RecoveryHistoricalZipCreate => "ZIP creation facts (historical)",
        Message::RecoveryHistoricalZipExtract => "ZIP extraction facts (historical)",
        Message::RecoveryZipStagePath => "Stage path",
        Message::RecoveryZipPhase => "Recorded phase",
        Message::RecoveryMoveStagePath => "Stage path",
        Message::RecoveryMovePhase => "Last durable phase",
        Message::RecoveryBackupSource => "Source",
        Message::RecoveryBackupDestination => "Destination",
        Message::RecoveryBackupPath => "Backup path",
        Message::RecoveryHistoricalRenameStage => "Rename stage intent (historical)",
        Message::RecoveryRenameStageSource => "Source",
        Message::RecoveryRenameStagePath => "Stage path",
        Message::RecoveryRenameStageDestination => "Destination",
        Message::RecoveryRenameStagePhase => "Recorded phase",
        Message::RecoveryHistoricalDirectoryMoveReplace => {
            "Same-volume directory Move Replace (historical)"
        }
        Message::RecoveryDirectoryMovePhase => "Recorded phase",
        Message::RecoverySourceLocation => "Recorded source location",
        Message::RecoveryTargetLocation => "Recorded target location",
        Message::RecoveryLocationOriginal => "Original",
        Message::RecoveryLocationPublished => "Published at destination",
        Message::RecoveryLocationDestination => "Destination",
        Message::RecoveryLocationBackup => "Backup",
        Message::RecoveryLocationDisposed => "Recorded as disposed",
        Message::RecoveryLocationUncertain => "Uncertain",
        Message::RecoveryCleanupSummary => "Recorded cleanup summary",
        Message::RecoveryCleanupChildren => "Child disposition receipts",
        Message::RecoveryBackupRootDisposal => "Backup root disposition receipt",
        Message::RecoveryBackupRootRecorded => "Recorded",
        Message::RecoveryBackupRootNotRecorded => "Not recorded",
        Message::RecoveryPendingCleanup => "Pending cleanup intent",
        Message::RecoveryCleanupOrdinal => "Ordinal",
        Message::RecoveryCleanupNodeKind => "Node kind",
        Message::RecoveryNodeFile => "File",
        Message::RecoveryNodeDirectory => "Directory",
        Message::RecoveryNodeUnknown => "Unknown",
        Message::RecoveryBackupRoot => "Backup root",
        Message::RecoveryRelativePath => "Relative path",
        Message::RecoveryMovePhasePrepared => "Prepared",
        Message::RecoveryMovePhaseNoEffect => "No effect",
        Message::RecoveryMovePhaseDisplaceIntent => "Displace intent",
        Message::RecoveryMovePhaseTargetDisplaced => "Target displaced",
        Message::RecoveryMovePhasePublishIntent => "Publish intent",
        Message::RecoveryMovePhaseDestinationPublished => "Destination published",
        Message::RecoveryMovePhaseBackupCleanup => "Backup cleanup",
        Message::RecoveryMovePhaseBackupComplete => "Backup complete",
        Message::RecoveryMovePhaseComplete => "Complete",
        Message::RecoveryNoneRecorded => "None recorded",
        Message::RecoveryDetails => "Details",
        Message::RecoveryHideDetails => "Hide details",
        Message::RecoveryDismiss => "Dismiss",
        Message::RecoveryNewCopy => "Start a new Copy from current source contents",
        Message::RecoveryNewCopyFailed => {
            "Could not start a new Copy because a current source or destination could not be verified"
        }
        Message::RecoverySaving => "Saving…",
        Message::RecoveryPreviousPage => "Prev",
        Message::RecoveryNextPage => "Next",
        Message::RecoveryDetailPage => "Details page",
        Message::RecoveryMoreRecords => "More records are on later pages",
        Message::RecoveryPreviousRecords => "Prev",
        Message::RecoveryNextRecords => "Next",
        Message::RecoverySourceSummary => "Recorded sources",
        Message::RecoveryShortcutHint => "Alt+R focuses recovery actions",
        Message::OperationJournalPreserved => "The file-operation journal was preserved",
        Message::FolderPreferencesNotSaved => "Folder preferences were not saved",
        Message::SavedPaneRestoreIncomplete => {
            "A saved pane could not be fully restored; session saving is paused"
        }
        Message::Dismiss => "Dismiss",
        Message::KeymapNotLoaded => "Could not load keyboard bindings",
        Message::MoreNotices => "More notices",
        Message::LayoutRows => "Rows",
        Message::LayoutTiles => "Tiles",
        Message::LayoutIcons => "Icons",
        Message::GroupNone => "No grouping",
        Message::GroupByDate => "Group by date",
        Message::GroupByKind => "Group by kind",
        Message::GroupDateToday => "Today",
        Message::GroupDateYesterday => "Yesterday",
        Message::GroupDateThisWeek => "This week",
        Message::GroupDateLastWeek => "Last week",
        Message::GroupDateThisMonth => "This month",
        Message::GroupDateLastMonth => "Last month",
        Message::GroupDateThisYear => "This year",
        Message::GroupDateLastYear => "Last year",
        Message::GroupDateEarlier => "Earlier",
        Message::GroupDateUnknown => "Unknown date",
        Message::ToggleColumnKind => "Kind",
        Message::ToggleColumnSize => "Size",
        Message::ToggleColumnModified => "Date modified",
        Message::MoveNameLeft => "Move Name left",
        Message::MoveNameRight => "Move Name right",
        Message::MoveKindLeft => "Move Kind left",
        Message::MoveKindRight => "Move Kind right",
        Message::MoveSizeLeft => "Move Size left",
        Message::MoveSizeRight => "Move Size right",
        Message::MoveModifiedLeft => "Move Date modified left",
        Message::MoveModifiedRight => "Move Date modified right",
        Message::ToggleFileInfoPane => "File information",
        Message::ShowHiddenItems => "Show hidden items",
        Message::ItemCheckBoxes => "Item check boxes",
        Message::FileInfoKind => "Kind",
        Message::FileInfoSize => "Size",
        Message::FileInfoModified => "Date modified",
        Message::FileInfoLocation => "Location",
        Message::FileInfoLinkTarget => "Link target",
        Message::FileInfoEmpty => "No items",
        Message::FileInfoMultiple => "Multiple items selected",
        Message::FileInfoItems => "Items",
        Message::FileInfoFiles => "Files",
        Message::FileInfoFolders => "Folders",
        Message::FileInfoTotalSize => "Total size",
        Message::FileInfoCalculating => "Calculating…",
        Message::FileInfoSizeUnknown => "Unknown",
        Message::FileInfoCopyName => "Copy name",
        Message::FileInfoCopyLocation => "Copy location",
        Message::FileInfoCopyNames => "Copy Names",
        Message::FileInfoCopyPaths => "Copy Paths",
        Message::FileInfoHash => "SHA-256",
        Message::FileInfoHashLoading => "Calculating…",
        Message::FileInfoHashFailed => "Could not calculate",
        Message::FileInfoHashUnavailable => "Unavailable",
        Message::FileInfoExif => "EXIF",
        Message::FileInfoExifLoading => "Loading…",
        Message::FileInfoExifNone => "None",
        Message::FileInfoExifFailed => "Could not read EXIF",
        Message::FileInfoExifMake => "Manufacturer",
        Message::FileInfoExifModel => "Model",
        Message::FileInfoExifDateTimeOriginal => "Date taken",
        Message::FileInfoExifLensModel => "Lens",
        Message::FileInfoExifExposureTime => "Exposure time",
        Message::FileInfoExifFNumber => "F-number",
        Message::FileInfoExifPhotographicSensitivity => "ISO",
        Message::FileInfoExifFocalLength => "Focal length",
        Message::FileInfoShortcut => "Shortcut",
        Message::FileInfoShortcutTarget => "Target",
        Message::FileInfoShortcutStatus => "Status",
        Message::FileInfoShortcutValid => "Valid",
        Message::FileInfoShortcutBroken => "Broken link",
        Message::FileInfoShortcutWorkingDirectory => "Working directory",
        Message::FileInfoShortcutArguments => "Arguments",
        Message::FileInfoShortcutLoading => "Loading…",
        Message::FileInfoShortcutFailed => "Could not read shortcut",
        Message::MenuFile => "File",
        Message::MenuEdit => "Edit",
        Message::MenuView => "View",
        Message::MenuTheme => "Theme",
        Message::MenuLanguage => "Language",
        Message::MenuLayout => "Layout",
        Message::MenuColumns => "Columns",
        Message::MenuCompression => "Compression",
        Message::MenuCreation => "Create",
        Message::SevenZipProvider => "7-Zip",
        Message::SevenZipUnknown => "Unknown",
        Message::SevenZipDiscovering => "Checking…",
        Message::SevenZipUnavailable => "Unavailable",
        Message::SevenZipAvailable => "Available",
        Message::SevenZipFormatCreate => "create",
        Message::LanguageSystem => "System default",
        Message::LanguageJapanese => "日本語",
        Message::LanguageEnglish => "English",
        #[cfg(debug_assertions)]
        Message::LanguagePseudo => "Pseudo locale (debug)",
        Message::SearchTitle => "Everything Search",
        Message::SearchTypeToSearch => "Type to search",
        Message::SearchUsage => "Type part of a file name or path to search",
        Message::SearchExamples => "Examples: report / *.png / C:\\Users\\ / folder:src",
        Message::SearchActivationHint => {
            "Folders open in the current tab; files open with their default app."
        },
        Message::SearchSearching => "Searching…",
        Message::SearchFailed => "Search failed",
        Message::SearchProviderChanged => "The Everything index changed during the search",
        Message::BatchRenameTitle => "Batch Rename",
        Message::BatchRenamePattern => "Pattern",
        Message::BatchRenameFind => "Find",
        Message::BatchRenameReplace => "Replace",
        Message::BatchRenameCancel => "Cancel",
        Message::BatchRenameSubmit => "Rename",
        Message::BatchRenameNoChanges => "No changes",
        Message::BatchRenameListingChanged => "Listing changed; close and reopen the preview.",
        Message::ConflictHeading => "An item with the same name already exists",
        Message::ConflictAlreadyExists => "already exists",
        Message::ConflictSkip => "Skip",
        Message::ConflictKeepBoth => "Keep both",
        Message::ConflictReplace => "Replace",
        Message::ConflictApplyToRemaining => "Apply to remaining",
        Message::ConflictCancel => "Cancel",
        Message::ConflictWaiting => "Waiting for a conflict decision…",
        Message::MenuGo => "Go",
        Message::NewWindow => "New window",
        Message::NewWindowFailed => "Could not open a new window",
        Message::ThisPc => "This PC",
        Message::Refresh => "Refresh",
        Message::NavigateBack => "Back",
        Message::NavigateForward => "Forward",
        Message::Activate => "Open",
        Message::ActivateNewTab => "Open in new tab",
        Message::OpenInNewPane => "Open in new pane",
        Message::CopyPaths => "Copy paths",
        Message::CopyNames => "Copy names",
        Message::CreateZip => "Compress to ZIP",
        Message::CreateShortcut => "Create shortcut",
        Message::Create7z => "Compress to 7z",
        Message::ExtractToFolder => "Extract to folder",
        Message::OpenInWindowsTerminal => "Open in Windows Terminal",
        Message::WindowsTerminalLaunchFailed => "Could not start Windows Terminal",
        Message::Rename => "Rename",
        Message::Properties => "Properties",
        Message::PasteLinkHere => "Create symbolic link here",
        Message::PasteJunctionHere => "Create junction here",
        Message::CreateFolder => "New folder",
        Message::CreateFile => "New file",
        Message::SelectExtensionOnRename => "Select extension when renaming",
        Message::ReloadKeymap => "Reload keyboard bindings",
        Message::TabDuplicate => "Duplicate tab",
        Message::TabClose => "Close",
        Message::TabCloseOthers => "Close other tabs",
        Message::TabCloseToRight => "Close tabs to the right",
        Message::TabLock => "Lock tab",
        Message::TabUnlock => "Unlock tab",
        Message::QuickPreviewLoading => "Loading preview",
        Message::QuickPreviewFailed => "Could not load preview",
        Message::QuickPreviewFallback => "Preview",
        Message::QuickPreviewEncoding => "Text encoding",
        Message::QuickPreviewLossy => "Replacement characters present",
        Message::QuickPreviewTruncated => "Preview truncated",
        Message::QuickPreviewDuration => "Duration",
        Message::QuickPreviewDimensions => "Dimensions",
        Message::QuickPreviewSize => "File size",
        Message::ThumbnailGenerationUnavailable => "Preview unavailable",
        Message::ThumbnailProcessingFailed => "Preview generation failed",
        Message::ThumbnailFolderUnavailable => "Could not load folder",
        Message::ThumbnailFolderNoPreview => "No preview available",
    })
}

/// Preserve the prototype's existing comma-grouped source formatting without
/// choosing a locale-formatting dependency before the real backend is selected.
fn group_source_decimal(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn unsupported_rtl_windows_languages_fall_back_to_japanese() {
        assert_eq!(locale_from_windows_language_id(0x0409), Locale::English);
        assert_eq!(locale_from_windows_language_id(0x0411), Locale::Japanese);
        assert_eq!(locale_from_windows_language_id(0x0401), Locale::Japanese);
        assert_eq!(locale_from_windows_language_id(0x040d), Locale::Japanese);
    }

    #[test]
    fn pseudo_startup_override_is_debug_only_and_not_persisted() {
        assert_eq!(LanguageOverride::startup(false), LanguageOverride::System);
        #[cfg(debug_assertions)]
        {
            assert_eq!(LanguageOverride::startup(true), LanguageOverride::Pseudo);
            assert_eq!(LanguageOverride::Pseudo.persisted(), "auto");
        }
        #[cfg(not(debug_assertions))]
        assert_eq!(LanguageOverride::startup(true), LanguageOverride::System);
    }

    #[test]
    fn japanese_catalog_resolves_common_messages() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(translator.text(Message::NewTab), "新しいタブ");
        assert_eq!(translator.text(Message::SampleList), "サンプル一覧");
        assert_eq!(translator.item_count(20_000), "20,000 個");
        assert_eq!(translator.item_count(999), "999 個");
        assert_eq!(translator.text(Message::MenuEdit), "編集");
        assert_eq!(translator.text(Message::ThisPc), "PC");
        assert_eq!(translator.text(Message::NavigateBack), "戻る");
    }

    #[test]
    fn search_and_batch_rename_messages_follow_the_active_language() {
        let japanese = Translator::new(Locale::Japanese);
        assert_eq!(japanese.text(Message::SearchTitle), "Everything 検索");
        assert_eq!(japanese.text(Message::SearchTypeToSearch), "入力して検索");
        assert_eq!(japanese.text(Message::SearchSearching), "検索中…");
        assert_eq!(japanese.text(Message::SearchFailed), "検索に失敗しました");
        assert_eq!(
            japanese.text(Message::SearchProviderChanged),
            "検索中に Everything の索引が変更されました"
        );
        assert_eq!(japanese.search_result_count(3), "検索結果: 3 件");
        assert_eq!(japanese.text(Message::BatchRenameTitle), "一括名前変更");
        assert_eq!(japanese.text(Message::BatchRenamePattern), "名前のパターン");
        assert_eq!(japanese.text(Message::BatchRenameFind), "検索文字列");
        assert_eq!(japanese.text(Message::BatchRenameReplace), "置換文字列");
        assert_eq!(japanese.text(Message::BatchRenameCancel), "キャンセル");
        assert_eq!(japanese.text(Message::BatchRenameSubmit), "名前を変更");
        assert_eq!(japanese.text(Message::BatchRenameNoChanges), "変更なし");
        assert_eq!(
            japanese.text(Message::BatchRenameListingChanged),
            "一覧が変わりました。プレビューを閉じて開き直してください。"
        );
        assert_eq!(japanese.item_count(1), "1 個");

        let english = Translator::new(Locale::English);
        assert_eq!(english.text(Message::SearchTitle), "Everything Search");
        assert_eq!(english.text(Message::SearchTypeToSearch), "Type to search");
        assert_eq!(english.text(Message::SearchSearching), "Searching…");
        assert_eq!(english.text(Message::SearchFailed), "Search failed");
        assert_eq!(
            english.text(Message::SearchProviderChanged),
            "The Everything index changed during the search"
        );
        assert_eq!(english.search_result_count(1), "1 result");
        assert_eq!(english.search_result_count(2), "2 results");
        assert_eq!(english.text(Message::BatchRenameTitle), "Batch Rename");
        assert_eq!(english.text(Message::BatchRenamePattern), "Pattern");
        assert_eq!(english.text(Message::BatchRenameFind), "Find");
        assert_eq!(english.text(Message::BatchRenameReplace), "Replace");
        assert_eq!(english.text(Message::BatchRenameCancel), "Cancel");
        assert_eq!(english.text(Message::BatchRenameSubmit), "Rename");
        assert_eq!(english.text(Message::BatchRenameNoChanges), "No changes");
        assert_eq!(
            english.text(Message::BatchRenameListingChanged),
            "Listing changed; close and reopen the preview."
        );
        assert_eq!(english.item_count(2), "2 items");

        #[cfg(debug_assertions)]
        {
            let pseudo = Translator::pseudo(Locale::Japanese);
            assert_eq!(
                pseudo.text(Message::SearchTitle),
                "⟦!! Everything 検索 ·· Everything 検索 !!⟧"
            );
            assert_eq!(
                pseudo.search_result_count(2),
                "⟦!! 検索結果: 2 件 ·· 検索結果: 2 件 !!⟧"
            );
            assert_eq!(
                pseudo.text(Message::BatchRenameTitle),
                "⟦!! 一括名前変更 ·· 一括名前変更 !!⟧"
            );
            assert_eq!(
                pseudo.text(Message::BatchRenamePattern),
                "⟦!! 名前のパターン ·· 名前のパターン !!⟧"
            );
            assert_eq!(pseudo.item_count(1), "⟦!! 1 個 ·· 1 個 !!⟧");
        }
    }

    #[test]
    fn a_blocked_session_shows_the_reason() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(
            translator.session_not_saved("malformed session: expected value"),
            "セッションを保存できません: malformed session: expected value"
        );
        assert_eq!(
            translator.keymap_not_loaded("keymap.toml: binding 2: unknown action `x`"),
            "キーボード割り当てを読み込めません: keymap.toml: binding 2: unknown action `x`"
        );
        assert_eq!(
            translator.more_notices(2),
            "ほかの通知 2 件（起動ログに出力）"
        );
    }

    #[test]
    fn rejected_file_operation_has_a_specific_notice() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(
            translator.operation_submit_failed(SubmitError::CapacityReached),
            "操作が多すぎるため、このファイル操作を開始できませんでした"
        );
        assert_eq!(
            translator.operation_submit_failed(SubmitError::IdExhausted),
            "操作を開始できませんでした。アプリを再起動してください"
        );
        assert_eq!(translator.text(Message::Dismiss), "閉じる");
    }

    #[test]
    fn rejected_external_move_cleanup_explains_that_sources_remain() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(
            translator.text(Message::ExternalMoveCleanupQueueRejected),
            "OLE は移動を報告しましたが、元ファイルの後処理を予約できませんでした。元ファイルは残っています"
        );
    }

    #[test]
    fn list_status_marks_multiple_selection_only_when_active() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(
            translator.list_status(20_000, 0, false),
            "20,000 個 · 0 件を選択"
        );
        assert_eq!(
            translator.list_status(1_200, 3, true),
            "1,200 個 · 3 件を選択 · 複数選択中"
        );
    }

    #[test]
    fn listing_status_separates_loading_from_failure() {
        let translator = Translator::new(Locale::Japanese);
        // 表示するものが無いときは行を出さない。
        assert_eq!(translator.listing_status(false, None, true), None);
        // 初回の読取中。
        assert_eq!(
            translator.listing_status(true, None, false),
            Some("読み込み中".into())
        );
        // last-good を残したままの読み直し。
        assert_eq!(
            translator.listing_status(true, None, true),
            Some("読み直し中".into())
        );
        // 初回の失敗は「読み込み失敗」。空一覧と混ぜない。
        assert_eq!(
            translator.listing_status(false, Some("denied"), false),
            Some("読み込み失敗: denied".into())
        );
        // last-good があるときの失敗は「読み直し失敗」。
        assert_eq!(
            translator.listing_status(false, Some("denied"), true),
            Some("読み直し失敗: denied".into())
        );
    }

    #[test]
    fn size_matches_v5_units_and_precision() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(translator.size(Some(0)), "0 B");
        assert_eq!(translator.size(Some(512)), "512 B");
        assert_eq!(translator.size(Some(1024)), "1.0 KB");
        assert_eq!(translator.size(Some(1536)), "1.5 KB");
        assert_eq!(translator.size(Some(2 * 1024 * 1024)), "2.0 MB");
        assert_eq!(translator.size(Some(3 * 1024 * 1024 * 1024)), "3.0 GB");
        // フォルダは Everything 連携まで未取得。v5 も空文字だった。
        assert_eq!(translator.size(None), "");
    }

    #[test]
    fn modified_uses_local_time_and_falls_back_to_a_dash() {
        let translator = Translator::new(Locale::Japanese);
        assert_eq!(translator.modified(None), "-");
        // ローカル時刻に依存するため、値そのものではなく形だけを確かめる。
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let text = translator.modified(Some(time));
        assert_eq!(text.len(), "2026-09-17 13:21".len(), "{text}");
        assert!(text.contains(' '), "{text}");
        assert!(text.starts_with("20"), "{text}");
    }
}
