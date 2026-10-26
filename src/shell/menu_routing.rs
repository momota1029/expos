//! Menu input routing and command delivery for `FilerShell`.

use super::*;

pub(super) fn context_menu_view_available(batch_rename_active: bool) -> bool {
    !batch_rename_active
}

#[cfg(test)]
mod batch_rename_modal_tests {
    use super::{captured_single_item_rename_target, context_menu_view_available};
    use crate::{entry_identity::EntryKey, menu};

    #[test]
    fn context_menu_admission_rejects_the_modal_owner_view() {
        assert!(context_menu_view_available(false));
        assert!(!context_menu_view_available(true));
    }

    #[test]
    fn captured_rename_row_requires_one_live_eligible_item_popup_target() {
        let key = EntryKey::demo(1);
        let other = EntryKey::demo(2);
        let item_popup = menu::Open::Popup(menu::Popup::Item);
        let matches = |open,
                       item,
                       keys: &[EntryKey],
                       eligible,
                       live_generation,
                       target_generation,
                       live_revision,
                       target_revision| {
            captured_single_item_rename_target(
                open,
                item,
                keys,
                eligible,
                live_generation,
                target_generation,
                live_revision,
                target_revision,
            )
        };

        assert!(matches(
            item_popup,
            Some(&key),
            std::slice::from_ref(&key),
            true,
            4,
            4,
            9,
            9
        ));
        assert!(!matches(
            item_popup,
            Some(&key),
            &[key.clone(), other.clone()],
            true,
            4,
            4,
            9,
            9
        ));
        assert!(!matches(
            item_popup,
            Some(&key),
            std::slice::from_ref(&key),
            false,
            4,
            4,
            9,
            9
        ));
        assert!(!matches(
            item_popup,
            Some(&other),
            std::slice::from_ref(&key),
            true,
            4,
            4,
            9,
            9
        ));
        assert!(!matches(
            menu::Open::Popup(menu::Popup::Background),
            Some(&key),
            std::slice::from_ref(&key),
            true,
            4,
            4,
            9,
            9
        ));
        assert!(!matches(
            item_popup,
            Some(&key),
            std::slice::from_ref(&key),
            true,
            5,
            4,
            9,
            9
        ));
        assert!(!matches(
            item_popup,
            Some(&key),
            std::slice::from_ref(&key),
            true,
            4,
            4,
            10,
            9
        ));
    }
}

#[cfg(test)]
mod properties_dispatch_tests {
    use super::open_properties_paths_with;
    use std::path::{Path, PathBuf};

    #[test]
    fn properties_dispatch_attempts_all_paths_after_an_error() {
        let paths = [PathBuf::from("A"), PathBuf::from("B"), PathBuf::from("C")];
        let mut attempted = Vec::new();
        open_properties_paths_with(&paths, |path: &Path| {
            attempted.push(path.to_path_buf());
            if path == Path::new("B") {
                Err("launch failed")
            } else {
                Ok(())
            }
        });
        assert_eq!(attempted, paths);
    }
}

impl FilerShell {
    pub(super) fn toggle_menu_mode_from_keyboard(&mut self, cx: &mut Context<Self>) {
        // メニュー表示で workspace tab face が消えても、キーを受ける active view に focus を移す。
        self.pending_focus = true;
        self.toggle_menu_mode(cx);
    }

    pub(super) fn toggle_menu_mode(&mut self, cx: &mut Context<Self>) {
        if !self.menu.bar_visible() && !self.menu.is_open() {
            let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
            if !policy.allowed {
                return;
            }
            if policy.close.folder_tree {
                self.close_folder_tree_menu(cx);
            }
        }
        self.menu.toggle_bar();
        self.workspace_menu_target = None;
        // バーを出すのも引くのも、開いていた面はここで消える。
        self.menu_target = None;
        self.tab_menu_target = None;
        self.menu_at = None;
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// タイトルの hover / click から menu を開く。同じ menu なら何もしない。
    ///
    /// 既に開いている menu で対象を捕まえ直さない。すると、view が変わっても世代が
    /// 更新されて、古い menu を実行しないための照合が働かなくなる。
    pub(super) fn show_menu(&mut self, menu: menu::Menu, cx: &mut Context<Self>) {
        let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
        if !policy.allowed {
            return;
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        if self.menu.open_menu() == Some(menu) {
            return;
        }
        if menu == menu::Menu::View {
            self.view_menu_scroll
                .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
            self.view_menu_root_scroll
                .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
        }
        self.menu.show_menu(menu);
        self.workspace_menu_target = None;
        self.menu_target = Some(self.capture_menu_target(None, cx));
        self.tab_menu_target = None;
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// menu モードの間のキー。
    ///
    /// Escape で menu モードごと閉じ、上下で行を選び、左右で menu を移り（端で折り返す）、
    /// Enter で選んだ行を実行する。マウスで行を指したときも同じ `highlight` を動かすので、
    /// 選択の見た目は一つだけ。
    pub(super) fn handle_menu_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match keystroke.key.as_str() {
            "escape" => {
                if self.menu.view_submenu().is_some() {
                    self.menu.close_submenu();
                    cx.notify();
                } else {
                    self.leave_menu(cx);
                }
                true
            }
            "up" => {
                self.move_menu_highlight(-1, cx);
                true
            }
            "down" => {
                self.move_menu_highlight(1, cx);
                true
            }
            "left" => {
                if self.menu.view_submenu().is_some() {
                    self.menu.close_submenu();
                    cx.notify();
                } else {
                    self.step_menu(-1, cx);
                }
                true
            }
            "right" => {
                if (self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
                    || matches!(
                        self.menu.open(),
                        Some(menu::Open::Popup(
                            menu::Popup::Item | menu::Popup::Background
                        ))
                    ))
                    && self.menu.view_submenu().is_none()
                {
                    let rows = if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View)) {
                        self.view_menu_root_rows(cx)
                    } else {
                        self.context_menu_root_rows(self.menu_entries(cx))
                    };
                    if matches!(
                        rows.get(self.menu.highlight_index()),
                        Some(menu::Row::Branch { .. })
                    ) {
                        self.open_view_submenu_from_keyboard(self.menu.highlight_index(), cx);
                    } else {
                        self.step_menu(1, cx);
                    }
                } else if self.menu.view_submenu().is_none() {
                    self.step_menu(1, cx);
                }
                true
            }
            "enter" => {
                self.run_highlighted_row(window, cx);
                true
            }
            // Menu is the top modal surface; do not let unhandled keys reach lower routes.
            _ => true,
        }
    }

