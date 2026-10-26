#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate};
use gpui::StyledImage as _;
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowOptions, div,
    prelude::FluentBuilder as _, px, relative, size,
};
use gpui_filer_input::{
    ActionId, FocusKind, InputDevice, InputPolicy, InputRoute, InteractionPath, InteractionRouter,
    MouseChord, ScopeId,
};
use gpui_filer_resources::{IconId, ResourceAssets};
use gpui_filer_ui::{
    ACCENT, BORDER, Icon, RADIUS_MD, RADIUS_SM, SCROLLBAR_THUMB, SCROLLBAR_THUMB_HOVER,
    SURFACE_BASE, SURFACE_RAISED, SelectionCheckbox, TEXT, TEXT_MUTED,
};
use gpui_filer_virtual_list::RowHeights;
use location::{Location, ShellRoot};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::atomic::Ordering,
};
mod archive_listing;
mod archive_manifest;
mod archive_provider;
mod bounds_capture;
mod directory_row_surface;
mod directory_watch;
mod rename_editor;
mod scrollbar;
mod selection;
use bounds_capture::BoundsCapture;
use selection::{FocusTarget, RangeSelectionState, RectangleMode, SectionKey, Selection, SelectionIntent};

const ESTIMATED_ROW_HEIGHT: f32 = 44.0;
const VISIBLE_ROW_OVERSCAN: usize = 2;
const INITIAL_VIEWPORT_HEIGHT: f32 = 560.0;
const HEADER_HEIGHT: f32 = 24.0;
const ROW_PADDING_X: f32 = 8.0;
const CELL_PADDING_X: f32 = 8.0;
const CELL_PADDING_Y: f32 = 4.0;
const CELL_GAP: f32 = 8.0;
const NAME_FONT_SIZE: f32 = 12.0;
const NAME_LINE_HEIGHT: f32 = 18.0;
/// toolbar と menu の文字。chrome の文字は揃える。
const CHROME_FONT_SIZE: f32 = 12.0;
/// checkboxの箱を隣の内容へ寄せたときの間隔。
/// `ViewLayout::columns` の最小幅は `CHECKBOX_HIT_SIZE * 2 + CHECKBOX_GAP * 2 + 16` を前提にする。
const CHECKBOX_GAP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ItemCheckBoxes(pub bool);

impl Default for ItemCheckBoxes {
    fn default() -> Self {
        Self(true)
    }
}

impl gpui::Global for ItemCheckBoxes {}

/// FolderTree menu は click 座標ではなく、実際に paint された button bounds を基準に置く。
/// 入口ごとに接続辺が違うため、shell が位置を決める際にも種別を保持する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FolderTreeAnchorKind {
    Item,
    Breadcrumb,
}

// アイコンサイズのスケール制御。タイル・アイコンビューで −/+ ボタンから操作できる。
const ICON_SCALE_DEFAULT: f32 = 1.25;
const ICON_SCALE_MIN: f32 = 0.75;
const ICON_SCALE_MAX: f32 = 2.5;
const ICON_SCALE_STEP: f32 = 0.25;

/// サムネイルを何device pxでdecodeするか。表示より小さいと拡大されてぼける。
fn preview_pixels(layout: ViewLayout, scale: f32, scale_factor: f32) -> u32 {
    (layout.icon_size(scale) * scale_factor).ceil().max(16.0) as u32
}

/// decode済み画像が必要な解像度に足りているか。
///
/// 解像度は freshness の証拠ではない。世代は別に照合する。
fn preview_covers(images: &directory::PreviewImages, needed: u32) -> bool {
    images.first().is_some_and(|image| {
        let size = image.size(0);
        size.width.0.max(size.height.0) >= needed as i32
    })
}

fn is_ico_file(entry: &DemoEntry) -> bool {
    entry.kind == EntryKind::File
        && entry.path.as_deref().is_some_and(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ico"))
        })
}

fn is_shortcut_file(entry: &DemoEntry) -> bool {
    entry.kind == EntryKind::File
        && entry.link.is_none()
        && entry.source_identity.is_some()
        && entry.path.as_deref().is_some_and(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
        })
}

fn uses_shell_icon_fallback(icon: IconId) -> bool {
    icon == IconId::File
}

fn shortcut_info_target(
    entry: &DemoEntry,
    location_generation: u64,
    presentation_revision: u64,
) -> Option<file_info_shortcut::Target> {
    if !is_shortcut_file(entry) {
        return None;
    }
    Some(file_info_shortcut::Target {
        key: entry.key.clone(),
        path: entry.path.clone()?,
        source_identity: entry.source_identity.clone()?,
        size: entry.size?,
        modified: entry.modified?,
        selection_revision: 0,
        location_generation,
        presentation_revision,
        source_view_revision: 0,
    })
}

fn shortcut_badge(icon_size: f32, inset: f32, broken: bool) -> gpui::AnyElement {
    let badge_size = (icon_size * 0.25).max(10.0);
    let icon = if broken {
        IconId::Warning
    } else {
        IconId::Link
    };
    div()
        .absolute()
        .left(px(inset))
        .bottom(px(inset))
        .size(px(badge_size + 3.0))
        .rounded_full()
        .bg(SURFACE_BASE)
        .border_1()
        .border_color(SURFACE_RAISED)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(px(badge_size)).color(TEXT.into()))
        .into_any_element()
}

/// 一つの path の表示用画像の状態。
///
/// 「まだ無い」を「待っている」と「もう出ない」に分ける。分けないと、失敗した
/// path が visible-first の待合せを永久に止め、毎 frame decode を試みてしまう。
///
/// 世代を持つのは、同じ path でも invalidation の前後で別の画像だから。
enum PreviewState {
    Ready {
        epoch: thumbnails::MediaEpoch,
        images: directory::PreviewImages,
    },
    /// 生成できなかった。明示的な invalidation まで再試行しない。
    Unavailable {
        epoch: thumbnails::MediaEpoch,
        size: u32,
        folder: bool,
        reason: thumbnails::PreviewFailureReason,
    },
}

fn clamp_scroll_offset(offset: f32, content: f32, viewport: f32) -> f32 {
    offset.clamp(0.0, (content - viewport).max(0.0))
}

/// 診断スイッチ。watch hint と thumbnail の世代を stderr へ出す。
///
/// 既定は無効。`GPUI_FILER_WATCH_LOG` があるときだけ出す。
fn log_watch_dirty() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GPUI_FILER_WATCH_LOG").is_some())
}

/// 見出しを除いた最初の項目の index。
///
/// v5 は flat index 0 を section heading（日付で整理したときの見出し）に使っていた。
/// そのため移動の起点と Home は `first_item_flat_index()` を通しており、見出しを
/// 選ばずに最初の実項目へ入るようになっていた。v6 はまだ section を持たないので
/// 先頭がそのまま最初の項目。section を導入するときは、ここが「見出しの次の行」を
/// 返すようにするだけで移動の入口が揃う。
fn first_item_index() -> usize {
    0
}

/// 矢印キーが次に置くフォーカス位置。
///
/// 上下は行を跨ぎ、左右は隣の項目へ動く。グリッドでも行の端で止めない。
/// Home/End だけは行ではなく一覧全体の先頭と末尾を指す。v5 の
/// `move_focus_home` / `move_focus_end` と同じ規則。
///
/// `focused` が `None`（まだ何も選んでいない）のときは**一覧の先頭の手前**にいるものと
/// して扱う。下・右でも上・左でも最初の項目へ入り、1 件目が飛ばない。v5 は読み込み時に
/// 最初の項目へフォーカスを置いていたので、そこから動かすのと同じ位置になる。
/// End だけは名前どおり末尾へ飛ぶ。
fn focus_target(delta: FocusDelta, focused: Option<usize>, count: usize, columns: usize) -> usize {
    let last = count.saturating_sub(1);
    let Some(current) = focused.map(|index| index.min(last)) else {
        return match delta {
            FocusDelta::End => last,
            _ => first_item_index(),
        };
    };
    match delta {
        FocusDelta::Up => current.saturating_sub(columns),
        FocusDelta::Down => (current + columns).min(last),
        FocusDelta::Left => current.saturating_sub(1),
        FocusDelta::Right => (current + 1).min(last),
        FocusDelta::Home => first_item_index(),
        FocusDelta::End => last,
        FocusDelta::PageUp | FocusDelta::PageDown => current,
    }
}

/// `Alt+Up` の行き先。path の親は通常の directory navigation で扱い、ドライブ根元だけを
/// PC へ接続する。UNC share の根元と PC 自身には親を発明しない。
fn up_destination(location: &Location) -> Option<Location> {
    match location {
        Location::ThisPc => None,
        Location::ShellRoot(_) => Some(Location::ThisPc),
        Location::ShellItem { root, .. } => Some(Location::ShellRoot(*root)),
        Location::Archive { source, components } if !components.is_empty() => {
            let mut parent = components.clone();
            parent.pop();
            Some(Location::Archive {
                source: source.clone(),
                components: parent,
            })
        }
        Location::Archive { source, .. } => source
            .parent()
            .map(|path| Location::Path(path.to_path_buf())),
        Location::Path(path) => navigation::parent_of(path)
            .map(Location::Path)
            .or_else(|| is_drive_root(path).then_some(Location::ThisPc)),
    }
}

fn can_navigate_up(location: Option<&Location>) -> bool {
    location.is_some_and(|location| up_destination(location).is_some())
}

#[cfg(windows)]
fn is_drive_root(path: &std::path::Path) -> bool {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(Component::Prefix(prefix)), Some(Component::RootDir), None)
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    )
}

#[cfg(not(windows))]
fn is_drive_root(_: &std::path::Path) -> bool {
    false
}

/// キーで解決した項目動作の対象になる項目。
///
/// `file_item` を条件に含む binding は hover 経路でしか解決しない（focus 経路の scope に
/// `FILE_ITEM` が無いため）ので、そのときは hover している項目を動かす。それ以外は
/// 矢印キーと同じで focus している項目。どちらも無ければ動かさない。
fn key_target_index(
    route: InputRoute,
    hovered: Option<usize>,
    focused: Option<usize>,
) -> Option<usize> {
    match route {
        InputRoute::Hover => hovered,
        _ => focused,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewLayout {
    Rows,
    Tiles,
    Icons,
}

impl ViewLayout {
    /// スケールを適用したアイコンのピクセルサイズ。行ビューは固定 20px。
    fn icon_size(self, scale: f32) -> f32 {
        let base = match self {
            Self::Rows => return 20.0,
            Self::Tiles => 64.0,
            Self::Icons => 80.0,
        };
        (base * scale).round()
    }

    /// アイコンを囲む余白込みのコンテナサイズ。
    fn icon_container(self, scale: f32) -> f32 {
        self.icon_size(scale) + 16.0
    }

    fn columns(self, width: f32, icon_scale: f32, item_check_boxes: bool) -> usize {
        let minimum = match self {
            Self::Rows => return 1,
            // 非アイコン部(checkbox, gap, padding, name最小幅)は固定。
            Self::Tiles => {
                self.icon_size(icon_scale) + if item_check_boxes { 216.0 } else { 164.0 }
            }
            // icon の両脇に checkbox と同幅の空きを置く。
            Self::Icons => {
                self.icon_size(icon_scale)
                    + if item_check_boxes {
                        gpui_filer_ui::CHECKBOX_HIT_SIZE * 2.0 + CHECKBOX_GAP * 2.0
                    } else {
                        0.0
                    }
                    + 16.0
            }
        };
        // 左右12px、列間16pxを引いた残りに収まる列数にする。
        ((width - 24.0 + 16.0) / (minimum + 16.0)).floor().max(1.0) as usize
    }

    fn estimated_height(self, icon_scale: f32) -> f32 {
        match self {
            Self::Rows => ESTIMATED_ROW_HEIGHT,
            Self::Tiles => self.icon_container(icon_scale).max(44.0) + 16.0,
            Self::Icons => self.icon_container(icon_scale) + 8.0 + 44.0 + 16.0,
        }
    }

    /// 保存用の値。
    fn to_value(self) -> session_store::LayoutValue {
        match self {
            Self::Rows => session_store::LayoutValue::Rows,
            Self::Tiles => session_store::LayoutValue::Tiles,
            Self::Icons => session_store::LayoutValue::Icons,
        }
    }

    fn from_value(value: session_store::LayoutValue) -> Self {
        match value {
            session_store::LayoutValue::Rows => Self::Rows,
            session_store::LayoutValue::Tiles => Self::Tiles,
            session_store::LayoutValue::Icons => Self::Icons,
        }
    }
}

mod app_paths;
mod commands;
mod diagnostics;
mod directory;
mod directory_session;
mod entry_identity;
mod everything;
mod file_info_exif;
mod file_info_hash;
mod file_info_shortcut;
mod filesystem_identity;
mod folder_tree_menu;
mod folder_view_preferences;
mod input;
mod keymap;
mod listing_status_surface;
mod location;
mod menu;
mod namespace_clipboard;
mod navigation;
mod navigation_history;
mod ole_diagnostics;
mod windows_name;
mod zip_archive;
// E1 は UI に未接続の操作ライフサイクル基盤。E2 でコマンド面へ接続する。
#[allow(dead_code)]
mod operations;
mod overlays;
mod pane_layout;
mod persistence;
mod platform_open;
mod platform_terminal;
mod quick_preview;
mod rows_layout;
mod session_store;
mod shell;
mod shell_icons;
mod show_hidden_items;
mod sorting;
mod special;
mod theme;
mod thumbnails;
mod titlebar;
mod view_controls;
mod window_coordinator;
use commands::{AppIntent, FocusDelta};
use directory::{Entry as DemoEntry, EntryKind, LinkKind};
use shell::{Locale, Message, Translator};
use sorting::{Column, SortState};

/// 一覧の項目を組み立てるための一時的な行。
///
/// PC の行は `DirectorySession` を通らずに来るので、ここで普通の `Entry` の形へ合わせる。
/// 見た目も操作も普通の folder の行と同じになる。
fn virtual_entry(
    name: String,
    target: Location,
    kind: EntryKind,
) -> (DemoEntry, entry_identity::EntryKey, Location) {
    let key = target.path().map_or_else(
        || {
            entry_identity::EntryKey::virtual_item(
                target
                    .virtual_identity()
                    .expect("non-path target has a virtual identity"),
            )
        },
        |path| entry_identity::EntryKey::path(path.to_path_buf(), kind),
    );
    let entry = DemoEntry {
        key: key.clone(),
        name,
        path: target.path().map(std::path::Path::to_path_buf),
        kind,
        link: None,
        link_target: None,
        source_identity: None,
        is_hidden: false,
        size: None,
        folder_size: None,
        modified: None,
    };
    (entry, key, target)
}

fn zip_create_context_is_eligible(
    clicked: Option<&entry_identity::EntryKey>,
    context_keys: &[entry_identity::EntryKey],
    selection_derived: bool,
    selected_keys: &[entry_identity::EntryKey],
    current_selection: &HashSet<entry_identity::EntryKey>,
    ordinary_entry_with_identities: bool,
) -> bool {
    let Some(clicked) = clicked else {
        return false;
    };
    if context_keys.is_empty()
        || context_keys.len() > 1024
        || !context_keys.contains(clicked)
        || !ordinary_entry_with_identities
    {
        return false;
    }
    if selection_derived {
        selected_keys == context_keys
            && current_selection == &selected_keys.iter().cloned().collect::<HashSet<_>>()
            && selected_keys.len() == context_keys.len()
    } else {
        context_keys.len() == 1 && context_keys[0] == *clicked
    }
}

fn zip_create_entry_is_eligible(
    path_backed: bool,
    ordinary_filesystem_entry: bool,
    non_link: bool,
    has_source_identity: bool,
    has_parent_identity: bool,
) -> bool {
    path_backed
        && ordinary_filesystem_entry
        && non_link
        && has_source_identity
        && has_parent_identity
}

fn zip_menu_revision_is_current(
    captured_generation: u64,
    captured_revision: u64,
    current_generation: u64,
    current_revision: u64,
) -> bool {
    captured_generation == current_generation && captured_revision == current_revision
}

fn zip_selection_order_is_current(
    selection_derived: bool,
    captured_selection: &[entry_identity::EntryKey],
    current_display_order: &[entry_identity::EntryKey],
) -> bool {
    !selection_derived || captured_selection == current_display_order
}

#[derive(Clone, Copy)]
enum CapturedTerminalLocation {
    Item,
    Background,
}

fn captured_terminal_request(
    path: &std::path::Path,
    identity: &filesystem_identity::FilesystemIdentity,
    current_directory: &std::path::Path,
    current_identity: &filesystem_identity::FilesystemIdentity,
    accepted_listing: bool,
    ordinary_directory: bool,
    location: CapturedTerminalLocation,
) -> Option<platform_terminal::Request> {
    let path_matches = match location {
        CapturedTerminalLocation::Item => path.parent() == Some(current_directory),
        CapturedTerminalLocation::Background => path == current_directory,
    };
    (accepted_listing
        && ordinary_directory
        && path_matches
        && identity.windows_parts().is_some()
        && current_identity.windows_parts().is_some()
        && (matches!(location, CapturedTerminalLocation::Item) || identity == current_identity))
        .then(|| platform_terminal::Request {
            path: path.to_path_buf(),
            identity: identity.clone(),
        })
}

#[cfg(test)]
fn create_zip_request_for_captured_entry(
    source: std::path::PathBuf,
    source_kind: EntryKind,
    source_identity: filesystem_identity::FilesystemIdentity,
    destination_parent_identity: filesystem_identity::FilesystemIdentity,
    view_id: u64,
    generation: u64,
    presentation_revision: u64,
) -> Option<operations::model::OperationRequest> {
    let parent = source.parent()?;
    let stem = match source_kind {
        EntryKind::File => source.file_stem()?.to_str()?.to_owned(),
        EntryKind::Folder => source.file_name()?.to_str()?.to_owned(),
    };
    let entry_name = source.file_name()?.to_str()?.to_owned();
    let destination = parent.join(format!("{stem}.zip"));
    operations::model::OperationRequest::create_zip(
        source,
        destination,
        operations::model::ZipCreateGuard {
            source_identity,
            destination_parent_identity,
            source_kind,
            entry_name,
        },
    )
    .ok()
    .map(|mut request| {
        request.origin = Some(operations::model::OperationOrigin {
            view_id,
            location_epoch: generation,
            presentation_revision,
        });
        request
    })
}

fn create_zip_request_for_captured_entries(
    anchor: &std::path::Path,
    sources: Vec<(
        std::path::PathBuf,
        EntryKind,
        filesystem_identity::FilesystemIdentity,
    )>,
    destination_parent_identity: filesystem_identity::FilesystemIdentity,
    view_id: u64,
    generation: u64,
    presentation_revision: u64,
) -> Option<operations::model::OperationRequest> {
    let parent = anchor.parent()?;
    let anchor_kind = sources.iter().find(|(path, _, _)| path == anchor)?.1;
    let stem = match anchor_kind {
        EntryKind::File => anchor.file_stem()?.to_str()?,
        EntryKind::Folder => anchor.file_name()?.to_str()?,
    };
    let destination = parent.join(format!("{stem}.zip"));
    let guards = sources
        .iter()
        .map(|(path, kind, identity)| {
            Some(operations::model::ZipCreateSourceGuard {
                source_identity: identity.clone(),
                source_kind: *kind,
                entry_name: path.file_name()?.to_str()?.to_owned(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    operations::model::OperationRequest::create_zip_many(
        sources.into_iter().map(|(path, _, _)| path).collect(),
        destination,
        operations::model::ZipArchiveCreateGuard {
            destination_parent_identity,
            sources: guards,
        },
    )
    .ok()
    .map(|mut request| {
        request.origin = Some(operations::model::OperationOrigin {
            view_id,
            location_epoch: generation,
            presentation_revision,
        });
        request
    })
}

fn visible_directory_entries(entries: &[DemoEntry], show_hidden_items: bool) -> Vec<DemoEntry> {
    entries
        .iter()
        .filter(|entry| show_hidden_items || !entry.is_hidden)
        .cloned()
        .collect()
}

#[cfg(test)]
mod hidden_items_projection_tests {
    use super::*;

    #[test]
    fn toggling_visibility_reprojects_the_same_unfiltered_entries() {
        let mut hidden = virtual_entry("hidden.txt".into(), Location::ThisPc, EntryKind::File).0;
        hidden.is_hidden = true;
        let ordinary = virtual_entry("ordinary.txt".into(), Location::ThisPc, EntryKind::File).0;
        let raw = [hidden, ordinary];

        assert_eq!(visible_directory_entries(&raw, false).len(), 1);
        assert_eq!(visible_directory_entries(&raw, true).len(), 2);
        assert_eq!(raw.len(), 2);
    }
}

fn pc_entry(row: &special::PcEntry) -> (DemoEntry, entry_identity::EntryKey, Location) {
    let (mut entry, key, target) =
        virtual_entry(row.name.clone(), row.target.clone(), EntryKind::Folder);
    entry.source_identity = row.source_identity.clone();
    (entry, key, target)
}

#[derive(Debug, PartialEq, Eq)]
enum ActivationTarget {
    Navigate(Location),
    OpenFile(std::path::PathBuf),
}

#[cfg(windows)]
fn read_archive_background(
    provider: &archive_provider::ArchiveProviderOwner,
    capability: Option<&archive_provider::Archive7zReadCapability>,
    location: &Location,
    cancelled: &std::sync::atomic::AtomicBool,
    #[cfg(test)] image_path: Option<std::path::PathBuf>,
) -> Result<archive_listing::ArchiveReadResult, directory::DirectoryError> {
    let Location::Archive { source, .. } = location else {
        return archive_listing::read(location, cancelled);
    };
    if !source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("7z"))
    {
        return archive_listing::read(location, cancelled);
    }
    let capability = capability.ok_or_else(|| {
        directory::DirectoryError("current 7z read capability is unavailable".into())
    })?;
    #[cfg(test)]
    let image = match image_path {
        Some(path) => archive_provider::application_image::lease_explicit_path_for_test(&path),
        None => archive_provider::application_image::lease_for_operation(),
    };
    #[cfg(not(test))]
    let image = archive_provider::application_image::lease_for_operation();
    let image = image.ok_or_else(|| {
        directory::DirectoryError("application image read lease is unavailable".into())
    })?;
    archive_listing::read_7z(provider, capability, image, location, cancelled)
}

fn resolve_activation_target(
    entry: &directory::Entry,
    projected_target: Option<Location>,
    seven_zip_read_available: bool,
) -> Option<ActivationTarget> {
    if let entry_identity::EntryKey::Archive {
        source,
        components,
        kind,
    } = &entry.key
    {
        return (*kind == EntryKind::Folder).then(|| {
            ActivationTarget::Navigate(Location::Archive {
                source: source.clone(),
                components: components.clone(),
            })
        });
    }

    let path = entry.path.as_ref();
    if entry.kind == EntryKind::File
        && entry.link.is_none()
        && entry
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity.kind == EntryKind::File)
        && path.is_some_and(|path| {
            path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("zip")
                    || (seven_zip_read_available && extension.eq_ignore_ascii_case("7z"))
            })
        })
    {
        return Some(ActivationTarget::Navigate(Location::Archive {
            source: path.unwrap().clone(),
            components: Vec::new(),
        }));
    }

    let target = projected_target.or_else(|| path.cloned().map(Location::Path))?;
    match entry.kind {
        EntryKind::Folder => Some(ActivationTarget::Navigate(target)),
        EntryKind::File => target
            .path()
            .map(std::path::Path::to_path_buf)
            .map(ActivationTarget::OpenFile),
    }
}

fn is_wsl_pc_key(key: &entry_identity::EntryKey) -> bool {
    matches!(key, entry_identity::EntryKey::Path { path, .. }
        if path
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("\\\\wsl.localhost\\"))
}

fn pc_icon(kind: special::PcEntryKind) -> Option<IconId> {
    match kind {
        special::PcEntryKind::Libraries => Some(IconId::Libraries),
        special::PcEntryKind::Network => Some(IconId::Network),
        special::PcEntryKind::RecycleBin => Some(IconId::RecycleBin),
        special::PcEntryKind::KnownFolder
        | special::PcEntryKind::Drive
        | special::PcEntryKind::WslDistribution => None,
    }
}

fn shell_entry(
    row: &special::ShellEntry,
) -> (DemoEntry, entry_identity::EntryKey, Option<Location>) {
    let kind = match row.kind {
        special::ShellEntryKind::Folder => EntryKind::Folder,
        special::ShellEntryKind::File => EntryKind::File,
    };
    let key = entry_identity::EntryKey::virtual_item(row.identity.clone());
    (
        DemoEntry {
            key: key.clone(),
            name: row.name.clone(),
            path: row
                .target
                .as_ref()
                .and_then(|target| target.path().map(Into::into)),
            kind,
            link: None,
            link_target: None,
            source_identity: None,
            is_hidden: false,
            size: None,
            folder_size: None,
            modified: None,
        },
        key,
        row.target.clone(),
    )
}

/// パンくずの 1 つ。
///
/// 先頭は PC で、path を持たない。それ以外は path の segment。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Crumb {
    Pc,
    ShellRoot(ShellRoot),
    ShellItem(Location),
    Path(std::path::PathBuf),
    Archive(Location),
}

/// Build the complete captured Trash scope without filesystem I/O.
///
/// Ordinary entries and supported reparse candidates keep their captured order;
/// the worker remains the authority that proves each exact candidate tag.
fn selected_trash_sources_for_selection(
    entries: &[DemoEntry],
    selection: &Selection,
    focused: Option<usize>,
) -> Option<Vec<(std::path::PathBuf, directory::EntrySourceIdentity, bool)>> {
    let indexes: Vec<usize> = if selection.len() == 0 {
        vec![focused?]
    } else {
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, _)| selection.contains(index).then_some(index))
            .collect()
    };
    let mut sources = Vec::with_capacity(indexes.len());
    for index in indexes {
        let entry = entries.get(index)?;
        if !matches!(entry.kind, EntryKind::File | EntryKind::Folder) {
            return None;
        }
        let candidate = match entry.link {
            None => false,
            Some(LinkKind::LinkOrJunction) => true,
            Some(LinkKind::OtherReparsePoint) => return None,
        };
        let path = entry.path.clone()?;
        let identity = entry.source_identity.clone()?;
        sources.push((path, identity, candidate));
    }

    // Also rejects selected indexes that became stale or no longer have a row.
    let expected_len = if selection.len() == 0 { 1 } else { selection.len() };
    if sources.len() != expected_len {
        return None;
    }
    Some(sources)
}

impl DemoEntry {
    fn icon(&self) -> IconId {
        if self.is_link() {
            return IconId::Link;
        }
        match self.kind {
            EntryKind::Folder => IconId::Folder,
            EntryKind::File => IconId::for_extension(
                std::path::Path::new(&self.name)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or(""),
            ),
        }
    }

    /// 行の Kind 列。v5 の `format_field` と同じく、フォルダは `Folder`、
    /// ファイルは拡張子を大文字にしたものを使う。
    fn kind_label(&self) -> String {
        if self.is_link() {
            return "Link".into();
        }
        match self.kind {
            EntryKind::Folder => "Folder".into(),
            EntryKind::File => std::path::Path::new(&self.name)
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| !ext.is_empty())
                .map(|ext| ext.to_ascii_uppercase())
                .unwrap_or_else(|| "FILE".into()),
        }
    }
}

/// Rows 表示で毎 frame 組み立てる detail 列の表示文字列。
///
/// filesystem の raw fact は `DemoEntry` / `DirectorySession` に残し、これは locale を
/// 越えた presentation projection として view だけが持つ。特に local time の整形を
/// 指スクロールの render path で繰り返さないために分けている。
#[derive(Clone)]
struct RowDetails {
    kind: SharedString,
    size: SharedString,
    modified: SharedString,
}

fn projected_entry_size(entry: &DemoEntry, translator: &Translator) -> String {
    let indexed_folder_size = entry.folder_size.map(|fact| fact.bytes);
    translator.size(entry.size.or(indexed_folder_size))
}

/// v5 の「日付で整理」と同じ、一覧 projection だけが持つ表示状態。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GroupBy {
    #[default]
    None,
    ModifiedDate,
    Kind,
}

impl From<GroupBy> for session_store::GroupValue {
    fn from(value: GroupBy) -> Self {
        match value {
            GroupBy::None => Self::None,
            GroupBy::ModifiedDate => Self::ModifiedDate,
            GroupBy::Kind => Self::Kind,
        }
    }
}

impl From<session_store::GroupValue> for GroupBy {
    fn from(value: session_store::GroupValue) -> Self {
        match value {
            session_store::GroupValue::None => Self::None,
            session_store::GroupValue::ModifiedDate => Self::ModifiedDate,
            session_store::GroupValue::Kind => Self::Kind,
        }
    }
}

#[derive(Clone, Debug)]
enum ProjectedRow {
    Heading {
        key: selection::SectionKey,
        label: SharedString,
        start: usize,
        end: usize,
    },
    Entries {
        start: usize,
        end: usize,
    },
    /// 種類 section が一項目だけの連続区間を、横並びの小さな column 群として描く。
    CategoryStrip {
        groups: Vec<CategoryStripGroup>,
        start: usize,
        end: usize,
    },
}

#[derive(Clone, Debug)]
struct CategoryStripGroup {
    key: selection::SectionKey,
    label: SharedString,
    index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ModifiedBucket {
    Unknown,
    VeryOld,
    LastYear,
    ThisYear,
    LastMonth,
    ThisMonth,
    LastWeek,
    ThisWeek,
    Yesterday,
    Today,
}

impl From<ModifiedBucket> for selection::ModifiedDateSection {
    fn from(value: ModifiedBucket) -> Self {
        match value {
            ModifiedBucket::Unknown => Self::Unknown,
            ModifiedBucket::VeryOld => Self::VeryOld,
            ModifiedBucket::LastYear => Self::LastYear,
            ModifiedBucket::ThisYear => Self::ThisYear,
            ModifiedBucket::LastMonth => Self::LastMonth,
            ModifiedBucket::ThisMonth => Self::ThisMonth,
            ModifiedBucket::LastWeek => Self::LastWeek,
            ModifiedBucket::ThisWeek => Self::ThisWeek,
            ModifiedBucket::Yesterday => Self::Yesterday,
            ModifiedBucket::Today => Self::Today,
        }
    }
}

#[derive(Clone, Copy)]
struct ModifiedDateGroupContext {
    today: NaiveDate,
    week_start: NaiveDate,
    month_start: NaiveDate,
    prev_month_start: NaiveDate,
    year_start: NaiveDate,
    last_year_start: NaiveDate,
}

impl ModifiedDateGroupContext {
    fn new() -> Self {
        let today = Local::now().date_naive();
        let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
        let prev_month_start = if today.month() == 1 {
            NaiveDate::from_ymd_opt(today.year() - 1, 12, 1).unwrap_or(today)
        } else {
            NaiveDate::from_ymd_opt(today.year(), today.month() - 1, 1).unwrap_or(today)
        };
        let year_start = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today);
        let last_year_start = NaiveDate::from_ymd_opt(today.year() - 1, 1, 1).unwrap_or(today);
        Self {
            today,
            week_start,
            month_start,
            prev_month_start,
            year_start,
            last_year_start,
        }
    }

    fn bucket(self, modified: Option<std::time::SystemTime>) -> ModifiedBucket {
        let Some(modified) = modified else {
            return ModifiedBucket::Unknown;
        };
        let date = DateTime::<Local>::from(modified).date_naive();
        if date == self.today {
            ModifiedBucket::Today
        } else if date == self.today - Duration::days(1) {
            ModifiedBucket::Yesterday
        } else if date >= self.week_start {
            ModifiedBucket::ThisWeek
        } else if date >= self.week_start - Duration::days(7) {
            ModifiedBucket::LastWeek
        } else if date >= self.month_start {
            ModifiedBucket::ThisMonth
        } else if date >= self.prev_month_start {
            ModifiedBucket::LastMonth
        } else if date >= self.year_start {
            ModifiedBucket::ThisYear
        } else if date >= self.last_year_start {
            ModifiedBucket::LastYear
        } else {
            ModifiedBucket::VeryOld
        }
    }

    fn label(self, bucket: ModifiedBucket, translator: &Translator) -> String {
        let ymd =
            |date: NaiveDate| format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day());
        let (message, dates) = match bucket {
            ModifiedBucket::Today => (Message::GroupDateToday, Some(ymd(self.today))),
            ModifiedBucket::Yesterday => (
                Message::GroupDateYesterday,
                Some(ymd(self.today - Duration::days(1))),
            ),
            ModifiedBucket::ThisWeek => (
                Message::GroupDateThisWeek,
                Some(format!("{}～{}", ymd(self.week_start), ymd(self.today))),
            ),
            ModifiedBucket::LastWeek => (
                Message::GroupDateLastWeek,
                Some(format!(
                    "{}～{}",
                    ymd(self.week_start - Duration::days(7)),
                    ymd(self.week_start - Duration::days(1))
                )),
            ),
            ModifiedBucket::ThisMonth => (
                Message::GroupDateThisMonth,
                Some(format!("{}～{}", ymd(self.month_start), ymd(self.today))),
            ),
            ModifiedBucket::LastMonth => (
                Message::GroupDateLastMonth,
                Some(format!(
                    "{}～{}",
                    ymd(self.prev_month_start),
                    ymd(self.month_start - Duration::days(1))
                )),
            ),
            ModifiedBucket::ThisYear => (
                Message::GroupDateThisYear,
                Some(format!(
                    "{}～{}",
                    ymd(self.year_start),
                    ymd(self.prev_month_start - Duration::days(1))
                )),
            ),
            ModifiedBucket::LastYear => (
                Message::GroupDateLastYear,
                Some(format!(
                    "{}～{}",
                    ymd(self.last_year_start),
                    ymd(self.year_start - Duration::days(1))
                )),
            ),
            ModifiedBucket::VeryOld => (
                Message::GroupDateEarlier,
                Some(format!(
                    "～{}",
                    ymd(self.last_year_start - Duration::days(1))
                )),
            ),
            ModifiedBucket::Unknown => (Message::GroupDateUnknown, None),
        };
        let label = translator.text(message);
        match dates {
            Some(dates) => format!("{label}({dates})"),
            None => label.into_owned(),
        }
    }
}

/// 種類 grouping の順序 key と表示名。folder を先頭にし、ファイルは拡張子ごとにまとめる。
fn kind_group(entry: &DemoEntry) -> (String, String) {
    match entry.kind {
        EntryKind::Folder => ("0-folder".into(), "フォルダー".into()),
        EntryKind::File => match std::path::Path::new(&entry.name)
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty())
        {
            Some(extension) => {
                let extension = extension.to_ascii_uppercase();
                (format!("1-{extension}"), format!("{extension} ファイル"))
            }
            None => ("2-no-extension".into(), "拡張子なし".into()),
        },
    }
}

/// Composition-root proof of the v6 large-directory path.
///
/// Filesystem enumeration and preview decoding live in the directory adapter.
/// Selection transitions are GPUI-independent; this view translates UI intents.
struct DirectoryListDemo {
    keymap_runtime: std::sync::Arc<input::KeymapRuntime>,
    show_hidden_items_subscription: Option<gpui::Subscription>,
    item_check_boxes_subscription: Option<gpui::Subscription>,
    item_check_boxes: bool,
    item_check_boxes_revision: u64,
    external_drop_shell: Option<gpui::WeakEntity<shell::FilerShell>>,
    external_drop_view_id: Option<usize>,
    external_file_drop_paths: Rc<RefCell<Option<Vec<std::path::PathBuf>>>>,
    heights: RowHeights,
    scroll_handle: ScrollHandle,
    selection: Selection,
    /// ページ移動で見出しや短い行を跨いでも、元の列を引き継ぐ。
    page_focus_column: Option<(FocusTarget, usize)>,
    selection_aggregate_revision: u64,
    selection_aggregate_work: Option<SelectionAggregateWork>,
    selection_aggregate: Option<SelectionAggregateProjection>,
    /// pointer-down の snapshot を、drag開始時の再描画後も同じpayloadへ渡す。
    internal_drag_snapshot: Rc<RefCell<Option<InternalDragSnapshot>>>,
    next_internal_drag_session_id: u64,
    /// Shell が一つだけ持つ tab-folder drag session の、各 view 共通の read-only projection。
    tab_directory_drag_source: Rc<RefCell<Option<TabDirectoryDragSource>>>,
    /// 外部 file drop の開始時点を、OS drag が終わるまで固定する。
    external_drag_session: Rc<RefCell<Option<ExternalDragSession>>>,
    next_external_drop_ticket: u64,
    navigation_identity_projection: NavigationIdentityProjection,
    /// 右クリック面の paint bounds。各 render で捨てるだけの view-local projection。
    item_menu_geometry: Rc<RefCell<Option<ItemMenuGeometry>>>,
    layout: ViewLayout,
    columns: usize,
    layout_width: f32,
    /// 等幅 pane allocation の初回配置用投影。実viewport幅はprepaint後に別途反映する。
    pane_width_projection: f32,
    measured_viewport_width: Option<f32>,
    viewport_reconcile_scheduled: Rc<Cell<bool>>,
    measured: Vec<bool>,
    focus_handle: gpui::FocusHandle,
    last_click_was_blank: bool,
    /// 読取ライフサイクルの唯一の owner。location / status / last-good を持つ。
    session: directory_session::DirectorySession,
    /// 表示用に展開した entries。session の last-good を sort した projection。
    entries: Vec<DemoEntry>,
    /// section heading と item 行を持つ disposable な virtual-list projection。
    projected_rows: Vec<ProjectedRow>,
    /// virtual row index を持つ section heading の小さな索引。sticky header の探索を
    /// directory 件数に比例させない。
    heading_rows: Vec<usize>,
    group_by: GroupBy,
    column_prefs: folder_view_preferences::ColumnPrefs,
    /// `entries` の raw fact から一度だけ作る Rows detail 列の presentation cache。
    row_details: std::collections::HashMap<entry_identity::EntryKey, RowDetails>,
    /// Shell namespace 行の typed target。`Entry` の表示名や path を virtual item の
    /// authority にせず、activation 時だけここから destination を得る。
    entry_targets: std::collections::HashMap<entry_identity::EntryKey, Location>,
    /// special root の catalog icon。entry 自体は filesystem fact の形を保ち、見た目だけを
    /// projection に置くことで行 target の authority にしない。
    entry_icons: std::collections::HashMap<entry_identity::EntryKey, IconId>,
    /// This PC rows eligible for the runtime Shell icon adapter.
    pc_shell_icon_targets: HashSet<entry_identity::EntryKey>,
    /// Invalidates requests when staged PC sources are replaced, including same-path rows.
    shell_icon_generation: u64,
    /// Re-captures row identity after navigation or explicit source invalidation.
    shell_icon_identity_generation: u64,
    /// Per-view pending projection; several views may independently subscribe to one cache key.
    shell_icon_pending: Rc<RefCell<HashSet<shell_icons::ViewRequestKey>>>,
    shortcut_icon_pending:
        Rc<RefCell<HashMap<entry_identity::EntryKey, (file_info_shortcut::Target, u64, u64)>>>,
    shortcut_icon_request_ticket: Rc<Cell<u64>>,
    shortcut_icon_projections: HashMap<entry_identity::EntryKey, ShortcutIconProjection>,
    shortcut_icon_projection_order: std::collections::VecDeque<entry_identity::EntryKey>,
    /// この view が今持っている表示用画像。path ごとの状態で持つ。
    /// 共有 cache ではなく projection なので、navigation で捨ててもよい。
    previews: std::collections::HashMap<std::path::PathBuf, PreviewState>,
    /// パンくずの表示名を無効化した世代。F5 と overflow で進める。
    names_epoch: u64,
    list_hovered: bool,
    scrollbar_drag: Option<f32>,
    /// pointer-down で固定した矩形選択の条件。click だけなら selection は変えない。
    rectangle_pointer_down: Option<RectanglePointerDown>,
    /// 表示と行 hit-test に必要な、drag 中だけの view-local geometry。
    rectangle_drag: Option<RectangleDragState>,
    row_column_resize: Option<RowColumnResize>,
    row_column_reorder_down: Option<RowColumnReorderDown>,
    row_column_reorder: Option<RowColumnReorder>,
    row_column_reorder_suppress_click: bool,
    breadcrumb_scroll: ScrollHandle,
    reveal_breadcrumb: bool,
    /// 縮小したときだけ現在地（末尾 crumb）を可視範囲へ戻すための前回幅。
    breadcrumb_viewport_width: Option<f32>,
    breadcrumb_names_ready: bool,
    /// パンくずバーにポインタが乗っているか。横スクロールバーのつまみを出す条件。
    breadcrumb_hovered: bool,
    /// 開いている FolderTree menu の起点。hover が外れても action button を残す。
    folder_tree_button_active: Option<std::path::PathBuf>,
    /// PC の行を entries へ写し終えたか。どれか1つの解決が届いたら true。
    pc_loaded: bool,
    /// PC の先頭から drive までの位置が確定する identity source。
    pc_base_sources_complete: bool,
    /// WSL 列挙は遅いため、実際に必要な bookmark key がある場合だけ待つ。
    pc_wsl_loaded: bool,
    pc_ticket: Option<directory_session::VirtualTicket>,
    /// 直前に entries へ写した PC の行。3つが段階で届くので、変わったときだけ写す。
    pc_rows: Vec<special::PcEntry>,
    /// Libraries の Shell enumeration が一度に返す行。PC の asset 群とは異なり、ticket
    /// に紐付く一回の background effect で更新する。
    library_loaded: bool,
    library_error: Option<String>,
    location_generation: u64,
    hovered_item_part: Option<(usize, &'static str)>,
    last_preview_row: usize,
    frame_profile: diagnostics::FrameProfile,
    /// タイル・アイコンの表示倍率（0.75〜2.5、0.25刻み）。
    icon_scale: f32,
    /// focus 位置のスコープ。キー割り当ての条件評価に使う。
    interaction: InteractionRouter,
    /// 一覧の表示文字列を整形する唯一の owner。タブごとに複製しない。
    translator: Translator,
    /// 一覧の並べ替え状態。列ヘッダのクリックで変わる。
    sort: SortState,
    /// 項目の対応・順序を交換するたびに進む。
    ///
    /// index を捕捉した古い click / deferred reveal を拒否するために使う。
    /// 同じ location の sort / refresh をまたいだ mouse-up を別 item の操作へ
    /// 読み替えないための番号。
    ///
    /// navigation は別の `location_generation` で弾くので、ここでは進めない。
    /// 同じ location で `entries` を差し替える経路（B1 の refresh）は必ず進めること。
    presentation_revision: u64,
    /// フォルダ変更の監視。`notify` を知る唯一の場所。
    watch: directory_watch::DirectoryWatch<Self>,
    /// 実行待ちの effect。`view.update` の中から effect が生まれても
    /// 二重更新にならないよう、一度ここへ積む。
    pending_effects: std::collections::VecDeque<directory_session::SessionEffect>,
    #[cfg(test)]
    test_application_image_path: Option<std::path::PathBuf>,
    #[cfg(test)]
    archive_completion_test_gate: Option<ArchiveCompletionTestGate>,
    /// drain 中か。再入を防ぐ。
    draining_effects: bool,
    /// 戻す予定のスクロール位置。読取が成功した frame で一度だけ使う。
    pending_restore: Option<PendingRestore>,
    /// ユーザーが一覧をスクロールするたびに進む。
    ///
    /// 古い bookmark を新しい操作の後に適用しないための番号。
    scroll_revision: u64,
    /// Native touch pan が wheel event を通らない環境向けに、layout確定offsetを保持する。
    observed_scroll_offset: gpui::Point<Pixels>,
    /// Native 先行 tap と GPUI が取得した後続 tap を同じ item click 列として扱う。
    touch_item_tap_series: Option<TouchItemTapSeries>,
    /// 同じ location の再列挙で、先頭表示 item を同じ画面位置へ戻す。
    pending_projection_anchor: Option<PendingProjectionAnchor>,
    /// rename 前後で path key が変わっても、近傍の安定 key を operation refresh まで持つ。
    pending_rename_anchor: Option<PendingProjectionAnchor>,
    /// accepted rename の terminal/listing commit まで保つ editor 行の表示 geometry。
    pending_rename_geometry: Option<PendingRenameGeometry>,
    /// rename reconciliation は推定行高で先に scroll を動かさず、可視行を測った
    /// 後の最終 anchor 一回だけを適用する。
    defer_projection_anchor_until_measured: bool,
    /// Disposable edit state. It captures the projection identity so refresh
    /// or navigation cannot retarget a later Enter to a reused path.
    rename_session: Option<RenameSession>,
    quick_search: Option<QuickSearchSession>,
    rename_attempt_ticket: u64,
    batch_rename_session: Option<BatchRenameSession>,
    batch_rename_probe_ticket: u64,
    /// パンくずの余白から一時的に開く location 入力欄。directory session は持たず、
    /// 現在地の表示だけを編集開始時に写す。
    address_edit: Option<AddressEditSession>,
    /// Cut は clipboard の外へ永続化しない view-local projection。filesystem
    /// authority ではなく、paste/copy/navigation の際に捨てられる表示上の印だけ。
    cut_paths: std::collections::HashSet<std::path::PathBuf>,
    #[cfg(test)]
    clipboard_write_result_for_test: Option<(bool, bool, u64, u64)>,
    /// Shell-owned preference の読み取り用 projection。
    select_extension_on_rename: bool,
}

#[cfg(test)]
#[derive(Clone)]
struct ArchiveCompletionTestGate {
    result_ready: async_channel::Sender<()>,
    release: async_channel::Receiver<()>,
    disposed: async_channel::Sender<()>,
}

/// Click count が Native / GPUI の contact owner をまたぐ間だけ使う view-local state。
struct TouchItemTapSeries {
    key: entry_identity::EntryKey,
    location_generation: u64,
    presentation_revision: u64,
    position: gpui::Point<Pixels>,
    scroll_offset: gpui::Point<Pixels>,
    count: usize,
    completed_at: std::time::Instant,
}

impl TouchItemTapSeries {
    fn matches(
        &self,
        key: &entry_identity::EntryKey,
        location_generation: u64,
        presentation_revision: u64,
        position: gpui::Point<Pixels>,
        scroll_offset: gpui::Point<Pixels>,
        now: std::time::Instant,
    ) -> bool {
        let dx = position.x.as_f32() - self.position.x.as_f32();
        let dy = position.y.as_f32() - self.position.y.as_f32();
        self.key == *key
            && self.location_generation == location_generation
            && self.presentation_revision == presentation_revision
            && self.scroll_offset == scroll_offset
            && now.duration_since(self.completed_at) <= std::time::Duration::from_millis(400)
            && dx * dx + dy * dy <= 16.0 * 16.0
    }
}

#[derive(Clone, Default)]
struct NavigationIdentityProjection {
    request_id: u64,
    pending: bool,
    location_generation: u64,
    presentation_revision: u64,
    paths: Vec<std::path::PathBuf>,
    identities:
        std::collections::HashMap<std::path::PathBuf, filesystem_identity::FilesystemIdentity>,
}

struct RenameSession {
    key: entry_identity::EntryKey,
    source: std::path::PathBuf,
    source_identity: Option<filesystem_identity::FilesystemIdentity>,
    link_kind: Option<directory::LinkKind>,
    parent_identity: Option<filesystem_identity::FilesystemIdentity>,
    accepted_scan: Option<directory_session::ScanTicket>,
    location_generation: u64,
    presentation_revision: u64,
    input: gpui::Entity<rename_editor::RenameInput>,
    _input_events: Subscription,
}

struct BatchRenameSession {
    keys: Vec<entry_identity::EntryKey>,
    kinds: Vec<EntryKind>,
    sources: Vec<operations::batch_rename::RenameSource>,
    parent_identity: filesystem_identity::FilesystemIdentity,
    accepted_scan: directory_session::ScanTicket,
    location_generation: u64,
    presentation_revision: u64,
    pattern: gpui::Entity<rename_editor::RenameInput>,
    find: gpui::Entity<rename_editor::RenameInput>,
    replace: gpui::Entity<rename_editor::RenameInput>,
    body_scroll: ScrollHandle,
    _events: Vec<Subscription>,
}

fn background_input_blocked_by_batch_rename(session_active: bool) -> bool {
    session_active
}

/// パス文字列を直接入力している間だけの view-local な状態。
///
/// location の authority は引き続き `DirectorySession` にあり、ここへは現在地を
/// 編集開始時に一度だけ複製する。したがって表示名の非同期更新で入力内容を上書きしない。
struct AddressEditSession {
    input: gpui::Entity<rename_editor::RenameInput>,
    _input_events: Subscription,
}

/// Rename の filesystem authority ではない。一覧が reconciliation で更新されるまで
/// editor 行の geometry だけを保つ、view-local な一時 projection。
#[derive(Clone)]
struct PendingRenameGeometry {
    token: Option<operations::model::AttemptToken>,
    key: entry_identity::EntryKey,
    expected_key: entry_identity::EntryKey,
    expected_identity: Option<filesystem_identity::FilesystemIdentity>,
    committed_name: String,
    anchor: Option<PendingProjectionAnchor>,
    fallback_anchor: Option<PendingProjectionAnchor>,
    location_generation: u64,
    presentation_revision: u64,
    terminal_waiting_for_listing: bool,
    accepted_scan_before_terminal: Option<directory_session::ScanTicket>,
    pending_scan_before_terminal: Option<directory_session::ScanTicket>,
    failed_scan_after_terminal: Option<directory_session::ScanTicket>,
    directory: std::path::PathBuf,
}

#[derive(Clone)]
struct InternalDragSnapshot {
    sources: Vec<InternalDragSource>,
    touch_start_position: Option<gpui::Point<Pixels>>,
    location_generation: u64,
    presentation_revision: u64,
    session_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InternalDragSource {
    key: entry_identity::EntryKey,
    path: std::path::PathBuf,
    identity: filesystem_identity::FilesystemIdentity,
    is_directory: bool,
    size: Option<u64>,
    modified: Option<std::time::SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InternalDropTarget {
    key: entry_identity::EntryKey,
    path: std::path::PathBuf,
    identity: filesystem_identity::FilesystemIdentity,
    location_generation: u64,
    presentation_revision: u64,
}

#[derive(Clone)]
pub(crate) struct TabDirectoryDragSource {
    pub(crate) session_id: u64,
    pub(crate) source_tab_id: usize,
    pub(crate) source_view: gpui::Entity<DirectoryListDemo>,
    pub(crate) source_path: std::path::PathBuf,
    pub(crate) source_location_generation: u64,
}

pub(crate) fn tab_directory_move_source_identity(
    source: &TabDirectoryDragSource,
    target_view: &DirectoryListDemo,
    target: &InternalDropTarget,
    cx: &App,
) -> Option<filesystem_identity::FilesystemIdentity> {
    if !target_view.accepts_internal_drag_target(target)
        || source.source_path == target.path
        || source.source_path.starts_with(&target.path)
        || target.path.starts_with(&source.source_path)
    {
        return None;
    }

    let source_target = source.source_view.read_with(cx, |view, _| {
        if view.current_location_generation() != source.source_location_generation
            || view.location().as_ref().and_then(Location::path)
                != Some(source.source_path.as_path())
        {
            return None;
        }
        let snapshot = view.capture_navigation_target_snapshot(
            source.source_view.entity_id(),
            &source.source_path,
            true,
        );
        view.navigation_drop_target(&snapshot)
    })?;
    let ExternalDropTarget::NavigationDirectory {
        path: source_path,
        identity: source_identity,
        ..
    } = source_target
    else {
        return None;
    };
    let source_volume = source_identity.windows_parts()?.0;
    let target_volume = target.identity.windows_parts()?.0;
    (source_volume == target_volume && source_path == source.source_path).then_some(source_identity)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExternalDropTarget {
    Background {
        path: std::path::PathBuf,
        identity: Option<filesystem_identity::FilesystemIdentity>,
        location_generation: u64,
        presentation_revision: u64,
    },
    Folder(InternalDropTarget),
    NavigationDirectory {
        view_entity_id: gpui::EntityId,
        require_current_location: bool,
        path: std::path::PathBuf,
        identity: filesystem_identity::FilesystemIdentity,
        location_generation: u64,
        presentation_revision: u64,
    },
}

fn foreign_file_drop_intent(context: &gpui::FileDropContext) -> input::ForeignFileDropIntent {
    input::foreign_file_drop_intent(input::Modifiers {
        control: context.modifiers.control,
        alt: context.modifiers.alt,
        shift: context.modifiers.shift,
        platform: context.modifiers.platform,
    })
}

fn reject_foreign_link_by_default(context: &gpui::FileDropContext) {
    if context.local_drag_session.is_none()
        && foreign_file_drop_intent(context) == input::ForeignFileDropIntent::Link
    {
        // 早期returnでも未設定のnative effectがCopyへ解決されないよう明示拒否する。
        context.response.set_effect(gpui::FileDropEffect::None);
    }
}

#[cfg(test)]
mod foreign_link_drop_rejection_tests {
    #[test]
    fn foreign_link_rejection_is_explicit_before_target_admission() {
        let context = gpui::FileDropContext {
            modifiers: gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            ..Default::default()
        };

        super::reject_foreign_link_by_default(&context);

        assert_eq!(context.response.effect(), Some(gpui::FileDropEffect::None));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NavigationTargetSurfaceSnapshot {
    view_entity_id: gpui::EntityId,
    path: std::path::PathBuf,
    location_generation: u64,
    presentation_revision: u64,
    expected_identity: Option<filesystem_identity::FilesystemIdentity>,
    require_current_location: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalDragSession {
    paths: Vec<std::path::PathBuf>,
    location_generation: u64,
    presentation_revision: u64,
    preparation: Option<ExternalDropPreparation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalDropPreparation {
    ticket: u64,
    target: ExternalDropTarget,
    result: Option<PreparedExternalDrop>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreparedExternalDrop {
    source_identities: Vec<filesystem_identity::FilesystemIdentity>,
    destination_identity: filesystem_identity::FilesystemIdentity,
    same_volume: bool,
}

impl ExternalDropTarget {
    fn location_generation(&self) -> u64 {
        match self {
            Self::Background {
                location_generation,
                ..
            } => *location_generation,
            Self::Folder(target) => target.location_generation,
            Self::NavigationDirectory {
                location_generation,
                ..
            } => *location_generation,
        }
    }

    fn presentation_revision(&self) -> u64 {
        match self {
            Self::Background {
                presentation_revision,
                ..
            } => *presentation_revision,
            Self::Folder(target) => target.presentation_revision,
            Self::NavigationDirectory {
                presentation_revision,
                ..
            } => *presentation_revision,
        }
    }

    fn path_and_identity(
        &self,
    ) -> Option<(&std::path::Path, &filesystem_identity::FilesystemIdentity)> {
        match self {
            Self::Background {
                path,
                identity: Some(identity),
                ..
            }
            | Self::NavigationDirectory {
                path, identity, ..
            } => Some((path, identity)),
            Self::Folder(target) => Some((&target.path, &target.identity)),
            Self::Background { identity: None, .. } => None,
        }
    }
}

fn prepared_external_drop(
    slot: &RefCell<Option<ExternalDragSession>>,
    paths: &[std::path::PathBuf],
    target: &ExternalDropTarget,
) -> Option<PreparedExternalDrop> {
    let session = slot.borrow();
    prepared_external_drop_from_session(session.as_ref(), paths, target)
}

fn prepared_external_drop_from_session(
    session: Option<&ExternalDragSession>,
    paths: &[std::path::PathBuf],
    target: &ExternalDropTarget,
) -> Option<PreparedExternalDrop> {
    let session = session?;
    if session.paths != paths
        || session.location_generation != target.location_generation()
        || session.presentation_revision != target.presentation_revision()
    {
        return None;
    }
    let preparation = session.preparation.as_ref()?;
    (preparation.target == *target)
        .then(|| preparation.result.clone())
        .flatten()
}

fn capture_external_drag_session(
    slot: &RefCell<Option<ExternalDragSession>>,
    payload: &gpui::ExternalPaths,
    location_generation: u64,
    presentation_revision: u64,
) {
    let mut session = slot.borrow_mut();
    if session
        .as_ref()
        .is_some_and(|session| session.paths == payload.paths())
    {
        return;
    }
    *session = Some(ExternalDragSession {
        paths: payload.paths().to_vec(),
        location_generation,
        presentation_revision,
        preparation: None,
    });
}

fn take_external_drag_session(
    slot: &RefCell<Option<ExternalDragSession>>,
) -> Option<ExternalDragSession> {
    slot.borrow_mut().take()
}

fn external_drag_session_matches(
    session: Option<&ExternalDragSession>,
    payload: &gpui::ExternalPaths,
    target: &ExternalDropTarget,
) -> bool {
    session.is_some_and(|session| {
        session.paths == payload.paths()
            && session.location_generation == target.location_generation()
            && session.presentation_revision == target.presentation_revision()
    })
}

fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes() & 0x400 != 0;
    }
    #[cfg(not(target_os = "windows"))]
    false
}

fn external_background_hover_is_eligible(
    _local_token_present: bool,
    otherwise_eligible: bool,
) -> bool {
    otherwise_eligible
}

#[derive(Clone)]
struct InternalDragPayload {
    source: gpui::EntityId,
    snapshot: Rc<RefCell<Option<InternalDragSnapshot>>>,
}

fn item_surface_element_id(
    part: &'static str,
    index: usize,
    key: Option<&entry_identity::EntryKey>,
    location_generation: u64,
    presentation_revision: u64,
) -> gpui::ElementId {
    let item = match key {
        Some(entry_identity::EntryKey::Path { path, kind }) => {
            let kind = match kind {
                EntryKind::Folder => "folder",
                EntryKind::File => "file",
            };
            gpui::ElementId::NamedChild(
                std::sync::Arc::new(gpui::ElementId::Path(std::sync::Arc::from(path.as_path()))),
                kind.into(),
            )
        }
        Some(entry_identity::EntryKey::Demo(id)) => {
            gpui::ElementId::NamedInteger("demo-entry".into(), *id)
        }
        Some(entry_identity::EntryKey::Virtual(identity)) => {
            gpui::ElementId::Name(identity.clone().into())
        }
        Some(entry_identity::EntryKey::Archive {
            source,
            components,
            kind,
        }) => gpui::ElementId::Name(
            format!("archive:{source:?}!/{}:{kind:?}", components.join("/")).into(),
        ),
        None => gpui::ElementId::NamedInteger("missing-entry".into(), index as u64),
    };
    let generation = gpui::ElementId::NamedChild(
        std::sync::Arc::new(item),
        format!("location-{location_generation}-revision-{presentation_revision}").into(),
    );
    gpui::ElementId::NamedChild(std::sync::Arc::new(generation), part.into())
}

struct InternalDragPreview {
    text: String,
    icons: Vec<IconId>,
}

fn internal_drag_preview_colors() -> (gpui::Hsla, gpui::Rgba) {
    (gpui::hsla(0.0, 0.0, 0.12, 0.94), TEXT)
}

impl Render for InternalDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground) = internal_drag_preview_colors();
        div()
            .max_w(px(320.0))
            .px_3()
            .py_2()
            .rounded_sm()
            .border_1()
            .border_color(gpui::hsla(0.0, 0.0, 0.7, 0.8))
            // preview は独立した暗色 surface を持つので、文字色もここで明示して可読性を保つ。
            .bg(background)
            .text_color(foreground)
            .text_sm()
            .child(drag_preview_contents(&self.text, &self.icons, foreground))
    }
}

fn drag_preview_contents(text: &str, icons: &[IconId], foreground: gpui::Rgba) -> gpui::AnyElement {
    let icon = if icons.len() > 1 {
        let mut stack = div().relative().size(px(28.0)).flex_shrink_0().child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .child(Icon::new(icons[0]).size(px(20.0)).color(foreground.into())),
        );
        if let Some(second) = icons.get(1) {
            stack = stack.child(
                div()
                    .absolute()
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .child(Icon::new(*second).size(px(20.0)).color(foreground.into())),
            );
        }
        stack.into_any_element()
    } else {
        Icon::new(icons.first().copied().unwrap_or(IconId::File))
            .size(px(20.0))
            .color(foreground.into())
            .into_any_element()
    };

    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .max_h(px(NAME_LINE_HEIGHT * 2.0))
                .overflow_hidden()
                .whitespace_normal()
                .line_clamp(2)
                .text_ellipsis()
                .text_size(px(NAME_FONT_SIZE))
                .line_height(px(NAME_LINE_HEIGHT))
                .child(text.to_owned()),
        )
        .into_any_element()
}

fn drag_preview_icon(source: &InternalDragSource) -> IconId {
    if source.is_directory {
        IconId::Folder
    } else {
        IconId::for_extension(
            source
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or(""),
        )
    }
}

fn capture_internal_drag_sources(
    entries: &[DemoEntry],
    source_index: usize,
    selected: &HashSet<usize>,
) -> Option<Vec<InternalDragSource>> {
    let source = entries.get(source_index)?;
    if source.path.is_none() || source.is_link() || source.source_identity.is_none() {
        return None;
    }
    let indices = if selected.contains(&source_index) {
        (0..entries.len())
            .filter(|index| selected.contains(index))
            .collect::<Vec<_>>()
    } else {
        vec![source_index]
    };
    let sources = indices
        .into_iter()
        .map(|index| {
            let entry = entries.get(index)?;
            if entry.is_link() {
                return None;
            }
            Some(InternalDragSource {
                key: entry.key.clone(),
                path: entry.path.clone()?,
                identity: entry.source_identity.as_ref()?.filesystem_identity(),
                is_directory: entry.kind == EntryKind::Folder,
                size: entry.size,
                modified: entry.modified,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!sources.is_empty()).then_some(sources)
}

fn internal_drag_source_for_entry(entry: &DemoEntry) -> Option<InternalDragSource> {
    if entry.is_link() {
        return None;
    }
    Some(InternalDragSource {
        key: entry.key.clone(),
        path: entry.path.clone()?,
        identity: entry.source_identity.as_ref()?.filesystem_identity(),
        is_directory: entry.kind == EntryKind::Folder,
        size: entry.size,
        modified: entry.modified,
    })
}

fn internal_drop_target_for_entry(
    entry: &DemoEntry,
    location_generation: u64,
    presentation_revision: u64,
) -> Option<InternalDropTarget> {
    if entry.kind != EntryKind::Folder || entry.is_link() {
        return None;
    }
    Some(InternalDropTarget {
        key: entry.key.clone(),
        path: entry.path.clone()?,
        identity: entry.source_identity.as_ref()?.filesystem_identity(),
        location_generation,
        presentation_revision,
    })
}

fn internal_drop_target_is_current(
    target: &InternalDropTarget,
    entries: &[DemoEntry],
    location_generation: u64,
    presentation_revision: u64,
) -> bool {
    target.location_generation == location_generation
        && target.presentation_revision == presentation_revision
        && entries.iter().any(|entry| {
            entry.key == target.key
                && entry.path.as_deref() == Some(target.path.as_path())
                && entry
                    .source_identity
                    .as_ref()
                    .is_some_and(|identity| identity.filesystem_identity() == target.identity)
                && entry.kind == EntryKind::Folder
                && !entry.is_link()
        })
}

fn internal_drag_snapshot_is_current(
    snapshot: &InternalDragSnapshot,
    entries: &[DemoEntry],
    location_generation: u64,
    presentation_revision: u64,
) -> bool {
    snapshot.location_generation == location_generation
        && snapshot.presentation_revision == presentation_revision
        && snapshot.sources.iter().all(|source| {
            entries.iter().any(|entry| {
                entry.key == source.key
                    && entry.path.as_deref() == Some(source.path.as_path())
                    && entry
                        .source_identity
                        .as_ref()
                        .is_some_and(|identity| identity.filesystem_identity() == source.identity)
                    && !entry.is_link()
            })
        })
}

fn internal_drop_target_is_valid(
    sources: &[InternalDragSource],
    target_path: Option<&std::path::Path>,
    target_kind: EntryKind,
    target_is_link: bool,
) -> bool {
    let Some(target_path) = target_path else {
        return false;
    };
    target_kind == EntryKind::Folder
        && !target_is_link
        && !sources.iter().any(|source| source.path == target_path)
}

fn take_internal_drag_snapshot(
    slot: &RefCell<Option<InternalDragSnapshot>>,
) -> Option<InternalDragSnapshot> {
    slot.borrow_mut().take()
}

fn peek_internal_drag_snapshot(
    slot: &RefCell<Option<InternalDragSnapshot>>,
) -> Option<InternalDragSnapshot> {
    slot.borrow().clone()
}

fn emit_internal_drag_drop_cancel(
    payload: &InternalDragPayload,
    cx: &mut Context<DirectoryListDemo>,
) {
    let session_id =
        peek_internal_drag_snapshot(&payload.snapshot).map(|snapshot| snapshot.session_id);
    cx.emit(DirectoryEvent::CancelNativeDrag {
        source_view_id: payload.source,
        session_id,
    });
}

fn internal_drag_session_id_is_current(current_session_id: u64, snapshot_session_id: u64) -> bool {
    snapshot_session_id != 0 && current_session_id == snapshot_session_id
}

fn internal_drag_preview_text(snapshot: &InternalDragSnapshot) -> String {
    if snapshot.sources.len() > 1 {
        format!("{} items", snapshot.sources.len())
    } else {
        snapshot
            .sources
            .first()
            .and_then(|source| source.path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn next_rename_attempt_ticket(current: u64) -> Option<u64> {
    current.checked_add(1)
}

fn is_current_rename_attempt(current: u64, candidate: u64) -> bool {
    candidate != 0 && current == candidate
}

fn should_reject_rename_geometry(current: u64, candidate: u64) -> bool {
    is_current_rename_attempt(current, candidate)
}

#[cfg(test)]
mod rename_probe_attempt_tests {
    #[test]
    fn a_later_commit_rejects_an_older_same_view_probe_ticket() {
        let first = super::next_rename_attempt_ticket(0).unwrap();
        let second = super::next_rename_attempt_ticket(first).unwrap();
        assert!(super::is_current_rename_attempt(second, second));
        assert!(!super::is_current_rename_attempt(second, first));
        assert!(!super::should_reject_rename_geometry(second, first));
    }
}

#[cfg(test)]
mod terminal_request_admission_tests {
    use super::*;

    fn id(index: u64) -> filesystem_identity::FilesystemIdentity {
        filesystem_identity::FilesystemIdentity::from_windows_parts(7, index)
    }

    #[test]
    fn captured_item_and_background_produce_one_exact_request_each() {
        let parent = std::path::Path::new(r"C:\Users\ユーザー\Parent Folder");
        let item = parent.join("Child Folder");
        let item_request = captured_terminal_request(
            &item,
            &id(12),
            parent,
            &id(11),
            true,
            true,
            CapturedTerminalLocation::Item,
        )
        .unwrap();
        assert_eq!(item_request.path, item);
        assert_eq!(item_request.identity, id(12));

        let background_request = captured_terminal_request(
            parent,
            &id(11),
            parent,
            &id(11),
            true,
            true,
            CapturedTerminalLocation::Background,
        )
        .unwrap();
        assert_eq!(background_request.path, parent);
        assert_eq!(background_request.identity, id(11));
    }

    #[test]
    fn stale_or_ineligible_terminal_targets_fail_closed() {
        let parent = std::path::Path::new(r"C:\Parent");
        let child = parent.join("Child");
        assert!(
            captured_terminal_request(
                &child,
                &id(12),
                parent,
                &id(11),
                false,
                true,
                CapturedTerminalLocation::Item
            )
            .is_none()
        );
        assert!(
            captured_terminal_request(
                &child,
                &id(12),
                parent,
                &id(11),
                true,
                false,
                CapturedTerminalLocation::Item
            )
            .is_none()
        );
        assert!(
            captured_terminal_request(
                parent,
                &id(99),
                parent,
                &id(11),
                true,
                true,
                CapturedTerminalLocation::Background
            )
            .is_none()
        );
    }
}

#[cfg(test)]
mod zip_menu_eligibility_tests {
    use super::*;

    fn target(key: entry_identity::EntryKey, context: Vec<entry_identity::EntryKey>) -> bool {
        zip_create_context_is_eligible(Some(&key), &context, false, &[], &HashSet::new(), true)
    }

    #[test]
    fn only_one_clicked_ordinary_file_with_both_identities_is_eligible() {
        let key = entry_identity::EntryKey::demo(1);
        assert!(target(key.clone(), vec![key]));
        assert!(zip_create_entry_is_eligible(true, true, true, true, true));
    }

    #[test]
    fn captured_file_request_keeps_source_and_targets_same_folder_stem_zip() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("report.tar.gz");
        std::fs::write(&source, b"source").unwrap();
        let destination = directory.path().join("report.tar.zip");
        let guard = operations::model::test_zip_create_guard(&source, &destination);
        let request = create_zip_request_for_captured_entry(
            source.clone(),
            EntryKind::File,
            guard.source_identity.clone(),
            guard.destination_parent_identity.clone(),
            9,
            12,
            13,
        )
        .unwrap();
        assert_eq!(request.kind, operations::model::OperationKind::CreateZip);
        assert_eq!(request.sources, vec![source]);
        assert_eq!(request.destination.as_deref(), Some(destination.as_path()));
        assert!(matches!(
            request.zip_create_guard.as_ref(),
            Some(operations::model::ZipOperationGuard::Create(_))
        ));
        assert_eq!(
            request.origin,
            Some(operations::model::OperationOrigin {
                view_id: 9,
                location_epoch: 12,
                presentation_revision: 13,
            })
        );
    }

    #[test]
    fn captured_folder_request_uses_the_full_dotted_leaf_for_archive_name() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("project.backup");
        std::fs::create_dir(&source).unwrap();
        let output = directory.path().join("project.backup.zip");
        let guard =
            operations::model::test_zip_create_guard_for_kind(&source, &output, EntryKind::Folder);
        let request = create_zip_request_for_captured_entry(
            source.clone(),
            EntryKind::Folder,
            guard.source_identity.clone(),
            guard.destination_parent_identity.clone(),
            9,
            12,
            13,
        )
        .unwrap();
        assert_eq!(request.destination.as_deref(), Some(output.as_path()));
        assert_eq!(
            match request.zip_create_guard.as_ref().unwrap() {
                operations::model::ZipOperationGuard::Create(guard) =>
                    guard.sources[0].entry_name.as_str(),
                _ => unreachable!(),
            },
            "project.backup"
        );
        assert!(zip_create_entry_is_eligible(true, true, true, true, true));
    }

    #[test]
    fn multi_context_and_non_ordinary_or_unidentified_entries_are_rejected() {
        let clicked = entry_identity::EntryKey::demo(1);
        let other = entry_identity::EntryKey::demo(2);
        assert!(!target(clicked.clone(), vec![clicked.clone(), other]));
        assert!(!zip_create_entry_is_eligible(true, false, true, true, true)); // unsupported kind
        assert!(!zip_create_entry_is_eligible(true, true, false, true, true)); // link/reparse
        assert!(!zip_create_entry_is_eligible(false, true, true, true, true)); // virtual/pathless
        assert!(!zip_create_entry_is_eligible(true, true, true, false, true)); // source identity
        assert!(!zip_create_entry_is_eligible(true, true, true, true, false)); // parent identity
        assert!(!zip_create_context_is_eligible(
            None,
            &[],
            false,
            &[],
            &HashSet::new(),
            true,
        ));
    }

    #[test]
    fn selection_derived_multi_context_requires_exact_captured_order_and_membership() {
        let first = entry_identity::EntryKey::demo(1);
        let anchor = entry_identity::EntryKey::demo(2);
        let third = entry_identity::EntryKey::demo(3);
        let captured = vec![first.clone(), anchor.clone(), third.clone()];
        let selected = captured.iter().cloned().collect::<HashSet<_>>();

        assert!(zip_create_context_is_eligible(
            Some(&anchor),
            &captured,
            true,
            &captured,
            &selected,
            true,
        ));
        assert!(!zip_create_context_is_eligible(
            Some(&anchor),
            &[anchor.clone(), first.clone(), third.clone()],
            true,
            &captured,
            &selected,
            true,
        ));
        assert!(!zip_create_context_is_eligible(
            Some(&anchor),
            &captured,
            true,
            &captured,
            &HashSet::from([first.clone(), anchor.clone()]),
            true,
        ));
        assert!(!zip_selection_order_is_current(
            true,
            &[third.clone(), anchor.clone(), first.clone()],
            &captured,
        ));
        assert!(!zip_create_context_is_eligible(
            Some(&anchor),
            &captured,
            false,
            &[],
            &selected,
            true,
        ));
    }

    #[test]
    fn stale_menu_generation_or_revision_is_rejected() {
        assert!(zip_menu_revision_is_current(7, 11, 7, 11));
        assert!(!zip_menu_revision_is_current(7, 11, 8, 11));
        assert!(!zip_menu_revision_is_current(7, 11, 7, 12));
    }
}

#[cfg(test)]
mod archive_activation_tests {
    use super::*;

    struct ArchiveCompletionHarness {
        view: Option<gpui::Entity<DirectoryListDemo>>,
    }

    impl gpui::Render for ArchiveCompletionHarness {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            match &self.view {
                Some(view) => div().size_full().child(view.clone()),
                None => div().size_full(),
            }
        }
    }

    #[cfg(windows)]
    #[gpui::test]
    fn archive_completion_after_view_drop_disposes_custody_off_ui_update(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("pending.zip");
        let archive = std::fs::File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(archive);
        archive
            .start_file("root/a.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"held source route").unwrap();
        archive.finish().unwrap();

        let (result_ready, result_ready_rx) = async_channel::bounded(1);
        let (release, release_rx) = async_channel::bounded(1);
        let (disposed, disposed_rx) = async_channel::bounded(1);
        let gate = ArchiveCompletionTestGate {
            result_ready,
            release: release_rx,
            disposed,
        };
        let (root, visual) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| {
                DirectoryListDemo::new_at(
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Some(Location::Path(temp.path().to_path_buf())),
                    cx,
                )
            });
            ArchiveCompletionHarness { view: Some(view) }
        });
        let view = visual.read_entity(&root, |root, _| root.view.clone().unwrap());
        visual.run_until_parked();
        visual.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.archive_completion_test_gate = Some(gate);
                view.navigate_location(
                    Location::Archive {
                        source: archive_path.clone(),
                        components: Vec::new(),
                    },
                    cx,
                );
            })
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            visual.run_until_parked();
            if result_ready_rx.try_recv().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "archive result did not reach its gate"
            );
        }
        let cancelled = visual.read_entity(&view, |view, _| {
            view.session.archive_cancellation_for_test().unwrap()
        });
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&archive_path)
                .is_err(),
            "detached result retains the source route while completion is gated"
        );

        visual.update_entity(&root, |root, _| root.view = None);
        drop(view);
        visual.update(|window, _| window.remove_window());
        drop(root);
        cx.run_until_parked();
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        release.send_blocking(()).unwrap();

        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if disposed_rx.try_recv().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "detached result disposal did not acknowledge"
            );
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&archive_path)
            .expect("background disposal releases the source route after weak update fails");
    }

    fn file(path: &str) -> directory::Entry {
        let path = std::path::PathBuf::from(path);
        directory::Entry {
            is_hidden: false,
            key: entry_identity::EntryKey::path(path.clone(), EntryKind::File),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path: Some(path),
            kind: EntryKind::File,
            link: None,
            link_target: None,
            source_identity: Some(directory::EntrySourceIdentity::test(
                EntryKind::File,
                "activation-file",
            )),
            size: Some(0),
            folder_size: None,
            modified: None,
        }
    }

    #[test]
    fn verified_zip_activates_archive_navigation_and_ordinary_files_open_normally() {
        let zip = file(r"C:\work\items.zip");
        assert_eq!(
            resolve_activation_target(&zip, None, false),
            Some(ActivationTarget::Navigate(Location::Archive {
                source: std::path::PathBuf::from(r"C:\work\items.zip"),
                components: Vec::new(),
            }))
        );

        let ordinary = file(r"C:\work\notes.txt");
        assert_eq!(
            resolve_activation_target(&ordinary, None, false),
            Some(ActivationTarget::OpenFile(std::path::PathBuf::from(
                r"C:\work\notes.txt"
            )))
        );
        let folder_path = std::path::PathBuf::from(r"C:\work\folder");
        let folder = directory::Entry {
            is_hidden: false,
            key: entry_identity::EntryKey::path(folder_path.clone(), EntryKind::Folder),
            name: "folder".into(),
            path: Some(folder_path.clone()),
            kind: EntryKind::Folder,
            link: None,
            link_target: None,
            source_identity: Some(directory::EntrySourceIdentity::test(
                EntryKind::Folder,
                "activation-folder",
            )),
            size: None,
            folder_size: None,
            modified: None,
        };
        assert_eq!(
            resolve_activation_target(&folder, None, false),
            Some(ActivationTarget::Navigate(Location::Path(folder_path)))
        );

        let archive_folder = directory::Entry {
            is_hidden: false,
            key: entry_identity::EntryKey::Archive {
                source: std::path::PathBuf::from(r"C:\work\items.zip"),
                components: vec!["nested".into()],
                kind: EntryKind::Folder,
            },
            name: "nested".into(),
            path: None,
            kind: EntryKind::Folder,
            link: None,
            link_target: None,
            source_identity: None,
            size: None,
            folder_size: None,
            modified: None,
        };
        assert!(matches!(
            resolve_activation_target(&archive_folder, None, false),
            Some(ActivationTarget::Navigate(Location::Archive { .. }))
        ));

        let seven_zip = file(r"C:\work\items.7z");
        assert!(matches!(
            resolve_activation_target(&seven_zip, None, false),
            Some(ActivationTarget::OpenFile(_))
        ));
        assert!(matches!(
            resolve_activation_target(&seven_zip, None, true),
            Some(ActivationTarget::Navigate(Location::Archive { .. }))
        ));

        let mut link = file(r"C:\work\linked.7z");
        link.link = Some(directory::LinkKind::LinkOrJunction);
        assert!(matches!(
            resolve_activation_target(&link, None, true),
            Some(ActivationTarget::OpenFile(_))
        ));
    }
}

enum DirectoryEvent {
    ReloadKeymap,
    PrimaryInteraction,
    LocationChanged {
        outgoing: Option<(std::path::PathBuf, folder_view_preferences::FolderViewPrefs)>,
        destination: Option<std::path::PathBuf>,
        generation: u64,
    },
    FolderPrefsChanged {
        path: std::path::PathBuf,
        prefs: folder_view_preferences::FolderViewPrefs,
        field: folder_view_preferences::ChangedField,
    },
    CancelNativeDrag {
        source_view_id: gpui::EntityId,
        session_id: Option<u64>,
    },
    /// 別のタブで開く。path を持たない場所（PC）もあるので location で運ぶ。
    OpenTab(Location),
    /// フォルダ項目を開く。locked 元タブの扱いは Shell が決める。
    OpenFolder(Location),
    /// 保存対象の表示状態が変わった（layout / icon scale / sort）。
    StateChanged,
    SelectionProjectionChanged(InfoProjection),
    ShortcutIconRequest {
        target: file_info_shortcut::Target,
        request_ticket: u64,
        source_generation: u64,
    },
    QuickPreview {
        target: Option<quick_preview::Target>,
        open: bool,
    },
    /// ポインタの位置に menu を出す。`item` は押した項目（余白なら `None`）。
    ///
    /// menu の owner はシェルなので、view は出す場所と対象だけを渡す。
    ShowContextMenu {
        request: menu::ItemMenuRequest,
        at: gpui::Point<gpui::Pixels>,
    },
    /// Shell owns clipboard writes; the view sends only captured plain text.
    CopyPlainText {
        text: String,
    },
    /// 現在の path scope は view が解決し、native Properties の起動は Shell が担う。
    OpenProperties(Vec<std::path::PathBuf>),
    /// v5 と同じ folder-tree cascade の入口。view は anchor と immutable path だけを渡す。
    OpenFolderTree {
        path: std::path::PathBuf,
        anchor: Bounds<Pixels>,
        kind: FolderTreeAnchorKind,
    },
    /// Clipboard 自体と job owner は Shell に置く。view は捕捉済みの path だけを渡す。
    ClipboardWrite {
        paths: Vec<std::path::PathBuf>,
        effect: operations::file_clipboard::ClipboardEffect,
        location_generation: u64,
        presentation_revision: u64,
    },
    /// Shell namespace Copy carries detached Shell parsing identities, never filesystem paths.
    NamespaceClipboardWrite {
        parent: Location,
        identities: Vec<String>,
        location_generation: u64,
        presentation_revision: u64,
    },
    /// paste target と、view が捕捉した freshness identity。
    PasteInto {
        destination: std::path::PathBuf,
        destination_identity: filesystem_identity::FilesystemIdentity,
        location_epoch: u64,
        presentation_revision: u64,
    },
    PasteHere {
        target: InternalDropTarget,
    },
    CreateSymbolicLinkHere {
        target: InternalDropTarget,
    },
    MoveTabDirectoryInto {
        session_id: u64,
        source_tab_id: usize,
        source_identity: filesystem_identity::FilesystemIdentity,
        target: InternalDropTarget,
    },
    EndTabDirectoryDrag {
        session_id: u64,
    },
    /// Clipboard source を辿らず、destination に symbolic-link object を作る。
    PasteLinkInto {
        destination: std::path::PathBuf,
        location_epoch: u64,
        presentation_revision: u64,
    },
    /// Clipboard folder を destination に junction object として作る。
    PasteJunctionInto {
        destination: std::path::PathBuf,
        location_epoch: u64,
        presentation_revision: u64,
    },
    CreateFolderInto {
        destination: std::path::PathBuf,
        destination_identity: filesystem_identity::FilesystemIdentity,
        location_epoch: u64,
        presentation_revision: u64,
    },
    CreateFileInto {
        destination: std::path::PathBuf,
        destination_identity: filesystem_identity::FilesystemIdentity,
        location_epoch: u64,
        presentation_revision: u64,
    },
    /// Delete key は immutable path を shell の operation owner へ渡すだけにする。
    Trash {
        paths: Vec<std::path::PathBuf>,
        source_identities: Vec<Option<directory::EntrySourceIdentity>>,
        reparse_candidates: Vec<bool>,
        location_epoch: u64,
        presentation_revision: u64,
    },
    /// Inline editor has committed one name. The view never mutates its row;
    /// the shell captures a detached request and reconciliation redraws it.
    Rename {
        source: std::path::PathBuf,
        key: entry_identity::EntryKey,
        new_name: String,
        identity_guard: Option<operations::model::MoveIdentityGuard>,
        attempt_ticket: u64,
        link_kind: Option<directory::LinkKind>,
        source_identity: Option<filesystem_identity::FilesystemIdentity>,
        accepted_scan: Option<directory_session::ScanTicket>,
        parent_identity: Option<filesystem_identity::FilesystemIdentity>,
        location_epoch: u64,
        presentation_revision: u64,
    },
    BatchRename {
        plan: Vec<operations::batch_rename::RenameTarget>,
        location_epoch: u64,
        presentation_revision: u64,
    },
    UndoFileOperation,
}

enum ClipboardCopySources {
    Paths(Vec<std::path::PathBuf>),
    Namespace(namespace_clipboard::NamespaceCopy),
}

#[derive(Clone, Debug)]
pub(crate) struct InfoProjection {
    pub(crate) selected_count: usize,
    pub(crate) copy_token: Option<InfoCopyToken>,
    pub(crate) item: Option<InfoItemProjection>,
    /// `None` while a multi-selection aggregation is pending.
    pub(crate) aggregate: Option<SelectionAggregateProjection>,
    pub(crate) hash_target: Option<file_info_hash::Target>,
    pub(crate) exif_target: Option<file_info_exif::Target>,
    pub(crate) shortcut_target: Option<file_info_shortcut::Target>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShortcutTargetVisual {
    Target {
        path: std::path::PathBuf,
        identity: directory::EntrySourceIdentity,
    },
    Broken {
        path: std::path::PathBuf,
    },
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShortcutIconProjection {
    target: file_info_shortcut::Target,
    source_generation: u64,
    visual: ShortcutTargetVisual,
}

#[derive(Clone, Debug)]
pub(crate) struct SelectionAggregateProjection {
    pub(crate) file_count: usize,
    pub(crate) folder_count: usize,
    /// Present only when every ordinary file had a known size and the sum fit in `u64`.
    pub(crate) total_size: Option<u64>,
    pub(crate) unknown_size_count: usize,
}

#[derive(Default)]
struct SelectionAggregateWork {
    cursor: Option<usize>,
    file_count: usize,
    folder_count: usize,
    total_size: Option<u64>,
    unknown_size_count: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct InfoItemProjection {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<std::time::SystemTime>,
    pub(crate) link_target: Option<std::path::PathBuf>,
    pub(crate) copy_token: Option<InfoCopyToken>,
    pub(crate) hash_target: Option<file_info_hash::Target>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InfoCopyToken {
    pub(crate) key: Option<entry_identity::EntryKey>,
    pub(crate) selection_revision: u64,
    pub(crate) location_generation: u64,
    pub(crate) presentation_revision: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum InfoCopyField {
    Name,
    Location,
}
impl gpui::EventEmitter<DirectoryEvent> for DirectoryListDemo {}

/// 保存する1タブ分の表示状態。
///
/// location は session が、それ以外は view が持つ。items や選択は保存しない。
#[derive(Clone, Debug)]
pub struct SavedViewState {
    /// 今いる場所。`None` は location を持たない一覧（`--demo` のサンプル）。
    pub location: Option<Location>,
    pub layout: session_store::LayoutValue,
    pub icon_scale: f32,
    pub sort: session_store::SortValue,
    pub group: session_store::GroupValue,
}

#[derive(Clone)]
struct ScrollbarDrag;

/// 空白から始まった left pointer-down の不変な情報。
///
/// modifier を move/up で読み直すと、Ctrl を途中で離しただけで矩形の意味が変わる。
struct RectanglePointerDown {
    x: f32,
    y: f32,
    content_y: f32,
    mode: RectangleMode,
    location_generation: u64,
    presentation_revision: u64,
    layout: ViewLayout,
    columns: usize,
    layout_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemMenuSurfacePart {
    RowName,
    Visual,
    Name,
    Checkbox,
}

struct ItemMenuSurfaceBounds {
    key: entry_identity::EntryKey,
    part: ItemMenuSurfacePart,
    bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
}

/// 描画された item right-click 面だけを持つ一 frame の disposable geometry。
struct ItemMenuGeometry {
    generation: u64,
    presentation_revision: u64,
    layout: ViewLayout,
    columns: usize,
    layout_width: f32,
    scroll_revision: u64,
    scroll_offset: gpui::Point<Pixels>,
    viewport: Bounds<Pixels>,
    surfaces: Vec<ItemMenuSurfaceBounds>,
    occluding_surfaces: Vec<Rc<RefCell<Option<Bounds<Pixels>>>>>,
}

impl ItemMenuGeometry {
    fn matches_projection(
        &self,
        generation: u64,
        presentation_revision: u64,
        layout: ViewLayout,
        columns: usize,
        layout_width: f32,
        viewport: Bounds<Pixels>,
    ) -> bool {
        self.generation == generation
            && self.presentation_revision == presentation_revision
            && self.layout == layout
            && self.columns == columns
            && self.layout_width.to_bits() == layout_width.to_bits()
            && self.viewport.origin == viewport.origin
            && self.viewport.size == viewport.size
    }

    fn matches_scroll(&self, scroll_revision: u64, scroll_offset: gpui::Point<Pixels>) -> bool {
        self.scroll_revision == scroll_revision && self.scroll_offset == scroll_offset
    }

    fn matches_snapshot(
        &self,
        generation: u64,
        presentation_revision: u64,
        layout: ViewLayout,
        columns: usize,
        layout_width: f32,
        viewport: Bounds<Pixels>,
        scroll_revision: u64,
        scroll_offset: gpui::Point<Pixels>,
    ) -> bool {
        self.matches_projection(
            generation,
            presentation_revision,
            layout,
            columns,
            layout_width,
            viewport,
        ) && self.matches_scroll(scroll_revision, scroll_offset)
    }

    fn matches_view(&self, view: &DirectoryListDemo) -> bool {
        let viewport = view.scroll_handle.bounds();
        self.matches_snapshot(
            view.location_generation,
            view.presentation_revision,
            view.layout,
            view.columns,
            view.layout_width,
            viewport,
            view.scroll_revision,
            view.scroll_handle.offset(),
        )
    }

    fn excludes_touch_drag_at(
        &self,
        position: gpui::Point<Pixels>,
        generation: u64,
        presentation_revision: u64,
        layout: ViewLayout,
        columns: usize,
        layout_width: f32,
        viewport: Bounds<Pixels>,
    ) -> bool {
        if !self.matches_projection(
            generation,
            presentation_revision,
            layout,
            columns,
            layout_width,
            viewport,
        ) {
            return true;
        }
        self.occluding_surfaces.iter().any(|bounds| {
            bounds
                .borrow()
                .as_ref()
                .is_some_and(|bounds| bounds.contains(&position))
        })
    }
}

/// GPUI の drag threshold を越えた後だけ存在する marquee の座標。
struct RectangleDragState {
    start_x: f32,
    /// 開始位置は content 座標で固定し、scroll に追従させない。
    start_y: f32,
    current_x: f32,
    current_y: f32,
    location_generation: u64,
    presentation_revision: u64,
    layout: ViewLayout,
    columns: usize,
    layout_width: f32,
    /// Drag 中だけ保持する、描画済み item target の content 座標 projection。
    surfaces: HashMap<entry_identity::EntryKey, Vec<ContentRect>>,
    surface_overflow: bool,
}

/// View-local basename query. Its input and location generation fence every edit event.
struct QuickSearchSession {
    input: gpui::Entity<rename_editor::RenameInput>,
    input_id: gpui::EntityId,
    location_generation: u64,
    _input_events: Subscription,
}

struct RectangleDrag;

#[derive(Clone, Copy)]
struct RowColumnResize {
    left: Column,
    right: Column,
    start_x: f32,
    start: folder_view_preferences::ColumnPrefs,
    preview: folder_view_preferences::ColumnPrefs,
    location_generation: u64,
    presentation_revision: u64,
    order: [Column; 4],
    visible: [bool; 3],
    projection: [bool; 3],
}

struct RowColumnResizeDrag;

#[derive(Clone, Copy)]
struct RowColumnReorderDown {
    source: Column,
    modifiers: gpui_filer_input::Modifiers,
    location_generation: u64,
    presentation_revision: u64,
    layout: ViewLayout,
    prefs: folder_view_preferences::ColumnPrefs,
    projection: rows_layout::RowColumns,
}

#[derive(Clone, Copy)]
struct RowColumnReorder {
    down: RowColumnReorderDown,
}

#[derive(Clone)]
pub(crate) struct RowColumnReorderDrag {
    pub(crate) source: Column,
}

fn width_of(prefs: folder_view_preferences::ColumnPrefs, column: Column) -> f32 {
    match column {
        Column::Name => 0.0,
        Column::Kind => prefs.kind_width,
        Column::Size => prefs.size_width,
        Column::Modified => prefs.modified_width,
    }
}

fn set_width(prefs: &mut folder_view_preferences::ColumnPrefs, column: Column, width: f32) {
    let width = if width.is_finite() {
        width.clamp(50.0, 500.0)
    } else {
        80.0
    };
    match column {
        Column::Name => {}
        Column::Kind => prefs.kind_width = width,
        Column::Size => prefs.size_width = width,
        Column::Modified => prefs.modified_width = width,
    }
}

#[derive(Clone, Copy, PartialEq)]
struct ContentRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl ContentRect {
    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// Back / Forward のあとに戻すスクロール位置。
///
/// 控えた時点の scroll revision と一緒に持ち、ユーザーが先にスクロールしていたら
/// 適用しない。
struct PendingRestore {
    bookmark: navigation_history::ViewBookmark,
    scroll_revision: u64,
}

/// 行の再列挙は実測高を推定値へ戻すため、数値 offset だけを残しても同じ item を
/// 指さない。更新前の先頭 key と行内位置を一 frame だけ保持する。
#[derive(Clone)]
struct PendingProjectionAnchor {
    key: entry_identity::EntryKey,
    /// anchor row の top が viewport top から何 px の位置にあったか。
    screen_y: f32,
    fallback_offset: f32,
    scroll_revision: u64,
}

impl DirectoryListDemo {
    fn row_columns_for_width(
        &self,
        width: f32,
        prefs: folder_view_preferences::ColumnPrefs,
    ) -> rows_layout::RowColumns {
        rows_layout::project_columns_with_checkbox(width, prefs, self.item_check_boxes)
    }

    fn effective_column_prefs(&self) -> folder_view_preferences::ColumnPrefs {
        self.row_column_resize
            .filter(|drag| {
                drag.location_generation == self.location_generation
                    && drag.presentation_revision == self.presentation_revision
                    && self.layout == ViewLayout::Rows
                    && drag.order == self.column_prefs.order
                    && drag.visible
                        == [
                            self.column_prefs.kind,
                            self.column_prefs.size,
                            self.column_prefs.modified,
                        ]
                    && {
                        let projection =
                            self.row_columns_for_width(self.layout_width, drag.preview);
                        [projection.kind, projection.size, projection.modified] == drag.projection
                    }
            })
            .map_or(self.column_prefs, |drag| drag.preview)
    }

    pub(crate) fn prepare_navigation_target(
        &mut self,
        snapshot: &NavigationTargetSurfaceSnapshot,
        cx: &mut Context<Self>,
    ) {
        if !self.navigation_snapshot_location_matches(snapshot) {
            return;
        }
        let paths = self
            .session
            .location()
            .and_then(Location::path)
            .map(navigation::ancestors)
            .unwrap_or_default();
        self.ensure_navigation_identity_projection(paths, cx);
    }

    fn ensure_navigation_identity_projection(
        &mut self,
        paths: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let current = &self.navigation_identity_projection;
        if current.location_generation == self.location_generation
            && current.presentation_revision == self.presentation_revision
            && current.paths == paths
        {
            return;
        }

        let projection = &mut self.navigation_identity_projection;
        projection.request_id = projection.request_id.wrapping_add(1);
        projection.pending = true;
        projection.location_generation = self.location_generation;
        projection.presentation_revision = self.presentation_revision;
        projection.paths = paths.clone();
        projection.identities.clear();

        let request_id = projection.request_id;
        let generation = self.location_generation;
        let revision = self.presentation_revision;
        let view_id = cx.entity().entity_id();
        let view = cx.entity().downgrade();
        let shell = self.external_drop_shell.clone();
        let expected_paths = paths.clone();
        cx.spawn(async move |_this, cx| {
            let observed = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .iter()
                        .filter_map(|path| {
                            let metadata = std::fs::symlink_metadata(path).ok()?;
                            if !metadata.file_type().is_dir()
                                || metadata_is_reparse_point(&metadata)
                            {
                                return None;
                            }
                            let identity =
                                directory::EntrySourceIdentity::from_path(path, EntryKind::Folder)?
                                    .filesystem_identity();
                            Some((path.clone(), identity))
                        })
                        .collect::<std::collections::HashMap<_, _>>()
                })
                .await;
            let committed = view
                .update(cx, |this, cx| {
                    let projection = &mut this.navigation_identity_projection;
                    if cx.entity().entity_id() != view_id
                        || this.location_generation != generation
                        || this.presentation_revision != revision
                        || projection.request_id != request_id
                        || projection.paths != expected_paths
                    {
                        return false;
                    }
                    projection.pending = false;
                    projection.identities = observed;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if committed && let Some(shell) = shell {
                let _ = shell.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    }

    fn navigation_surface_snapshot(
        &self,
        view_entity_id: gpui::EntityId,
        path: &std::path::Path,
        require_current_location: bool,
    ) -> NavigationTargetSurfaceSnapshot {
        let projection = &self.navigation_identity_projection;
        let current_paths = self
            .session
            .location()
            .and_then(Location::path)
            .map(navigation::ancestors)
            .unwrap_or_default();
        let projection_current = !projection.pending
            && projection.location_generation == self.location_generation
            && projection.presentation_revision == self.presentation_revision
            && projection.paths == current_paths;
        NavigationTargetSurfaceSnapshot {
            view_entity_id,
            path: path.to_path_buf(),
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            expected_identity: projection_current
                .then(|| projection.identities.get(path).cloned())
                .flatten(),
            require_current_location,
        }
    }

    fn navigation_snapshot_location_matches(
        &self,
        snapshot: &NavigationTargetSurfaceSnapshot,
    ) -> bool {
        if self.rename_session.is_some()
            || self.location_generation != snapshot.location_generation
            || self.presentation_revision != snapshot.presentation_revision
        {
            return false;
        }
        let Some(current_path) = self.session.location().and_then(Location::path) else {
            return false;
        };
        let current_paths = navigation::ancestors(current_path);
        if !current_paths.iter().any(|path| path == &snapshot.path) {
            return false;
        }
        if snapshot.require_current_location && current_path != snapshot.path {
            return false;
        }
        true
    }

    fn navigation_snapshot_matches(&self, snapshot: &NavigationTargetSurfaceSnapshot) -> bool {
        if !self.navigation_snapshot_location_matches(snapshot) {
            return false;
        }
        let projection = &self.navigation_identity_projection;
        let current_paths = self
            .session
            .location()
            .and_then(Location::path)
            .map(navigation::ancestors)
            .unwrap_or_default();
        if projection.pending
            || projection.location_generation != snapshot.location_generation
            || projection.presentation_revision != snapshot.presentation_revision
            || projection.paths != current_paths
            || !projection.paths.iter().any(|path| path == &snapshot.path)
        {
            return false;
        }
        snapshot
            .expected_identity
            .as_ref()
            .is_some_and(|expected| projection.identities.get(&snapshot.path) == Some(expected))
    }

    pub(crate) fn navigation_drop_target(
        &self,
        snapshot: &NavigationTargetSurfaceSnapshot,
    ) -> Option<ExternalDropTarget> {
        if self.rename_session.is_some() {
            return None;
        }
        let projection = &self.navigation_identity_projection;
        if projection.pending || !self.navigation_snapshot_matches(snapshot) {
            return None;
        }
        Some(ExternalDropTarget::NavigationDirectory {
            view_entity_id: snapshot.view_entity_id,
            require_current_location: snapshot.require_current_location,
            path: snapshot.path.clone(),
            identity: projection.identities.get(&snapshot.path)?.clone(),
            location_generation: snapshot.location_generation,
            presentation_revision: snapshot.presentation_revision,
        })
    }

    pub(crate) fn capture_navigation_target_snapshot(
        &self,
        view_entity_id: gpui::EntityId,
        path: &std::path::Path,
        require_current_location: bool,
    ) -> NavigationTargetSurfaceSnapshot {
        self.navigation_surface_snapshot(view_entity_id, path, require_current_location)
    }

    pub(crate) fn accepts_navigation_drop_target(&self, target: &ExternalDropTarget) -> bool {
        let ExternalDropTarget::NavigationDirectory {
            view_entity_id,
            require_current_location,
            path,
            identity,
            location_generation,
            presentation_revision,
        } = target
        else {
            return false;
        };
        let snapshot = NavigationTargetSurfaceSnapshot {
            view_entity_id: *view_entity_id,
            path: path.clone(),
            location_generation: *location_generation,
            presentation_revision: *presentation_revision,
            expected_identity: Some(identity.clone()),
            require_current_location: *require_current_location,
        };
        self.navigation_drop_target(&snapshot).as_ref() == Some(target)
    }

    fn set_folder_tree_button_active(
        &mut self,
        path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.folder_tree_button_active != path {
            self.folder_tree_button_active = path;
            cx.notify();
        }
    }

    fn entry_icon(&self, entry: &DemoEntry) -> IconId {
        if entry.is_link() {
            return IconId::Link;
        }
        if is_shortcut_file(entry) {
            return match self.shortcut_projection(entry).map(|projection| &projection.visual) {
                Some(ShortcutTargetVisual::Target { path: _, identity })
                    if identity.kind == EntryKind::Folder => IconId::Folder,
                Some(ShortcutTargetVisual::Target { path, .. })
                | Some(ShortcutTargetVisual::Broken { path }) => path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .filter(|extension| !extension.is_empty())
                    .map(IconId::for_extension)
                    .unwrap_or(IconId::Link),
                Some(ShortcutTargetVisual::Failed) | None => IconId::Link,
            };
        }
        self.entry_icons
            .get(&entry.key)
            .copied()
            .unwrap_or_else(|| entry.icon())
    }

    fn request_shortcut_icon(&self, entry: &DemoEntry, cx: &mut Context<Self>) {
        let Some(target) =
            shortcut_info_target(entry, self.location_generation, self.presentation_revision)
        else {
            return;
        };
        let source_generation = self.shell_icon_generation;
        if self
            .shortcut_icon_projections
            .get(&entry.key)
            .is_some_and(|projection| {
                projection.target == target && projection.source_generation == source_generation
            })
        {
            return;
        }
        if self
            .shortcut_icon_pending
            .borrow()
            .get(&entry.key)
            .is_some_and(|(current, _, generation)| {
                current == &target && *generation == source_generation
            })
        {
            return;
        }

        let ticket = self
            .shortcut_icon_request_ticket
            .get()
            .wrapping_add(1)
            .max(1);
        self.shortcut_icon_request_ticket.set(ticket);
        self.shortcut_icon_pending.borrow_mut().insert(
            entry.key.clone(),
            (target.clone(), ticket, source_generation),
        );
        cx.emit(DirectoryEvent::ShortcutIconRequest {
            target,
            request_ticket: ticket,
            source_generation,
        });
    }

    pub(crate) fn shortcut_icon_target_is_current(
        &self,
        target: &file_info_shortcut::Target,
        source_generation: u64,
    ) -> bool {
        if source_generation != self.shell_icon_generation
            || target.location_generation != self.location_generation
            || target.presentation_revision != self.presentation_revision
        {
            return false;
        }
        self.entries.iter().any(|entry| {
            entry.key == target.key
                && is_shortcut_file(entry)
                && entry.path.as_ref() == Some(&target.path)
                && entry.source_identity.as_ref() == Some(&target.source_identity)
                && entry.size == Some(target.size)
                && entry.modified == Some(target.modified)
        })
    }

    pub(crate) fn resolve_shortcut_icon(
        &mut self,
        target: file_info_shortcut::Target,
        request_ticket: u64,
        source_generation: u64,
        visual: ShortcutTargetVisual,
        cx: &mut Context<Self>,
    ) {
        let matches_pending = self
            .shortcut_icon_pending
            .borrow()
            .get(&target.key)
            .is_some_and(|(pending, ticket, generation)| {
                pending == &target && *ticket == request_ticket && *generation == source_generation
            });
        if !matches_pending {
            return;
        }
        self.shortcut_icon_pending.borrow_mut().remove(&target.key);
        if !self.shortcut_icon_target_is_current(&target, source_generation) {
            return;
        }
        self.shortcut_icon_projections.insert(
            target.key.clone(),
            ShortcutIconProjection {
                target: target.clone(),
                source_generation,
                visual,
            },
        );
        if let Some(index) = self
            .shortcut_icon_projection_order
            .iter()
            .position(|key| key == &target.key)
        {
            self.shortcut_icon_projection_order.remove(index);
        }
        self.shortcut_icon_projection_order
            .push_back(target.key.clone());
        while self.shortcut_icon_projections.len() > 96 {
            let Some(oldest) = self.shortcut_icon_projection_order.pop_front() else {
                break;
            };
            self.shortcut_icon_projections.remove(&oldest);
        }
        cx.notify();
    }

    pub(crate) fn defer_shortcut_icon(
        &mut self,
        target: &file_info_shortcut::Target,
        request_ticket: u64,
        source_generation: u64,
    ) {
        let matches_pending = self
            .shortcut_icon_pending
            .borrow()
            .get(&target.key)
            .is_some_and(|(pending, ticket, generation)| {
                pending == target && *ticket == request_ticket && *generation == source_generation
            });
        if matches_pending {
            self.shortcut_icon_pending.borrow_mut().remove(&target.key);
        }
    }

    fn shortcut_projection(&self, entry: &DemoEntry) -> Option<&ShortcutIconProjection> {
        let target =
            shortcut_info_target(entry, self.location_generation, self.presentation_revision)?;
        self.shortcut_icon_projections
            .get(&entry.key)
            .filter(|projection| {
                projection.target == target
                    && projection.source_generation == self.shell_icon_generation
            })
    }

    fn shortcut_is_broken(&self, entry: &DemoEntry) -> bool {
        self.shortcut_projection(entry)
            .is_some_and(|projection| matches!(&projection.visual, ShortcutTargetVisual::Broken { .. }))
    }

    fn shell_association_icon(
        &self,
        entry: &DemoEntry,
        logical_size: f32,
        scale_factor: f32,
        measuring: bool,
        cx: &mut Context<Self>,
    ) -> Option<std::sync::Arc<gpui::RenderImage>> {
        let this_pc_target = self.is_this_pc() && self.pc_shell_icon_targets.contains(&entry.key);
        let ordinary_path_target =
            matches!(self.session.location(), Some(Location::Path(_))) && entry.link.is_none();
        if measuring || (!this_pc_target && !ordinary_path_target) {
            return None;
        }
        let (Some(path), Some(identity)) = (entry.path.clone(), entry.source_identity.clone())
        else {
            return None;
        };
        let source_path = path.clone();
        let source_identity = identity.clone();
        let pixels = (logical_size * scale_factor).ceil().clamp(16.0, 256.0) as u32;
        let is_shortcut = is_shortcut_file(entry);
        if is_shortcut {
            self.request_shortcut_icon(entry, cx);
        }
        if !uses_shell_icon_fallback(self.entry_icon(entry)) {
            return None;
        }
        let shortcut_target = if is_shortcut {
            match self.shortcut_projection(entry).map(|projection| &projection.visual) {
                Some(ShortcutTargetVisual::Target { path, identity })
                    if identity.kind == EntryKind::File => Some((path.clone(), identity.clone())),
                _ => return None,
            }
        } else {
            None
        };
        let (path, identity) = shortcut_target
            .clone()
            .unwrap_or((path, identity));
        let invalidation_epoch = thumbnails::epoch_of(&path);
        let location_generation = self.location_generation;
        let presentation_revision = self.presentation_revision;
        let source_generation = self.shell_icon_generation;
        let key = shell_icons::Key::new(path.clone(), identity.clone(), pixels, invalidation_epoch);
        let view_request_key = shell_icons::ViewRequestKey::new(
            key.clone(),
            location_generation,
            presentation_revision,
            source_generation,
        );
        if let Some(image) = shell_icons::cached(&key) {
            return Some(image);
        }
        if shell_icons::failed(&key) {
            return None;
        }
        let pending_projection = self.shell_icon_pending.clone();
        if !pending_projection
            .borrow_mut()
            .insert(view_request_key.clone())
        {
            return None;
        }
        let Some(request_id) = shell_icons::begin(&key) else {
            pending_projection.borrow_mut().remove(&view_request_key);
            return None;
        };
        let view = cx.entity().downgrade();
        let entry_key = entry.key.clone();
        let request_key = key.clone();
        cx.spawn(async move |_this, cx| {
            let result_key = request_key.clone();
            let image = cx
                .background_executor()
                .spawn(async move { shell_icons::load(&result_key).await })
                .await;
            let stale = matches!(&image, shell_icons::LoadResult::Stale);
            let (signature, image) = match image {
                shell_icons::LoadResult::Loaded(loaded) => {
                    (Some(loaded.signature), Some(loaded.image))
                }
                shell_icons::LoadResult::Failed(signature) => (Some(signature), None),
                shell_icons::LoadResult::Stale => (None, None),
            };
            if stale {
                shell_icons::cancel(request_id);
                pending_projection.borrow_mut().remove(&view_request_key);
                return;
            }
            let committed = view
                .update(cx, |this, cx| {
                    let current = this
                        .entries
                        .iter()
                        .find(|candidate| candidate.key == entry_key);
                    let current_is_eligible = current.is_some_and(|candidate| {
                        uses_shell_icon_fallback(this.entry_icon(candidate))
                            && ((this.is_this_pc()
                                && this.pc_shell_icon_targets.contains(&entry_key))
                                || (matches!(this.session.location(), Some(Location::Path(_)))
                                    && candidate.link.is_none()))
                    });
                    let shortcut_target_is_current = shortcut_target.as_ref().is_none_or(
                        |(target_path, target_identity)| {
                            current
                                .and_then(|candidate| this.shortcut_projection(candidate))
                                .is_some_and(|projection| {
                                    matches!(
                                        &projection.visual,
                                        ShortcutTargetVisual::Target { path, identity }
                                            if path == target_path && identity == target_identity
                                    )
                                })
                        },
                    );
                    let target_is_current = current_is_eligible
                        && this.location_generation == location_generation
                        && this.presentation_revision == presentation_revision
                        && this.shell_icon_generation == source_generation
                        && thumbnails::epoch_of(&path) == invalidation_epoch
                        && shortcut_target_is_current
                        && current.is_some_and(|candidate| {
                            candidate.path.as_deref() == Some(source_path.as_path())
                                && candidate.source_identity.as_ref() == Some(&source_identity)
                        });
                    if target_is_current {
                        let finished = signature.is_some_and(|signature| {
                            shell_icons::finish(request_id, &request_key, signature, image.clone())
                        });
                        if finished && image.is_some() {
                            cx.notify();
                        }
                        finished
                    } else {
                        shell_icons::cancel(request_id);
                        false
                    }
                })
                .unwrap_or(false);
            if !committed {
                shell_icons::cancel(request_id);
            }
            pending_projection.borrow_mut().remove(&view_request_key);
        })
        .detach();
        None
    }

    /// 別の場所へ移動する。同じ path は何もしない。
    ///
    /// 読取の要求は session が作る。ここは表示状態を離して effect を実行するだけ。
    fn navigate(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.navigate_location(Location::Path(path), cx);
    }

    /// path と Shell namespace のどちらにも同じ session history を通して移動する。
    fn navigate_location(&mut self, location: Location, cx: &mut Context<Self>) {
        if self.location().as_ref() != Some(&location) {
            self.shell_icon_generation = self.shell_icon_generation.wrapping_add(1);
            self.shell_icon_identity_generation =
                self.shell_icon_identity_generation.wrapping_add(1);
        }
        let outgoing = self.folder_view_preferences();
        // PC から出るときは bookmark を渡さない。PC の並びは session の履歴に意味を持たず、
        // 渡すと「直前にいた path」の保存済みの位置を上書きしてしまう。
        let bookmark = if self.location().is_some_and(|current| current.is_virtual()) {
            None
        } else {
            self.view_bookmark()
        };
        let effects = self.session.navigate(location, bookmark);
        // 通常の移動では位置を戻さない。
        self.pending_restore = None;
        self.begin_location_change(effects, outgoing, cx);
    }

    /// PC へ移る。location と履歴の authority は session にある。
    fn navigate_to_pc(&mut self, cx: &mut Context<Self>) {
        self.navigate_location(Location::ThisPc, cx);
    }

    /// 親へ移動する。PC は仮想 root、drive root の親だけが PC になる。
    fn navigate_up(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(location) = self.location() else {
            return false;
        };
        let Some(destination) = up_destination(&location) else {
            return false;
        };
        self.navigate_location(destination, cx);
        true
    }

    /// 履歴を一つ戻る。
    fn back(&mut self, cx: &mut Context<Self>) {
        let outgoing = self.folder_view_preferences();
        let bookmark = self.view_bookmark();
        let effects = self.session.back(bookmark);
        if effects.is_empty() {
            return;
        }
        // 移動先の entry に保存されていた位置を控える。適用は読取が成功してから。
        self.remember_restore_target();
        self.begin_location_change(effects, outgoing, cx);
    }

    /// 履歴を一つ進む。
    fn forward(&mut self, cx: &mut Context<Self>) {
        let outgoing = self.folder_view_preferences();
        let bookmark = self.view_bookmark();
        let effects = self.session.forward(bookmark);
        if effects.is_empty() {
            return;
        }
        self.remember_restore_target();
        self.begin_location_change(effects, outgoing, cx);
    }

    /// location が変わったときの view 側の後始末。
    ///
    /// `navigate` / `back` / `forward` が同じ処理を通る。session は rows を持たないので、
    /// 表示状態の交換はここでしかできない。
    fn begin_location_change(
        &mut self,
        effects: Vec<directory_session::SessionEffect>,
        outgoing: Option<(std::path::PathBuf, folder_view_preferences::FolderViewPrefs)>,
        cx: &mut Context<Self>,
    ) {
        if effects.is_empty() {
            return;
        }
        self.location_generation += 1;
        // Old input events must not search or restore focus after this view leaves.
        self.quick_search = None;
        self.row_column_resize = None;
        self.cancel_row_column_reorder();
        self.pc_loaded = false;
        self.pc_base_sources_complete = false;
        self.pc_wsl_loaded = false;
        self.pc_ticket = None;
        self.pc_rows.clear();
        self.library_loaded = false;
        self.library_error = None;
        self.reset_view_state(cx);
        for effect in effects {
            self.run_effect(effect, cx);
        }
        let destination = self
            .session
            .location()
            .and_then(Location::path)
            .map(std::path::Path::to_path_buf);
        cx.emit(DirectoryEvent::LocationChanged {
            outgoing,
            destination,
            generation: self.location_generation,
        });
        cx.notify();
    }

    /// location をまたぐときに表示状態を離す。
    ///
    /// path へ移るとき（`begin_location_change`）と PC へ移るとき（`navigate_to_pc`）が
    /// 同じものを使う。選択もスクロール位置も場所をまたいで持ち越さない。
    fn reset_view_state(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        self.projected_rows.clear();
        self.heading_rows.clear();
        self.row_details.clear();
        self.entry_targets.clear();
        self.entry_icons.clear();
        self.selection = Selection::default();
        self.emit_info_projection(cx);
        self.heights.reset(0);
        self.measured.clear();
        self.previews.clear();
        self.columns = 1;
        self.layout_width = 0.0;
        self.scroll_handle.set_offset(gpui::point(px(0.0), px(0.0)));
        self.observed_scroll_offset = self.scroll_handle.offset();
        self.scrollbar_drag = None;
        self.rectangle_pointer_down = None;
        self.rectangle_drag = None;
        self.last_click_was_blank = false;
        self.reveal_breadcrumb = true;
        self.breadcrumb_names_ready = false;
        self.hovered_item_part = None;
        self.last_preview_row = 0;
        self.pending_projection_anchor = None;
        self.pending_rename_anchor = None;
        self.pending_rename_geometry = None;
        self.defer_projection_anchor_until_measured = false;
    }

    /// entry projection を、heading を含む virtual-list の行へ写す。
    ///
    /// tile/icon 表示では 1 行に複数 entry がある。Shell の非同期列挙が完了したときに
    /// entry 数のまま reset すると、以前の grid の visible row が entry slice の外を指す。
    fn reset_projected_rows(&mut self) {
        self.rebuild_projected_rows();
        let rows = self.projected_rows.len();
        self.heights.reset(rows);
        self.heights.set_content(0..rows, ESTIMATED_ROW_HEIGHT);
        self.apply_heading_height_estimates();
        self.measured = vec![false; rows];
        self.last_preview_row = 0;
    }

    /// section heading は entry card と別の固定高。全行へ tile/icon の推定高を入れた後に
    /// これを戻しておかないと、未測定 heading の checkbox と次の section が大きく離れ、
    /// 初回の実測で scroll anchor まで揺れる。
    fn apply_heading_height_estimates(&mut self) {
        for &row in &self.heading_rows {
            self.heights
                .set_content(row..row + 1, gpui_filer_ui::CHECKBOX_HIT_SIZE);
        }
        let category_height =
            self.layout.estimated_height(self.icon_scale) + gpui_filer_ui::CHECKBOX_HIT_SIZE;
        for (row, projected) in self.projected_rows.iter().enumerate() {
            if matches!(projected, ProjectedRow::CategoryStrip { .. }) {
                self.heights.set_content(row..row + 1, category_height);
            }
        }
    }

    fn rebuild_projected_rows(&mut self) {
        self.projected_rows.clear();
        if self.entries.is_empty() {
            self.selection.clear_section_focus();
            return;
        }
        if self.group_by == GroupBy::None {
            self.projected_rows.push(ProjectedRow::Entries {
                start: 0,
                end: self.entries.len(),
            });
        } else if self.group_by == GroupBy::ModifiedDate {
            let context = ModifiedDateGroupContext::new();
            let mut start = 0;
            while start < self.entries.len() {
                let bucket = context.bucket(self.entries[start].modified);
                let mut end = start + 1;
                while end < self.entries.len()
                    && context.bucket(self.entries[end].modified) == bucket
                {
                    end += 1;
                }
                self.projected_rows.push(ProjectedRow::Heading {
                    key: selection::SectionKey::ModifiedDate(bucket.into()),
                    label: context.label(bucket, &self.translator).into(),
                    start,
                    end,
                });
                self.projected_rows
                    .push(ProjectedRow::Entries { start, end });
                start = end;
            }
        } else {
            let mut groups = Vec::new();
            let mut start = 0;
            while start < self.entries.len() {
                let (key, label) = kind_group(&self.entries[start]);
                let mut end = start + 1;
                while end < self.entries.len() && kind_group(&self.entries[end]).0 == key {
                    end += 1;
                }
                groups.push((key, label, start, end));
                start = end;
            }

            // 一件だけの種類 section が並ぶときは、複数の見出しと項目を一行にまとめる。
            // 最後に一件だけ余る場合は通常 section のままにして、横並びの見た目を保つ。
            let columns = self.columns.max(1);
            let mut group_index = 0;
            while group_index < groups.len() {
                if self.layout != ViewLayout::Rows
                    && columns > 1
                    && groups[group_index].3 - groups[group_index].2 == 1
                {
                    let run_start = group_index;
                    while group_index < groups.len()
                        && groups[group_index].3 - groups[group_index].2 == 1
                    {
                        group_index += 1;
                    }
                    let run_len = group_index - run_start;
                    let strip_len = if run_len >= 2 && run_len % columns == 1 {
                        run_len - 1
                    } else if run_len >= 2 {
                        run_len
                    } else {
                        0
                    };
                    for chunk in groups[run_start..run_start + strip_len].chunks(columns) {
                        let strip_groups = chunk
                            .iter()
                            .map(|(key, label, start, _)| CategoryStripGroup {
                                key: selection::SectionKey::Kind(key.clone()),
                                label: label.clone().into(),
                                index: *start,
                            })
                            .collect::<Vec<_>>();
                        let strip_start = chunk.first().expect("non-empty category strip").2;
                        let strip_end = chunk.last().expect("non-empty category strip").3;
                        self.projected_rows.push(ProjectedRow::CategoryStrip {
                            groups: strip_groups,
                            start: strip_start,
                            end: strip_end,
                        });
                    }
                    for (key, label, start, end) in &groups[run_start + strip_len..group_index] {
                        self.projected_rows.push(ProjectedRow::Heading {
                            key: selection::SectionKey::Kind(key.clone()),
                            label: label.clone().into(),
                            start: *start,
                            end: *end,
                        });
                        self.projected_rows.push(ProjectedRow::Entries {
                            start: *start,
                            end: *end,
                        });
                    }
                } else {
                    let (key, label, start, end) = &groups[group_index];
                    self.projected_rows.push(ProjectedRow::Heading {
                        key: selection::SectionKey::Kind(key.clone()),
                        label: label.clone().into(),
                        start: *start,
                        end: *end,
                    });
                    self.projected_rows.push(ProjectedRow::Entries {
                        start: *start,
                        end: *end,
                    });
                    group_index += 1;
                }
            }
        }
        self.expand_projected_entry_rows();
        if let Some(FocusTarget::Section(key)) = self.selection.focus_target().cloned()
            && self.section_members(&key).is_none()
        {
            self.selection.clear_section_focus();
        }
    }

    fn expand_projected_entry_rows(&mut self) {
        let columns = self.columns.max(1);
        let mut expanded = Vec::with_capacity(self.projected_rows.len());
        for row in self.projected_rows.drain(..) {
            match row {
                ProjectedRow::Heading { .. } | ProjectedRow::CategoryStrip { .. } => {
                    expanded.push(row)
                }
                ProjectedRow::Entries { start, end } => {
                    for row_start in (start..end).step_by(columns) {
                        expanded.push(ProjectedRow::Entries {
                            start: row_start,
                            end: (row_start + columns).min(end),
                        });
                    }
                }
            }
        }
        self.projected_rows = expanded;
        self.heading_rows = self
            .projected_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| matches!(item, ProjectedRow::Heading { .. }).then_some(row))
            .collect();
    }

    fn entry_row(&self, index: usize) -> Option<usize> {
        self.projected_rows.iter().position(|row| matches!(row, ProjectedRow::Entries { start, end } | ProjectedRow::CategoryStrip { start, end, .. } if *start <= index && index < *end))
    }

    fn row_entries(&self, row: usize) -> std::ops::Range<usize> {
        match self.projected_rows.get(row) {
            Some(ProjectedRow::Entries { start, end }) => *start..*end,
            Some(ProjectedRow::CategoryStrip { start, end, .. }) => *start..*end,
            _ => 0..0,
        }
    }

    fn first_entry_in_row(&self, row: usize) -> Option<usize> {
        self.row_entries(row).next()
    }

    fn sticky_heading_row(&self) -> Option<usize> {
        if self.group_by == GroupBy::None {
            return None;
        }
        let offset = (-self.scroll_handle.offset().y.as_f32()).max(0.0);
        let row = self.heights.row_at_offset(offset);
        let index = self.heading_rows.partition_point(|heading| *heading <= row);
        let heading = index.checked_sub(1).map(|index| self.heading_rows[index])?;
        // 通常の heading がまだ画面内の先頭にいる間は overlay を出さない。
        // 初期表示だけ別の見た目になる二重描画を避ける。
        let bottom = self.heights.offset_before(heading)
            + self
                .heights
                .row_height(heading)
                .map_or(gpui_filer_ui::CHECKBOX_HIT_SIZE, |height| height.content);
        (bottom < offset).then_some(heading)
    }

    fn apply_group_order(&mut self) {
        // 既に sort 済みの順を section 内へ保った stable grouping。v5 と同じく
        // grouping は順序の大枠だけを決め、各 section の列 sort は壊さない。
        match self.group_by {
            GroupBy::None => {}
            GroupBy::ModifiedDate => {
                let context = ModifiedDateGroupContext::new();
                self.entries.sort_by(|left, right| {
                    let order = context
                        .bucket(left.modified)
                        .cmp(&context.bucket(right.modified));
                    // v5 は sort 未指定も新しい日付から始める。
                    if !matches!(self.sort, SortState::Asc(_)) {
                        order.reverse()
                    } else {
                        order
                    }
                });
            }
            GroupBy::Kind => self
                .entries
                .sort_by(|left, right| kind_group(left).0.cmp(&kind_group(right).0)),
        }
    }

    fn projection_anchor(&self) -> Option<PendingProjectionAnchor> {
        let offset = (-self.scroll_handle.offset().y.as_f32()).max(0.0);
        let row = self.heights.row_at_offset(offset);
        self.first_entry_in_row(row)
            .and_then(|index| self.projection_anchor_at(index, offset))
    }

    fn projection_anchor_avoiding(
        &self,
        avoided: &entry_identity::EntryKey,
    ) -> Option<PendingProjectionAnchor> {
        let offset = (-self.scroll_handle.offset().y.as_f32()).max(0.0);
        let row = self.heights.row_at_offset(offset);
        let index = self
            .row_entries(row)
            .find(|index| self.entries[*index].key != *avoided)
            .or_else(|| {
                (0..self.entries.len())
                    .rev()
                    .find(|index| self.entries[*index].key != *avoided)
            })?;
        self.projection_anchor_at(index, offset)
    }

    fn projection_anchor_at(&self, index: usize, offset: f32) -> Option<PendingProjectionAnchor> {
        let row = self.entry_row(index)?;
        Some(PendingProjectionAnchor {
            key: self.entries.get(index)?.key.clone(),
            screen_y: self.heights.offset_before(row) - offset,
            fallback_offset: offset,
            scroll_revision: self.scroll_revision,
        })
    }

    /// 幅変更で折返しや列数が変わる直前の視覚位置を key で保持する。
    /// 画面内の focus を優先し、なければ viewport 中央を含む行を使う。
    fn resize_projection_anchor(
        &self,
        offset: f32,
        viewport: f32,
    ) -> Option<PendingProjectionAnchor> {
        let is_visible = |index: usize| {
            let Some(row) = self.entry_row(index) else {
                return false;
            };
            let top = self.heights.offset_before(row);
            let bottom = top
                + self
                    .heights
                    .row_height(row)
                    .map_or(44.0, |height| height.content);
            bottom > offset && top < offset + viewport
        };
        let renaming = self.rename_session.as_ref().and_then(|session| {
            self.entries
                .iter()
                .position(|entry| entry.key == session.key)
                .filter(|index| is_visible(*index))
        });
        let focused = self.selection.focused().filter(|index| is_visible(*index));
        let index = renaming.or(focused).unwrap_or_else(|| {
            let row = self.heights.row_at_offset(offset + viewport.max(0.0) / 2.0);
            self.first_entry_in_row(row).unwrap_or(0)
        });
        self.projection_anchor_at(index, offset)
    }

    fn apply_projection_anchor(&mut self, consume: bool) {
        let Some(anchor) = self.pending_projection_anchor.as_ref() else {
            return;
        };
        if anchor.scroll_revision != self.scroll_revision {
            self.pending_projection_anchor = None;
            return;
        }
        let offset = self
            .entries
            .iter()
            .map(|entry| &entry.key)
            .position(|key| key == &anchor.key)
            .and_then(|index| self.entry_row(index))
            .map(|row| self.heights.offset_before(row) - anchor.screen_y)
            .unwrap_or(anchor.fallback_offset);
        let viewport = self.scroll_handle.bounds().size.height.as_f32();
        let offset = if viewport > 0.0 {
            clamp_scroll_offset(offset, self.heights.total_height() + 44.0, viewport)
        } else {
            offset.max(0.0)
        };
        self.scroll_handle
            .set_offset(gpui::point(px(0.0), -px(offset)));
        if consume {
            self.pending_projection_anchor = None;
        }
    }

    /// raw entry fact を Rows 表示の文字列へ一度だけ写す。
    ///
    /// `modified` は local time 変換を含むので、可視行を毎 frame 再構築する render path
    /// に置くと指スクロールの p95 を押し上げる。並べ替えは key を変えないため、並び替え
    /// だけでは作り直さない。
    fn rebuild_row_details(&mut self) {
        self.row_details.clear();
        self.row_details.reserve(self.entries.len());
        for entry in &self.entries {
            self.row_details.insert(
                entry.key.clone(),
                RowDetails {
                    kind: entry.kind_label().into(),
                    // Everything index の folder size は通常の一覧サイズとして表示する。
                    // ただし operation の byte total には使わない。
                    size: projected_entry_size(entry, &self.translator).into(),
                    modified: self.translator.modified(entry.modified).into(),
                },
            );
        }
    }

    /// 今の一覧の表示状態。移動の直前に session へ渡す。
    ///
    /// 先頭に見えている項目の key と行内 offset、そのときの選択とフォーカスの
    /// best-effort 値。
    fn view_bookmark(&mut self) -> Option<navigation_history::ViewBookmark> {
        let offset = -self.scroll_handle.offset().y.as_f32();
        let row = self.heights.row_at_offset(offset.max(0.0));
        let index = self.first_entry_in_row(row)?;
        let key = self.entries.get(index)?.key.clone();
        // 保留中の Shift 範囲はここで確定し、並びをまたぐ Shift session は終了する。
        Some(navigation_history::ViewBookmark {
            key,
            offset: offset - self.heights.offset_before(row),
            selection: self
                .selection
                .snapshot_with(|index| self.entries.get(index).map(|entry| entry.key.clone())),
        })
    }

    /// 保存する形に今の表示状態を写す。
    ///
    /// 今いる場所。
    ///
    pub(crate) fn location(&self) -> Option<Location> {
        self.session.location().cloned()
    }

    #[cfg(test)]
    pub(crate) fn navigation_history_state_for_test(&self) -> (bool, bool) {
        (self.session.can_back(), self.session.can_forward())
    }

    pub(crate) fn current_location_generation(&self) -> u64 {
        self.location_generation
    }

    #[cfg(test)]
    pub(crate) fn current_presentation_revision_for_test(&self) -> u64 {
        self.presentation_revision
    }

    pub(crate) fn row_column_visibility(&self, column: Column) -> Option<bool> {
        (self.layout == ViewLayout::Rows).then(|| match column {
            Column::Name => true,
            Column::Kind => self.column_prefs.kind,
            Column::Size => self.column_prefs.size,
            Column::Modified => self.column_prefs.modified,
        })
    }

    pub(crate) fn row_column_order(&self) -> Option<[Column; 4]> {
        (self.layout == ViewLayout::Rows).then_some(self.column_prefs.order)
    }

    pub(crate) fn folder_view_preferences(
        &self,
    ) -> Option<(std::path::PathBuf, folder_view_preferences::FolderViewPrefs)> {
        let path = self
            .session
            .location()
            .and_then(Location::path)?
            .to_path_buf();
        Some((
            path,
            folder_view_preferences::FolderViewPrefs {
                layout: self.layout.to_value(),
                sort: self.sort.into(),
                group: self.group_by.into(),
                columns: self.column_prefs,
            },
        ))
    }

    pub(crate) fn apply_folder_view_preferences(
        &mut self,
        prefs: folder_view_preferences::FolderViewPrefs,
        cx: &mut Context<Self>,
    ) {
        let resize_was_active = self.row_column_resize.take().is_some();
        self.cancel_row_column_reorder();
        let layout = ViewLayout::from_value(prefs.layout);
        let sort = SortState::from(prefs.sort);
        let group = GroupBy::from(prefs.group);
        if self.layout == layout
            && self.sort == sort
            && self.group_by == group
            && self.column_prefs == prefs.columns
        {
            if resize_was_active {
                cx.notify();
            }
            return;
        }
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        let selection = self.selection.snapshot(&keys);
        self.layout = layout;
        self.sort = sort;
        self.group_by = group;
        self.column_prefs = prefs.columns.normalized();
        sorting::sort_entries(&mut self.entries, sort);
        self.apply_group_order();
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        self.selection.restore_remapped(selection, &keys);
        self.rebuild_projected_rows();
        self.layout_width = 0.0;
        self.measured = vec![false; self.heights.len()];
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.emit_info_projection(cx);
        cx.notify();
    }

    fn emit_folder_prefs_changed(
        &self,
        field: folder_view_preferences::ChangedField,
        cx: &mut Context<Self>,
    ) {
        if let Some((path, prefs)) = self.folder_view_preferences() {
            cx.emit(DirectoryEvent::FolderPrefsChanged { path, prefs, field });
        }
    }

    /// PC 表示かは location owner である session から都度導く。
    /// view に同じ事実を複製すると、非同期の場所変更と食い違い得る。
    fn is_this_pc(&self) -> bool {
        matches!(self.session.location(), Some(Location::ThisPc))
    }

    /// location の owner は session なので、ここでは複製して渡すだけ。
    /// 列挙済み entries・選択・スクロール位置は保存しない。
    fn saved_view_state(&self) -> SavedViewState {
        SavedViewState {
            location: self.location(),
            layout: self.layout.to_value(),
            icon_scale: self.icon_scale,
            sort: self.sort.into(),
            group: self.group_by.into(),
        }
    }

    pub(crate) fn new_pane_state_for_menu(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<SavedViewState> {
        if self.location_generation != target.generation
            || self.presentation_revision != target.presentation_revision
            || target.selection_derived
        {
            return None;
        }
        let entry_key = target.item.as_ref()?;
        if !matches!(target.context_keys.as_slice(), [key] if key == entry_key) {
            return None;
        }
        let entry_identity::EntryKey::Path {
            path,
            kind: EntryKind::Folder,
        } = entry_key
        else {
            return None;
        };
        let entry = self.entries.iter().find(|entry| &entry.key == entry_key)?;
        if entry.kind != EntryKind::Folder || entry.path.as_ref() != Some(path) {
            return None;
        }
        let mut state = self.saved_view_state();
        state.location = Some(Location::Path(path.clone()));
        Some(state)
    }

    pub(crate) fn closed_tab_state(
        &mut self,
    ) -> Result<
        (
            SavedViewState,
            navigation_history::NavigationHistorySnapshot,
        ),
        session_store::SessionError,
    > {
        self.detect_pending_restore_scroll();
        // A navigation can be pending while the old projection is still on screen. Keep the
        // session's prior bookmark instead of attaching another folder's rows.
        let live_pending_pc_wsl_restore = self.is_this_pc()
            && self.pending_restore.as_ref().is_some_and(|pending| {
                pending.scroll_revision == self.scroll_revision
                    && self.bookmark_waits_for_pc_wsl(&pending.bookmark)
            });
        let refresh_current = self.session.has_accepted_current_listing()
            && (!self.is_this_pc()
                || (self.pc_base_sources_complete && !live_pending_pc_wsl_restore));
        let (selection_count, selection_bytes) = if refresh_current {
            let (count, mut bytes) = self.selection.snapshot_preflight(|index| {
                self.entries.get(index).map_or(Ok(0), |entry| {
                    session_store::estimate_entry_key_size(&entry.key)
                })
            })?;
            let offset = -self.scroll_handle.offset().y.as_f32();
            let row = self.heights.row_at_offset(offset.max(0.0));
            if let Some(key) = self
                .first_entry_in_row(row)
                .and_then(|index| self.entries.get(index))
                .map(|entry| &entry.key)
            {
                bytes = bytes.saturating_add(session_store::estimate_entry_key_size(key)?);
            }
            (count, bytes)
        } else {
            (0, 0)
        };
        let location =
            self.session
                .location()
                .ok_or(session_store::SessionError::LimitExceeded(
                    "closed tab location",
                ))?;
        session_store::preflight_closed_tab_capture(
            location,
            self.session.navigation_history(),
            refresh_current,
            selection_count,
            selection_bytes,
        )?;
        if refresh_current {
            let bookmark = self.view_bookmark();
            self.session.set_current_bookmark(bookmark);
        }
        Ok((self.saved_view_state(), self.session.navigation_snapshot()))
    }

    /// 移動先の entry のスクロール位置を控える。
    ///
    /// session に寿命を持たせず、view が次の読取成功まで持ち越す。
    fn remember_restore_target(&mut self) {
        self.pending_restore =
            self.session
                .current_bookmark()
                .cloned()
                .map(|bookmark| PendingRestore {
                    bookmark,
                    scroll_revision: self.scroll_revision,
                });
    }

    /// ユーザーが選択を操作したら、遅れて届く復元がその選択を上書きしないようにする。
    fn invalidate_pending_restore_for_selection(&mut self) {
        self.pending_restore = None;
    }

    /// Native touch pan may update ScrollHandle without dispatching a wheel event to the view.
    /// Compare before layout anchoring, then refresh the baseline after programmatic layout
    /// offsets settle so resize/projection corrections are not mistaken for user input.
    fn detect_pending_restore_scroll(&mut self) {
        let offset = self.scroll_handle.offset();
        if self.pending_restore.as_ref().is_some_and(|pending| {
            pending.scroll_revision == self.scroll_revision && offset != self.observed_scroll_offset
        }) {
            self.pending_restore = None;
            self.scroll_revision = self.scroll_revision.wrapping_add(1);
        }
        self.observed_scroll_offset = offset;
    }

    fn note_user_scroll(&mut self) {
        self.pending_restore = None;
        self.scroll_revision = self.scroll_revision.wrapping_add(1);
        self.touch_item_tap_series = None;
    }

    /// 移動先の entry の表示状態を戻す。ユーザーが先にスクロールしていれば適用しない。
    fn apply_pending_restore(&mut self, cx: &mut Context<Self>) {
        if self.is_this_pc() && !self.pc_base_sources_complete {
            return;
        }
        if let Some(pending) = self.pending_restore.as_ref() {
            if pending.scroll_revision != self.scroll_revision {
                self.pending_restore = None;
                return;
            }
            if self.is_this_pc() && self.bookmark_waits_for_pc_wsl(&pending.bookmark) {
                return;
            }
        }
        if self.entries.is_empty() {
            // 一覧がまだ来ていない。次の frame で試す。
            return;
        }
        let Some(pending) = self.pending_restore.take() else {
            return;
        };
        if pending.scroll_revision != self.scroll_revision {
            // 新しいユーザー操作の後に古い位置を適用しない。
            return;
        }
        // 選択とフォーカスを先に戻す。消えた項目は落とし、隣へは移さない。
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        self.selection.restore(pending.bookmark.selection, &keys);
        self.emit_info_projection(cx);

        let Some(row) = self
            .entries
            .iter()
            .position(|entry| entry.key == pending.bookmark.key)
            .and_then(|index| self.entry_row(index))
        else {
            // 先頭に見えていた項目が消えていても、選択は戻した分を残す。
            return;
        };
        let offset = (self.heights.offset_before(row) + pending.bookmark.offset).max(0.0);
        self.scroll_handle
            .set_offset(gpui::point(px(0.0), -px(offset)));
    }

    /// WSL rows may arrive much later than known folders and drive identities. Wait only when
    /// this bookmark contains an absent WSL key; unrelated missing keys can already be dropped.
    fn bookmark_waits_for_pc_wsl(&self, bookmark: &navigation_history::ViewBookmark) -> bool {
        if self.pc_wsl_loaded {
            return false;
        }
        let (selected, anchor, focus, _) = bookmark.selection.parts();
        std::iter::once(&bookmark.key)
            .chain(selected.iter())
            .chain(anchor)
            .chain(focus)
            .any(|key| is_wsl_pc_key(key) && !self.entries.iter().any(|entry| entry.key == *key))
    }

    /// session が返した要求を background executor へ渡す。
    ///
    /// 結果は event として session へ戻す。GPUI の asset は使わない。
    ///
    /// この関数は `view.update` の中から呼ばれることがある（`watch_ready` など）。
    /// その中で更に `view.update` すると二重更新で panic するので、effect は
    /// 一度キューへ積み、次のフレームで実行する。
    fn run_effect(&mut self, effect: directory_session::SessionEffect, cx: &mut Context<Self>) {
        self.pending_effects.push_back(effect);
        self.drain_effects(cx);
    }

    /// 溜まった effect を実行する。既に drain 中なら何もしない。
    fn drain_effects(&mut self, cx: &mut Context<Self>) {
        if self.draining_effects {
            return;
        }
        self.draining_effects = true;
        while let Some(effect) = self.pending_effects.pop_front() {
            self.spawn_effect(effect, cx);
        }
        self.draining_effects = false;
    }

    fn spawn_effect(&mut self, effect: directory_session::SessionEffect, cx: &mut Context<Self>) {
        match effect {
            directory_session::SessionEffect::Enumerate(ticket) => {
                // F5/watch が外部index sizeを invalidate したら、scan 完了を待たず表示を
                // 空へ戻す。last-good の entries は残るので画面が空になることはない。
                self.apply_listing(cx);
                let path = ticket.path.clone();
                let view = cx.entity().downgrade();
                cx.spawn(async move |_this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { directory::enumerate(path) })
                        .await;
                    let _ = view.update(cx, |this, cx| {
                        let scan_failed = result.is_err();
                        if this.session.commit_enumerated(&ticket, result) {
                            if scan_failed {
                                this.rename_reconciliation_failed(&ticket);
                            }
                            this.apply_listing(cx);
                            if let Some(request) = this.session.request_folder_sizes() {
                                this.spawn_folder_size_request(request, cx);
                            }
                        }
                        // 読取中に来た変更があれば、ここで一度だけ読み直す。
                        if let Some(effect) = this.session.take_follow_up() {
                            this.run_effect(effect, cx);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            directory_session::SessionEffect::EnumerateArchive(ticket) => {
                self.apply_listing(cx);
                let location = ticket.location.clone();
                let cancellation = ticket.cancellation();
                let is_7z = matches!(
                    &location,
                    Location::Archive { source, .. }
                        if source.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("7z"))
                );
                #[cfg(windows)]
                let provider = cx
                    .try_global::<archive_provider::ArchiveProviderOwner>()
                    .cloned()
                    .unwrap_or_default();
                #[cfg(windows)]
                let capability = is_7z
                    .then(|| provider.capture_7z_read_capability())
                    .flatten();
                #[cfg(windows)]
                let worker_provider = provider.clone();
                #[cfg(windows)]
                let worker_capability = capability.clone();
                #[cfg(all(windows, test))]
                let test_image_path = self.test_application_image_path.clone();
                #[cfg(test)]
                let completion_gate = self.archive_completion_test_gate.take();
                #[cfg(test)]
                let worker_completion_gate = completion_gate.clone();
                let worker_location = location.clone();
                let view = cx.entity().downgrade();
                cx.spawn(async move |_this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            #[cfg(test)]
                            let result = {
                                #[cfg(windows)]
                                let result = read_archive_background(
                                    &worker_provider,
                                    worker_capability.as_ref(),
                                    &worker_location,
                                    &cancellation,
                                    #[cfg(test)]
                                    test_image_path,
                                );
                                #[cfg(not(windows))]
                                let result = archive_listing::read(&worker_location, &cancellation);
                                if let Some(gate) = worker_completion_gate {
                                    let _ = gate.result_ready.send(()).await;
                                    let _ = gate.release.recv().await;
                                }
                                result
                            };
                            #[cfg(not(test))]
                            let result = {
                                #[cfg(windows)]
                                let result = read_archive_background(
                                    &worker_provider,
                                    worker_capability.as_ref(),
                                    &worker_location,
                                    &cancellation,
                                );
                                #[cfg(not(windows))]
                                let result = archive_listing::read(&worker_location, &cancellation);
                                result
                            };
                            result
                        })
                        .await;
                    let mut result = Some(result);
                    let mut disposal = None;
                    let _ = view.update(cx, |this, cx| {
                        if let Some(result) = result.take() {
                            #[cfg(windows)]
                            let outcome = if is_7z {
                                this.session.commit_archive_7z(
                                    &provider,
                                    capability.as_ref(),
                                    &ticket,
                                    result,
                                )
                            } else {
                                this.session.commit_archive_result(&ticket, result)
                            };
                            #[cfg(not(windows))]
                            let outcome = this.session.commit_archive_result(&ticket, result);
                            let (committed, cleanup) = outcome.into_parts();
                            if committed {
                                this.apply_listing(cx);
                            }
                            disposal = Some(cleanup);
                        }
                        cx.notify();
                    });
                    // entity が破棄済みなら result は外側に残る。manifest と native lease は
                    // 成否にかかわらず background executor 側で解放する。
                    cx.background_executor()
                        .spawn(async move {
                            drop(disposal);
                            drop(result);
                            #[cfg(test)]
                            if let Some(gate) = completion_gate {
                                let _ = gate.disposed.send(()).await;
                            }
                        })
                        .await;
                })
                .detach();
            }
            // 登録は `RecommendedWatcher::watch` を呼ぶので、input path に置かない。
            // 切断されたネットワーク path で短く block し得る。
            directory_session::SessionEffect::LoadVirtual(ticket) => {
                // asset 自体は共有キャッシュでも、現在の session ticket が一致しない限り
                // 表示へ写さない。A → PC → A の古い PC 結果を再入場へ混ぜない。
                if !self.session.accepts_virtual(&ticket) {
                    return;
                }
                match &ticket.location {
                    Location::ThisPc => {
                        self.pc_ticket = Some(ticket);
                        cx.notify();
                    }
                    Location::ShellRoot(_) | Location::ShellItem { .. } => {
                        let location = ticket.location.clone();
                        let view = cx.entity().downgrade();
                        cx.spawn(async move |_this, cx| {
                            let result = cx
                                .background_executor()
                                .spawn(async move { special::shell_rows(&location) })
                                .await;
                            let _ = view.update(cx, |this, cx| {
                                if !this.session.complete_virtual(&ticket) {
                                    return;
                                }
                                this.apply_shell_rows(result, cx);
                            });
                        })
                        .detach();
                    }
                    Location::Path(_) => unreachable!("virtual ticket has no path location"),
                    Location::Archive { .. } => {
                        unreachable!("archive locations use an archive ticket")
                    }
                }
            }
            directory_session::SessionEffect::SubscribeWatch { .. }
            | directory_session::SessionEffect::UnsubscribeWatch { .. } => {
                let view = cx.entity().downgrade();
                cx.spawn(async move |_this, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.watch.apply(&effect, cx);
                    });
                })
                .detach();
            }
        }
    }

    /// Everything IPC は同期呼出しなので、一覧の effect と同じ background executor に
    /// 置く。DirectorySession が request identity を保持し、navigation/F5/watch が来た
    /// あとの late result はここで表示へ触れない。
    fn spawn_folder_size_request(
        &mut self,
        request: directory_session::FolderSizeRequest,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity().downgrade();
        let retry_ticket = request.ticket.clone();
        cx.spawn(async move |_this, cx| {
            let cancellation = request.ticket.cancellation();
            let probes = request.probes.clone();
            let provider_revision = request.provider_revision;
            let bypass_cache = request.bypass_cache;
            let query_cancellation = cancellation.clone();
            let background_executor = cx.background_executor().clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    everything::query_folder_sizes(
                        background_executor,
                        probes,
                        &query_cancellation,
                        provider_revision,
                        bypass_cache,
                    )
                    .await
                })
                .await;
            if cancellation.load(Ordering::Acquire) {
                return;
            }
            let _ = view.update(cx, |this, cx| {
                if matches!(outcome.terminal, everything::ProviderTerminal::Cancelled) {
                    cx.notify();
                    return;
                }
                let mut retry_invalidated = outcome.retry_invalidated
                    || outcome
                        .results
                        .iter()
                        .any(|result| !result.freshness.is_current());
                let committed = this
                    .session
                    .commit_folder_sizes(&request.ticket, outcome.results);
                if committed {
                    this.apply_listing(cx);
                }
                retry_invalidated |= everything::ProviderRevision::current()
                    != request.ticket.provider_revision;
                let stale_results = retry_invalidated
                    && outcome.terminal == everything::ProviderTerminal::Completed;
                if stale_results
                    && let Some(retry) = this
                        .session
                        .folder_size_retry_after_invalidation(&retry_ticket)
                {
                    match retry {
                        directory_session::FolderSizeRetry::Immediate => {
                            if let Some(request) = this.session.request_folder_sizes() {
                                this.spawn_folder_size_request(request, cx);
                            }
                        }
                        directory_session::FolderSizeRetry::Delayed { token, delay } => {
                            let view = cx.entity().downgrade();
                            cx.spawn(async move |_this, cx| {
                                cx.background_executor().timer(delay).await;
                                let _ = view.update(cx, |this, cx| {
                                    if let Some(request) =
                                        this.session.folder_size_retry_request(&token)
                                    {
                                        this.spawn_folder_size_request(request, cx);
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// session の last-good を表示用の entries へ展開する。
    ///
    /// 並びは view の sort が決める。snapshot は sort 設定を持たない。
    fn apply_listing(&mut self, cx: &mut Context<Self>) {
        // PC の一覧は session のものではない。古い場所の読取が後から届いても書き戻さない。
        if self.is_this_pc() {
            return;
        }
        // drag 中の index は新しい projection を指せないので、commit ではなく cancel する。
        // `listing` を借りる前に済ませ、session の immutable borrow と混ぜない。
        self.cancel_rectangle_drag();
        let show_hidden_items = cx
            .try_global::<show_hidden_items::ShowHiddenItems>()
            .is_some_and(|setting| setting.0);
        let listing = match self.session.location().cloned() {
            Some(Location::Archive { source, components }) => {
                self.session.archive_manifest().map(|manifest| {
                    std::sync::Arc::new(archive_listing::project_children(
                        &source,
                        &components,
                        manifest,
                    ))
                })
            }
            _ => self.session.listing().cloned(),
        };
        let Some(listing) = listing else {
            if !show_hidden_items && self.entries.iter().any(|entry| entry.is_hidden) {
                let keys = self
                    .entries
                    .iter()
                    .map(|entry| entry.key.clone())
                    .collect::<Vec<_>>();
                let selection = self.selection.snapshot(&keys);
                self.entries.retain(|entry| !entry.is_hidden);
                let keys = self
                    .entries
                    .iter()
                    .map(|entry| entry.key.clone())
                    .collect::<Vec<_>>();
                self.selection.restore_remapped(selection, &keys);
                let visible_keys = keys.into_iter().collect::<std::collections::HashSet<_>>();
                self.entry_icons.retain(|key, _| visible_keys.contains(key));
                self.rebuild_row_details();
                self.reset_projected_rows();
                self.presentation_revision = self.presentation_revision.wrapping_add(1);
                self.hovered_item_part = None;
                self.emit_info_projection(cx);
                cx.notify();
            }
            return;
        };
        // Rename の filesystem authority は runtime だが、watcher は rename worker の
        // terminal より先に同じ directory の listing を届け得る。その途中結果をここで
        // projection へ写すと、保持した editor 高と別の anchor が混ざって scroll が
        // 恒久的にずれる。accepted terminal が届くまでは session の last-good にだけ
        // 保持し、terminal が要求した次の listing を一回だけ表示へ反映する。
        let holding_rename_listing = self
            .pending_rename_geometry
            .as_ref()
            .is_some_and(|pending| {
                !pending.terminal_waiting_for_listing
                    && pending.location_generation == self.location_generation
                    && pending.presentation_revision == self.presentation_revision
            });
        if holding_rename_listing {
            return;
        }
        // 交換の直前に選択と viewport anchor を key で退避する。
        let waiting_for_renamed_entry =
            self.pending_rename_geometry
                .as_ref()
                .is_some_and(|pending| {
                    pending.terminal_waiting_for_listing
                        && pending.location_generation == self.location_generation
                        && pending.presentation_revision == self.presentation_revision
                });
        let current_scan = self.session.accepted_scan_ticket();
        let settled_rename = waiting_for_renamed_entry
            && self
                .pending_rename_geometry
                .as_ref()
                .is_some_and(|pending| {
                    current_scan.is_some_and(|scan| {
                        pending.accepted_scan_before_terminal.as_ref() != Some(scan)
                            && pending.pending_scan_before_terminal.as_ref() != Some(scan)
                            && scan.path == pending.directory
                    })
                });
        let failed_rename_reconciliation = waiting_for_renamed_entry
            && self
                .pending_rename_geometry
                .as_ref()
                .is_some_and(|pending| {
                    pending
                        .failed_scan_after_terminal
                        .as_ref()
                        .is_some_and(|ticket| {
                            ticket.path == pending.directory
                                && self.session.pending().is_none()
                                && matches!(
                                    self.session.status(),
                                    directory_session::ListingStatus::RefreshFailed(_)
                                )
                        })
                });
        // terminal より後に受理された一覧なら、destination の消失時も geometry pin を解放する。
        if waiting_for_renamed_entry && !settled_rename && !failed_rename_reconciliation {
            return;
        }
        let destination_matches_renamed_source =
            self.pending_rename_geometry
                .as_ref()
                .is_some_and(|pending| {
                    listing.iter().any(|entry| {
                        entry.key == pending.expected_key
                            && pending.expected_identity.as_ref().is_some_and(|expected| {
                                entry.source_identity.as_ref().is_some_and(|identity| {
                                    identity.filesystem_identity() == *expected
                                })
                            })
                    })
                });
        let settled_geometry = if settled_rename || failed_rename_reconciliation {
            self.pending_rename_geometry.take()
        } else {
            None
        };
        let rename_anchor = settled_geometry
            .as_ref()
            .map(|pending| {
                if destination_matches_renamed_source {
                    pending.anchor.clone()
                } else {
                    pending.fallback_anchor.clone()
                }
            })
            .flatten()
            .or_else(|| self.pending_rename_anchor.take());
        self.defer_projection_anchor_until_measured = rename_anchor.is_some();
        self.pending_projection_anchor = rename_anchor.or_else(|| self.projection_anchor());
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        let mut snapshot = self.selection.snapshot(&keys);
        if settled_rename && let Some(pending) = settled_geometry.as_ref() {
            let source_key_still_matches = listing.iter().any(|entry| {
                entry.key == pending.key
                    && pending.expected_identity.as_ref().is_some_and(|expected| {
                        entry
                            .source_identity
                            .as_ref()
                            .is_some_and(|identity| identity.filesystem_identity() == *expected)
                    })
            });
            if destination_matches_renamed_source {
                snapshot.remap_key(&pending.key, &pending.expected_key);
            }
            if !source_key_still_matches {
                let selection_mode = snapshot.selection_mode();
                let (selected, anchor, focus, focus_origin) = snapshot.parts();
                let selected = selected
                    .iter()
                    .filter(|key| *key != &pending.key)
                    .cloned()
                    .collect();
                snapshot = selection::SelectionSnapshot::from_parts_with_mode(
                    selected,
                    anchor.filter(|key| *key != &pending.key).cloned(),
                    focus.filter(|key| *key != &pending.key).cloned(),
                    focus_origin,
                    selection_mode,
                );
            }
        }
        self.entries = visible_directory_entries(listing.as_ref(), show_hidden_items);
        self.entry_icons.clear();
        sorting::sort_entries(&mut self.entries, self.sort);
        self.apply_group_order();
        self.rebuild_row_details();
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        self.selection.restore_remapped(snapshot, &keys);
        // 並びが変わったので、index を捕捉した古い gesture を無効にする。
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        // 同じ location の reconciliation は `reset_projected_rows` だけで十分。
        // ここで幅まで無効化すると次 frame で virtual rows を再 reset し、
        // rename 確定後に現在の scroll anchor が飛んでしまう。
        self.reset_projected_rows();
        self.hovered_item_part = None;
        self.emit_info_projection(cx);
        cx.notify();
    }

    /// PC の行を asset から受け取り、表示用の entries へ写す。
    ///
    /// 3つの解決は別々に走るので、**届いたものから並べる**。順序は [`special::rows`] が
    /// 決める。届く前は空で、状態行に「読み込み中」を出す。
    fn apply_pc_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ticket) = self.pc_ticket.clone() else {
            return;
        };
        if !self.session.commit_virtual(&ticket) {
            return;
        }
        let folders = window.use_asset::<special::PcKnownFolders>(&(), cx);
        let letters = window.use_asset::<special::PcDrives>(&(), cx);
        let labels = window.use_asset::<special::PcDriveLabels>(&(), cx);
        let wsl = window.use_asset::<special::PcWsl>(&(), cx);
        let mut known_folder_identities = std::collections::HashMap::new();
        for (_, path) in folders.as_deref().unwrap_or_default() {
            let identity_source = (path.clone(), self.shell_icon_identity_generation);
            if let Some(identity) =
                window.use_asset::<special::PcKnownFolderIdentity>(&identity_source, cx)
            {
                if let Some(identity) = identity.clone() {
                    known_folder_identities.insert(path.clone(), identity);
                }
            }
        }
        let mut drive_identities = std::collections::HashMap::new();
        for letter in letters.as_deref().unwrap_or_default() {
            let identity_source = (*letter, self.shell_icon_identity_generation);
            if let Some(identity) =
                window.use_asset::<special::PcDriveIdentity>(&identity_source, cx)
            {
                if let Some(identity) = identity.clone() {
                    drive_identities.insert(*letter, identity);
                }
            }
        }
        self.pc_base_sources_complete = folders.is_some() && letters.is_some();
        self.pc_wsl_loaded = wsl.is_some();
        // Libraries は同期で組み立てられる typed root。ほかの PC source がまだ届かなくても
        // 先に出し、遅い network/WSL probe を入口の表示に結び付けない。
        self.pc_loaded = true;
        let rows = special::rows(&special::PcSources {
            known_folders: folders.unwrap_or_default(),
            known_folder_identities,
            drives: special::drives_with_labels(
                &letters.unwrap_or_default(),
                &labels.unwrap_or_default(),
            ),
            drive_identities,
            wsl_distributions: wsl.unwrap_or_default(),
        });
        if rows == self.pc_rows {
            // 同じ内容を毎フレーム書き戻さない。選択とスクロールを保つ。
            return;
        }
        self.pc_rows = rows;
        self.entries.clear();
        self.entry_targets.clear();
        self.entry_icons.clear();
        self.pc_shell_icon_targets.clear();
        self.shell_icon_generation = self.shell_icon_generation.wrapping_add(1);
        for row in &self.pc_rows {
            let (entry, key, target) = pc_entry(row);
            if matches!(
                row.kind,
                special::PcEntryKind::KnownFolder | special::PcEntryKind::Drive
            ) && entry.path.is_some()
                && entry.source_identity.is_some()
            {
                self.pc_shell_icon_targets.insert(key.clone());
            }
            self.entries.push(entry);
            self.entry_targets.insert(key, target);
            if let Some(icon) = pc_icon(row.kind) {
                self.entry_icons
                    .insert(self.entries.last().unwrap().key.clone(), icon);
            }
        }
        self.rebuild_row_details();
        self.reset_projected_rows();
        // 並びが確定したので、index を捕捉した古い gesture を無効にする。
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.emit_info_projection(cx);
        cx.notify();
    }

    /// Shell namespace の background enumeration を表示用 projection へ写す。
    fn apply_shell_rows(
        &mut self,
        result: Result<Vec<special::ShellEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        self.library_loaded = true;
        match result {
            Ok(rows) => {
                self.library_error = None;
                self.entries.clear();
                self.entry_targets.clear();
                self.entry_icons.clear();
                for row in &rows {
                    let (entry, key, target) = shell_entry(row);
                    self.entries.push(entry);
                    if let Some(target) = target {
                        self.entry_targets.insert(key, target);
                    }
                }
                self.rebuild_row_details();
                self.reset_projected_rows();
                self.presentation_revision = self.presentation_revision.wrapping_add(1);
                self.emit_info_projection(cx);
            }
            Err(error) => self.library_error = Some(error),
        }
        cx.notify();
    }

    fn render_breadcrumbs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some(input) = self
            .address_edit
            .as_ref()
            .map(|session| session.input.clone())
        {
            let input_for_pointer = input.clone();
            return div()
                .id("breadcrumb-address-editor")
                .w_full()
                .h(px(52.0))
                .flex_shrink_0()
                .px(px(12.0))
                .flex()
                .items_center()
                .bg(gpui_filer_ui::theme_palette(cx).surface_bar)
                .text_size(px(CHROME_FONT_SIZE))
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    cx.stop_propagation();
                    input_for_pointer.update(cx, |input, cx| {
                        input.focus_at(event.position, window, cx);
                    });
                })
                .child(div().w_full().min_w_0().overflow_hidden().child(input));
        }
        let location = self.location();
        let paths = match location.as_ref() {
            Some(Location::Archive { source, .. }) => source
                .parent()
                .map(navigation::ancestors)
                .unwrap_or_default(),
            Some(location) => location
                .path()
                .map(navigation::ancestors)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // 先頭は常に PC（Explorer と同じ）。path を持たない一覧（サンプル）だけは出さない。
        let crumbs: Vec<Crumb> = match &location {
            Some(Location::Path(_)) => std::iter::once(Crumb::Pc)
                .chain(paths.iter().cloned().map(Crumb::Path))
                .collect(),
            Some(Location::Archive { source, components }) => {
                let mut crumbs = std::iter::once(Crumb::Pc)
                    .chain(paths.iter().cloned().map(Crumb::Path))
                    .collect::<Vec<_>>();
                crumbs.push(Crumb::Archive(Location::Archive {
                    source: source.clone(),
                    components: Vec::new(),
                }));
                for depth in 1..=components.len() {
                    crumbs.push(Crumb::Archive(Location::Archive {
                        source: source.clone(),
                        components: components[..depth].to_vec(),
                    }));
                }
                crumbs
            }
            Some(Location::ThisPc) => vec![Crumb::Pc],
            Some(Location::ShellRoot(root)) => vec![Crumb::Pc, Crumb::ShellRoot(*root)],
            Some(item @ Location::ShellItem { root, .. }) => {
                vec![
                    Crumb::Pc,
                    Crumb::ShellRoot(*root),
                    Crumb::ShellItem(item.clone()),
                ]
            }
            None => Vec::new(),
        };
        self.ensure_navigation_identity_projection(paths.clone(), cx);
        // path のラベルは shell の表示名（届いていれば）か末尾の名前。
        let mut path_labels: Vec<gpui::SharedString> = paths
            .iter()
            .map(|path| navigation::label(path).into())
            .collect();
        if let Some(path) = location.as_ref().and_then(Location::path)
            && let Some(names) = window
                .use_asset::<navigation::DisplayNames>(&(path.to_path_buf(), self.names_epoch), cx)
        {
            path_labels = names.into_iter().map(Into::into).collect();
            if !self.breadcrumb_names_ready {
                self.breadcrumb_names_ready = true;
                self.reveal_breadcrumb = true;
            }
        }
        let mut labels: Vec<gpui::SharedString> = Vec::with_capacity(crumbs.len());
        for crumb in &crumbs {
            let label = match crumb {
                Crumb::Pc => self.translator.text(Message::ThisPc).into_owned(),
                Crumb::ShellRoot(root) => root.display_name().into(),
                Crumb::ShellItem(Location::ShellItem { display_name, .. }) => display_name.clone(),
                Crumb::ShellItem(_) => unreachable!("Shell crumb has a Shell location"),
                Crumb::Path(_) => path_labels.remove(0).to_string(),
                Crumb::Archive(Location::Archive { source, components }) => {
                    components.last().cloned().unwrap_or_else(|| {
                        source
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| source.display().to_string())
                    })
                }
                Crumb::Archive(_) => unreachable!("archive crumb has an archive location"),
            };
            labels.push(label.into());
        }
        let viewport_width = self
            .measured_viewport_width
            .unwrap_or(self.pane_width_projection);
        let previous_viewport_width = self.breadcrumb_viewport_width.replace(viewport_width);
        let viewport_width_changed = previous_viewport_width
            .is_some_and(|previous| (viewport_width - previous).abs() > f32::EPSILON);
        let viewport_shrank =
            previous_viewport_width.is_some_and(|previous| viewport_width < previous);
        if (self.reveal_breadcrumb || viewport_shrank) && !labels.is_empty() {
            let index = (labels.len() - 1) * 2;
            let generation = self.location_generation;
            let view = cx.entity().downgrade();
            // 初回・表示名の確定後・window 縮小時は、ラベルのboundsが揃った次 frame
            // で現在地を見せる。幅を広げたときは手動の scroll offset を動かさない。
            window.on_next_frame(move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.location_generation == generation {
                        this.breadcrumb_scroll.scroll_to_item(index);
                        cx.notify();
                    }
                });
            });
            self.reveal_breadcrumb = false;
        } else if viewport_width_changed {
            // ScrollHandle の viewport / max offset はこの frame の layout 後に確定する。
            // 拡大だけのときも一度だけ描き直し、消えた overflow のつまみを残さない。
            let view = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = view.update(cx, |_this, cx| cx.notify());
            });
        }
        if labels.is_empty() {
            labels.push(self.translator.text(Message::SampleList).into());
        }
        let generation = self.location_generation;
        let crumb_entity = cx.entity().downgrade();
        let crumb_target_view_id = cx.entity().entity_id();
        let crumb_snapshots = crumbs
            .iter()
            .map(|crumb| match crumb {
                Crumb::Path(path) => {
                    Some(self.capture_navigation_target_snapshot(crumb_target_view_id, path, false))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let crumb_shell = self.external_drop_shell.clone();
        let crumb_target_tab_id = self.external_drop_view_id;
        let crumb_paths_cache = self.external_file_drop_paths.clone();
        let crumb_drag_session = self.external_drag_session.clone();
        let crumb_decorator_entity = crumb_entity.clone();
        let crumb_decorator_shell = crumb_shell.clone();
        let crumb_decorator = move |index: usize, element: gpui::Div| {
            let snapshot = crumb_snapshots.get(index).cloned().flatten();
            let target_entity = crumb_decorator_entity.clone();
            let hover_entity = target_entity.clone();
            let hover_snapshot = snapshot.clone();
            let drop_snapshot = snapshot;
            let hover_drop_entity = hover_entity.clone();
            let hover_effect = Rc::new(Cell::new(false));
            let hover_effect_for_event = hover_effect.clone();
            let hover_event_entity = hover_entity.clone();
            let hover_event_snapshot = hover_snapshot.clone();
            let hover_drag_session = crumb_drag_session.clone();
            let hover_visual_snapshot = hover_snapshot;
            let internal_shell = crumb_decorator_shell.clone();
            let internal_drop_shell = crumb_decorator_shell.clone();
            let internal_hover_entity = hover_entity.clone();
            let internal_drop_entity = target_entity.clone();
            let internal_hover_snapshot = drop_snapshot.clone();
            let internal_drop_snapshot = drop_snapshot.clone();
            let internal_tab_id = crumb_target_tab_id;
            let internal_view_id = crumb_target_view_id;
            let drop_entity = target_entity;
            let shell = crumb_decorator_shell.clone();
            let local_ole_hover_shell = crumb_decorator_shell.clone();
            let local_ole_drop_shell = crumb_decorator_shell.clone();
            let tab_id = crumb_target_tab_id;
            let event_paths_cache = crumb_paths_cache.clone();
            let drop_paths_cache = crumb_paths_cache.clone();
            element
                .drag_over::<InternalDragPayload>(move |style, payload, _window, cx| {
                    let eligible = internal_tab_id
                        .and_then(|tab_id| {
                            peek_internal_drag_snapshot(&payload.snapshot)
                                .zip(internal_hover_snapshot.as_ref())
                                .and_then(|(source, snapshot)| {
                                    internal_hover_entity
                                        .read_with(cx, |this, _| {
                                            this.navigation_drop_target(snapshot)
                                        })
                                        .ok()
                                        .flatten()
                                        .map(|target| (source, target, tab_id))
                                })
                        })
                        .is_some_and(|(source, target, tab_id)| {
                            internal_shell
                                .as_ref()
                                .and_then(|shell| {
                                    shell
                                        .read_with(cx, |shell, cx| {
                                            shell.internal_navigation_move_candidate_for_ui(
                                                payload.source,
                                                &source,
                                                tab_id,
                                                internal_view_id,
                                                &target,
                                                cx,
                                            )
                                        })
                                        .ok()
                                })
                                .unwrap_or(false)
                        });
                    if eligible {
                        style
                            .border_2()
                            .border_color(gpui::hsla(0.58, 0.45, 0.62, 0.8))
                    } else {
                        style
                    }
                })
                .on_drop::<InternalDragPayload>(move |payload, _window, cx| {
                    cx.stop_propagation();
                    let source = take_internal_drag_snapshot(&payload.snapshot);
                    let target = internal_drop_snapshot.as_ref().and_then(|snapshot| {
                        internal_drop_entity
                            .read_with(cx, |this, _| this.navigation_drop_target(snapshot))
                            .ok()
                            .flatten()
                    });
                    if let (Some(tab_id), Some(shell), Some(source)) =
                        (internal_tab_id, internal_drop_shell.as_ref(), source)
                    {
                        let _ = shell.update(cx, |shell, cx| {
                            shell.admit_internal_navigation_move(
                                payload.source,
                                source,
                                tab_id,
                                internal_view_id,
                                target,
                                cx,
                            )
                        });
                    }
                })
                .on_mouse_move(move |_, window, cx| {
                    hover_effect_for_event.set(false);
                    let Some(context) = window.file_drop_context() else {
                        return;
                    };
                    context.response.set_effect(gpui::FileDropEffect::None);
                    if context.local_drag_session.is_some() {
                        event_paths_cache.borrow_mut().take();
                        let (Some(local_token), Some(snapshot), Some(shell), Some(tab_id)) = (
                            context.local_drag_session,
                            hover_event_snapshot.as_ref(),
                            local_ole_hover_shell.as_ref(),
                            tab_id,
                        ) else {
                            return;
                        };
                        let paths = context.paths.as_ref().map(|paths| paths.paths());
                        if paths.is_some_and(|paths| paths.is_empty()) {
                            return;
                        }
                        let _ = hover_event_entity.update(cx, |this, cx| {
                            this.prepare_navigation_target(snapshot, cx);
                        });
                        let target = hover_event_entity
                            .read_with(cx, |this, _| this.navigation_drop_target(snapshot))
                            .ok()
                            .flatten();
                        let effect = target.and_then(|target| {
                            shell
                                .read_with(cx, |shell, cx| {
                                    shell.local_ole_navigation_move_candidate_effect(
                                        local_token.value(),
                                        tab_id,
                                        crumb_target_view_id,
                                        &target,
                                        paths,
                                        cx,
                                    )
                                })
                                .ok()
                                .flatten()
                        });
                        if let Some(effect) = effect {
                            hover_effect_for_event.set(true);
                            context.response.set_effect(match effect {
                                shell::LocalOleDropEffect::Copy => gpui::FileDropEffect::Copy,
                                shell::LocalOleDropEffect::Move => gpui::FileDropEffect::Move,
                            });
                        }
                        return;
                    }
                    let paths = if let Some(paths) = context.paths.as_ref() {
                        let paths = paths.paths().to_vec();
                        *event_paths_cache.borrow_mut() =
                            (!paths.is_empty()).then_some(paths.clone());
                        paths
                    } else {
                        event_paths_cache.borrow().clone().unwrap_or_default()
                    };
                    if paths.is_empty() {
                        return;
                    }
                    let Some(snapshot) = hover_event_snapshot.as_ref() else {
                        return;
                    };
                    let _ = hover_event_entity.update(cx, |this, cx| {
                        this.prepare_navigation_target(snapshot, cx);
                    });
                    let target = hover_event_entity
                        .read_with(cx, |this, _| {
                            this.navigation_drop_target(snapshot)
                        })
                        .ok()
                        .flatten();
                    if let Some(target) = target {
                        let payload = gpui::ExternalPaths(paths.clone().into());
                        capture_external_drag_session(
                            &hover_drag_session,
                            &payload,
                            target.location_generation(),
                            target.presentation_revision(),
                        );
                        let target_for_prep = target.clone();
                        let _ = hover_event_entity.update(cx, |this, cx| {
                            this.prepare_external_drop(&paths, &target_for_prep, cx);
                        });
                        hover_effect_for_event.set(true);
                        context.response.set_effect(gpui::FileDropEffect::Copy);
                    }
                })
                .drag_over::<gpui::ExternalPaths>(move |style, _payload, _window, cx| {
                    let eligible = hover_effect.get()
                        && hover_drop_entity
                            .read_with(cx, |this, _| {
                                hover_visual_snapshot
                                    .as_ref()
                                    .and_then(|snapshot| this.navigation_drop_target(snapshot))
                                    .is_some()
                            })
                            .unwrap_or(false);
                    if eligible {
                        style
                            .border_2()
                            .border_color(gpui::hsla(0.58, 0.45, 0.62, 0.8))
                    } else {
                        style
                    }
                })
                .on_drop::<gpui::ExternalPaths>(move |payload, window, cx| {
                    drop_paths_cache.borrow_mut().take();
                    cx.stop_propagation();
                    let Some(context) = window.file_drop_context() else {
                        return;
                    };
                    context.response.set_effect(gpui::FileDropEffect::None);
                    let Some(shell) = shell.as_ref() else {
                        return;
                    };
                    let Some(tab_id) = tab_id else {
                        return;
                    };
                    if payload.paths().is_empty() {
                        return;
                    }
                    let Some(snapshot) = drop_snapshot.as_ref() else {
                        return;
                    };
                    let Some(target) = drop_entity
                        .read_with(cx, |this, _| this.navigation_drop_target(snapshot))
                        .ok()
                        .flatten()
                    else {
                        return;
                    };
                    let paths = context
                        .paths
                        .as_ref()
                        .map(|paths| paths.paths().to_vec())
                        .unwrap_or_default();
                    if paths.is_empty() || paths != payload.paths() {
                        return;
                    }
                    let prepared = drop_entity
                        .read_with(cx, |view, _| {
                            prepared_external_drop(&view.external_drag_session, &paths, &target)
                        })
                        .ok()
                        .flatten();
                    let _ = drop_entity.update(cx, |view, _| {
                        take_external_drag_session(&view.external_drag_session);
                    });
                    if let Some(local_token) = context.local_drag_session {
                        let Some(shell) = local_ole_drop_shell.as_ref() else {
                            return;
                        };
                        let effect = shell
                            .update(cx, |shell, cx| {
                                shell.admit_local_ole_navigation_move(
                                    local_token.value(),
                                    tab_id,
                                    crumb_target_view_id,
                                    target,
                                    paths,
                                    cx,
                                )
                            })
                            .ok()
                            .flatten();
                        if let Some(effect) = effect {
                            context.response.set_effect(match effect {
                                shell::LocalOleDropEffect::Copy => gpui::FileDropEffect::Copy,
                                shell::LocalOleDropEffect::Move => gpui::FileDropEffect::Move,
                            });
                        }
                        return;
                    }
                    let intent = foreign_file_drop_intent(&context);
                    let (admitted, effect) = shell
                        .update(cx, |shell, cx| match intent {
                            input::ForeignFileDropIntent::Link if context.link_allowed => {
                                context.response.set_effect(gpui::FileDropEffect::None);
                                (
                                    shell.admit_external_symbolic_link(
                                        tab_id,
                                        crumb_target_view_id,
                                        target,
                                        paths,
                                        cx,
                                    ),
                                    gpui::FileDropEffect::Link,
                                )
                            }
                            input::ForeignFileDropIntent::Link => {
                                context.response.set_effect(gpui::FileDropEffect::None);
                                (false, gpui::FileDropEffect::None)
                            }
                            input::ForeignFileDropIntent::Move => {
                                let Some(feedback) = context
                                    .transfer_lease
                                    .as_ref()
                                    .and_then(gpui::FileDropTransferLease::start)
                                else {
                                    return (false, gpui::FileDropEffect::None);
                                };
                                (
                                    shell.admit_external_navigation_copy(
                                        tab_id,
                                        crumb_target_view_id,
                                        target,
                                        paths,
                                        false,
                                        Some(feedback),
                                        cx,
                                    ),
                                    gpui::FileDropEffect::Copy,
                                )
                            }
                            input::ForeignFileDropIntent::Default => {
                                if let Some(prepared) =
                                    prepared.filter(|prepared| prepared.same_volume)
                                    && let Some(feedback) = context
                                        .transfer_lease
                                        .as_ref()
                                        .and_then(gpui::FileDropTransferLease::start)
                                {
                                    (
                                        shell.admit_external_navigation_prepared_move(
                                            tab_id,
                                            crumb_target_view_id,
                                            target,
                                            paths,
                                            prepared,
                                            feedback,
                                            cx,
                                        ),
                                        gpui::FileDropEffect::Copy,
                                    )
                                } else {
                                    (
                                        shell.admit_external_navigation_copy(
                                            tab_id,
                                            crumb_target_view_id,
                                            target,
                                            paths,
                                            false,
                                            None,
                                            cx,
                                        ),
                                        gpui::FileDropEffect::Copy,
                                    )
                                }
                            }
                            input::ForeignFileDropIntent::Copy => (
                                shell.admit_external_navigation_copy(
                                    tab_id,
                                    crumb_target_view_id,
                                    target,
                                    paths,
                                    false,
                                    None,
                                    cx,
                                ),
                                gpui::FileDropEffect::Copy,
                            ),
                        })
                        .unwrap_or((false, gpui::FileDropEffect::None));
                    if admitted {
                        context.response.set_effect(effect);
                    }
                })
        };
        let hovered = self.breadcrumb_hovered;
        // hover の解決はここで行う（component は状態を持てない）。スクロールバーの
        // つまみを出し入れするためだけなので、変わったときだけ通知する。
        div()
            .id("breadcrumb-hover")
            .w_full()
            .flex_shrink_0()
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if this.breadcrumb_hovered != *hovered {
                    this.breadcrumb_hovered = *hovered;
                    cx.notify();
                }
            }))
            .child(
                gpui_filer_ui::Breadcrumbs::new(
                    labels,
                    self.breadcrumb_scroll.clone(),
                    hovered,
                    cx.listener(
                        move |this, intent: &gpui_filer_ui::BreadcrumbIntent, window, cx| {
                            if this.location_generation != generation {
                                return;
                            }
                            if matches!(intent, gpui_filer_ui::BreadcrumbIntent::EditPath) {
                                this.begin_address_edit(window, cx);
                                return;
                            }
                            let (index, new_tab, open_tree_anchor) = match intent {
                                gpui_filer_ui::BreadcrumbIntent::EditPath => return,
                                gpui_filer_ui::BreadcrumbIntent::Activate { index, new_tab } => {
                                    (*index, *new_tab, None)
                                }
                                gpui_filer_ui::BreadcrumbIntent::OpenFolderTree {
                                    index,
                                    anchor,
                                } => (*index, false, Some(*anchor)),
                            };
                            let Some(crumb) = crumbs.get(index).cloned() else {
                                return;
                            };
                            this.touch_item_tap_series = None;
                            if let Some(anchor) = open_tree_anchor {
                                if let Crumb::Path(path) = crumb {
                                    cx.emit(DirectoryEvent::OpenFolderTree {
                                        path,
                                        anchor,
                                        kind: FolderTreeAnchorKind::Breadcrumb,
                                    });
                                }
                                return;
                            }
                            this.last_click_was_blank = false;
                            this.focus_handle.focus(window, cx);
                            match crumb {
                                Crumb::Pc => {
                                    if new_tab {
                                        cx.emit(DirectoryEvent::OpenTab(Location::ThisPc));
                                    } else {
                                        this.navigate_to_pc(cx);
                                    }
                                }
                                crumb @ (Crumb::ShellRoot(_) | Crumb::ShellItem(_)) => {
                                    let destination = match crumb {
                                        Crumb::ShellRoot(root) => Location::ShellRoot(root),
                                        Crumb::ShellItem(location) => location,
                                        _ => unreachable!(),
                                    };
                                    if new_tab {
                                        cx.emit(DirectoryEvent::OpenTab(destination));
                                    } else {
                                        this.navigate_location(destination, cx);
                                    }
                                }
                                Crumb::Path(path) => {
                                    if new_tab {
                                        cx.emit(DirectoryEvent::OpenTab(Location::Path(path)));
                                    } else {
                                        this.navigate(path, cx);
                                    }
                                }
                                Crumb::Archive(destination) => {
                                    if new_tab {
                                        cx.emit(DirectoryEvent::OpenTab(destination));
                                    } else {
                                        this.navigate_location(destination, cx);
                                    }
                                }
                            }
                        },
                    ),
                )
                .with_drop_target_decorator(crumb_decorator),
            )
    }

    /// 列ヘッダのクリックで並べ替えを切り替える。
    ///
    /// v5 は並べ替えのたびにディレクトリを読み直していたが、`Entry` が raw fact を
    /// 持っているので手元の並びを入れ替えるだけで足りる。再列挙はしない。
    ///
    /// 選択とフォーカスは index ではなく key で持ち越す。index のまま並べ替えると、
    /// 選択が別の項目を指してしまう。
    fn toggle_sort(&mut self, column: Column, cx: &mut Context<Self>) {
        let sort = self.sort.toggled(column);
        if sort == self.sort {
            return;
        }
        // 並べ替えは rename 開始時の projection identity を無効にする。revision を
        // 進める前に pin を解放し、完了通知が古い並びへ補正を掛けないようにする。
        self.release_pending_rename_geometry(cx);
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        let snapshot = self.selection.snapshot(&keys);
        self.sort = sort;
        sorting::sort_entries(&mut self.entries, sort);
        self.apply_group_order();
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        self.selection.restore_remapped(snapshot, &keys);
        // 並びが変わったので、index を捕捉した古い gesture を無効にする。
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.emit_info_projection(cx);
        // 並びが変わると行の高さの対応も変わる。測定を捨てて先頭から測り直す。
        self.layout_width = 0.0;
        self.measured = vec![false; self.heights.len()];
        self.hovered_item_part = None;
        self.scroll_handle.set_offset(gpui::point(px(0.0), px(0.0)));
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(folder_view_preferences::ChangedField::Sort, cx);
        cx.notify();
    }

    fn cycle_grouping(&mut self, cx: &mut Context<Self>) {
        self.release_pending_rename_geometry(cx);
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        let snapshot = self.selection.snapshot(&keys);
        self.group_by = match self.group_by {
            GroupBy::None => GroupBy::ModifiedDate,
            GroupBy::ModifiedDate => GroupBy::Kind,
            GroupBy::Kind => GroupBy::None,
        };
        // grouping を外すときも、元の列 sort へ安定して戻す。
        sorting::sort_entries(&mut self.entries, self.sort);
        self.apply_group_order();
        let keys: Vec<_> = self.entries.iter().map(|entry| entry.key.clone()).collect();
        self.selection.restore_remapped(snapshot, &keys);
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.emit_info_projection(cx);
        self.layout_width = 0.0;
        self.hovered_item_part = None;
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(folder_view_preferences::ChangedField::Group, cx);
        cx.notify();
    }

    /// 今の一覧に出ている項目の表示用画像と、パンくずの表示名を作り直す。
    ///
    /// F5 と overflow で使う。列挙は size / mtime が同じ差替えを拾えないので、
    /// 明示的な再読込では画像も作り直す。大きな directory では世代の記録が上限に
    /// 達して全体 invalidation へ畳まれるが、正しさは変わらない。
    fn invalidate_known_media(&mut self) {
        self.shell_icon_generation = self.shell_icon_generation.wrapping_add(1);
        self.shell_icon_identity_generation = self.shell_icon_identity_generation.wrapping_add(1);
        let mut paths: Vec<std::path::PathBuf> = self
            .entries
            .iter()
            .filter_map(|entry| entry.path.clone())
            .collect();
        // 空のフォルダでも、そのフォルダ自身の見本は古くなる。PC は path を持たないので
        // 何も足さない（サムネイルの対象が無い）。
        if let Some(location) = self
            .location()
            .and_then(|location| location.path().map(std::path::Path::to_path_buf))
        {
            paths.push(location);
        }
        thumbnails::invalidate(&paths);
        self.names_epoch = self.names_epoch.wrapping_add(1);
    }

    fn prepare_previews(&mut self, size: u32, window: &mut Window, cx: &mut Context<Self>) {
        // PC の行は drive や既知のフォルダで、中身の見本を作る対象ではない。
        // 切れた network drive を読みに行かないためにも、ここで止める。
        if self.layout == ViewLayout::Rows || self.is_this_pc() {
            self.previews.clear();
            return;
        }
        thumbnails::begin_frame(cx);
        // 表示側へ渡す要求サイズと同じ値でdecodeする。
        let range = self.visible_range();
        let visible: Vec<_> = range
            .clone()
            .flat_map(|row| self.row_entries(row))
            .collect();
        // 画面外に出た画像だけを手放す。範囲内はdecodeが終わるまで前の画像を出し続ける。
        let visible_paths: std::collections::HashSet<std::path::PathBuf> = visible
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .filter_map(|entry| entry.path.clone())
            .collect();
        self.previews
            .retain(|path, _| visible_paths.contains(path.as_path()));

        // 対象の項目を先に集める。`self.entries` を借りたまま `previews` を書かない。
        let targets: Vec<(std::path::PathBuf, bool)> = visible
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .filter_map(|entry| {
                let path = entry.path.clone()?;
                let folder = matches!(entry.kind, EntryKind::Folder);
                if !folder && (is_ico_file(entry) || !thumbnails::supported(&path)) {
                    return None;
                }
                Some((path, folder))
            })
            .collect();

        let mut pending = 0;
        for (path, folder) in &targets {
            let epoch = thumbnails::epoch_of(path);
            if log_watch_dirty()
                && let Some(PreviewState::Ready { epoch: stale, .. }) = self.previews.get(path)
                && stale != &epoch
            {
                eprintln!(
                    "[v6-thumb] stale projection path={} state={stale:?} now={epoch:?}",
                    path.display()
                );
            }
            match self.previews.get(path) {
                // 解像度が足りていればそのまま使う。アイコンサイズを変えた時だけ取り直す。
                Some(PreviewState::Ready { epoch: e, images })
                    if *e == epoch && preview_covers(images, size) =>
                {
                    continue;
                }
                // 生成できなかった項目は明示的な invalidation まで再試行しない。
                // 世代が進めば下の request へ落ちる。
                Some(PreviewState::Unavailable {
                    epoch: e,
                    size: failed_size,
                    folder: failed_folder,
                    ..
                }) if *e == epoch && *failed_size == size && *failed_folder == *folder => continue,
                _ => {}
            }
            match thumbnails::request(&(path.clone(), size, *folder, epoch), true, window, cx) {
                Some(output) if !output.images.is_empty() => {
                    self.previews.insert(
                        path.clone(),
                        PreviewState::Ready {
                            epoch,
                            images: output.images,
                        },
                    );
                }
                Some(output) => {
                    // 空の結果は「もう出ない」として扱う。待合せを止めない。
                    let reason = output.reason.unwrap_or(if *folder {
                        thumbnails::PreviewFailureReason::FolderNoPreview
                    } else {
                        thumbnails::PreviewFailureReason::ProcessingFailed
                    });
                    self.previews.insert(
                        path.clone(),
                        PreviewState::Unavailable {
                            epoch,
                            size,
                            folder: *folder,
                            reason,
                        },
                    );
                }
                // まだ読み込み中。次フレームでもう一度試す。
                None => pending += 1,
            }
        }
        // 描画は近傍2行だけ。画像はさらに約1画面分を先読みするが要素は作らない。
        //
        // 可視範囲の decode がまだ終わっていない間は先読みを積まない。
        // decode は本数制限付きの待ち行列を通るので、先に積んだ先読みが
        // 可視範囲を追い越してしまう。見えている1枚を先に出すほうが体感がよい。
        //
        // 待合せに数えるのは未解決の項目だけ。`Unavailable` は終端なので
        // 数えない。数えると失敗した1枚が先読みを永久に止める。
        if pending > 0 {
            // visible request は `Window::use_asset` を通る。GPUI は asset の完了時に
            // この view だけを再描画するので、ここで polling しない。毎フレーム notify
            // すると、decode 待ちの間ずっと layout と preview projection を作り直してしまう。
            return;
        }
        let range = self.visible_range();
        let count = range.len().max(1);
        let total = self.heights.len();
        let before = (range.start.saturating_sub(count)..range.start).rev();
        let after = range.end..(range.end + count).min(total);
        let prefetch: Vec<_> = if range.start < self.last_preview_row {
            before.chain(after).collect()
        } else {
            after.chain(before).collect()
        };
        self.last_preview_row = range.start;
        for row in prefetch {
            for index in self.row_entries(row) {
                let entry = &self.entries[index];
                if let Some(path) = &entry.path {
                    let folder = matches!(entry.kind, EntryKind::Folder);
                    if folder || (!is_ico_file(entry) && thumbnails::supported(path)) {
                        let epoch = thumbnails::epoch_of(path);
                        thumbnails::request(
                            &(path.clone(), size, folder, epoch),
                            false,
                            window,
                            cx,
                        );
                    }
                }
            }
        }
    }

    fn new(keymap_runtime: std::sync::Arc<input::KeymapRuntime>, cx: &mut Context<Self>) -> Self {
        let argument = std::env::args_os().nth(1);
        let directory = if demo_requested() {
            None
        } else {
            Some(
                argument
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
            )
        };
        Self::new_at(keymap_runtime, directory.map(Location::from), cx)
    }

    fn new_at(
        keymap_runtime: std::sync::Arc<input::KeymapRuntime>,
        location: Option<Location>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_view_state(keymap_runtime, location, None, None, cx)
    }

    /// 保存されていた表示状態から作る。復元の入口。
    ///
    /// 読取は最初の描画で始まるので、非 active のタブは表示されるまで列挙しない。
    fn restored(
        keymap_runtime: std::sync::Arc<input::KeymapRuntime>,
        state: &SavedViewState,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_view_state(
            keymap_runtime,
            state.location.clone(),
            Some(state),
            None,
            cx,
        )
    }

    fn restored_closed(
        keymap_runtime: std::sync::Arc<input::KeymapRuntime>,
        state: &SavedViewState,
        history: navigation_history::NavigationHistorySnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_view_state(
            keymap_runtime,
            state.location.clone(),
            Some(state),
            Some(history),
            cx,
        )
    }

    fn with_view_state(
        keymap_runtime: std::sync::Arc<input::KeymapRuntime>,
        location: Option<Location>,
        saved: Option<&SavedViewState>,
        history: Option<navigation_history::NavigationHistorySnapshot>,
        cx: &mut Context<Self>,
    ) -> Self {
        let location = location.map(|location| match location {
            Location::Path(path) => Location::Path(std::path::absolute(&path).unwrap_or(path)),
            virtual_location => virtual_location,
        });
        // PC と Libraries は path を持たない。session の typed location から virtual effect
        // を出し、Shell adapter が行を背景で解決する。
        let demo = location.is_none();
        let entries = (0..if demo { 20_000 } else { 0 })
            .map(|index| DemoEntry {
                // demo は path を持たないので、生成時の index を安定 ID にする。
                key: entry_identity::EntryKey::demo(index as u64),
                path: None,
                name: match index % 11 {
                    0 => format!("project-{index:05}"),
                    1 => format!("notes-{index:05}.md"),
                    2 => format!("image-{index:05}.png"),
                    3 => format!("プロジェクト資料-{index:05}-画面の幅に合わせて名前を折り返すための長いファイル名.md"),
                    4 => format!("project-{index:05}-a-long-file-name-without-spaces-for-wrapping-verification.txt"),
                    _ => format!("file-{index:05}.txt"),
                },
                kind: if index % 11 == 0 {
                    EntryKind::Folder
                } else {
                    EntryKind::File
                },
                source_identity: None,
                link: None,
                link_target: None,
                is_hidden: false,
                size: if index % 11 == 0 {
                    None
                } else {
                    Some((4 + index % 996) as u64 * 1024)
                },
                folder_size: None,
                modified: Some(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(
                            1_767_225_600 + (index as u64 % 28) * 86_400 + (index as u64 % 60) * 60,
                        ),
                ),
            })
            .collect::<Vec<_>>();
        let mut heights = RowHeights::new(entries.len());
        heights.set_content(0..entries.len(), ESTIMATED_ROW_HEIGHT);
        // 保存値は範囲外かもしれない。表示の制約へ丸めてから使う。
        let layout = saved.map_or(ViewLayout::Rows, |state| {
            ViewLayout::from_value(state.layout)
        });
        let icon_scale = saved.map_or(ICON_SCALE_DEFAULT, |state| {
            state.icon_scale.clamp(ICON_SCALE_MIN, ICON_SCALE_MAX)
        });
        let sort = saved.map_or(SortState::None, |state| SortState::from(state.sort));
        let group_by = saved.map_or(GroupBy::None, |state| state.group.into());
        // demo は列挙しないので、session に場所を渡さず last-good だけを直接置く。
        let has_saved_history = history.is_some();
        let mut session = history
            .map_or_else(directory_session::DirectorySession::default, |history| {
                directory_session::DirectorySession::from_navigation_snapshot(history)
            });
        if !has_saved_history && demo {
            session.adopt_demo(std::sync::Arc::new(entries.clone()));
        } else if !has_saved_history && let Some(location) = location {
            // 最初の読取は Render の初回で拾う。
            session.start_at(location);
        }
        let pending_restore = session
            .current_bookmark()
            .cloned()
            .map(|bookmark| PendingRestore {
                bookmark,
                scroll_revision: 0,
            });
        let mut view = Self {
            keymap_runtime,
            show_hidden_items_subscription: None,
            item_check_boxes_subscription: None,
            item_check_boxes: true,
            item_check_boxes_revision: 0,
            external_drop_shell: None,
            external_drop_view_id: None,
            external_file_drop_paths: Rc::new(RefCell::new(None)),
            entries,
            projected_rows: Vec::new(),
            heading_rows: Vec::new(),
            group_by,
            column_prefs: folder_view_preferences::ColumnPrefs::default(),
            row_details: Default::default(),
            heights,
            scroll_handle: ScrollHandle::new(),
            selection: Selection::default(),
            page_focus_column: None,
            selection_aggregate_revision: 0,
            selection_aggregate_work: None,
            selection_aggregate: None,
            internal_drag_snapshot: Rc::new(RefCell::new(None)),
            next_internal_drag_session_id: 0,
            tab_directory_drag_source: Rc::new(RefCell::new(None)),
            external_drag_session: Rc::new(RefCell::new(None)),
            next_external_drop_ticket: 0,
            navigation_identity_projection: NavigationIdentityProjection::default(),
            item_menu_geometry: Rc::new(RefCell::new(None)),
            layout,
            columns: 1,
            layout_width: 0.0,
            pane_width_projection: shell::DEFAULT_SIDEBAR_WIDTH,
            measured_viewport_width: None,
            viewport_reconcile_scheduled: Rc::new(Cell::new(false)),
            measured: vec![false; 20_000],
            focus_handle: cx.focus_handle(),
            last_click_was_blank: false,
            session,
            entry_targets: Default::default(),
            entry_icons: Default::default(),
            pc_shell_icon_targets: Default::default(),
            shell_icon_generation: 0,
            shell_icon_identity_generation: 0,
            shell_icon_pending: Rc::default(),
            shortcut_icon_pending: Rc::default(),
            shortcut_icon_request_ticket: Rc::new(Cell::new(0)),
            shortcut_icon_projections: Default::default(),
            shortcut_icon_projection_order: Default::default(),
            previews: Default::default(),
            names_epoch: 0,
            list_hovered: false,
            scrollbar_drag: None,
            rectangle_pointer_down: None,
            rectangle_drag: None,
            row_column_resize: None,
            row_column_reorder_down: None,
            row_column_reorder: None,
            row_column_reorder_suppress_click: false,
            breadcrumb_scroll: ScrollHandle::new(),
            reveal_breadcrumb: true,
            breadcrumb_viewport_width: None,
            breadcrumb_names_ready: false,
            breadcrumb_hovered: false,
            folder_tree_button_active: None,
            pc_loaded: false,
            pc_base_sources_complete: false,
            pc_wsl_loaded: false,
            pc_ticket: None,
            pc_rows: Vec::new(),
            library_loaded: false,
            library_error: None,
            location_generation: 0,
            hovered_item_part: None,
            last_preview_row: 0,
            frame_profile: Default::default(),
            icon_scale,
            interaction: InteractionRouter::default(),
            translator: Translator::new(Locale::Japanese),
            sort,
            presentation_revision: 0,
            watch: directory_watch::DirectoryWatch::new(cx.entity().downgrade()),
            pending_effects: std::collections::VecDeque::new(),
            #[cfg(test)]
            test_application_image_path: None,
            #[cfg(test)]
            archive_completion_test_gate: None,
            draining_effects: false,
            pending_restore,
            scroll_revision: 0,
            observed_scroll_offset: gpui::point(px(0.0), px(0.0)),
            touch_item_tap_series: None,
            pending_projection_anchor: None,
            pending_rename_anchor: None,
            pending_rename_geometry: None,
            defer_projection_anchor_until_measured: false,
            rename_session: None,
            quick_search: None,
            rename_attempt_ticket: 0,
            batch_rename_session: None,
            batch_rename_probe_ticket: 0,
            address_edit: None,
            cut_paths: std::collections::HashSet::new(),
            #[cfg(test)]
            clipboard_write_result_for_test: None,
            select_extension_on_rename: false,
        };
        sorting::sort_entries(&mut view.entries, view.sort);
        view.apply_group_order();
        view.rebuild_row_details();
        view.reset_projected_rows();
        view.show_hidden_items_subscription = Some(
            cx.observe_global::<show_hidden_items::ShowHiddenItems>(|view, cx| {
                view.apply_listing(cx)
            }),
        );
        view.item_check_boxes = cx
            .try_global::<ItemCheckBoxes>()
            .is_none_or(|setting| setting.0);
        view.item_check_boxes_subscription =
            Some(cx.observe_global::<ItemCheckBoxes>(|view, cx| {
                let item_check_boxes = cx.global::<ItemCheckBoxes>().0;
                if view.item_check_boxes != item_check_boxes {
                    view.item_check_boxes = item_check_boxes;
                    view.item_check_boxes_revision = view.item_check_boxes_revision.wrapping_add(1);
                }
                view.measured_viewport_width = None;
                let columns =
                    view.layout
                        .columns(view.layout_width, view.icon_scale, view.item_check_boxes);
                view.columns = columns;
                view.reset_projected_rows();
                view.cancel_rectangle_drag();
                cx.notify();
            }));
        view
    }

    /// binding の条件評価に使うスコープ列の共通部分。
    /// 状態スコープは有効なときだけ積み、v5 の `selection_mode` と同じ役割にする。
    fn base_scopes(&self) -> Vec<ScopeId> {
        let layout = match self.layout {
            ViewLayout::Rows => input::scope::ROWS,
            ViewLayout::Tiles => input::scope::TILES,
            ViewLayout::Icons => input::scope::ICONS,
        };
        let mut scopes = vec![input::scope::SHELL, input::scope::FILE_VIEW, layout];
        if self.selection.is_selection_mode() {
            scopes.push(input::scope::SELECTION_MODE);
        }
        if matches!(self.selection.focus_target(), Some(FocusTarget::Section(_))) {
            scopes.push(input::scope::SECTION_FOCUS);
        }
        scopes
    }

    fn section_members(&self, key: &SectionKey) -> Option<std::ops::Range<usize>> {
        let mut found = None;
        for row in &self.projected_rows {
            let range = match row {
                ProjectedRow::Heading { key: row_key, start, end, .. } if row_key == key => Some(*start..*end),
                ProjectedRow::CategoryStrip { groups, .. } => {
                    let mut matches = groups.iter().filter(|group| &group.key == key);
                    let range = matches.next().map(|group| group.index..group.index + 1);
                    if matches.next().is_some() {
                        return None;
                    }
                    range
                }
                _ => None,
            };
            if let Some(range) = range {
                if found.is_some() {
                    return None;
                }
                found = Some(range);
            }
        }
        found.filter(|range| range.start < range.end && range.end <= self.entries.len())
    }

    fn toggle_section(&mut self, key: &SectionKey, cx: &mut Context<Self>) -> bool {
        let Some(members) = self.section_members(key) else {
            self.selection.clear_section_focus();
            cx.notify();
            return true;
        };
        self.selection.focus_section(key.clone());
        self.selection.toggle_section_members(members);
        self.invalidate_pending_restore_for_selection();
        self.emit_info_projection(cx);
        cx.notify();
        true
    }

    /// 現在の window focus が、この view の text editor の中にあるか。
    ///
    /// `rename_session` / `address_edit` の存在だけを見ると、blur と次の
    /// render の間に shell の command listener が一瞬だけ復活する。実際の
    /// `FocusHandle` を確認して、focus adapter と同じ事実を使う。
    pub(crate) fn text_input_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.rename_session
            .as_ref()
            .is_some_and(|session| session.input.read(cx).is_focused(window))
            || self
                .address_edit
                .as_ref()
                .is_some_and(|session| session.input.read(cx).is_focused(window))
            || self
                .quick_search
                .as_ref()
                .is_some_and(|session| session.input.read(cx).is_focused(window))
            || self.batch_rename_session.as_ref().is_some_and(|session| {
                session.pattern.read(cx).is_focused(window)
                    || session.find.read(cx).is_focused(window)
                    || session.replace.read(cx).is_focused(window)
            })
    }

    /// focus 位置のスコープ列。
    fn focus_path(&self) -> InteractionPath {
        InteractionPath::new(self.base_scopes()).expect("focus path is never empty")
    }

    /// 項目のスコープ列。クリックは hover ではなく、イベントが起きた要素のパスで
    /// 条件を評価する。タッチには hover が無いため。
    fn item_path(&self) -> InteractionPath {
        let mut scopes = self.base_scopes();
        scopes.push(input::scope::FILE_ITEM);
        InteractionPath::new(scopes).expect("item path is never empty")
    }

    /// hover 位置のスコープ列。項目の上では `FILE_ITEM` を足す。
    /// 余白やcheckboxの上では `hovered_item_part` が None なので、項目の条件に含まれない。
    ///
    /// タッチのあとは配信しない。タッチは離したあとも最後の接触位置が hover として残るため、
    /// そのままでは意味のない位置へキーを渡してしまう。**キーボードのあとは配信する**。
    /// 止めると `file_item` を条件にした binding が永久に当たらない（表示だけを止める）。
    fn hover_path(&self) -> Option<InteractionPath> {
        if !self.interaction.last_device().keeps_hover_routing() {
            return None;
        }
        self.hovered_item_part?;
        Some(self.item_path())
    }

    /// キーを binding で解決して実行する。扱えたときだけ true を返す。
    fn handle_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Shell must cancel the native handoff even when this view consumes Escape itself.
        if keystroke.key == "escape" {
            if self.row_column_resize.is_some()
                || self.row_column_reorder_down.is_some()
                || self.row_column_reorder.is_some()
            {
                if self.row_column_reorder.is_some() {
                    cx.stop_active_drag(window);
                }
                self.cancel_row_column_resize();
                self.cancel_row_column_reorder();
                cx.notify();
                return true;
            }
            let source_view_id = cx.entity().entity_id();
            cx.emit(DirectoryEvent::CancelNativeDrag {
                source_view_id,
                session_id: None,
            });
        }
        if keystroke.key == "escape" && self.cancel_rectangle_drag() {
            cx.notify();
            return true;
        }
        if self.address_edit.is_some() {
            return self.handle_address_keystroke(keystroke, window, cx);
        }
        if self.batch_rename_session.is_some() {
            if keystroke.key == "enter" {
                self.submit_batch_rename_dialog(window, cx);
                return true;
            }
            if keystroke.key == "escape" {
                self.batch_rename_session = None;
                self.focus_handle.focus(window, cx);
                cx.notify();
                return true;
            }
            return false;
        }
        if self.rename_session.is_some() {
            return self.handle_rename_keystroke(keystroke, window, cx);
        }
        if let Some(session) = self
            .quick_search
            .as_ref()
            .filter(|session| session.input.read(cx).is_focused(window))
        {
            // Composition confirmation/cancellation belongs to RenameInput first.
            if session.input.read(cx).is_composing()
                && matches!(keystroke.key.as_str(), "enter" | "escape")
            {
                return false;
            }
            match keystroke.key.as_str() {
                "enter" => {
                    let query = session.input.read(cx).text().to_owned();
                    self.apply_quick_search_query(&query, cx);
                    if let Some(session) = self.quick_search.as_ref() {
                        session.input.update(cx, |input, cx| input.focus(window, cx));
                    }
                    return true;
                }
                "escape" => {
                    self.quick_search = None;
                    self.focus_handle.focus(window, cx);
                    cx.notify();
                    return true;
                }
                "/" => {
                    if let Some(session) = self.quick_search.as_ref() {
                        session.input.update(cx, |input, cx| {
                            input.select_all(cx);
                            input.focus(window, cx);
                        });
                    }
                    return true;
                }
                _ => return false,
            }
        }
        // 項目の動作を hover 中の項目へ当てられるよう、捨てずに読む。
        let hovered = self.hovered_item_part.map(|(index, _)| index);
        // `hovered_item_part` は消さない。ポインタはまだその項目の上にあり、
        // hover を条件にした binding の配送先になる。ハイライトだけを
        // `hover_enabled` で消す。
        self.interaction.set_last_device(InputDevice::Keyboard);
        let chord = input::chord(keystroke);
        let bindings = self.keymap_runtime.snapshot();
        let Some(resolved) = self.interaction.resolve_key(
            bindings.file_view.as_ref(),
            &chord,
            if matches!(self.selection.focus_target(), Some(FocusTarget::Section(_))) {
                InputPolicy::FocusedOnly
            } else {
                InputPolicy::FocusedThenHovered
            },
        ) else {
            return false;
        };
        if resolved.target.route == InputRoute::Focus
            && resolved.action.as_str() == input::PICK_TOGGLE
            && let Some(FocusTarget::Section(key)) = self.selection.focus_target().cloned()
        {
            return self.toggle_section(&key, cx);
        }
        if resolved.target.route == InputRoute::Focus
            && matches!(self.selection.focus_target(), Some(FocusTarget::Section(_)))
            && [
                input::ACTIVATE,
                input::ACTIVATE_NEW_TAB,
                input::QUICK_PREVIEW,
                input::RENAME,
                input::DELETE_TO_TRASH,
            ]
            .contains(&resolved.action.as_str())
        {
            return true;
        }
        // 項目に対する動作は、解決した経路の項目へ当てる。`file_item` を条件に書いた
        // binding なら hover している項目、そうでなければ focus している項目になる。
        let mut item = key_target_index(resolved.target.route, hovered, self.selection.focused());
        // v5 の既定 activation は未フォーカス時に表示一覧の先頭を開いていた。
        // activation の Focus 経路だけで再現し、hover と他の command の対象は変えない。
        if !matches!(self.selection.focus_target(), Some(FocusTarget::Section(_)))
            && resolved.target.route == InputRoute::Focus
            && matches!(resolved.action.as_str(), input::ACTIVATE | input::ACTIVATE_NEW_TAB)
        {
            item = item.or_else(|| (!self.entries.is_empty()).then_some(first_item_index()));
        }
        let Some(intent) =
            commands::resolve(resolved.action, &self.command_context(), item.is_some())
        else {
            // 今の状態では実行できない。当たったことを消費にしない。
            return false;
        };
        self.run_intent(intent, item, window, cx)
    }

    fn handle_rename_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // IME candidate confirmation/cancellation belongs to the input handler
        // first. Treating its Enter/Escape as a rename decision would submit or
        // discard a still-marked composition.
        if self
            .rename_session
            .as_ref()
            .is_some_and(|session| session.input.read(cx).is_composing())
        {
            return false;
        }
        match keystroke.key.as_str() {
            "enter" => self.commit_rename(window, cx),
            "escape" => {
                self.close_rename_editor(false);
                self.focus_handle.focus(window, cx);
                cx.notify();
            }
            // Editor actions and Text/IME events must continue to the focused
            // EntityInputHandler. Handling edit keys here as well made a key
            // travel through both the root and the input entity.
            _ => return false,
        }
        true
    }

    /// menu から来たコマンドを実行する。
    ///
    /// 開いた時の状態が変わっていたら実行しない。別のタブ・別の場所へ黙って配送しないため。
    /// 項目は index ではなく key で引き直す。並びが変わっていても同じ項目に当たる。
    fn run_menu_command(
        &mut self,
        action: ActionId,
        target: &menu::MenuTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.location_generation != target.generation
            || (target.item.is_some() && self.presentation_revision != target.presentation_revision)
        {
            return false;
        }

        if action == ActionId::new(input::PASTE) {
            if target.item.is_some() {
                return false;
            }
            let Some(destination) = target.background_destination.as_ref() else {
                return false;
            };
            if !self.accepts_paste_target(
                destination,
                target.generation,
                target.presentation_revision,
            ) {
                return false;
            }
            let Some(destination_identity) = self.session.directory_identity().cloned() else {
                return false;
            };
            cx.emit(DirectoryEvent::PasteInto {
                destination: destination.clone(),
                destination_identity,
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            self.cut_paths.clear();
            cx.notify();
            return true;
        }

        if action == ActionId::new(input::CREATE_FOLDER) {
            if target.item.is_some() {
                return false;
            }
            let (Some(destination), Some(destination_identity)) = (
                target.background_destination.as_ref(),
                target.background_destination_identity.as_ref(),
            ) else {
                return false;
            };
            if !self.accepts_background_paste_target(
                destination,
                destination_identity,
                target.generation,
                target.presentation_revision,
            ) {
                return false;
            }
            cx.emit(DirectoryEvent::CreateFolderInto {
                destination: destination.clone(),
                destination_identity: destination_identity.clone(),
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            return true;
        }
        if action == ActionId::new(input::CREATE_FILE) {
            if target.item.is_some() {
                return false;
            }
            let (Some(destination), Some(destination_identity)) = (
                target.background_destination.as_ref(),
                target.background_destination_identity.as_ref(),
            ) else {
                return false;
            };
            if !self.accepts_background_paste_target(
                destination,
                destination_identity,
                target.generation,
                target.presentation_revision,
            ) {
                return false;
            }
            cx.emit(DirectoryEvent::CreateFileInto {
                destination: destination.clone(),
                destination_identity: destination_identity.clone(),
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            cx.notify();
            return true;
        }

        if action == ActionId::new(input::COPY) || action == ActionId::new(input::CUT) {
            let Some(clicked) = target.item.as_ref() else {
                return false;
            };
            let keys = if target.selection_derived {
                if !target.selected_keys.contains(clicked)
                    || self.selected_entry_keys_in_display_order() != target.selected_keys
                {
                    return false;
                }
                target.context_keys.as_slice()
            } else {
                if target.context_keys.as_slice() != [clicked.clone()] {
                    return false;
                }
                target.context_keys.as_slice()
            };
            if action == ActionId::new(input::COPY) {
                match self.clipboard_copy_sources_for_keys(keys) {
                    Some(ClipboardCopySources::Paths(paths)) => {
                        cx.emit(DirectoryEvent::ClipboardWrite {
                            paths,
                            effect: operations::file_clipboard::ClipboardEffect::Copy,
                            location_generation: target.generation,
                            presentation_revision: target.presentation_revision,
                        })
                    }
                    Some(ClipboardCopySources::Namespace(request)) => {
                        cx.emit(DirectoryEvent::NamespaceClipboardWrite {
                            parent: request.parent,
                            identities: request.identities,
                            location_generation: target.generation,
                            presentation_revision: target.presentation_revision,
                        })
                    }
                    None => return false,
                }
            } else {
                let Some(paths) = self.clipboard_sources_for_keys(keys) else {
                    return false;
                };
                cx.emit(DirectoryEvent::ClipboardWrite {
                    paths,
                    effect: operations::file_clipboard::ClipboardEffect::Move,
                    location_generation: target.generation,
                    presentation_revision: target.presentation_revision,
                });
            }
            return true;
        }
        if action == ActionId::new(input::PASTE_HERE) {
            let Some(target_folder) = target.paste_target.clone() else {
                return false;
            };
            if target.item.as_ref() != Some(&target_folder.key)
                || !self.accepts_internal_drag_target(&target_folder)
            {
                return false;
            }
            cx.emit(DirectoryEvent::PasteHere {
                target: target_folder,
            });
            return true;
        }
        if action == ActionId::new(input::PASTE_LINK)
            && target.item.is_some()
            && target.clipboard_has_files
        {
            let Some(target_folder) = target.paste_target.clone() else {
                return false;
            };
            if target.item.as_ref() != Some(&target_folder.key)
                || !self.accepts_internal_drag_target(&target_folder)
            {
                return false;
            }
            cx.emit(DirectoryEvent::CreateSymbolicLinkHere {
                target: target_folder,
            });
            return true;
        }
        let item = target.item.as_ref();
        let index = match item {
            Some(key) => {
                match menu::resolve_item_index(self.entries.iter().map(|entry| &entry.key), key) {
                    Some(index) => Some(index),
                    // 消えた項目。動かす先がない。
                    None => return false,
                }
            }
            None => None,
        };
        if action.as_str() == input::BATCH_RENAME {
            let selection_keys = target.selected_keys.iter().cloned().collect::<HashSet<_>>();
            if target.selection_derived {
                if target.context_keys.as_slice() != target.selected_keys.as_slice()
                    || item.is_none_or(|clicked| !selection_keys.contains(clicked))
                    || !menu::selection_matches(&selection_keys, &self.selected_entry_keys())
                {
                    return false;
                }
            } else if target.context_keys.as_slice()
                != item.into_iter().cloned().collect::<Vec<_>>()
            {
                return false;
            }
            let mut context = self.command_context();
            context.rename_targets = target.context_keys.len();
            if commands::resolve(action, &context, index.is_some()) != Some(AppIntent::BatchRename)
            {
                return false;
            }
            return self.begin_batch_rename_for_keys(
                target.context_keys.clone(),
                target.generation,
                target.presentation_revision,
                window,
                cx,
            );
        }
        let captured_selection = target.selected_keys.iter().cloned().collect::<HashSet<_>>();
        let clicked_was_selected = matches!(action.as_str(), input::COPY_PATHS | input::COPY_NAMES)
            && item.is_some_and(|key| captured_selection.contains(key));
        let selection_identity_required = target.selection_derived
            || clicked_was_selected
            || (action.as_str() != input::RENAME && commands::uses_selection_identity(action));
        if selection_identity_required
            && !menu::selection_matches(&captured_selection, &self.selected_entry_keys())
        {
            return false;
        }
        if action.as_str() == input::RENAME {
            let [key] = target.context_keys.as_slice() else {
                return false;
            };
            if target.selection_derived || target.item.as_ref() != Some(key) {
                return false;
            }
            let Some(entry) = self.entries.iter().find(|entry| &entry.key == key) else {
                return false;
            };
            if !target.rename_eligible
                || entry.path.is_none()
                || entry.link == Some(LinkKind::OtherReparsePoint)
            {
                return false;
            }
            let mut context = self.command_context();
            context.rename_targets = 1;
            let Some(AppIntent::Rename) = commands::resolve(action, &context, true) else {
                return false;
            };
            let Some(source) = entry.path.clone() else {
                return false;
            };
            self.begin_rename(source, true, window, cx);
            return true;
        }
        if action.as_str() == input::DELETE_TO_TRASH {
            let Some(clicked) = target.item.as_ref() else {
                return false;
            };
            if target.selection_derived {
                if target.context_keys != target.selected_keys
                    || !target.selected_keys.iter().any(|key| key == clicked)
                    || self.selected_entry_keys_in_display_order() != target.selected_keys
                {
                    return false;
                }
            } else if target.context_keys.as_slice() != [clicked.clone()] {
                return false;
            }
            let Some(sources) = self.trash_sources_for_keys(&target.context_keys) else {
                return false;
            };
            cx.emit(DirectoryEvent::Trash {
                paths: sources.iter().map(|(path, _, _)| path.clone()).collect(),
                source_identities: sources
                    .iter()
                    .map(|(_, identity, _)| Some(identity.clone()))
                    .collect(),
                reparse_candidates: sources.into_iter().map(|(_, _, candidate)| candidate).collect(),
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            return true;
        }
        let Some(intent) = commands::resolve(action, &self.command_context(), index.is_some())
        else {
            return false;
        };
        if matches!(intent, AppIntent::CopyPaths) {
            let Some(paths) = self.copy_paths_for_menu_target(target) else {
                return false;
            };
            cx.emit(DirectoryEvent::CopyPlainText {
                text: paths
                    .iter()
                    .map(|path| path.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
            return true;
        }
        if matches!(intent, AppIntent::CopyNames) {
            let names = target
                .context_keys
                .iter()
                .map(|key| {
                    self.entries
                        .iter()
                        .find(|entry| &entry.key == key)
                        .map(|entry| entry.name.clone())
                })
                .collect::<Option<Vec<_>>>();
            let Some(names) = names.filter(|names| !names.is_empty()) else {
                return false;
            };
            cx.emit(DirectoryEvent::CopyPlainText {
                text: names.join("\n"),
            });
            return true;
        }
        self.run_intent(intent, index, window, cx)
    }

    /// menu の captured scope 全体が現在も同じ path-backed row 群ならその path 群を返す。
    fn properties_paths_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<Vec<std::path::PathBuf>> {
        if target
            .context_keys
            .iter()
            .any(|key| !matches!(key, entry_identity::EntryKey::Path { .. }))
        {
            return None;
        }
        self.copy_paths_for_menu_target(target)
    }

    /// キー操作時点の selection を表示順で使い、空なら focus に戻す。
    fn properties_paths_for_live_scope(&self) -> Option<Vec<std::path::PathBuf>> {
        let keys = if self.selection.len() > 0 {
            self.selected_entry_keys_in_display_order()
        } else {
            vec![self
                .selection
                .focused()
                .and_then(|index| self.entries.get(index))?
                .key
                .clone()]
        };
        if keys.is_empty()
            || keys
                .iter()
                .any(|key| !matches!(key, entry_identity::EntryKey::Path { .. }))
        {
            return None;
        }
        keys.iter()
            .map(|key| {
                self.entries
                    .iter()
                    .find(|entry| &entry.key == key)?
                    .path
                    .clone()
            })
            .collect()
    }

    /// menu が開いている間、key を一覧へ届けない。
    ///
    /// modal の外の候補は router が落とすので、一覧の binding は当たらない。
    /// Shell が選んだ top surface だけを view の modal path へ投影する。
    pub(crate) fn set_overlay_modal(&mut self, kind: Option<overlays::OverlayKind>) {
        let modal = kind.map(|kind| {
            let scope = match kind {
                overlays::OverlayKind::Conflict | overlays::OverlayKind::Search => {
                    input::scope::DIALOG
                }
                overlays::OverlayKind::Menu => input::scope::MENU,
                overlays::OverlayKind::FolderTree => input::scope::FOLDER_TREE,
            };
            InteractionPath::new([input::scope::SHELL, scope]).expect("overlay path")
        });
        self.interaction.set_modal(modal);
    }

    /// menu を開いた時に捕捉する世代。navigation で進む。
    fn generation(&self) -> u64 {
        self.location_generation
    }

    pub(crate) fn batch_rename_keys_eligible(
        &self,
        keys: &[entry_identity::EntryKey],
        generation: u64,
        presentation_revision: u64,
    ) -> bool {
        generation == self.location_generation
            && presentation_revision == self.presentation_revision
            && !keys.is_empty()
            && self.session.directory_identity().is_some()
            && self.session.accepted_scan_ticket().is_some()
            && keys.iter().collect::<HashSet<_>>().len() == keys.len()
            && keys.iter().all(|key| {
                self.entries.iter().any(|entry| {
                    &entry.key == key
                        && entry.path.is_some()
                        && entry.source_identity.is_some()
                        && entry.link != Some(directory::LinkKind::OtherReparsePoint)
                })
            })
    }

    pub(crate) fn clipboard_sources_eligible_keys(
        &self,
        keys: &[entry_identity::EntryKey],
        generation: u64,
        presentation_revision: u64,
    ) -> bool {
        generation == self.location_generation
            && presentation_revision == self.presentation_revision
            && self.clipboard_sources_for_keys(keys).is_some()
    }

    pub(crate) fn trash_sources_eligible_keys(
        &self,
        keys: &[entry_identity::EntryKey],
        generation: u64,
        presentation_revision: u64,
    ) -> bool {
        generation == self.location_generation
            && presentation_revision == self.presentation_revision
            && self.session.directory_identity().is_some()
            && self.session.accepted_scan_ticket().is_some()
            && self.trash_sources_for_keys(keys).is_some()
    }

    fn trash_sources_for_keys(
        &self,
        keys: &[entry_identity::EntryKey],
    ) -> Option<Vec<(std::path::PathBuf, directory::EntrySourceIdentity, bool)>> {
        if keys.is_empty() || keys.iter().collect::<HashSet<_>>().len() != keys.len() {
            return None;
        }
        keys.iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                if !matches!(entry.kind, EntryKind::File | EntryKind::Folder) {
                    return None;
                }
                let reparse_candidate = match entry.link {
                    None => false,
                    Some(LinkKind::LinkOrJunction) => true,
                    Some(LinkKind::OtherReparsePoint) => return None,
                };
                Some((
                    entry.path.clone()?,
                    entry.source_identity.clone()?,
                    reparse_candidate,
                ))
            })
            .collect()
    }

    pub(crate) fn clipboard_copy_eligible_keys(
        &self,
        keys: &[entry_identity::EntryKey],
        generation: u64,
        presentation_revision: u64,
    ) -> bool {
        generation == self.location_generation
            && presentation_revision == self.presentation_revision
            && self.clipboard_copy_sources_for_keys(keys).is_some()
    }

    fn clipboard_copy_sources_for_keys(
        &self,
        keys: &[entry_identity::EntryKey],
    ) -> Option<ClipboardCopySources> {
        if let Some(paths) = self.clipboard_sources_for_keys(keys) {
            return Some(ClipboardCopySources::Paths(paths));
        }
        if keys.is_empty() || keys.iter().collect::<HashSet<_>>().len() != keys.len() {
            return None;
        }
        let parent = self.session.location()?.clone();
        let candidates = keys
            .iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                Some(if entry.path.is_some() {
                    namespace_clipboard::CopyCandidate::PathBacked
                } else {
                    match key {
                        entry_identity::EntryKey::Virtual(identity) => {
                            namespace_clipboard::CopyCandidate::Virtual(identity.clone())
                        }
                        _ => namespace_clipboard::CopyCandidate::Other,
                    }
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let identities = namespace_clipboard::namespace_copy_selection(&parent, &candidates, true)?;
        Some(ClipboardCopySources::Namespace(
            namespace_clipboard::NamespaceCopy { parent, identities },
        ))
    }

    fn clipboard_copy_sources_for_live_selection_or_focus(&self) -> Option<ClipboardCopySources> {
        let keys = if self.selection.len() > 0 {
            self.selected_entry_keys_in_display_order()
        } else {
            vec![
                self.selection
                    .focused()
                    .and_then(|index| self.entries.get(index))?
                    .key
                    .clone(),
            ]
        };
        self.clipboard_copy_sources_for_keys(&keys)
    }

    fn item_menu_request_for_key(
        &self,
        clicked: Option<entry_identity::EntryKey>,
    ) -> menu::ItemMenuRequest {
        // 右クリック時点の selection と表示順を view で確定し、Shell へ渡す。
        // 後から Shell が active selection を読み直すと、別の項目へ誤配送し得る。
        let selected_keys = self.selected_entry_keys_in_display_order();
        let clicked_is_selected = clicked
            .as_ref()
            .is_some_and(|key| selected_keys.iter().any(|selected| selected == key));
        let selection_derived = clicked_is_selected
            && (self.selection.is_multiple() || self.selection.is_selection_mode());
        let context_keys = if selection_derived {
            selected_keys.clone()
        } else {
            clicked.iter().cloned().collect()
        };
        let rename_eligible = context_keys.len() == 1
            && self.entries.iter().any(|entry| {
                entry.key == context_keys[0]
                    && entry.path.is_some()
                    && match entry.link {
                        None => true,
                        Some(LinkKind::LinkOrJunction) => cfg!(target_os = "windows"),
                        Some(LinkKind::OtherReparsePoint) => false,
                    }
            });
        let paste_target = clicked.as_ref().and_then(|key| {
            self.entries
                .iter()
                .find(|entry| &entry.key == key)
                .and_then(|entry| {
                    internal_drop_target_for_entry(
                        entry,
                        self.location_generation,
                        self.presentation_revision,
                    )
                })
        });
        let background_destination = if clicked.is_none() {
            self.session
                .location()
                .and_then(Location::path)
                .map(std::path::Path::to_path_buf)
        } else {
            None
        };
        let background_destination_identity = if clicked.is_none() {
            self.session.directory_identity().cloned()
        } else {
            None
        };
        let touch_target = self.interaction.last_device() == InputDevice::Touch;
        menu::ItemMenuRequest {
            clicked,
            selected_keys,
            context_keys,
            selection_derived,
            rename_eligible,
            paste_target,
            background_destination,
            background_destination_identity,
            touch_target,
            generation: self.location_generation,
            presentation_revision: self.presentation_revision,
        }
    }

    #[cfg(test)]
    pub(crate) fn item_menu_request_for_key_for_test(
        &self,
        clicked: entry_identity::EntryKey,
    ) -> menu::ItemMenuRequest {
        self.item_menu_request_for_key(Some(clicked))
    }

    /// Whether the captured clicked item currently activates as a navigable location.
    /// Share the typed activation resolver so the popup cannot offer new-tab open for
    /// a target that normal activation sends to the default application.
    pub(crate) fn item_menu_target_navigates(
        &self,
        target: &menu::MenuTarget,
        seven_zip_read_available: bool,
    ) -> bool {
        if self.location_generation != target.generation
            || self.presentation_revision != target.presentation_revision
        {
            return false;
        }
        let Some(key) = target.item.as_ref() else {
            return false;
        };
        let Some(entry) = self.entries.iter().find(|entry| &entry.key == key) else {
            return false;
        };
        matches!(
            resolve_activation_target(
                entry,
                self.entry_targets.get(key).cloned(),
                seven_zip_read_available,
            ),
            Some(ActivationTarget::Navigate(_))
        )
    }

    /// Return every filesystem path in the captured item-popup scope, or reject the whole
    /// command when any captured member is stale or pathless.
    pub(crate) fn copy_paths_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<Vec<std::path::PathBuf>> {
        if self.location_generation != target.generation
            || self.presentation_revision != target.presentation_revision
        {
            return None;
        }
        let clicked = target.item.as_ref()?;
        if target.context_keys.is_empty() || !target.context_keys.contains(clicked) {
            return None;
        }
        if target.selection_derived {
            let captured = target.selected_keys.iter().cloned().collect::<HashSet<_>>();
            if target.context_keys != target.selected_keys
                || !captured.contains(clicked)
                || !menu::selection_matches(&captured, &self.selected_entry_keys())
            {
                return None;
            }
        } else if target.context_keys.as_slice() != [clicked.clone()] {
            return None;
        }

        target
            .context_keys
            .iter()
            .map(|key| {
                self.entries
                    .iter()
                    .find(|entry| &entry.key == key)?
                    .path
                    .clone()
            })
            .collect::<Option<Vec<_>>>()
            .filter(|paths| !paths.is_empty())
    }

    /// Resolve only the current filesystem directory captured by a background popup.
    pub(crate) fn current_path_for_background_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<String> {
        if target.item.is_some() || !target.context_keys.is_empty() {
            return None;
        }
        let destination = target.background_destination.as_ref()?;
        let identity = target.background_destination_identity.as_ref()?;
        self.accepts_background_paste_target(
            destination,
            identity,
            target.generation,
            target.presentation_revision,
        )
        .then(|| destination.to_string_lossy().into_owned())
    }

    /// Build the operation from the immutable popup target only. This same
    /// predicate is used by menu projection and by Shell dispatch.
    pub(crate) fn zip_create_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<operations::model::OperationRequest> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        let clicked = target.item.as_ref()?;
        if !target.context_keys.contains(clicked) {
            return None;
        }
        let parent_identity = self.session.directory_identity()?.clone();
        let current_selection = self.selected_entry_keys();
        if !zip_selection_order_is_current(
            target.selection_derived,
            &target.selected_keys,
            &self.selected_entry_keys_in_display_order(),
        ) {
            return None;
        }
        // 大量選択は entry ごとの探索を始める前に上限で拒否する。
        if !zip_create_context_is_eligible(
            Some(clicked),
            &target.context_keys,
            target.selection_derived,
            &target.selected_keys,
            &current_selection,
            true,
        ) {
            return None;
        }
        let captured = target
            .context_keys
            .iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                let source = entry.path.as_ref()?;
                let identity = entry.source_identity.as_ref()?;
                if !zip_create_entry_is_eligible(
                    matches!(key, entry_identity::EntryKey::Path { .. }),
                    matches!(entry.kind, EntryKind::File | EntryKind::Folder),
                    entry.link.is_none(),
                    identity.kind == entry.kind,
                    self.session.directory_identity().is_some(),
                ) || source.parent()? != self.session.location()?.path()?
                {
                    return None;
                }
                Some((source.clone(), entry.kind, identity.filesystem_identity()))
            })
            .collect::<Option<Vec<_>>>()?;
        let anchor = self
            .entries
            .iter()
            .find(|entry| &entry.key == clicked)?
            .path
            .clone()?;
        create_zip_request_for_captured_entries(
            &anchor,
            captured,
            parent_identity,
            target.tab as u64,
            target.generation,
            target.presentation_revision,
        )
    }

    pub(crate) fn shortcut_create_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<operations::model::OperationRequest> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        let clicked = target.item.as_ref()?;
        if target.context_keys.as_slice() != [clicked.clone()] {
            return None;
        }
        if self.selected_entry_keys_in_display_order() != target.selected_keys {
            return None;
        }
        if target.selection_derived {
            if target.selected_keys.as_slice() != [clicked.clone()] {
                return None;
            }
        }
        let parent_path = self.session.location()?.path()?;
        let parent_identity = self.session.directory_identity()?.clone();
        let entry = self.entries.iter().find(|entry| &entry.key == clicked)?;
        if !matches!(clicked, entry_identity::EntryKey::Path { .. })
            || !matches!(entry.kind, EntryKind::File | EntryKind::Folder)
            || entry.link.is_some()
            || entry.source_identity.as_ref()?.kind != entry.kind
        {
            return None;
        }
        let source = entry.path.as_ref()?;
        if source.parent()? != parent_path {
            return None;
        }
        let mut request = operations::model::OperationRequest::create_shortcut(
            source.clone(),
            parent_path.to_path_buf(),
            operations::model::ShortcutCreateGuard {
                source_kind: entry.kind,
                source_identity: entry.source_identity.as_ref()?.filesystem_identity(),
                parent_identity,
            },
        )
        .ok()?;
        request.origin = Some(operations::model::OperationOrigin {
            view_id: target.tab as u64,
            location_epoch: target.generation,
            presentation_revision: target.presentation_revision,
        });
        Some(request)
    }

    pub(crate) fn seven_zip_create_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
        provider: &archive_provider::ArchiveProviderSnapshot,
    ) -> Option<operations::model::OperationRequest> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        let hint = target.seven_zip_creation.as_ref()?;
        if !hint.matches_live(provider) {
            return None;
        }
        let clicked = target.item.as_ref()?;
        if !target.context_keys.contains(clicked) {
            return None;
        }
        let parent_identity = self.session.directory_identity()?.clone();
        let current_selection = self.selected_entry_keys();
        if !zip_selection_order_is_current(
            target.selection_derived,
            &target.selected_keys,
            &self.selected_entry_keys_in_display_order(),
        ) || !zip_create_context_is_eligible(
            Some(clicked),
            &target.context_keys,
            target.selection_derived,
            &target.selected_keys,
            &current_selection,
            true,
        ) {
            return None;
        }
        let parent = self.session.location()?.path()?;
        let captured = target
            .context_keys
            .iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                let source = entry.path.as_ref()?;
                let identity = entry.source_identity.as_ref()?;
                if !zip_create_entry_is_eligible(
                    matches!(key, entry_identity::EntryKey::Path { .. }),
                    matches!(entry.kind, EntryKind::File | EntryKind::Folder),
                    entry.link.is_none(),
                    identity.kind == entry.kind,
                    self.session.directory_identity().is_some(),
                ) || source.parent()? != parent
                {
                    return None;
                }
                Some((
                    source.clone(),
                    entry.kind,
                    identity.filesystem_identity(),
                    source.file_name()?.to_str()?.to_owned(),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let anchor = self
            .entries
            .iter()
            .find(|entry| &entry.key == clicked)?
            .path
            .as_ref()?;
        let anchor_entry = captured.iter().find(|(path, _, _, _)| path == anchor)?;
        let stem = match anchor_entry.1 {
            EntryKind::File => anchor.file_stem()?.to_str()?,
            EntryKind::Folder => anchor.file_name()?.to_str()?,
        };
        let destination = parent.join(format!("{stem}.7z"));
        let guards = captured
            .iter()
            .map(
                |(_, kind, identity, entry_name)| operations::model::SevenZipCreateSourceGuard {
                    source_identity: identity.clone(),
                    source_kind: *kind,
                    entry_name: entry_name.clone(),
                },
            )
            .collect();
        operations::model::OperationRequest::create_7z(
            captured.into_iter().map(|(path, _, _, _)| path).collect(),
            destination,
            operations::model::SevenZipCreateGuard {
                destination_parent_identity: parent_identity,
                sources: guards,
                provider_revision: hint.revision,
                provider_binding: hint.binding.clone(),
            },
        )
        .ok()
        .map(|mut request| {
            request.origin = Some(operations::model::OperationOrigin {
                view_id: target.tab as u64,
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            request
        })
    }

    pub(crate) fn zip_extract_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<operations::model::OperationRequest> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        let clicked = target.item.as_ref()?;
        let keys = target.context_keys.as_slice();
        if keys.is_empty() || keys.len() > 1024 || !keys.contains(clicked) {
            return None;
        }
        let parent_identity = self.session.directory_identity()?.clone();
        let current_selection = self.selected_entry_keys();
        if !zip_selection_order_is_current(
            target.selection_derived,
            &target.selected_keys,
            &self.selected_entry_keys_in_display_order(),
        ) {
            return None;
        }
        if !zip_create_context_is_eligible(
            Some(clicked),
            keys,
            target.selection_derived,
            &target.selected_keys,
            &current_selection,
            self.session.directory_identity().is_some(),
        ) {
            return None;
        }
        let current_directory = self.session.location()?.path()?;
        let captured = keys
            .iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                let source = entry.path.as_ref()?;
                let identity = entry.source_identity.as_ref()?;
                if !matches!(key, entry_identity::EntryKey::Path { .. })
                    || entry.kind != EntryKind::File
                    || entry.link.is_some()
                    || identity.kind != EntryKind::File
                    || source.parent()? != current_directory
                    || !source.extension()?.to_str()?.eq_ignore_ascii_case("zip")
                {
                    return None;
                }
                Some((
                    source.clone(),
                    operations::model::ZipCreateSourceGuard {
                        source_identity: identity.filesystem_identity(),
                        source_kind: EntryKind::File,
                        entry_name: source.file_name()?.to_str()?.to_owned(),
                    },
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let sources = captured
            .iter()
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>();
        let request = if captured.len() == 1 {
            let (source, source_guard) = captured.into_iter().next()?;
            operations::model::OperationRequest::extract_zip(
                source.clone(),
                source.parent()?.join(source.file_stem()?),
                operations::model::ZipCreateGuard {
                    source_identity: source_guard.source_identity,
                    destination_parent_identity: parent_identity,
                    source_kind: source_guard.source_kind,
                    entry_name: source_guard.entry_name,
                },
            )
        } else {
            operations::model::OperationRequest::extract_zip_many(
                sources,
                operations::model::ZipArchiveExtractGuard {
                    destination_parent_identity: parent_identity,
                    sources: captured.into_iter().map(|(_, guard)| guard).collect(),
                },
            )
        };
        request.ok().map(|mut request| {
            request.origin = Some(operations::model::OperationOrigin {
                view_id: target.tab as u64,
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            request
        })
    }

    pub(crate) fn seven_zip_extract_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
        provider: &archive_provider::ArchiveProviderSnapshot,
    ) -> Option<operations::model::OperationRequest> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        let provider_hint = target.seven_zip_read.as_ref()?;
        if !provider_hint.matches_live(provider) {
            return None;
        }
        let clicked = target.item.as_ref()?;
        let keys = target.context_keys.as_slice();
        if keys.is_empty() || keys.len() > 1024 || !keys.contains(clicked) {
            return None;
        }
        let parent_identity = self.session.directory_identity()?.clone();
        if target.background_destination.as_deref() != self.session.location()?.path()
            || target.background_destination_identity.as_ref() != Some(&parent_identity)
            || target.seven_zip_source_identities.len() != keys.len()
        {
            return None;
        }
        let current_selection = self.selected_entry_keys();
        if !zip_selection_order_is_current(
            target.selection_derived,
            &target.selected_keys,
            &self.selected_entry_keys_in_display_order(),
        ) || !zip_create_context_is_eligible(
            Some(clicked),
            keys,
            target.selection_derived,
            &target.selected_keys,
            &current_selection,
            self.session.directory_identity().is_some(),
        ) {
            return None;
        }
        let current_directory = self.session.location()?.path()?;
        let captured = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                let source = entry.path.as_ref()?;
                let identity = entry.source_identity.as_ref()?;
                if !matches!(key, entry_identity::EntryKey::Path { .. })
                    || entry.kind != EntryKind::File
                    || entry.link.is_some()
                    || identity.kind != EntryKind::File
                    || target.seven_zip_source_identities.get(index)
                        != Some(&identity.filesystem_identity())
                    || source.parent()? != current_directory
                    || !source.extension()?.to_str()?.eq_ignore_ascii_case("7z")
                {
                    return None;
                }
                Some((
                    source.clone(),
                    operations::model::SevenZipExtractSourceGuard {
                        source_identity: identity.filesystem_identity(),
                        source_kind: EntryKind::File,
                        entry_name: source.file_name()?.to_str()?.to_owned(),
                    },
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let sources = captured.iter().map(|(source, _)| source.clone()).collect();
        operations::model::OperationRequest::extract_7z_many(
            sources,
            operations::model::SevenZipExtractGuard {
                destination_parent_identity: parent_identity,
                sources: captured.into_iter().map(|(_, guard)| guard).collect(),
                provider_revision: provider_hint.revision,
                provider_binding: provider_hint.binding.clone(),
            },
        )
        .ok()
        .map(|mut request| {
            request.origin = Some(operations::model::OperationOrigin {
                view_id: target.tab as u64,
                location_epoch: target.generation,
                presentation_revision: target.presentation_revision,
            });
            request
        })
    }

    pub(crate) fn terminal_request_for_menu_target(
        &self,
        target: &menu::MenuTarget,
    ) -> Option<platform_terminal::Request> {
        if !zip_menu_revision_is_current(
            target.generation,
            target.presentation_revision,
            self.location_generation,
            self.presentation_revision,
        ) {
            return None;
        }
        if let Some(clicked) = target.item.as_ref() {
            let [key] = target.context_keys.as_slice() else {
                return None;
            };
            if clicked != key {
                return None;
            }
            let entry = self.entries.iter().find(|entry| &entry.key == key)?;
            if entry.kind != EntryKind::Folder || entry.is_link() {
                return None;
            }
            let path = entry.path.as_ref()?;
            let identity = entry.source_identity.as_ref()?;
            if identity.kind != EntryKind::Folder
                || !matches!(key, entry_identity::EntryKey::Path { .. })
            {
                return None;
            }
            return captured_terminal_request(
                path,
                &identity.filesystem_identity(),
                self.session.location()?.path()?,
                self.session.directory_identity()?,
                self.session.has_accepted_current_listing(),
                entry.kind == EntryKind::Folder && !entry.is_link(),
                CapturedTerminalLocation::Item,
            );
        }
        let path = target.background_destination.as_ref()?;
        let identity = target.background_destination_identity.as_ref()?;
        captured_terminal_request(
            path,
            identity,
            self.session.location()?.path()?,
            self.session.directory_identity()?,
            self.session.has_accepted_current_listing(),
            true,
            CapturedTerminalLocation::Background,
        )
    }

    /// 描画済みの右クリック面だけを使う。推定 geometry は使わず、stale render は閉じる。
    pub(crate) fn item_menu_request_at(
        &self,
        position: gpui::Point<Pixels>,
    ) -> Option<menu::ItemMenuRequest> {
        let geometry = self.item_menu_geometry.borrow();
        let geometry = geometry.as_ref()?;
        if !geometry.matches_view(self) || !geometry.viewport.contains(&position) {
            return None;
        }
        if geometry.occluding_surfaces.iter().any(|bounds| {
            bounds
                .borrow()
                .as_ref()
                .is_some_and(|bounds| bounds.contains(&position))
        }) {
            return None;
        }
        let key = geometry
            .surfaces
            .iter()
            .rev()
            .find(|surface| {
                let part_matches_layout = match (geometry.layout, surface.part) {
                    (ViewLayout::Rows, ItemMenuSurfacePart::RowName) => true,
                    (
                        ViewLayout::Tiles | ViewLayout::Icons,
                        ItemMenuSurfacePart::Visual | ItemMenuSurfacePart::Name,
                    ) => true,
                    _ => false,
                };
                part_matches_layout
                    && surface
                        .bounds
                        .borrow()
                        .is_some_and(|bounds| bounds.contains(&position))
            })
            .map(|surface| surface.key.clone())?;
        self.entries
            .iter()
            .any(|entry| entry.key == key)
            .then(|| self.item_menu_request_for_key(Some(key)))
    }

    /// 解決済みの intent を実行する。key と menu が同じ入口を通る。
    fn run_intent(
        &mut self,
        intent: AppIntent,
        item: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.quick_search.is_some()
            && matches!(
                intent,
                AppIntent::QuickPreview | AppIntent::BatchRename | AppIntent::OpenProperties
            )
        {
            return true;
        }
        // Batch Rename は FileView が所有する modal。背面の古い callback も一覧操作へ進めない。
        if background_input_blocked_by_batch_rename(self.batch_rename_session.is_some()) {
            return true;
        }
        if self.address_edit.is_some() {
            return self.handle_address_key(intent, window, cx);
        }
        if self.rename_session.is_some() {
            return self.handle_rename_key(intent, window, cx);
        }
        match intent {
            AppIntent::ExitSelection => {
                self.exit_selection(cx);
                true
            }
            AppIntent::ToggleSelectionMode => {
                self.invalidate_pending_restore_for_selection();
                self.selection.toggle_selection_mode();
                self.emit_info_projection(cx);
                cx.notify();
                true
            }
            AppIntent::SelectAllEntries => {
                self.cancel_rectangle_drag();
                self.invalidate_pending_restore_for_selection();
                self.selection.select_all(self.entries.len());
                self.emit_info_projection(cx);
                cx.notify();
                true
            }
            AppIntent::MoveFocus { delta, extend } => self.move_focus(delta, extend, cx),
            AppIntent::SetLayout { layout } => self.set_layout(layout, cx),
            AppIntent::ToggleColumnVisibility { column } => {
                self.toggle_column_visibility(column, cx)
            }
            AppIntent::MoveColumn { column, direction } => self.move_column(column, direction, cx),
            AppIntent::Refresh => self.refresh_listing(cx),
            AppIntent::NavigateUp => self.navigate_up(cx),
            AppIntent::OpenProperties => {
                let Some(paths) = self.properties_paths_for_live_scope() else {
                    return false;
                };
                cx.emit(DirectoryEvent::OpenProperties(paths));
                true
            }
            AppIntent::ShowContextMenu => {
                let clicked =
                    item.and_then(|index| self.entries.get(index).map(|entry| entry.key.clone()));
                let request = self.item_menu_request_for_key(clicked);
                cx.emit(DirectoryEvent::ShowContextMenu {
                    request,
                    at: window.mouse_position(),
                });
                true
            }
            AppIntent::Back => {
                self.back(cx);
                true
            }
            AppIntent::Forward => {
                self.forward(cx);
                true
            }
            AppIntent::Copy | AppIntent::Cut => {
                if matches!(intent, AppIntent::Copy) {
                    match self.clipboard_copy_sources_for_live_selection_or_focus() {
                        Some(ClipboardCopySources::Paths(paths)) => {
                            cx.emit(DirectoryEvent::ClipboardWrite {
                                paths,
                                effect: operations::file_clipboard::ClipboardEffect::Copy,
                                location_generation: self.location_generation,
                                presentation_revision: self.presentation_revision,
                            })
                        }
                        Some(ClipboardCopySources::Namespace(request)) => {
                            cx.emit(DirectoryEvent::NamespaceClipboardWrite {
                                parent: request.parent,
                                identities: request.identities,
                                location_generation: self.location_generation,
                                presentation_revision: self.presentation_revision,
                            })
                        }
                        None => return false,
                    }
                    return true;
                }
                let Some(paths) = self.clipboard_sources_for_live_selection_or_focus() else {
                    return false;
                };
                cx.emit(DirectoryEvent::ClipboardWrite {
                    paths,
                    effect: operations::file_clipboard::ClipboardEffect::Move,
                    location_generation: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                true
            }
            AppIntent::Paste => {
                let Some(destination) = self.session.location().and_then(Location::path) else {
                    return false;
                };
                let Some(destination_identity) = self.session.directory_identity().cloned() else {
                    return false;
                };
                cx.emit(DirectoryEvent::PasteInto {
                    destination: destination.to_path_buf(),
                    destination_identity,
                    location_epoch: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                // Clipboard read/operation acceptance happens in Shell. The
                // visual mark is not an operation result and must not survive
                // a user starting a different paste target.
                self.cut_paths.clear();
                cx.notify();
                true
            }
            AppIntent::PasteHere => false,
            AppIntent::PasteLink => {
                let Some(destination) = self.session.location().and_then(Location::path) else {
                    return false;
                };
                cx.emit(DirectoryEvent::PasteLinkInto {
                    destination: destination.to_path_buf(),
                    location_epoch: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                true
            }
            AppIntent::PasteJunction => {
                let Some(destination) = self.session.location().and_then(Location::path) else {
                    return false;
                };
                cx.emit(DirectoryEvent::PasteJunctionInto {
                    destination: destination.to_path_buf(),
                    location_epoch: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                true
            }
            AppIntent::CreateFolder => {
                let Some(destination) = self.session.location().and_then(Location::path) else {
                    return false;
                };
                let Some(destination_identity) = self.session.directory_identity().cloned() else {
                    return false;
                };
                cx.emit(DirectoryEvent::CreateFolderInto {
                    destination: destination.to_path_buf(),
                    destination_identity,
                    location_epoch: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                true
            }
            AppIntent::CreateFile => {
                // New File は background menu が保持した target ticket だけで開始する。
                false
            }
            AppIntent::DeleteToTrash => {
                let Some(sources) = self.selected_trash_sources() else {
                    return false;
                };
                cx.emit(DirectoryEvent::Trash {
                    paths: sources.iter().map(|(path, _, _)| path.clone()).collect(),
                    source_identities: sources
                        .iter()
                        .map(|(_, identity, _)| Some(identity.clone()))
                        .collect(),
                    reparse_candidates: sources.into_iter().map(|(_, _, candidate)| candidate).collect(),
                    location_epoch: self.location_generation,
                    presentation_revision: self.presentation_revision,
                });
                true
            }
            AppIntent::UndoFileOperation => {
                cx.emit(DirectoryEvent::UndoFileOperation);
                true
            }
            AppIntent::ReloadKeymap => {
                cx.emit(DirectoryEvent::ReloadKeymap);
                true
            }
            AppIntent::QuickPreview => {
                if matches!(self.selection.focus_target(), Some(FocusTarget::Section(_))) {
                    return true;
                }
                let target_index = item.or_else(|| {
                    self.selection
                        .focused()
                        .or_else(|| (!self.entries.is_empty()).then_some(first_item_index()))
                });
                let target = target_index.and_then(|index| self.quick_preview_target(index));
                cx.emit(DirectoryEvent::QuickPreview { target, open: true });
                true
            }
            AppIntent::QuickSearch => {
                self.begin_quick_search(window, cx);
                true
            }
            AppIntent::Rename => {
                let Some(source) = self.selected_rename_source() else {
                    return false;
                };
                // The editing surface is added below this typed boundary.  Do
                // not submit a rename until its value has been explicitly
                // committed; F2 only begins a local editing session.
                self.begin_rename(source, false, window, cx);
                true
            }
            AppIntent::BatchRename => self.begin_batch_rename(window, cx),
            AppIntent::CopyPaths | AppIntent::CopyNames => false,
            intent @ (AppIntent::PickItem { .. } | AppIntent::ActivateItem { .. }) => {
                let Some(index) = item else {
                    // 対象が無い。当たったことを消費にしない。
                    return false;
                };
                self.apply_item(intent, index, window, cx);
                true
            }
            // タブ切替はシェル側の binding なので、一覧では解決されない。
            AppIntent::NextTab
            | AppIntent::PrevTab
            | AppIntent::ToggleMenu
            | AppIntent::OpenEverythingSearch
            | AppIntent::ToggleRenameExtensionSelection => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn dispatch_intent_for_test(
        &mut self,
        intent: AppIntent,
        item: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.run_intent(intent, item, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn selection_identity_for_test(
        &self,
    ) -> (
        Vec<entry_identity::EntryKey>,
        Option<entry_identity::EntryKey>,
        Vec<entry_identity::EntryKey>,
    ) {
        let keys = self
            .entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        let focused = self
            .selection
            .focused()
            .and_then(|index| keys.get(index).cloned());
        let selected = keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| self.selection.contains(index).then(|| key.clone()))
            .collect();
        (keys, focused, selected)
    }

    #[cfg(test)]
    pub(crate) fn set_selection_for_test(&mut self, keys: &[entry_identity::EntryKey]) {
        self.selection
            .apply(SelectionIntent::Clear, self.entries.len());
        for key in keys {
            if let Some(index) = self.entries.iter().position(|entry| &entry.key == key) {
                self.selection.apply(
                    SelectionIntent::Pick {
                        index,
                        toggle: true,
                        range: false,
                    },
                    self.entries.len(),
                );
            }
        }
    }

    #[cfg(test)]
    #[cfg(test)]
    pub(crate) fn mark_other_reparse_point_for_test(
        &mut self,
        key: &entry_identity::EntryKey,
    ) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.key == key) {
            entry.link = Some(directory::LinkKind::OtherReparsePoint);
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_link_for_test(&mut self, key: &entry_identity::EntryKey) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.key == key) {
            entry.link = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn is_focused_for_test(&self, window: &gpui::Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Delete carries identities from the complete captured selection; incomplete scopes reject.
    fn selected_trash_sources(
        &self,
    ) -> Option<Vec<(std::path::PathBuf, directory::EntrySourceIdentity, bool)>> {
        selected_trash_sources_for_selection(
            &self.entries,
            &self.selection,
            self.selection.focused(),
        )
    }

    fn clipboard_sources_for_keys(
        &self,
        keys: &[entry_identity::EntryKey],
    ) -> Option<Vec<std::path::PathBuf>> {
        if keys.is_empty() || keys.iter().collect::<HashSet<_>>().len() != keys.len() {
            return None;
        }
        keys.iter()
            .map(|key| {
                let entry = self.entries.iter().find(|entry| &entry.key == key)?;
                let path = entry.path.as_ref()?;
                entry.source_identity.as_ref()?;
                (entry.kind == EntryKind::File || entry.kind == EntryKind::Folder).then_some(())?;
                if matches!(entry.link, Some(LinkKind::OtherReparsePoint)) {
                    return None;
                }
                Some(path.clone())
            })
            .collect()
    }

    fn clipboard_sources_for_live_selection_or_focus(&self) -> Option<Vec<std::path::PathBuf>> {
        let keys = if self.selection.len() > 0 {
            self.selected_entry_keys_in_display_order()
        } else {
            vec![
                self.selection
                    .focused()
                    .and_then(|index| self.entries.get(index))?
                    .key
                    .clone(),
            ]
        };
        self.clipboard_sources_for_keys(&keys)
    }

    fn internal_drag_snapshot(&self, source_index: usize) -> Option<InternalDragSnapshot> {
        let source = self.entries.get(source_index)?;
        if source.path.is_none() {
            return None;
        }
        let selected = (0..self.entries.len())
            .filter(|index| self.selection.contains(*index))
            .collect::<HashSet<_>>();
        let sources = capture_internal_drag_sources(&self.entries, source_index, &selected)?;
        if self.rename_session.is_some() {
            return None;
        }
        (!sources.is_empty()).then_some(InternalDragSnapshot {
            sources,
            touch_start_position: None,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            session_id: 0,
        })
    }

    /// Touch drag は folder-tree の action button 上から始めない。
    /// hit geometry が現在の一覧と合わない間も、操作部を item drag と誤認しないよう閉じる。
    fn touch_drag_excluded_at(&self, position: gpui::Point<Pixels>) -> bool {
        let geometry = self.item_menu_geometry.borrow();
        let Some(geometry) = geometry.as_ref() else {
            return true;
        };
        geometry.excludes_touch_drag_at(
            position,
            self.location_generation,
            self.presentation_revision,
            self.layout,
            self.columns,
            self.layout_width,
            self.scroll_handle.bounds(),
        )
    }

    fn accepts_internal_drag_source(&self, snapshot: &InternalDragSnapshot) -> bool {
        self.rename_session.is_none()
            && internal_drag_snapshot_is_current(
                snapshot,
                &self.entries,
                self.location_generation,
                self.presentation_revision,
            )
    }

    fn accepts_native_drag_source(&self, snapshot: &InternalDragSnapshot) -> bool {
        internal_drag_session_id_is_current(self.next_internal_drag_session_id, snapshot.session_id)
            && self.accepts_internal_drag_source(snapshot)
    }

    fn accepts_internal_drag_session(&self, session_id: u64) -> bool {
        internal_drag_session_id_is_current(self.next_internal_drag_session_id, session_id)
    }

    fn current_internal_drag_source_projection(&self) -> Option<(u64, bool)> {
        self.internal_drag_snapshot
            .borrow()
            .as_ref()
            .map(|snapshot| {
                (
                    snapshot.session_id,
                    self.accepts_native_drag_source(snapshot),
                )
            })
    }

    fn accepts_internal_drag_target(&self, target: &InternalDropTarget) -> bool {
        internal_drop_target_is_current(
            target,
            &self.entries,
            self.location_generation,
            self.presentation_revision,
        )
    }

    fn tab_directory_drop_source_identity(
        &self,
        payload: &gpui_filer_ui::TabDirectoryDragPayload,
        target: &InternalDropTarget,
        cx: &App,
    ) -> Option<filesystem_identity::FilesystemIdentity> {
        let Some(source) = self.tab_directory_drag_source.borrow().clone() else {
            return None;
        };
        if source.session_id != payload.session_id()
            || source.source_tab_id != payload.source_tab_id()
        {
            return None;
        }
        tab_directory_move_source_identity(&source, self, target, cx)
    }

    fn accepts_tab_directory_drop(
        &self,
        payload: &gpui_filer_ui::TabDirectoryDragPayload,
        target: &InternalDropTarget,
        cx: &App,
    ) -> bool {
        self.tab_directory_drop_source_identity(payload, target, cx)
            .is_some()
    }

    fn external_background_drop_target(&self) -> Option<ExternalDropTarget> {
        if self.rename_session.is_some() {
            return None;
        }
        let Some(Location::Path(path)) = self.session.location() else {
            return None;
        };
        Some(ExternalDropTarget::Background {
            path: path.clone(),
            identity: self.session.directory_identity().cloned(),
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
        })
    }

    pub(crate) fn accepts_external_drop_target(&self, target: &ExternalDropTarget) -> bool {
        if self.rename_session.is_some() {
            return false;
        }
        match target {
            ExternalDropTarget::Background {
                path,
                location_generation,
                presentation_revision,
                ..
            } => {
                self.location_generation == *location_generation
                    && self.presentation_revision == *presentation_revision
                    && matches!(self.session.location(), Some(Location::Path(current)) if current == path)
            }
            ExternalDropTarget::Folder(target) => self.accepts_internal_drag_target(target),
            ExternalDropTarget::NavigationDirectory { .. } => false,
        }
    }

    pub(crate) fn set_external_drop_shell(
        &mut self,
        shell: gpui::WeakEntity<shell::FilerShell>,
        view_id: usize,
        external_file_drop_paths: Rc<RefCell<Option<Vec<std::path::PathBuf>>>>,
    ) {
        self.external_drop_shell = Some(shell);
        self.external_drop_view_id = Some(view_id);
        self.external_file_drop_paths = external_file_drop_paths;
    }

    fn prepare_external_drop(
        &mut self,
        paths: &[std::path::PathBuf],
        target: &ExternalDropTarget,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty()
            || target.location_generation() != self.location_generation
            || target.presentation_revision() != self.presentation_revision
        {
            return;
        }
        let Some((destination_path, expected_destination_identity)) = target.path_and_identity()
        else {
            return;
        };
        let session_slot = self.external_drag_session.clone();
        let mut session_guard = session_slot.borrow_mut();
        let Some(session) = session_guard.as_mut().filter(|session| {
            session.paths == paths
                && session.location_generation == self.location_generation
                && session.presentation_revision == self.presentation_revision
        }) else {
            return;
        };
        if session
            .preparation
            .as_ref()
            .is_some_and(|preparation| preparation.target == *target)
        {
            return;
        }
        let ticket = self.next_external_drop_ticket.wrapping_add(1).max(1);
        self.next_external_drop_ticket = ticket;
        session.preparation = Some(ExternalDropPreparation {
            ticket,
            target: target.clone(),
            result: None,
        });
        let captured_paths = paths.to_vec();
        let worker_paths = captured_paths.clone();
        let destination_path = destination_path.to_path_buf();
        let expected_destination_identity = expected_destination_identity.clone();
        let location_generation = self.location_generation;
        let presentation_revision = self.presentation_revision;
        let target = target.clone();
        let view = cx.entity().downgrade();
        drop(session_guard);

        cx.spawn(async move |_this, cx| {
            let observed = cx
                .background_executor()
                .spawn(async move {
                    let destination_metadata = std::fs::symlink_metadata(&destination_path).ok()?;
                    if !destination_metadata.is_dir()
                        || metadata_is_reparse_point(&destination_metadata)
                    {
                        return None;
                    }
                    let destination_observation = directory::EntrySourceIdentity::from_path_no_follow(
                        &destination_path,
                        EntryKind::Folder,
                    )?;
                    let destination_identity =
                        destination_observation.filesystem_identity();
                    if destination_identity != expected_destination_identity {
                        return None;
                    }
                    let mut source_identities = Vec::with_capacity(worker_paths.len());
                    let mut same_volume = destination_identity.windows_parts().is_some();
                    for path in &worker_paths {
                        let metadata = std::fs::symlink_metadata(path).ok()?;
                        if metadata_is_reparse_point(&metadata)
                            || (!metadata.is_file() && !metadata.is_dir())
                        {
                            return None;
                        }
                        let kind = if metadata.is_dir() {
                            EntryKind::Folder
                        } else {
                            EntryKind::File
                        };
                        let observation =
                            directory::EntrySourceIdentity::from_path_no_follow(path, kind)?;
                        let identity = observation.filesystem_identity();
                        match (identity.windows_parts(), destination_identity.windows_parts()) {
                            (Some((source_volume, _)), Some((target_volume, _))) => {
                                same_volume &= source_volume == target_volume;
                            }
                            _ => same_volume = false,
                        }
                        source_identities.push(identity);
                    }
                    Some(PreparedExternalDrop {
                        source_identities,
                        destination_identity,
                        same_volume,
                    })
                })
                .await;
            let Some(observed) = observed else {
                return;
            };
            let _ = view.update(cx, |view, _| {
                if view.location_generation != location_generation
                    || view.presentation_revision != presentation_revision
                {
                    return;
                }
                let mut slot = view.external_drag_session.borrow_mut();
                let Some(session) = slot.as_mut().filter(|session| {
                    session.paths == captured_paths
                        && session.location_generation == location_generation
                        && session.presentation_revision == presentation_revision
                }) else {
                    return;
                };
                let Some(preparation) = session.preparation.as_mut().filter(|preparation| {
                    preparation.ticket == ticket && preparation.target == target
                }) else {
                    return;
                };
                preparation.result = Some(observed);
            });
        })
        .detach();
    }

    pub(crate) fn set_tab_directory_drag_source(
        &mut self,
        source: Rc<RefCell<Option<TabDirectoryDragSource>>>,
    ) {
        self.tab_directory_drag_source = source;
    }

    fn begin_internal_drag(
        &mut self,
        index: usize,
        rendered_source: Option<&InternalDragSource>,
        touch_start_position: Option<gpui::Point<Pixels>>,
        location_generation: u64,
        presentation_revision: u64,
        slot: &RefCell<Option<InternalDragSnapshot>>,
    ) {
        *slot.borrow_mut() = None;
        if self.location_generation != location_generation
            || self.presentation_revision != presentation_revision
            || rendered_source.is_none_or(|source| {
                self.entries
                    .get(index)
                    .and_then(internal_drag_source_for_entry)
                    .as_ref()
                    != Some(source)
            })
        {
            return;
        }
        let Some(mut snapshot) = self.internal_drag_snapshot(index) else {
            return;
        };
        snapshot.touch_start_position = touch_start_position;
        let Some(session_id) = self.next_internal_drag_session_id.checked_add(1) else {
            return;
        };
        self.next_internal_drag_session_id = session_id;
        snapshot.session_id = session_id;
        *slot.borrow_mut() = Some(snapshot);
    }

    fn drop_internal_drag(
        &mut self,
        payload: &InternalDragPayload,
        target: Option<&InternalDropTarget>,
        cx: &mut Context<Self>,
    ) {
        emit_internal_drag_drop_cancel(payload, cx);
        let Some(snapshot) = take_internal_drag_snapshot(&payload.snapshot) else {
            cx.stop_propagation();
            return;
        };
        let Some(target) = target else {
            cx.stop_propagation();
            return;
        };
        let Some(shell) = self.external_drop_shell.as_ref() else {
            cx.stop_propagation();
            return;
        };
        let source_entity_id = payload.source;
        let target_entity_id = cx.entity().entity_id();
        let target_tab_id = self.external_drop_view_id;
        let target_projection_current = self.accepts_internal_drag_target(&target);
        let source_projection_current = (source_entity_id == target_entity_id).then(|| {
            self.accepts_internal_drag_session(snapshot.session_id)
                && self.accepts_internal_drag_source(&snapshot)
        });
        let snapshot = snapshot.clone();
        let target = target.clone();
        if let Some(target_tab_id) = target_tab_id {
            let _ = shell.update(cx, |shell, cx| {
                shell.admit_internal_cross_pane_move(
                    source_entity_id,
                    &snapshot,
                    target_tab_id,
                    target_entity_id,
                    target,
                    target_projection_current,
                    source_projection_current,
                    cx,
                );
            });
        }
        cx.stop_propagation();
    }

    fn is_blank_list_viewport_drop(&self, position: gpui::Point<Pixels>) -> bool {
        let viewport = self.scroll_handle.bounds();
        if !viewport.contains(&position) {
            return false;
        }
        let content_y = position.y.as_f32()
            - viewport.origin.y.as_f32()
            - self.scroll_handle.offset().y.as_f32();
        content_y >= self.heights.total_height()
    }

    fn accepts_internal_background_target(&self, target: &ExternalDropTarget) -> bool {
        let ExternalDropTarget::Background {
            identity: Some(identity),
            ..
        } = target
        else {
            return false;
        };
        self.accepts_external_drop_target(target)
            && self.session.directory_identity() == Some(identity)
    }

    fn drop_internal_background_drag(
        &mut self,
        payload: &InternalDragPayload,
        target: Option<&ExternalDropTarget>,
        cx: &mut Context<Self>,
    ) {
        emit_internal_drag_drop_cancel(payload, cx);
        let Some(snapshot) = take_internal_drag_snapshot(&payload.snapshot) else {
            cx.stop_propagation();
            return;
        };
        let Some(ExternalDropTarget::Background { .. }) = target else {
            cx.stop_propagation();
            return;
        };
        let Some(shell) = self.external_drop_shell.as_ref() else {
            cx.stop_propagation();
            return;
        };
        let target_view_entity_id = cx.entity().entity_id();
        let Some(target_tab_id) = self.external_drop_view_id else {
            cx.stop_propagation();
            return;
        };
        let target_current = self.accepts_internal_background_target(target.unwrap());
        let source_current = (payload.source == target_view_entity_id).then(|| {
            self.accepts_internal_drag_session(snapshot.session_id)
                && self.accepts_internal_drag_source(&snapshot)
        });
        let _ = shell.update(cx, |shell, cx| {
            shell.admit_internal_background_move(
                payload.source,
                &snapshot,
                target_tab_id,
                target_view_entity_id,
                target.unwrap().clone(),
                target_current,
                source_current,
                cx,
            );
        });
        cx.stop_propagation();
    }

    /// Rename accepts exactly one ordinary filesystem item. Unlike copy and
    /// trash, folders are safe here because the adapter renames one opened
    /// object and never recurses through its children.
    fn selected_rename_source(&self) -> Option<std::path::PathBuf> {
        let mut selected = self
            .entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                self.selection.contains(*index) && entry.path.is_some() && !entry.is_link()
            })
            .filter_map(|(_, entry)| entry.path.clone());
        let source = selected.next()?;
        selected.next().is_none().then_some(source)
    }

    fn begin_quick_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename_session.is_some()
            || self.address_edit.is_some()
            || self.batch_rename_session.is_some()
        {
            return;
        }
        if let Some(session) = self.quick_search.as_ref() {
            session.input.update(cx, |input, cx| {
                input.select_all(cx);
                input.focus(window, cx);
            });
            return;
        }

        let input = cx.new(|cx| rename_editor::RenameInput::address_bar(cx, String::new()));
        let input_id = input.entity_id();
        let generation = self.location_generation;
        let view_id = cx.entity().entity_id();
        let input_events = cx.subscribe(&input, move |this, _, event, cx| {
            if cx.entity().entity_id() != view_id
                || this.location_generation != generation
                || !this.quick_search.as_ref().is_some_and(|session| {
                    session.input_id == input_id
                        && session.location_generation == generation
                        && session.input.entity_id() == input_id
                })
            {
                return;
            }
            match event {
                rename_editor::RenameInputEvent::TextChanged => {
                    let Some(input) = this
                        .quick_search
                        .as_ref()
                        .map(|session| session.input.clone())
                    else {
                        return;
                    };
                    if input.read(cx).is_composing() {
                        return;
                    }
                    let query = input.read(cx).text().to_owned();
                    this.apply_quick_search_query(&query, cx);
                }
                rename_editor::RenameInputEvent::CopyPlainText(text) => {
                    cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
                }
            }
        });
        self.quick_search = Some(QuickSearchSession {
            input: input.clone(),
            input_id,
            location_generation: generation,
            _input_events: input_events,
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn apply_quick_search_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = query.trim();
        if query.is_empty() || self.entries.is_empty() {
            return;
        }
        let needle = query.to_lowercase();
        let count = self.entries.len();
        let start = self.selection.focused().unwrap_or(0);
        let found = (1..=count)
            .map(|offset| (start + offset) % count)
            .find(|&index| self.entries[index].name.to_lowercase().contains(&needle));
        let Some(index) = found else {
            return;
        };
        if self.selection.focused() == Some(index) {
            return;
        }
        self.selection.apply(
            SelectionIntent::MoveFocus {
                index,
                extend: false,
            },
            count,
        );
        self.emit_info_projection(cx);
        self.invalidate_pending_restore_for_selection();
        self.reveal_focus(index);
        cx.emit(DirectoryEvent::QuickPreview {
            target: self.quick_preview_target(index),
            open: false,
        });
        cx.notify();
    }

    /// パンくずの余白を text input に置き換える。現在地を表示するだけなので、
    /// 編集中の文字列は navigation/display-name の更新から独立している。
    fn begin_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.address_edit.is_some() {
            return;
        }
        self.close_rename_editor(false);
        let text = self
            .location()
            .as_ref()
            .and_then(Location::path)
            .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let input = cx.new(|cx| rename_editor::RenameInput::address_bar(cx, text));
        let input_events = cx.subscribe(&input, |_this, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => cx.notify(),
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
            }
        });
        self.address_edit = Some(AddressEditSession {
            input: input.clone(),
            _input_events: input_events,
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 空文字は移動先にならない。Windows path の正当性は非同期の directory session が
    /// 検証してエラーを表示するため、ここで同期 I/O はしない。
    fn commit_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.address_edit.take() else {
            return;
        };
        let destination = session.input.read(cx).text().trim().to_owned();
        self.focus_handle.focus(window, cx);
        if !destination.is_empty() {
            if is_pc_address_alias(&destination) {
                self.navigate_to_pc(cx);
                cx.notify();
                return;
            }
            #[cfg(windows)]
            let destination = resolve_shell_folder_alias(
                &destination,
                std::env::var_os("USERPROFILE")
                    .as_deref()
                    .map(std::path::Path::new),
                std::env::var_os("APPDATA")
                    .as_deref()
                    .map(std::path::Path::new),
            )
            .unwrap_or_else(|| std::path::PathBuf::from(&destination));
            #[cfg(not(windows))]
            let destination = std::path::PathBuf::from(destination);
            self.navigate(destination, cx);
        }
        cx.notify();
    }

    fn cancel_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.address_edit.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    fn handle_address_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .address_edit
            .as_ref()
            .is_some_and(|session| session.input.read(cx).is_composing())
        {
            return false;
        }
        match keystroke.key.as_str() {
            "enter" => self.commit_address_edit(window, cx),
            "escape" => self.cancel_address_edit(window, cx),
            // Text/IME と editor action は focused input だけに配送する。
            _ => return false,
        }
        true
    }

    fn handle_address_key(
        &mut self,
        intent: AppIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if matches!(intent, AppIntent::ExitSelection) {
            self.cancel_address_edit(window, cx);
        }
        // input 中の file-view intent は行や selection へ漏らさない。
        true
    }

    fn begin_rename(
        &mut self,
        source: std::path::PathBuf,
        captured_single_target: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.address_edit = None;
        if self.selection.is_multiple() && !captured_single_target {
            return;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path.as_ref() == Some(&source))
        else {
            return;
        };
        if entry.source_identity.is_none()
            || self.session.directory_identity().is_none()
            || entry.link == Some(directory::LinkKind::OtherReparsePoint)
        {
            return;
        }
        #[cfg(not(target_os = "windows"))]
        if entry.link.is_some() {
            return;
        }
        let centered = self.layout == ViewLayout::Icons;
        let selection = rename_editor::initial_filename_selection(
            &entry.name,
            matches!(entry.kind, EntryKind::File) && !self.select_extension_on_rename,
        );
        let input = cx
            .new(|cx| rename_editor::RenameInput::new(cx, entry.name.clone(), centered, selection));
        let input_events = cx.subscribe(&input, |this, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => {
                this.pending_projection_anchor = this.projection_anchor();
                if let Some(session) = this.rename_session.as_ref()
                    && let Some(index) = this
                        .entries
                        .iter()
                        .position(|entry| entry.key == session.key)
                {
                    let Some(row) = this.entry_row(index) else {
                        return;
                    };
                    if row < this.measured.len() {
                        this.measured[row] = false;
                    }
                }
                cx.notify();
            }
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
            }
        });
        self.rename_session = Some(RenameSession {
            key: entry.key.clone(),
            source,
            source_identity: entry
                .source_identity
                .as_ref()
                .map(|identity| identity.filesystem_identity()),
            link_kind: entry.link,
            parent_identity: self.session.directory_identity().cloned(),
            accepted_scan: self.session.accepted_scan_ticket().cloned(),
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            input,
            _input_events: input_events,
        });
        self.rename_session
            .as_ref()
            .unwrap()
            .input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn begin_batch_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let keys = if self.selection.len() > 0 {
            self.selected_entry_keys_in_display_order()
        } else {
            self.selection
                .focused()
                .and_then(|index| self.entries.get(index))
                .map(|entry| vec![entry.key.clone()])
                .unwrap_or_default()
        };
        self.begin_batch_rename_for_keys(
            keys,
            self.location_generation,
            self.presentation_revision,
            window,
            cx,
        )
    }

    fn begin_batch_rename_for_keys(
        &mut self,
        keys: Vec<entry_identity::EntryKey>,
        expected_generation: u64,
        expected_revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.rename_session.is_some()
            || self.address_edit.is_some()
            || self.batch_rename_session.is_some()
            || self.location_generation != expected_generation
            || self.presentation_revision != expected_revision
            || keys.is_empty()
        {
            return false;
        }
        let (Some(parent_identity), Some(accepted_scan)) = (
            self.session.directory_identity().cloned(),
            self.session.accepted_scan_ticket().cloned(),
        ) else {
            return false;
        };
        let mut kinds = Vec::with_capacity(keys.len());
        let mut sources = Vec::with_capacity(keys.len());
        let mut reparse_items = Vec::with_capacity(keys.len());
        for key in &keys {
            let Some(entry) = self.entries.iter().find(|entry| &entry.key == key) else {
                return false;
            };
            let (Some(path), Some(identity)) = (entry.path.clone(), entry.source_identity.as_ref())
            else {
                return false;
            };
            if entry.link.is_some_and(|link| link != directory::LinkKind::LinkOrJunction) {
                return false;
            }
            kinds.push(entry.kind);
            reparse_items.push(entry.is_link());
            sources.push(operations::batch_rename::RenameSource {
                source: path,
                name: entry.name.clone(),
                kind: entry.kind,
                identity: identity.filesystem_identity(),
                reparse_guard: None,
            });
        }
        self.batch_rename_probe_ticket = self.batch_rename_probe_ticket.wrapping_add(1);
        let probe_ticket = self.batch_rename_probe_ticket;
        let parent_for_probe = parent_identity.clone();
        let captures = sources
            .iter()
            .zip(&reparse_items)
            .map(|(source, is_reparse)| {
                (source.source.clone(), source.identity.clone(), source.kind, *is_reparse)
            })
            .collect::<Vec<_>>();
        let window_handle = window.window_handle();
        let view = cx.entity().downgrade();
        let view_id = cx.entity().entity_id();
        let shell = self.external_drop_shell.clone();
        let tab_id = self.external_drop_view_id;
        cx.spawn(async move |_this, cx| {
            let captured = cx.background_executor().spawn(async move {
                let mut evidence = Vec::with_capacity(captures.len());
                for (path, source_identity, kind, is_reparse) in captures {
                    let parent = path.parent()?;
                    let parent_metadata = std::fs::symlink_metadata(parent).ok()?;
                    if !parent_metadata.is_dir()
                        || metadata_is_reparse_point(&parent_metadata)
                        || directory::EntrySourceIdentity::from_path_no_follow(
                            parent,
                            EntryKind::Folder,
                        )?.filesystem_identity() != parent_for_probe
                    {
                        return None;
                    }
                    let metadata = std::fs::symlink_metadata(&path).ok()?;
                    if metadata_is_reparse_point(&metadata) != is_reparse
                        || (!is_reparse && metadata.is_dir() != (kind == EntryKind::Folder))
                    {
                        return None;
                    }
                    let guard = if is_reparse {
                        let guard = operations::rename::probe_reparse_source(
                            &path,
                            &source_identity,
                            &parent_for_probe,
                        ).ok()?;
                        if guard.reparse_tag == 0xA000_0003 && kind != EntryKind::Folder {
                            return None;
                        }
                        Some(guard)
                    } else {
                        if directory::EntrySourceIdentity::from_path_no_follow(&path, kind)?
                                .filesystem_identity() != source_identity
                        {
                            return None;
                        }
                        None
                    };
                    evidence.push(guard);
                }
                Some(evidence)
            }).await;
            let Some(evidence) = captured else { return; };
            let _ = window_handle.update(cx, |_handle, window, cx| {
                let Some(shell) = shell.as_ref() else {
                    let _ = view.update(cx, |this, _| {
                        if this.batch_rename_probe_ticket == probe_ticket {
                            this.batch_rename_probe_ticket = this.batch_rename_probe_ticket.wrapping_add(1);
                        }
                    });
                    return;
                };
                let owner_current = shell.update(cx, |shell, cx| {
                    tab_id.is_some_and(|tab_id| {
                        shell.batch_rename_target_is_current(tab_id, view_id, cx)
                    })
                }).unwrap_or(false);
                if !owner_current {
                    let _ = view.update(cx, |this, _| {
                        if this.batch_rename_probe_ticket == probe_ticket {
                            this.batch_rename_probe_ticket = this.batch_rename_probe_ticket.wrapping_add(1);
                        }
                    });
                    return;
                }
                let _ = view.update(cx, |this, cx| {
                    if this.batch_rename_probe_ticket != probe_ticket {
                        return;
                    }
                    if !this.batch_rename_request_current(
                        &keys, &sources, &kinds, &reparse_items,
                        &parent_identity, &accepted_scan,
                        expected_generation, expected_revision,
                    ) || this.rename_session.is_some()
                        || this.address_edit.is_some()
                        || this.batch_rename_session.is_some()
                    {
                        this.batch_rename_probe_ticket = this.batch_rename_probe_ticket.wrapping_add(1);
                        return;
                    }
                    let mut sources = sources;
                    for (source, guard) in sources.iter_mut().zip(evidence) {
                        source.reparse_guard = guard;
                    }
                    this.open_batch_rename_session(
                        keys, kinds, sources, parent_identity, accepted_scan,
                        expected_generation, expected_revision, window, cx,
                    );
                });
            });
        }).detach();
        true
    }

    fn batch_rename_request_current(
        &self,
        keys: &[entry_identity::EntryKey],
        sources: &[operations::batch_rename::RenameSource],
        kinds: &[EntryKind],
        reparse_items: &[bool],
        parent_identity: &filesystem_identity::FilesystemIdentity,
        accepted_scan: &directory_session::ScanTicket,
        generation: u64,
        revision: u64,
    ) -> bool {
        self.location_generation == generation
            && self.presentation_revision == revision
            && self.session.accepted_scan_ticket() == Some(accepted_scan)
            && self.session.directory_identity() == Some(parent_identity)
            && keys.len() == sources.len()
            && keys.len() == kinds.len()
            && keys.len() == reparse_items.len()
            && keys.iter().zip(sources).zip(kinds).zip(reparse_items).all(
                |(((key, source), kind), is_reparse)| {
                    self.entries.iter().find(|entry| &entry.key == key).is_some_and(|entry| {
                        entry.kind == *kind
                            && entry.is_link() == *is_reparse
                            && entry.path.as_ref() == Some(&source.source)
                            && entry.name == source.name
                            && entry.source_identity.as_ref().is_some_and(|identity| {
                                identity.filesystem_identity() == source.identity
                            })
                    })
                },
            )
    }

    fn open_batch_rename_session(
        &mut self,
        keys: Vec<entry_identity::EntryKey>,
        kinds: Vec<EntryKind>,
        sources: Vec<operations::batch_rename::RenameSource>,
        parent_identity: filesystem_identity::FilesystemIdentity,
        accepted_scan: directory_session::ScanTicket,
        location_generation: u64,
        presentation_revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pattern_value = "{name}_{n}{ext}".to_owned();
        let pattern = cx.new(|cx| {
            rename_editor::RenameInput::new(
                cx,
                pattern_value.clone(),
                false,
                0..pattern_value.len(),
            )
        });
        let find = cx.new(|cx| rename_editor::RenameInput::new(cx, String::new(), false, 0..0));
        let replace = cx.new(|cx| rename_editor::RenameInput::new(cx, String::new(), false, 0..0));
        let changed = cx.subscribe(&pattern, |_, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => cx.notify(),
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
            }
        });
        let changed_find = cx.subscribe(&find, |_, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => cx.notify(),
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
            }
        });
        let changed_replace = cx.subscribe(&replace, |_, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => cx.notify(),
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                cx.emit(DirectoryEvent::CopyPlainText { text: text.clone() });
            }
        });
        self.batch_rename_session = Some(BatchRenameSession {
            keys,
            kinds,
            sources,
            parent_identity,
            accepted_scan,
            location_generation,
            presentation_revision,
            pattern: pattern.clone(),
            find,
            replace,
            body_scroll: ScrollHandle::new(),
            _events: vec![changed, changed_find, changed_replace],
        });
        pattern.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn batch_rename_probe_is_admissible(&self) -> bool {
        self.rename_session.is_none()
            && self.address_edit.is_none()
            && self.batch_rename_session.is_none()
            && self.quick_search.is_none()
    }

    fn batch_rename_capture_current(&self, session: &BatchRenameSession) -> bool {
        self.location_generation == session.location_generation
            && self.presentation_revision == session.presentation_revision
            && self.session.accepted_scan_ticket() == Some(&session.accepted_scan)
            && self.session.directory_identity() == Some(&session.parent_identity)
            && session.keys.len() == session.sources.len()
            && session.keys.len() == session.kinds.len()
            && session
                .keys
                .iter()
                .zip(&session.sources)
                .zip(&session.kinds)
                .all(|((key, source), kind)| {
                    self.entries
                        .iter()
                        .find(|entry| &entry.key == key)
                        .is_some_and(|entry| {
                            entry.is_link() == source.reparse_guard.is_some()
                                && source.reparse_guard.as_ref().is_none_or(|guard| {
                                    guard.source_identity == source.identity
                                        && guard.parent_identity == session.parent_identity
                                })
                                && entry.kind == *kind
                                && entry.path.as_ref() == Some(&source.source)
                                && entry.name == source.name
                                && entry.source_identity.as_ref().is_some_and(|identity| {
                                    identity.filesystem_identity() == source.identity
                                })
                        })
                })
    }

    fn submit_batch_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.batch_rename_session.as_ref() else {
            return;
        };
        if !self.batch_rename_capture_current(session) {
            self.batch_rename_session = None;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        let sources = session.sources.clone();
        let parent_identity = session.parent_identity.clone();
        let location_epoch = session.location_generation;
        let presentation_revision = session.presentation_revision;
        let pattern = session.pattern.read(cx).text().to_owned();
        let find = session.find.read(cx).text().to_owned();
        let replace = session.replace.read(cx).text().to_owned();
        let existing_names = self
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        let preview =
            operations::batch_rename::preview(&sources, &pattern, &find, &replace, &existing_names);
        if preview.no_change {
            self.batch_rename_session = None;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        if !preview.can_submit {
            cx.notify();
            return;
        }
        let plan = preview
            .rows
            .into_iter()
            .zip(sources)
            .filter(|(row, _)| row.changed)
            .map(|(row, source)| operations::batch_rename::RenameTarget {
                source: source.source,
                destination: row.destination,
                source_identity: source.identity,
                parent_identity: parent_identity.clone(),
                kind: source.kind,
                reparse_guard: source.reparse_guard,
            })
            .collect();
        self.batch_rename_session = None;
        self.focus_handle.focus(window, cx);
        cx.emit(DirectoryEvent::BatchRename {
            plan,
            location_epoch,
            presentation_revision,
        });
        cx.notify();
        let _ = window;
    }

    fn render_batch_rename_dialog(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let session = self.batch_rename_session.as_ref()?;
        let current = self.batch_rename_capture_current(session);
        let sources = session.sources.clone();
        let existing_names = self
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        let pattern_text = session.pattern.read(cx).text().to_owned();
        let find_text = session.find.read(cx).text().to_owned();
        let replace_text = session.replace.read(cx).text().to_owned();
        let preview = operations::batch_rename::preview(
            &sources,
            &pattern_text,
            &find_text,
            &replace_text,
            &existing_names,
        );
        let pattern = session.pattern.clone();
        let find = session.find.clone();
        let replace = session.replace.clone();
        let body_scroll = session.body_scroll.clone();
        let mut panel = div()
            .id("batch-rename-dialog-panel")
            .w(relative(0.92))
            .max_w(px(680.0))
            .max_h(relative(0.92))
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .bg(gpui_filer_ui::theme_palette(cx).surface_base)
            .text_color(gpui_filer_ui::theme_palette(cx).text)
            .rounded(px(8.0));
        let mut body = div()
            .id("batch-rename-dialog-body")
            .flex_grow_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .overflow_y_scroll()
            .track_scroll(&body_scroll)
            .occlude()
            .child(
                div()
                    .flex_shrink_0()
                    .child(self.translator.text(Message::BatchRenameTitle).into_owned()),
            );
        for (label, input) in [
            (Message::BatchRenamePattern, pattern),
            (Message::BatchRenameFind, find),
            (Message::BatchRenameReplace, replace),
        ] {
            let input_for_focus = input.clone();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .gap(px(4.0))
                    .child(self.translator.text(label).into_owned())
                    .child(
                        div()
                            .h(px(44.0))
                            .flex_shrink_0()
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(gpui_filer_ui::theme_palette(cx).border)
                            .child(input.clone())
                            .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                                cx.stop_propagation();
                                input_for_focus.update(cx, |input, cx| {
                                    input.focus_at(event.position, window, cx)
                                });
                            }),
                    ),
            );
        }
        body = body.child(
            div()
                .flex_shrink_0()
                .child(self.translator.item_count(preview.rows.len())),
        );
        let mut preview_rows = div().flex().flex_col().gap(px(4.0));
        for row in preview.rows.iter().take(12) {
            let source = row
                .source
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            let destination = row
                .destination
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            preview_rows = preview_rows.child(
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(format!("{source}  →  {destination}")),
                    )
                    .when_some(row.error, |element, error| {
                        element.child(div().flex_shrink_0().text_color(gpui::red()).child(error))
                    }),
            );
        }
        body = body.child(preview_rows);
        if !current {
            body = body.child(
                div().flex_shrink_0().text_color(gpui::red()).child(
                    self.translator
                        .text(Message::BatchRenameListingChanged)
                        .into_owned(),
                ),
            );
        }
        panel = panel.child(body);
        let can_submit = current && preview.can_submit && !preview.no_change;
        panel = panel.child(
            div()
                .flex_shrink_0()
                .flex()
                .justify_end()
                .gap(px(8.0))
                .child(
                    div()
                        .id("batch-rename-cancel")
                        .min_h(px(44.0))
                        .flex()
                        .items_center()
                        .px(px(12.0))
                        .py(px(8.0))
                        .child(
                            self.translator
                                .text(Message::BatchRenameCancel)
                                .into_owned(),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.batch_rename_session = None;
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("batch-rename-submit")
                        .min_h(px(44.0))
                        .flex()
                        .items_center()
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_color(if can_submit {
                            gpui_filer_ui::theme_palette(cx).text
                        } else {
                            gpui_filer_ui::theme_palette(cx).text_muted
                        })
                        .child(if preview.no_change {
                            self.translator
                                .text(Message::BatchRenameNoChanges)
                                .into_owned()
                        } else {
                            self.translator
                                .text(Message::BatchRenameSubmit)
                                .into_owned()
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_batch_rename_dialog(window, cx)
                        })),
                ),
        );
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x00000088))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .child(panel)
                .into_any_element(),
        )
    }

    fn set_select_extension_on_rename(&mut self, select: bool) {
        self.select_extension_on_rename = select;
    }

    pub(crate) fn set_translator(&mut self, translator: Translator, cx: &mut Context<Self>) {
        self.translator = translator;
        cx.notify();
    }

    /// The router has already resolved a file-view intent, so an active editor
    /// consumes it rather than letting Delete/Ctrl+V/etc. escape to rows.
    fn handle_rename_key(
        &mut self,
        intent: AppIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match intent {
            AppIntent::ExitSelection => {
                self.close_rename_editor(false);
                self.focus_handle.focus(window, cx);
                cx.notify();
                true
            }
            AppIntent::Rename => {
                self.commit_rename(window, cx);
                true
            }
            _ => true,
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rename_session.as_ref() else {
            return;
        };
        let new_name = rename.input.read(cx).text().to_owned();
        let Some(attempt_ticket) = next_rename_attempt_ticket(self.rename_attempt_ticket) else {
            self.close_rename_editor(false);
            return;
        };
        self.rename_attempt_ticket = attempt_ticket;
        let case_only = operations::rename::is_case_only_rename(&rename.source, &new_name);
        let ordinary_directory = self.entries.iter().any(|entry| {
            entry.key == rename.key
                && entry.kind == directory::EntryKind::Folder
                && entry.link.is_none()
        });
        let ordinary_inline = self.rename_session.as_ref().is_some_and(|session| session.link_kind.is_none());
        let ordinary_identity_captured = self.rename_session.as_ref().is_some_and(|session| {
            session.source_identity.is_some() && session.parent_identity.is_some()
        });
        let ordinary_undo_eligible = ordinary_inline && ordinary_identity_captured;
        let identity_guard = (case_only || ordinary_directory || ordinary_undo_eligible)
            .then(|| {
                Some(operations::model::MoveIdentityGuard {

                    source_identities: vec![rename.source_identity.clone()?],
                    destination_identity: rename.parent_identity.clone()?,
                    })
            })
            .flatten();
        let expected_path = rename.source.with_file_name(&new_name);
        let pending_rename_anchor = self.projection_anchor_avoiding(&rename.key);
        let pending_geometry = self.rename_session.as_ref().and_then(|session| {
            self.entries
                .iter()
                .position(|entry| entry.key == session.key)
                .map(|index| {
                    let entry = &self.entries[index];
                    let expected_key = entry_identity::EntryKey::path(expected_path, entry.kind);
                    let offset = (-self.scroll_handle.offset().y.as_f32()).max(0.0);
                    let mut anchor = self.projection_anchor_at(index, offset);
                    if let Some(anchor) = anchor.as_mut() {
                        anchor.key = expected_key.clone();
                    }
                    PendingRenameGeometry {
                        token: None,
                        key: session.key.clone(),
                        expected_key,
                        expected_identity: session.source_identity.clone(),
                        committed_name: new_name.clone(),
                        anchor,
                        fallback_anchor: pending_rename_anchor.clone(),
                        location_generation: session.location_generation,
                        presentation_revision: session.presentation_revision,
                        terminal_waiting_for_listing: false,
                        accepted_scan_before_terminal: None,
                        pending_scan_before_terminal: None,
                        failed_scan_after_terminal: None,
                        directory: session
                            .source
                            .parent()
                            .map(std::path::Path::to_path_buf)
                            .unwrap_or_default(),
                    }
                })
        });
        let still_current = self.rename_session.as_ref().is_some_and(|session| {
            self.location_generation == session.location_generation
                && self.presentation_revision == session.presentation_revision
                && (!(case_only || ordinary_directory || ordinary_undo_eligible)
                    || identity_guard.is_some()
                        && self.session.pending().is_none()
                        && session.parent_identity.as_ref() == self.session.directory_identity()
                        && session.accepted_scan.as_ref() == self.session.accepted_scan_ticket())
                && self.entries.iter().any(|entry| {
                    entry.key == session.key
                        && entry.path.as_ref() == Some(&session.source)
                        && (((case_only || ordinary_directory || identity_guard.is_some())
                            && entry
                                .source_identity
                                .as_ref()
                                .map(|identity| identity.filesystem_identity())
                                == session.source_identity)
                            || (!case_only
                                && !ordinary_directory
                                && identity_guard.is_none()
                                && session.link_kind.is_none()))
                })
        });
        let retain_geometry = still_current && pending_geometry.is_some();
        let Some(session) = self.close_rename_editor(retain_geometry) else {
            return;
        };
        if retain_geometry {
            // terminal と対応する listing が揃うまで editor 行高を保つ。ここで
            // 通常行高へ戻すと、listing 側の anchor 補正と二段階になって揺れる。
            self.pending_projection_anchor = None;
            self.pending_rename_anchor = None;
            self.pending_rename_geometry = pending_geometry;
        }
        if still_current {
            cx.emit(DirectoryEvent::Rename {
                source: session.source,
                key: session.key,
                new_name,
                identity_guard,
                attempt_ticket,
                link_kind: session.link_kind,
                source_identity: session.source_identity,
                accepted_scan: session.accepted_scan,
                parent_identity: session.parent_identity,
                location_epoch: session.location_generation,
                presentation_revision: session.presentation_revision,
            });
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Escape/cancel は直ちに通常行高へ戻す。commit は operation terminal/listing
    /// まで高さを pin するので、その間だけ `retain_geometry` を使う。
    fn close_rename_editor(&mut self, retain_geometry: bool) -> Option<RenameSession> {
        let key = self.rename_session.as_ref()?.key.clone();
        if !retain_geometry {
            self.pending_projection_anchor = self.projection_anchor_avoiding(&key);
        }
        let session = self.rename_session.take()?;
        if !retain_geometry
            && let Some(index) = self.entries.iter().position(|entry| entry.key == key)
        {
            let Some(row) = self.entry_row(index) else {
                return Some(session);
            };
            if row < self.measured.len() {
                self.measured[row] = false;
            }
        }
        Some(session)
    }

    /// operation terminal の hint は snapshot を書き換えず、watch と同じ session
    /// reconciliation にだけ渡す。
    pub(crate) fn reconcile_operation_hint(
        &mut self,
        directory: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        if self.session.operation_dirty(directory) {
            if let Some(effect) = self.session.take_follow_up() {
                self.run_effect(effect, cx);
            }
            cx.notify();
        }
    }

    pub(crate) fn shell_visibility(&self) -> bool {
        self.session.is_visible()
    }

    pub(crate) fn set_shell_visibility(&mut self, visible: bool, cx: &mut Context<Self>) {
        let changed = self.session.is_visible() != visible;
        if !changed {
            return;
        }
        if visible {
            // 再表示までに watcher が root の変更を取りこぼしても、旧 preview を再利用しない。
            self.invalidate_known_media();
        }
        if let Some(effect) = self.session.set_visible(visible) {
            self.run_effect(effect, cx);
        }
        cx.notify();
    }

    pub(crate) fn rename_operation_accepted(
        &mut self,
        token: operations::model::AttemptToken,
        location_epoch: u64,
        presentation_revision: u64,
    ) {
        if self.location_generation != location_epoch
            || self.presentation_revision != presentation_revision
        {
            return;
        }
        if let Some(pending) = self.pending_rename_geometry.as_mut()
            && pending.location_generation == location_epoch
            && pending.presentation_revision == presentation_revision
        {
            pending.token = Some(token);
        }
    }

    pub(crate) fn rename_operation_rejected(&mut self, cx: &mut Context<Self>) {
        self.release_pending_rename_geometry(cx);
    }

    pub(crate) fn rename_operation_terminal(
        &mut self,
        token: operations::model::AttemptToken,
        wait_for_listing: bool,
        cx: &mut Context<Self>,
    ) {
        let matches = self
            .pending_rename_geometry
            .as_ref()
            .is_some_and(|pending| {
                pending.token == Some(token)
                    && pending.location_generation == self.location_generation
                    && pending.presentation_revision == self.presentation_revision
            });
        if !matches {
            return;
        }
        if wait_for_listing {
            let pending = self.pending_rename_geometry.as_mut().unwrap();
            pending.terminal_waiting_for_listing = true;
            pending.accepted_scan_before_terminal = self.session.accepted_scan_ticket().cloned();
            pending.pending_scan_before_terminal = self.session.pending().cloned();
        } else {
            self.release_pending_rename_geometry(cx);
        }
    }

    fn rename_reconciliation_failed(&mut self, ticket: &directory_session::ScanTicket) {
        let Some(pending) = self.pending_rename_geometry.as_mut() else {
            return;
        };
        if !pending.terminal_waiting_for_listing
            || pending.location_generation != self.location_generation
            || pending.presentation_revision != self.presentation_revision
            || ticket.path != pending.directory
            || pending.accepted_scan_before_terminal.as_ref() == Some(ticket)
            || pending.pending_scan_before_terminal.as_ref() == Some(ticket)
        {
            return;
        }
        pending.failed_scan_after_terminal = Some(ticket.clone());
    }

    fn release_pending_rename_geometry(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_rename_geometry.take() else {
            return;
        };
        if self.location_generation != pending.location_generation
            || self.presentation_revision != pending.presentation_revision
        {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.key == pending.key)
        {
            let Some(row) = self.entry_row(index) else {
                return;
            };
            if row < self.measured.len() {
                self.measured[row] = false;
            }
        }
        self.pending_projection_anchor =
            pending.fallback_anchor.or_else(|| self.projection_anchor());
        cx.notify();
    }

    /// Shell が paste event を受け取る直前に、その event がまだ同じ directory
    /// projection を指すか確認する。clipboard の内容は live state ではないが、貼り付け先は
    /// navigation / refresh をまたいで再解釈しない。
    pub(crate) fn accepts_paste_target(
        &self,
        destination: &std::path::Path,
        location_epoch: u64,
        presentation_revision: u64,
    ) -> bool {
        self.location_generation == location_epoch
            && self.presentation_revision == presentation_revision
            && self.session.location().and_then(Location::path) == Some(destination)
    }

    pub(crate) fn accepts_background_paste_target(
        &self,
        destination: &std::path::Path,
        destination_identity: &filesystem_identity::FilesystemIdentity,
        location_epoch: u64,
        presentation_revision: u64,
    ) -> bool {
        self.accepts_paste_target(destination, location_epoch, presentation_revision)
            && self.session.directory_identity() == Some(destination_identity)
    }

    pub(crate) fn accepts_paste_folder_target(&self, target: &InternalDropTarget) -> bool {
        self.accepts_internal_drag_target(target)
    }

    /// Delete は navigation 後の別 location に配送しない。対象 path 自体は command
    /// acceptance 時の immutable value なので、表示行を operation authority にしない。
    pub(crate) fn accepts_operation_origin(
        &self,
        location_epoch: u64,
        presentation_revision: u64,
    ) -> bool {
        self.location_generation == location_epoch
            && self.presentation_revision == presentation_revision
    }

    pub(crate) fn accepts_rename_probe(
        &self,
        key: &entry_identity::EntryKey,
        source: &std::path::Path,
        link_kind: directory::LinkKind,
        source_identity: &filesystem_identity::FilesystemIdentity,
        accepted_scan: &directory_session::ScanTicket,
        parent_identity: &filesystem_identity::FilesystemIdentity,
        location_epoch: u64,
        presentation_revision: u64,
        attempt_ticket: u64,
    ) -> bool {
        self.is_current_rename_attempt(attempt_ticket)
            && self.accepts_operation_origin(location_epoch, presentation_revision)
            && self.session.accepted_scan_ticket() == Some(accepted_scan)
            && self.session.directory_identity() == Some(parent_identity)
            && self.entries.iter().any(|entry| {
                entry.key == *key
                    && entry.path.as_deref() == Some(source)
                    && entry.link == Some(link_kind)
                    && entry
                        .source_identity
                        .as_ref()
                        .is_some_and(|identity| identity.filesystem_identity() == *source_identity)
            })
    }

    pub(crate) fn is_current_rename_attempt(&self, attempt_ticket: u64) -> bool {
        should_reject_rename_geometry(self.rename_attempt_ticket, attempt_ticket)
    }

    /// Clipboard の結果は view-local な見た目だけへ反映し、古い event が再利用された
    /// directory projection を変更しないよう identity を確認する。
    pub(crate) fn apply_clipboard_write_result(
        &mut self,
        paths: &[std::path::PathBuf],
        effect: operations::file_clipboard::ClipboardEffect,
        succeeded: bool,
        location_generation: u64,
        presentation_revision: u64,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_operation_origin(location_generation, presentation_revision) {
            #[cfg(test)]
            {
                self.clipboard_write_result_for_test =
                    Some((succeeded, false, location_generation, presentation_revision));
            }
            return;
        }
        #[cfg(test)]
        {
            self.clipboard_write_result_for_test =
                Some((succeeded, true, location_generation, presentation_revision));
        }
        if succeeded && effect == operations::file_clipboard::ClipboardEffect::Move {
            self.cut_paths = paths.iter().cloned().collect();
        } else {
            self.cut_paths.clear();
        }
        cx.notify();
    }

    /// Everything の結果は directory listing の行ではない。Shell が activation 時の
    /// active tab へ immutable path だけ渡し、通常の navigation / OS open 境界へ
    /// 接続する。検索結果で selection や entry projection を書き換えない。
    pub(crate) fn activate_search_result(
        &mut self,
        path: std::path::PathBuf,
        is_folder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_folder {
            cx.emit(DirectoryEvent::OpenFolder(Location::Path(path)));
            return;
        }
        // Everything の属性が古く、folder が file として届く場合にもここでは OS に
        // 委ねる。同期的に再判定して UI thread を止めない。
        window.defer(cx, move |_window, _cx| {
            if let Err(error) = platform_open::open_in_default_app(&path) {
                eprintln!(
                    "[v6] failed to open Everything result: path={} err={error}",
                    path.display()
                );
            }
        });
    }

    /// 項目を対象にする動作を適用する。
    ///
    /// クリック、キー、menu のどれから来てもここを通る。availability は呼び出し側が見ている。
    fn apply_item(
        &mut self,
        intent: AppIntent,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            AppIntent::PickItem { toggle, range } => {
                self.invalidate_pending_restore_for_selection();
                self.selection.apply(
                    SelectionIntent::Pick {
                        index,
                        toggle,
                        range,
                    },
                    self.entries.len(),
                );
                self.emit_info_projection(cx);
                cx.emit(DirectoryEvent::QuickPreview {
                    target: self.quick_preview_target(index),
                    open: false,
                });
                cx.notify();
            }
            AppIntent::ActivateItem { new_tab } => {
                let Some(entry) = self.entries.get(index) else {
                    return;
                };
                #[cfg(windows)]
                let seven_zip_read_available = entry.path.as_ref().is_some_and(|path| {
                    path.extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("7z"))
                        && cx
                            .try_global::<archive_provider::ArchiveProviderOwner>()
                            .is_some_and(|owner| owner.capture_7z_read_capability().is_some())
                });
                #[cfg(not(windows))]
                let seven_zip_read_available = false;
                let action = resolve_activation_target(
                    entry,
                    self.entry_targets.get(&entry.key).cloned(),
                    seven_zip_read_available,
                );
                match action {
                    Some(ActivationTarget::Navigate(target)) => {
                        if new_tab {
                            cx.emit(DirectoryEvent::OpenTab(target));
                        } else {
                            cx.emit(DirectoryEvent::OpenFolder(target));
                        }
                    }
                    // 通常ファイルは既定アプリへ渡す。プロセス起動で描画を止めないよう、
                    // フレームを描き終えてから起動する(v5 の `Window::defer` と同じ)。
                    Some(ActivationTarget::OpenFile(path)) => {
                        window.defer(cx, move |_window, _cx| {
                            if let Err(error) = platform_open::open_in_default_app(&path) {
                                eprintln!(
                                    "[v6] failed to open file: path={} err={error}",
                                    path.display()
                                );
                            }
                        });
                    }
                    None => {}
                }
            }
            // 項目以外を対象にする動作はここでは扱わない。
            _ => {}
        }
    }

    /// 表示形式を切り替える。toolbar と key が同じ入口を通る。
    ///
    /// 今と同じ形式を選んでも何もしない（当たったことを消費にもしない）。
    fn set_layout(&mut self, layout: ViewLayout, cx: &mut Context<Self>) -> bool {
        if self.layout == layout {
            return false;
        }
        self.row_column_resize = None;
        self.cancel_row_column_reorder();
        self.touch_item_tap_series = None;
        self.last_click_was_blank = false;
        self.cancel_rectangle_drag();
        self.layout = layout;
        self.hovered_item_part = None;
        self.layout_width = 0.0;
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(folder_view_preferences::ChangedField::Layout, cx);
        cx.notify();
        true
    }

    fn toggle_column_visibility(&mut self, column: Column, cx: &mut Context<Self>) -> bool {
        if self.layout != ViewLayout::Rows || column == Column::Name {
            return false;
        }
        self.row_column_resize = None;
        self.cancel_row_column_reorder();
        match column {
            Column::Kind => self.column_prefs.kind = !self.column_prefs.kind,
            Column::Size => self.column_prefs.size = !self.column_prefs.size,
            Column::Modified => self.column_prefs.modified = !self.column_prefs.modified,
            Column::Name => return false,
        }
        let visible = match column {
            Column::Kind => self.column_prefs.kind,
            Column::Size => self.column_prefs.size,
            Column::Modified => self.column_prefs.modified,
            Column::Name => return false,
        };
        self.hovered_item_part = None;
        self.layout_width = 0.0;
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(
            folder_view_preferences::ChangedField::Column { column, visible },
            cx,
        );
        cx.notify();
        true
    }

    pub(crate) fn begin_row_column_resize(&mut self, left: Column, right: Column, x: f32) {
        if self.layout != ViewLayout::Rows || left == Column::Name && right == Column::Name {
            return;
        }
        let projection = self.row_columns_for_width(self.layout_width, self.column_prefs);
        self.row_column_resize = Some(RowColumnResize {
            left,
            right,
            start_x: x,
            start: self.column_prefs,
            preview: self.column_prefs,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            order: self.column_prefs.order,
            visible: [
                self.column_prefs.kind,
                self.column_prefs.size,
                self.column_prefs.modified,
            ],
            projection: [projection.kind, projection.size, projection.modified],
        });
    }

    pub(crate) fn update_row_column_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(mut drag) = self.row_column_resize else {
            return;
        };
        if self.layout != ViewLayout::Rows
            || self.location_generation != drag.location_generation
            || self.presentation_revision != drag.presentation_revision
            || self.column_prefs.order != drag.order
            || [
                self.column_prefs.kind,
                self.column_prefs.size,
                self.column_prefs.modified,
            ] != drag.visible
            || {
                let projection = self.row_columns_for_width(self.layout_width, drag.preview);
                [projection.kind, projection.size, projection.modified] != drag.projection
            }
        {
            self.row_column_resize = None;
            return;
        }
        let delta = x - drag.start_x;
        if drag.left == Column::Name {
            set_width(
                &mut drag.preview,
                drag.right,
                width_of(drag.start, drag.right) - delta,
            );
        } else if drag.right == Column::Name {
            set_width(
                &mut drag.preview,
                drag.left,
                width_of(drag.start, drag.left) + delta,
            );
        } else {
            let left = width_of(drag.start, drag.left);
            let right = width_of(drag.start, drag.right);
            let change = delta
                .max((50.0 - left).max(right - 500.0))
                .min((500.0 - left).min(right - 50.0));
            set_width(&mut drag.preview, drag.left, left + change);
            set_width(&mut drag.preview, drag.right, right - change);
        }
        let preview_projection = self.row_columns_for_width(self.layout_width, drag.preview);
        if [
            preview_projection.kind,
            preview_projection.size,
            preview_projection.modified,
        ] != drag.projection
        {
            self.row_column_resize = None;
            cx.notify();
            return;
        }
        self.row_column_resize = Some(drag);
        cx.notify();
    }

    pub(crate) fn finish_row_column_resize(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.row_column_resize.take() else {
            return;
        };
        if self.location_generation != drag.location_generation
            || self.layout != ViewLayout::Rows
            || self.presentation_revision != drag.presentation_revision
            || self.column_prefs.order != drag.order
            || [
                self.column_prefs.kind,
                self.column_prefs.size,
                self.column_prefs.modified,
            ] != drag.visible
            || {
                let projection = self.row_columns_for_width(self.layout_width, drag.preview);
                [projection.kind, projection.size, projection.modified] != drag.projection
            }
        {
            return;
        }
        for column in [drag.left, drag.right] {
            if column == Column::Name {
                continue;
            }
            let width = width_of(drag.preview, column);
            if (width - width_of(drag.start, column)).abs() >= 0.5 {
                set_width(&mut self.column_prefs, column, width);
                self.emit_folder_prefs_changed(
                    folder_view_preferences::ChangedField::Width { column, width },
                    cx,
                );
            }
        }
    }

    pub(crate) fn cancel_row_column_resize(&mut self) {
        self.row_column_resize = None;
    }

    fn capture_row_column_reorder_down(&mut self, column: Column, event: &gpui::MouseDownEvent) {
        self.row_column_reorder_down = None;
        if event.button != gpui::MouseButton::Left
            || event.click_count != 1
            || event.source != gpui::MouseInputSource::Mouse
        {
            return;
        }
        let modifiers = input::press_chord(event).map(|chord| chord.modifiers);
        let Some(modifiers) = modifiers else {
            return;
        };
        self.row_column_reorder_down = Some(RowColumnReorderDown {
            source: column,
            modifiers,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            layout: self.layout,
            prefs: self.column_prefs,
            projection: self.row_columns_for_width(self.layout_width, self.column_prefs),
        });
    }

    fn begin_row_column_reorder(&mut self, source: Column) {
        let Some(down) = self.row_column_reorder_down.take() else {
            return;
        };
        if down.source != source
            || down.modifiers != gpui_filer_input::Modifiers::default()
            || !self.row_column_reorder_is_current(down)
        {
            return;
        }
        self.row_column_reorder = Some(RowColumnReorder { down });
        self.row_column_reorder_suppress_click = true;
    }

    fn row_column_reorder_is_current(&self, down: RowColumnReorderDown) -> bool {
        self.layout == ViewLayout::Rows
            && down.layout == self.layout
            && down.location_generation == self.location_generation
            && down.presentation_revision == self.presentation_revision
            && down.prefs == self.column_prefs
            && down.projection == self.row_columns_for_width(self.layout_width, self.column_prefs)
    }

    fn cancel_row_column_reorder(&mut self) {
        self.row_column_reorder_down = None;
        self.row_column_reorder = None;
    }

    fn finish_row_column_reorder(
        &mut self,
        source: Column,
        target: Column,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.row_column_reorder.take() else {
            return;
        };
        let projection = self.row_columns_for_width(self.layout_width, self.column_prefs);
        let target_visible = match target {
            Column::Name => true,
            Column::Kind => projection.kind,
            Column::Size => projection.size,
            Column::Modified => projection.modified,
        };
        if target_visible
            && self.row_column_reorder_is_current(drag.down)
            && drag.down.source == source
            && source != target
        {
            self.reorder_column(source, target, cx);
        }
    }

    fn move_column(&mut self, column: Column, direction: isize, cx: &mut Context<Self>) -> bool {
        if self.layout != ViewLayout::Rows || !matches!(direction, -1 | 1) {
            return false;
        }
        let Some(index) = self
            .column_prefs
            .order
            .iter()
            .position(|item| *item == column)
        else {
            return false;
        };
        let target = index as isize + direction;
        if !(0..4).contains(&target) {
            return false;
        }
        self.row_column_resize = None;
        self.cancel_row_column_reorder();
        let mut order = self.column_prefs.order;
        order.swap(index, target as usize);
        self.column_prefs.order = order;
        self.hovered_item_part = None;
        self.layout_width = 0.0;
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(folder_view_preferences::ChangedField::Order { order }, cx);
        cx.notify();
        true
    }

    fn reorder_column(&mut self, source: Column, target: Column, cx: &mut Context<Self>) -> bool {
        if self.layout != ViewLayout::Rows || source == target {
            return false;
        }
        let mut order = self.column_prefs.order.to_vec();
        let Some(source_index) = order.iter().position(|column| *column == source) else {
            return false;
        };
        let Some(target_index) = order.iter().position(|column| *column == target) else {
            return false;
        };
        let moved = order.remove(source_index);
        order.insert(target_index, moved);
        let Ok(order): Result<[Column; 4], _> = order.try_into() else {
            return false;
        };
        if self.column_prefs.order == order {
            return false;
        }
        self.row_column_resize = None;
        self.cancel_row_column_reorder();
        self.column_prefs.order = order;
        self.hovered_item_part = None;
        self.layout_width = 0.0;
        cx.emit(DirectoryEvent::StateChanged);
        self.emit_folder_prefs_changed(folder_view_preferences::ChangedField::Order { order }, cx);
        cx.notify();
        true
    }

    /// 選択を解除する。Escape と toolbar のボタンが同じ入口を通る。
    fn exit_selection(&mut self, cx: &mut Context<Self>) {
        self.scrollbar_drag = None;
        self.cancel_rectangle_drag();
        self.touch_item_tap_series = None;
        self.invalidate_pending_restore_for_selection();
        self.selection
            .apply(SelectionIntent::Clear, self.entries.len());
        self.emit_info_projection(cx);
        self.last_click_was_blank = false;
        cx.notify();
    }

    /// 空白の left pointer-down を控える。ここではまだ selection を変えないので、通常の
    /// 背景 click / double click は従来どおり `ClickEvent` だけで完結する。
    fn capture_rectangle_pointer_down(&mut self, event: &gpui::MouseDownEvent) {
        self.rectangle_pointer_down = None;
        match event.source {
            gpui::MouseInputSource::Mouse => self.interaction.set_last_device(InputDevice::Mouse),
            gpui::MouseInputSource::Touch => {
                self.interaction.set_last_device(InputDevice::Touch);
                return;
            }
            gpui::MouseInputSource::Pen => {
                self.interaction.set_last_device(InputDevice::Pen);
                return;
            }
            gpui::MouseInputSource::Unknown => return,
        }
        if event.click_count != 1 || event.button != gpui::MouseButton::Left {
            return;
        }
        let Some(chord) =
            input::press_chord(event).map(|chord| MouseChord::drag(chord.button, chord.modifiers))
        else {
            return;
        };
        let scopes = self.base_scopes();
        let bindings = self.keymap_runtime.snapshot();
        let Some(action) = bindings
            .file_view
            .resolve_mouse(&chord, &scopes)
            .map(|binding| binding.action)
        else {
            return;
        };
        let mode = match action.as_str() {
            input::RECTANGLE_ADD => RectangleMode::Add,
            input::RECTANGLE_TOGGLE => RectangleMode::Toggle,
            _ => return,
        };
        let bounds = self.scroll_handle.bounds();
        let offset = self.scroll_handle.offset().y.as_f32();
        self.rectangle_pointer_down = Some(RectanglePointerDown {
            x: event.position.x.as_f32(),
            y: event.position.y.as_f32(),
            content_y: event.position.y.as_f32() - bounds.origin.y.as_f32() - offset,
            mode,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            layout: self.layout,
            columns: self.columns,
            layout_width: self.layout_width,
        });
    }

    /// GPUI の drag threshold を越えた時点で初めて marquee transaction を始める。
    fn begin_rectangle_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(down) = self.rectangle_pointer_down.take() else {
            return false;
        };
        if !self.rectangle_gesture_is_current(
            down.location_generation,
            down.presentation_revision,
            down.layout,
            down.columns,
            down.layout_width,
        ) {
            return false;
        }
        self.touch_item_tap_series = None;
        self.focus_handle.focus(window, cx);
        self.last_click_was_blank = false;
        self.invalidate_pending_restore_for_selection();
        self.selection.begin_rectangle(down.mode);
        self.rectangle_drag = Some(RectangleDragState {
            start_x: down.x,
            start_y: down.content_y,
            current_x: down.x,
            current_y: down.y,
            location_generation: down.location_generation,
            presentation_revision: down.presentation_revision,
            layout: down.layout,
            columns: down.columns,
            layout_width: down.layout_width,
            surfaces: HashMap::new(),
            surface_overflow: false,
        });
        self.capture_rectangle_surfaces();
        self.update_rectangle_preview(down.x, down.y);
        cx.notify();
        true
    }

    fn rectangle_gesture_is_current(
        &self,
        location_generation: u64,
        presentation_revision: u64,
        layout: ViewLayout,
        columns: usize,
        layout_width: f32,
    ) -> bool {
        self.location_generation == location_generation
            && self.presentation_revision == presentation_revision
            && self.layout == layout
            && self.columns == columns
            && self.layout_width == layout_width
    }

    /// viewport 座標を RowHeights の content 座標へ写し、交差した行だけを interval として
    /// selection へ渡す。ここで項目数に比例する work は行わない。
    fn update_rectangle_preview(&mut self, x: f32, y: f32) {
        let Some((
            start_y,
            location_generation,
            presentation_revision,
            layout,
            columns,
            layout_width,
        )) = self.rectangle_drag.as_mut().map(|drag| {
            drag.current_x = x;
            drag.current_y = y;
            (
                drag.start_y,
                drag.location_generation,
                drag.presentation_revision,
                drag.layout,
                drag.columns,
                drag.layout_width,
            )
        })
        else {
            return;
        };
        if !self.rectangle_gesture_is_current(
            location_generation,
            presentation_revision,
            layout,
            columns,
            layout_width,
        ) {
            self.cancel_rectangle_drag();
            return;
        }
        let row_count = self.heights.len();
        if row_count == 0 || self.entries.is_empty() {
            return;
        }
        let bounds = self.scroll_handle.bounds();
        let offset = self.scroll_handle.offset().y.as_f32();
        let start = start_y.max(0.0);
        let end = (y - bounds.origin.y.as_f32() - offset).max(0.0);
        let top = start.min(end).min(self.heights.total_height());
        let bottom = start.max(end).min(self.heights.total_height());
        let first_row = self.heights.row_at_offset(top).min(row_count);
        let last_row = self
            .heights
            .row_at_offset((bottom - 0.01).max(0.0))
            .min(row_count.saturating_sub(1));
        let _candidate_rows = (first_row, last_row);
    }

    /// RectangleDrag の pointer update ごとにだけ edge scroll する。静止中の timer は持たない。
    fn update_rectangle_drag_pointer(&mut self, x: f32, y: f32) {
        let current = self.rectangle_drag.as_ref().is_some_and(|drag| {
            self.rectangle_gesture_is_current(
                drag.location_generation,
                drag.presentation_revision,
                drag.layout,
                drag.columns,
                drag.layout_width,
            )
        });
        if !current {
            if self.rectangle_drag.is_some() {
                self.cancel_rectangle_drag();
            }
            return;
        }

        let bounds = self.scroll_handle.bounds();
        let viewport = bounds.size.height.as_f32();
        let local_y = y - bounds.origin.y.as_f32();
        let edge = 28.0;
        let max_scroll_per_event = 30.0;
        let delta = if local_y < edge {
            -max_scroll_per_event * ((edge - local_y) / edge).clamp(0.0, 1.0)
        } else if local_y > viewport - edge {
            max_scroll_per_event * ((local_y - (viewport - edge)) / edge).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if viewport > 0.0 && delta != 0.0 {
            let content = self.heights.total_height() + 44.0;
            let current_offset = -self.scroll_handle.offset().y.as_f32();
            let next_offset = clamp_scroll_offset(current_offset + delta, content, viewport);
            if (next_offset - current_offset).abs() > 0.01 {
                self.scroll_handle
                    .set_offset(gpui::point(px(0.0), -px(next_offset)));
                self.scroll_revision = self.scroll_revision.wrapping_add(1);
            }
        }
        // 境界で scroll できない場合も、最新 pointer に preview を追従させる。
        self.update_rectangle_preview(x, y);
    }

    fn finish_rectangle_drag(&mut self, cx: &mut Context<Self>) -> bool {
        self.rectangle_pointer_down = None;
        self.capture_rectangle_surfaces();
        let Some(drag) = self.rectangle_drag.take() else {
            return false;
        };
        if self.rectangle_gesture_is_current(
            drag.location_generation,
            drag.presentation_revision,
            drag.layout,
            drag.columns,
            drag.layout_width,
        ) {
            if let Some(hits) = self.rectangle_hit_indices(&drag) {
                self.invalidate_pending_restore_for_selection();
                self.selection.commit_rectangle_hits(hits);
                self.emit_info_projection(cx);
            } else {
                self.selection.cancel_rectangle();
            }
        } else {
            self.selection.cancel_rectangle();
        }
        cx.notify();
        true
    }

    /// release 時だけ候補行を segment tree で絞って実hitを作る。drag move はこの走査を
    /// 行わず、可視 item の paint だけが矩形を読む。
    fn rectangle_hit_indices(&self, drag: &RectangleDragState) -> Option<Vec<usize>> {
        let Some(rectangle) = self.rectangle_content_rect(drag) else {
            return Some(Vec::new());
        };
        let rows = self.heights.len();
        if rows == 0 || drag.surface_overflow {
            return (!drag.surface_overflow).then(Vec::new);
        }
        let first = self.heights.row_at_offset(rectangle.y).min(rows);
        let last = self
            .heights
            .row_at_offset((rectangle.y + rectangle.height - 0.01).max(0.0))
            .min(rows.saturating_sub(1));
        if first > last || first >= rows {
            return Some(Vec::new());
        }
        let mut hits = Vec::new();
        for index in (first..=last).flat_map(|row| self.row_entries(row)) {
            let entry = self.entries.get(index)?;
            let surfaces = drag.surfaces.get(&entry.key)?;
            if surfaces.iter().any(|hit| rectangle.intersects(*hit)) {
                hits.push(index);
            }
        }
        Some(hits)
    }

    fn rectangle_content_rect(&self, drag: &RectangleDragState) -> Option<ContentRect> {
        let bounds = self.scroll_handle.bounds();
        let offset = self.scroll_handle.offset().y.as_f32();
        let viewport_width = bounds.size.width.as_f32();
        let left = (drag.start_x.min(drag.current_x) - bounds.origin.x.as_f32())
            .clamp(0.0, viewport_width);
        let right = (drag.start_x.max(drag.current_x) - bounds.origin.x.as_f32())
            .clamp(0.0, viewport_width);
        let start = drag.start_y.max(0.0);
        let current = (drag.current_y - bounds.origin.y.as_f32() - offset).max(0.0);
        let top = start.min(current);
        let bottom = start.max(current);
        (right > left && bottom > top).then_some(ContentRect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    fn item_intersects_rectangle(&self, index: usize, rectangle: ContentRect) -> bool {
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        self.rectangle_drag
            .as_ref()
            .and_then(|drag| drag.surfaces.get(&entry.key))
            .is_some_and(|surfaces| surfaces.iter().any(|hit| rectangle.intersects(*hit)))
    }

    /// 完成した一 frame の測定面を、現在の drag だけに content 座標で蓄積する。
    /// 描画要素は使い捨てのまま、スクロールで画面外へ出た行の geometry は残す。
    fn capture_rectangle_surfaces(&mut self) {
        let Some(drag) = self.rectangle_drag.as_mut() else {
            return;
        };
        if drag.surface_overflow {
            return;
        }
        let geometry = self.item_menu_geometry.borrow();
        let Some(geometry) = geometry.as_ref().filter(|geometry| {
            geometry.matches_projection(
                drag.location_generation,
                drag.presentation_revision,
                drag.layout,
                drag.columns,
                drag.layout_width,
                self.scroll_handle.bounds(),
            )
        }) else {
            return;
        };
        const MAX_DRAG_SURFACES: usize = 32_768;
        let mut total = drag.surfaces.values().map(Vec::len).sum::<usize>();
        for surface in &geometry.surfaces {
            let eligible = matches!(
                (drag.layout, surface.part),
                (ViewLayout::Rows, ItemMenuSurfacePart::RowName | ItemMenuSurfacePart::Checkbox)
                    | (ViewLayout::Tiles | ViewLayout::Icons,
                        ItemMenuSurfacePart::Visual | ItemMenuSurfacePart::Name | ItemMenuSurfacePart::Checkbox)
            );
            if !eligible {
                continue;
            }
            let Some(bounds) = *surface.bounds.borrow() else { continue };
            let viewport = geometry.viewport;
            let left = bounds.origin.x.as_f32().max(viewport.origin.x.as_f32());
            let right = (bounds.origin.x + bounds.size.width)
                .as_f32()
                .min((viewport.origin.x + viewport.size.width).as_f32());
            let top = bounds.origin.y.as_f32().max(viewport.origin.y.as_f32());
            let bottom = (bounds.origin.y + bounds.size.height)
                .as_f32()
                .min((viewport.origin.y + viewport.size.height).as_f32());
            if right <= left || bottom <= top {
                continue;
            }
            let rect = ContentRect {
                x: left - viewport.origin.x.as_f32(),
                y: top - viewport.origin.y.as_f32()
                    - geometry.scroll_offset.y.as_f32(),
                width: right - left,
                height: bottom - top,
            };
            let rects = drag.surfaces.entry(surface.key.clone()).or_default();
            if !rects.contains(&rect) {
                rects.push(rect);
                total += 1;
                if total > MAX_DRAG_SURFACES {
                    drag.surface_overflow = true;
                    return;
                }
            }
        }
    }

    /// 直近に描画した item surface の bounds だけを返す。stale な projection は使わない。
    #[cfg(test)]
    fn item_hit_rects(&self, index: usize) -> Vec<ContentRect> {
        let Some(entry) = self.entries.get(index) else {
            return Vec::new();
        };
        let geometry = self.item_menu_geometry.borrow();
        let Some(geometry) = geometry
            .as_ref()
            .filter(|geometry| geometry.matches_view(self))
        else {
            return Vec::new();
        };
        let mut rects = Vec::new();
        for surface in geometry
            .surfaces
            .iter()
            .filter(|surface| surface.key == entry.key)
        {
            let eligible = match (self.layout, surface.part) {
                (
                    ViewLayout::Rows,
                    ItemMenuSurfacePart::RowName | ItemMenuSurfacePart::Checkbox,
                )
                | (
                    ViewLayout::Tiles | ViewLayout::Icons,
                    ItemMenuSurfacePart::Visual
                    | ItemMenuSurfacePart::Name
                    | ItemMenuSurfacePart::Checkbox,
                ) => true,
                _ => false,
            };
            if !eligible {
                continue;
            }
            let Some(bounds) = *surface.bounds.borrow() else {
                continue;
            };
            let viewport = geometry.viewport;
            let left = bounds.origin.x.as_f32().max(viewport.origin.x.as_f32());
            let right = (bounds.origin.x + bounds.size.width)
                .as_f32()
                .min((viewport.origin.x + viewport.size.width).as_f32());
            let top = bounds.origin.y.as_f32().max(viewport.origin.y.as_f32());
            let bottom = (bounds.origin.y + bounds.size.height)
                .as_f32()
                .min((viewport.origin.y + viewport.size.height).as_f32());
            if right > left && bottom > top {
                rects.push(ContentRect {
                    x: left - viewport.origin.x.as_f32(),
                    y: top - viewport.origin.y.as_f32() - geometry.scroll_offset.y.as_f32(),
                    width: right - left,
                    height: bottom - top,
                });
            }
        }
        rects
    }

    fn cancel_rectangle_drag(&mut self) -> bool {
        self.rectangle_pointer_down = None;
        let active = self.rectangle_drag.take().is_some() || self.selection.rectangle_active();
        self.selection.cancel_rectangle();
        active
    }

    fn cancel_stale_rectangle_drag(&mut self, cx: &mut Context<Self>) {
        let stale = self.rectangle_drag.as_ref().is_some_and(|drag| {
            !self.rectangle_gesture_is_current(
                drag.location_generation,
                drag.presentation_revision,
                drag.layout,
                drag.columns,
                drag.layout_width,
            )
        });
        if stale && self.cancel_rectangle_drag() {
            cx.notify();
        }
    }

    /// list-frame 内で描く marquee の可視部分。scroll content ではなく viewport に重ねるので、
    /// 長い一覧でも 1 要素だけで済む。
    fn rectangle_visual_bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let drag = self.rectangle_drag.as_ref()?;
        if !self.rectangle_gesture_is_current(
            drag.location_generation,
            drag.presentation_revision,
            drag.layout,
            drag.columns,
            drag.layout_width,
        ) {
            return None;
        }
        let bounds = self.scroll_handle.bounds();
        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32();
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let left = (drag.start_x.min(drag.current_x) - bounds.origin.x.as_f32()).clamp(0.0, width);
        let right = (drag.start_x.max(drag.current_x) - bounds.origin.x.as_f32()).clamp(0.0, width);
        let start_screen_y =
            bounds.origin.y.as_f32() + drag.start_y + self.scroll_handle.offset().y.as_f32();
        let top =
            (start_screen_y.min(drag.current_y) - bounds.origin.y.as_f32()).clamp(0.0, height);
        let bottom =
            (start_screen_y.max(drag.current_y) - bounds.origin.y.as_f32()).clamp(0.0, height);
        (right > left && bottom > top).then_some((left, top, right - left, bottom - top))
    }

    /// 今の場所を読み直す。F5 と、失敗中の再試行ボタンが同じ入口を通る。
    ///
    /// 読む場所が無ければ何もしない（availability と同じ条件）。
    fn refresh_listing(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_this_pc() {
            // PC 自体は再列挙しない。icon identity asset の世代を進め、古い描画を
            // semantic fallback へ戻してから、同じ staged source から identity を取り直す。
            self.invalidate_known_media();
            for row in &mut self.pc_rows {
                row.source_identity = None;
            }
            for entry in &mut self.entries {
                entry.source_identity = None;
            }
            self.pc_shell_icon_targets.clear();
            cx.emit(DirectoryEvent::QuickPreview {
                target: self.focused_quick_preview_target(),
                open: false,
            });
            cx.notify();
            return true;
        }
        let Some(effect) = self.session.refresh() else {
            return false;
        };
        // 列挙では size / mtime が同じ差替えを拾えない。
        // 明示的な再読込では画像と表示名も作り直す。
        self.invalidate_known_media();
        cx.emit(DirectoryEvent::QuickPreview {
            target: self.focused_quick_preview_target(),
            open: false,
        });
        self.run_effect(effect, cx);
        cx.notify();
        true
    }

    /// menu の availability と、対象になる項目があるか。
    ///
    /// bar の menu は focus している項目を見る。`Needs::Item` の判定だけに使うので、
    /// index は渡さない。
    fn menu_context(&self) -> (commands::CommandContext, bool) {
        (self.command_context(), self.selection.focused().is_some())
    }

    /// menu の stale 検証へ渡すために、表示上の選択を identity 集合へ投影する。
    /// `snapshot` は Shift の transient を確定してしまうので、ここでは contains を使う。
    pub(crate) fn selected_entry_keys(&self) -> HashSet<entry_identity::EntryKey> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, _)| self.selection.contains(*index))
            .map(|(_, entry)| entry.key.clone())
            .collect()
    }

    fn selected_entry_keys_in_display_order(&self) -> Vec<entry_identity::EntryKey> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, _)| self.selection.contains(*index))
            .map(|(_, entry)| entry.key.clone())
            .collect()
    }

    pub(crate) fn info_projection(&self) -> InfoProjection {
        let selected_count = self.selection.len();
        let copy_token = (selected_count > 1).then(|| InfoCopyToken {
            key: None,
            selection_revision: self.selection_aggregate_revision,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
        });
        let candidate = match selected_count {
            0 => self
                .selection
                .focused()
                .and_then(|index| self.entries.get(index)),
            1 => self
                .entries
                .iter()
                .enumerate()
                .find(|(index, _)| self.selection.contains(*index))
                .map(|(_, entry)| entry),
            _ => None,
        };
        let selected_count = selected_count.max(usize::from(candidate.is_some()));
        let item = candidate.and_then(|entry| {
            let path = entry.path.as_ref()?;
            let kind = if entry.link.is_some() {
                "リンク".to_string()
            } else if entry.kind == EntryKind::Folder {
                "フォルダ".to_string()
            } else {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| format!("{} ファイル", extension.to_ascii_uppercase()))
                    .unwrap_or_else(|| "ファイル".to_string())
            };
            Some(InfoItemProjection {
                name: entry.name.clone(),
                kind,
                path: path.clone(),
                size: (entry.kind == EntryKind::File)
                    .then_some(entry.size)
                    .flatten(),
                modified: entry.modified,
                link_target: matches!(entry.link, Some(LinkKind::LinkOrJunction))
                    .then(|| entry.link_target.clone())
                    .flatten(),
                copy_token: (!entry.link.is_some()).then(|| InfoCopyToken {
                    key: Some(entry.key.clone()),
                    selection_revision: self.selection_aggregate_revision,
                    location_generation: self.location_generation,
                    presentation_revision: self.presentation_revision,
                }),
                hash_target: (entry.kind == EntryKind::File && entry.link.is_none())
                    .then(|| {
                        Some(file_info_hash::Target {
                            key: entry.key.clone(),
                            path: path.clone(),
                            source_identity: entry.source_identity.clone()?,
                            size: entry.size?,
                            modified: entry.modified?,
                            selection_revision: self.selection_aggregate_revision,
                            location_generation: self.location_generation,
                            presentation_revision: self.presentation_revision,
                        })
                    })
                    .flatten(),
            })
        });
        let hash_target = item.as_ref().and_then(|item| item.hash_target.clone());
        let exif_target = hash_target
            .as_ref()
            .filter(|target| file_info_exif::is_candidate(&target.path))
            .map(|target| file_info_exif::Target {
                key: target.key.clone(),
                path: target.path.clone(),
                source_identity: target.source_identity.clone(),
                size: target.size,
                modified: target.modified,
                selection_revision: target.selection_revision,
                location_generation: target.location_generation,
                presentation_revision: target.presentation_revision,
                source_view_revision: 0,
            });
        let shortcut_target = hash_target
            .as_ref()
            .filter(|target| file_info_shortcut::is_candidate(&target.path))
            .map(|target| file_info_shortcut::Target {
                key: target.key.clone(),
                path: target.path.clone(),
                source_identity: target.source_identity.clone(),
                size: target.size,
                modified: target.modified,
                selection_revision: target.selection_revision,
                location_generation: target.location_generation,
                presentation_revision: target.presentation_revision,
                source_view_revision: 0,
            });
        InfoProjection {
            selected_count,
            copy_token,
            item,
            aggregate: (selected_count > 1)
                .then(|| self.selection_aggregate.clone())
                .flatten(),
            hash_target,
            exif_target,
            shortcut_target,
        }
    }

    pub(crate) fn info_hash_target(&self) -> Option<file_info_hash::Target> {
        let (selected, _, done) =
            self.selection
                .effective_indices_batch(None, 2, self.entries.len());
        if !done || selected.len() > 1 {
            return None;
        }
        let index = match selected.as_slice() {
            [index] => *index,
            [] => self.selection.focused()?,
            _ => return None,
        };
        let entry = self.entries.get(index)?;
        if entry.kind != EntryKind::File || entry.link.is_some() {
            return None;
        }
        Some(file_info_hash::Target {
            key: entry.key.clone(),
            path: entry.path.clone()?,
            source_identity: entry.source_identity.clone()?,
            size: entry.size?,
            modified: entry.modified?,
            selection_revision: self.selection_aggregate_revision,
            location_generation: self.location_generation,
            presentation_revision: self.presentation_revision,
        })
    }

    pub(crate) fn info_exif_target(&self) -> Option<file_info_exif::Target> {
        let target = self.info_hash_target()?;
        file_info_exif::is_candidate(&target.path).then(|| file_info_exif::Target {
            key: target.key,
            path: target.path,
            source_identity: target.source_identity,
            size: target.size,
            modified: target.modified,
            selection_revision: target.selection_revision,
            location_generation: target.location_generation,
            presentation_revision: target.presentation_revision,
            source_view_revision: 0,
        })
    }

    pub(crate) fn info_shortcut_target(&self) -> Option<file_info_shortcut::Target> {
        let target = self.info_hash_target()?;
        file_info_shortcut::is_candidate(&target.path).then(|| file_info_shortcut::Target {
            key: target.key,
            path: target.path,
            source_identity: target.source_identity,
            size: target.size,
            modified: target.modified,
            selection_revision: target.selection_revision,
            location_generation: target.location_generation,
            presentation_revision: target.presentation_revision,
            source_view_revision: 0,
        })
    }

    pub(crate) fn info_copy_text(
        &self,
        token: &InfoCopyToken,
        field: InfoCopyField,
    ) -> Option<String> {
        if self.location_generation != token.location_generation
            || self.selection_aggregate_revision != token.selection_revision
            || self.presentation_revision != token.presentation_revision
        {
            return None;
        }
        if let Some(key) = token.key.as_ref() {
            let (selected, _, done) = self.selection.effective_indices_batch(None, 2, self.entries.len());
            if !done || selected.len() > 1 {
                return None;
            }
            let index = match selected.as_slice() {
                [index] => *index,
                [] => self.selection.focused()?,
                _ => return None,
            };
            let entry = self.entries.get(index)?;
            if &entry.key != key || entry.path.is_none() || entry.link.is_some() {
                return None;
            }
            let path = entry.path.as_ref()?;
            return Some(match field {
                InfoCopyField::Name => entry.name.clone(),
                InfoCopyField::Location => path.to_string_lossy().into_owned(),
            });
        }

        if self.selection.len() <= 1 {
            return None;
        }
        let (mut selected, _, done) = self.selection.effective_indices_batch(
            None,
            self.entries.len().saturating_add(1),
            self.entries.len(),
        );
        if !done || selected.len() != self.selection.len() {
            return None;
        }
        selected.sort_unstable();
        let mut values = Vec::with_capacity(selected.len());
        for index in selected {
            let entry = self.entries.get(index)?;
            let path = entry.path.as_ref()?;
            values.push(match field {
                InfoCopyField::Name => path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| path.display().to_string()),
                InfoCopyField::Location => path.display().to_string(),
            });
        }
        Some(values.join("\n"))
    }

    fn emit_info_projection(&mut self, cx: &mut Context<Self>) {
        self.selection_aggregate_revision = self.selection_aggregate_revision.wrapping_add(1);
        let revision = self.selection_aggregate_revision;
        let selected_count = self.selection.len();
        if selected_count > 1 {
            self.selection_aggregate = None;
            self.selection_aggregate_work = Some(SelectionAggregateWork {
                total_size: Some(0),
                ..SelectionAggregateWork::default()
            });
            let view = cx.entity().downgrade();
            cx.spawn(async move |_this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::ZERO)
                        .await;
                    let more = view
                        .update(cx, |this, cx| {
                            if this.selection_aggregate_revision != revision {
                                return false;
                            }
                            let Some(work) = this.selection_aggregate_work.as_mut() else {
                                return false;
                            };
                            let (indices, cursor, done) = this.selection.effective_indices_batch(
                                work.cursor,
                                128,
                                this.entries.len(),
                            );
                            work.cursor = cursor;
                            for index in indices {
                                let Some(entry) = this.entries.get(index) else {
                                    continue;
                                };
                                match entry.kind {
                                    EntryKind::File => work.file_count += 1,
                                    EntryKind::Folder => work.folder_count += 1,
                                }
                                if entry.kind == EntryKind::File
                                    && entry.link.is_none()
                                    && matches!(
                                        &entry.key,
                                        entry_identity::EntryKey::Path {
                                            kind: EntryKind::File,
                                            ..
                                        }
                                    )
                                {
                                    if let Some(size) = entry.size {
                                        work.total_size = work
                                            .total_size
                                            .and_then(|total| total.checked_add(size));
                                    } else {
                                        work.unknown_size_count += 1;
                                    }
                                }
                            }
                            if done {
                                let work = this.selection_aggregate_work.take().unwrap();
                                this.selection_aggregate = Some(SelectionAggregateProjection {
                                    file_count: work.file_count,
                                    folder_count: work.folder_count,
                                    total_size: (work.unknown_size_count == 0)
                                        .then_some(work.total_size)
                                        .flatten(),
                                    unknown_size_count: work.unknown_size_count,
                                });
                                cx.emit(DirectoryEvent::SelectionProjectionChanged(
                                    this.info_projection(),
                                ));
                                false
                            } else {
                                true
                            }
                        })
                        .ok()
                        .unwrap_or(false);
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        } else {
            self.selection_aggregate_work = None;
            self.selection_aggregate = None;
        }
        cx.emit(DirectoryEvent::SelectionProjectionChanged(
            self.info_projection(),
        ));
    }

    /// availability を判定する材料。view が知っている事実だけを写す。
    ///
    /// タブ数はシェルが持つのでここでは 1 にする。タブのコマンドはシェル側の表で判定する。
    fn command_context(&self) -> commands::CommandContext {
        commands::CommandContext {
            items: self.entries.len(),
            selection: self.selection.len(),
            selection_mode: self.selection.is_selection_mode(),
            can_back: self.session.can_back(),
            can_forward: self.session.can_forward(),
            // PC の Refresh は row 列挙ではなく runtime icon identity を再観測する。
            location: self.session.location().is_some(),
            can_up: can_navigate_up(self.location().as_ref()),
            tabs: 1,
            operation_sources: self
                .clipboard_sources_for_live_selection_or_focus()
                .map_or(0, |paths| paths.len()),
            trash_sources: usize::from(self.selected_trash_sources().is_some()),
            rename_targets: usize::from(self.selected_rename_source().is_some()),
            directory: self.session.location().and_then(Location::path).is_some(),
            create_folder_target: self.session.location().and_then(Location::path).is_some()
                && self.session.directory_identity().is_some(),
        }
    }

    /// 矢印キーでフォーカスを動かし、必要ならスクロールを追従させる。
    ///
    /// 上下は行を跨ぎ、左右は隣の項目へ動く。グリッドでも行の端で止めない。
    /// 右端から右を押せば次の行の先頭へ、左端から左を押せば前の行の末尾へ流れる。
    ///
    /// Home/End は行の端ではなく一覧全体の先頭と末尾へ移る。v5 の
    /// `move_focus_home` / `move_focus_end` が `first_item_flat_index` /
    /// `last_item_flat_index` を使っていたのと同じ規則。
    ///
    /// **フォーカスが無いときの起点は index 0 で、そこから動かす。** v5 は flat index 0 を
    /// section heading（日付で整理したときの見出し）に使っていたので、見出しを選ばずに
    /// 最初の実項目へ入るにはこの置き方が要る。v6 はまだ section を持たないが規則は同じで、
    /// フォーカスが無い状態で下を押すと 2 件目へ入る（1 件目へは上か Home）。
    /// section を導入するときは `first_item_flat_index` 相当の「最初の項目」を返す helper を
    /// ここへ通すこと。
    /// **フォーカスが無いときは「一覧の先頭の手前」から入る。** 下・右でも最初の項目へ
    /// 入り、1 件目が飛ばない。v5 は読み込み時に最初の項目へフォーカスを置いていたので、
    /// そこから動かすのと同じ位置になる。
    fn move_focus(&mut self, delta: FocusDelta, extend: bool, cx: &mut Context<Self>) -> bool {
        let count = self.entries.len();
        if count == 0 {
            return false;
        }
        if !matches!(delta, FocusDelta::PageUp | FocusDelta::PageDown) {
            self.page_focus_column = None;
        }
        if matches!(delta, FocusDelta::PageUp | FocusDelta::PageDown) {
            let Some(current) = self.selection.focus_target().cloned() else {
                return false;
            };
            let Some((current_top, current_column)) = self.page_focus_position(&current) else {
                return false;
            };
            let viewport = self.scroll_handle.bounds().size.height.as_f32();
            if viewport <= 0.0 {
                return false;
            }
            let target_offset = match delta {
                FocusDelta::PageUp => (current_top - viewport).max(0.0),
                FocusDelta::PageDown => current_top + viewport,
                _ => unreachable!(),
            };
            let target_row = self
                .heights
                .row_at_offset(target_offset)
                .min(self.projected_rows.len().saturating_sub(1));
            let preferred_column = self
                .page_focus_column
                .as_ref()
                .filter(|(last_focus, _)| *last_focus == current)
                .map(|(_, column)| *column)
                .unwrap_or(current_column);
            let relative_offset = target_offset - self.heights.offset_before(target_row);
            let Some(next) = self.page_focus_target(target_row, relative_offset, preferred_column)
            else {
                return false;
            };
            if next == current {
                return false;
            }
            match &next {
                FocusTarget::Entry(index) if *index < count => {
                    self.selection
                        .apply(SelectionIntent::MoveFocus { index: *index, extend }, count);
                }
                FocusTarget::Section(key) if extend => {
                    self.selection.extend_focus_section(key.clone());
                }
                FocusTarget::Section(key) => {
                    // なぜ: 保留中の Shift 範囲を確定してから通常モードか判定する。
                    self.selection.focus_section(key.clone());
                    if !self.selection.is_selection_mode() {
                        // v5 の通常モードでは見出しへのページ移動で単一選択が空になる。
                        self.selection.apply(SelectionIntent::Clear, count);
                        self.selection.focus_section(key.clone());
                    }
                }
                _ => return false,
            }
            self.page_focus_column = Some((next.clone(), preferred_column));
            self.emit_info_projection(cx);
            self.invalidate_pending_restore_for_selection();
            match &next {
                FocusTarget::Entry(index) => self.reveal_focus(*index),
                FocusTarget::Section(key) => self.reveal_section_focus(key),
            }
            cx.emit(DirectoryEvent::QuickPreview {
                target: self.focused_quick_preview_target(),
                open: false,
            });
            cx.notify();
            return true;
        }
        if self.group_by != GroupBy::None {
            return self.move_grouped_focus(delta, extend, cx);
        }
        let columns = self.columns.max(1);
        // 範囲外のフォーカスは先頭へ倒してから判定する。
        let focused = self.selection.focused().map(|index| index.min(count - 1));
        let next = focus_target(delta, focused, count, columns);
        if Some(next) == focused && !extend {
            return false;
        }
        self.selection.apply(
            SelectionIntent::MoveFocus {
                index: next,
                extend,
            },
            count,
        );
        self.emit_info_projection(cx);
        self.invalidate_pending_restore_for_selection();
        self.reveal_focus(next);
        cx.emit(DirectoryEvent::QuickPreview {
            target: self.quick_preview_target(next),
            open: false,
        });
        cx.notify();
        true
    }

    fn page_focus_position(&self, target: &FocusTarget) -> Option<(f32, usize)> {
        match target {
            FocusTarget::Entry(index) if *index < self.entries.len() => {
                let row = self.entry_row(*index)?;
                let column = match self.projected_rows.get(row)? {
                    ProjectedRow::Entries { start, .. } => index.saturating_sub(*start),
                    ProjectedRow::CategoryStrip { groups, .. } => {
                        groups.iter().position(|group| group.index == *index)?
                    }
                    ProjectedRow::Heading { .. } => return None,
                };
                let top = self.heights.offset_before(row)
                    + if matches!(
                        self.projected_rows.get(row),
                        Some(ProjectedRow::CategoryStrip { .. })
                    ) {
                        gpui_filer_ui::CHECKBOX_HIT_SIZE
                    } else {
                        0.0
                    };
                Some((top, column))
            }
            FocusTarget::Section(key) => {
                let row = self.projected_rows.iter().position(|projected| match projected {
                    ProjectedRow::Heading { key: candidate, .. } => candidate == key,
                    ProjectedRow::CategoryStrip { groups, .. } => {
                        groups.iter().any(|group| &group.key == key)
                    }
                    ProjectedRow::Entries { .. } => false,
                })?;
                let column = match self.projected_rows.get(row)? {
                    ProjectedRow::CategoryStrip { groups, .. } => {
                        groups.iter().position(|group| &group.key == key)?
                    }
                    _ => 0,
                };
                Some((self.heights.offset_before(row), column))
            }
            _ => None,
        }
    }

    fn page_focus_target(
        &self,
        row: usize,
        relative_offset: f32,
        preferred_column: usize,
    ) -> Option<FocusTarget> {
        match self.projected_rows.get(row)? {
            ProjectedRow::Heading { key, .. } => Some(FocusTarget::Section(key.clone())),
            ProjectedRow::Entries { start, end } => {
                (*start < *end).then(|| {
                    FocusTarget::Entry(start + preferred_column.min(end - start - 1))
                })
            }
            ProjectedRow::CategoryStrip { groups, .. } => {
                let group = groups.get(preferred_column.min(groups.len().saturating_sub(1)))?;
                if relative_offset < gpui_filer_ui::CHECKBOX_HIT_SIZE {
                    Some(FocusTarget::Section(group.key.clone()))
                } else {
                    Some(FocusTarget::Entry(group.index))
                }
            }
        }
    }

    fn move_grouped_focus(&mut self, delta: FocusDelta, extend: bool, cx: &mut Context<Self>) -> bool {
        let count = self.entries.len();
        let mut stops: Vec<(FocusTarget, usize, usize)> = Vec::new();
        let mut y = 0usize;
        for row in &self.projected_rows {
            match row {
                ProjectedRow::Heading { key, .. } => {
                    stops.push((FocusTarget::Section(key.clone()), y, 0));
                    y += 1;
                }
                ProjectedRow::Entries { start, end } => {
                    for index in *start..*end {
                        stops.push((FocusTarget::Entry(index), y, index - *start));
                    }
                    y += 1;
                }
                ProjectedRow::CategoryStrip { groups, .. } => {
                    for (column, group) in groups.iter().enumerate() {
                        stops.push((FocusTarget::Section(group.key.clone()), y, column));
                        stops.push((FocusTarget::Entry(group.index), y + 1, column));
                    }
                    y += 2;
                }
            }
        }
        if stops.is_empty() {
            return false;
        }
        let current = self.selection.focus_target().cloned();
        let current_position = current.as_ref().and_then(|target| {
            let mut positions = stops.iter().enumerate().filter(|(_, (candidate, _, _))| candidate == target);
            let first = positions.next().map(|(position, _)| position);
            if positions.next().is_some() { None } else { first }
        });
        if current.is_some() && current_position.is_none() {
            if matches!(current.as_ref(), Some(FocusTarget::Section(_))) {
                self.selection.clear_section_focus();
                cx.notify();
                return true;
            }
        }
        if current.is_none() && !matches!(delta, FocusDelta::Home | FocusDelta::End) {
            let next = match delta {
                FocusDelta::Up | FocusDelta::Left => stops.len() - 1,
                FocusDelta::Down | FocusDelta::Right => 0,
                FocusDelta::PageUp | FocusDelta::PageDown => return false,
                _ => unreachable!(),
            };
            let target = stops[next].0.clone();
            match &target {
                FocusTarget::Section(key) if extend => {
                    self.selection.extend_focus_section(key.clone())
                }
                FocusTarget::Section(key) => self.selection.focus_section(key.clone()),
                FocusTarget::Entry(index) => self.selection.apply(
                    SelectionIntent::MoveFocus { index: *index, extend }, count,
                ),
            }
            self.invalidate_pending_restore_for_selection();
            self.emit_info_projection(cx);
            match &target {
                FocusTarget::Entry(index) => self.reveal_focus(*index),
                FocusTarget::Section(key) => self.reveal_section_focus(key),
            }
            cx.emit(DirectoryEvent::QuickPreview { target: self.focused_quick_preview_target(), open: false });
            cx.notify();
            return true;
        }
        let next = match delta {
            FocusDelta::Home => stops.iter().position(|(target, _, _)| matches!(target, FocusTarget::Entry(_))),
            FocusDelta::End => stops.iter().rposition(|(target, _, _)| matches!(target, FocusTarget::Entry(_))),
            FocusDelta::PageUp | FocusDelta::PageDown => None,
            FocusDelta::Left | FocusDelta::Right => {
                let current_position = current_position;
                let current_row = current_position
                    .map(|position| stops[position].1)
                    .unwrap_or(usize::MAX);
                let peer = current_position.and_then(|position| {
                    let direction = if delta == FocusDelta::Left { -1isize } else { 1 };
                    let wanted = stops[position].2 as isize + direction;
                    stops.iter().position(|(_, row, column)| *row == current_row && *column as isize == wanted)
                });
                peer.or_else(|| current_position.map(|position| {
                    if delta == FocusDelta::Left { position.saturating_sub(1) } else { (position + 1).min(stops.len() - 1) }
                })).or_else(|| (current.is_none()).then_some(0))
            }
            FocusDelta::Up | FocusDelta::Down => {
                let (row, column) = current_position
                    .map(|position| (stops[position].1, stops[position].2))
                    .unwrap_or((if delta == FocusDelta::Up { y } else { 0 }, 0));
                let candidate_row = if delta == FocusDelta::Up {
                    stops.iter().filter(|(_, candidate, _)| *candidate < row).map(|(_, candidate, _)| *candidate).max()
                } else {
                    stops.iter().filter(|(_, candidate, _)| *candidate > row).map(|(_, candidate, _)| *candidate).min()
                };
                candidate_row.and_then(|candidate| {
                    stops.iter().enumerate()
                        .filter(|(_, (_, candidate_y, _))| *candidate_y == candidate)
                        .min_by_key(|(_, (_, _, candidate_x))| candidate_x.abs_diff(column))
                        .map(|(position, _)| position)
                }).or_else(|| (current.is_none()).then_some(0))
            }
        };
        let Some(next) = next else { return false; };
        let target = stops[next].0.clone();
        if current.as_ref() == Some(&target) && !extend { return false; }
        match &target {
            FocusTarget::Section(key) if extend => {
                self.selection.extend_focus_section(key.clone())
            }
            FocusTarget::Section(key) => self.selection.focus_section(key.clone()),
            FocusTarget::Entry(index) => self.selection.apply(
                SelectionIntent::MoveFocus { index: *index, extend }, count,
            ),
        }
        self.emit_info_projection(cx);
        self.invalidate_pending_restore_for_selection();
        match &target {
            FocusTarget::Entry(index) => self.reveal_focus(*index),
            FocusTarget::Section(key) => self.reveal_section_focus(&key),
        }
        cx.emit(DirectoryEvent::QuickPreview { target: self.focused_quick_preview_target(), open: false });
        cx.notify();
        true
    }

    fn quick_preview_target(&self, index: usize) -> Option<quick_preview::Target> {
        let entry = self.entries.get(index)?;
        let path = entry.path.clone()?;
        let is_folder = entry.kind == EntryKind::Folder
            && !entry.is_link()
            && entry
                .source_identity
                .as_ref()
                .is_some_and(|identity| identity.kind == EntryKind::Folder);
        let is_text = quick_preview::is_text_preview_target(&path);
        let is_supported = thumbnails::supported(&path);
        let is_video = is_supported && thumbnails::is_video(&path);
        let is_image = is_supported && !is_video;
        let supported_file = entry.kind == EntryKind::File && !entry.is_link();
        let generic_fallback_file = supported_file
            && entry
                .source_identity
                .as_ref()
                .is_some_and(|identity| identity.kind == EntryKind::File);
        let fallback_icon = (generic_fallback_file && !is_text && !is_supported).then(|| {
            IconId::for_extension(
                path.extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default(),
            )
        });
        let is_preview_file =
            supported_file && (is_text || is_image || is_video || fallback_icon.is_some());
        if !is_folder && !is_preview_file {
            return None;
        }
        Some(quick_preview::Target {
            tab_id: 0,
            location_generation: self.location_generation,
            key: entry.key.clone(),
            path,
            source_identity: entry.source_identity.clone(),
            fallback_icon,
            epoch: thumbnails::epoch_of(entry.path.as_deref()?),
            spec: quick_preview::DecodeSpec::default(),
        })
    }

    fn focused_quick_preview_target(&self) -> Option<quick_preview::Target> {
        self.selection
            .focused()
            .and_then(|index| self.quick_preview_target(index))
    }

    /// フォーカスした行が画面外なら、見える位置までスクロールする。
    /// v5 の `scroll_to_focus` と同じく、必要なときだけ動かす。
    fn reveal_focus(&mut self, index: usize) {
        // キーボードでの移動もユーザー操作として数える。
        self.scroll_revision = self.scroll_revision.wrapping_add(1);
        let Some(row) = self.entry_row(index) else {
            return;
        };
        self.reveal_row(row);
    }

    fn reveal_section_focus(&mut self, key: &SectionKey) {
        self.scroll_revision = self.scroll_revision.wrapping_add(1);
        let mut row = None;
        for (index, projected) in self.projected_rows.iter().enumerate() {
            let matches = match projected {
                ProjectedRow::Heading { key: candidate, .. } => candidate == key,
                ProjectedRow::CategoryStrip { groups, .. } => groups.iter().any(|group| &group.key == key),
                ProjectedRow::Entries { .. } => false,
            };
            if matches {
                if row.replace(index).is_some() {
                    self.selection.clear_section_focus();
                    return;
                }
            }
        }
        if let Some(row) = row {
            self.reveal_row(row);
        } else {
            self.selection.clear_section_focus();
        }
    }

    fn reveal_row(&mut self, row: usize) {
        let viewport = self.scroll_handle.bounds().size.height.as_f32();
        if viewport <= 0.0 {
            return;
        }
        let top = self.heights.offset_before(row);
        let bottom = top + self.heights.row_height(row).map_or(44.0, |h| h.content);
        let offset = -self.scroll_handle.offset().y.as_f32();
        if top < offset {
            self.scroll_handle
                .set_offset(gpui::point(px(0.0), -px(top)));
        } else if bottom > offset + viewport {
            self.scroll_handle
                .set_offset(gpui::point(px(0.0), -px(bottom - viewport)));
        }
    }

    /// 捕捉した index が今も同じ項目を指しているか。
    ///
    /// 並べ替えや再列挙で並びが変わると、古い描画から届いた gesture の index は
    /// 別の項目を指す。今のところ revision の照合だけで足りる。
    ///
    /// key の走査は B1 の refresh への備え。同じ location で `entries` を
    /// 差し替えるようになると、revision を進め忘れた経路で「消えた key」を
    /// 拾える。ただし index がずれただけの場合は拾えないので、refresh 側で
    /// 必ず revision を進めること。
    fn gesture_still_applies(&self, revision: u64, key: Option<&entry_identity::EntryKey>) -> bool {
        if self.presentation_revision != revision {
            return false;
        }
        let Some(key) = key else {
            return false;
        };
        self.entries.iter().any(|entry| &entry.key == key)
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let viewport = self.scroll_handle.bounds().size.height.as_f32();
        let viewport = if viewport > 0.0 {
            viewport
        } else {
            INITIAL_VIEWPORT_HEIGHT
        };
        let offset = clamp_scroll_offset(
            -self.scroll_handle.offset().y.as_f32(),
            self.heights.total_height() + 44.0,
            viewport,
        );
        self.heights
            .visible_range(offset, viewport, VISIBLE_ROW_OVERSCAN)
    }

    fn item_surface(
        &self,
        index: usize,
        part: &'static str,
        content: gpui::Div,
        measuring: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if measuring {
            return content.into_any_element();
        }
        let selected = self.selection.rectangle_contains(
            index,
            self.rectangle_drag
                .as_ref()
                .and_then(|drag| self.rectangle_content_rect(drag))
                .is_some_and(|rectangle| self.item_intersects_rectangle(index, rectangle)),
        );
        // 非選択行の Native touch は Windows Direct Manipulation に渡し、一覧の pan を保つ。
        let touch_drag_selected = self.selection.contains(index);
        let generation = self.location_generation;
        // 並びが変わったら、この index を捕捉した gesture は別の項目を指す。
        // key と revision の両方で検証する。
        let revision = self.presentation_revision;
        let key = self.entries.get(index).map(|entry| entry.key.clone());
        let menu_part = match (self.layout, part) {
            (ViewLayout::Rows, "row-name") => Some(ItemMenuSurfacePart::RowName),
            (ViewLayout::Tiles | ViewLayout::Icons, "visual") => Some(ItemMenuSurfacePart::Visual),
            (ViewLayout::Tiles | ViewLayout::Icons, "name") => Some(ItemMenuSurfacePart::Name),
            _ => None,
        };
        let menu_bounds = key
            .clone()
            .zip(menu_part)
            .map(|(key, part)| (key, part, Rc::new(RefCell::new(None))));
        let grouped = matches!(part, "visual" | "name" | "row-name");
        let rendered_drag_source = self
            .entries
            .get(index)
            .and_then(internal_drag_source_for_entry);
        let drag_source_available = rendered_drag_source.is_some();
        let rendered_drop_target = self
            .entries
            .get(index)
            .and_then(|entry| internal_drop_target_for_entry(entry, generation, revision));
        let drag_view = cx.entity().entity_id();
        let drag_view_entity = cx.entity().downgrade();
        let drag_shell = self.external_drop_shell.clone();
        let drag_tab_id = self.external_drop_view_id;
        let rename_editor_active = self.rename_session.is_some();
        let drag_slot = self.internal_drag_snapshot.clone();
        // ハイライトはキーボードやタッチのあと出さない。ポインタの位置そのものは
        // `hovered_item_part` に残るので、hover を条件にした binding は当たり続ける。
        let hovered = grouped
            && self.interaction.hover_enabled()
            && self
                .hovered_item_part
                .is_some_and(|(item, _)| item == index);
        // 選択モード中はフォーカスが選択と別の場所を指す。そこを灰色で示さないと、
        // 矢印で動かしたカーソルがどこにあるか読めない。
        // 通常モードではフォーカス＝選択なので、灰色は出さない。
        let focused = grouped
            && self.selection.is_selection_mode()
            && self.selection.focused() == Some(index);
        // フォーカスが選択の上に来たら、選択を少し強くして「今ここ」を重ねて示す。
        let focused_selected = focused && selected;
        let palette = gpui_filer_ui::theme_palette(cx);
        let mut surface = content
            // 行番号ではなく項目と一覧世代を含め、gestureの接触元を固定する。
            .id(item_surface_element_id(
                part,
                index,
                key.as_ref(),
                generation,
                revision,
            ))
            .debug_selector(|| format!("item-surface-{part}-{index}"))
            .when(selected, |surface| surface.bg(palette.selection_bg))
            .when(focused_selected, |surface| {
                surface.bg(palette.selection_focus_bg)
            })
            .when(focused && !selected, |surface| surface.bg(palette.focus_bg))
            .when(hovered, |surface| {
                surface.bg(if selected {
                    palette.selection_hover_bg
                } else {
                    palette.hover_bg
                })
            })
            .hover(move |style| {
                style.bg(if selected {
                    palette.selection_hover_bg
                } else {
                    palette.hover_bg
                })
            })
            .when(grouped, |surface| {
                // カードの余白までhover扱いにせず、iconと名前の実際のhit領域だけを連動する。
                surface.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if background_input_blocked_by_batch_rename(this.batch_rename_session.is_some())
                    {
                        return;
                    }
                    if this.location_generation != generation {
                        return;
                    }
                    // タッチのあとは hover を出さない。最後の接触位置が残るため。
                    // キーボードのあとは GPUI 側が `is_hovered` を false にするので、
                    // ここでは見ない。マウスを動かせば戻る。
                    if !this.interaction.last_device().keeps_hover_routing() {
                        return;
                    }
                    let previous = this.hovered_item_part;
                    if *hovered {
                        this.hovered_item_part = Some((index, part));
                    } else if previous == Some((index, part)) {
                        this.hovered_item_part = None;
                    }
                    if previous != this.hovered_item_part {
                        cx.notify();
                    }
                }))
            })
            // marquee は空白からだけ始める。項目、checkbox、rename editor の上で始めた
            // pointer-down を親の directory-list へ通すと、click と drag が競合する。
            .on_mouse_down(gpui::MouseButton::Left, {
                let drag_slot = drag_slot.clone();
                let rendered_drag_source = rendered_drag_source.clone();
                cx.listener(move |this, _, _window, cx| {
                    cx.stop_propagation();
                    this.begin_internal_drag(
                        index,
                        rendered_drag_source.as_ref(),
                        None,
                        generation,
                        revision,
                        &drag_slot,
                    );
                })
            })
            .on_click({
                let key = key.clone();
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if this.batch_rename_session.is_some() {
                        return;
                    }
                    // 移動前の描画に届いた遅い入力を、新しい一覧へ適用しない。
                    if this.location_generation != generation {
                        return;
                    }
                    // 並びが変わっていたら、この index は別の項目を指す。
                    if !this.gesture_still_applies(revision, key.as_ref()) {
                        return;
                    }
                    // タッチなら以降の hover を止める。マウスならそのまま。
                    let touch_position = match event {
                        gpui::ClickEvent::Mouse(mouse)
                            if mouse.down.button == gpui::MouseButton::Left
                                && mouse.down.source == gpui::MouseInputSource::Touch =>
                        {
                            Some(mouse.up.position)
                        }
                        gpui::ClickEvent::Touch(touch) if !touch.long_press => Some(touch.position),
                        _ => None,
                    };
                    this.interaction.set_last_device(match event {
                        gpui::ClickEvent::Touch(_) => InputDevice::Touch,
                        gpui::ClickEvent::Mouse(mouse) => match mouse.down.source {
                            gpui::MouseInputSource::Touch => InputDevice::Touch,
                            gpui::MouseInputSource::Pen => InputDevice::Pen,
                            gpui::MouseInputSource::Mouse | gpui::MouseInputSource::Unknown => {
                                InputDevice::Mouse
                            }
                        },
                        gpui::ClickEvent::Keyboard(_) => InputDevice::Keyboard,
                    });
                    let Some(mut chord) = input::click_chord(event) else {
                        this.touch_item_tap_series = None;
                        return;
                    };
                    if let Some(position) = touch_position {
                        if let Some(key) = key.as_ref() {
                            let now = std::time::Instant::now();
                            let scroll_offset = this.scroll_handle.offset();
                            let count = this
                                .touch_item_tap_series
                                .as_ref()
                                .filter(|series| {
                                    series.matches(
                                        key,
                                        generation,
                                        revision,
                                        position,
                                        scroll_offset,
                                        now,
                                    )
                                })
                                .map_or(1, |series| series.count.saturating_add(1));
                            this.touch_item_tap_series = Some(TouchItemTapSeries {
                                key: key.clone(),
                                location_generation: generation,
                                presentation_revision: revision,
                                position,
                                scroll_offset,
                                count,
                                completed_at: now,
                            });
                            chord.count = count;
                        } else {
                            this.touch_item_tap_series = None;
                        }
                    } else {
                        // Keyboard / mouse / pen click interrupts a touch tap sequence.
                        this.touch_item_tap_series = None;
                    }
                    this.last_click_was_blank = false;
                    this.focus_handle.focus(window, cx);
                    let path = this.item_path();
                    let bindings = this.keymap_runtime.snapshot();
                    let Some(binding) = bindings.file_view.resolve_mouse(&chord, path.scopes())
                    else {
                        return;
                    };
                    let Some(intent) =
                        commands::resolve(binding.action, &this.command_context(), true)
                    else {
                        return;
                    };
                    this.run_intent(intent, Some(index), window, cx);
                })
            });
        // 中・右クリックは `ClickEvent` に来ないので、down で拾って同じ表に通す。
        // v5 の keymap も項目の中クリックを新しいタブで開く割り当てにしていた。
        //
        // 押下で確定するので、左クリック（離した時に発火）と違って並べ替えを
        // またぐことがない。revision の照合はここでは念のため。
        for button in [gpui::MouseButton::Middle, gpui::MouseButton::Right] {
            let key = key.clone();
            surface = surface.on_mouse_down(
                button,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if this.location_generation != generation {
                        return;
                    }
                    this.apply_press(event, index, revision, key.as_ref(), window, cx);
                }),
            );
        }
        if grouped {
            let tab_directory_target = rendered_drop_target.clone();
            let tab_directory_drop_target = rendered_drop_target.clone();
            let tab_directory_hover_view = cx.entity().downgrade();
            surface = surface.drag_over::<gpui_filer_ui::TabDirectoryDragPayload>(
                move |style, payload, _, cx| {
                    let Some(target) = tab_directory_target.as_ref() else {
                        return style;
                    };
                    let eligible = tab_directory_hover_view
                        .read_with(cx, |this, app| {
                            this.accepts_tab_directory_drop(payload, target, app)
                        })
                        .unwrap_or(false);
                    if eligible {
                        style
                            .border_2()
                            .border_color(gpui::hsla(0.58, 0.72, 0.62, 0.9))
                    } else {
                        style
                    }
                },
            );
            surface = surface.on_drop::<gpui_filer_ui::TabDirectoryDragPayload>(cx.listener(
                move |this, payload: &gpui_filer_ui::TabDirectoryDragPayload, _window, cx| {
                    cx.stop_propagation();
                    let Some(target) = tab_directory_drop_target.as_ref() else {
                        cx.emit(DirectoryEvent::EndTabDirectoryDrag {
                            session_id: payload.session_id(),
                        });
                        return;
                    };
                    if let Some(source_identity) =
                        this.tab_directory_drop_source_identity(payload, target, cx)
                    {
                        cx.emit(DirectoryEvent::MoveTabDirectoryInto {
                            session_id: payload.session_id(),
                            source_tab_id: payload.source_tab_id(),
                            source_identity,
                            target: target.clone(),
                        });
                    } else {
                        cx.emit(DirectoryEvent::EndTabDirectoryDrag {
                            session_id: payload.session_id(),
                        });
                    }
                },
            ));
            let payload = InternalDragPayload {
                source: cx.entity().entity_id(),
                snapshot: drag_slot.clone(),
            };
            if !rename_editor_active && drag_source_available {
                let drag_tap_series_view = drag_view_entity.clone();
                surface = surface.on_drag_with_threshold_when(
                    payload.clone(),
                    px(2.0),
                    move |event| {
                        // Nonselected Native contacts may also surface as compatibility mouse
                        // input; only a selected item can start drag from that Touch source.
                        event.source != gpui::MouseInputSource::Touch || touch_drag_selected
                    },
                    move |_, _, _, cx| {
                        // touch candidate の pointer-down ではなく、slop を越えて drag が
                        // 実際に始まった時点で、直前の tap 列を破棄する。
                        let _ = drag_tap_series_view
                            .update(cx, |this, _cx| this.touch_item_tap_series = None);
                        let snapshot = drag_slot.borrow().clone();
                        let text = snapshot
                            .as_ref()
                            .map(internal_drag_preview_text)
                            .unwrap_or_default();
                        let icons = snapshot
                            .as_ref()
                            .map(|snapshot| {
                                snapshot
                                    .sources
                                    .iter()
                                    .take(2)
                                    .map(drag_preview_icon)
                                    .collect()
                            })
                            .unwrap_or_default();
                        cx.new(|_| InternalDragPreview { text, icons })
                    },
                );
                if touch_drag_selected {
                    let touch_drag_slot = self.internal_drag_snapshot.clone();
                    let touch_drag_view = drag_view_entity.clone();
                    let touch_drag_source = rendered_drag_source.clone();
                    surface = surface.on_touch_drag_when(move |event, _window, app| {
                        touch_drag_slot.borrow_mut().take();
                        let mut accepted = false;
                        let _ = touch_drag_view.update(app, |this, _cx| {
                            // GPUI が contact を取得した後に選択状態が変わっていたら、pan へは
                            // 引き渡さず、drag snapshot だけを作らずに終了する。
                            if !this.selection.contains(index)
                                || this.touch_drag_excluded_at(event.position)
                            {
                                return;
                            }
                            this.interaction.set_last_device(InputDevice::Touch);
                            this.begin_internal_drag(
                                index,
                                touch_drag_source.as_ref(),
                                Some(event.position),
                                generation,
                                revision,
                                &touch_drag_slot,
                            );
                            if touch_drag_slot.borrow().is_some() {
                                _cx.emit(DirectoryEvent::PrimaryInteraction);
                                accepted = true;
                            }
                        });
                        accepted
                    });
                }
            }
            let drop_target = rendered_drop_target.clone();
            surface = surface.drag_over::<InternalDragPayload>(move |style, payload, _, cx| {
                let snapshot_slot = payload.snapshot.borrow();
                let Some(snapshot) = snapshot_slot.as_ref() else {
                    return style;
                };
                let Some(target) = drop_target.as_ref() else {
                    return style;
                };
                let eligible = drag_shell
                    .as_ref()
                    .zip(drag_tab_id)
                    .and_then(|(shell, target_tab_id)| {
                        shell
                            .read_with(cx, |shell, cx| {
                                shell.internal_cross_pane_move_candidate(
                                    payload.source,
                                    snapshot,
                                    target_tab_id,
                                    drag_view,
                                    target,
                                    None,
                                    None,
                                    cx,
                                )
                            })
                            .ok()
                    })
                    .unwrap_or(false);
                if eligible {
                    style
                        .border_1()
                        .border_color(gpui::hsla(0.58, 0.72, 0.62, 0.9))
                } else {
                    style
                }
            });
            surface = surface.on_drop::<InternalDragPayload>(cx.listener(
                move |this, payload, _window, cx| {
                    this.drop_internal_drag(payload, rendered_drop_target.as_ref(), cx);
                },
            ));
        }
        if grouped && !rename_editor_active {
            let long_press_key = key.clone();
            surface = surface.on_long_press(cx.listener(
                move |this, event: &gpui::LongPressEvent, window, cx| {
                    if event.phase != gpui::TouchPhase::Started {
                        return;
                    }
                    if this.touch_drag_excluded_at(event.position) {
                        return;
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    if this.location_generation != generation
                        || !this.gesture_still_applies(revision, long_press_key.as_ref())
                    {
                        return;
                    }
                    let press = gpui::MouseDownEvent {
                        source: gpui::MouseInputSource::Touch,
                        button: gpui::MouseButton::Right,
                        position: event.position,
                        ..Default::default()
                    };
                    this.apply_press(&press, index, revision, long_press_key.as_ref(), window, cx);
                },
            ));
        }
        if let Some((key, part, bounds)) = menu_bounds {
            if let Some(geometry) = self.item_menu_geometry.borrow_mut().as_mut() {
                geometry.surfaces.push(ItemMenuSurfaceBounds {
                    key,
                    part,
                    bounds: bounds.clone(),
                });
            }
            BoundsCapture {
                child: surface.into_any_element(),
                bounds,
            }
            .into_any_element()
        } else {
            surface.into_any_element()
        }
    }

    fn checkbox_surface(&self, index: usize, content: gpui::AnyElement) -> gpui::AnyElement {
        let Some(key) = self.entries.get(index).map(|entry| entry.key.clone()) else {
            return content;
        };
        let bounds = Rc::new(RefCell::new(None));
        if let Some(geometry) = self.item_menu_geometry.borrow_mut().as_mut() {
            geometry.surfaces.push(ItemMenuSurfaceBounds {
                key,
                part: ItemMenuSurfacePart::Checkbox,
                bounds: bounds.clone(),
            });
        }
        BoundsCapture {
            child: content,
            bounds,
        }
        .into_any_element()
    }

    /// 余白の押下を binding で解決する。
    ///
    /// 中・右クリックは項目の上では項目自身が受けて伝播を止めるので、ここへ来るのは余白だけ。
    /// 条件に `file_item` を含まない binding が当たる。
    fn apply_background_press(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.interaction.set_last_device(match event.source {
            gpui::MouseInputSource::Mouse | gpui::MouseInputSource::Unknown => InputDevice::Mouse,
            gpui::MouseInputSource::Touch => InputDevice::Touch,
            gpui::MouseInputSource::Pen => InputDevice::Pen,
        });
        let Some(chord) = input::press_chord(event) else {
            return;
        };
        let path = self.focus_path();
        let bindings = self.keymap_runtime.snapshot();
        let Some(binding) = bindings.file_view.resolve_mouse(&chord, path.scopes()) else {
            return;
        };
        self.focus_handle.focus(window, cx);
        let Some(intent) = commands::resolve(binding.action, &self.command_context(), false) else {
            return;
        };
        self.run_intent(intent, None, window, cx);
    }

    /// 押下（中・右クリック）の binding を解決して適用する。
    ///
    /// 左クリックは離した時に `ClickEvent` で扱うが、中・右は down でしか来ない。
    /// chord はイベントから作る。固定値で引くと keymap の割り当てが届かない。
    fn apply_press(
        &mut self,
        event: &gpui::MouseDownEvent,
        index: usize,
        revision: u64,
        key: Option<&entry_identity::EntryKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if background_input_blocked_by_batch_rename(self.batch_rename_session.is_some()) {
            cx.stop_propagation();
            return;
        }
        if !self.gesture_still_applies(revision, key) {
            return;
        }
        self.touch_item_tap_series = None;
        self.interaction.set_last_device(match event.source {
            gpui::MouseInputSource::Mouse | gpui::MouseInputSource::Unknown => InputDevice::Mouse,
            gpui::MouseInputSource::Touch => InputDevice::Touch,
            gpui::MouseInputSource::Pen => InputDevice::Pen,
        });
        self.last_click_was_blank = false;
        self.focus_handle.focus(window, cx);
        if event.button == gpui::MouseButton::Right {
            if self.selection.is_multiple() {
                // 複数選択中は、項目の上でも空白右クリックと同じ scope / intent で扱う。
                self.apply_background_press(event, window, cx);
                return;
            }
            self.apply_item(
                AppIntent::PickItem {
                    toggle: false,
                    range: false,
                },
                index,
                window,
                cx,
            );
        }
        let Some(chord) = input::press_chord(event) else {
            return;
        };
        let path = self.item_path();
        let bindings = self.keymap_runtime.snapshot();
        let Some(binding) = bindings.file_view.resolve_mouse(&chord, path.scopes()) else {
            return;
        };
        let Some(intent) = commands::resolve(binding.action, &self.command_context(), true) else {
            return;
        };
        self.run_intent(intent, Some(index), window, cx);
    }

    /// v5 の item 右下の FolderTree button。item surface の click/drag へ伝播させない。
    fn render_folder_tree_button(
        &self,
        index: usize,
        path: std::path::PathBuf,
        group: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds = Rc::new(RefCell::new(None));
        if let Some(geometry) = self.item_menu_geometry.borrow_mut().as_mut() {
            geometry.occluding_surfaces.push(bounds.clone());
        }
        let bounds_for_click = bounds.clone();
        BoundsCapture {
            bounds,
            child: div()
                .id(("folder-tree-button", index))
                .absolute()
                .right(px(4.0))
                .bottom(px(4.0))
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.0))
                .border_1()
                .border_color(gpui_filer_ui::theme_palette(cx).border)
                .bg(gpui_filer_ui::theme_palette(cx).chrome_hover)
                .opacity(if self.folder_tree_button_active.as_ref() == Some(&path) {
                    1.0
                } else {
                    0.0
                })
                .group_hover(group, |button| button.opacity(1.0))
                .hover(|button| button.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong))
                .active(|button| button.bg(gpui_filer_ui::theme_palette(cx).accent_hover))
                .child(
                    Icon::new(IconId::FolderTree)
                        .size(px(14.0))
                        .color(gpui_filer_ui::theme_palette(cx).text.into()),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.touch_item_tap_series = None;
                        this.last_click_was_blank = false;
                        let anchor = bounds_for_click.borrow().unwrap_or_else(|| {
                            Bounds::new(event.position, size(px(24.0), px(24.0)))
                        });
                        cx.emit(DirectoryEvent::OpenFolderTree {
                            path: path.clone(),
                            anchor,
                            kind: FolderTreeAnchorKind::Item,
                        });
                    }),
                )
                .into_any_element(),
        }
        .into_any_element()
    }

    fn prepare_layout(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (std::time::Duration, usize) {
        self.detect_pending_restore_scroll();
        let viewport = self.scroll_handle.bounds().size.height.as_f32();
        let width = self
            .measured_viewport_width
            .unwrap_or(self.pane_width_projection)
            .max(1.0);
        let columns = self
            .layout
            .columns(width, self.icon_scale, self.item_check_boxes);
        let offset = (-self.scroll_handle.offset().y).as_f32().max(0.0);
        if width != self.layout_width || columns != self.columns {
            let resize_anchor = self.resize_projection_anchor(offset, viewport);
            self.columns = columns;
            self.layout_width = width;
            self.rebuild_projected_rows();
            self.heights.reset(self.projected_rows.len());
            self.measured = vec![false; self.heights.len()];
            let estimate = self.layout.estimated_height(self.icon_scale);
            self.heights.set_content(0..self.heights.len(), estimate);
            self.apply_heading_height_estimates();
            // operation reconciliation 等のより強い anchor が無いときだけ、resize
            // 開始時の focus / 中央項目を同じ screen_y へ戻す。
            if self.pending_projection_anchor.is_none() {
                self.pending_projection_anchor = resize_anchor;
            }
        }
        // 通常の projection 差替えは、推定高でも先に対象行を可視範囲へ入れる。
        // rename reconciliation はここで一度動かすと、実測後にももう一度戻るため
        // 最終測定まで defer する。
        if !self.defer_projection_anchor_until_measured {
            self.apply_projection_anchor(false);
        }
        // editor の折返し行数は入力ごとに変わる。該当する仮想行だけを再測定し、
        // 後続行の absolute top と重ならないよう height tree へ反映する。
        if let Some(session) = self.rename_session.as_ref()
            && let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.key == session.key)
        {
            let Some(row) = self.entry_row(index) else {
                return (std::time::Duration::ZERO, 0);
            };
            if row < self.measured.len() {
                self.measured[row] = false;
            }
        }
        // GPUIと同じ文字整形で折り返しを測る。行全体のTaffy layoutを繰り返さない。
        // row ごとの呼出し時間を既存の opt-in 計測内でまとめ、通常描画に時計読取を足さない。
        let measure_rows_started = self.frame_profile.start();
        let mut measured_rows = 0;
        loop {
            let pending = self
                .visible_range()
                .filter(|row| !self.measured[*row])
                .collect::<Vec<_>>();
            if pending.is_empty() {
                break;
            }
            for row in pending {
                self.measure_row_height(row, width, window, cx);
                measured_rows += 1;
            }
        }
        let measure_rows_time =
            measure_rows_started.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        // 可視行の実測高を反映した同じ anchor へもう一度合わせて完了する。
        self.apply_projection_anchor(true);
        self.defer_projection_anchor_until_measured = false;
        // 測定が落ち着いてから、Back / Forward で控えた位置へ戻す。
        self.apply_pending_restore(cx);
        // resize 前の tree から anchor を採った後、最終的な実測高でだけ端を clamp する。
        if viewport > 0.0 {
            let offset = clamp_scroll_offset(
                -self.scroll_handle.offset().y.as_f32(),
                self.heights.total_height() + 44.0,
                viewport,
            );
            if (-self.scroll_handle.offset().y.as_f32() - offset).abs() > 0.01 {
                self.scroll_handle
                    .set_offset(gpui::point(px(0.0), -px(offset)));
            }
        }
        self.observed_scroll_offset = self.scroll_handle.offset();
        (measure_rows_time, measured_rows)
    }

    pub(crate) fn set_pane_width_projection(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.pane_width_projection != width {
            self.pane_width_projection = width;
            self.measured_viewport_width = None;
            cx.notify();
        }
    }

    fn measure_row_height(
        &mut self,
        row: usize,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `render_group` より先に確定した折返し高を渡す。F2 直後だけ親の
        // stretch 高で描かれて次 frame に縮む、という見た目の揺れを防ぐ。
        self.sync_rename_input_height(row, width, window, cx);
        let shaped = self.measure_group_text(row, width, window, cx);
        let contains_rename = self.rename_session.as_ref().is_some_and(|session| {
            self.row_entries(row)
                .any(|index| self.entries[index].key == session.key)
        });
        // 診断時は実要素との一致も検査する。通常時の失敗だけ従来測定へ戻す。
        static VERIFY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let verify =
            *VERIFY.get_or_init(|| std::env::var_os("GPUI_FILER_VERIFY_HEIGHTS").is_some());
        let measured = if let Some(shaped) = shaped {
            // RenameInput の最初の root layout は、custom element の幅が paint で
            // 確定する前に親の既知 height を受ける。その一回限りの 138px を
            // height tree に採用すると、内容が80pxへ収束した後も空行が残る。
            // ここでは表示と同じ text system の折返し高を authoritative にする。
            // 通常行だけは診断モードで従来どおり実レイアウトとの差を検査する。
            if !verify || contains_rename {
                shaped
            } else {
                let actual = self
                    .render_group(row, true, "measure", cx)
                    .into_any_element()
                    .layout_as_root(
                        size(
                            gpui::AvailableSpace::Definite(px(width)),
                            gpui::AvailableSpace::MaxContent,
                        ),
                        window,
                        cx,
                    )
                    .height
                    .as_f32();
                assert!(
                    (actual - shaped).abs() < 1.0,
                    "row {row}: shaped={shaped}, layout={actual}"
                );
                actual
            }
        } else {
            let actual = self
                .render_group(row, true, "measure", cx)
                .into_any_element()
                .layout_as_root(
                    size(
                        gpui::AvailableSpace::Definite(px(width)),
                        gpui::AvailableSpace::MaxContent,
                    ),
                    window,
                    cx,
                )
                .height
                .as_f32();
            actual
        };
        self.heights.set_content(row..row + 1, measured.max(44.0));
        self.measured[row] = true;
    }

    fn item_name_text_width(&self, width: f32) -> f32 {
        let cell_width =
            (width - 24.0 - 16.0 * self.columns.saturating_sub(1) as f32) / self.columns as f32;
        match self.layout {
            ViewLayout::Rows => {
                self.row_columns_for_width(width, self.effective_column_prefs())
                    .name_width
                    - CELL_PADDING_X * 2.0
                    - 20.0
                    - CELL_GAP
            }
            ViewLayout::Tiles => {
                cell_width - 44.0 - self.layout.icon_container(self.icon_scale) - 16.0 - 12.0
            }
            ViewLayout::Icons => cell_width - 12.0,
        }
        .max(1.0)
    }

    fn shaped_name_height(&self, name: &str, text_width: f32, window: &Window) -> Option<f32> {
        let style = window.text_style();
        let lines = window
            .text_system()
            .shape_text(
                name.into(),
                px(NAME_FONT_SIZE),
                &[style.to_run(name.len())],
                Some(px(text_width)),
                None,
            )
            .ok()?;
        Some(
            lines
                .iter()
                .map(|line| line.size(px(NAME_LINE_HEIGHT)).height.as_f32())
                .sum(),
        )
    }

    fn sync_rename_input_height(
        &self,
        row: usize,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.rename_session.as_ref().and_then(|session| {
            self.entries
                .iter()
                .position(|entry| entry.key == session.key)
                .filter(|index| self.entry_row(*index) == Some(row))
                .map(|_| session.input.clone())
        }) else {
            return;
        };
        let name = input.read(cx).text().to_owned();
        let Some(height) = self.shaped_name_height(&name, self.item_name_text_width(width), window)
        else {
            return;
        };
        input.update(cx, |input, _| input.set_expected_height(px(height)));
    }

    fn measure_group_text(&self, row: usize, width: f32, window: &Window, cx: &App) -> Option<f32> {
        let text_width = self.item_name_text_width(width);
        let mut height: f32 = 44.0;
        for index in self.row_entries(row) {
            let entry = &self.entries[index];
            let rename_name = self
                .rename_session
                .as_ref()
                .filter(|session| session.key == entry.key)
                .map(|session| session.input.read(cx).text().to_owned());
            let name = rename_name.as_deref().unwrap_or(&entry.name);
            let text_height = self.shaped_name_height(name, text_width, window)?;
            let label = (text_height + CELL_PADDING_Y * 2.0).max(44.0);
            let container = self.layout.icon_container(self.icon_scale);
            let item = match self.layout {
                ViewLayout::Rows => label,
                ViewLayout::Tiles => label.max(container) + 16.0,
                ViewLayout::Icons => container + 8.0 + label + 16.0,
            };
            height = height.max(item);
        }
        Some(height)
    }
}

fn resolve_shell_folder_alias(
    input: &str,
    user_profile: Option<&std::path::Path>,
    app_data: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let moniker = input
        .trim()
        .trim_end_matches(|character| character == '/' || character == '\\')
        .to_ascii_lowercase();
    let user_profile = user_profile?;
    match moniker.as_str() {
        "shell:desktop" => Some(user_profile.join("Desktop")),
        "shell:downloads" => Some(user_profile.join("Downloads")),
        "shell:personal" | "shell:mydocuments" | "shell:documentslibrary" => {
            Some(user_profile.join("Documents"))
        }
        "shell:profile" | "shell:home" => Some(user_profile.to_path_buf()),
        "shell:startup" => Some(
            app_data?
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup"),
        ),
        _ => None,
    }
}

fn is_pc_address_alias(input: &str) -> bool {
    let normalized = input.trim().trim_end_matches(['\\', '/']);
    ["shell:mycomputerfolder", "pc", "this pc"]
        .iter()
        .any(|alias| normalized.eq_ignore_ascii_case(alias))
}

/// 監視の配送先。adapter は `WeakEntity` だけを持ち、閉じたタブの view を
/// 生かし続けない。
impl directory_watch::WatchOwner for DirectoryListDemo {
    fn watch_ready(&mut self, token: u64, cx: &mut Context<Self>) {
        if let Some(effect) = self.session.watch_ready(token) {
            self.run_effect(effect, cx);
        }
        // 登録できたので、変更を拾うタイマーを回し始める。
        self.watch.start_debounce(cx);
        cx.notify();
    }

    fn watch_failed(&mut self, token: u64, reason: String, cx: &mut Context<Self>) {
        self.session.watch_failed(token, reason);
        cx.notify();
    }

    fn watch_degraded(&mut self, token: u64, reason: String, cx: &mut Context<Self>) {
        self.session.watch_degraded(token, reason);
        cx.notify();
    }

    fn watch_dirty(
        &mut self,
        token: u64,
        location_epoch: u64,
        hint: directory_session::WatchHint,
        cx: &mut Context<Self>,
    ) {
        // 古い watcher event は media epoch も進めない。
        if log_watch_dirty() {
            eprintln!("[v6-watch] hint token={token} epoch={location_epoch} {hint:?}");
        }
        if !self
            .session
            .watch_dirty(token, location_epoch, hint.clone())
        {
            return;
        }
        match &hint {
            directory_session::WatchHint::All => self.invalidate_known_media(),
            directory_session::WatchHint::Paths(paths) => thumbnails::invalidate(paths),
        }
        cx.emit(DirectoryEvent::QuickPreview {
            target: self.focused_quick_preview_target(),
            open: false,
        });
        // debounce を待たずに読む。F5 と同じ入口。
        if let Some(effect) = self.session.take_follow_up() {
            self.run_effect(effect, cx);
        }
        cx.notify();
    }

    fn watch_flush(&mut self, cx: &mut Context<Self>) {
        self.watch.flush(cx);
    }
}

impl DirectoryListDemo {
    pub(crate) fn batch_rename_dialog_active(&self) -> bool {
        self.batch_rename_session.is_some()
    }

    /// 監視をやめる。タブを閉じるときに呼ぶ。
    ///
    /// `notify` の callback は専用 thread で走るので、unwatch の後にも通知が
    /// 届き得る。session が token で落とすので、ここでは登録を外すだけでよい。
    pub fn shutdown_watch(&mut self) {
        self.watch.shutdown();
    }
}

impl Render for DirectoryListDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // blur は v5 と同じく入力を破棄する。location 更新では閉じないので、編集中に
        // 非同期の表示名/一覧更新が来ても文字列と selection を保てる。
        if self
            .address_edit
            .as_ref()
            .is_some_and(|session| !session.input.read(cx).is_focused(window))
        {
            self.address_edit = None;
        }
        let focus_path = self.focus_path();
        let text_input_focused = self.text_input_has_focus(window, cx);
        self.interaction.set_focused(
            Some(focus_path),
            if text_input_focused {
                FocusKind::TextInput
            } else {
                FocusKind::Regular
            },
        );
        let hover_path = self.hover_path();
        self.interaction.set_hovered(hover_path);
        // GPUI clears the platform-owned drag before repainting Exited/Ended. This also forgets
        // a cancelled session when the next external drag reuses the same path payload.
        if !cx.has_active_drag() {
            take_external_drag_session(&self.external_drag_session);
        }
        let started = self.frame_profile.start();
        // 起動時の読取要求を一度だけ拾う。監視の登録もここで一緒に始まる。
        for effect in self.session.take_startup_effects() {
            self.run_effect(effect, cx);
        }
        // PC の行は背景で解決される。届いていればここで写す。
        if self.is_this_pc() {
            self.apply_pc_rows(window, cx);
        }
        let measure_started = self.frame_profile.start();
        let (measure_rows_time, measured_rows) = self.prepare_layout(window, cx);
        self.cancel_stale_rectangle_drag(cx);
        self.capture_rectangle_surfaces();
        self.item_menu_geometry = Rc::new(RefCell::new(Some(ItemMenuGeometry {
            generation: self.location_generation,
            presentation_revision: self.presentation_revision,
            layout: self.layout,
            columns: self.columns,
            layout_width: self.layout_width,
            scroll_revision: self.scroll_revision,
            scroll_offset: self.scroll_handle.offset(),
            viewport: self.scroll_handle.bounds(),
            surfaces: Vec::new(),
            occluding_surfaces: Vec::new(),
        })));
        let layout_time =
            measure_started.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        let previews_started = self.frame_profile.start();
        let preview_size = preview_pixels(self.layout, self.icon_scale, window.scale_factor());
        self.prepare_previews(preview_size, window, cx);
        let previews_time =
            previews_started.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        let rows_started = self.frame_profile.start();
        let foreground: Hsla = gpui_filer_ui::theme_palette(cx).text.into();
        let range = self.visible_range();
        let rendered_rows = range.len();
        // v5 と同じく、全体の高さだけを持つ canvas に可視行だけを絶対配置する。
        // 前後の巨大な spacer を flex の子にすると、指スクロールで offset が変わるたび
        // に flow 内の全 child を組み直すことになる。可変行高は `heights` が持つので、
        // scroll content の高さと各行の top をそこから導けばよい。
        let background_target = self.external_background_drop_target();
        let background_hover_target = background_target.clone();
        let background_external_drop_target = background_target.clone();
        let background_drag_session = self.external_drag_session.clone();
        let background_ole_shell = self.external_drop_shell.clone();
        let background_tab_id = self.external_drop_view_id;
        let drag_location_generation = self.location_generation;
        let drag_presentation_revision = self.presentation_revision;
        let view_entity = cx.entity().downgrade();
        let drop_view_entity = view_entity.clone();
        let drag_view_entity_id = cx.entity().entity_id();
        let mut rows = div()
            .relative()
            .w_full()
            .h(px(self.heights.total_height() + 44.0))
            .min_h_full()
            .flex_shrink_0();
        rows = rows
            .drag_over::<gpui::ExternalPaths>(move |style, payload, window, cx| {
                capture_external_drag_session(
                    &background_drag_session,
                    payload,
                    drag_location_generation,
                    drag_presentation_revision,
                );
                let in_viewport = view_entity
                    .read_with(cx, |this, _| {
                        this.is_blank_list_viewport_drop(window.mouse_position())
                    })
                    .unwrap_or(false);
                let local_token = window
                    .file_drop_context()
                    .and_then(|context| context.local_drag_session);
                let eligible = in_viewport
                        && !payload.paths().is_empty()
                        && background_hover_target.as_ref().is_some_and(|target| {
                            let target_current = view_entity
                                .read_with(cx, |this, _| {
                                    (if local_token.is_some() {
                                        this.accepts_internal_background_target(target)
                                    } else {
                                        this.accepts_external_drop_target(target)
                                    })
                                        && (local_token.is_some()
                                            || external_drag_session_matches(
                                                background_drag_session.borrow().as_ref(),
                                                payload,
                                                target,
                                            ))
                                })
                                .unwrap_or(false);
                            if let (Some(token), Some(shell), Some(tab_id)) = (
                                local_token,
                                background_ole_shell.as_ref(),
                                background_tab_id,
                            ) {
                                let source_projection = view_entity
                                    .read_with(cx, |this, _| {
                                        this.current_internal_drag_source_projection()
                                    })
                                    .ok()
                                    .flatten();
                                let effect = shell
                                    .read_with(cx, |shell, cx| {
                                        shell.local_ole_move_candidate_effect(
                                            token.value(),
                                            tab_id,
                                            drag_view_entity_id,
                                            target,
                                            payload.paths(),
                                            Some(target_current),
                                            source_projection,
                                            cx,
                                        )
                                    })
                                    .ok()
                                    .flatten();
                                if let Some(effect) = effect {
                                    if let Some(context) = window.file_drop_context() {
                                        context.response.set_effect(match effect {
                                            shell::LocalOleDropEffect::Copy => {
                                                gpui::FileDropEffect::Copy
                                            }
                                            shell::LocalOleDropEffect::Move => {
                                                gpui::FileDropEffect::Move
                                            }
                                        });
                                    }
                                }
                                external_background_hover_is_eligible(true, effect.is_some())
                            } else {
                                external_background_hover_is_eligible(false, target_current)
                            }
                        });
                if eligible
                    && in_viewport
                    && local_token.is_none()
                    && let Some(target) = background_hover_target.as_ref()
                {
                    let paths = payload.paths().to_vec();
                    let target = target.clone();
                    let _ = view_entity.update(cx, |this, cx| {
                        this.prepare_external_drop(&paths, &target, cx);
                    });
                }
                if eligible {
                    style
                        .border_1()
                        .border_color(gpui::hsla(0.58, 0.25, 0.62, 0.4))
                } else {
                    style
                }
            })
            .on_drop::<gpui::ExternalPaths>(move |_payload, window, cx| {
                let trace_context = window.file_drop_context();
                if let Some(context) = trace_context.as_ref() {
                    reject_foreign_link_by_default(context);
                }
                ole_diagnostics::record(ole_diagnostics::Event::FileDrop {
                    local: trace_context
                        .as_ref()
                        .is_some_and(|context| context.local_drag_session.is_some()),
                    source_count: trace_context
                        .as_ref()
                        .and_then(|context| context.paths.as_ref())
                        .map_or(0, |paths| paths.paths().len()),
                    target: ole_diagnostics::TargetClass::Background,
                });
                // Shell admission が保持中の drag route 経由で view を読み直すため、先に entity lease を解放する。
                let prepared_inputs = drop_view_entity
                    .update(cx, |this, _| {
                        this.external_file_drop_paths.borrow_mut().take();
                        let session = take_external_drag_session(&this.external_drag_session);
                        let in_viewport = this.is_blank_list_viewport_drop(window.mouse_position());
                        let (Some(target), Some(context), Some(shell), Some(view_id)) = (
                            background_external_drop_target.as_ref(),
                            trace_context.as_ref(),
                            this.external_drop_shell.as_ref(),
                            this.external_drop_view_id,
                        ) else {
                            return None;
                        };
                        let Some(drop_paths) = context.paths.as_ref().filter(|paths| {
                            !paths.paths().is_empty()
                                && (context.local_drag_session.is_some()
                                    || external_drag_session_matches(
                                        session.as_ref(),
                                        paths,
                                        target,
                                    ))
                        }) else {
                            return None;
                        };
                        if !in_viewport || !this.accepts_external_drop_target(target) {
                            return None;
                        }
                        let paths = drop_paths.paths().to_vec();
                        let prepared =
                            prepared_external_drop_from_session(session.as_ref(), &paths, target);
                        Some((
                            paths,
                            prepared,
                            target.clone(),
                            shell.clone(),
                            view_id,
                            this.accepts_internal_background_target(target),
                            this.current_internal_drag_source_projection(),
                        ))
                    })
                    .ok()
                    .flatten();
                if let (
                    Some(context),
                    Some((
                        paths,
                        prepared,
                        target,
                        shell,
                        view_id,
                        target_current,
                        source_projection,
                    )),
                ) = (trace_context.as_ref(), prepared_inputs)
                {
                    let source_view_entity_id = drag_view_entity_id;
                    let response = context.response.clone();
                    if let Some(local_token) = context.local_drag_session {
                        let effect = shell
                            .update(cx, |shell, cx| {
                                shell.admit_local_ole_move(
                                    local_token.value(),
                                    view_id,
                                    source_view_entity_id,
                                    target,
                                    paths,
                                    target_current,
                                    source_projection,
                                    cx,
                                )
                            })
                            .ok()
                            .flatten();
                        if let Some(effect) = effect {
                            response.set_effect(match effect {
                                shell::LocalOleDropEffect::Copy => gpui::FileDropEffect::Copy,
                                shell::LocalOleDropEffect::Move => gpui::FileDropEffect::Move,
                            });
                        }
                    } else {
                        let intent = foreign_file_drop_intent(&context);
                        let (admitted, effect) = shell
                            .update(cx, |shell, cx| match intent {
                                input::ForeignFileDropIntent::Link if context.link_allowed => {
                                    context.response.set_effect(gpui::FileDropEffect::None);
                                    (
                                        shell.admit_external_symbolic_link(
                                            view_id,
                                            source_view_entity_id,
                                            target,
                                            paths,
                                            cx,
                                        ),
                                        gpui::FileDropEffect::Link,
                                    )
                                }
                                input::ForeignFileDropIntent::Link => {
                                    context.response.set_effect(gpui::FileDropEffect::None);
                                    (false, gpui::FileDropEffect::None)
                                }
                                input::ForeignFileDropIntent::Move => {
                                    let Some(feedback) = context
                                        .transfer_lease
                                        .as_ref()
                                        .and_then(gpui::FileDropTransferLease::start)
                                    else {
                                        return (false, gpui::FileDropEffect::None);
                                    };
                                    (
                                        shell.admit_external_copy(
                                            view_id,
                                            source_view_entity_id,
                                            target,
                                            paths,
                                            false,
                                            Some(feedback),
                                            cx,
                                        ),
                                        gpui::FileDropEffect::Copy,
                                    )
                                }
                                input::ForeignFileDropIntent::Default => {
                                    if let Some(prepared) =
                                        prepared.filter(|prepared| prepared.same_volume)
                                        && let Some(feedback) = context
                                            .transfer_lease
                                            .as_ref()
                                            .and_then(gpui::FileDropTransferLease::start)
                                    {
                                        (
                                            shell.admit_external_prepared_move(
                                                view_id,
                                                source_view_entity_id,
                                                target,
                                                paths,
                                                prepared,
                                                feedback,
                                                cx,
                                            ),
                                            gpui::FileDropEffect::Copy,
                                        )
                                    } else {
                                        (
                                            shell.admit_external_copy(
                                                view_id,
                                                source_view_entity_id,
                                                target,
                                                paths,
                                                false,
                                                None,
                                                cx,
                                            ),
                                            gpui::FileDropEffect::Copy,
                                        )
                                    }
                                }
                                input::ForeignFileDropIntent::Copy => (
                                    shell.admit_external_copy(
                                        view_id,
                                        source_view_entity_id,
                                        target,
                                        paths,
                                        false,
                                        None,
                                        cx,
                                    ),
                                    gpui::FileDropEffect::Copy,
                                ),
                            })
                            .unwrap_or((false, gpui::FileDropEffect::None));
                        if admitted {
                            response.set_effect(effect);
                        }
                    }
                }
                // Scrollbar and overlay hits cannot become a background copy.
                cx.stop_propagation();
            });
        let internal_background_drop_target = background_target.clone();
        rows = rows.on_drop::<InternalDragPayload>(cx.listener(
            move |this, payload: &InternalDragPayload, window, cx| {
                if this.is_blank_list_viewport_drop(window.mouse_position()) {
                    this.drop_internal_background_drag(
                        payload,
                        internal_background_drop_target.as_ref(),
                        cx,
                    );
                } else {
                    emit_internal_drag_drop_cancel(payload, cx);
                    let _ = take_internal_drag_snapshot(&payload.snapshot);
                    cx.stop_propagation();
                }
            },
        ));
        rows = rows.on_drop::<gpui_filer_ui::TabDirectoryDragPayload>(cx.listener(
            |_, payload: &gpui_filer_ui::TabDirectoryDragPayload, _, cx| {
                cx.emit(DirectoryEvent::EndTabDirectoryDrag {
                    session_id: payload.session_id(),
                });
                cx.stop_propagation();
            },
        ));
        for index in range {
            rows = rows.child(
                div()
                    .absolute()
                    .top(px(self.heights.offset_before(index)))
                    .left_0()
                    .right_0()
                    .child(self.render_group_with_preview_size(
                        index,
                        false,
                        "row",
                        Some(preview_size),
                        window.scale_factor(),
                        cx,
                    )),
            );
        }
        let rows_time = rows_started.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        let scroll_surface_started = self.frame_profile.start();
        let scroll_surface = self.render_scroll_area(rows, cx);
        let scroll_surface_time =
            scroll_surface_started.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        let width_reconcile_view = cx.entity().downgrade();
        let width_reconcile_scheduled = self.viewport_reconcile_scheduled.clone();
        let rendered_layout_width = self.layout_width;
        let list_viewport = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_children_prepainted(move |child_bounds: Vec<Bounds<Pixels>>, window, _app| {
                let Some(viewport) = child_bounds.first() else {
                    return;
                };
                let width = viewport.size.width.as_f32();
                if width <= 0.0
                    || !width.is_finite()
                    || (rendered_layout_width - width).abs() <= 0.5
                    || width_reconcile_scheduled.replace(true)
                {
                    return;
                }
                let view = width_reconcile_view.clone();
                let scheduled = width_reconcile_scheduled.clone();
                window.on_next_frame(move |_, cx| {
                    scheduled.set(false);
                    let _ = view.update(cx, |this, cx| {
                        let measured = this.scroll_handle.bounds().size.width.as_f32();
                        if measured <= 0.0 || !measured.is_finite() {
                            return;
                        }
                        let changed = this
                            .measured_viewport_width
                            .is_none_or(|previous| (previous - measured).abs() > 0.5);
                        this.measured_viewport_width = Some(measured);
                        if changed && (this.layout_width - measured).abs() > 0.5 {
                            cx.notify();
                        }
                    });
                });
            })
            .child(scroll_surface);

        let quick_search_input = self
            .quick_search
            .as_ref()
            .map(|session| session.input.clone());
        let palette = gpui_filer_ui::theme_palette(cx);
        let rendered = div()
            .track_focus(&self.focus_handle)
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                if event.button == gpui::MouseButton::Left {
                    this.row_column_reorder_suppress_click = false;
                    this.row_column_reorder_down = None;
                    cx.emit(DirectoryEvent::PrimaryInteraction);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.cancel_row_column_reorder();
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.cancel_row_column_reorder();
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.touch_item_tap_series = None;
                if this.handle_key(&event.keystroke, window, cx) {
                    cx.stop_propagation();
                }
            }))
            // ポインタが動いたら hover を戻す。キーボードやタッチのあとの抑止は
            // 「マウスが動くまで」で解ける。ここが無いとクリックするまで戻らない。
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _window, cx| {
                if this.interaction.last_device() != InputDevice::Mouse {
                    this.interaction.set_last_device(InputDevice::Mouse);
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(gpui_filer_ui::theme_palette(cx).surface_base)
            .text_color(foreground)
            .child(self.render_breadcrumbs(window, cx))
            .when_some(self.render_listing_status(cx), |root, status| {
                root.child(status)
            })
            .child(
                div()
                    .min_h(px(52.0))
                    .px(px(12.0))
                    .py(px(4.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(CHROME_FONT_SIZE))
                    .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                    .child(div().flex_1().min_w(px(120.0)).overflow_hidden().child(
                        self.translator.list_status(
                            self.entries.len(),
                            self.selection.len(),
                            self.selection.is_selection_mode(),
                        ),
                    ))
                    .child(self.render_layout_switch(cx))
                    .child(self.render_group_switch(cx).flex_shrink_0())
                    .when(self.layout != ViewLayout::Rows, |bar| {
                        bar.child(self.render_icon_scale(cx).flex_shrink_0())
                    })
                    .when(self.selection.len() > 0, |bar| bar.child({
                        div()
                            .id("clear-selection")
                            .min_h(px(44.0))
                            .max_w(px(168.0))
                            .flex_shrink_0()
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .hover(|s| s.bg(gpui_filer_ui::theme_palette(cx).border))
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_center()
                                    .child(
                                        self.translator.text(Message::ClearSelection).into_owned(),
                                    ),
                            )
                            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _window, cx| {
                                if !event.standard_click() {
                                    return;
                                }
                                // 描画から時間が経っている。実行の直前にもう一度見る。
                                if !commands::available(
                                    ActionId::new(input::EXIT_SELECTION),
                                    &this.command_context(),
                                    false,
                                ) {
                                    return;
                                }
                                this.exit_selection(cx);
                            }))
                    })),
            )
            .when(self.layout == ViewLayout::Rows, |root| {
                root.child(rows_layout::rows_header(
                    foreground,
                    &self.translator,
                    self.sort,
                    self.row_columns_for_width(self.layout_width, self.effective_column_prefs()),
                    self.effective_column_prefs(),
                    cx,
                ))
            })
            .child(list_viewport)
            .when_some(quick_search_input, |root, input| {
                root.child(
                    div()
                        .absolute()
                        .top(px(8.0))
                        .right(px(14.0))
                        .w(px(300.0))
                        .p(px(6.0))
                        .bg(palette.surface_raised)
                        .border_1()
                        .border_color(palette.border)
                        .rounded(px(6.0))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                if let Some(session) = this.quick_search.as_ref() {
                                    session
                                        .input
                                        .update(cx, |input, cx| input.focus(window, cx));
                                }
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_down(gpui::MouseButton::Middle, |_, _, cx| {
                            cx.stop_propagation()
                        })
                        .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| {
                            cx.stop_propagation()
                        })
                        .child(div().min_h(px(44.0)).child(input)),
                )
            })
            .when_some(self.render_batch_rename_dialog(cx), |root, dialog| {
                root.child(dialog)
            });
        self.frame_profile.finish(
            started,
            rendered_rows,
            diagnostics::FramePhases {
                layout: layout_time,
                measure_rows: measure_rows_time,
                measured_rows,
                previews: previews_time,
                rows: rows_time,
                scroll_surface: scroll_surface_time,
            },
        );
        rendered
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use std::cell::Cell;

    struct InlineRenameRouteHarness {
        shell: gpui::Entity<shell::FilerShell>,
        undo_events: std::rc::Rc<Cell<usize>>,
        subscription: Option<gpui::Subscription>,
    }

    impl gpui::Render for InlineRenameRouteHarness {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            if self.subscription.is_none() {
                let view = self
                    .shell
                    .read(cx)
                    .active_view()
                    .expect("Shell has an active directory view");
                let undo_events = self.undo_events.clone();
                self.subscription = Some(cx.subscribe(
                    &view,
                    move |_, _, event: &DirectoryEvent, _| {
                        if matches!(event, DirectoryEvent::UndoFileOperation) {
                            undo_events.set(undo_events.get() + 1);
                        }
                    },
                ));
            }
            self.shell.clone()
        }
    }

    struct ClipboardFailureRouteHarness {
        shell: gpui::Entity<shell::FilerShell>,
    }

    impl gpui::Render for ClipboardFailureRouteHarness {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.shell.clone()
        }
    }

    #[test]
    fn clipboard_owner_startup_failure_parent_reaps_isolated_children() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        for mode in ["current", "stale"] {
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--ignored")
                .arg("--exact")
                .arg("layout_tests::clipboard_owner_startup_failure_child")
                .arg("--nocapture")
                .env("GPUI_FILER_TEST_CLIPBOARD_STARTUP_FAILURE", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn isolated clipboard test child");
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().expect("poll clipboard child") {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("clipboard child timed out in {mode} mode");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let output = child.wait_with_output().expect("reap clipboard child output");
            assert!(
                status.success(),
                "clipboard child failed in {mode} mode: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("[v6][clipboard] write failed: injected clipboard owner window creation failure"),
                "Shell did not report the injected startup error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[gpui::test]
    #[ignore = "runs only as a fresh child of clipboard_owner_startup_failure_parent_reaps_isolated_children"]
    fn clipboard_owner_startup_failure_child(cx: &mut gpui::TestAppContext) {
        use std::time::{Duration, Instant};

        assert!(
            namespace_clipboard::owner_is_uninitialized_for_test(),
            "child must start before the process-global clipboard owner is initialized"
        );
        let mode = std::env::var("GPUI_FILER_TEST_CLIPBOARD_STARTUP_FAILURE")
            .expect("parent supplies the isolated failure mode");
        assert!(mode == "current" || mode == "stale");
        initialize_shell_test_globals(cx);
        let (root, visual) = cx.add_window_view(move |_, cx| {
            let shell = cx.new(|cx| {
                shell::FilerShell::new(
                    persistence::Startup::disabled(),
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Vec::new(),
                    cx,
                )
            });
            ClipboardFailureRouteHarness { shell }
        });
        visual.run_until_parked();
        let shell = visual.read_entity(&root, |root, _| root.shell.clone());
        let view = visual
            .read_entity(&shell, |shell, _| shell.active_view())
            .expect("Shell owns its active directory view");

        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("cut-source.bin");
        let source_bytes = b"clipboard startup failure leaves source unchanged";
        std::fs::write(&source, source_bytes).unwrap();
        let source_identity =
            directory::EntrySourceIdentity::from_path(&source, EntryKind::File).unwrap();
        visual.update_entity(&view, |view, cx| {
            view.navigate(source_dir.path().to_path_buf(), cx);
        });
        visual.run_until_parked();
        let source_text = source.to_string_lossy().into_owned();
        let entry = drag_test_entry(Some(&source_text), EntryKind::File, None);
        visual.update_entity(&view, |view, _| {
            view.entries = vec![entry];
            view.rebuild_row_details();
            view.selection.apply(
                SelectionIntent::Pick { index: 0, toggle: false, range: false },
                view.entries.len(),
            );
            view.reset_projected_rows();
        });
        let (captured_generation, captured_revision) =
            visual.read_entity(&view, |view, _| {
                (view.location_generation, view.presentation_revision)
            });
        visual.update(|window, app| {
            view.update(app, |view, cx| {
                assert!(view.dispatch_intent_for_test(AppIntent::Cut, None, window, cx));
            });
        });
        if mode == "current" {
            visual.run_until_parked();
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        if mode == "stale" {
            let replacement_dir = tempfile::tempdir().unwrap();
            let replacement = replacement_dir.path().to_path_buf();
            let marker = replacement.join("newer-view-cut-marker");
            visual.update_entity(&view, |view, cx| view.navigate(replacement, cx));
            visual.update_entity(&view, |view, _| {
                view.cut_paths.insert(marker.clone());
            });
            let (new_generation, new_revision) = visual.read_entity(&view, |view, _| {
                (view.location_generation, view.presentation_revision)
            });
            assert!(
                new_generation != captured_generation || new_revision != captured_revision,
                "navigation must invalidate the captured clipboard origin"
            );
            wait_for_clipboard_result(visual, &view, deadline);
            visual.read_entity(&view, |view, _| {
                assert_eq!(
                    view.clipboard_write_result_for_test,
                    Some((false, false, captured_generation, captured_revision))
                );
                assert_eq!(view.cut_paths, [marker].into_iter().collect());
            });
        } else {
            wait_for_clipboard_result(visual, &view, deadline);
            visual.read_entity(&view, |view, _| {
                assert_eq!(
                    view.clipboard_write_result_for_test,
                    Some((false, true, captured_generation, captured_revision))
                );
                assert!(view.cut_paths.is_empty(), "failed Cut must not show a Cut mark");
            });
        }
        assert_eq!(namespace_clipboard::file_write_command_count_for_test(), 0);
        let (jobs, undo_entries) = visual.update(|_, cx| {
            let owner = cx.global::<operations::application::OperationApplicationOwner>();
            (owner.runtime.jobs(), owner.runtime.undo_snapshot_for_test().1)
        });
        assert!(jobs.is_empty(), "clipboard failure must not submit a file operation");
        assert!(undo_entries.is_empty(), "clipboard failure must not create file-operation receipts");
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            directory::EntrySourceIdentity::from_path(&source, EntryKind::File).unwrap(),
            source_identity
        );
    }

    fn wait_for_clipboard_result(
        visual: &gpui::VisualTestContext,
        view: &gpui::Entity<DirectoryListDemo>,
        deadline: std::time::Instant,
    ) {
        loop {
            visual.run_until_parked();
            if visual.read_entity(view, |view, _| view.clipboard_write_result_for_test.is_some()) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "Shell did not deliver clipboard failure");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn shell_folder_aliases_match_v5_paths() {
        let root = std::env::temp_dir().join("shell-alias-fixture");
        let profile = root.join("profile");
        let app_data = profile.join("AppData").join("Roaming");
        let cases = [
            ("shell:desktop", profile.join("Desktop")),
            ("shell:downloads", profile.join("Downloads")),
            ("shell:personal", profile.join("Documents")),
            ("shell:mydocuments", profile.join("Documents")),
            ("shell:documentslibrary", profile.join("Documents")),
            ("shell:profile", profile.clone()),
            ("shell:home", profile.clone()),
            (
                "shell:startup",
                app_data
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs")
                    .join("Startup"),
            ),
        ];
        for (alias, expected) in cases {
            assert_eq!(
                resolve_shell_folder_alias(alias, Some(&profile), Some(&app_data)),
                Some(expected),
                "{alias}"
            );
        }
    }

    #[test]
    fn shell_folder_aliases_ignore_case_and_trailing_separators() {
        let profile = std::env::temp_dir().join("shell-alias-profile");
        for alias in [
            "SHELL:DOWNLOADS/",
            "Shell:Downloads\\",
            "shell:downloads/\\",
        ] {
            assert_eq!(
                resolve_shell_folder_alias(alias, Some(&profile), None),
                Some(profile.join("Downloads")),
                "{alias}"
            );
        }
    }

    #[test]
    fn unknown_or_unresolvable_shell_aliases_remain_unresolved() {
        let profile = std::env::temp_dir().join("shell-alias-profile");
        assert_eq!(
            resolve_shell_folder_alias("shell:unknown", Some(&profile), None),
            None
        );
        for alias in [
            "shell:desktop",
            "shell:downloads",
            "shell:personal",
            "shell:mydocuments",
            "shell:documentslibrary",
            "shell:profile",
            "shell:home",
        ] {
            assert_eq!(
                resolve_shell_folder_alias(alias, None, Some(&profile)),
                None,
                "{alias}"
            );
        }
        assert_eq!(
            resolve_shell_folder_alias("shell:startup", Some(&profile), None),
            None
        );
        let app_data = std::env::temp_dir().join("shell-alias-appdata");
        assert_eq!(
            resolve_shell_folder_alias("shell:startup", None, Some(&app_data)),
            None,
            "USERPROFILE is required even when APPDATA is available"
        );
    }

    #[test]
    fn shell_folder_aliases_trim_surrounding_whitespace() {
        let profile = std::env::temp_dir().join("shell-alias-profile");
        assert_eq!(
            resolve_shell_folder_alias("  shell:downloads  ", Some(&profile), None),
            Some(profile.join("Downloads"))
        );
    }

    #[cfg(windows)]
    #[gpui::test]
    fn address_editor_submission_resolves_alias_and_preserves_unknown_path_failure(
        cx: &mut gpui::TestAppContext,
    ) {
        let profile = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .expect("Windows test process has USERPROFILE");
        let initial = tempfile::tempdir().unwrap();
        let initial_path = initial.path().to_path_buf();
        let (view, visual) = cx.add_window_view(move |_, cx| {
            DirectoryListDemo::new_at(
                std::sync::Arc::new(input::KeymapRuntime::new()),
                Some(Location::Path(initial_path)),
                cx,
            )
        });

        visual.run_until_parked();
        assert!(submit_address_edit_text(
            &view,
            visual,
            "shell:profile"
        ));
        visual.read_entity(&view, |view, _| {
            assert_eq!(
                view.session.location(),
                Some(&Location::Path(profile.clone()))
            );
        });

        assert!(submit_address_edit_text(
            &view,
            visual,
            "shell:unknown"
        ));
        visual.read_entity(&view, |view, _| {
            assert_eq!(
                view.session.location(),
                Some(&Location::Path(std::path::PathBuf::from("shell:unknown")))
            );
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            visual.run_until_parked();
            if visual.read_entity(&view, |view, _| {
                matches!(
                    view.session.status(),
                    directory_session::ListingStatus::Unavailable(_)
                )
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "unknown shell alias did not follow asynchronous path failure"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        visual.read_entity(&view, |view, _| {
            assert!(view.session.status().error().is_some());
        });
    }

    #[cfg(windows)]
    fn submit_address_edit_text(
        view: &gpui::Entity<DirectoryListDemo>,
        visual: &mut gpui::VisualTestContext,
        text: &str,
    ) -> bool {
        let focused = Cell::new(false);
        visual.update(|window, app| {
            view.update(app, |view, cx| {
                let input = cx.new(|cx| rename_editor::RenameInput::address_bar(cx, text.into()));
                let input_events = cx.subscribe(&input, |_this, _, event, cx| match event {
                    rename_editor::RenameInputEvent::TextChanged => cx.notify(),
                    rename_editor::RenameInputEvent::CopyPlainText(_) => {}
                });
                input.update(cx, |input, cx| input.focus(window, cx));
                view.address_edit = Some(AddressEditSession {
                    input,
                    _input_events: input_events,
                });
                view.commit_address_edit(window, cx);
                focused.set(view.is_focused_for_test(window));
            });
        });
        focused.get()
    }

    #[test]
    fn batch_rename_modal_blocks_background_actions() {
        assert!(!background_input_blocked_by_batch_rename(false));
        assert!(background_input_blocked_by_batch_rename(true));
    }

    fn touch_geometry_test_projection() -> (ItemMenuGeometry, Bounds<Pixels>) {
        let viewport = Bounds::new(gpui::point(px(10.0), px(20.0)), size(px(300.0), px(200.0)));
        let occluder = Bounds::new(gpui::point(px(40.0), px(50.0)), size(px(30.0), px(24.0)));
        (
            ItemMenuGeometry {
                generation: 7,
                presentation_revision: 11,
                layout: ViewLayout::Rows,
                columns: 1,
                layout_width: 300.0,
                scroll_revision: 3,
                scroll_offset: gpui::point(px(0.0), px(-80.0)),
                viewport,
                surfaces: Vec::new(),
                occluding_surfaces: vec![Rc::new(RefCell::new(Some(occluder)))],
            },
            viewport,
        )
    }

    #[test]
    fn touch_drag_geometry_allows_scroll_only_staleness_but_keeps_projection_guards() {
        let (geometry, viewport) = touch_geometry_test_projection();
        // Native Direct Manipulation can move the list after this frame was painted.
        // 右クリックと同じ完全一致では、scroll-only 更新でも stale と判定する。
        assert!(!geometry.matches_snapshot(
            7,
            11,
            ViewLayout::Rows,
            1,
            300.0,
            viewport,
            4,
            gpui::point(px(0.0), px(-81.0)),
        ));
        // Touch drag はこの scroll-only 差分を許し、遮蔽物外なら候補を保つ。
        assert!(!geometry.excludes_touch_drag_at(
            gpui::point(px(100.0), px(100.0)),
            7,
            11,
            ViewLayout::Rows,
            1,
            300.0,
            viewport,
        ));
        assert!(geometry.excludes_touch_drag_at(
            gpui::point(px(50.0), px(60.0)),
            7,
            11,
            ViewLayout::Rows,
            1,
            300.0,
            viewport,
        ));
        assert!(geometry.excludes_touch_drag_at(
            gpui::point(px(100.0), px(100.0)),
            8,
            11,
            ViewLayout::Rows,
            1,
            300.0,
            viewport,
        ));
        let stale_viewport =
            Bounds::new(gpui::point(px(11.0), px(20.0)), size(px(300.0), px(200.0)));
        assert!(geometry.excludes_touch_drag_at(
            gpui::point(px(100.0), px(100.0)),
            7,
            11,
            ViewLayout::Rows,
            1,
            300.0,
            stale_viewport,
        ));
        assert!(geometry.excludes_touch_drag_at(
            gpui::point(px(100.0), px(100.0)),
            7,
            11,
            ViewLayout::Rows,
            2,
            300.0,
            viewport,
        ));
        for (revision, layout, width) in [
            (12, ViewLayout::Rows, 300.0),
            (11, ViewLayout::Icons, 300.0),
            (11, ViewLayout::Rows, 301.0),
        ] {
            assert!(geometry.excludes_touch_drag_at(
                gpui::point(px(100.0), px(100.0)),
                7,
                revision,
                layout,
                1,
                width,
                viewport,
            ));
        }
    }

    #[test]
    fn item_menu_full_snapshot_match_remains_scroll_sensitive() {
        let (geometry, viewport) = touch_geometry_test_projection();
        let matches = |scroll_revision, scroll_offset| {
            geometry.matches_snapshot(
                7,
                11,
                ViewLayout::Rows,
                1,
                300.0,
                viewport,
                scroll_revision,
                scroll_offset,
            )
        };
        assert!(matches(3, gpui::point(px(0.0), px(-80.0))));
        assert!(!matches(4, gpui::point(px(0.0), px(-80.0))));
        assert!(!matches(3, gpui::point(px(0.0), px(-81.0))));
    }

    #[test]
    fn local_file_drop_can_show_background_move_hover_cue() {
        assert!(external_background_hover_is_eligible(true, true));
        assert!(external_background_hover_is_eligible(false, true));
        assert!(!external_background_hover_is_eligible(false, false));
    }

    struct MouseUpOutHarness {
        release_count: Rc<Cell<usize>>,
    }

    struct PointerDownCaptureHarness {
        left_down_count: Rc<Cell<usize>>,
    }

    impl Render for MouseUpOutHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let release_count = self.release_count.clone();
            div().w(px(100.0)).h(px(100.0)).on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(move |_, _, _, _| release_count.set(release_count.get() + 1)),
            )
        }
    }

    impl Render for PointerDownCaptureHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let left_down_count = self.left_down_count.clone();
            div()
                .w(px(100.0))
                .h(px(100.0))
                .capture_any_mouse_down(cx.listener(
                    move |_, event: &gpui::MouseDownEvent, _, _| {
                        if event.button == gpui::MouseButton::Left {
                            left_down_count.set(left_down_count.get() + 1);
                        }
                    },
                ))
        }
    }

    struct EscapeCancelHarness {
        view: gpui::Entity<DirectoryListDemo>,
        cancel_count: Rc<Cell<usize>>,
        subscription: Option<Subscription>,
    }

    impl Render for EscapeCancelHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.subscription.is_none() {
                let cancel_count = self.cancel_count.clone();
                self.subscription = Some(cx.subscribe(
                    &self.view,
                    move |_this, _view, event: &DirectoryEvent, _cx| {
                        if matches!(event, DirectoryEvent::CancelNativeDrag { .. }) {
                            cancel_count.set(cancel_count.get() + 1);
                        }
                    },
                ));
            }
            self.view.clone()
        }
    }

    #[gpui::test]
    fn mouse_up_out_receives_the_original_release_outside_the_element(
        cx: &mut gpui::TestAppContext,
    ) {
        let release_count = Rc::new(Cell::new(0));
        let (harness, visual) = cx.add_window_view({
            let release_count = release_count.clone();
            move |_, _| MouseUpOutHarness { release_count }
        });
        let modifiers = gpui::Modifiers::default();
        visual.simulate_mouse_down(
            gpui::point(px(40.0), px(40.0)),
            gpui::MouseButton::Left,
            modifiers,
        );
        visual.simulate_mouse_move(
            gpui::point(px(150.0), px(40.0)),
            Some(gpui::MouseButton::Left),
            modifiers,
        );
        visual.simulate_mouse_up(
            gpui::point(px(150.0), px(40.0)),
            gpui::MouseButton::Left,
            modifiers,
        );
        visual.read_entity(&harness, |_, _| assert_eq!(release_count.get(), 1));
    }

    #[gpui::test]
    fn capture_any_mouse_down_receives_a_new_contact_on_blank_space(cx: &mut gpui::TestAppContext) {
        let left_down_count = Rc::new(Cell::new(0));
        let (harness, visual) = cx.add_window_view({
            let left_down_count = left_down_count.clone();
            move |_, _| PointerDownCaptureHarness { left_down_count }
        });
        visual.simulate_mouse_down(
            gpui::point(px(80.0), px(80.0)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        visual.read_entity(&harness, |_, _| assert_eq!(left_down_count.get(), 1));
    }

    #[gpui::test]
    fn escape_emits_native_drag_cancel_before_view_consumes_it(cx: &mut gpui::TestAppContext) {
        let cancel_count = Rc::new(Cell::new(0));
        let (harness, visual) = cx.add_window_view({
            let cancel_count = cancel_count.clone();
            move |_, cx| EscapeCancelHarness {
                view: cx.new(|cx| {
                    DirectoryListDemo::new_at(
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        None,
                        cx,
                    )
                }),
                cancel_count,
                subscription: None,
            }
        });
        let view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        visual.update(|window, app| {
            view.update(app, |view, cx| view.focus_handle.focus(window, cx));
        });
        visual.simulate_keystrokes("escape");
        visual.read_entity(&harness, |_, _| assert_eq!(cancel_count.get(), 1));
    }

    struct CreateFolderKeyHarness {
        view: gpui::Entity<DirectoryListDemo>,
        events: Rc<
            RefCell<
                Vec<(
                    std::path::PathBuf,
                    filesystem_identity::FilesystemIdentity,
                    u64,
                    u64,
                )>,
            >,
        >,
        subscription: Option<Subscription>,
    }

    impl Render for CreateFolderKeyHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.subscription.is_none() {
                let events = self.events.clone();
                self.subscription = Some(cx.subscribe(
                    &self.view,
                    move |_this, _view, event: &DirectoryEvent, _cx| {
                        if let DirectoryEvent::CreateFolderInto {
                            destination,
                            destination_identity,
                            location_epoch,
                            presentation_revision,
                        } = event
                        {
                            events.borrow_mut().push((
                                destination.clone(),
                                destination_identity.clone(),
                                *location_epoch,
                                *presentation_revision,
                            ));
                        }
                    },
                ));
            }
            self.view.clone()
        }
    }

    #[gpui::test]
    fn ctrl_shift_n_from_directory_list_emits_captured_create_folder_target(
        cx: &mut gpui::TestAppContext,
    ) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_path_buf();
        let events = Rc::new(RefCell::new(Vec::new()));
        let (harness, visual) = cx.add_window_view({
            let events = events.clone();
            let path = path.clone();
            move |_, cx| CreateFolderKeyHarness {
                view: cx.new(|cx| {
                    DirectoryListDemo::new_at(
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        Some(Location::Path(path)),
                        cx,
                    )
                }),
                events,
                subscription: None,
            }
        });
        let view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        visual.run_until_parked();
        let (identity, epoch, revision) = visual.read_entity(&view, |view, _| {
            (
                view.session
                    .directory_identity()
                    .cloned()
                    .expect("accepted directory identity"),
                view.location_generation,
                view.presentation_revision,
            )
        });
        visual.update(|window, app| {
            view.update(app, |view, cx| view.focus_handle.focus(window, cx));
        });
        visual.simulate_keystrokes("ctrl-shift-n");
        assert_eq!(
            events.borrow().as_slice(),
            &[(path, identity, epoch, revision)],
            "the real DirectoryList key route emits one immutable parent target"
        );
    }

    #[gpui::test]
    fn background_create_folder_target_rejects_navigation_and_identity_change(
        cx: &mut gpui::TestAppContext,
    ) {
        let original = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let original_path = original.path().to_path_buf();
        let destination_path = destination.path().to_path_buf();
        let (view, visual) = cx.add_window_view({
            let path = original_path.clone();
            move |_, cx| {
                DirectoryListDemo::new_at(
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Some(Location::Path(path)),
                    cx,
                )
            }
        });
        visual.run_until_parked();
        let (identity, epoch, revision) = visual.read_entity(&view, |view, _| {
            (
                view.session
                    .directory_identity()
                    .cloned()
                    .expect("accepted original directory identity"),
                view.location_generation,
                view.presentation_revision,
            )
        });
        assert!(visual.read_entity(&view, |view, _| {
            view.accepts_background_paste_target(&original_path, &identity, epoch, revision)
        }));

        visual.update(|_, app| {
            view.update(app, |view, cx| view.navigate(destination_path.clone(), cx));
        });
        visual.run_until_parked();
        assert!(!visual.read_entity(&view, |view, _| {
            view.accepts_background_paste_target(&original_path, &identity, epoch, revision)
        }));
        assert!(!visual.read_entity(&view, |view, _| {
            view.accepts_background_paste_target(
                &destination_path,
                &identity,
                view.location_generation,
                view.presentation_revision,
            )
        }));
    }

    struct KeyPathHarness {
        view: gpui::Entity<DirectoryListDemo>,
        preview_events: Rc<RefCell<Vec<(Option<quick_preview::Target>, bool)>>>,
        subscription: Option<Subscription>,
    }

    impl Render for KeyPathHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.subscription.is_none() {
                let preview_events = self.preview_events.clone();
                self.subscription = Some(cx.subscribe(
                    &self.view,
                    move |_this, _view, event: &DirectoryEvent, _cx| {
                        if let DirectoryEvent::QuickPreview { target, open } = event {
                            preview_events.borrow_mut().push((target.clone(), *open));
                        }
                    },
                ));
            }
            div().size_full().child(self.view.clone())
        }
    }

    fn hidden_visibility_window(
        cx: &mut gpui::TestAppContext,
        path_prefix: &str,
    ) -> (gpui::Entity<KeyPathHarness>, gpui::AnyWindowHandle) {
        let (harness, visual) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| {
                DirectoryListDemo::new_at(
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    None,
                    cx,
                )
            });
            let mut hidden = drag_test_entry(
                Some(&format!("C:/{path_prefix}/hidden.txt")),
                EntryKind::File,
                None,
            );
            hidden.is_hidden = true;
            let ordinary = drag_test_entry(
                Some(&format!("C:/{path_prefix}/ordinary.txt")),
                EntryKind::File,
                None,
            );
            let entries = vec![hidden, ordinary];
            view.update(cx, |view, _| {
                view.entries = entries.clone();
                view.rebuild_row_details();
                view.session.adopt_demo(std::sync::Arc::new(entries));
            });
            KeyPathHarness {
                view,
                preview_events: Rc::new(RefCell::new(Vec::new())),
                subscription: None,
            }
        });
        let view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        visual.update_entity(&view, |view, cx| {
            view.selection.apply(
                SelectionIntent::SetRange {
                    start: 0,
                    end: 1,
                    selected: true,
                },
                view.entries.len(),
            );
            cx.notify();
        });
        let window = gpui::VisualContext::window_handle(visual);
        (harness, window)
    }

    #[gpui::test]
    fn app_global_hidden_toggle_reprojects_views_in_two_windows(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::show_hidden_items::ShowHiddenItems(true));
        let (first_harness, first_window_handle) = hidden_visibility_window(cx, "hidden-first");
        let (second_harness, second_window_handle) = hidden_visibility_window(cx, "hidden-second");
        let first_window = gpui::VisualTestContext::from_window(first_window_handle, cx);
        let second_window = gpui::VisualTestContext::from_window(second_window_handle, cx);
        let first_view =
            first_window.read_entity(&first_harness, |harness, _| harness.view.clone());
        let second_view =
            second_window.read_entity(&second_harness, |harness, _| harness.view.clone());

        for (window, view) in [(&first_window, &first_view), (&second_window, &second_view)] {
            window.read_entity(view, |view, _| {
                assert_eq!(view.entries.len(), 2);
                assert!(view.entries[0].is_hidden);
                assert_eq!(
                    (0..view.entries.len())
                        .filter(|index| view.selection.contains(*index))
                        .collect::<Vec<_>>(),
                    vec![0]
                );
            });
        }

        cx.set_global(crate::show_hidden_items::ShowHiddenItems(false));
        first_window.run_until_parked();
        second_window.run_until_parked();
        for (window, view) in [(&first_window, &first_view), (&second_window, &second_view)] {
            window.read_entity(view, |view, _| {
                assert_eq!(view.entries.len(), 1);
                assert!(!view.entries[0].is_hidden);
                assert!((0..view.entries.len()).all(|index| !view.selection.contains(index)));
                assert_eq!(view.selection.focused(), None);
            });
        }
    }

    #[gpui::test]
    fn hidden_item_check_boxes_remove_only_their_hit_surface_in_each_layout(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.set_global(crate::ItemCheckBoxes(false));
        let (harness, window_handle) = hidden_visibility_window(cx, "checkbox-geometry");
        let mut window = gpui::VisualTestContext::from_window(window_handle, cx);
        window.run_until_parked();
        let view = window.read_entity(&harness, |harness, _| harness.view.clone());

        for layout in [ViewLayout::Rows, ViewLayout::Tiles, ViewLayout::Icons] {
            window.update_entity(&view, |view, _| {
                view.layout = layout;
                view.layout_width = view.scroll_handle.bounds().size.width.as_f32().max(320.0);
                view.columns = layout.columns(view.layout_width, view.icon_scale, false);
                view.reset_projected_rows();
                let measured_height = layout.estimated_height(view.icon_scale) + 64.0;
                view.heights.set_content(0..1, measured_height);
            });
            window.read_entity(&view, |view, _| {
                let rects = view.item_hit_rects(0);
                let expected = if layout == ViewLayout::Rows { 1 } else { 2 };
                assert_eq!(rects.len(), expected, "layout={layout:?}");
                assert!(
                    rects
                        .iter()
                        .any(|rect| rect.width > 0.0 && rect.height > 0.0)
                );
            });

            window.update_entity(&view, |view, _| {
                view.item_check_boxes = true;
                view.columns = layout.columns(view.layout_width, view.icon_scale, true);
                view.reset_projected_rows();
                let measured_height = layout.estimated_height(view.icon_scale) + 64.0;
                view.heights.set_content(0..1, measured_height);
            });
            window.read_entity(&view, |view, _| {
                let rects = view.item_hit_rects(0);
                let expected = if layout == ViewLayout::Rows { 2 } else { 3 };
                assert_eq!(rects.len(), expected, "layout={layout:?}");
            });
            window.update_entity(&view, |view, _| view.item_check_boxes = false);
        }
    }

    #[gpui::test]
    fn stale_item_checkbox_contact_is_ignored_across_visibility_transitions(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.set_global(crate::ItemCheckBoxes(true));
        let (harness, window_handle) = hidden_visibility_window(cx, "checkbox-stale-contact");
        let mut window = gpui::VisualTestContext::from_window(window_handle, cx);
        window.run_until_parked();
        let view = window.read_entity(&harness, |harness, _| harness.view.clone());
        macro_rules! checkbox_point {
            () => {{
                let bounds = window.debug_bounds("item-surface-row-name-0").unwrap();
                gpui::point(
                    bounds.origin.x - px(26.0),
                    bounds.origin.y + bounds.size.height / 2.0,
                )
            }};
        }
        let modifiers = gpui::Modifiers::default();

        // An already-painted checkbox release must not commit after the global toggle,
        // even when GPUI has not painted the new visibility state yet.
        let point = checkbox_point!();
        window.simulate_mouse_down(point, gpui::MouseButton::Left, modifiers);
        cx.set_global(crate::ItemCheckBoxes(false));
        window.simulate_mouse_up(point, gpui::MouseButton::Left, modifiers);
        window.read_entity(&view, |view, _| assert!(view.selection.contains(0)));

        // The same stale contact remains harmless after the hidden state is painted.
        cx.set_global(crate::ItemCheckBoxes(true));
        window.run_until_parked();
        let point = checkbox_point!();
        window.simulate_mouse_down(point, gpui::MouseButton::Left, modifiers);
        cx.set_global(crate::ItemCheckBoxes(false));
        window.run_until_parked();
        window.simulate_mouse_up(point, gpui::MouseButton::Left, modifiers);
        window.read_entity(&view, |view, _| assert!(view.selection.contains(0)));

        // Hiding and showing before release still invalidates the original contact.
        cx.set_global(crate::ItemCheckBoxes(true));
        window.run_until_parked();
        let point = checkbox_point!();
        window.simulate_mouse_down(point, gpui::MouseButton::Left, modifiers);
        cx.set_global(crate::ItemCheckBoxes(false));
        cx.set_global(crate::ItemCheckBoxes(true));
        window.simulate_mouse_up(point, gpui::MouseButton::Left, modifiers);
        window.read_entity(&view, |view, _| assert!(view.selection.contains(0)));

        // Repaint after hide→show must replace the checkbox element identity, so
        // GPUI cannot complete the original pending down on the new element.
        window.simulate_mouse_down(point, gpui::MouseButton::Left, modifiers);
        cx.set_global(crate::ItemCheckBoxes(false));
        cx.set_global(crate::ItemCheckBoxes(true));
        window.run_until_parked();
        window.simulate_mouse_up(point, gpui::MouseButton::Left, modifiers);
        window.read_entity(&view, |view, _| assert!(view.selection.contains(0)));
    }

    #[gpui::test]
    fn item_check_box_setting_projects_to_views_in_multiple_windows(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::ItemCheckBoxes::default());
        let (first_harness, first_window_handle) = hidden_visibility_window(cx, "checkbox-first");
        let (second_harness, second_window_handle) =
            hidden_visibility_window(cx, "checkbox-second");
        let first_window = gpui::VisualTestContext::from_window(first_window_handle, cx);
        let second_window = gpui::VisualTestContext::from_window(second_window_handle, cx);
        let first_view =
            first_window.read_entity(&first_harness, |harness, _| harness.view.clone());
        let second_view =
            second_window.read_entity(&second_harness, |harness, _| harness.view.clone());

        for (window, view) in [(&first_window, &first_view), (&second_window, &second_view)] {
            window.read_entity(view, |view, _| assert!(view.item_check_boxes));
        }
        cx.set_global(crate::ItemCheckBoxes(false));
        first_window.run_until_parked();
        second_window.run_until_parked();
        for (window, view) in [(&first_window, &first_view), (&second_window, &second_view)] {
            window.read_entity(view, |view, _| assert!(!view.item_check_boxes));
        }
    }

    #[cfg(windows)]
    #[gpui::test]
    #[ignore = "slow Windows filesystem and GPUI focus route regression"]
    fn copy_file_undo_waits_for_inline_rename_text_undo(cx: &mut gpui::TestAppContext) {
        initialize_shell_test_globals(cx);
        cx.update(|app| {
            app.bind_keys([
                gpui::KeyBinding::new("ctrl-a", rename_editor::SelectAll, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-v", rename_editor::PasteText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-z", rename_editor::UndoText, Some("RenameInput")),
            ]);
        });

        let undo_events = std::rc::Rc::new(Cell::new(0));
        let undo_events_for_root = undo_events.clone();
        let (root, visual) = cx.add_window_view(move |_, cx| {
            let shell = cx.new(|cx| {
                shell::FilerShell::new(
                    persistence::Startup::disabled(),
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Vec::new(),
                    cx,
                )
            });
            InlineRenameRouteHarness {
                shell,
                undo_events: undo_events_for_root,
                subscription: None,
            }
        });
        visual.run_until_parked();
        let shell = visual.read_entity(&root, |root, _| root.shell.clone());
        let view = visual
            .read_entity(&shell, |shell, _| shell.active_view())
            .expect("Shell has an active directory view");

        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = temp.path().join("copy-source.bin");
        let destination = temp.path().join("destination");
        let copied = destination.join("copy-source.bin");
        let sibling = destination.join("sibling.bin");
        let bytes = b"copy undo remains available after text undo";
        let sibling_bytes = b"sibling remains untouched";
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, bytes).unwrap();
        std::fs::write(&sibling, sibling_bytes).unwrap();
        let sibling_identity =
            directory::EntrySourceIdentity::from_path(&sibling, EntryKind::File).unwrap();
        let (source_identity, copied_identity, original_attempts) = {
            let request = operations::model::OperationRequest {
                kind: operations::model::OperationKind::Copy,
                virtual_source: None,
                sources: vec![source.clone()],
                trash_source_guards: None,
                destination: Some(destination.clone()),
                batch_rename_plan: None,
                move_identity_guard: None,
                clipboard_move_source_evidence: None,
                rename_reparse_guard: None,
                move_destination_identity_guard: None,
                move_replace_source_evidence: None,
                source_cleanup_identity_guard: None,
                initial_conflict_policy: None,
                copy_destination_identity_guard: None,
                create_parent_directory_identity_guard: None,
                zip_create_guard: None,
                shortcut_create_guard: None,
                seven_zip_create_guard: None,
                seven_zip_extract_guard: None,
                origin: None,
            };
            let (token, worker) = visual.update(|_, cx| {
                let owner = cx.global_mut::<operations::application::OperationApplicationOwner>();
                let token = owner.runtime.submit(request).expect("Copy is admitted");
                (
                    token,
                    owner.runtime.start_next().expect("Copy worker starts"),
                )
            });
            let mut facts = Vec::new();
            operations::executor::execute_prepare(
                worker.command,
                worker.cancellation.as_ref(),
                &worker.controls,
                |fact| facts.push(fact),
            );
            let accepted_terminal = facts
                .iter()
                .find_map(|fact| match fact {
                    operations::executor::ExecutorFact::Terminal {
                        token: actual,
                        report,
                    } if *actual == token => Some(report.clone()),
                    _ => None,
                })
                .expect("executor returns the exact Copy terminal");
            assert_eq!(
                accepted_terminal.outcome,
                operations::model::TerminalOutcome::Succeeded
            );
            assert!(accepted_terminal.receipts.iter().any(|receipt| {
                receipt.outcome == operations::model::ItemOutcome::Completed
                    && receipt
                        .copy_undo
                        .as_ref()
                        .is_some_and(|receipt| receipt.destination == copied)
            }));
            visual.update(|_, cx| {
                let runtime = &mut cx
                    .global_mut::<operations::application::OperationApplicationOwner>()
                    .runtime;
                let committed: Vec<_> = facts
                    .into_iter()
                    .filter_map(|fact| runtime.commit_fact(fact))
                    .collect();
                assert_eq!(
                    committed.len(),
                    1,
                    "the exact Copy terminal is accepted by Runtime"
                );
                let job = runtime.job(token).expect("Copy job remains observable");
                let attempt = job
                    .attempts
                    .iter()
                    .find(|attempt| attempt.token == token)
                    .unwrap();
                assert_eq!(attempt.state, operations::model::AttemptState::Terminal);
                assert_eq!(attempt.terminal.as_ref(), Some(&accepted_terminal));
                let entries = runtime.undo_snapshot_for_test().1;
                assert_eq!(
                    entries.len(),
                    1,
                    "Copy receipt occupies the initial undo slot"
                );
                assert!(matches!(
                    entries.first(),
                    Some(operations::runtime::UndoEntry::Copy(_))
                ));
                (
                    directory::EntrySourceIdentity::from_path(&source, EntryKind::File).unwrap(),
                    directory::EntrySourceIdentity::from_path(&copied, EntryKind::File).unwrap(),
                    job.attempts.clone(),
                )
            })
        };
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read(&copied).unwrap(), bytes);
        let (reserved_before, original_receipts) = visual.update(|_, cx| {
            cx.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .undo_snapshot_for_test()
        });
        assert!(!reserved_before);
        assert_eq!(original_receipts.len(), 1);
        let original_copy_receipt = match original_receipts.as_slice() {
            [operations::runtime::UndoEntry::Copy(receipt)] => receipt.clone(),
            _ => panic!("the completed Copy occupies the initial undo slot"),
        };

        visual.update_entity(&view, |view, cx| {
            view.navigate(destination.clone(), cx);
        });
        visual.run_until_parked();
        let copied_key = entry_identity::EntryKey::path(copied.clone(), EntryKind::File);
        let (baseline_scan, location_epoch, copied_index) = visual.read_entity(&view, |view, _| {
            assert_eq!(
                view.session.location(),
                Some(&Location::Path(destination.clone()))
            );
            assert_eq!(
                view.session.status(),
                &directory_session::ListingStatus::Ready
            );
            let scan = view
                .session
                .accepted_scan_ticket()
                .expect("destination has an accepted filesystem enumeration")
                .clone();
            assert_eq!(scan.path, destination);
            assert_eq!(scan.location_epoch, view.session.location_epoch());
            let listing = view
                .session
                .listing()
                .expect("accepted listing is available");
            assert!(listing.iter().any(|entry| {
                entry.key == copied_key
                    && entry.path.as_ref() == Some(&copied)
                    && entry.name == "copy-source.bin"
            }));
            assert!(listing.iter().any(|entry| {
                entry.key == entry_identity::EntryKey::path(sibling.clone(), EntryKind::File)
                    && entry.path.as_ref() == Some(&sibling)
                    && entry.name == "sibling.bin"
            }));
            assert!(view.entries.iter().any(|entry| {
                entry.key == copied_key
                    && entry.path.as_ref() == Some(&copied)
                    && entry.name == "copy-source.bin"
            }));
            let index = view
                .entries
                .iter()
                .position(|entry| entry.key == copied_key)
                .expect("copied output is projected in the active view");
            (scan, view.session.location_epoch(), index)
        });
        visual.update_entity(&view, |view, _| {
            view.selection.apply(
                SelectionIntent::Pick {
                    index: copied_index,
                    toggle: false,
                    range: false,
                },
                view.entries.len(),
            );
        });
        visual.update(|window, app| {
            view.update(app, |view, cx| {
                view.begin_rename(copied.clone(), false, window, cx)
            })
        });
        visual.run_until_parked();
        let rename_input = visual.read_entity(&view, |view, _| {
            view.rename_session
                .as_ref()
                .expect("inline rename is open")
                .input
                .clone()
        });
        visual.update(|window, app| {
            assert!(
                rename_input.read(app).is_focused(window),
                "RenameInput owns keyboard focus"
            );
            app.write_to_clipboard(gpui::ClipboardItem::new_string("edited.bin".to_owned()));
        });
        visual.simulate_keystrokes("ctrl-a");
        visual.simulate_keystrokes("ctrl-v");
        visual.run_until_parked();
        visual.read_entity(&rename_input, |input, _| {
            assert_eq!(input.text(), "edited.bin")
        });

        visual.simulate_keystrokes("ctrl-z");
        visual.run_until_parked();
        visual.read_entity(&rename_input, |input, _| {
            assert_eq!(input.text(), "copy-source.bin")
        });
        assert_eq!(undo_events.get(), 0, "text Ctrl+Z emits no file undo event");
        let (reserved_during_editor, receipts_during_editor) = visual.update(|_, cx| {
            cx.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .undo_snapshot_for_test()
        });
        assert!(!reserved_during_editor);
        assert_eq!(receipts_during_editor.len(), 1);
        let editor_copy_receipt = match receipts_during_editor.as_slice() {
            [operations::runtime::UndoEntry::Copy(receipt)] => receipt,
            _ => panic!("text undo preserves the original Copy receipt slot"),
        };
        assert_eq!(editor_copy_receipt, &original_copy_receipt);
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(
            directory::EntrySourceIdentity::from_path(&source, EntryKind::File).unwrap(),
            source_identity
        );
        assert_eq!(std::fs::read(&copied).unwrap(), bytes);
        assert_eq!(
            directory::EntrySourceIdentity::from_path(&copied, EntryKind::File).unwrap(),
            copied_identity
        );
        let attempts_during_editor = visual.update(|_, cx| {
            cx.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .job(original_attempts[0].token)
                .unwrap()
                .attempts
                .clone()
        });
        assert_eq!(attempts_during_editor, original_attempts);

        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        assert!(visual.read_entity(&view, |view, _| view.rename_session.is_none()));
        visual.update(|window, app| {
            assert!(
                view.read(app).is_focused_for_test(window),
                "directory view regains focus"
            );
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.run_until_parked();
        assert_eq!(
            undo_events.get(),
            1,
            "file Ctrl+Z is dispatched exactly once"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            visual.run_until_parked();
            let (reserved, receipts) = visual.update(|_, cx| {
                cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .undo_snapshot_for_test()
            });
            let listing_reconciled = visual.read_entity(&view, |view, _| {
                let Some(scan) = view.session.accepted_scan_ticket() else {
                    return false;
                };
                let Some(listing) = view.session.listing() else {
                    return false;
                };
                scan.path == destination
                    && scan.location_epoch == location_epoch
                    && scan.scan_id > baseline_scan.scan_id
                    && view.session.location_epoch() == location_epoch
                    && view.session.status() == &directory_session::ListingStatus::Ready
                    && !listing.iter().any(|entry| {
                        entry.key == copied_key
                            || entry.path.as_ref() == Some(&copied)
                            || entry.name == "copy-source.bin"
                    })
                    && !view.entries.iter().any(|entry| {
                        entry.key == copied_key
                            || entry.path.as_ref() == Some(&copied)
                            || entry.name == "copy-source.bin"
                    })
            });
            if !copied.exists() && !reserved && receipts.is_empty() && listing_reconciled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Copy Undo did not finish, consume its receipt, and reconcile the active listing"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(
            directory::EntrySourceIdentity::from_path(&source, EntryKind::File).unwrap(),
            source_identity
        );
        assert!(!copied.exists());
        assert_eq!(std::fs::read(&sibling).unwrap(), sibling_bytes);
        assert_eq!(
            directory::EntrySourceIdentity::from_path(&sibling, EntryKind::File).unwrap(),
            sibling_identity
        );
        let attempts_after_undo = visual.update(|_, cx| {
            cx.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .job(original_attempts[0].token)
                .unwrap()
                .attempts
                .clone()
        });
        assert_eq!(attempts_after_undo, original_attempts);
    }

    #[gpui::test]
    fn inline_rename_clipboard_routes_through_active_shell_without_file_operations(
        cx: &mut gpui::TestAppContext,
    ) {
        // Shell text publication tickets are process-global; serialize repeated
        // runs of this actual Shell writer regression within the test process.
        static CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _clipboard_test_guard = CLIPBOARD_TEST_LOCK.lock().unwrap();

        initialize_shell_test_globals(cx);
        cx.update(|app| {
            app.bind_keys([
                gpui::KeyBinding::new("ctrl-a", rename_editor::SelectAll, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-c", rename_editor::CopyText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-v", rename_editor::PasteText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-z", rename_editor::UndoText, Some("RenameInput")),
                gpui::KeyBinding::new("right", rename_editor::Right, Some("RenameInput")),
            ]);
        });

        let undo_events = std::rc::Rc::new(Cell::new(0));
        let undo_events_for_root = undo_events.clone();
        let (root, visual) = cx.add_window_view(move |_, cx| {
            let shell = cx.new(|cx| {
                shell::FilerShell::new(
                    persistence::Startup::disabled(),
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Vec::new(),
                    cx,
                )
            });
            InlineRenameRouteHarness {
                shell,
                undo_events: undo_events_for_root,
                subscription: None,
            }
        });
        visual.run_until_parked();
        let shell = visual.read_entity(&root, |root, _| root.shell.clone());
        let view = visual
            .read_entity(&shell, |shell, _| shell.active_view())
            .expect("Shell has an active directory view");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("日本語 😀.txt");
        std::fs::write(&path, b"rename clipboard route").unwrap();
        visual.update_entity(&view, |view, cx| {
            view.navigate(temp.path().to_path_buf(), cx);
        });
        visual.run_until_parked();
        let path_text = path.to_string_lossy().into_owned();
        let entry = drag_test_entry(Some(&path_text), EntryKind::File, None);
        visual.update_entity(&view, |view, _| {
            assert!(view.session.directory_identity().is_some());
            view.entries = vec![entry];
            view.rebuild_row_details();
            view.selection.apply(
                SelectionIntent::Pick {
                    index: 0,
                    toggle: false,
                    range: false,
                },
                view.entries.len(),
            );
            view.reset_projected_rows();
        });

        visual.update(|window, app| {
            view.update(app, |view, cx| {
                view.begin_rename(path.clone(), false, window, cx)
            });
        });
        visual.run_until_parked();
        assert!(visual.read_entity(&view, |view, _| view.rename_session.is_some()));
        visual.simulate_keystrokes("ctrl-c");
        visual.run_until_parked();
        assert_eq!(
            visual.update(|_, app| app
                .read_from_clipboard()
                .expect("Shell published selected rename text")
                .text()),
            Some("日本語 😀".to_owned())
        );

        let rename_input = visual.read_entity(&view, |view, _| {
            view.rename_session
                .as_ref()
                .expect("focused inline rename remains open")
                .input
                .clone()
        });
        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("edited".to_owned()));
        });
        visual.simulate_keystrokes("ctrl-v");
        visual.read_entity(&rename_input, |input, _| {
            assert_eq!(input.text(), "edited.txt");
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.read_entity(&rename_input, |input, _| {
            assert_eq!(input.text(), "日本語 😀.txt");
        });
        assert_eq!(undo_events.get(), 0, "Ctrl+Z must not emit file undo");
        assert!(path.exists(), "Ctrl+Z must not run file-operation undo");

        // An empty selection must leave the existing clipboard value alone.
        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("keep".to_owned()));
        });
        visual.simulate_keystrokes("right");
        visual.simulate_keystrokes("ctrl-c");
        assert_eq!(
            visual.update(|_, app| app.read_from_clipboard().unwrap().text()),
            Some("keep".to_owned())
        );

        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string(
                "貼り付け\r\n\u{0007}ok".to_owned(),
            ));
        });
        visual.simulate_keystrokes("ctrl-a");
        visual.simulate_keystrokes("ctrl-v");
        let input = visual.read_entity(&view, |view, _| {
            view.rename_session
                .as_ref()
                .expect("inline rename remains active")
                .input
                .clone()
        });
        assert_eq!(
            visual.read_entity(&input, |input, _| input.text().to_owned()),
            "貼り付けok"
        );

        let mut external_paths = gpui::ExternalPaths::default();
        external_paths.0.push(path.clone());
        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem {
                entries: vec![gpui::ClipboardEntry::ExternalPaths(external_paths)],
            });
        });
        visual.simulate_keystrokes("ctrl-v");
        visual.run_until_parked();
        assert_eq!(
            visual.read_entity(&input, |input, _| input.text().to_owned()),
            "貼り付けok"
        );
        assert!(visual.read_entity(&shell, |shell, cx| {
            shell.operation_requests_for_test(cx).is_empty()
        }));
    }

    #[gpui::test]
    fn selection_mode_space_routes_through_directory_view_focus_despite_other_hover(
        cx: &mut gpui::TestAppContext,
    ) {
        let preview_events = Rc::new(RefCell::new(Vec::new()));
        let (harness, visual) = cx.add_window_view({
            let preview_events = preview_events.clone();
            move |_, cx| {
                let view = cx.new(|cx| {
                    DirectoryListDemo::new_at(
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        None,
                        cx,
                    )
                });
                let entries = vec![
                    drag_test_entry(Some("C:/space/a.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/b.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/c.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/d.txt"), EntryKind::File, None),
                ];
                view.update(cx, |view, _| {
                    view.entries = entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(entries));
                    view.layout = ViewLayout::Rows;
                    view.reset_projected_rows();
                    view.selection.apply(
                        SelectionIntent::SetRange {
                            start: 0,
                            end: 2,
                            selected: true,
                        },
                        view.entries.len(),
                    );
                    view.selection.apply(
                        SelectionIntent::MoveFocus {
                            index: 2,
                            extend: false,
                        },
                        view.entries.len(),
                    );
                });
                KeyPathHarness {
                    view,
                    preview_events,
                    subscription: None,
                }
            }
        });
        let view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        let keys = visual.read_entity(&view, |view, _| {
            view.entries
                .iter()
                .map(|entry| entry.key.clone())
                .collect::<Vec<_>>()
        });
        visual.update(|window, app| {
            view.update(app, |view, cx| view.focus_handle.focus(window, cx));
        });

        let hovered_bounds = visual
            .debug_bounds("item-surface-row-name-3")
            .expect("the fourth row is rendered");
        visual.simulate_mouse_move(
            gpui::point(
                hovered_bounds.origin.x + hovered_bounds.size.width / 2.0,
                hovered_bounds.origin.y + hovered_bounds.size.height / 2.0,
            ),
            None,
            gpui::Modifiers::default(),
        );
        visual.read_entity(&view, |view, _| {
            assert_eq!(view.hovered_item_part.map(|(index, _)| index), Some(3));
            assert_eq!(view.selection.focused(), Some(2));
            assert_eq!(
                (0..view.entries.len())
                    .filter(|index| view.selection.contains(*index))
                    .collect::<Vec<_>>(),
                vec![0, 1]
            );
        });

        visual.simulate_keystrokes("space");
        visual.read_entity(&view, |view, _| {
            assert_eq!(view.selection.focused(), Some(2));
            assert_eq!(
                (0..view.entries.len())
                    .filter(|index| view.selection.contains(*index))
                    .collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
        });
        assert!(
            preview_events.borrow().iter().all(|(_, open)| !open),
            "selection-mode Space must not open Quick Preview"
        );

        // With two selected items, toggling the focused one exits selection mode (2 -> 1).
        visual.update_entity(&view, |view, cx| {
            view.selection
                .apply(SelectionIntent::Clear, view.entries.len());
            view.selection.apply(
                SelectionIntent::SetRange {
                    start: 0,
                    end: 2,
                    selected: true,
                },
                view.entries.len(),
            );
            view.selection.apply(
                SelectionIntent::MoveFocus {
                    index: 1,
                    extend: false,
                },
                view.entries.len(),
            );
            cx.notify();
        });
        visual.simulate_keystrokes("space");
        visual.read_entity(&view, |view, _| {
            assert_eq!(
                (0..view.entries.len())
                    .filter(|index| view.selection.contains(*index))
                    .collect::<Vec<_>>(),
                vec![0]
            );
            assert_eq!(view.selection.focused(), Some(1));
        });
        visual.simulate_keystrokes("space");
        let opened_target = preview_events
            .borrow()
            .iter()
            .find_map(|(target, open)| {
                open.then_some(target.as_ref().map(|target| target.key.clone()))
            })
            .flatten();
        assert_eq!(opened_target, Some(keys[1].clone()));
    }

    #[gpui::test]
    fn space_without_focus_previews_first_displayed_item_without_changing_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        let preview_events = Rc::new(RefCell::new(Vec::new()));
        let (harness, visual) = cx.add_window_view({
            let preview_events = preview_events.clone();
            move |_, cx| {
                let view = cx.new(|cx| {
                    DirectoryListDemo::new_at(
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        None,
                        cx,
                    )
                });
                let entries = vec![
                    drag_test_entry(Some("C:/space/first.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/second.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/third.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/space/hovered.txt"), EntryKind::File, None),
                ];
                view.update(cx, |view, _| {
                    view.entries = entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(entries));
                    view.layout = ViewLayout::Rows;
                    view.reset_projected_rows();
                    view.selection
                        .apply(SelectionIntent::Clear, view.entries.len());
                });
                KeyPathHarness {
                    view,
                    preview_events,
                    subscription: None,
                }
            }
        });
        let view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        let keys = visual.read_entity(&view, |view, _| {
            view.entries
                .iter()
                .map(|entry| entry.key.clone())
                .collect::<Vec<_>>()
        });
        visual.update(|window, app| {
            view.update(app, |view, cx| view.focus_handle.focus(window, cx));
        });

        let hovered_bounds = visual
            .debug_bounds("item-surface-row-name-3")
            .expect("the fourth row is rendered");
        visual.simulate_mouse_move(
            gpui::point(
                hovered_bounds.origin.x + hovered_bounds.size.width / 2.0,
                hovered_bounds.origin.y + hovered_bounds.size.height / 2.0,
            ),
            None,
            gpui::Modifiers::default(),
        );
        visual.read_entity(&view, |view, _| {
            assert_eq!(view.hovered_item_part.map(|(index, _)| index), Some(3));
            assert_eq!(view.selection.focused(), None);
            assert_eq!(view.selection.len(), 0);
        });

        visual.simulate_keystrokes("space");
        visual.read_entity(&view, |view, _| {
            assert_eq!(view.selection.focused(), None);
            assert_eq!(view.selection.len(), 0);
        });
        let opened_target = preview_events
            .borrow()
            .iter()
            .find_map(|(target, open)| {
                open.then_some(target.as_ref().map(|target| target.key.clone()))
            })
            .flatten();
        assert_eq!(opened_target, Some(keys[0].clone()));
    }

    struct InternalDragRouteHarness {
        entries: Vec<DemoEntry>,
        snapshot: Rc<RefCell<Option<InternalDragSnapshot>>>,
        drop_count: Rc<RefCell<usize>>,
    }

    struct DirectoryEventHarness {
        view: gpui::Entity<DirectoryListDemo>,
        shell: Option<gpui::Entity<shell::FilerShell>>,
        render_shell: bool,
        requests: Rc<RefCell<Vec<operations::model::OperationRequest>>>,
        bubbled_mouse_ups: Rc<RefCell<usize>>,
        event_order: Rc<RefCell<Vec<&'static str>>>,
        subscription: Option<Subscription>,
    }

    #[derive(Clone, Copy, Debug)]
    enum PostPointerDownChange {
        None,
        SourceIdentity,
        LocationGeneration,
        PresentationRevision,
    }

    #[derive(Clone, Copy, Debug)]
    enum ExternalDropChange {
        None,
        LocationGeneration,
        PresentationRevision,
        TargetIdentity,
        MissingSession,
        StaleSourceView,
    }

    fn initialize_shell_test_globals(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<persistence::SessionAutosaveOwner>() {
                cx.set_global(persistence::SessionAutosaveOwner::disabled());
            }
            if !cx.has_global::<persistence::FolderPreferencesOwner>() {
                cx.set_global(persistence::FolderPreferencesOwner::disabled());
            }
            if !cx.has_global::<window_coordinator::WindowCoordinator>() {
                cx.set_global(window_coordinator::WindowCoordinator::default());
            }
            if !cx.has_global::<operations::application::OperationApplicationOwner>() {
                cx.set_global(operations::application::OperationApplicationOwner::for_test());
            }
        });
    }

    impl Render for DirectoryEventHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.subscription.is_none() {
                let event_order = self.event_order.clone();
                self.subscription = Some(cx.subscribe(
                    &self.view,
                    move |_this, _view, event: &DirectoryEvent, _cx| match event {
                        DirectoryEvent::CancelNativeDrag { .. } => {
                            event_order.borrow_mut().push("cancel");
                        }
                        _ => {}
                    },
                ));
            }
            let bubbled_mouse_ups = self.bubbled_mouse_ups.clone();
            let mut root = div().size_full().on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |_, _, _, _| {
                    *bubbled_mouse_ups.borrow_mut() += 1;
                }),
            );
            if self.render_shell {
                if let Some(shell) = self.shell.clone() {
                    root = root.child(shell);
                } else {
                    root = root.child(self.view.clone());
                }
            } else {
                root = root.child(self.view.clone());
            }
            root
        }
    }

    fn run_actual_directory_drag(
        cx: &mut gpui::TestAppContext,
        layout: ViewLayout,
        target_kind: EntryKind,
        post_pointer_down_change: PostPointerDownChange,
    ) -> (
        Vec<operations::model::OperationRequest>,
        usize,
        Vec<&'static str>,
        usize,
        bool,
    ) {
        initialize_shell_test_globals(cx);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let bubbled_mouse_ups = Rc::new(RefCell::new(0));
        let event_order = Rc::new(RefCell::new(Vec::new()));
        let (harness, visual) = cx.add_window_view({
            let requests = requests.clone();
            let bubbled_mouse_ups = bubbled_mouse_ups.clone();
            let event_order = event_order.clone();
            move |_, cx| {
                let shell = cx.new(|cx| {
                    shell::FilerShell::new(
                        persistence::Startup::disabled(),
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        Vec::new(),
                        cx,
                    )
                });
                let view = shell
                    .read(cx)
                    .active_view()
                    .expect("test shell has an active view");
                view.update(cx, |view, _| {
                    let entries = vec![
                        drag_test_entry(Some("C:/drag/source-a.txt"), EntryKind::File, None),
                        drag_test_entry(Some("C:/drag/source-b.txt"), EntryKind::File, None),
                        drag_test_entry(Some("C:/drag/target"), target_kind, None),
                    ];
                    view.entries = entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(entries));
                    view.layout = layout;
                    view.location_generation = 7;
                    view.presentation_revision = 11;
                    view.reset_projected_rows();
                    view.selection.apply(
                        SelectionIntent::SetRange {
                            start: 0,
                            end: 2,
                            selected: true,
                        },
                        view.entries.len(),
                    );
                });
                DirectoryEventHarness {
                    view,
                    shell: Some(shell),
                    render_shell: true,
                    requests,
                    bubbled_mouse_ups,
                    event_order,
                    subscription: None,
                }
            }
        });
        let (source_selector, target_selector) = match layout {
            ViewLayout::Rows => ("item-surface-row-name-0", "item-surface-row-name-2"),
            ViewLayout::Icons => ("item-surface-visual-0", "item-surface-visual-2"),
            ViewLayout::Tiles => unreachable!(),
        };
        let source_bounds = visual
            .debug_bounds(source_selector)
            .expect("the rendered source hit surface must have bounds");
        let shell = visual.read_entity(&harness, |harness, _| harness.shell.clone().unwrap());
        let source_tab_id = visual.read_entity(&shell, |shell, _| shell.active_tab_id_for_test());
        let prior_request_count = visual.read_entity(&shell, |shell, cx| {
            shell.operation_requests_for_test(cx).len()
        });
        let mut target_bounds = visual
            .debug_bounds(target_selector)
            .expect("the rendered destination hit surface must have bounds");
        let center = |bounds: Bounds<Pixels>| {
            gpui::point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        };
        let modifiers = gpui::Modifiers::default();
        visual.simulate_mouse_down(center(source_bounds), gpui::MouseButton::Left, modifiers);
        if !matches!(post_pointer_down_change, PostPointerDownChange::None) {
            visual.update_entity(&harness, |harness, cx| {
                harness.view.update(cx, |view, cx| {
                    match post_pointer_down_change {
                        PostPointerDownChange::None => unreachable!(),
                        PostPointerDownChange::SourceIdentity => {
                            view.entries[0].source_identity =
                                Some(directory::EntrySourceIdentity::test(
                                    EntryKind::File,
                                    "reused-source-object",
                                ));
                            view.rebuild_row_details();
                        }
                        PostPointerDownChange::LocationGeneration => {
                            view.location_generation += 1;
                        }
                        PostPointerDownChange::PresentationRevision => {
                            view.presentation_revision += 1;
                        }
                    }
                    cx.notify();
                });
            });
            // stale source の差し替えで行の高さが変わる場合があるため、移動先は再描画後の
            // surface bounds を使う。押下元だけは更新前の hit surface を保持する。
            target_bounds = visual
                .debug_bounds(target_selector)
                .expect("the refreshed destination hit surface must have bounds");
        }
        visual.simulate_mouse_move(
            center(target_bounds),
            Some(gpui::MouseButton::Left),
            modifiers,
        );
        visual.simulate_mouse_up(center(target_bounds), gpui::MouseButton::Left, modifiers);
        let submitted: Vec<operations::model::OperationRequest> = visual
            .read_entity(&shell, |shell, cx| shell.operation_requests_for_test(cx))
            .into_iter()
            .skip(prior_request_count)
            .collect();
        let worker_claimed = visual.read_entity(&shell, |shell, cx| {
            shell.operation_worker_claimed_for_test(cx)
        });
        visual.update_entity(&harness, |harness, _| {
            *harness.requests.borrow_mut() = submitted.clone();
        });
        visual.read_entity(&harness, |harness, _| {
            (
                submitted,
                *harness.bubbled_mouse_ups.borrow(),
                harness.event_order.borrow().clone(),
                source_tab_id,
                worker_claimed,
            )
        })
    }

    #[gpui::test]
    fn actual_directory_drag_moves_to_nonfocused_pane_folder(cx: &mut gpui::TestAppContext) {
        initialize_shell_test_globals(cx);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let bubbled_mouse_ups = Rc::new(RefCell::new(0));
        let target_identity = Rc::new(RefCell::new(None));
        let source_identities = Rc::new(RefCell::new(None));
        let (harness, visual) = cx.add_window_view({
            let requests = requests.clone();
            let bubbled_mouse_ups = bubbled_mouse_ups.clone();
            let target_identity = target_identity.clone();
            let source_identities = source_identities.clone();
            move |_, cx| {
                let shell = cx.new(|cx| {
                    shell::FilerShell::new(
                        persistence::Startup::disabled(),
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        Vec::new(),
                        cx,
                    )
                });
                let source_view = shell.read(cx).active_view().unwrap();
                let source_entries = vec![
                    drag_test_entry(Some("C:/cross-pane/source-a.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/cross-pane/source-b.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/cross-pane/source-c.txt"), EntryKind::File, None),
                ];
                *source_identities.borrow_mut() = Some(
                    source_entries
                        .iter()
                        .take(2)
                        .map(|entry| {
                            entry
                                .source_identity
                                .as_ref()
                                .expect("selected source row has a captured filesystem identity")
                                .filesystem_identity()
                        })
                        .collect::<Vec<_>>(),
                );
                source_view.update(cx, |view, _| {
                    view.entries = source_entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(source_entries));
                    view.layout = ViewLayout::Rows;
                    view.location_generation = 17;
                    view.presentation_revision = 23;
                    view.reset_projected_rows();
                    view.selection.apply(
                        SelectionIntent::SetRange {
                            start: 0,
                            end: 2,
                            selected: true,
                        },
                        view.entries.len(),
                    );
                });
                let target_entries = vec![
                    drag_test_entry(Some("C:/cross-pane/other-a.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/cross-pane/other-b.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/cross-pane/destination"), EntryKind::Folder, None),
                ];
                *target_identity.borrow_mut() = Some(
                    target_entries
                        .iter()
                        .find(|entry| {
                            entry.path.as_deref()
                                == Some(std::path::Path::new("C:/cross-pane/destination"))
                        })
                        .and_then(|entry| entry.source_identity.as_ref())
                        .expect("destination row has a captured filesystem identity")
                        .filesystem_identity(),
                );
                let target_view = cx.new(|cx| {
                    let mut view = DirectoryListDemo::new_at(
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        None,
                        cx,
                    );
                    view.entries = target_entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(target_entries));
                    view.layout = ViewLayout::Icons;
                    view.location_generation = 31;
                    view.presentation_revision = 37;
                    view.reset_projected_rows();
                    view
                });
                shell.update(cx, |shell, cx| {
                    shell
                        .add_pane_with_view_for_test(target_view, cx)
                        .expect("test shell should attach the destination pane");
                });
                DirectoryEventHarness {
                    view: source_view,
                    shell: Some(shell),
                    render_shell: true,
                    requests,
                    bubbled_mouse_ups,
                    event_order: Rc::new(RefCell::new(Vec::new())),
                    subscription: None,
                }
            }
        });
        let shell = visual.read_entity(&harness, |harness, _| harness.shell.clone().unwrap());
        let baseline = visual.read_entity(&shell, |shell, _| {
            shell.pane_focus_and_active_tabs_for_test()
        });
        assert_eq!(baseline.1.len(), 2);
        let source_tab_id = baseline
            .1
            .iter()
            .find(|(pane, _)| *pane == baseline.0)
            .unwrap()
            .1;
        let target_tab_id = baseline
            .1
            .iter()
            .find(|(pane, _)| *pane != baseline.0)
            .unwrap()
            .1;
        let source_view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        let source_bounds = visual.debug_bounds("item-surface-row-name-0").unwrap();
        let target_bounds = visual.debug_bounds("item-surface-visual-2").unwrap();
        let center = |bounds: Bounds<Pixels>| {
            gpui::point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        };
        let prior_count = visual.read_entity(&shell, |shell, cx| {
            shell.operation_requests_for_test(cx).len()
        });
        let modifiers = gpui::Modifiers::default();
        visual.simulate_mouse_down(center(source_bounds), gpui::MouseButton::Left, modifiers);
        visual.simulate_mouse_move(
            center(target_bounds),
            Some(gpui::MouseButton::Left),
            modifiers,
        );
        visual.simulate_mouse_up(center(target_bounds), gpui::MouseButton::Left, modifiers);
        let submitted = visual.read_entity(&shell, |shell, cx| {
            shell
                .operation_requests_for_test(cx)
                .into_iter()
                .skip(prior_count)
                .collect::<Vec<_>>()
        });
        assert_eq!(submitted.len(), 1);
        let request = &submitted[0];
        assert_eq!(request.kind, operations::model::OperationKind::Move);
        assert_eq!(
            request.sources,
            vec![
                std::path::PathBuf::from("C:/cross-pane/source-a.txt"),
                std::path::PathBuf::from("C:/cross-pane/source-b.txt"),
            ]
        );
        assert_eq!(
            request.destination,
            Some("C:/cross-pane/destination".into())
        );
        let guard = request
            .move_identity_guard
            .as_ref()
            .expect("internal Move keeps identity guards");
        assert_eq!(
            guard.source_identities,
            source_identities
                .borrow()
                .clone()
                .expect("source fixture captured identities in selection order")
        );
        assert_eq!(
            guard.destination_identity,
            target_identity
                .borrow()
                .clone()
                .expect("destination fixture captured its row identity")
        );
        assert_eq!(
            request.origin,
            Some(operations::model::OperationOrigin {
                view_id: target_tab_id as u64,
                location_epoch: 31,
                presentation_revision: 37,
            })
        );
        assert_eq!(
            visual.read_entity(&shell, |shell, _| shell
                .pane_focus_and_active_tabs_for_test()),
            baseline
        );
        visual.read_entity(&harness, |harness, _| {
            assert_eq!(*harness.bubbled_mouse_ups.borrow(), 0)
        });
        visual.read_entity(&source_view, |view, _| {
            assert!(view.internal_drag_snapshot.borrow().is_none())
        });
        assert_ne!(source_tab_id, target_tab_id);
        assert!(!visual.read_entity(&shell, |shell, cx| {
            shell.operation_worker_claimed_for_test(cx)
        }));
    }

    #[gpui::test]
    fn actual_directory_drag_dwell_switches_workspace_then_moves_to_folder(
        cx: &mut gpui::TestAppContext,
    ) {
        for (
            stale_source,
            deleted_source,
            replaced_source_identity,
            replaced_source_view,
            source_tab_inactive,
            stale_session,
            blank_viewport,
            release_before_dwell,
            replace_snapshot_before_release,
        ) in [
            (false, false, false, false, false, false, false, false, false),
            (false, false, false, false, false, false, true, false, false),
            (true, false, false, false, false, false, false, false, false),
            (false, true, false, false, false, false, false, false, false),
            (false, false, true, false, false, false, false, false, false),
            (false, false, false, true, false, false, false, false, false),
            (false, false, false, false, true, false, false, false, false),
            (false, false, false, false, false, true, false, false, false),
            (false, false, false, false, false, false, true, false, false),
            (false, false, false, false, false, false, false, true, false),
            (false, false, false, false, false, false, false, true, true),
        ] {
            initialize_shell_test_globals(cx);
            let blank_target_directory = blank_viewport.then(tempfile::tempdir).transpose().unwrap();
            let blank_target_path = blank_target_directory
                .as_ref()
                .map(|directory| directory.path().to_path_buf());
            let blank_target_path_for_view = blank_target_path.clone();
            let requests = Rc::new(RefCell::new(Vec::new()));
            let bubbled_mouse_ups = Rc::new(RefCell::new(0));
            let event_order = Rc::new(RefCell::new(Vec::new()));
            let source_identities = Rc::new(RefCell::new(Vec::new()));
            let destination_identity = Rc::new(RefCell::new(None));
            let alternate_source_tab = Rc::new(RefCell::new(None));
            let (harness, visual) = cx.add_window_view({
                let requests = requests.clone();
                let bubbled_mouse_ups = bubbled_mouse_ups.clone();
                let event_order = event_order.clone();
                let source_identities = source_identities.clone();
                let destination_identity = destination_identity.clone();
                let alternate_source_tab = alternate_source_tab.clone();
                move |_, cx| {
                    let shell = cx.new(|cx| {
                        shell::FilerShell::new(
                            persistence::Startup::disabled(),
                            std::sync::Arc::new(input::KeymapRuntime::new()),
                            Vec::new(),
                            cx,
                        )
                    });
                    let source_view = shell.read(cx).active_view().unwrap();
                    let source_entries = vec![
                        drag_test_entry(Some("C:/workspace-drag/source-a.txt"), EntryKind::File, None),
                        drag_test_entry(Some("C:/workspace-drag/source-b.txt"), EntryKind::File, None),
                        drag_test_entry(Some("C:/workspace-drag/source-c.txt"), EntryKind::File, None),
                    ];
                    *source_identities.borrow_mut() = source_entries
                        .iter()
                        .take(2)
                        .map(|entry| entry.source_identity.as_ref().unwrap().filesystem_identity())
                        .collect();
                    source_view.update(cx, |view, _| {
                        view.entries = source_entries.clone();
                        view.rebuild_row_details();
                        view.session.adopt_demo(std::sync::Arc::new(source_entries));
                        view.layout = ViewLayout::Rows;
                        view.location_generation = 41;
                        view.presentation_revision = 43;
                        view.reset_projected_rows();
                        view.selection.apply(
                            SelectionIntent::SetRange { start: 0, end: 2, selected: true },
                            view.entries.len(),
                        );
                    });
                    let target_view = if let Some(path) = blank_target_path_for_view.clone() {
                        cx.new(|cx| {
                            let mut view = DirectoryListDemo::new_at(
                                std::sync::Arc::new(input::KeymapRuntime::new()),
                                Some(Location::Path(path)),
                                cx,
                            );
                            view.layout = ViewLayout::Rows;
                            view.location_generation = 47;
                            view.presentation_revision = 53;
                            view
                        })
                    } else {
                        let target_entries = vec![
                            drag_test_entry(Some("C:/workspace-drag/destination-a.txt"), EntryKind::File, None),
                            drag_test_entry(Some("C:/workspace-drag/destination-b.txt"), EntryKind::File, None),
                            drag_test_entry(Some("C:/workspace-drag/target-folder"), EntryKind::Folder, None),
                        ];
                        *destination_identity.borrow_mut() = target_entries[2]
                            .source_identity.as_ref().map(|identity| identity.filesystem_identity());
                        cx.new(|cx| {
                            let mut view = DirectoryListDemo::new_at(
                                std::sync::Arc::new(input::KeymapRuntime::new()), None, cx,
                            );
                            view.entries = target_entries.clone();
                            view.rebuild_row_details();
                            view.session.adopt_demo(std::sync::Arc::new(target_entries));
                            view.layout = ViewLayout::Rows;
                            view.location_generation = 47;
                            view.presentation_revision = 53;
                            view.reset_projected_rows();
                            view
                        })
                    };
                    shell.update(cx, |shell, cx| {
                        shell.add_inactive_workspace_with_view_for_test(target_view, cx);
                    });
                    if source_tab_inactive {
                        *alternate_source_tab.borrow_mut() = shell.update(cx, |shell, cx| {
                            let source_workspace = shell.active_workspace_for_test();
                            shell.add_background_tab_for_workspace_test(
                                source_workspace,
                                source_view.entity_id(),
                                cx,
                            )
                        });
                        assert!(alternate_source_tab.borrow().is_some());
                    }
                    DirectoryEventHarness {
                        view: source_view,
                        shell: Some(shell),
                        render_shell: true,
                        requests,
                        bubbled_mouse_ups,
                        event_order,
                        subscription: None,
                    }
                }
            });

            let shell = visual.read_entity(&harness, |harness, _| harness.shell.clone().unwrap());
            let (source_workspace, target_workspace) =
                visual.read_entity(&shell, |shell, _| shell.workspace_ids_for_test());
            let source_bounds = visual.debug_bounds("item-surface-row-name-0").unwrap();
            let target_tab_bounds = visual.read_entity(&shell, |shell, _| {
                shell.workspace_tab_bounds_for_test(target_workspace)
            }).expect("rendered inactive tab bounds");
            let center = |bounds: Bounds<Pixels>| gpui::point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            let modifiers = gpui::Modifiers::default();
            let prior_count = visual.read_entity(&shell, |shell, cx| {
                shell.operation_requests_for_test(cx).len()
            });
            visual.simulate_mouse_down(center(source_bounds), gpui::MouseButton::Left, modifiers);
            visual.simulate_mouse_move(
                gpui::point(center(source_bounds).x + px(4.0), center(source_bounds).y),
                Some(gpui::MouseButton::Left),
                modifiers,
            );
            visual.simulate_mouse_move(center(target_tab_bounds), Some(gpui::MouseButton::Left), modifiers);
            assert_eq!(
                visual.read_entity(&shell, |shell, _| shell.workspace_drop_hover_for_test()),
                Some(target_workspace),
                "root drag routing retained hover on the rendered inactive tab"
            );
            assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), source_workspace);
            if release_before_dwell {
                let source_view = visual.read_entity(&harness, |harness, _| harness.view.clone());
                let replacement = if replace_snapshot_before_release {
                    Some(visual.update_entity(&source_view, |view, _| {
                        let mut snapshot = view
                            .internal_drag_snapshot
                            .borrow()
                            .clone()
                            .expect("the active drag owns its source snapshot");
                        snapshot.session_id = snapshot.session_id.wrapping_add(1).max(1);
                        *view.internal_drag_snapshot.borrow_mut() = Some(snapshot.clone());
                        snapshot
                    }))
                } else {
                    None
                };
                visual.simulate_mouse_up(center(target_tab_bounds), gpui::MouseButton::Left, modifiers);
                // GPUI の timer queue が release 後にも処理される状態で、実時間 guard を越えて検証する。
                std::thread::sleep(std::time::Duration::from_millis(351));
                visual
                    .executor()
                    .advance_clock(std::time::Duration::from_millis(351));
                visual.run_until_parked();

                assert_eq!(
                    visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()),
                    source_workspace,
                    "release before dwell cancels the queued workspace switch"
                );
                assert_eq!(
                    visual.read_entity(&shell, |shell, _| shell.workspace_drop_hover_for_test()),
                    None,
                    "release clears the workspace hover candidate"
                );
                let submitted = visual.read_entity(&shell, |shell, cx| {
                    shell.operation_requests_for_test(cx).into_iter().skip(prior_count).collect::<Vec<_>>()
                });
                assert!(submitted.is_empty(), "release before dwell submits no Move");
                assert_eq!(
                    *bubbled_mouse_ups.borrow(),
                    1,
                    "workspace-tab mouse-up follows the root route and bubbles once"
                );
                visual.read_entity(&source_view, |view, _| {
                    assert_eq!(
                        view.internal_drag_snapshot.borrow().as_ref().map(|snapshot| snapshot.session_id),
                        replacement.as_ref().map(|snapshot| snapshot.session_id),
                        "route cleanup retires its snapshot but preserves a replacement session"
                    );
                });
                continue;
            }
            // Shell の timer は GPUI clock に加えて実時間の dwell guard も確認する。
            // visual clock を進める前に実時間を待ち、境界だけは virtual time で検証する。
            std::thread::sleep(std::time::Duration::from_millis(351));
            visual
                .executor()
                .advance_clock(std::time::Duration::from_millis(349));
            visual.run_until_parked();
            assert_eq!(
                visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()),
                source_workspace,
                "workspace stays unchanged before 350ms dwell"
            );
            visual
                .executor()
                .advance_clock(std::time::Duration::from_millis(1));
            visual.run_until_parked();
            assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), target_workspace);

            if source_tab_inactive {
                let alternate_tab = alternate_source_tab
                    .borrow()
                    .expect("source workspace retains its alternate tab");
                visual.update_entity(&shell, |shell, cx| {
                    assert!(shell.activate_workspace_tab_for_test(
                        source_workspace,
                        alternate_tab,
                        cx,
                    ));
                });
            }
            if stale_session {
                let source_view = visual.read_entity(&harness, |harness, _| harness.view.clone());
                visual.update_entity(&source_view, |view, _| {
                    let snapshot_session_id = view
                        .internal_drag_snapshot
                        .borrow()
                        .as_ref()
                        .expect("the active drag owns its source snapshot")
                        .session_id;
                    assert_eq!(snapshot_session_id, view.next_internal_drag_session_id);
                    view.next_internal_drag_session_id = snapshot_session_id + 1;
                });
            }

            let target_point = if blank_viewport {
                let target_view = visual.read_entity(&shell, |shell, _| {
                    shell.active_view().expect("dwell activated the target directory")
                });
                let (viewport, destination) = visual.read_entity(&target_view, |view, _| {
                    (
                        view.scroll_handle.bounds(),
                        view.session.directory_identity().cloned(),
                    )
                });
                assert!(viewport.size.width > px(20.0) && viewport.size.height > px(20.0));
                assert!(visual.debug_bounds("item-surface-row-name-0").is_none());
                *destination_identity.borrow_mut() = destination;
                let point = gpui::point(
                    viewport.origin.x + px(10.0),
                    viewport.origin.y + px(10.0),
                );
                visual.read_entity(&target_view, |view, _| {
                    assert!(view.is_blank_list_viewport_drop(point));
                });
                point
            } else {
                let bounds = visual.debug_bounds("item-surface-row-name-2").unwrap();
                center(bounds)
            };
            let target_path = if blank_viewport {
                blank_target_path.as_ref().unwrap().to_string_lossy().into_owned()
            } else {
                "C:/workspace-drag/target-folder".to_owned()
            };
            if stale_source {
                visual.update_entity(&harness, |harness, cx| {
                    harness.view.update(cx, |view, cx| {
                        view.location_generation += 1;
                        cx.notify();
                    });
                });
            }
            if deleted_source {
                visual.update_entity(&harness, |harness, cx| {
                    harness.view.update(cx, |view, cx| {
                        view.entries.retain(|entry| {
                            entry.path.as_deref()
                                != Some(std::path::Path::new("C:/workspace-drag/source-b.txt"))
                        });
                        view.session
                            .adopt_demo(std::sync::Arc::new(view.entries.clone()));
                        view.rebuild_row_details();
                        cx.notify();
                    });
                });
            }
            if replaced_source_identity {
                visual.update_entity(&harness, |harness, cx| {
                    harness.view.update(cx, |view, cx| {
                        let captured_identity = view
                            .internal_drag_snapshot
                            .borrow()
                            .as_ref()
                            .and_then(|snapshot| {
                                snapshot
                                    .sources
                                    .iter()
                                    .find(|source| {
                                        source.path
                                            == std::path::Path::new(
                                                "C:/workspace-drag/source-b.txt",
                                            )
                                    })
                            })
                            .map(|source| source.identity.clone())
                            .expect("captured source identity exists");
                        let replacement_identity = view
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.path.as_deref()
                                    == Some(std::path::Path::new(
                                        "C:/workspace-drag/source-c.txt",
                                    ))
                            })
                            .and_then(|entry| entry.source_identity.clone())
                            .expect("replacement source identity exists");
                        assert_ne!(
                            replacement_identity.filesystem_identity(),
                            captured_identity,
                            "replacement identity differs while the source path stays the same"
                        );
                        let source = view
                            .entries
                            .iter_mut()
                            .find(|entry| {
                                entry.path.as_deref()
                                    == Some(std::path::Path::new(
                                        "C:/workspace-drag/source-b.txt",
                                    ))
                            })
                            .expect("captured source row remains at the same path");
                        source.source_identity = Some(replacement_identity);
                        view.session
                            .adopt_demo(std::sync::Arc::new(view.entries.clone()));
                        view.rebuild_row_details();
                        cx.notify();
                    });
                });
            }
            if replaced_source_view {
                visual.update_entity(&harness, |harness, cx| {
                    let replacement = cx.new(|cx| {
                        DirectoryListDemo::new_at(
                            std::sync::Arc::new(input::KeymapRuntime::new()),
                            None,
                            cx,
                        )
                    });
                    let shell = harness.shell.as_ref().expect("test Shell remains attached");
                    shell.update(cx, |shell, cx| {
                        assert!(shell.replace_workspace_view_for_test(
                            source_workspace,
                            harness.view.entity_id(),
                            replacement,
                            cx,
                        ));
                    });
                });
            }
            visual.simulate_mouse_move(target_point, Some(gpui::MouseButton::Left), modifiers);
            visual.simulate_mouse_up(target_point, gpui::MouseButton::Left, modifiers);
            let submitted = visual.read_entity(&shell, |shell, cx| {
                shell.operation_requests_for_test(cx).into_iter().skip(prior_count).collect::<Vec<_>>()
            });
            assert_eq!(*bubbled_mouse_ups.borrow(), 0);
            let source_view = visual.read_entity(&harness, |harness, _| harness.view.clone());
            visual.read_entity(&source_view, |view, _| {
                assert_eq!(
                    view.internal_drag_snapshot
                        .borrow()
                        .as_ref()
                        .map(|snapshot| snapshot.session_id),
                    None,
                    "drop consumes the captured snapshot after route validation"
                );
            });
            if stale_source
                || deleted_source
                || replaced_source_identity
                || replaced_source_view
                || source_tab_inactive
                || stale_session
            {
                assert!(
                    submitted.is_empty(),
                    "stale source/session, deleted source, replaced identity/view, or inactive source tab admits no Move"
                );
                continue;
            }
            assert_eq!(submitted.len(), 1);
            let request = &submitted[0];
            assert_eq!(request.kind, operations::model::OperationKind::Move);
            assert_eq!(request.sources, vec![
                std::path::PathBuf::from("C:/workspace-drag/source-a.txt"),
                std::path::PathBuf::from("C:/workspace-drag/source-b.txt"),
            ]);
            assert_eq!(request.destination, Some(target_path.into()));
            let guard = request.move_identity_guard.as_ref().expect("guarded workspace Move");
            assert_eq!(guard.source_identities, *source_identities.borrow());
            assert_eq!(
                guard.destination_identity,
                destination_identity.borrow().clone().expect("captured folder identity")
            );
        }
    }

    #[gpui::test]
    fn rendered_workspace_hover_move_uses_real_files_and_shell_pump(
        cx: &mut gpui::TestAppContext,
    ) {
        let fixture = tempfile::Builder::new()
            .prefix("gpui-filer-workspace-move-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let first = source.join("a.txt");
        let second = source.join("b.txt");
        let sentinel = source.join("sentinel.txt");
        let first_bytes = b"first real payload";
        let second_bytes = b"second real payload";
        let sentinel_bytes = b"unselected sentinel";
        std::fs::write(&first, first_bytes).unwrap();
        std::fs::write(&second, second_bytes).unwrap();
        std::fs::write(&sentinel, sentinel_bytes).unwrap();

        let journal_path = fixture.path().join("journal.json");
        let store = operations::journal::JournalStore::open(journal_path.clone()).unwrap();
        let (test_writer, mut journal_driver) = operations::journal::JournalWriter::test_driver();
        let mut operation_owner =
            operations::application::OperationApplicationOwner::for_test_with_journal_store(store);
        operation_owner.journal = Some(test_writer);
        cx.update(|cx| {
            cx.set_global(operation_owner);
            if !cx.has_global::<archive_provider::ArchiveProviderOwner>() {
                cx.set_global(archive_provider::ArchiveProviderOwner::default());
            }
        });
        let real_writer = operations::journal::JournalWriter::start(
            operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let persist_journal_commands = |driver: &mut operations::journal::JournalWriterTestDriver| {
            while let Some(command) = driver.try_command() {
                match command {
                    operations::journal::JournalWriterCommandForTest::Upsert(upsert) => {
                        let receipt = real_writer
                            .upsert_with_reply(upsert.attempt.clone())
                            .recv_blocking()
                            .expect("real JournalWriter returns an Upsert receipt");
                        upsert.reply(receipt);
                    }
                    operations::journal::JournalWriterCommandForTest::UnacknowledgedUpsert(
                        attempt,
                    ) => {
                        operations::journal::JournalWriterCommandForTest::UnacknowledgedUpsert(
                            attempt,
                        )
                        .persist_to(&real_writer)
                        .expect("real JournalWriter persists an unacknowledged snapshot");
                    }
                    _ => panic!("unexpected journal command in regular-file Move test"),
                }
            }
        };
        initialize_shell_test_globals(cx);

        let requests = Rc::new(RefCell::new(Vec::new()));
        let bubbled_mouse_ups = Rc::new(RefCell::new(0));
        let event_order = Rc::new(RefCell::new(Vec::new()));
        let (harness, visual) = cx.add_window_view({
            let requests = requests.clone();
            let bubbled_mouse_ups = bubbled_mouse_ups.clone();
            let event_order = event_order.clone();
            let source = source.clone();
            let destination = destination.clone();
            move |_, cx| {
                let shell = cx.new(|cx| shell::FilerShell::new(
                    persistence::Startup::disabled(),
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Vec::new(),
                    cx,
                ));
                let source_view = shell.read(cx).active_view().unwrap();
                source_view.update(cx, |view, cx| {
                    view.navigate_location(Location::Path(source.clone()), cx);
                });
                let target_view = cx.new(|cx| DirectoryListDemo::new_at(
                    std::sync::Arc::new(input::KeymapRuntime::new()),
                    Some(Location::Path(destination)),
                    cx,
                ));
                shell.update(cx, |shell, cx| {
                    shell.add_inactive_workspace_with_view_for_test(target_view, cx);
                });
                DirectoryEventHarness {
                    view: source_view,
                    shell: Some(shell),
                    render_shell: true,
                    requests,
                    bubbled_mouse_ups,
                    event_order,
                    subscription: None,
                }
            }
        });
        let shell = visual.read_entity(&harness, |harness, _| harness.shell.clone().unwrap());
        let (source_workspace, target_workspace) =
            visual.read_entity(&shell, |shell, _| shell.workspace_ids_for_test());

        let wait_until = |visual: &mut gpui::VisualTestContext, stage: &str, mut ready: Box<dyn FnMut(&mut gpui::VisualTestContext) -> bool>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !ready(visual) {
                visual.run_until_parked();
                assert!(std::time::Instant::now() < deadline, "rendered real Move fixture timed out waiting for {stage}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let source_view = visual.read_entity(&harness, |harness, _| harness.view.clone());
        wait_until(visual, "initial source scan", Box::new({
            let source_view = source_view.clone();
            move |visual| visual.read_entity(&source_view, |view, _| {
                view.session.status() == &directory_session::ListingStatus::Ready
                    && view.session.listing().is_some_and(|listing| listing.len() == 3)
            })
        }));
        let (first_index, second_index, sentinel_identity, source_identities) =
            visual.read_entity(&source_view, |view, _| {
                let listing = view.session.listing().unwrap();
                let locate = |path: &std::path::Path| listing.iter().position(|entry| entry.path.as_ref() == Some(&path.to_path_buf())).unwrap();
                let first_index = locate(&first);
                let second_index = locate(&second);
                let sentinel_identity = listing.iter().find(|entry| entry.path.as_ref() == Some(&sentinel)).unwrap().source_identity.clone().unwrap().filesystem_identity();
                let identities = [first_index, second_index].into_iter().map(|index| view.entries[index].source_identity.as_ref().unwrap().filesystem_identity()).collect::<Vec<_>>();
                (first_index, second_index, sentinel_identity, identities)
            });
        assert_eq!(second_index, first_index + 1, "selected source rows are contiguous in the accepted scan");
        assert_eq!(first_index, 0, "fixture names place the selected files at the first real rows");
        visual.update_entity(&source_view, |view, _| {
            view.selection.apply(
                SelectionIntent::SetRange { start: first_index, end: second_index + 1, selected: true },
                view.entries.len(),
            );
        });

        let source_bounds = visual.debug_bounds("item-surface-row-name-0").unwrap();
        let tab_bounds = visual.read_entity(&shell, |shell, _| shell.workspace_tab_bounds_for_test(target_workspace)).unwrap();
        let center = |bounds: Bounds<Pixels>| gpui::point(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
        let modifiers = gpui::Modifiers::default();
        visual.simulate_mouse_down(center(source_bounds), gpui::MouseButton::Left, modifiers);
        visual.simulate_mouse_move(gpui::point(center(source_bounds).x + px(4.0), center(source_bounds).y), Some(gpui::MouseButton::Left), modifiers);
        visual.simulate_mouse_move(center(tab_bounds), Some(gpui::MouseButton::Left), modifiers);
        assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), source_workspace);
        std::thread::sleep(std::time::Duration::from_millis(351));
        visual.executor().advance_clock(std::time::Duration::from_millis(349));
        visual.run_until_parked();
        assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), source_workspace);
        visual.executor().advance_clock(std::time::Duration::from_millis(1));
        visual.run_until_parked();
        assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), target_workspace);

        let target_view = visual.read_entity(&shell, |shell, _| shell.active_view().unwrap());
        wait_until(visual, "initial destination scan", Box::new({
            let target_view = target_view.clone();
            move |visual| visual.read_entity(&target_view, |view, _| view.session.status() == &directory_session::ListingStatus::Ready)
        }));
        let (destination_identity, target_bounds) = visual.read_entity(&target_view, |view, _| {
            (view.session.directory_identity().unwrap().clone(), view.scroll_handle.bounds())
        });
        assert_eq!(
            source_identities,
            [first.clone(), second.clone()].into_iter().map(|path| {
                directory::EntrySourceIdentity::from_path_no_follow(&path, EntryKind::File)
                    .unwrap()
                    .filesystem_identity()
            }).collect::<Vec<_>>(),
            "source identities came from accepted enumeration and still match the real files",
        );
        assert_eq!(
            destination_identity,
            directory::EntrySourceIdentity::from_path_no_follow(&destination, EntryKind::Folder)
                .unwrap()
                .filesystem_identity(),
            "destination identity came from accepted session readiness and still matches",
        );
        assert!(visual.debug_bounds("item-surface-row-name-0").is_none());
        let drop_point = gpui::point(target_bounds.origin.x + px(10.0), target_bounds.origin.y + px(10.0));
        assert!(visual.read_entity(&target_view, |view, _| view.is_blank_list_viewport_drop(drop_point)));
        visual.simulate_mouse_move(drop_point, Some(gpui::MouseButton::Left), modifiers);
        visual.simulate_mouse_up(drop_point, gpui::MouseButton::Left, modifiers);
        assert_eq!(*bubbled_mouse_ups.borrow(), 0);

        let attempts = visual.read_entity(&shell, |shell, cx| shell.operation_requests_for_test(cx));
        assert_eq!(attempts.len(), 1);
        let request = &attempts[0];
        assert_eq!(request.kind, operations::model::OperationKind::Move);
        assert_eq!(request.sources, vec![first.clone(), second.clone()]);
        assert_eq!(request.destination.as_deref(), Some(destination.as_path()));
        let guard = request.move_identity_guard.as_ref().expect("guarded real Move");
        assert_eq!(guard.source_identities, source_identities);
        assert_eq!(guard.destination_identity, destination_identity);

        let attempt_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = loop {
            visual.run_until_parked();
            persist_journal_commands(&mut journal_driver);
            let token = visual.read_entity(&shell, |_, cx| {
                cx.read_global::<operations::application::OperationApplicationOwner, _>(|owner, _| {
                    owner.runtime.jobs().into_iter().last().and_then(|job| job.attempts.last().map(|attempt| attempt.token))
                })
            });
            if let Some(token) = token {
                break token;
            }
            assert!(std::time::Instant::now() < attempt_deadline, "Shell did not create a Runtime attempt for the admitted Move");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let completion_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            visual.run_until_parked();
            persist_journal_commands(&mut journal_driver);
            let (claimed, attempt) = visual.read_entity(&shell, |shell, cx| {
                let attempt = cx.read_global::<operations::application::OperationApplicationOwner, _>(|owner, _| {
                    owner.runtime.job(token).and_then(|job| job.attempts.into_iter().find(|attempt| attempt.token == token))
                });
                (shell.operation_worker_claimed_for_test(cx), attempt)
            });
            if !claimed && attempt.as_ref().and_then(|attempt| attempt.terminal.as_ref()).is_some_and(|terminal| terminal.outcome == operations::model::TerminalOutcome::Succeeded) {
                break;
            }
            assert!(std::time::Instant::now() < completion_deadline, "Move terminal/return timed out: worker_claimed={claimed}, attempt={attempt:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let journal_key = visual.read_entity(&shell, |_, cx| {
            cx.read_global::<operations::application::OperationApplicationOwner, _>(|owner, _| {
                owner.runtime.journal_attempt(token).expect("terminal attempt snapshot").key
            })
        });
        let durable = operations::journal::JournalStore::open(journal_path).unwrap();
        let durable_attempt = durable.attempt(&journal_key).expect("successful attempt durably acknowledged");
        assert_eq!(durable_attempt.state, operations::journal::JournalState::Terminal);
        assert_eq!(durable_attempt.terminal_outcome, Some(operations::journal::JournalTerminalOutcome::Succeeded));
        assert!(!first.exists() && !second.exists());
        assert_eq!(std::fs::read(destination.join("a.txt")).unwrap(), first_bytes);
        assert_eq!(std::fs::read(destination.join("b.txt")).unwrap(), second_bytes);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
        assert_eq!(directory::EntrySourceIdentity::from_path_no_follow(&sentinel, EntryKind::File).unwrap().filesystem_identity(), sentinel_identity);

        wait_until(visual, "destination reconciliation", Box::new({
            let target_view = target_view.clone();
            move |visual| visual.read_entity(&target_view, |view, _| {
                view.session.status() == &directory_session::ListingStatus::Ready
                    && view.session.accepted_scan_ticket().is_some_and(|scan| scan.scan_id > 1)
                    && ["a.txt", "b.txt"].iter().all(|name| view.entries.iter().any(|entry| entry.name == *name))
            })
        }));

        let newer_than = visual.read_entity(&source_view, |view, _| view.session.accepted_scan_ticket().unwrap().scan_id);
        let source_tab = visual.read_entity(&shell, |shell, _| shell.workspace_tab_bounds_for_test(source_workspace)).unwrap();
        visual.simulate_mouse_down(center(source_tab), gpui::MouseButton::Left, modifiers);
        visual.simulate_mouse_up(center(source_tab), gpui::MouseButton::Left, modifiers);
        assert_eq!(visual.read_entity(&shell, |shell, _| shell.active_workspace_for_test()), source_workspace);
        wait_until(visual, "source reactivation scan", Box::new({
            let source_view = source_view.clone();
            move |visual| visual.read_entity(&source_view, |view, _| view.session.status() == &directory_session::ListingStatus::Ready
                && view.session.accepted_scan_ticket().is_some_and(|scan| scan.scan_id > newer_than)
                && !view.entries.iter().any(|entry| entry.name == "a.txt" || entry.name == "b.txt")
                && view.entries.iter().any(|entry| entry.name == "sentinel.txt"))
        }));
    }

    fn run_actual_external_drop(
        cx: &mut gpui::TestAppContext,
        target_path: Option<&str>,
        target_kind: EntryKind,
        target_link: Option<LinkKind>,
        background: bool,
        empty_sources: bool,
        change: ExternalDropChange,
        local_drag_session: Option<u64>,
        omit_submit_context: bool,
        fill_operation_capacity: bool,
    ) -> (
        Vec<operations::model::OperationRequest>,
        usize,
        Option<gpui::FileDropEffect>,
    ) {
        run_actual_external_drop_with_policy(
            cx,
            target_path,
            target_kind,
            target_link,
            background,
            empty_sources,
            change,
            local_drag_session,
            omit_submit_context,
            fill_operation_capacity,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            false,
            false,
            None,
            false,
        )
    }

    fn run_actual_external_drop_with_policy(
        cx: &mut gpui::TestAppContext,
        target_path: Option<&str>,
        target_kind: EntryKind,
        target_link: Option<LinkKind>,
        background: bool,
        empty_sources: bool,
        change: ExternalDropChange,
        local_drag_session: Option<u64>,
        omit_submit_context: bool,
        fill_operation_capacity: bool,
        local_pin_policy: win_shell_dnd::FileDragPinPolicy,
        drop_paths: Option<Vec<std::path::PathBuf>>,
        duplicate_local_submit: bool,
        file_edge_scroll: bool,
        edge_scroll_source: Option<gpui::MouseInputSource>,
        nonfocused_pane_target: bool,
    ) -> (
        Vec<operations::model::OperationRequest>,
        usize,
        Option<gpui::FileDropEffect>,
    ) {
        initialize_shell_test_globals(cx);
        let drop_paths = drop_paths.unwrap_or_else(|| {
            if empty_sources {
                Vec::new()
            } else {
                vec![std::path::PathBuf::from("C:/outside/source.bin")]
            }
        });
        let paths = gpui::ExternalPaths(drop_paths.into_iter().collect());
        let captured_paths_for_local = paths.paths().to_vec();
        let requests = Rc::new(RefCell::new(Vec::new()));
        let bubbled_mouse_ups = Rc::new(RefCell::new(0));
        let event_order = Rc::new(RefCell::new(Vec::new()));
        let view_slot = Rc::new(RefCell::new(None));
        let pane_state_baseline = Rc::new(RefCell::new(None));
        let (harness, visual) = cx.add_window_view({
            let requests = requests.clone();
            let bubbled_mouse_ups = bubbled_mouse_ups.clone();
            let event_order = event_order.clone();
            let view_slot = view_slot.clone();
            let pane_state_baseline = pane_state_baseline.clone();
            let captured_paths_for_local = captured_paths_for_local.clone();
            move |_, cx| {
                let shell = cx.new(|cx| {
                    shell::FilerShell::new(
                        persistence::Startup::disabled(),
                        std::sync::Arc::new(input::KeymapRuntime::new()),
                        Vec::new(),
                        cx,
                    )
                });
                let view = shell
                    .read(cx)
                    .active_view()
                    .expect("new test shell must have an active directory view");
                let focused_view = view.clone();
                let source_snapshot = view.update(cx, |view, _| {
                    let mut entries = if local_drag_session.is_some() {
                        captured_paths_for_local
                            .iter()
                            .map(|path| {
                                let path_text = path.to_string_lossy();
                                let kind = if path.file_name().is_some_and(|name| name == "folder")
                                {
                                    EntryKind::Folder
                                } else {
                                    EntryKind::File
                                };
                                drag_test_entry(Some(path_text.as_ref()), kind, None)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        vec![drag_test_entry(
                            Some("C:/drag/source.txt"),
                            EntryKind::File,
                            None,
                        )]
                    };
                    entries.push(drag_test_entry(target_path, target_kind, target_link));
                    if file_edge_scroll {
                        entries.extend((0..100).map(|index| {
                            drag_test_entry(
                                Some(&format!("C:/drag/edge-{index}.txt")),
                                EntryKind::File,
                                None,
                            )
                        }));
                    }
                    view.session
                        .navigate(Location::Path("C:/drag".into()), None);
                    view.entries = entries.clone();
                    view.rebuild_row_details();
                    view.session.adopt_demo(std::sync::Arc::new(entries));
                    view.location_generation = 7;
                    view.presentation_revision = 11;
                    view.reset_projected_rows();
                    if local_drag_session.is_some() && !captured_paths_for_local.is_empty() {
                        view.next_internal_drag_session_id = 3;
                        let selected = (0..captured_paths_for_local.len()).collect::<HashSet<_>>();
                        let sources = capture_internal_drag_sources(&view.entries, 0, &selected)
                            .expect("local OLE test paths must identify rendered source rows");
                        let snapshot = InternalDragSnapshot {
                            sources,
                            touch_start_position: None,
                            location_generation: 7,
                            presentation_revision: 11,
                            session_id: 3,
                        };
                        *view.internal_drag_snapshot.borrow_mut() = Some(snapshot.clone());
                        Some(snapshot)
                    } else {
                        None
                    }
                });
                let source_snapshot_slot = view.read(cx).internal_drag_snapshot.clone();
                if local_drag_session.is_some() {
                    shell.update(cx, |shell, _| {
                        shell.prepare_native_drag_for_test(
                            view.entity_id(),
                            0x1234,
                            captured_paths_for_local.clone(),
                            source_snapshot.expect("local OLE drag has captured rendered sources"),
                            7,
                            11,
                            local_pin_policy,
                            edge_scroll_source.unwrap_or(gpui::MouseInputSource::Mouse),
                        );
                        shell.retain_native_drag_source_route_for_test(source_snapshot_slot);
                    });
                }
                if fill_operation_capacity {
                    shell.update(cx, |shell, cx| shell.fill_operation_capacity_for_test(cx));
                }
                let view = if nonfocused_pane_target {
                    let target_view = cx.new(|cx| {
                        DirectoryListDemo::new_at(
                            std::sync::Arc::new(input::KeymapRuntime::new()),
                            None,
                            cx,
                        )
                    });
                    target_view.update(cx, |view, _| {
                        let mut entries = vec![
                            drag_test_entry(Some("C:/drop-b/first.txt"), EntryKind::File, None),
                            drag_test_entry(Some("C:/drop-b/second.txt"), EntryKind::File, None),
                            drag_test_entry(target_path, target_kind, target_link),
                        ];
                        view.session
                            .navigate(Location::Path("C:/drop-b".into()), None);
                        view.entries = entries.clone();
                        view.rebuild_row_details();
                        view.session
                            .adopt_demo(std::sync::Arc::new(std::mem::take(&mut entries)));
                        view.location_generation = 7;
                        view.presentation_revision = 11;
                        view.reset_projected_rows();
                    });
                    let (_, target_tab_id) = shell.update(cx, |shell, cx| {
                        shell
                            .add_pane_with_view_for_test(target_view.clone(), cx)
                            .expect("test shell should be able to attach one additional pane")
                    });
                    let state = shell.read(cx).pane_focus_and_active_tabs_for_test();
                    *pane_state_baseline.borrow_mut() = Some(state.clone());
                    assert!(state.1.iter().any(|(_, tab_id)| *tab_id == target_tab_id));
                    assert_eq!(
                        shell
                            .read(cx)
                            .active_view()
                            .map(|active| active.entity_id()),
                        Some(focused_view.entity_id()),
                        "adding the test pane must preserve pane A focus"
                    );
                    target_view
                } else {
                    view.clone()
                };
                *view_slot.borrow_mut() = Some(view.clone());
                DirectoryEventHarness {
                    view,
                    shell: Some(shell),
                    render_shell: true,
                    requests,
                    bubbled_mouse_ups,
                    event_order,
                    subscription: None,
                }
            }
        });

        let position = if background {
            let bounds = visual.read_entity(view_slot.borrow().as_ref().unwrap(), |view, _| {
                view.scroll_handle.bounds()
            });
            gpui::point(
                bounds.origin.x + px(24.0),
                bounds.origin.y + bounds.size.height - px(20.0),
            )
        } else {
            let target_index = if nonfocused_pane_target {
                2
            } else if local_drag_session.is_some() {
                captured_paths_for_local.len()
            } else {
                1
            };
            let selector = match target_index {
                0 | 1 => "external-drop-row-1",
                2 => "external-drop-row-2",
                _ => panic!("external-drop test fixture supports at most two local sources"),
            };
            let bounds = visual
                .debug_bounds(selector)
                .expect("rendered file row should expose its drop surface");
            // In Rows this reaches Kind/Size/Date rather than the child name hit surface.
            gpui::point(
                bounds.origin.x + bounds.size.width - px(8.0),
                bounds.origin.y + px(20.0),
            )
        };
        let entered_position = if file_edge_scroll {
            let bounds = visual.read_entity(view_slot.borrow().as_ref().unwrap(), |view, _| {
                view.scroll_handle.bounds()
            });
            gpui::point(
                bounds.origin.x + px(24.0),
                bounds.origin.y + bounds.size.height / 2.0,
            )
        } else {
            position
        };
        visual.update(|window, app| {
            let context = gpui::FileDropContext {
                local_drag_session: local_drag_session.map(gpui::LocalDragSessionToken::new),
                paths: Some(paths.clone()),
                response: gpui::FileDropResponse::default(),
                ..gpui::FileDropContext::default()
            };
            gpui::with_file_drop_context(context.clone(), || {
                window.dispatch_event(
                    gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Entered {
                        position: entered_position,
                        paths: paths.clone(),
                    }),
                    app,
                );
            });
            let pending_context = if local_drag_session.is_none() {
                gpui::FileDropContext {
                    paths: None,
                    ..context
                }
            } else {
                context
            };
            gpui::with_file_drop_context(pending_context, || {
                window.dispatch_event(
                    gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Pending { position }),
                    app,
                );
            });
        });
        // GPUI は Entered を mouse move に変換してから次 frame の drag_over を描く。
        // ドロップ対象の再描画で世代が取り直されないことも実際の routing で確かめる。
        visual.run_until_parked();
        let shell = visual.read_entity(&harness, |harness, _| {
            harness
                .shell
                .clone()
                .expect("external-drop harness has a shell")
        });
        if nonfocused_pane_target {
            let baseline = pane_state_baseline
                .borrow()
                .clone()
                .expect("two-pane fixture records initial active tab ownership");
            let after_hover = visual.read_entity(&shell, |shell, _| {
                shell.pane_focus_and_active_tabs_for_test()
            });
            assert_eq!(
                after_hover, baseline,
                "hover must preserve pane focus and tabs"
            );
        }
        let prior_request_count = visual.read_entity(&shell, |shell, cx| {
            shell.operation_requests_for_test(cx).len()
        });
        if file_edge_scroll {
            let offset = visual.read_entity(view_slot.borrow().as_ref().unwrap(), |view, _| {
                view.scroll_handle.offset().y.as_f32()
            });
            assert!(
                offset < 0.0,
                "Pending OLE move must edge-scroll the list: {offset}"
            );
        }
        if matches!(change, ExternalDropChange::MissingSession) {
            let view = view_slot.borrow().as_ref().unwrap().clone();
            let session = visual.read_entity(&view, |view, _| view.external_drag_session.clone());
            take_external_drag_session(&session);
        } else if matches!(change, ExternalDropChange::StaleSourceView) {
            visual.update(|_, app| {
                shell.update(app, |shell, cx| {
                    let replacement = cx.new(|cx| {
                        DirectoryListDemo::new_at(
                            std::sync::Arc::new(input::KeymapRuntime::new()),
                            None,
                            cx,
                        )
                    });
                    shell.replace_active_view_for_test(replacement);
                });
            });
            visual.run_until_parked();
        } else if !matches!(change, ExternalDropChange::None) {
            let view = view_slot.borrow().as_ref().unwrap().clone();
            visual.update(|_, app| {
                view.update(app, |view, cx| {
                    match change {
                        ExternalDropChange::None => {}
                        ExternalDropChange::LocationGeneration => {
                            view.location_generation = 8;
                        }
                        ExternalDropChange::PresentationRevision => {
                            view.presentation_revision = 12;
                        }
                        ExternalDropChange::TargetIdentity => {
                            view.entries[1].source_identity =
                                Some(directory::EntrySourceIdentity::test(
                                    EntryKind::Folder,
                                    "replacement-folder-identity",
                                ));
                            // Projection identity changes advance the revision in production.
                            view.presentation_revision += 1;
                        }
                        ExternalDropChange::MissingSession
                        | ExternalDropChange::StaleSourceView => unreachable!(),
                    }
                    cx.notify();
                });
            });
            visual.run_until_parked();
        }
        let response = gpui::FileDropResponse::default();
        visual.update(|window, app| {
            window.set_modifiers(gpui::Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            });
            let mut submit = || {
                window.dispatch_event(
                    gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Submit { position }),
                    app,
                );
            };
            if omit_submit_context {
                submit();
            } else {
                gpui::with_file_drop_context(
                    gpui::FileDropContext {
                        local_drag_session: local_drag_session
                            .map(gpui::LocalDragSessionToken::new),
                        paths: Some(paths.clone()),
                        response: response.clone(),
                        ..gpui::FileDropContext::default()
                    },
                    || {
                        submit();
                        if local_drag_session.is_some() && duplicate_local_submit {
                            submit();
                        }
                    },
                );
            }
            window.dispatch_event(
                gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Ended),
                app,
            );
        });
        visual.run_until_parked();
        if nonfocused_pane_target {
            let baseline = pane_state_baseline
                .borrow()
                .clone()
                .expect("two-pane fixture records initial active tab ownership");
            let after_drop = visual.read_entity(&shell, |shell, _| {
                shell.pane_focus_and_active_tabs_for_test()
            });
            assert_eq!(
                after_drop, baseline,
                "drop completion must preserve pane focus and tabs"
            );
        }
        let mut submitted =
            visual.read_entity(&shell, |shell, cx| shell.operation_requests_for_test(cx));
        submitted.drain(..prior_request_count);
        let submitted_paths = paths.paths().to_vec();
        submitted.retain(|request| request.sources == submitted_paths);
        if nonfocused_pane_target {
            let baseline = pane_state_baseline
                .borrow()
                .clone()
                .expect("two-pane fixture records initial active tab ownership");
            let target_tab_id = baseline
                .1
                .iter()
                .find(|(pane_id, _)| *pane_id != baseline.0)
                .map(|(_, tab_id)| *tab_id)
                .expect("fixture has a pane outside the focused pane");
            assert_eq!(submitted.len(), 1, "the hit row admits one external Copy");
            assert_eq!(
                submitted[0].origin.as_ref().map(|origin| origin.view_id),
                Some(target_tab_id as u64),
                "operation origin must identify the nonfocused hit tab"
            );
        }
        (submitted, *bubbled_mouse_ups.borrow(), response.effect())
    }

    impl Render for InternalDragRouteHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            const GENERATION: u64 = 7;
            const REVISION: u64 = 11;

            let payload = InternalDragPayload {
                source: cx.entity().entity_id(),
                snapshot: self.snapshot.clone(),
            };
            let source = div()
                .id("drag-route-source")
                .absolute()
                .left(px(10.0))
                .top(px(10.0))
                .w(px(80.0))
                .h(px(80.0))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        let selected = HashSet::from([0]);
                        let sources = capture_internal_drag_sources(&this.entries, 0, &selected);
                        *this.snapshot.borrow_mut() = sources.map(|sources| InternalDragSnapshot {
                            sources,
                            touch_start_position: None,
                            location_generation: GENERATION,
                            presentation_revision: REVISION,
                            session_id: 1,
                        });
                        cx.stop_propagation();
                    }),
                )
                .on_drag(payload.clone(), move |_, _, _, cx| cx.new(|_| gpui::Empty));

            let target = internal_drop_target_for_entry(&self.entries[1], GENERATION, REVISION)
                .expect("test destination is an ordinary folder");
            let target_entry = target.clone();
            let target_surface = div()
                .id("drag-route-target")
                .absolute()
                .left(px(120.0))
                .top(px(10.0))
                .w(px(80.0))
                .h(px(80.0))
                .on_drop::<InternalDragPayload>(cx.listener(
                    move |this, payload: &InternalDragPayload, _window, cx| {
                        let Some(snapshot) = take_internal_drag_snapshot(&payload.snapshot) else {
                            cx.stop_propagation();
                            return;
                        };
                        let source_current = payload.source == cx.entity().entity_id()
                            && internal_drag_snapshot_is_current(
                                &snapshot,
                                &this.entries,
                                GENERATION,
                                REVISION,
                            );
                        let target_current = internal_drop_target_is_current(
                            &target_entry,
                            &this.entries,
                            GENERATION,
                            REVISION,
                        );
                        let valid = source_current
                            && target_current
                            && internal_drop_target_is_valid(
                                &snapshot.sources,
                                Some(&target_entry.path),
                                EntryKind::Folder,
                                false,
                            );
                        if valid {
                            *this.drop_count.borrow_mut() += 1;
                        }
                        cx.stop_propagation();
                    },
                ));
            div().child(source).child(target_surface)
        }
    }

    #[test]
    fn indexed_folder_size_is_projected_without_rewriting_exact_size() {
        let mut folder = drag_test_entry(Some("C:/items/folder"), EntryKind::Folder, None);
        folder.folder_size = Some(directory::FolderSizeFact {
            bytes: 8 * 1024,
            provenance: directory::FolderSizeProvenance::EverythingIndex,
        });
        let mut file = drag_test_entry(Some("C:/items/file.bin"), EntryKind::File, None);
        file.size = Some(3 * 1024);
        let translator = Translator::new(Locale::Japanese);

        assert_eq!(projected_entry_size(&folder, &translator), "8.0 KB");
        assert_eq!(projected_entry_size(&file, &translator), "3.0 KB");

        // folder_size は表示専用の index 情報で、厳密な filesystem size には混ぜない。
        assert_eq!(folder.size, None);
        assert_eq!(folder.folder_size.map(|fact| fact.bytes), Some(8 * 1024));
        assert_eq!(file.size, Some(3 * 1024));
        assert_eq!(file.folder_size, None);
    }

    fn drag_test_entry(path: Option<&str>, kind: EntryKind, link: Option<LinkKind>) -> DemoEntry {
        let path = path.map(std::path::PathBuf::from);
        let source_identity = path.as_ref().map(|path| {
            directory::EntrySourceIdentity::test(kind, path.to_string_lossy().into_owned())
        });
        DemoEntry {
            key: path.as_ref().map_or_else(
                || entry_identity::EntryKey::virtual_item("drag-test".into()),
                |path| entry_identity::EntryKey::path(path.clone(), kind),
            ),
            name: path.as_ref().and_then(|path| path.file_name()).map_or_else(
                || "virtual".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path,
            kind,
            link,
            link_target: None,
            source_identity,
            is_hidden: false,
            size: None,
            folder_size: None,
            modified: None,
        }
    }

    fn trash_test_selection(count: usize, selected: &[usize]) -> Selection {
        let mut selection = Selection::default();
        for &index in selected {
            selection.apply(SelectionIntent::Checkbox { index }, count);
        }
        selection
    }

    #[test]
    fn trash_snapshot_accepts_complete_ordinary_candidate_and_mixed_scopes() {
        let ordinary = vec![
            drag_test_entry(Some("C:/items/a.txt"), EntryKind::File, None),
            drag_test_entry(Some("C:/items/folder"), EntryKind::Folder, None),
        ];
        let selection = trash_test_selection(2, &[0, 1]);
        let sources = selected_trash_sources_for_selection(&ordinary, &selection, None).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].0, ordinary[0].path.clone().unwrap());
        assert_eq!(sources[0].1, ordinary[0].source_identity.clone().unwrap());
        assert_eq!(sources[1].0, ordinary[1].path.clone().unwrap());
        assert_eq!(sources[1].1, ordinary[1].source_identity.clone().unwrap());
        assert!(sources.iter().all(|(_, _, candidate)| !candidate));

        let candidate = vec![drag_test_entry(
            Some("C:/items/junction"),
            EntryKind::Folder,
            Some(LinkKind::LinkOrJunction),
        )];
        let sources =
            selected_trash_sources_for_selection(&candidate, &trash_test_selection(1, &[0]), None)
                .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, candidate[0].path.clone().unwrap());
        assert_eq!(sources[0].1, candidate[0].source_identity.clone().unwrap());
        assert!(sources[0].2);

        let mixed = vec![
            drag_test_entry(Some("C:/items/a.txt"), EntryKind::File, None),
            candidate[0].clone(),
        ];
        let mixed_sources = selected_trash_sources_for_selection(
            &mixed,
            &trash_test_selection(2, &[0, 1]),
            None,
        )
        .unwrap();
        assert_eq!(mixed_sources.len(), 2);
        assert_eq!(mixed_sources[0].0, mixed[0].path.clone().unwrap());
        assert_eq!(mixed_sources[0].1, mixed[0].source_identity.clone().unwrap());
        assert!(!mixed_sources[0].2);
        assert_eq!(mixed_sources[1].0, mixed[1].path.clone().unwrap());
        assert_eq!(mixed_sources[1].1, mixed[1].source_identity.clone().unwrap());
        assert!(mixed_sources[1].2);

        let second_candidate = drag_test_entry(
            Some("C:/items/symlink"),
            EntryKind::File,
            Some(LinkKind::LinkOrJunction),
        );
        let multiple_candidates = vec![candidate[0].clone(), second_candidate.clone()];
        let sources = selected_trash_sources_for_selection(
            &multiple_candidates,
            &trash_test_selection(2, &[0, 1]),
            None,
        )
        .unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].0, candidate[0].path.clone().unwrap());
        assert_eq!(sources[1].0, second_candidate.path.clone().unwrap());
        assert!(sources.iter().all(|(_, _, candidate)| *candidate));

        let other_reparse = vec![drag_test_entry(
            Some("C:/items/other-reparse"),
            EntryKind::File,
            Some(LinkKind::OtherReparsePoint),
        )];
        assert!(selected_trash_sources_for_selection(
            &other_reparse,
            &trash_test_selection(1, &[0]),
            None,
        )
        .is_none());
    }

    #[test]
    fn trash_snapshot_rejects_missing_or_stale_selected_rows() {
        let missing_path = vec![drag_test_entry(None, EntryKind::File, None)];
        assert!(selected_trash_sources_for_selection(
            &missing_path,
            &trash_test_selection(1, &[0]),
            None,
        )
        .is_none());

        let mut missing_identity = vec![drag_test_entry(
            Some("C:/items/no-identity.txt"),
            EntryKind::File,
            None,
        )];
        missing_identity[0].source_identity = None;
        assert!(selected_trash_sources_for_selection(
            &missing_identity,
            &trash_test_selection(1, &[0]),
            None,
        )
        .is_none());

        let rows = vec![drag_test_entry(Some("C:/items/stale.txt"), EntryKind::File, None)];
        let stale = trash_test_selection(2, &[0, 1]);
        assert!(selected_trash_sources_for_selection(&rows, &stale, None).is_none());
        assert!(selected_trash_sources_for_selection(&rows, &Selection::default(), None).is_none());
    }

    #[test]
    fn internal_drag_uses_selected_filesystem_paths_or_only_the_unselected_source() {
        let mut entries = vec![
            drag_test_entry(Some("C:/items/one.txt"), EntryKind::File, None),
            drag_test_entry(Some("C:/items/two"), EntryKind::Folder, None),
            drag_test_entry(Some("C:/items/three.txt"), EntryKind::File, None),
            drag_test_entry(None, EntryKind::Folder, None),
        ];
        let captured_modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1234);
        entries[0].size = Some(42);
        entries[0].modified = Some(captured_modified);
        let selected = HashSet::from([0, 1]);
        let group = capture_internal_drag_sources(&entries, 0, &selected).unwrap();
        assert_eq!(group[0].size, Some(42));
        assert_eq!(group[0].modified, Some(captured_modified));
        assert_eq!(
            group,
            vec![
                internal_drag_source_for_entry(&entries[0]).unwrap(),
                internal_drag_source_for_entry(&entries[1]).unwrap(),
            ]
        );
        let single = capture_internal_drag_sources(&entries, 2, &selected).unwrap();
        assert_eq!(
            single,
            vec![internal_drag_source_for_entry(&entries[2]).unwrap()]
        );
        assert!(capture_internal_drag_sources(&entries, 3, &selected).is_none());
    }

    #[test]
    fn internal_drag_preview_uses_only_the_captured_source_paths() {
        let single = InternalDragSnapshot {
            sources: vec![InternalDragSource {
                key: entry_identity::EntryKey::path("C:/items/report.txt".into(), EntryKind::File),
                path: "C:/items/report.txt".into(),
                identity: filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "source".into(),
                ),
                is_directory: false,
                size: Some(1),
                modified: Some(std::time::UNIX_EPOCH),
            }],
            touch_start_position: None,
            location_generation: 1,
            presentation_revision: 2,
            session_id: 1,
        };
        assert_eq!(internal_drag_preview_text(&single), "report.txt");

        let multiple = InternalDragSnapshot {
            sources: ["first.txt", "second"]
                .into_iter()
                .map(|name| InternalDragSource {
                    key: entry_identity::EntryKey::path(
                        format!("C:/items/{name}").into(),
                        EntryKind::File,
                    ),
                    path: format!("C:/items/{name}").into(),
                    identity: filesystem_identity::FilesystemIdentity::from_fallback_token(
                        name.into(),
                    ),
                    is_directory: false,
                    size: Some(1),
                    modified: Some(std::time::UNIX_EPOCH),
                })
                .collect(),
            touch_start_position: None,
            location_generation: 1,
            presentation_revision: 2,
            session_id: 2,
        };
        assert_eq!(internal_drag_preview_text(&multiple), "2 items");
    }

    #[test]
    fn internal_drag_preview_colors_meet_wcag_aa_contrast() {
        let (background, foreground) = internal_drag_preview_colors();
        let background = background.to_rgb();

        // 白は半透明カードが重なる背景として最も明るく、コントラストの最悪条件になる。
        let background = gpui::Rgba {
            r: background.r * background.a + (1.0 - background.a),
            g: background.g * background.a + (1.0 - background.a),
            b: background.b * background.a + (1.0 - background.a),
            a: 1.0,
        };

        let relative_luminance = |color: gpui::Rgba| {
            let linear = |channel: f32| {
                if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
        };

        let foreground_luminance = relative_luminance(foreground);
        let background_luminance = relative_luminance(background);
        let contrast = (foreground_luminance + 0.05) / (background_luminance + 0.05);

        assert!(
            contrast >= 4.5,
            "internal drag preview contrast was {contrast:.2}:1, expected at least 4.5:1"
        );
    }

    #[test]
    fn internal_drag_rejects_invalid_targets_and_stale_source_identity() {
        let entries = vec![drag_test_entry(
            Some("C:/items/source"),
            EntryKind::Folder,
            None,
        )];
        let snapshot = InternalDragSnapshot {
            sources: vec![internal_drag_source_for_entry(&entries[0]).unwrap()],
            touch_start_position: None,
            location_generation: 7,
            presentation_revision: 11,
            session_id: 1,
        };
        assert!(internal_drag_snapshot_is_current(
            &snapshot, &entries, 7, 11
        ));
        assert!(!internal_drag_snapshot_is_current(
            &snapshot, &entries, 8, 11
        ));
        assert!(!internal_drag_snapshot_is_current(
            &snapshot, &entries, 7, 12
        ));
        let mut replaced = entries.clone();
        replaced[0].source_identity = Some(directory::EntrySourceIdentity::test(
            EntryKind::Folder,
            "different-object",
        ));
        assert!(!internal_drag_snapshot_is_current(
            &snapshot, &replaced, 7, 11
        ));

        let target_entry = drag_test_entry(Some("C:/items/target"), EntryKind::Folder, None);
        let target = internal_drop_target_for_entry(&target_entry, 7, 11).unwrap();
        assert!(internal_drop_target_is_current(
            &target,
            &[target_entry.clone()],
            7,
            11
        ));
        let mut replaced_target = target_entry.clone();
        replaced_target.source_identity = Some(directory::EntrySourceIdentity::test(
            EntryKind::Folder,
            "different-target-object",
        ));
        assert!(!internal_drop_target_is_current(
            &target,
            &[replaced_target],
            7,
            11
        ));
        assert!(!internal_drop_target_is_current(
            &target,
            &[target_entry],
            7,
            12
        ));

        let sources = snapshot.sources.as_slice();
        assert!(internal_drop_target_is_valid(
            sources,
            Some(std::path::Path::new("C:/items/target")),
            EntryKind::Folder,
            false,
        ));
        assert!(!internal_drop_target_is_valid(
            sources,
            Some(std::path::Path::new("C:/items/file.txt")),
            EntryKind::File,
            false,
        ));
        assert!(!internal_drop_target_is_valid(
            sources,
            None,
            EntryKind::Folder,
            false
        ));
        assert!(!internal_drop_target_is_valid(
            sources,
            Some(std::path::Path::new("C:/items/link")),
            EntryKind::Folder,
            true,
        ));
        assert!(!internal_drop_target_is_valid(
            sources,
            Some(std::path::Path::new("C:/items/source")),
            EntryKind::Folder,
            false,
        ));
        // Path-component descendants are left to the operation worker's identity/handle checks.
        assert!(internal_drop_target_is_valid(
            sources,
            Some(std::path::Path::new("C:/items/source/child")),
            EntryKind::Folder,
            false,
        ));
    }

    #[test]
    fn internal_drag_snapshot_is_consumed_once() {
        let snapshot = InternalDragSnapshot {
            sources: Vec::new(),
            touch_start_position: None,
            location_generation: 1,
            presentation_revision: 2,
            session_id: 1,
        };
        let slot = RefCell::new(Some(snapshot));
        assert!(take_internal_drag_snapshot(&slot).is_some());
        assert!(take_internal_drag_snapshot(&slot).is_none());
    }

    #[test]
    fn native_drag_candidate_peek_preserves_snapshot_for_internal_drop() {
        let snapshot = InternalDragSnapshot {
            sources: vec![],
            touch_start_position: None,
            location_generation: 2,
            presentation_revision: 4,
            session_id: 6,
        };
        let slot = RefCell::new(Some(snapshot.clone()));
        let peeked = peek_internal_drag_snapshot(&slot).expect("candidate can inspect snapshot");
        assert_eq!(peeked.session_id, snapshot.session_id);
        assert_eq!(peeked.location_generation, snapshot.location_generation);
        assert_eq!(peeked.presentation_revision, snapshot.presentation_revision);
        assert!(take_internal_drag_snapshot(&slot).is_some());
    }

    #[test]
    fn a_new_pointer_down_invalidates_an_older_native_drag_session() {
        assert!(internal_drag_session_id_is_current(4, 4));
        assert!(!internal_drag_session_id_is_current(5, 4));
        assert!(!internal_drag_session_id_is_current(0, 0));
    }

    #[gpui::test]
    fn internal_drag_uses_gpui_pointer_drag_and_drop_routing(cx: &mut gpui::TestAppContext) {
        let drop_count = Rc::new(RefCell::new(0));
        let (view, visual) = cx.add_window_view({
            let drop_count = drop_count.clone();
            move |_, _| InternalDragRouteHarness {
                entries: vec![
                    drag_test_entry(Some("C:/items/source.txt"), EntryKind::File, None),
                    drag_test_entry(Some("C:/items/target"), EntryKind::Folder, None),
                ],
                snapshot: Rc::new(RefCell::new(None)),
                drop_count,
            }
        });
        let modifiers = gpui::Modifiers::default();
        visual.simulate_mouse_down(
            gpui::point(px(40.0), px(40.0)),
            gpui::MouseButton::Left,
            modifiers,
        );
        visual.simulate_mouse_move(
            gpui::point(px(150.0), px(40.0)),
            Some(gpui::MouseButton::Left),
            modifiers,
        );
        visual.simulate_mouse_up(
            gpui::point(px(150.0), px(40.0)),
            gpui::MouseButton::Left,
            modifiers,
        );

        visual.read_entity(&view, |view, _| {
            assert_eq!(*view.drop_count.borrow(), 1);
        });
    }

    #[gpui::test]
    fn actual_directory_item_surface_emits_guarded_move_request(cx: &mut gpui::TestAppContext) {
        for layout in [ViewLayout::Rows, ViewLayout::Icons] {
            let (requests, bubbled_mouse_ups, event_order, source_tab_id, worker_claimed) =
                run_actual_directory_drag(
                    cx,
                    layout,
                    EntryKind::Folder,
                    PostPointerDownChange::None,
                );
            assert_eq!(bubbled_mouse_ups, 0);
            assert_eq!(event_order, vec!["cancel"]);
            assert_eq!(requests.len(), 1);
            assert!(
                !worker_claimed,
                "a submitted test Move stays queued without claiming an executor worker"
            );
            let request = &requests[0];
            assert_eq!(request.kind, operations::model::OperationKind::Move);
            assert_eq!(
                request.sources,
                vec![
                    std::path::PathBuf::from("C:/drag/source-a.txt"),
                    std::path::PathBuf::from("C:/drag/source-b.txt"),
                ]
            );
            assert_eq!(request.destination, Some("C:/drag/target".into()));
            let guard = request
                .move_identity_guard
                .as_ref()
                .expect("a D&D move must carry captured filesystem identities");
            assert_eq!(guard.source_identities.len(), 2);
            assert_eq!(
                request.origin,
                Some(operations::model::OperationOrigin {
                    view_id: source_tab_id as u64,
                    location_epoch: 7,
                    presentation_revision: 11,
                })
            );
        }

        let (invalid_requests, bubbled_mouse_ups, event_order, _, _) = run_actual_directory_drag(
            cx,
            ViewLayout::Rows,
            EntryKind::File,
            PostPointerDownChange::None,
        );
        assert!(invalid_requests.is_empty());
        assert_eq!(bubbled_mouse_ups, 0);
        assert_eq!(event_order, vec!["cancel"]);
        for change in [
            PostPointerDownChange::SourceIdentity,
            PostPointerDownChange::LocationGeneration,
            PostPointerDownChange::PresentationRevision,
        ] {
            let (stale_requests, bubbled_mouse_ups, event_order, _, _) =
                run_actual_directory_drag(cx, ViewLayout::Rows, EntryKind::Folder, change);
            assert!(stale_requests.is_empty());
            match change {
                PostPointerDownChange::SourceIdentity => {
                    assert_eq!(bubbled_mouse_ups, 0);
                    assert_eq!(event_order, vec!["cancel"]);
                }
                PostPointerDownChange::LocationGeneration
                | PostPointerDownChange::PresentationRevision => {
                    // 再描画でdrag元が差し替わると、GPUIはdragを開始せずMouseUpをShellまで
                    // 届ける。Shell側が保留中のnative dragを解放し、操作要求は出さない。
                    assert_eq!(bubbled_mouse_ups, 1);
                    assert!(event_order.is_empty());
                }
                PostPointerDownChange::None => unreachable!(),
            }
        }
    }

    #[gpui::test]
    fn external_file_drop_routes_copy_and_consumes_ineligible_rows(cx: &mut gpui::TestAppContext) {
        for (background, target_path) in [(false, Some("C:/drag/folder")), (true, None)] {
            let (requests, bubbled, effect) = run_actual_external_drop(
                cx,
                target_path,
                EntryKind::Folder,
                None,
                background,
                false,
                ExternalDropChange::None,
                None,
                false,
                false,
            );
            assert_eq!(bubbled, 0);
            assert_eq!(effect, Some(gpui::FileDropEffect::Copy));
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.kind, operations::model::OperationKind::Copy);
            assert_eq!(
                request.sources,
                vec![std::path::PathBuf::from("C:/outside/source.bin")]
            );
            assert_eq!(
                request.destination,
                Some(target_path.unwrap_or("C:/drag").into())
            );
            assert!(request.move_identity_guard.is_none());
            assert_eq!(
                request
                    .origin
                    .as_ref()
                    .map(|origin| (origin.location_epoch, origin.presentation_revision)),
                Some((7, 11))
            );
        }

        for (target_path, target_kind, link) in [
            (Some("C:/drag/file.txt"), EntryKind::File, None),
            (
                Some("C:/drag/link"),
                EntryKind::Folder,
                Some(LinkKind::LinkOrJunction),
            ),
            (None, EntryKind::Folder, None),
        ] {
            let (requests, bubbled, effect) = run_actual_external_drop(
                cx,
                target_path,
                target_kind,
                link,
                false,
                false,
                ExternalDropChange::None,
                None,
                false,
                false,
            );
            assert!(requests.is_empty());
            assert_eq!(bubbled, 0);
            assert_eq!(effect, None);
        }

        let (requests, bubbled, effect) = run_actual_external_drop(
            cx,
            None,
            EntryKind::Folder,
            None,
            true,
            true,
            ExternalDropChange::None,
            None,
            false,
            false,
        );
        assert!(requests.is_empty());
        assert_eq!(bubbled, 0);
        assert_eq!(effect, None);

        for change in [
            ExternalDropChange::LocationGeneration,
            ExternalDropChange::PresentationRevision,
            ExternalDropChange::TargetIdentity,
            ExternalDropChange::MissingSession,
            ExternalDropChange::StaleSourceView,
        ] {
            let (requests, bubbled, effect) = run_actual_external_drop(
                cx,
                Some("C:/drag/folder"),
                EntryKind::Folder,
                None,
                false,
                false,
                change,
                None,
                false,
                false,
            );
            assert!(requests.is_empty(), "stale drop accepted after {change:?}");
            assert_eq!(bubbled, 0);
            assert_eq!(effect, None);
        }

        let (local_requests, bubbled, local_effect) = run_actual_external_drop_with_policy(
            cx,
            Some("C:/drag/folder"),
            EntryKind::Folder,
            None,
            false,
            false,
            ExternalDropChange::None,
            Some(0x1234),
            false,
            false,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            true,
            false,
            None,
            false,
        );
        assert_eq!(local_requests.len(), 1);
        assert_eq!(
            local_requests[0].kind,
            operations::model::OperationKind::Move
        );
        assert_eq!(
            local_requests[0].sources,
            vec![std::path::PathBuf::from("C:/outside/source.bin")]
        );
        assert_eq!(
            bubbled, 1,
            "the intentionally repeated local Submit is rejected and its synthetic MouseUp reaches the harness parent"
        );
        assert_eq!(local_effect, Some(gpui::FileDropEffect::Copy));

        for (policy, expected_effect, paths) in [
            (
                win_shell_dnd::FileDragPinPolicy::MoveCapable,
                gpui::FileDropEffect::Move,
                vec![
                    std::path::PathBuf::from("C:/drop/regular-a.txt"),
                    std::path::PathBuf::from("C:/drop/regular-b.txt"),
                ],
            ),
            (
                win_shell_dnd::FileDragPinPolicy::CopyOnly,
                gpui::FileDropEffect::Copy,
                vec![
                    std::path::PathBuf::from("D:/drop/folder"),
                    std::path::PathBuf::from("D:/drop/mixed-file.txt"),
                ],
            ),
        ] {
            let (requests, bubbled, effect) = run_actual_external_drop_with_policy(
                cx,
                Some("C:/drag/folder"),
                EntryKind::Folder,
                None,
                false,
                false,
                ExternalDropChange::None,
                Some(0x1234),
                false,
                false,
                policy,
                Some(paths.clone()),
                false,
                false,
                None,
                false,
            );
            assert_eq!(
                effect,
                Some(expected_effect),
                "effect for policy {policy:?}"
            );
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].kind, operations::model::OperationKind::Move);
            assert_eq!(requests[0].sources, paths);
            assert_eq!(bubbled, 0);
        }

        let (capacity_requests, bubbled, capacity_effect) = run_actual_external_drop(
            cx,
            Some("C:/drag/folder"),
            EntryKind::Folder,
            None,
            false,
            false,
            ExternalDropChange::None,
            None,
            false,
            true,
        );
        assert!(
            capacity_requests.is_empty(),
            "full Runtime must reject Copy admission"
        );
        assert_eq!(bubbled, 0);
        assert_eq!(capacity_effect, None);

        let (missing_context_requests, bubbled, missing_context_effect) = run_actual_external_drop(
            cx,
            Some("C:/drag/folder"),
            EntryKind::Folder,
            None,
            false,
            false,
            ExternalDropChange::None,
            None,
            true,
            false,
        );
        assert!(missing_context_requests.is_empty());
        assert_eq!(bubbled, 0);
        assert_eq!(missing_context_effect, None);
    }

    #[gpui::test]
    fn external_file_drop_targets_folder_in_nonfocused_pane_without_switching_tabs(
        cx: &mut gpui::TestAppContext,
    ) {
        let (requests, bubbled, effect) = run_actual_external_drop_with_policy(
            cx,
            Some("C:/drop-b/target-folder"),
            EntryKind::Folder,
            None,
            false,
            false,
            ExternalDropChange::None,
            None,
            false,
            false,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            false,
            false,
            None,
            true,
        );

        assert_eq!(bubbled, 0, "the folder-row drop is consumed");
        assert_eq!(effect, Some(gpui::FileDropEffect::Copy));
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].kind, operations::model::OperationKind::Copy);
        assert_eq!(
            requests[0].sources,
            vec![std::path::PathBuf::from("C:/outside/source.bin")]
        );
        assert_eq!(
            requests[0].destination,
            Some(std::path::PathBuf::from("C:/drop-b/target-folder"))
        );
        assert_eq!(
            requests[0]
                .origin
                .as_ref()
                .map(|origin| (origin.location_epoch, origin.presentation_revision)),
            Some((7, 11))
        );
    }

    #[gpui::test]
    fn pending_touch_ole_move_edge_scrolls_the_directory_list(cx: &mut gpui::TestAppContext) {
        let (requests, bubbled, effect) = run_actual_external_drop_with_policy(
            cx,
            None,
            EntryKind::Folder,
            None,
            true,
            false,
            ExternalDropChange::None,
            Some(0x1234),
            false,
            false,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            false,
            true,
            Some(gpui::MouseInputSource::Touch),
            false,
        );

        assert!(requests.is_empty());
        assert_eq!(bubbled, 0);
        assert_eq!(effect, None);
    }

    #[gpui::test]
    fn pending_mouse_ole_move_edge_scrolls_the_directory_list(cx: &mut gpui::TestAppContext) {
        let (_, _, _) = run_actual_external_drop_with_policy(
            cx,
            None,
            EntryKind::Folder,
            None,
            true,
            false,
            ExternalDropChange::None,
            Some(0x1234),
            false,
            false,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            false,
            true,
            Some(gpui::MouseInputSource::Mouse),
            false,
        );
    }

    #[gpui::test]
    fn external_ole_drag_move_edge_scrolls_without_a_local_session(cx: &mut gpui::TestAppContext) {
        let (_, _, _) = run_actual_external_drop_with_policy(
            cx,
            None,
            EntryKind::Folder,
            None,
            true,
            false,
            ExternalDropChange::None,
            None,
            false,
            false,
            win_shell_dnd::FileDragPinPolicy::CopyOnly,
            None,
            false,
            true,
            None,
            false,
        );
    }

    #[test]
    fn refresh_with_last_good_does_not_insert_an_in_flow_status_row() {
        assert!(!listing_status_surface::show_listing_status_row(
            &directory_session::ListingStatus::Refreshing,
            true,
        ));
        assert!(listing_status_surface::show_listing_status_row(
            &directory_session::ListingStatus::InitialLoading,
            false,
        ));
        assert!(listing_status_surface::show_listing_status_row(
            &directory_session::ListingStatus::Ready,
            true,
        ));
    }

    #[test]
    fn overscroll_keeps_a_full_viewport_at_the_end() {
        assert_eq!(clamp_scroll_offset(1e12, 880_044.0, 500.0), 879_544.0);
        assert_eq!(clamp_scroll_offset(100.0, 44.0, 500.0), 0.0);
        assert_eq!(clamp_scroll_offset(-100.0, 1000.0, 500.0), 0.0);
    }

    #[test]
    fn tiles_fit_two_columns_at_initial_window_width() {
        assert_eq!(
            ViewLayout::Tiles.columns(
                840.0 - shell::DEFAULT_SIDEBAR_WIDTH,
                ICON_SCALE_DEFAULT,
                true
            ),
            2
        );
        assert_eq!(
            ViewLayout::Tiles.columns(300.0, ICON_SCALE_DEFAULT, true),
            1
        );
        assert!(
            ViewLayout::Tiles.columns(840.0, ICON_SCALE_DEFAULT, false)
                >= ViewLayout::Tiles.columns(840.0, ICON_SCALE_DEFAULT, true)
        );
    }

    #[test]
    fn icon_row_fits_inside_a_column() {
        // アイコン表示は icon の両脇に checkbox と同幅の空きを置く。
        // その行が1列の幅に収まらないと、checkbox が隣の列へはみ出す。
        let width = 840.0 - shell::DEFAULT_SIDEBAR_WIDTH;
        let columns = ViewLayout::Icons.columns(width, ICON_SCALE_DEFAULT, true);
        let cell = (width - 24.0 - 16.0 * (columns - 1) as f32) / columns as f32;
        let icon = ViewLayout::Icons.icon_size(ICON_SCALE_DEFAULT);
        let row = gpui_filer_ui::CHECKBOX_HIT_SIZE * 2.0 + CHECKBOX_GAP * 2.0 + (icon + 16.0);
        assert!(row <= cell, "row={row} cell={cell}");
    }

    #[test]
    fn icon_scale_adjusts_sizes() {
        assert_eq!(ViewLayout::Tiles.icon_size(1.0), 64.0);
        assert_eq!(ViewLayout::Tiles.icon_size(1.25), 80.0);
        assert_eq!(ViewLayout::Icons.icon_size(1.0), 80.0);
        assert_eq!(ViewLayout::Icons.icon_size(2.0), 160.0);
        // 行ビューはスケールに関係なく固定。
        assert_eq!(ViewLayout::Rows.icon_size(2.0), 20.0);
    }

    #[test]
    fn shell_icons_are_fallbacks_for_generic_files_only() {
        assert!(uses_shell_icon_fallback(IconId::File));
        assert!(!uses_shell_icon_fallback(IconId::Folder));
        assert!(!uses_shell_icon_fallback(IconId::FileText));
        assert!(!uses_shell_icon_fallback(IconId::Link));
        assert!(!uses_shell_icon_fallback(IconId::Libraries));
    }

    #[test]
    fn preview_decode_size_follows_icon_scale_and_dpi() {
        // 既定のタイルは 64×1.25=80 論理px。DPI 2 では 160device px必要。
        assert_eq!(
            preview_pixels(ViewLayout::Tiles, ICON_SCALE_DEFAULT, 2.0),
            160
        );
        assert_eq!(preview_pixels(ViewLayout::Icons, 1.0, 1.0), 80);
        assert_eq!(preview_pixels(ViewLayout::Icons, ICON_SCALE_MAX, 1.0), 200);
    }

    #[test]
    fn preview_covers_requires_enough_pixels() {
        let image = gpui::RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(80, 40))]);
        let images = std::sync::Arc::new(vec![std::sync::Arc::new(image)]);
        // 長辺が表示サイズを満たしていれば拡大されない。
        assert!(preview_covers(&images, 80));
        assert!(!preview_covers(&images, 81));
        assert!(!preview_covers(&std::sync::Arc::new(Vec::new()), 1));
    }

    #[cfg(windows)]
    #[test]
    fn alt_up_uses_pc_only_above_a_drive_root() {
        assert_eq!(
            up_destination(&Location::Path(r"C:\Users".into())),
            Some(Location::Path(r"C:\".into()))
        );
        assert_eq!(
            up_destination(&Location::Path(r"C:\".into())),
            Some(Location::ThisPc)
        );
        // UNC share は filesystem path の根元であり、PC の配下とは扱わない。
        assert_eq!(
            up_destination(&Location::Path(r"\\server\share\".into())),
            None
        );
        assert_eq!(up_destination(&Location::ThisPc), None);
        assert_eq!(
            up_destination(&Location::ShellRoot(ShellRoot::Network)),
            Some(Location::ThisPc)
        );
        assert_eq!(
            up_destination(&Location::ShellItem {
                root: ShellRoot::RecycleBin,
                parsing_name: "::{libraries}\\Documents".into(),
                display_name: "Documents".into(),
            }),
            Some(Location::ShellRoot(ShellRoot::RecycleBin))
        );
    }

    /// Home/End の行き先を、実際の `move_focus` と同じ式で確かめる。
    ///
    /// 以前は現在行の端へ動いていた。v5 の `move_focus_home` /
    /// `move_focus_end` は一覧全体の先頭と末尾へ移るので、そちらに合わせる。
    #[test]
    fn home_and_end_target_the_whole_list_not_the_current_row() {
        let count = 10;
        let columns = 3;
        // 2行目の中央(index 4)にいるとき、Home は行頭の 3 ではなく 0 へ。
        let current = Some(4);
        assert_eq!(focus_target(FocusDelta::Home, current, count, columns), 0);
        // End も行末の 5 ではなく末尾の 9 へ。
        assert_eq!(
            focus_target(FocusDelta::End, current, count, columns),
            count - 1
        );
        // 先頭にいるときの Home は動かない(呼び出し側が false を返す)。
        assert_eq!(focus_target(FocusDelta::Home, Some(0), count, columns), 0);
        // 末尾にいるときの End も動かない。
        assert_eq!(
            focus_target(FocusDelta::End, Some(count - 1), count, columns),
            count - 1
        );
        // 上下と左右は今までどおり行と隣を跨ぐ。
        assert_eq!(focus_target(FocusDelta::Up, current, count, columns), 1);
        assert_eq!(focus_target(FocusDelta::Down, current, count, columns), 7);
        assert_eq!(focus_target(FocusDelta::Left, current, count, columns), 3);
        assert_eq!(focus_target(FocusDelta::Right, current, count, columns), 5);
    }

    /// まだ何も選んでいないときは「一覧の先頭の手前」から入る。
    ///
    /// 以前は index 0 にいるものとして扱っていたため、下を押すと 1 件目を飛ばして
    /// 2 件目へ入っていた。v5 は読み込み時に最初の項目へフォーカスを置いていたので、
    /// そこから動かすのと同じ位置へ入るのが正しい。
    #[test]
    fn the_first_arrow_enters_at_the_first_item() {
        let count = 4;
        let columns = 1;
        for delta in [
            FocusDelta::Down,
            FocusDelta::Right,
            FocusDelta::Up,
            FocusDelta::Left,
            FocusDelta::Home,
        ] {
            assert_eq!(
                focus_target(delta, None, count, columns),
                first_item_index(),
                "{delta:?} should enter at the first item"
            );
        }
        // End だけは末尾へ行く。フォーカスが無くても一発で届く。
        assert_eq!(
            focus_target(FocusDelta::End, None, count, columns),
            count - 1
        );
        // 1 件目へ入ったあとは今までどおり隣へ動く。
        assert_eq!(
            focus_target(FocusDelta::Down, Some(first_item_index()), count, columns),
            1
        );
    }

    /// hover 条件の binding は hover している項目を、それ以外は focus している項目を動かす。
    #[test]
    fn a_key_resolved_from_hover_targets_the_hovered_item() {
        assert_eq!(
            key_target_index(InputRoute::Hover, Some(3), Some(7)),
            Some(3)
        );
        assert_eq!(
            key_target_index(InputRoute::Focus, Some(3), Some(7)),
            Some(7)
        );
        // 対象が無ければ動かさない。当たったことを消費にもしない。
        assert_eq!(key_target_index(InputRoute::Hover, None, Some(7)), None);
        assert_eq!(key_target_index(InputRoute::Focus, Some(3), None), None);
    }

    #[test]
    fn recycle_bin_file_rows_have_no_filesystem_operation_target() {
        let row = special::ShellEntry {
            name: "deleted.txt".into(),
            target: None,
            identity: "::{recycle-bin}\\deleted.txt".into(),
            kind: special::ShellEntryKind::File,
        };
        let (entry, _key, target) = shell_entry(&row);
        assert!(entry.path.is_none());
        assert!(target.is_none());
    }
}

/// `--demo` は列挙を伴わない開発用の一覧。保存の対象にしない。
///
/// 保存を読むと `--demo` が上書きされ、書くと開発用の一覧で本物の保存を潰す。
fn demo_requested() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--demo"))
}

fn main() {
    #[cfg(windows)]
    if let Some(code) = archive_provider::run_seven_zip_helper_from_process_args() {
        if code != 0 {
            eprintln!("7-Zip helper failed");
        }
        std::process::exit(code);
    }
    if everything::run_helper_from_process_args() {
        return;
    }
    if file_info_shortcut::run_helper_from_process_args() {
        return;
    }
    match operations::privileged::run_broker_from_process_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("[v6][privileged-broker] {error}");
            std::process::exit(1);
        }
    }
    #[cfg(windows)]
    archive_provider::application_image::initialize();
    let platform = Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("failed to initialize Windows platform"),
    );
    Application::with_platform(platform)
        .with_assets(ResourceAssets)
        .run(|cx: &mut App| {
            // Open the optional trace before GPUI begins dispatching input.
            if ole_diagnostics::initialize() == ole_diagnostics::InitStatus::Failed {
                eprintln!("[v6][ole-trace] setup failed (open-or-writer)");
            }
            // 7-Zip は任意機能。PATH の検出は UI を止めず、結果だけを世代付きで commit する。
            let archive_provider = archive_provider::ArchiveProviderOwner::default();
            let discovery_revision = archive_provider.begin_discovery();
            let discovery_path = std::env::var_os("PATH");
            let discovery_cwd = std::env::current_dir().ok();
            cx.set_global(archive_provider.clone());
            cx.spawn(async move |cx| {
                let executable = cx
                    .background_executor()
                    .spawn(async move {
                        discovery_cwd.and_then(|cwd| {
                            archive_provider::discover_from_path(discovery_path, cwd)
                        })
                    })
                    .await;
                // revision gateはProviderに残し、accepted commitをforegroundで行ってShellへ通知する。
                cx.update_global::<archive_provider::ArchiveProviderOwner, _>(|owner, _| {
                    owner.commit_discovery(discovery_revision, executable)
                });
            })
            .detach();
            cx.bind_keys([
                gpui::KeyBinding::new("backspace", rename_editor::Backspace, Some("RenameInput")),
                gpui::KeyBinding::new("delete", rename_editor::Delete, Some("RenameInput")),
                gpui::KeyBinding::new("left", rename_editor::Left, Some("RenameInput")),
                gpui::KeyBinding::new("right", rename_editor::Right, Some("RenameInput")),
                gpui::KeyBinding::new("home", rename_editor::Home, Some("RenameInput")),
                gpui::KeyBinding::new("end", rename_editor::End, Some("RenameInput")),
                gpui::KeyBinding::new("shift-left", rename_editor::SelectLeft, Some("RenameInput")),
                gpui::KeyBinding::new(
                    "shift-right",
                    rename_editor::SelectRight,
                    Some("RenameInput"),
                ),
                gpui::KeyBinding::new("ctrl-a", rename_editor::SelectAll, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-c", rename_editor::CopyText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-v", rename_editor::PasteText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-z", rename_editor::UndoText, Some("RenameInput")),
            ]);
            // 保存は window を作る前に読む。位置と大きさもここで決める。
            let (startup, session_autosave, folder_preferences) = if demo_requested() {
                (
                    persistence::Startup::disabled(),
                    persistence::SessionAutosaveOwner::disabled(),
                    persistence::FolderPreferencesOwner::disabled(),
                )
            } else {
                persistence::open()
            };
            // Session の writer は Shell ごとではなく、この Application が一つだけ持つ。
            cx.set_global(session_autosave);
            cx.set_global(show_hidden_items::ShowHiddenItems(
                startup
                    .restore
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.show_hidden_items),
            ));
            cx.set_global(ItemCheckBoxes(
                startup
                    .restore
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.item_check_boxes),
            ));
            cx.set_global(window_coordinator::WindowCoordinator::default());
            cx.global_mut::<persistence::SessionAutosaveOwner>()
                .set_writes_paused(true);
            // Folder preference snapshot と writer/revision も Application が一つだけ持つ。
            cx.set_global(folder_preferences);
            // Operation runtime と crash-journal writer も Shell より先に一つだけ作る。
            cx.set_global(operations::application::OperationApplicationOwner::open());
            // 起動時も active snapshot を一度だけ commit する。無いファイルは defaults のまま、
            // 壊れたファイルは defaults と診断を残す。
            let keymap_runtime = std::sync::Arc::new(input::KeymapRuntime::new());
            let prepared = input::KeymapRuntime::prepare(
                0,
                keymap::load(app_paths::keymap_file(), input::KEYMAP_TABLE),
            );
            let keymap_notices = match keymap_runtime.commit(0, prepared) {
                input::CommitResult::Applied { .. } | input::CommitResult::Stale => Vec::new(),
                input::CommitResult::Rejected { diagnostics, .. } => diagnostics,
            };
            let mut windows = startup
                .restore
                .as_ref()
                .map(|snapshot| snapshot.windows.clone())
                .unwrap_or_default();
            if windows.is_empty() {
                windows.push(session_store::WindowSessionSnapshot {
                    id: 1,
                    workspaces: Vec::new(),
                    closed_workspaces: Vec::new(),
                    active_workspace: 1,
                    window: None,
                });
            }
            let active_window = startup
                .restore
                .as_ref()
                .map(|snapshot| snapshot.active_window);
            windows.sort_by_key(|window| (Some(window.id) == active_window, window.id));
            let mut restore_complete = true;
            for saved_window in windows {
                let initial =
                    persistence::InitialWindow::restore_window(saved_window.window.as_ref(), cx);
                let options = WindowOptions {
                    window_bounds: Some(initial.bounds),
                    display_id: initial.display,
                    // タイトルバーは自分で描く。ドラッグ・最小化・最大化・閉じるは
                    // platform が `WindowControlArea` を見てやる。
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("expos".into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    touch_input_mode: gpui::TouchInputMode::Native,
                    ..Default::default()
                };
                let mut window_startup = startup.clone();
                window_startup.window_id = saved_window.id;
                if let Some(snapshot) = window_startup.restore.as_mut() {
                    snapshot.active_window = saved_window.id;
                }
                let handle = match cx.open_window(options, |_window, cx| {
                    cx.new(|cx| {
                        shell::FilerShell::new(
                            window_startup,
                            keymap_runtime.clone(),
                            keymap_notices.clone(),
                            cx,
                        )
                    })
                }) {
                    Ok(handle) => handle,
                    Err(error) => {
                        eprintln!(
                            "[v6][session] failed to materialize window {}: {error:#}",
                            saved_window.id
                        );
                        restore_complete = false;
                        break;
                    }
                };
                let Ok(shell_entity) = handle.entity(cx) else {
                    restore_complete = false;
                    break;
                };
                let incarnation = cx
                    .global_mut::<window_coordinator::WindowCoordinator>()
                    .register(saved_window.id, handle);
                let Some(incarnation) = incarnation else {
                    restore_complete = false;
                    break;
                };
                let _ = handle.update(cx, |shell, _window, _cx| {
                    shell.set_window_incarnation(incarnation);
                });
                if shell_entity.read(cx).restore_incomplete() {
                    restore_complete = false;
                }
            }
            if !restore_complete {
                cx.global_mut::<persistence::SessionAutosaveOwner>()
                    .disable_writes();
            } else {
                cx.global_mut::<persistence::SessionAutosaveOwner>()
                    .set_writes_paused(false);
            }
            if let Some(active_window) = active_window {
                cx.global_mut::<window_coordinator::WindowCoordinator>()
                    .set_active_window(active_window);
            }
        });
}