    /// 開いている面の行。availability は menu を組むのと同じ表から読む。
    pub(super) fn menu_entries(&self, cx: &Context<Self>) -> Vec<menu::Entry> {
        let Some(open) = self.menu.open() else {
            return Vec::new();
        };
        if self
            .menu_target
            .as_ref()
            .is_some_and(|target| target.workspace != self.workspace.active)
            || self
                .tab_menu_target
                .as_ref()
                .is_some_and(|target| target.workspace != self.workspace.active)
        {
            return Vec::new();
        }
        if open == menu::Open::Popup(menu::Popup::Workspace) {
            let Some(target) = self.workspace_menu_target else {
                return Vec::new();
            };
            if !self.workspace.ordered.iter().any(|(id, _)| *id == target) {
                return Vec::new();
            }
            return vec![menu::Entry {
                action: gpui_filer_input::ActionId::new(menu::WORKSPACE_DUPLICATE),
                label: self
                    .translator
                    .text(i18n::Message::DuplicateWorkspace)
                    .into_owned(),
                enabled: true,
                checked: false,
            }];
        }
        if open == menu::Open::Popup(menu::Popup::Tab) {
            return self
                .tab_menu_target
                .as_ref()
                .map_or_else(Vec::new, |target| {
                    let mut rows = vec![
                        menu::Entry {
                            action: gpui_filer_input::ActionId::new(menu::TAB_DUPLICATE),
                            label: self
                                .translator
                                .text(i18n::Message::TabDuplicate)
                                .into_owned(),
                            enabled: true,
                            checked: false,
                        },
                        menu::Entry {
                            action: gpui_filer_input::ActionId::new(menu::TAB_CLOSE),
                            label: self.translator.text(i18n::Message::TabClose).into_owned(),
                            enabled: true,
                            checked: false,
                        },
                    ];
                    if target.snapshot.len() > 1 {
                        rows.push(menu::Entry {
                            action: gpui_filer_input::ActionId::new(menu::TAB_CLOSE_OTHERS),
                            label: self
                                .translator
                                .text(i18n::Message::TabCloseOthers)
                                .into_owned(),
                            enabled: true,
                            checked: false,
                        });
                    }
                    if target
                        .snapshot
                        .iter()
                        .position(|(id, _)| *id == target.id)
                        .is_some_and(|index| index + 1 < target.snapshot.len())
                    {
                        rows.push(menu::Entry {
                            action: gpui_filer_input::ActionId::new(menu::TAB_CLOSE_TO_RIGHT),
                            label: self
                                .translator
                                .text(i18n::Message::TabCloseToRight)
                                .into_owned(),
                            enabled: true,
                            checked: false,
                        });
                    }
                    let locked = target
                        .snapshot
                        .iter()
                        .find(|(id, _)| *id == target.id)
                        .is_some_and(|(_, locked)| *locked);
                    rows.push(menu::Entry {
                        action: gpui_filer_input::ActionId::new(menu::TAB_TOGGLE_LOCK),
                        label: self
                            .translator
                            .text(if locked {
                                i18n::Message::TabUnlock
                            } else {
                                i18n::Message::TabLock
                            })
                            .into_owned(),
                        enabled: true,
                        checked: locked,
                    });
                    rows
                });
        }
        if open == menu::Open::Popup(menu::Popup::ReopenTabs) {
            return self
                .reopen_menu_targets
                .as_ref()
                .map_or_else(Vec::new, |targets| {
                    if targets.is_empty() {
                        return vec![menu::Entry {
                            action: ActionId::new(menu::REOPEN_TAB),
                            label: self
                                .translator
                                .text(i18n::Message::NoClosedTabs)
                                .into_owned(),
                            enabled: false,
                            checked: false,
                        }];
                    }
                    let records = targets
                        .iter()
                        .filter_map(|id| {
                            let record = self.closed_tabs.iter().find(|record| record.id == *id)?;
                            let (title, path, parents) = match &record.values.location {
                                crate::location::Location::Path(path) => {
                                    let title = path.file_name().map_or_else(
                                        || path.display().to_string(),
                                        |name| name.to_string_lossy().into_owned(),
                                    );
                                    let parents = path
                                        .parent()
                                        .into_iter()
                                        .flat_map(std::path::Path::components)
                                        .rev()
                                        .map(|component| {
                                            component.as_os_str().to_string_lossy().into_owned()
                                        })
                                        .collect::<Vec<_>>();
                                    (title, path.display().to_string(), parents)
                                }
                                crate::location::Location::Archive { source, components } => {
                                    let title = components.last().cloned().unwrap_or_else(|| {
                                        source
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| source.display().to_string())
                                    });
                                    let mut parents =
                                        components.iter().rev().cloned().collect::<Vec<_>>();
                                    parents.extend(
                                        source
                                            .parent()
                                            .into_iter()
                                            .flat_map(std::path::Path::components)
                                            .rev()
                                            .map(|component| {
                                                component.as_os_str().to_string_lossy().into_owned()
                                            }),
                                    );
                                    let path = if components.is_empty() {
                                        format!("{}!", source.display())
                                    } else {
                                        format!("{}!/{}", source.display(), components.join("/"))
                                    };
                                    (title, path, parents)
                                }
                                crate::location::Location::ThisPc => {
                                    let name =
                                        self.translator.text(i18n::Message::ThisPc).into_owned();
                                    (name.clone(), name, Vec::new())
                                }
                                crate::location::Location::ShellRoot(root) => {
                                    let name = root.display_name().to_owned();
                                    (name.clone(), name, Vec::new())
                                }
                                crate::location::Location::ShellItem { display_name, .. } => {
                                    (display_name.clone(), display_name.clone(), Vec::new())
                                }
                            };
                            Some((*id, title, path, parents))
                        })
                        .collect::<Vec<_>>();
                    targets
                        .iter()
                        .map(|id| {
                            let record = records.iter().find(|record| record.0 == *id);
                            let label =
                                record.map_or_else(String::new, |(_, title, path, parents)| {
                                    let same_name = records
                                        .iter()
                                        .filter(|other| other.1 == *title)
                                        .collect::<Vec<_>>();
                                    if same_name.len() < 2 {
                                        return format!("{title} — {path}");
                                    }
                                    let suffix = (1..=parents.len()).find_map(|depth| {
                                        let candidate = parents
                                            .iter()
                                            .take(depth)
                                            .rev()
                                            .cloned()
                                            .collect::<Vec<_>>()
                                            .join("\\");
                                        let unique = same_name
                                            .iter()
                                            .filter(|other| {
                                                let other_suffix = other
                                                    .3
                                                    .iter()
                                                    .take(depth)
                                                    .rev()
                                                    .cloned()
                                                    .collect::<Vec<_>>()
                                                    .join("\\");
                                                other_suffix == candidate
                                            })
                                            .count()
                                            == 1;
                                        unique.then_some(candidate)
                                    });
                                    let suffix = suffix.unwrap_or_else(|| {
                                        let ordinal = same_name
                                            .iter()
                                            .position(|other| other.0 == *id)
                                            .unwrap_or(0)
                                            + 1;
                                        parents.first().map_or_else(
                                            || format!("#{ordinal}"),
                                            |parent| format!("{parent} #{ordinal}"),
                                        )
                                    });
                                    format!("{title} — {suffix} — {path}")
                                });
                            menu::Entry {
                                action: ActionId::new(menu::REOPEN_TAB),
                                label,
                                enabled: record.is_some(),
                                checked: false,
                            }
                        })
                        .collect()
                });
        }
        let menu_target = self.menu_target.as_ref();
        let view = if let Some(target) = menu_target {
            let Some(view_id) = target.view else {
                return Vec::new();
            };
            let Some((pane, tab)) = self.workspace.active().pane_and_tab(target.tab, view_id)
            else {
                return Vec::new();
            };
            if pane.id != target.pane || pane.directory().active_tab_id != target.tab {
                return Vec::new();
            }
            Some(tab.view.clone())
        } else {
            self.active_view()
        };
        let Some(view) = view else {
            return Vec::new();
        };
        // popup の対象は開いた時に捕まえた項目。メニューバーの dropdown は
        // focus している項目を見る。
        let (mut context, focused_item) = view.read(cx).menu_context();
        if let (menu::Open::Popup(menu::Popup::Item), Some(target)) = (open, menu_target) {
            // Rename availability describes the immutable context target, not the live selection.
            context.rename_targets = usize::from(target.rename_eligible);
        }
        let has_item = match open {
            menu::Open::Popup(_) => self
                .menu_target
                .as_ref()
                .is_some_and(|target| target.item.is_some()),
            menu::Open::Menu(_) => focused_item,
        };
        let mut rows = menu::rows(
            open,
            &context,
            has_item,
            &self.translator,
            self.select_extension_on_rename,
        );
        if open == menu::Open::Popup(menu::Popup::Background)
            && menu_target.is_some_and(|target| !target.clipboard_has_files)
        {
            rows.retain(|row| {
                !matches!(
                    row.action.as_str(),
                    input::PASTE_LINK | input::PASTE_JUNCTION
                )
            });
        }
        if open == menu::Open::Popup(menu::Popup::Item) {
            rows.retain(|row| row.action.as_str() != input::PASTE_LINK);
        }
        if open == menu::Open::Popup(menu::Popup::Background) {
            let captured_path = menu_target.and_then(|target| {
                view.read(cx)
                    .current_path_for_background_menu_target(target)
            });
            if captured_path.is_none() {
                rows.retain(|row| row.action.as_str() != input::COPY_CURRENT_PATH);
            } else if let Some(row) = rows
                .iter_mut()
                .find(|row| row.action.as_str() == input::COPY_CURRENT_PATH)
            {
                row.enabled = true;
            }
        }
        if open == menu::Open::Popup(menu::Popup::Item)
            && let Some(target) = menu_target
            && !view.read(cx).item_menu_target_navigates(
                target,
                cx.try_global::<crate::archive_provider::ArchiveProviderOwner>()
                    .is_some_and(|owner| owner.snapshot().seven_zip_read_binding().is_some()),
            )
        {
            rows.retain(|row| row.action.as_str() != input::ACTIVATE_NEW_TAB);
        }
        if open == menu::Open::Popup(menu::Popup::Item)
            && let Some(target) = menu_target
            && view.read(cx).copy_paths_for_menu_target(target).is_none()
        {
            rows.retain(|row| row.action.as_str() != input::COPY_PATHS);
        }
        // Rename は共通 item popup の既定行から外し、捕捉した単一対象が
        // 現在も同じ listing projection に属するときだけ表示する。
        rows.retain(|row| row.action.as_str() != input::RENAME);
        if let (menu::Open::Popup(menu::Popup::Item), Some(target)) = (open, menu_target)
            && captured_single_item_rename_target(
                open,
                target.item.as_ref(),
                &target.context_keys,
                target.rename_eligible,
                view.read(cx).generation(),
                target.generation,
                view.read(cx).presentation_revision,
                target.presentation_revision,
            )
        {
            rows.push(menu::Entry {
                action: ActionId::new(input::RENAME),
                label: self.translator.text(i18n::Message::Rename).into_owned(),
                enabled: true,
                checked: false,
            });
        }
        if open == menu::Open::Popup(menu::Popup::Item)
            && let Some(target) = menu_target
            && target.item.is_some()
        {
            let source_scope_matches = if target.selection_derived {
                target.context_keys == target.selected_keys
                    && target.item.as_ref().is_some_and(|clicked| {
                        target.selected_keys.iter().any(|key| key == clicked)
                    })
                    && menu::selection_matches(
                        &target.selected_keys.iter().cloned().collect(),
                        &view.read(cx).selected_entry_keys(),
                    )
            } else {
                target.context_keys.as_slice()
                    == target.item.iter().cloned().collect::<Vec<_>>().as_slice()
            };
            let clipboard_copy_eligible = source_scope_matches
                && view.read(cx).clipboard_copy_eligible_keys(
                    &target.context_keys,
                    target.generation,
                    target.presentation_revision,
                );
            let clipboard_cut_eligible = source_scope_matches
                && view.read(cx).clipboard_sources_eligible_keys(
                    &target.context_keys,
                    target.generation,
                    target.presentation_revision,
                );
            if clipboard_copy_eligible {
                rows.push(menu::Entry {
                    action: ActionId::new(input::COPY),
                    label: "Copy".into(),
                    enabled: true,
                    checked: false,
                });
            }
            if clipboard_cut_eligible {
                rows.push(menu::Entry {
                    action: ActionId::new(input::CUT),
                    label: "Cut".into(),
                    enabled: true,
                    checked: false,
                });
            }
            if source_scope_matches
                && view.read(cx).trash_sources_eligible_keys(
                    &target.context_keys,
                    target.generation,
                    target.presentation_revision,
                )
            {
                rows.push(menu::Entry {
                    action: ActionId::new(input::DELETE_TO_TRASH),
                    label: self
                        .translator
                        .text(i18n::Message::RecoveryKindTrash)
                        .into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            if target.paste_target.is_some() && target.clipboard_has_files {
                rows.push(menu::Entry {
                    action: ActionId::new(input::PASTE_HERE),
                    label: "Paste Here".into(),
                    enabled: true,
                    checked: false,
                });
                rows.push(menu::Entry {
                    action: ActionId::new(input::PASTE_LINK),
                    label: self
                        .translator
                        .text(i18n::Message::PasteLinkHere)
                        .into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            if view
                .read(cx)
                .zip_create_request_for_menu_target(target)
                .is_some()
            {
                rows.push(menu::Entry {
                    action: ActionId::new(menu::CREATE_ZIP),
                    label: self.translator.text(i18n::Message::CreateZip).into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            if view
                .read(cx)
                .shortcut_create_request_for_menu_target(target)
                .is_some()
            {
                rows.push(menu::Entry {
                    action: ActionId::new(menu::CREATE_SHORTCUT),
                    label: self.translator.text(i18n::Message::CreateShortcut).into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            if view
                .read(cx)
                .seven_zip_create_request_for_menu_target(
                    target,
                    &cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                        |owner, _| owner.snapshot(),
                    ),
                )
                .is_some()
            {
                rows.push(menu::Entry {
                    action: ActionId::new(menu::CREATE_7Z),
                    label: self.translator.text(i18n::Message::Create7z).into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            let provider =
                cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(|owner, _| {
                    owner.snapshot()
                });
            if view
                .read(cx)
                .zip_extract_request_for_menu_target(target)
                .is_some()
                || view
                    .read(cx)
                    .seven_zip_extract_request_for_menu_target(target, &provider)
                    .is_some()
            {
                rows.push(menu::Entry {
                    action: ActionId::new(menu::EXTRACT_TO_FOLDER),
                    label: self
                        .translator
                        .text(i18n::Message::ExtractToFolder)
                        .into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
            if view
                .read(cx)
                .terminal_request_for_menu_target(target)
                .is_some()
            {
                rows.push(menu::Entry {
                    action: ActionId::new(menu::OPEN_IN_TERMINAL),
                    label: self
                        .translator
                        .text(i18n::Message::OpenInWindowsTerminal)
                        .into_owned(),
                    enabled: true,
                    checked: false,
                });
            }
        }
        if open == menu::Open::Popup(menu::Popup::Background)
            && let Some(target) = menu_target
            && view
                .read(cx)
                .terminal_request_for_menu_target(target)
                .is_some()
        {
            rows.push(menu::Entry {
                action: ActionId::new(menu::OPEN_IN_TERMINAL),
                label: self
                    .translator
                    .text(i18n::Message::OpenInWindowsTerminal)
                    .into_owned(),
                enabled: true,
                checked: false,
            });
        }
        if open == menu::Open::Popup(menu::Popup::Background)
            && let Some(target) = menu_target
            && target.clipboard_has_files
            && target
                .background_destination
                .as_ref()
                .is_some_and(|destination| {
                    view.read(cx).accepts_paste_target(
                        destination,
                        target.generation,
                        target.presentation_revision,
                    )
                })
        {
            rows.push(menu::Entry {
                action: ActionId::new(input::PASTE),
                label: "Paste".into(),
                enabled: true,
                checked: false,
            });
        }
        for row in &mut rows {
            if row.action.as_str() == input::TOGGLE_FILE_INFO_PANE {
                row.enabled = true;
                row.checked = self.file_info_open;
            }
            let column = match row.action.as_str() {
                input::TOGGLE_COLUMN_KIND => Some(crate::sorting::Column::Kind),
                input::TOGGLE_COLUMN_SIZE => Some(crate::sorting::Column::Size),
                input::TOGGLE_COLUMN_MODIFIED => Some(crate::sorting::Column::Modified),
                _ => None,
            };
            if let Some(column) = column {
                row.checked = view.read(cx).row_column_visibility(column).unwrap_or(false);
                row.enabled = view.read(cx).row_column_visibility(column).is_some();
            }
            let move_action = match row.action.as_str() {
                input::MOVE_NAME_LEFT => Some((crate::sorting::Column::Name, -1isize)),
                input::MOVE_NAME_RIGHT => Some((crate::sorting::Column::Name, 1)),
                input::MOVE_KIND_LEFT => Some((crate::sorting::Column::Kind, -1)),
                input::MOVE_KIND_RIGHT => Some((crate::sorting::Column::Kind, 1)),
                input::MOVE_SIZE_LEFT => Some((crate::sorting::Column::Size, -1)),
                input::MOVE_SIZE_RIGHT => Some((crate::sorting::Column::Size, 1)),
                input::MOVE_MODIFIED_LEFT => Some((crate::sorting::Column::Modified, -1)),
                input::MOVE_MODIFIED_RIGHT => Some((crate::sorting::Column::Modified, 1)),
                _ => None,
            };
            if let Some((column, direction)) = move_action {
                let order = view.read(cx).row_column_order();
                row.enabled = order.is_some_and(|order| {
                    let index = order.iter().position(|item| *item == column);
                    index.is_some_and(|index| {
                        let target = index as isize + direction;
                        (0..4).contains(&target)
                    })
                });
            }
        }
        let open_in_new_pane = (open == menu::Open::Popup(menu::Popup::Item))
            .then_some(menu_target)
            .flatten()
            .is_some_and(|target| {
                !target.selection_derived
                    && target.context_keys.len() == 1
                    && target.item.as_ref() == target.context_keys.first()
                    && matches!(
                        target.item.as_ref(),
                        Some(entry_identity::EntryKey::Path {
                            kind: crate::EntryKind::Folder,
                            ..
                        })
                    )
                    && self.workspace.active().next_pane_id.is_some()
            });
        if open_in_new_pane {
            rows.push(menu::Entry {
                action: ActionId::new(menu::OPEN_IN_NEW_PANE),
                label: self
                    .translator
                    .text(i18n::Message::OpenInNewPane)
                    .into_owned(),
                enabled: true,
                checked: false,
            });
        }
        let properties = cfg!(windows)
            && (open == menu::Open::Popup(menu::Popup::Item))
                .then_some(menu_target)
                .flatten()
                .is_some_and(|target| {
                    view.read(cx)
                        .properties_paths_for_menu_target(target)
                        .is_some()
                });
        if properties {
            rows.push(menu::Entry {
                action: ActionId::new(input::PROPERTIES),
                label: self.translator.text(i18n::Message::Properties).into_owned(),
                enabled: true,
                checked: false,
            });
        }
        if open == menu::Open::Popup(menu::Popup::Item)
            && let Some(target) = menu_target
            && target.item.is_some()
        {
            let selection_scope_matches = if target.selection_derived {
                target.context_keys == target.selected_keys
                    && target.item.as_ref().is_some_and(|clicked| {
                        target.selected_keys.iter().any(|key| key == clicked)
                    })
                    && menu::selection_matches(
                        &target.selected_keys.iter().cloned().collect(),
                        &view.read(cx).selected_entry_keys(),
                    )
            } else {
                target.context_keys.as_slice()
                    == target.item.iter().cloned().collect::<Vec<_>>().as_slice()
            };
            let enabled = selection_scope_matches
                && view.read(cx).batch_rename_keys_eligible(
                    &target.context_keys,
                    target.generation,
                    target.presentation_revision,
                );
            if enabled {
                rows.push(menu::Entry {
                    action: ActionId::new(input::BATCH_RENAME),
                    label: "Batch Rename".into(),
                    enabled: true,
                    checked: false,
                });
            }
        }
        if open == menu::Open::Menu(menu::Menu::View) {
            rows.push(menu::Entry {
                action: ActionId::new(input::TOGGLE_SHOW_HIDDEN_ITEMS),
                label: self
                    .translator
                    .text(i18n::Message::ShowHiddenItems)
                    .into_owned(),
                enabled: true,
                checked: cx
                    .try_global::<crate::show_hidden_items::ShowHiddenItems>()
                    .is_some_and(|setting| setting.0),
            });
            rows.push(menu::Entry {
                action: ActionId::new(input::TOGGLE_ITEM_CHECK_BOXES),
                label: self
                    .translator
                    .text(i18n::Message::ItemCheckBoxes)
                    .into_owned(),
                enabled: true,
                checked: cx
                    .try_global::<crate::ItemCheckBoxes>()
                    .is_none_or(|setting| setting.0),
            });
        }
        if matches!(open, menu::Open::Popup(_)) {
            // Context menus list only actions that the captured target can run now.
            rows.retain(|row| row.enabled);
        }
        rows
    }

    pub(super) fn view_menu_root_rows(&self, cx: &Context<Self>) -> Vec<menu::Row> {
        let entries = self.menu_entries(cx);
        if entries.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![
            menu::Row::Branch {
                submenu: menu::ViewSubmenu::Layout,
                label: self.translator.text(i18n::Message::MenuLayout).into_owned(),
            },
            menu::Row::Branch {
                submenu: menu::ViewSubmenu::Columns,
                label: self
                    .translator
                    .text(i18n::Message::MenuColumns)
                    .into_owned(),
            },
            menu::Row::Branch {
                submenu: menu::ViewSubmenu::Theme,
                label: self.translator.text(i18n::Message::MenuTheme).into_owned(),
            },
            menu::Row::Branch {
                submenu: menu::ViewSubmenu::Language,
                label: self
                    .translator
                    .text(i18n::Message::MenuLanguage)
                    .into_owned(),
            },
        ];
        if let Some(file_info) = entries
            .iter()
            .find(|entry| entry.action.as_str() == input::TOGGLE_FILE_INFO_PANE)
            .cloned()
        {
            rows.push(menu::Row::Leaf(file_info));
        }
        if let Some(show_hidden) = entries
            .iter()
            .find(|entry| entry.action.as_str() == input::TOGGLE_SHOW_HIDDEN_ITEMS)
            .cloned()
        {
            rows.push(menu::Row::Leaf(show_hidden));
        }
        if let Some(item_check_boxes) = entries
            .iter()
            .find(|entry| entry.action.as_str() == input::TOGGLE_ITEM_CHECK_BOXES)
            .cloned()
        {
            rows.push(menu::Row::Leaf(item_check_boxes));
        }
        rows
    }

    pub(super) fn menu_rows(&mut self, cx: &Context<Self>) -> Vec<menu::Row> {
        let Some(open) = self.menu.open() else {
            return Vec::new();
        };
        let entries = self.menu_entries(cx);
        if open != menu::Open::Menu(menu::Menu::View) {
            if matches!(
                open,
                menu::Open::Popup(menu::Popup::Item | menu::Popup::Background)
            ) {
                if let Some(submenu) = self.menu.view_submenu() {
                    return match submenu {
                        menu::ViewSubmenu::Compression => entries
                            .into_iter()
                            .filter(|row| {
                                matches!(row.action.as_str(), menu::CREATE_ZIP | menu::CREATE_7Z)
                            })
                            .map(menu::Row::Leaf)
                            .collect(),
                        menu::ViewSubmenu::Creation => entries
                            .into_iter()
                            .filter(|row| {
                                matches!(
                                    row.action.as_str(),
                                    input::CREATE_FILE
                                        | input::CREATE_FOLDER
                                        | input::PASTE_LINK
                                        | input::PASTE_JUNCTION
                                )
                                    && (open != menu::Open::Popup(menu::Popup::Item)
                                        || row.action.as_str() != input::PASTE_LINK)
                            })
                            .map(menu::Row::Leaf)
                            .collect(),
                        _ => entries.into_iter().map(menu::Row::Leaf).collect(),
                    };
                }
                return self.context_menu_root_rows(entries);
            }
            let mut rows = entries.into_iter().map(menu::Row::Leaf).collect::<Vec<_>>();
            if open == menu::Open::Menu(menu::Menu::File) {
                let snapshot = cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                    |owner, _| owner.snapshot(),
                );
                menu::append_seven_zip_info_row(&mut rows, &snapshot, &self.translator);
            }
            return rows;
        }
        if entries.is_empty() {
            self.menu.reset_view_submenu();
            return Vec::new();
        }
        let entry = |action: &str| {
            entries
                .iter()
                .find(|entry| entry.action.as_str() == action)
                .cloned()
        };
        match self.menu.view_submenu() {
            None => self.view_menu_root_rows(cx),
            Some(menu::ViewSubmenu::Layout) => {
                [input::LAYOUT_ROWS, input::LAYOUT_TILES, input::LAYOUT_ICONS]
                    .into_iter()
                    .filter_map(entry)
                    .map(menu::Row::Leaf)
                    .collect()
            }
            Some(menu::ViewSubmenu::Columns) => entries
                .into_iter()
                .filter(|row| {
                    matches!(
                        row.action.as_str(),
                        input::TOGGLE_COLUMN_KIND
                            | input::TOGGLE_COLUMN_SIZE
                            | input::TOGGLE_COLUMN_MODIFIED
                            | input::MOVE_NAME_LEFT
                            | input::MOVE_NAME_RIGHT
                            | input::MOVE_KIND_LEFT
                            | input::MOVE_KIND_RIGHT
                            | input::MOVE_SIZE_LEFT
                            | input::MOVE_SIZE_RIGHT
                            | input::MOVE_MODIFIED_LEFT
                            | input::MOVE_MODIFIED_RIGHT
                    )
                })
                .map(menu::Row::Leaf)
                .collect(),
            Some(menu::ViewSubmenu::Theme) => self
                .theme_names
                .iter()
                .map(|basename| menu::Row::Theme {
                    basename: basename.clone(),
                    label: basename.clone(),
                    checked: basename == &self.selected_theme,
                })
                .collect(),
            Some(menu::ViewSubmenu::Language) => {
                let rows = vec![
                    (
                        menu::LANGUAGE_SYSTEM,
                        i18n::Message::LanguageSystem,
                        i18n::LanguageOverride::System,
                    ),
                    (
                        menu::LANGUAGE_JAPANESE,
                        i18n::Message::LanguageJapanese,
                        i18n::LanguageOverride::Japanese,
                    ),
                    (
                        menu::LANGUAGE_ENGLISH,
                        i18n::Message::LanguageEnglish,
                        i18n::LanguageOverride::English,
                    ),
                ]
                .into_iter()
                .map(|(action, message, setting)| {
                    menu::Row::Leaf(menu::Entry {
                        action: ActionId::new(action),
                        label: self.translator.text(message).into_owned(),
                        enabled: true,
                        checked: self.language_override == setting,
                    })
                })
                .collect::<Vec<_>>();
                #[cfg(debug_assertions)]
                let rows = {
                    let mut rows = rows;
                    rows.push(menu::Row::Leaf(menu::Entry {
                        action: ActionId::new(menu::LANGUAGE_PSEUDO),
                        label: self
                            .translator
                            .text(i18n::Message::LanguagePseudo)
                            .into_owned(),
                        enabled: true,
                        checked: self.language_override == i18n::LanguageOverride::Pseudo,
                    }));
                    rows
                };
                rows
            }
            Some(menu::ViewSubmenu::Compression | menu::ViewSubmenu::Creation) => {
                self.menu.reset_view_submenu();
                Vec::new()
            }
        }
    }

    pub(super) fn context_menu_root_rows(&self, entries: Vec<menu::Entry>) -> Vec<menu::Row> {
        let item_popup = self.menu.open() == Some(menu::Open::Popup(menu::Popup::Item));
        let has = |actions: &[&str]| {
            entries
                .iter()
                .any(|row| actions.contains(&row.action.as_str()))
        };
        let mut rows = Vec::new();
        if has(&[menu::CREATE_ZIP, menu::CREATE_7Z]) {
            rows.push(menu::Row::Branch {
                submenu: menu::ViewSubmenu::Compression,
                label: self
                    .translator
                    .text(i18n::Message::MenuCompression)
                    .into_owned(),
            });
        }
        if has(&[input::CREATE_FILE, input::CREATE_FOLDER, input::PASTE_JUNCTION])
            || (!item_popup && has(&[input::PASTE_LINK]))
        {
            rows.push(menu::Row::Branch {
                submenu: menu::ViewSubmenu::Creation,
                label: self
                    .translator
                    .text(i18n::Message::MenuCreation)
                    .into_owned(),
            });
        }
        rows.extend(
            entries
                .into_iter()
                .filter(|row| {
                    !matches!(
                        row.action.as_str(),
                        menu::CREATE_ZIP
                            | menu::CREATE_7Z
                            | input::CREATE_FILE
                            | input::CREATE_FOLDER
                            | input::PASTE_JUNCTION
                    )
                        && (item_popup || row.action.as_str() != input::PASTE_LINK)
                })
                .map(menu::Row::Leaf),
        );
        rows
    }

    /// 選んでいる行を動かす。端では止める。
    ///
    /// dropdown がまだ開いていないときの上下は、バーへの入口にする。バーをキーで開く
    /// 割り当てはまだ無いので、指で出したあとキーだけでも中へ入れるようにしている。
    pub(super) fn move_menu_highlight(&mut self, offset: isize, cx: &mut Context<Self>) {
        if !self.menu.is_open() {
            if self.menu.bar_visible() {
                self.show_menu(menu::Menu::ALL[0], cx);
            }
            return;
        }
        let rows = self.menu_rows(cx);
        self.menu.move_highlight(offset, rows.len());
        if self.menu.open() == Some(menu::Open::Popup(menu::Popup::ReopenTabs)) {
            let top = self.menu.highlight_index() as f32 * super::menu_surface::REOPEN_ROW_HEIGHT;
            self.reopen_scroll
                .set_offset(gpui::point(gpui::px(0.0), -gpui::px(top)));
        } else if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View)) {
            let root_open = self.menu.view_submenu().is_none();
            let scroll = if root_open {
                &self.view_menu_root_scroll
            } else {
                &self.view_menu_scroll
            };
            let visible_height = scroll.bounds().size.height.as_f32();
            let content_height = rows.len() as f32 * super::menu_surface::VIEW_ROW_HEIGHT;
            let row_height = super::menu_surface::VIEW_ROW_HEIGHT;
            let top = super::menu_surface::reveal_scroll_offset(
                -scroll.offset().y.as_f32(),
                self.menu.highlight_index() as f32 * row_height,
                row_height,
                visible_height,
                content_height,
            );
            scroll.set_offset(gpui::point(gpui::px(0.0), -gpui::px(top)));
        } else if matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background
            ))
        ) {
            let scroll = if self.menu.view_submenu().is_some() {
                &self.view_menu_scroll
            } else {
                &self.context_popup_scroll
            };
            let row_height = if self
                .menu_target
                .as_ref()
                .is_some_and(|target| target.touch_target)
            {
                super::menu_surface::TOUCH_CONTEXT_ROW_HEIGHT
            } else {
                super::menu_surface::ROW_HEIGHT
            };
            let top = super::menu_surface::reveal_scroll_offset(
                -scroll.offset().y.as_f32(),
                self.menu.highlight_index() as f32 * row_height,
                row_height,
                scroll.bounds().size.height.as_f32(),
                rows.len() as f32 * row_height,
            );
            scroll.set_offset(gpui::point(gpui::px(0.0), -gpui::px(top)));
        }
        cx.notify();
    }

    pub(super) fn open_view_submenu_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let root_rows = if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View)) {
            self.view_menu_root_rows(cx)
        } else if matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background
            ))
        ) {
            self.context_menu_root_rows(self.menu_entries(cx))
        } else {
            return;
        };
        if let Some(menu::Row::Branch { submenu, .. }) = root_rows.get(index) {
            self.enter_view_submenu(*submenu, index, cx);
        }
    }

    fn open_view_submenu_from_keyboard(&mut self, index: usize, cx: &mut Context<Self>) {
        let root_rows = if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View)) {
            self.view_menu_root_rows(cx)
        } else if matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background
            ))
        ) {
            self.context_menu_root_rows(self.menu_entries(cx))
        } else {
            return;
        };
        if let Some(menu::Row::Branch { submenu, .. }) = root_rows.get(index) {
            self.enter_view_submenu_from_keyboard(*submenu, index, cx);
        }
    }

    fn enter_view_submenu(
        &mut self,
        submenu: menu::ViewSubmenu,
        parent_index: usize,
        cx: &mut Context<Self>,
    ) {
        let changed = self.menu.view_submenu() != Some(submenu);
        self.menu.open_submenu(submenu, parent_index);
        if changed {
            if matches!(
                self.menu.open(),
                Some(menu::Open::Popup(
                    menu::Popup::Item | menu::Popup::Background
                ))
            ) {
                self.context_popup_scroll
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
            } else {
                self.view_menu_scroll
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
            }
        }
        cx.notify();
    }

    fn enter_view_submenu_from_keyboard(
        &mut self,
        submenu: menu::ViewSubmenu,
        parent_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.enter_view_submenu(submenu, parent_index, cx);
        self.menu.activate_first_row();
        cx.notify();
    }

    pub(super) fn hover_menu_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let rows = self.menu_rows(cx);
        self.menu.hover(index);
        if (self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
            || matches!(
                self.menu.open(),
                Some(menu::Open::Popup(
                    menu::Popup::Item | menu::Popup::Background
                ))
            ))
            && self.menu.view_submenu().is_none()
            && matches!(rows.get(index), Some(menu::Row::Branch { .. }))
        {
            self.open_view_submenu_at(index, cx);
        } else if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
            && self.menu.view_submenu().is_some()
            && matches!(rows.get(index), Some(menu::Row::Leaf(_)))
        {
            // Child rows are handled by their own panel; root leaf hover closes the child.
        }
        cx.notify();
    }

    pub(super) fn clear_menu_hover(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.menu.clear_hover(index) {
            cx.notify();
        }
    }

    pub(super) fn hover_view_root_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let roots = if self.menu.open() == Some(menu::Open::Menu(menu::Menu::View)) {
            self.view_menu_root_rows(cx)
        } else if matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background
            ))
        ) {
            self.context_menu_root_rows(self.menu_entries(cx))
        } else {
            return;
        };
        match roots.get(index) {
            Some(menu::Row::Branch { submenu, .. }) => self.enter_view_submenu(*submenu, index, cx),
            Some(menu::Row::Leaf(_)) => {
                self.menu.close_submenu();
                self.menu.hover(index);
            }
            _ => return,
        }
        cx.notify();
    }

    /// 左右で menu を移る。端は折り返す。dropdown が無ければバーへ入る。
    pub(super) fn step_menu(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(current) = self.menu.open_menu() else {
            // popup のときは動かさない。バーが出ているときだけ入口にする。
            if self.menu.bar_visible() {
                self.show_menu(menu::Menu::ALL[0], cx);
            }
            return;
        };
        let menus = menu::Menu::ALL;
        let index = menus.iter().position(|menu| *menu == current).unwrap_or(0);
        let next = (index as isize + offset).rem_euclid(menus.len() as isize) as usize;
        self.show_menu(menus[next], cx);
    }

    /// 選んでいる行を実行する。使えない行では何もしない。
    pub(super) fn run_highlighted_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.menu_rows(cx);
        let Some(row) = self.menu.highlighted(&rows).cloned() else {
            return;
        };
        match row {
            menu::Row::Branch { submenu, .. } => {
                self.enter_view_submenu_from_keyboard(submenu, self.menu.highlight_index(), cx);
            }
            menu::Row::Theme { basename, .. } => {
                if self.theme_names.iter().any(|name| name == &basename) {
                    self.leave_menu(cx);
                    self.select_theme(basename, cx);
                }
            }
            menu::Row::Leaf(entry) if entry.enabled => {
                self.run_menu_command_at(
                    entry.action,
                    Some(self.menu.highlight_index()),
                    window,
                    cx,
                );
            }
            menu::Row::Leaf(_) => {}
            menu::Row::Info { .. } => {}
        }
    }

    /// menu モードを終わる。modal も外す。
    pub(super) fn leave_menu(&mut self, cx: &mut Context<Self>) {
        if !self.menu.bar_visible() && !self.menu.is_open() {
            return;
        }
        self.menu.close();
        self.menu_target = None;
        self.tab_menu_target = None;
        self.reopen_menu_targets = None;
        self.workspace_menu_target = None;
        self.menu_at = None;
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// ポインタの位置に menu を出す。項目の上か余白かで中身が変わる。
    pub(super) fn open_context_menu(
        &mut self,
        pane: super::PaneId,
        tab: usize,
        view: gpui::EntityId,
        request: menu::ItemMenuRequest,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some((owner_pane, owner_tab)) = self.workspace.active().pane_and_tab(tab, view) else {
            return;
        };
        if owner_pane.id != pane || owner_pane.directory().active_tab_id != tab {
            return;
        }
        let live_view = owner_tab.view.read(cx);
        if live_view.generation() != request.generation
            || live_view.presentation_revision != request.presentation_revision
            || !context_menu_view_available(live_view.batch_rename_dialog_active())
        {
            return;
        }
        let seven_zip_source_identities = request
            .context_keys
            .iter()
            .filter_map(|key| {
                live_view
                    .entries
                    .iter()
                    .find(|entry| &entry.key == key)
                    .and_then(|entry| entry.source_identity.as_ref())
                    .map(|identity| identity.filesystem_identity())
            })
            .collect();
        let captured_destination = if request.clicked.is_some() {
            live_view
                .session
                .location()
                .and_then(crate::Location::path)
                .map(std::path::Path::to_path_buf)
        } else {
            request.background_destination.clone()
        };
        let captured_destination_identity = if request.clicked.is_some() {
            live_view.session.directory_identity().cloned()
        } else {
            request.background_destination_identity.clone()
        };

        let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
        if !policy.allowed {
            return;
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        self.menu.open_popup(match request.clicked.is_some() {
            true => menu::Popup::Item,
            false => menu::Popup::Background,
        });
        self.context_popup_scroll
            .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
        self.workspace_menu_target = None;
        self.menu_target = Some(menu::MenuTarget {
            workspace: self.workspace.active,
            pane,
            tab,
            view: Some(view),
            generation: request.generation,
            presentation_revision: request.presentation_revision,
            item: request.clicked,
            selected_keys: request.selected_keys,
            context_keys: request.context_keys,
            selection_derived: request.selection_derived,
            rename_eligible: request.rename_eligible,
            paste_target: request.paste_target,
            clipboard_has_files: file_clipboard::has_files().unwrap_or(false),
            background_destination: captured_destination,
            background_destination_identity: captured_destination_identity,
            touch_target: request.touch_target,
            seven_zip_creation: cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                |owner, _| menu::SevenZipCreationHint::capture(&owner.snapshot()),
            ),
            seven_zip_read: cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                |owner, _| menu::SevenZipReadHint::capture(&owner.snapshot()),
            ),
            seven_zip_source_identities,
        });
        self.tab_menu_target = None;
        self.menu_at = Some(at);
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// 開いている item popup の背面で右クリックされたら、現在描画中の active view だけを
    /// 各 owner の実測 hit bounds で調べ、popup state と anchor を一度に置き換える。
    pub(super) fn replace_context_menu_from_backdrop(
        &mut self,
        at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu.open() == Some(menu::Open::Popup(menu::Popup::Workspace)) {
            let hit = self
                .workspace_tab_context_bounds
                .iter()
                .find_map(|(id, bounds)| {
                    self.workspace
                        .ordered
                        .iter()
                        .any(|(live, _)| live == id)
                        .then(|| super::workspace_tab_bounds_hit(false, *bounds.borrow(), at))
                        .flatten()
                        .map(|_| *id)
                });
            if let Some(id) = hit {
                self.open_workspace_context_menu(id, at, window, cx);
            } else {
                self.leave_menu(cx);
            }
            return;
        }
        let replacement = (self.menu.open() == Some(menu::Open::Popup(menu::Popup::Item)))
            .then(|| {
                self.painted_active_panes.iter().find_map(|painted| {
                    let pane = self.workspace.active().directory_pane(painted.pane)?;
                    if pane.active_tab_id != painted.tab {
                        return None;
                    }
                    let tab = pane.tabs.iter().find(|tab| tab.id == painted.tab)?;
                    if tab.view.entity_id() != painted.view {
                        return None;
                    }
                    let view = tab.view.clone();
                    view.update(cx, |view, _| view.item_menu_request_at(at))
                        .map(|request| (painted.pane, painted.tab, painted.view, request))
                })
            })
            .flatten();
        if let Some((pane_id, tab_id, view_id, request)) = replacement {
            self.open_context_menu(pane_id, tab_id, view_id, request, at, cx);
        } else {
            self.leave_menu(cx);
        }
    }

    /// ファイルタブの安定 ID を対象に、共有 popup 面を開く。
    pub(super) fn open_tab_context_menu(
        &mut self,
        pane_id: super::PaneId,
        id: usize,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
        let Some(pane) = self.workspace.active().directory_pane(pane_id) else {
            return;
        };
        if !policy.allowed || !pane.tabs.iter().any(|tab| tab.id == id) {
            return;
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        self.menu.open_popup(menu::Popup::Tab);
        self.menu_target = None;
        self.workspace_menu_target = None;
        self.tab_menu_target = Some(menu::TabMenuTarget {
            workspace: self.workspace.active,
            pane: pane_id,
            id,
            snapshot: self
                .workspace
                .active()
                .directory_pane(pane_id)
                .unwrap()
                .tabs
                .iter()
                .map(|tab| (tab.id, tab.locked))
                .collect(),
        });
        self.menu_at = Some(at);
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// Open the shared popup for one exact live workspace without activating it.
    pub(super) fn open_workspace_context_menu(
        &mut self,
        id: super::WorkspaceId,
        at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
        if !policy.allowed || !self.workspace.ordered.iter().any(|(live, _)| *live == id) {
            return;
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        self.workspace_restore_menu_open = false;
        self.menu.open_popup(menu::Popup::Workspace);
        self.menu_target = None;
        self.tab_menu_target = None;
        self.reopen_menu_targets = None;
        self.workspace_menu_target = Some(id);
        self.menu_at = Some(at);
        if let Some(focus) = self.workspace_duplicate_focus.get(&id) {
            focus.focus(window, cx);
        }
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    pub(super) fn open_reopen_menu(
        &mut self,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let policy = overlays::open_policy(overlays::OverlayKind::Menu, self.open_overlays(cx));
        if !policy.allowed {
            return;
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        self.menu.open_popup(menu::Popup::ReopenTabs);
        self.workspace_menu_target = None;
        self.reopen_scroll
            .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
        self.menu_target = None;
        self.tab_menu_target = None;
        self.reopen_menu_targets = Some(self.closed_tabs.iter().map(|record| record.id).collect());
        self.menu_at = Some(at);
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// 開いた時の対象を捕捉する。実行の直前に照合する。
    pub(super) fn capture_menu_target(
        &self,
        item: Option<entry_identity::EntryKey>,
        cx: &Context<Self>,
    ) -> menu::MenuTarget {
        let generation = self
            .workspace
            .active()
            .focused_pane()
            .tabs
            .iter()
            .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
            .map_or(0, |tab| tab.view.read(cx).generation());
        let selection = self
            .workspace
            .active()
            .focused_pane()
            .tabs
            .iter()
            .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
            .map_or_else(Default::default, |tab| {
                tab.view.read(cx).selected_entry_keys()
            });
        menu::MenuTarget {
            workspace: self.workspace.active,
            pane: self.workspace.active().focused_pane_id,
            tab: self.workspace.active().focused_pane().active_tab_id,
            view: self
                .workspace
                .active()
                .focused_pane()
                .tabs
                .iter()
                .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
                .map(|tab| tab.view.entity_id()),
            generation,
            presentation_revision: self
                .workspace
                .active()
                .focused_pane()
                .tabs
                .iter()
                .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
                .map_or(0, |tab| tab.view.read(cx).presentation_revision),
            item: item.clone(),
            selected_keys: selection.iter().cloned().collect(),
            context_keys: item.into_iter().collect(),
            selection_derived: false,
            rename_eligible: false,
            paste_target: None,
            clipboard_has_files: false,
            background_destination: self
                .workspace
                .active()
                .focused_pane()
                .tabs
                .iter()
                .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
                .and_then(|tab| {
                    tab.view
                        .read(cx)
                        .session
                        .location()
                        .and_then(crate::Location::path)
                })
                .map(std::path::Path::to_path_buf),
            background_destination_identity: self
                .workspace
                .active()
                .focused_pane()
                .tabs
                .iter()
                .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
                .and_then(|tab| tab.view.read(cx).session.directory_identity().cloned()),
            touch_target: false,
            seven_zip_creation: cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                |owner, _| menu::SevenZipCreationHint::capture(&owner.snapshot()),
            ),
            seven_zip_read: cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(
                |owner, _| menu::SevenZipReadHint::capture(&owner.snapshot()),
            ),
            seven_zip_source_identities: Vec::new(),
        }
    }

    /// menu の行を選んだ。開いた時の対象と変わっていれば実行しない。
    pub(super) fn run_menu_command_at(
        &mut self,
        action: ActionId,
        row_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let popup = self.menu.open();
        if action.as_str() == menu::NEW_WINDOW
            && self.menu.open() == Some(menu::Open::Menu(menu::Menu::File))
        {
            self.leave_menu(cx);
            self.open_new_window(window, cx);
            return;
        }
        if self.menu.open() == Some(menu::Open::Popup(menu::Popup::Workspace))
            && action.as_str() == menu::WORKSPACE_DUPLICATE
        {
            let target = self.workspace_menu_target.take();
            self.leave_menu(cx);
            if let Some(target) = target
                && self.workspace.ordered.iter().any(|(id, _)| *id == target)
            {
                self.duplicate_workspace(target, window, cx);
            }
            return;
        }
        if self
            .menu_target
            .as_ref()
            .is_some_and(|target| target.workspace != self.workspace.active)
            || self
                .tab_menu_target
                .as_ref()
                .is_some_and(|target| target.workspace != self.workspace.active)
        {
            return;
        }
        let language = match action.as_str() {
            menu::LANGUAGE_SYSTEM => Some(i18n::LanguageOverride::System),
            menu::LANGUAGE_JAPANESE => Some(i18n::LanguageOverride::Japanese),
            menu::LANGUAGE_ENGLISH => Some(i18n::LanguageOverride::English),
            #[cfg(debug_assertions)]
            menu::LANGUAGE_PSEUDO => Some(i18n::LanguageOverride::Pseudo),
            _ => None,
        };
        if let Some(language) = language
            && self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
        {
            self.leave_menu(cx);
            self.select_language(language, cx);
            return;
        }
        if action.as_str() == input::TOGGLE_FILE_INFO_PANE {
            self.leave_menu(cx);
            self.toggle_file_info_pane(cx);
            return;
        }
        if action.as_str() == input::TOGGLE_SHOW_HIDDEN_ITEMS
            && self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
        {
            self.leave_menu(cx);
            cx.update_global::<crate::show_hidden_items::ShowHiddenItems, _>(|setting, _| {
                setting.0 = !setting.0;
            });
            self.save_requested = true;
            cx.notify();
            return;
        }
        if action.as_str() == input::TOGGLE_ITEM_CHECK_BOXES
            && self.menu.open() == Some(menu::Open::Menu(menu::Menu::View))
        {
            self.leave_menu(cx);
            cx.update_global::<crate::ItemCheckBoxes, _>(|setting, _| {
                setting.0 = !setting.0;
            });
            self.save_requested = true;
            cx.notify();
            return;
        }
        let reopen_record = (self.menu.open() == Some(menu::Open::Popup(menu::Popup::ReopenTabs)))
            .then(|| {
                self.reopen_menu_targets.as_ref().and_then(|targets| {
                    targets
                        .get(row_index.unwrap_or_else(|| self.menu.highlight_index()))
                        .copied()
                })
            })
            .flatten();
        let target = self.menu_target.clone();
        let tab_target = self.tab_menu_target.clone();
        // 実行するにしても、古い menu は閉じる。
        self.leave_menu(cx);
        if action.as_str() == menu::REOPEN_TAB {
            if let Some(record_id) = reopen_record {
                self.reopen_closed_tab(record_id, cx);
            }
            return;
        }
        if let Some(target) = tab_target {
            self.cancel_pending_native_drag(cx);
            self.quick_preview.close();
            self.run_tab_menu_command(action, target, cx);
            return;
        }
        if action.as_str() == input::SELECT_EXTENSION_ON_RENAME {
            self.select_extension_on_rename = !self.select_extension_on_rename;
            let select_extension = self.select_extension_on_rename;
            for tab in self.workspace.tabs() {
                tab.view.update(cx, |view, _| {
                    view.set_select_extension_on_rename(select_extension)
                });
            }
            self.save_requested = true;
            cx.notify();
            return;
        }
        if action.as_str() == input::RELOAD_KEYMAP {
            self.reload_keymap(cx);
            return;
        }
        let Some(target) = target else {
            return;
        };
        if action.as_str() == menu::OPEN_IN_NEW_PANE {
            self.open_folder_in_new_pane(target, cx);
            return;
        }
        // 選択が変わった後に表示中の menu から実行されたとき、別のタブへ黙って配送しない。
        let Some(view_id) = target.view else {
            return;
        };
        let Some((pane, tab)) = self.workspace.active().pane_and_tab(target.tab, view_id) else {
            return;
        };
        if pane.id != target.pane || pane.directory().active_tab_id != target.tab {
            return;
        }
        let view = tab.view.clone();
        if action.as_str() == input::COPY_CURRENT_PATH {
            if popup != Some(menu::Open::Popup(menu::Popup::Background)) {
                return;
            }
            let Some(text) = view.update(cx, |view, _| {
                view.current_path_for_background_menu_target(&target)
            }) else {
                return;
            };
            view.update(cx, |_, cx| {
                cx.emit(crate::DirectoryEvent::CopyPlainText { text });
            });
            return;
        }
        if action == ActionId::new(menu::CREATE_ZIP) {
            let Some(request) = view.update(cx, |view, _| {
                view.zip_create_request_for_menu_target(&target)
            }) else {
                return;
            };
            match cx
                .global_mut::<operations::application::OperationApplicationOwner>()
                .runtime
                .submit(request)
            {
                Ok(_) => self.start_next_operation(cx),
                Err(error) => {
                    self.session_notice = Some(SessionNotice::Message(
                        Translator::operation_submit_message(error),
                    ));
                    cx.notify();
                }
            }
            return;
        }
        if action == ActionId::new(menu::CREATE_SHORTCUT) {
            if popup != Some(menu::Open::Popup(menu::Popup::Item)) {
                return;
            }
            let Some(request) = view.update(cx, |view, _| {
                view.shortcut_create_request_for_menu_target(&target)
            }) else {
                return;
            };
            match cx
                .global_mut::<operations::application::OperationApplicationOwner>()
                .runtime
                .submit(request)
            {
                Ok(_) => self.start_next_operation(cx),
                Err(error) => {
                    self.session_notice = Some(SessionNotice::Message(
                        Translator::operation_submit_message(error),
                    ));
                    cx.notify();
                }
            }
            return;
        }
        if action == ActionId::new(menu::CREATE_7Z) {
            let snapshot =
                cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(|owner, _| {
                    owner.snapshot()
                });
            let Some(request) = view.update(cx, |view, _| {
                view.seven_zip_create_request_for_menu_target(&target, &snapshot)
            }) else {
                return;
            };
            match cx
                .global_mut::<operations::application::OperationApplicationOwner>()
                .runtime
                .submit(request)
            {
                Ok(_) => self.start_next_operation(cx),
                Err(error) => {
                    self.session_notice = Some(SessionNotice::Message(
                        Translator::operation_submit_message(error),
                    ));
                    cx.notify();
                }
            }
            return;
        }
        if action == ActionId::new(menu::EXTRACT_TO_FOLDER) {
            let provider =
                cx.read_global::<crate::archive_provider::ArchiveProviderOwner, _>(|owner, _| {
                    owner.snapshot()
                });
            let request = view.update(cx, |view, _| {
                view.zip_extract_request_for_menu_target(&target)
                    .or_else(|| view.seven_zip_extract_request_for_menu_target(&target, &provider))
            });
            let Some(request) = request else {
                return;
            };
            match cx
                .global_mut::<operations::application::OperationApplicationOwner>()
                .runtime
                .submit(request)
            {
                Ok(_) => self.start_next_operation(cx),
                Err(error) => {
                    self.session_notice = Some(SessionNotice::Message(
                        Translator::operation_submit_message(error),
                    ));
                    cx.notify();
                }
            }
            return;
        }
        if action == ActionId::new(menu::OPEN_IN_TERMINAL) {
            let Some(request) =
                view.update(cx, |view, _| view.terminal_request_for_menu_target(&target))
            else {
                return;
            };
            let ticket = super::next_terminal_request_ticket(&mut self.terminal_request_ticket);
            let shell = cx.entity().downgrade();
            let captured_target = target.clone();
            #[cfg(test)]
            let launch_observer = self.terminal_launch_observer.clone();
            cx.spawn(async move |_, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        #[cfg(test)]
                        if let Some(observer) = launch_observer {
                            observer
                                .lock()
                                .expect("terminal launch observer")
                                .push(request);
                            return Ok(());
                        }
                        crate::platform_terminal::launch(&request)
                    })
                    .await;
                let _ = shell.update(cx, |shell, cx| {
                    if !super::terminal_request_is_current(shell.terminal_request_ticket, ticket) {
                        return;
                    }
                    let Some(view_id) = captured_target.view else {
                        return;
                    };
                    if shell.workspace.active != captured_target.workspace {
                        return;
                    }
                    let Some((pane, tab)) = shell
                        .workspace
                        .active()
                        .pane_and_tab(captured_target.tab, view_id)
                    else {
                        return;
                    };
                    if pane.id != captured_target.pane
                        || pane.directory().active_tab_id != captured_target.tab
                        || tab
                            .view
                            .read(cx)
                            .terminal_request_for_menu_target(&captured_target)
                            .is_none()
                    {
                        return;
                    }
                    if result.is_err() {
                        super::append_operation_notice(
                            &mut shell.operation_notice,
                            i18n::Message::WindowsTerminalLaunchFailed,
                        );
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        if action.as_str() == input::PROPERTIES {
            let Some(paths) =
                view.update(cx, |view, _| view.properties_paths_for_menu_target(&target))
            else {
                return;
            };
            open_properties_paths_with(&paths, properties_dialog::open);
            return;
        }
        // `Context<FilerShell>` は `VisualContext` ではないので、`update_in` ではなく
        // クリックハンドラの window をそのまま closure へ渡す。
        view.update(cx, |view, cx| {
            view.run_menu_command(action, &target, window, cx);
        });
    }

    fn select_language(&mut self, language: i18n::LanguageOverride, cx: &mut Context<Self>) {
        if self.language_override == language {
            return;
        }
        self.language_override = language;
        self.translator = language.translator(i18n::system_locale());
        let translator = self.translator.clone();
        for tab in self.workspace.tabs() {
            let translator = translator.clone();
            let _ = tab
                .view
                .update(cx, |view, cx| view.set_translator(translator, cx));
        }
        cx.notify();
        self.save_requested = true;
    }

    fn run_tab_menu_command(
        &mut self,
        action: ActionId,
        target: menu::TabMenuTarget,
        cx: &mut Context<Self>,
    ) {
        if target.workspace != self.workspace.active {
            return;
        }
        let Some(pane) = self.workspace.active().directory_pane(target.pane) else {
            return;
        };
        let Some(target_index) = pane.tabs.iter().position(|tab| tab.id == target.id) else {
            return;
        };
        let current = pane
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.locked))
            .collect::<Vec<_>>();
        let pane_len = pane.tabs.len();
        let active_id = pane.active_tab_id;
        let target_locked = pane.tabs[target_index].locked;
        match action.as_str() {
            menu::TAB_DUPLICATE => {
                if current != target.snapshot {
                    return;
                }
                let Some(location) = pane.tabs[target_index].view.read(cx).location() else {
                    return;
                };
                self.add_tab_in(target.pane, Some(location), cx);
            }
            menu::TAB_TOGGLE_LOCK => {
                let Some(pane) = self.workspace.active_mut().directory_pane_mut(target.pane) else {
                    return;
                };
                pane.tabs[target_index].locked = !pane.tabs[target_index].locked;
                self.place_locked_tabs_first_in(target.pane);
            }
            menu::TAB_CLOSE => {
                if pane_len <= 1 || target_locked {
                    return;
                }
                let ids = current.iter().map(|(id, _)| *id).collect::<Vec<_>>();
                let fallback = navigation::active_after_close(&ids, active_id, target.id);
                self.close_tabs_by_ids_in(target.pane, [target.id], fallback, cx);
            }
            menu::TAB_CLOSE_OTHERS | menu::TAB_CLOSE_TO_RIGHT => {
                if current != target.snapshot {
                    return;
                }
                let Some(index) = target.snapshot.iter().position(|(id, _)| *id == target.id)
                else {
                    return;
                };
                let ids = target
                    .snapshot
                    .iter()
                    .enumerate()
                    .filter_map(|(position, (id, locked))| {
                        let selected = if action.as_str() == menu::TAB_CLOSE_OTHERS {
                            *id != target.id
                        } else {
                            position > index
                        };
                        (selected && !locked).then_some(*id)
                    })
                    .collect::<Vec<_>>();
                if !ids.is_empty() {
                    self.close_tabs_by_ids_in(target.pane, ids, target.id, cx);
                }
                return;
            }
            _ => return,
        }
        if self.workspace.active().focused_pane_id == target.pane {
            self.pending_focus = true;
        }
        self.save_requested = true;
        self.sync_overlay_modal(cx);
        cx.notify();
    }
}

fn open_properties_paths_with<E>(
    paths: &[std::path::PathBuf],
    mut open: impl FnMut(&std::path::Path) -> Result<(), E>,
) where
    E: std::fmt::Display,
{
    for path in paths {
        if let Err(error) = open(path) {
            eprintln!("[v6][properties-dialog] {error}");
        }
    }
}

fn captured_single_item_rename_target(
    open: menu::Open,
    item: Option<&entry_identity::EntryKey>,
    context_keys: &[entry_identity::EntryKey],
    rename_eligible: bool,
    live_generation: u64,
    target_generation: u64,
    live_presentation_revision: u64,
    target_presentation_revision: u64,
) -> bool {
    open == menu::Open::Popup(menu::Popup::Item)
        && context_keys.len() == 1
        && item == context_keys.first()
        && rename_eligible
        && target_generation == live_generation
        && target_presentation_revision == live_presentation_revision
}
