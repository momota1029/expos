use super::*;

impl Render for FilerShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pane_drop_hover.as_ref().is_some_and(|hover| {
            hover.workspace != self.workspace.active
                || hover.topology_revision != self.workspace.active().topology_revision
                || self.workspace.active().directory_pane(hover.source).is_none()
                || self.workspace.active().directory_pane(hover.target).is_none()
        }) {
            self.pane_drop_hover = None;
        }
        if self.active_pane_drag.is_some_and(|drag| {
            drag.workspace != self.workspace.active
                || drag.topology_revision != self.workspace.active().topology_revision
                || self.workspace.active().directory_pane(drag.source).is_none()
        }) {
            self.active_pane_drag = None;
            self.pane_drop_hover = None;
        }
        self.prune_consumed_internal_move_sessions();
        self.sync_directory_view_visibility(cx);
        self.poll_folder_preferences_writer(cx);
        self.observe_window(window, cx);
        // GPUI refreshes this Shell when the platform window activates; persist the
        // active identity now instead of waiting for an unrelated workspace edit.
        if window.is_window_active()
            && cx.has_global::<window_coordinator::WindowCoordinator>()
            && cx
                .global::<window_coordinator::WindowCoordinator>()
                .active_window()
                != Some(self.window_id)
        {
            cx.global_mut::<window_coordinator::WindowCoordinator>()
                .set_active_window(self.window_id);
            self.save_requested = true;
        }
        let clamped_sidebar_width =
            clamp_sidebar_width(self.sidebar_width, window.viewport_size().width.as_f32());
        if (self.sidebar_width - clamped_sidebar_width).abs() > f32::EPSILON {
            self.sidebar_width = clamped_sidebar_width;
            self.save_requested = true;
        }
        let workspace_width = (window.viewport_size().width.as_f32()
            - clamped_sidebar_width
            - SIDEBAR_SPLITTER_WIDTH)
            .max(1.0);
        self.project_pane_width_to_active_views(window.viewport_size().width.as_f32(), cx);
        if self.save_requested {
            self.save_requested = false;
            self.capture_and_request(window, cx);
        }
        let view = self
            .workspace
            .active()
            .focused_pane()
            .tabs
            .iter()
            .find(|tab| tab.id == self.workspace.active().focused_pane().active_tab_id)
            .expect("one active tab")
            .view
            .clone();
        if self.recovery_focus_restore {
            self.recovery_focus_restore = false;
            let fallback_focus = view.read(cx).focus_handle.clone();
            let shell = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                let _ = shell.update(cx, |this, cx| {
                    if this.recovery_records.is_empty() {
                        fallback_focus.focus(window, cx);
                    } else {
                        this.recovery_heading_focus.focus(window, cx);
                    }
                });
            });
        }
        let view_text_input_focused = view.read(cx).text_input_has_focus(window, cx);
        let shell_text_input_focused = self
            .search
            .as_ref()
            .is_some_and(|search| search.input.read(cx).is_focused(window))
            || self
                .workspace_rename
                .as_ref()
                .is_some_and(|rename| rename.input.read(cx).is_focused(window));
        let text_input_focused = view_text_input_focused || shell_text_input_focused;
        let shell_path = InteractionPath::new([input::scope::SHELL]).expect("shell scope");
        self.interaction.set_focused(
            Some(shell_path),
            if text_input_focused {
                FocusKind::TextInput
            } else {
                FocusKind::Regular
            },
        );
        let stale_job_progress_selection =
            self.job_progress_selection.is_some() && !self.job_progress_selection_is_current();
        if stale_job_progress_selection {
            self.clear_job_progress_selection();
        }
        if self.job_progress_focus_reconcile || stale_job_progress_selection {
            self.job_progress_focus_reconcile = false;
            if self.job_progress_rail_focus.is_focused(window)
                || self.job_progress_details_focus.is_focused(window)
                || self.job_progress_close_focus.is_focused(window)
            {
                self.pending_focus = true;
            }
        }
        if self.pending_focus {
            if self.job_progress_selection_is_current() {
                self.job_progress_rail_focus.focus(window, cx);
            } else {
                view.read(cx).focus_handle.clone().focus(window, cx);
            }
            self.pending_focus = false;
        }
        let pane_ids = self.workspace.active().pane_layout.pane_ids();
        let active_pane_views = pane_ids
            .iter()
            .filter_map(|pane_id| {
                let pane = self.workspace.active().directory_pane(*pane_id)?;
                pane.tabs
                    .iter()
                    .find(|tab| tab.id == pane.active_tab_id)
                    .map(|tab| (*pane_id, tab.id, tab.view.clone()))
            })
            .collect::<Vec<_>>();
        self.painted_active_panes = active_pane_views
            .iter()
            .map(|(pane, tab, view)| PaintedActivePane {
                pane: *pane,
                tab: *tab,
                view: view.entity_id(),
            })
            .collect();
        let pane_layout = self.workspace.active().pane_layout.clone();
        let show_pane_headers = pane_ids.len() > 1;
        let pane_content =
            self.render_pane_layout(&pane_layout, workspace_width, show_pane_headers, cx);
        let mut navigation_targets = std::collections::HashMap::new();
        for tab in &self.workspace.active().focused_pane().tabs {
            let target_view = tab.view.clone();
            let target_view_entity_id = target_view.entity_id();
            let snapshot = target_view
                .read(cx)
                .location()
                .and_then(|location| location.path().map(std::path::Path::to_path_buf))
                .map(|path| {
                    target_view.read(cx).capture_navigation_target_snapshot(
                        target_view_entity_id,
                        &path,
                        true,
                    )
                });
            let snapshot =
                snapshot.filter(|snapshot| snapshot.view_entity_id == target_view_entity_id);
            navigation_targets.insert(
                tab.id,
                (target_view.downgrade(), target_view_entity_id, snapshot),
            );
        }
        let navigation_targets = std::rc::Rc::new(navigation_targets);
        let tab_paths_cache = self.external_file_drop_paths.clone();
        let navigation_shell = cx.entity().downgrade();
        let workspace_id = self.workspace.active;
        let pane_id = self.workspace.active().focused_pane_id;
        let focused_pane_index = pane_ids
            .iter()
            .position(|candidate| *candidate == pane_id)
            .unwrap_or(0);
        let reveal_key = WorkspacePaneId {
            workspace: self.workspace.active,
            pane: pane_id,
        };
        if self.last_revealed_pane_id != Some(reveal_key) {
            self.pane_rail_scroll.scroll_to_item(focused_pane_index);
            self.last_revealed_pane_id = Some(reveal_key);
        }
        let leading_tab_groups = pane_ids[..focused_pane_index]
            .iter()
            .filter_map(|pane_id| self.render_basic_tab_group(*pane_id, window, cx))
            .collect::<Vec<_>>();
        let trailing_tab_groups = pane_ids[focused_pane_index + 1..]
            .iter()
            .filter_map(|pane_id| self.render_basic_tab_group(*pane_id, window, cx))
            .collect::<Vec<_>>();
        let mut labels: Vec<TabLabel> = self
            .workspace
            .active()
            .focused_pane()
            .tabs
            .iter()
            .map(|tab| {
                let location = tab.view.read(cx).location();
                TabLabel {
                    id: tab.id,
                    // PC は path を持たないので、タブ名は i18n の PC を使う。
                    title: match &location {
                        Some(Location::Path(path)) => navigation::label(path),
                        Some(Location::ThisPc) => {
                            self.translator.text(Message::ThisPc).into_owned()
                        }
                        Some(Location::ShellRoot(root)) => root.display_name().into(),
                        Some(Location::ShellItem { display_name, .. }) => display_name.clone(),
                        Some(location @ Location::Archive { .. }) => {
                            location.display_label().unwrap_or_default()
                        }
                        None => self.translator.text(Message::SampleList).into_owned(),
                    }
                    .into(),
                    location: match &location {
                        Some(Location::Path(path)) => path.to_string_lossy().into_owned(),
                        Some(Location::ThisPc) => {
                            self.translator.text(Message::ThisPc).into_owned()
                        }
                        Some(Location::ShellRoot(root)) => root.display_name().into(),
                        Some(Location::ShellItem { display_name, .. }) => display_name.clone(),
                        Some(location @ Location::Archive { .. }) => {
                            location.display_address().unwrap_or_default()
                        }
                        None => self.translator.item_count(20_000),
                    }
                    .into(),
                    disambiguation: None,
                    locked: tab.locked,
                }
            })
            .collect();
        let hints = pinned_tab_disambiguation_hints(&labels);
        for (label, hint) in labels.iter_mut().zip(hints) {
            label.disambiguation = hint.map(Into::into);
        }
        let active_tab_id = self.workspace.active().focused_pane().active_tab_id;
        let active_tab_group_height =
            self.pane_tab_group_height(&labels, active_tab_id, PANE_TAB_GROUP_HEIGHT, window)
                + if self.workspace.active().job_progress.is_some() {
                    px(44.0)
                } else {
                    px(0.0)
                };
        self.schedule_active_tab_reveal(
            pane_id,
            active_tab_id,
            active_tab_group_height,
            window,
            cx,
        );
        let text = VerticalTabsText {
            heading: self
                .pane_header_title(pane_id, cx)
                .unwrap_or_default()
                .into(),
            show_heading: self.workspace.active().pane_layout.pane_ids().len() > 1,
            new_tab: self.translator.text(Message::NewTab).into_owned().into(),
        };
        let notices = self.render_notices(cx);
        // タイトルの幅は一度だけ測り、バーと dropdown の位置で同じ値を使う。
        let menu_titles = self.menu_titles(window);
        let menu_bar = self.render_menu_bar(&menu_titles, cx);
        let active_workspace_id = self.workspace.active;
        if !self.menu.bar_visible() && self.last_revealed_workspace_id != Some(active_workspace_id)
        {
            self.last_revealed_workspace_id = Some(active_workspace_id);
            let shell = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = shell.update(cx, |this, cx| {
                    if this.workspace.active != active_workspace_id {
                        return;
                    }
                    if let Some(index) = this
                        .workspace
                        .ordered
                        .iter()
                        .position(|(workspace_id, _)| *workspace_id == active_workspace_id)
                    {
                        this.workspace_scroll.scroll_to_item(index);
                        cx.notify();
                    }
                });
            });
        }
        let titlebar = self.render_titlebar(window, menu_bar, cx);
        let conflict = self.active_conflict(cx);
        // conflict は filesystem mutation の判断を待たせている。背面の shell-wide
        // surface を残すと focus と router modal が二重になるので、projection だけ閉じる。
        if conflict.is_some() {
            if self.search.is_some() {
                self.close_search(cx);
            }
            if self.menu.bar_visible() || self.menu.is_open() {
                self.leave_menu(cx);
            }
            if self.folder_tree_menu.is_some() {
                self.close_folder_tree_menu(cx);
            }
        }
        self.sync_conflict_ui(conflict.as_ref());
        if conflict.is_some() && self.focus_conflict_on_render {
            self.conflict_focus.focus(window, cx);
            self.focus_conflict_on_render = false;
        }
        let search_input_to_focus = self.search.as_mut().and_then(|search| {
            search.focus_on_render.then(|| {
                search.focus_on_render = false;
                search.input.clone()
            })
        });
        if let Some(input) = search_input_to_focus {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        self.sync_overlay_modal(cx);
        let has_job_progress_pane = self.workspace.active().job_progress.is_some();
        let stale_job_progress_contact = self.job_progress_dock_contact.is_some_and(|contact| {
            contact.payload.workspace != self.workspace.active
                || self.workspace.active().topology_revision != contact.payload.topology_revision
                || self.workspace.active().job_progress.as_ref().is_none_or(|job| {
                    job.pane.id != contact.payload.pane
                        || job.generation != contact.payload.generation
                        || !matches!(job.placement, JobProgressPlacement::Floating { .. })
                })
        });
        if stale_job_progress_contact {
            self.clear_job_progress_dock_contact(cx);
        }
        let viewport_width = window.viewport_size().width.as_f32();
        let viewport_height = window.viewport_size().height.as_f32();
        let side_surface_width = ((viewport_width - 36.0) / 2.0).clamp(120.0, 400.0);
        let operation_surface = self.render_operation_surface(
            has_job_progress_pane,
            if has_job_progress_pane { side_surface_width } else { 400.0 },
            cx,
        );
        let floating_job_progress =
            self.render_floating_job_progress(viewport_width, viewport_height, cx);
        let operation_history = self.render_operation_history_overlay(window, cx);
        let top_overlay = self.top_overlay(cx);
        let conflict_overlay = (top_overlay == Some(overlays::OverlayKind::Conflict))
            .then(|| {
                conflict
                    .as_ref()
                    .map(|conflict| self.render_conflict_overlay(conflict, cx))
            })
            .flatten();
        let search_overlay = (top_overlay == Some(overlays::OverlayKind::Search))
            .then(|| self.render_search_overlay(cx))
            .flatten();
        let folder_tree_overlay = (top_overlay == Some(overlays::OverlayKind::FolderTree))
            .then(|| self.render_folder_tree_menu_overlay(window, cx))
            .flatten();
        let menu_overlay = (top_overlay == Some(overlays::OverlayKind::Menu))
            .then(|| self.render_menu_overlay(&menu_titles, window, window.viewport_size(), cx))
            .flatten();
        let quick_preview = (top_overlay.is_none())
            .then(|| self.render_quick_preview(window.viewport_size(), cx))
            .flatten();
        let shell_modal_active = top_overlay.is_some();
        let shell_key_delivery_enabled =
            Self::shell_key_delivery_enabled(text_input_focused, shell_modal_active);
        let allow_standard_tab_navigation = !text_input_focused && !shell_modal_active;
        let touch_drag_shell = cx.entity().downgrade();
        div()
            .size_full()
            .flex()
            .flex_col()
            // dropdown を絶対位置で置くため。
            .relative()
            .overflow_hidden()
            .bg(gpui_filer_ui::theme_palette(cx).surface_base)
            // Drag Escape must reach Shell even while a text editor owns focus. When no
            // drag is active this listener leaves Escape to the focused editor.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.clear_workspace_drop_hover();
                    this.clear_retained_internal_drag_route();
                    let pane_drag_cancelled = this.active_pane_drag.is_some_and(|drag| {
                        drag.workspace == this.workspace.active
                            && drag.topology_revision == this.workspace.active().topology_revision
                            && this.workspace.active().directory_pane(drag.source).is_some()
                    });
                    if pane_drag_cancelled {
                        if cx.has_active_drag() {
                            cx.stop_active_drag(window);
                        }
                        cx.stop_propagation();
                        this.clear_active_pane_drag(None);
                    }
                    this.clear_pane_drop_hover(cx);
                    // JobProgress title capture is the only route that creates this contact,
                    // so an active GPUI drag under it is the dock drag to stop here.
                    let progress_drag_cancelled = this.job_progress_dock_contact.is_some();
                    if progress_drag_cancelled {
                        if cx.has_active_drag() {
                            cx.stop_active_drag(window);
                        }
                        this.clear_job_progress_dock_contact(cx);
                    }
                    let transfer_cancelled = this.cancel_workspace_transfer_or_drag(cx);
                    if transfer_cancelled || progress_drag_cancelled {
                        cx.stop_propagation();
                    }
                }
            }))
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, window, cx| {
                this.update_workspace_drag_target(window, cx);
                let context = window.file_drop_context();
                this.update_workspace_drop_hover_from_native_drag(
                    window.mouse_position(),
                    context
                        .as_ref()
                        .and_then(|context| context.local_drag_session)
                        .map(|token| token.value()),
                    context.is_some(),
                    cx,
                );
                let Some(context) = context else {
                    return;
                };
                let mut cached = this.external_file_drop_paths.borrow_mut();
                if context.local_drag_session.is_some() {
                    *cached = None;
                } else if let Some(paths) = context.paths.as_ref() {
                    let paths = paths.paths().to_vec();
                    *cached = (!paths.is_empty()).then_some(paths);
                }
            }))
            .on_file_drop_exit(cx.listener(|this, _, _, _| {
                this.external_file_drop_paths.borrow_mut().take();
                this.clear_workspace_drop_hover();
            }))
            .when(shell_key_delivery_enabled, |root| {
                root.on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape"
                        && this.cancel_workspace_transfer_or_drag(cx)
                    {
                        cx.stop_propagation();
                    } else if this.handle_recovery_keyboard(&event.keystroke, window, cx)
                        || this.handle_key(&event.keystroke, event.is_held, window, cx)
                    {
                        cx.stop_propagation();
                    } else if allow_standard_tab_navigation
                        && event.keystroke.key == "tab"
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                        && !event.keystroke.modifiers.platform
                        && !event.keystroke.modifiers.function
                    {
                        // GPUI tab_stop は順序へ登録するだけなので、Shellが標準Tab移動へ橋渡しする。
                        if event.keystroke.modifiers.shift {
                            window.focus_prev(cx);
                        } else {
                            window.focus_next(cx);
                        }
                        if let Some(index) = this.focused_workspace_duplicate_index(window) {
                            if let Some((workspace_id, _)) = this.workspace.ordered.get(index) {
                                if let Some(anchor) = this
                                    .workspace_duplicate_scroll_anchors
                                    .get(workspace_id)
                                {
                                    anchor.scroll_to(window, cx);
                                }
                            }
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }
                }))
            })
            .on_drag_move::<InternalDragPayload>(cx.listener(|this, event, window, cx| {
                this.track_internal_workspace_drop_hover(event, cx);
                this.request_native_drag(event, window, cx);
            }))
            .on_drag_move::<PaneReorderPayload>(cx.listener(
                |this, event: &gpui::DragMoveEvent<PaneReorderPayload>, _window, cx| {
                if let Some(hover) = &this.pane_drop_hover
                    && !hover.bounds.contains(&event.event.position)
                {
                    this.clear_pane_drop_hover(cx);
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _, cx| {
                    if this.workspace_restore_menu_open {
                        this.workspace_restore_menu_open = false;
                        cx.notify();
                    }
                    this.end_tab_directory_drag(None, cx);
                    this.cancel_pending_native_drag(cx);
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.clear_active_pane_drag(None);
                    this.clear_pane_drop_hover(cx);
                    this.clear_workspace_drop_hover();
                    this.clear_retained_internal_drag_route();
                    this.clear_job_progress_dock_contact(cx);
                    this.finish_workspace_drag(window, cx);
                    this.sidebar_resize_start = None;
                    this.external_file_drop_paths.borrow_mut().take();
                    this.end_tab_directory_drag(None, cx);
                    this.cancel_pending_native_drag(cx);
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.clear_active_pane_drag(None);
                    this.clear_pane_drop_hover(cx);
                    this.clear_workspace_drop_hover();
                    this.clear_retained_internal_drag_route();
                    this.clear_job_progress_dock_contact(cx);
                    this.finish_workspace_drag(window, cx);
                    this.sidebar_resize_start = None;
                    this.external_file_drop_paths.borrow_mut().take();
                    this.end_tab_directory_drag(None, cx);
                    this.cancel_pending_native_drag(cx);
                }),
            )
            .child(titlebar)
            .children(notices)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .relative()
                            .w(px(self.sidebar_width))
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .flex_shrink_0()
                            .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                            .child(
                                div()
                                    .id("pane-tab-groups-scroll")
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.pane_rail_scroll)
                                    .children(leading_tab_groups)
                                    .child(
                                        div()
                                            .id(("pane-tab-group", pane_id.0))
                                            .h(active_tab_group_height)
                                            // V5同様、余った高さでfocused paneのtab groupをrail下端まで伸ばす。
                                            .flex_grow(1.0)
                                            .flex_shrink_0()
                                            .overflow_hidden()
                                            // 1ペインでは区切りを省き、レールと下端の背景をつなぐ。
                                            .when(show_pane_headers, |group| {
                                                group
                                                    .border_b_1()
                                                    .border_l_2()
                                                    .border_color(gpui_filer_ui::theme_palette(cx).border)
                                            })
                                            .child(VerticalTabs::new(
                                labels,
                                active_tab_id,
                                self.pane_tab_presentation(pane_id).scroll.clone(),
                                text,
                                self.pane_tab_presentation(pane_id).reorder.clone(),
                                // VerticalTabs の outer padding は左右8pxずつ。drag ghostへ実row幅を渡す。
                                px(self.sidebar_width - 16.0),
                                cx.listener(move |this, intent, _window, cx| {
                                    this.handle_tab(workspace_id, pane_id, intent, cx)
                                }),
                            )
                            .with_focused_pane(!self.job_progress_selection_is_current())
                            .with_owner_pane_key(pane_id.0)
                            .with_drop_target_decorator(
                                move |tab_id, element| {
                                    let Some((target_view, target_view_entity_id, snapshot)) =
                                        navigation_targets.get(&tab_id).cloned()
                                    else {
                                        return element;
                                    };
                                    let hover_view = target_view.clone();
                                    let drop_view = target_view.clone();
                                    let drop_shell = navigation_shell.clone();
                                    let local_ole_hover_shell = navigation_shell.clone();
                                    let local_ole_drop_shell = navigation_shell.clone();
                                    let internal_shell = navigation_shell.clone();
                                    let internal_drop_shell = navigation_shell.clone();
                                    let internal_probe_view = target_view.clone();
                                    let internal_probe_snapshot = snapshot.clone();
                                    let paths_cache = tab_paths_cache.clone();
                                    let drop_paths_cache = tab_paths_cache.clone();
                                    let hover_snapshot = snapshot.clone();
                                    let internal_hover_snapshot = snapshot.clone();
                                    let internal_drop_snapshot = snapshot.clone();
                                    let drop_snapshot = snapshot;
                                    let hover_effect = Rc::new(Cell::new(false));
                                    let hover_effect_for_event = hover_effect.clone();
                                    let hover_event_view = hover_view.clone();
                                    let hover_event_snapshot = hover_snapshot.clone();
                                    let hover_visual_snapshot = hover_snapshot;
                                    let internal_hover_view = hover_view.clone();
                                    let internal_drop_view = drop_view.clone();
                                    let internal_tab_id = tab_id;
                                    let internal_view_id = target_view_entity_id;
                                    element
                                        .on_drag_move::<crate::InternalDragPayload>(
                                            move |event, _window, cx| {
                                                let payload = event.drag(cx);
                                                if peek_internal_drag_snapshot(&payload.snapshot)
                                                    .is_none()
                                                {
                                                    return;
                                                }
                                                if let Some(snapshot) =
                                                    internal_probe_snapshot.as_ref()
                                                {
                                                    let _ = internal_probe_view.update(cx, |view, cx| {
                                                        view.prepare_navigation_target(snapshot, cx);
                                                    });
                                                }
                                            },
                                        )
                                        .drag_over::<crate::InternalDragPayload>(
                                            move |style, payload, _window, cx| {
                                                let eligible = peek_internal_drag_snapshot(
                                                    &payload.snapshot,
                                                )
                                                .zip(internal_hover_snapshot.as_ref())
                                                .and_then(|(source, target_snapshot)| {
                                                    let fresh = internal_hover_view
                                                        .read_with(cx, |view, _| {
                                                            refresh_navigation_target_snapshot(
                                                                view,
                                                                target_snapshot,
                                                            )
                                                        })
                                                        .ok()
                                                        .flatten()?;
                                                    let _ = internal_hover_view.update(cx, |view, cx| {
                                                        view.prepare_navigation_target(&fresh, cx);
                                                    });
                                                    internal_hover_view
                                                        .read_with(cx, |view, _| {
                                                            view.navigation_drop_target(&fresh)
                                                        })
                                                        .ok()
                                                        .flatten()
                                                        .map(|target| (source, target))
                                                })
                                                .is_some_and(|(source, target)| {
                                                    internal_shell
                                                        .read_with(cx, |shell, cx| {
                                                            shell.internal_navigation_move_candidate_for_ui(
                                                                payload.source,
                                                                &source,
                                                                internal_tab_id,
                                                                internal_view_id,
                                                                &target,
                                                                cx,
                                                            )
                                                        })
                                                        .unwrap_or(false)
                                                });
                                                if eligible {
                                                    style.border_2().border_color(gpui::hsla(
                                                        0.58, 0.45, 0.62, 0.8,
                                                    ))
                                                } else {
                                                    style
                                                }
                                            },
                                        )
                                        .on_drop::<crate::InternalDragPayload>(
                                            move |payload, _window, cx| {
                                                cx.stop_propagation();
                                                let snapshot = take_internal_drag_snapshot(
                                                    &payload.snapshot,
                                                );
                                                let target = internal_drop_snapshot
                                                    .as_ref()
                                                    .and_then(|snapshot| {
                                                        let fresh = internal_drop_view
                                                            .read_with(cx, |view, _| {
                                                                refresh_navigation_target_snapshot(
                                                                    view,
                                                                    snapshot,
                                                                )
                                                            })
                                                            .ok()
                                                            .flatten()?;
                                                        let _ = internal_drop_view.update(cx, |view, cx| {
                                                            view.prepare_navigation_target(&fresh, cx);
                                                        });
                                                        internal_drop_view
                                                            .read_with(cx, |view, _| {
                                                                view.navigation_drop_target(&fresh)
                                                            })
                                                            .ok()
                                                            .flatten()
                                                    });
                                                if let Some(snapshot) = snapshot {
                                                    let _ = internal_drop_shell.update(cx, |shell, cx| {
                                                        shell.admit_internal_navigation_move(
                                                            payload.source,
                                                            snapshot,
                                                            internal_tab_id,
                                                            internal_view_id,
                                                            target,
                                                            cx,
                                                        )
                                                    });
                                                }
                                            },
                                        )
                                        .on_mouse_move(move |_, window, cx| {
                                            hover_effect_for_event.set(false);
                                            let Some(context) = window.file_drop_context() else {
                                                return;
                                            };
                                            context.response.set_effect(gpui::FileDropEffect::None);
                                            if context.local_drag_session.is_some() {
                                                paths_cache.borrow_mut().take();
                                                let (Some(local_token), Some(snapshot)) = (
                                                    context.local_drag_session,
                                                    hover_event_snapshot.as_ref(),
                                                ) else {
                                                    return;
                                                };
                                                let paths = context
                                                    .paths
                                                    .as_ref()
                                                    .map(|paths| paths.paths());
                                                if paths.is_some_and(|paths| paths.is_empty()) {
                                                    return;
                                                }
                                                let Some(fresh) = hover_event_view
                                                    .read_with(cx, |view, _| {
                                                        refresh_navigation_target_snapshot(view, snapshot)
                                                    })
                                                    .ok()
                                                    .flatten()
                                                else {
                                                    return;
                                                };
                                                let _ = hover_event_view.update(cx, |view, cx| {
                                                    view.prepare_navigation_target(&fresh, cx);
                                                });
                                                let target = hover_event_view
                                                    .read_with(cx, |view, _| {
                                                        view.navigation_drop_target(&fresh)
                                                    })
                                                    .ok()
                                                    .flatten();
                                                let effect = target.and_then(|target| {
                                                    local_ole_hover_shell
                                                        .read_with(cx, |shell, cx| {
                                                            shell.local_ole_navigation_move_candidate_effect(
                                                                local_token.value(),
                                                                tab_id,
                                                                target_view_entity_id,
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
                                                        LocalOleDropEffect::Copy => gpui::FileDropEffect::Copy,
                                                        LocalOleDropEffect::Move => gpui::FileDropEffect::Move,
                                                    });
                                                }
                                                return;
                                            }
                                            let paths = if let Some(paths) = context.paths.as_ref()
                                            {
                                                let paths = paths.paths().to_vec();
                                                *paths_cache.borrow_mut() =
                                                    (!paths.is_empty()).then_some(paths.clone());
                                                paths
                                            } else {
                                                paths_cache.borrow().clone().unwrap_or_default()
                                            };
                                            if paths.is_empty() {
                                                return;
                                            }
                                            let Some(snapshot) = hover_event_snapshot.as_ref()
                                            else {
                                                return;
                                            };
                                            let Some(fresh) = hover_event_view
                                                .read_with(cx, |view, _| {
                                                    refresh_navigation_target_snapshot(view, snapshot)
                                                })
                                                .ok()
                                                .flatten()
                                            else {
                                                return;
                                            };
                                            let _ = hover_event_view.update(cx, |view, cx| {
                                                view.prepare_navigation_target(&fresh, cx);
                                            });
                                            let eligible = hover_event_view
                                                .read_with(cx, |view, _| {
                                                    view.navigation_drop_target(&fresh).is_some()
                                                })
                                                .unwrap_or(false);
                                            if eligible {
                                                let _ = hover_event_view.update(cx, |view, cx| {
                                                    let Some(target) =
                                                        view.navigation_drop_target(&fresh)
                                                    else {
                                                        return;
                                                    };
                                                    let payload = gpui::ExternalPaths(paths.clone().into());
                                                    crate::capture_external_drag_session(
                                                        &view.external_drag_session,
                                                        &payload,
                                                        target.location_generation(),
                                                        target.presentation_revision(),
                                                    );
                                                    view.prepare_external_drop(&paths, &target, cx);
                                                });
                                                hover_effect_for_event.set(true);
                                                context
                                                    .response
                                                    .set_effect(gpui::FileDropEffect::Copy);
                                            }
                                        })
                                        .drag_over::<gpui::ExternalPaths>(
                                            move |style, _payload, _window, cx| {
                                                let eligible = hover_effect.get()
                                                    && hover_view
                                                        .read_with(cx, |view, _| {
                                                            hover_visual_snapshot
                                                                .as_ref()
                                                                .and_then(|snapshot| {
                                                                    refresh_navigation_target_snapshot(
                                                                        view,
                                                                        snapshot,
                                                                    )
                                                                })
                                                                .and_then(|fresh| {
                                                                    view.navigation_drop_target(&fresh)
                                                                })
                                                                .is_some()
                                                        })
                                                        .unwrap_or(false);
                                                if eligible {
                                                    style.border_2().border_color(gpui::hsla(
                                                        0.58, 0.45, 0.62, 0.8,
                                                    ))
                                                } else {
                                                    style
                                                }
                                            },
                                        )
                                        .on_drop::<gpui::ExternalPaths>(
                                            move |payload, window, cx| {
                                                drop_paths_cache.borrow_mut().take();
                                                cx.stop_propagation();
                                                let Some(context) = window.file_drop_context()
                                                else {
                                                    return;
                                                };
                                                context.response.set_effect(gpui::FileDropEffect::None);
                                                if payload.paths().is_empty() {
                                                    return;
                                                }
                                                let Some(drop_snapshot) = drop_snapshot.as_ref()
                                                else {
                                                    return;
                                                };
                                                let Some(fresh) = drop_view
                                                    .read_with(cx, |view, _| {
                                                        refresh_navigation_target_snapshot(view, drop_snapshot)
                                                    })
                                                    .ok()
                                                    .flatten()
                                                else {
                                                    return;
                                                };
                                                let _ = drop_view.update(cx, |view, cx| {
                                                    view.prepare_navigation_target(&fresh, cx);
                                                });
                                                let Some(target) = drop_view
                                                    .read_with(cx, |view, _| {
                                                        view.navigation_drop_target(&fresh)
                                                    })
                                                    .ok()
                                                    .flatten()
                                                else {
                                                    return;
                                                };
                                                let sources = context
                                                    .paths
                                                    .as_ref()
                                                    .map(|paths| paths.paths().to_vec())
                                                    .unwrap_or_default();
                                                if sources.is_empty() || sources != payload.paths()
                                                {
                                                    return;
                                                }
                                                let prepared = drop_view
                                                    .read_with(cx, |view, _| {
                                                        crate::prepared_external_drop(
                                                            &view.external_drag_session,
                                                            &sources,
                                                            &target,
                                                        )
                                                    })
                                                    .ok()
                                                    .flatten();
                                                if let Some(local_token) = context.local_drag_session {
                                                    let effect = local_ole_drop_shell
                                                        .update(cx, |shell, cx| {
                                                            shell.admit_local_ole_navigation_move(
                                                                local_token.value(),
                                                                tab_id,
                                                                target_view_entity_id,
                                                                target,
                                                                sources,
                                                                cx,
                                                            )
                                                        })
                                                        .ok()
                                                        .flatten();
                                                    if let Some(effect) = effect {
                                                        context.response.set_effect(match effect {
                                                            LocalOleDropEffect::Copy => gpui::FileDropEffect::Copy,
                                                            LocalOleDropEffect::Move => gpui::FileDropEffect::Move,
                                                        });
                                                    }
                                                    return;
                                                }
                                                if context.local_drag_session.is_some() {
                                                    return;
                                                }
                                                let intent = crate::foreign_file_drop_intent(&context);
                                                let (admitted, effect) = drop_shell
                                                    .update(cx, |shell, cx| match intent {
                                                        input::ForeignFileDropIntent::Link
                                                            if context.link_allowed => {
                                                                context.response.set_effect(gpui::FileDropEffect::None);
                                                                let admitted = shell.admit_external_symbolic_link(
                                                                    tab_id,
                                                                    target_view_entity_id,
                                                                    target,
                                                                    sources,
                                                                    cx,
                                                                );
                                                                (admitted, gpui::FileDropEffect::Link)
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
                                                                    target_view_entity_id,
                                                                    target,
                                                                    sources,
                                                                    false,
                                                                    Some(feedback),
                                                                    cx,
                                                                ),
                                                                gpui::FileDropEffect::Copy,
                                                            )
                                                        }
                                                        input::ForeignFileDropIntent::Default => {
                                                            if let Some(prepared) = prepared
                                                                .filter(|prepared| prepared.same_volume)
                                                                && let Some(feedback) = context
                                                                    .transfer_lease
                                                                    .as_ref()
                                                                    .and_then(gpui::FileDropTransferLease::start)
                                                            {
                                                                (
                                                                    shell.admit_external_navigation_prepared_move(
                                                                        tab_id,
                                                                        target_view_entity_id,
                                                                        target,
                                                                        sources,
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
                                                                        target_view_entity_id,
                                                                        target,
                                                                        sources,
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
                                                                target_view_entity_id,
                                                                target,
                                                                sources,
                                                                false,
                                                                None,
                                                                cx,
                                                            ),
                                                            gpui::FileDropEffect::Copy,
                                                        ),
                                                    })
                                                    .unwrap_or((false, gpui::FileDropEffect::None));
                                                if admitted {
                                                    context
                                                        .response
                                                        .set_effect(effect);
                                                }
                                            },
                                        )
                                },
                                            )),
                                    )
                                    .children(self.render_job_progress_rail_entry(cx))
                                    .children(trailing_tab_groups),
                            )
                            .child(ReopenTabsButton::new(
                                cx.listener(|this, intent, _window, cx| {
                                    if let TabIntent::ReopenMenu { at } = intent {
                                        this.open_reopen_menu(*at, cx);
                                    }
                                }),
                            ))
                        .child(
                            div()
                                .id("sidebar-width-splitter")
                                .absolute()
                                .right(px(0.0))
                                .top_0()
                                .bottom_0()
                                .w(px(SIDEBAR_SPLITTER_WIDTH))
                                .cursor_col_resize()
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                                        if event.source != gpui::MouseInputSource::Mouse {
                                            return;
                                        }
                                        cx.stop_propagation();
                                        this.sidebar_resize_start = Some((
                                            this.sidebar_width,
                                            event.position.x.as_f32(),
                                        ));
                                    }),
                                )
                                .on_drag_with_threshold_when(
                                    SidebarResizeDrag,
                                    px(2.0),
                                    |down| {
                                        down.source == gpui::MouseInputSource::Mouse
                                            && down.button == gpui::MouseButton::Left
                                    },
                                    |_, _, _, cx| cx.new(|_| gpui::Empty),
                                )
                                .on_drag_move(cx.listener(
                                    |this,
                                     event: &gpui::DragMoveEvent<SidebarResizeDrag>,
                                     window,
                                     cx| {
                                        let Some((start_width, start_x)) =
                                            this.sidebar_resize_start
                                        else {
                                            return;
                                        };
                                        this.set_sidebar_width(
                                            start_width + event.event.position.x.as_f32() - start_x,
                                            window.viewport_size().width.as_f32(),
                                            cx,
                                        );
                                    },
                                ))
                                .on_drop(cx.listener(|this, _: &SidebarResizeDrag, _, cx| {
                                    this.sidebar_resize_start = None;
                                    cx.notify();
                                }))
                                .on_mouse_up(
                                    gpui::MouseButton::Left,
                                    cx.listener(|this, _, _, _| {
                                        this.sidebar_resize_start = None;
                                    }),
                                )
                                .on_mouse_up_out(
                                    gpui::MouseButton::Left,
                                    cx.listener(|this, _, _, _| {
                                        this.sidebar_resize_start = None;
                                    }),
                                ),
                    ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .overflow_hidden()
                            .child(pane_content),
                    ),
            )
            .children(operation_surface)
            .children(floating_job_progress)
            .children(operation_history)
            .children(conflict_overlay)
            .children(search_overlay)
            .children(folder_tree_overlay)
            // 最後に置く。dropdown はこの面より上に描かれる。
            .children(menu_overlay)
            .children(quick_preview)
            .child(
                gpui::canvas(|_, _, _| {}, move |_, _, window, _| {
                    let shell = touch_drag_shell.clone();
                    window.on_mouse_event(
                        move |event: &gpui::TouchDragEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Capture {
                                return;
                            }
                            let _ = shell.update(cx, |shell, cx| {
                                shell.handle_touch_drag_terminal(
                                    event.phase,
                                    event.start_position,
                                    cx,
                                );
                            });
                        },
                    );
                })
                .absolute()
                .inset_0(),
            )
    }
}

fn pane_drop_preview(zone: PaneDropZone) -> gpui::AnyElement {
    let (left, top, width, height) = match zone {
        PaneDropZone::Merge => (35.0, 35.0, 30.0, 30.0),
        PaneDropZone::Split {
            direction: SplitDirection::Vertical,
            after: false,
        } => (0.0, 0.0, 100.0, 50.0),
        PaneDropZone::Split {
            direction: SplitDirection::Vertical,
            after: true,
        } => (0.0, 50.0, 100.0, 50.0),
        PaneDropZone::Split {
            direction: SplitDirection::Horizontal,
            after: false,
        } => (0.0, 0.0, 50.0, 100.0),
        PaneDropZone::Split {
            direction: SplitDirection::Horizontal,
            after: true,
        } => (50.0, 0.0, 50.0, 100.0),
    };
    div()
        .absolute()
        .left(gpui::relative(left / 100.0))
        .top(gpui::relative(top / 100.0))
        .w(gpui::relative(width / 100.0))
        .h(gpui::relative(height / 100.0))
        .border_2()
        .border_color(gpui::rgba(0x3f8cffcc))
        .bg(gpui::rgba(0x3f8cff33))
        .into_any_element()
}

impl FilerShell {
    fn render_job_progress_rail_entry(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let job = self.workspace.active().job_progress.as_ref()?;
        let selection = JobProgressSelection {
            workspace: self.workspace.active,
            pane: job.pane.id,
            generation: job.generation,
        };
        let is_selected = self.job_progress_selection == Some(selection)
            && self.job_progress_selection_is_current();
        Some(
            div()
                .id("job-progress-rail-entry")
                .track_focus(&self.job_progress_rail_focus)
                .focusable()
                .tab_stop(true)
                .w_full()
                .h(px(44.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(RADIUS_SM))
                .text_size(px(12.0))
                .text_color(gpui_filer_ui::theme_palette(cx).text)
                .when(is_selected, |style| {
                    style.bg(gpui_filer_ui::theme_palette(cx).accent.opacity(0.18))
                })
                .hover(|style| style.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong))
                .focus_visible(|style| style.border_1().border_color(ON_ACCENT))
                // 進捗専用 IconId がないため、作業の開始・実行を示す既存の Play を使う。
                .child(
                    Icon::new(IconId::Play)
                        .size(px(16.0))
                        .color(gpui_filer_ui::theme_palette(cx).text.into()),
                )
                .child("Job progress")
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if event.standard_click() {
                        this.activate_job_progress_selection(selection, window, cx);
                    }
                }))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                    {
                        cx.stop_propagation();
                        this.activate_job_progress_selection(selection, window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    pub(super) fn pane_header_title(&self, pane_id: PaneId, cx: &Context<Self>) -> Option<String> {
        let pane = self.workspace.active().directory_pane(pane_id)?;
        let tab = pane.tabs.iter().find(|tab| tab.id == pane.active_tab_id)?;
        Some(match tab.view.read(cx).location() {
            Some(Location::Path(path)) => navigation::label(&path),
            Some(Location::ThisPc) => self.translator.text(Message::ThisPc).into_owned(),
            Some(Location::ShellRoot(root)) => root.display_name().into(),
            Some(Location::ShellItem { display_name, .. }) => display_name,
            Some(location @ Location::Archive { .. }) => {
                location.display_label().unwrap_or_default()
            }
            None => self.translator.text(Message::SampleList).into_owned(),
        })
    }

    fn render_pane_header(
        &mut self,
        pane_id: PaneId,
        title: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focused = self.workspace.active().focused_pane_id == pane_id;
        let workspace_id = self.workspace.active;
        let topology_revision = self.workspace.active().topology_revision;
        let reorder_payload = PaneReorderPayload {
            workspace: workspace_id,
            source: pane_id,
            topology_revision,
        };
        let drag_shell = cx.entity().downgrade();
        let ghost_title = title.clone();
        let title_click = cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
            cx.stop_propagation();
            if event.standard_click() && this.workspace.active().directory_pane(pane_id).is_some() {
                this.clear_job_progress_selection();
                this.workspace.active_mut().focused_pane_id = pane_id;
                this.sync_file_info_projection(cx);
                this.pending_focus = true;
                this.save_requested = true;
                cx.notify();
            }
        });
        let title_area = div()
            .id(("pane-drag-handle", pane_id.0))
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .px(px(12.0))
            .text_size(px(12.0))
            .text_color(gpui_filer_ui::theme_palette(cx).text)
            .child(div().min_w_0().truncate().child(title))
            .on_click(title_click)
            .on_drag(reorder_payload, move |_, _, _, cx| {
                let _ = drag_shell.update(cx, |shell, _| {
                    shell.active_pane_drag = Some(reorder_payload);
                });
                cx.new(|_| PaneReorderGhost {
                    title: ghost_title.clone(),
                })
            });

        let can_close = self.workspace.active().panes.len() > 1;
        let close = can_close.then(|| {
            div()
                .id(("close-pane", pane_id.0))
                .w(px(44.0))
                .h(px(44.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_SM))
                .hover(|style| style.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong))
                .child(
                    Icon::new(IconId::Close)
                        .size(px(14.0))
                        .color(gpui_filer_ui::theme_palette(cx).text.into()),
                )
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        if event.standard_click() {
                            this.close_pane(pane_id, cx);
                        }
                    }),
                )
        });

        let can_drop_revision = topology_revision;
        div()
            .id(("pane-header", pane_id.0))
            .h(px(PANE_HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(gpui_filer_ui::theme_palette(cx).border)
            .when(focused, |header| {
                header.bg(gpui_filer_ui::theme_palette(cx).surface_raised)
            })
            .child(title_area)
            .children(close)
            .can_drop(move |value, _, _| {
                value
                    .downcast_ref::<PaneReorderPayload>()
                    .is_some_and(|drag| {
                        drag.workspace == workspace_id
                            && drag.source != pane_id
                            && drag.topology_revision == can_drop_revision
                    })
            })
            .on_drop::<PaneReorderPayload>(cx.listener(
                move |this, drag: &PaneReorderPayload, _window, cx| {
                    this.clear_active_pane_drag(Some(*drag));
                    if drag.workspace == this.workspace.active {
                        this.swap_pane_positions(drag.source, pane_id, drag.topology_revision, cx);
                    }
                },
            ))
    }

    fn render_pane_layout(
        &mut self,
        layout: &PaneLayoutNode,
        workspace_width: f32,
        show_header: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match layout {
            PaneLayoutNode::Pane(pane_id) => {
                let target_pane_id = *pane_id;
                if self.workspace.active().job_progress.as_ref().is_some_and(|job| {
                    job.pane.id == *pane_id && matches!(job.placement, JobProgressPlacement::Docked)
                }) {
                    return self.render_docked_job_progress(*pane_id, cx);
                }
                let Some(view) = self.workspace.active().directory_pane(*pane_id).and_then(|pane| {
                    pane.tabs
                        .iter()
                        .find(|tab| tab.id == pane.active_tab_id)
                        .map(|tab| tab.view.clone())
                }) else {
                    return div().into_any_element();
                };
                let title = self
                    .pane_header_title(*pane_id, cx)
                    .unwrap_or_else(|| "".into());
                let fraction = self
                    .workspace
                    .active()
                    .pane_layout
                    .width_fractions()
                    .into_iter()
                    .find_map(|(id, fraction)| (id == *pane_id).then_some(fraction))
                    .unwrap_or(1.0);
                let info_width = self.file_info_width(*pane_id, workspace_width * fraction);
                if *pane_id == self.workspace.active().focused_pane_id {
                    let visible = self.file_info_open && info_width > 0.0;
                    if self.file_info_visible != visible {
                        self.file_info_visible = visible;
                        let source_id = self.file_info_source;
                        let target = self
                            .file_info_projection
                            .as_ref()
                            .and_then(|projection| projection.hash_target.clone());
                        if let Some(source_id) = source_id {
                            self.set_file_info_hash_target(source_id, target, cx);
                            let target = self
                                .file_info_projection
                                .as_ref()
                                .and_then(|projection| projection.exif_target.clone());
                            self.set_file_info_exif_target(source_id, target, cx);
                            let target = self
                                .file_info_projection
                                .as_ref()
                                .and_then(|projection| projection.shortcut_target.clone());
                            self.set_file_info_shortcut_target(source_id, target, cx);
                        }
                    }
                }
                let info = (info_width > 0.0)
                    .then(|| self.render_file_info_pane(info_width, view.clone(), cx));
                let dock_targets = self.render_job_progress_dock_targets(*pane_id, cx);
                let pane_drop_preview = self
                    .pane_drop_hover
                    .as_ref()
                    .filter(|hover| {
                        hover.workspace == self.workspace.active
                            && hover.target == target_pane_id
                            && hover.topology_revision
                                == self.workspace.active().topology_revision
                    })
                    .map(|hover| pane_drop_preview(hover.zone));
                let pane_drop_workspace = self.workspace.active;
                let pane_drop_revision = self.workspace.active().topology_revision;
                let pane_drop_directories = self.workspace.active().directory_pane_ids();
                let content = div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .relative()
                    .flex()
                    .child(div().flex_1().min_w_0().min_h_0().h_full().child(view))
                    .children(info)
                    .children(pane_drop_preview)
                    .can_drop(move |value, _, _| {
                        value
                            .downcast_ref::<PaneReorderPayload>()
                            .is_some_and(|drag| {
                                drag.workspace == pane_drop_workspace
                                    && drag.source != target_pane_id
                                    && drag.topology_revision == pane_drop_revision
                                    && pane_drop_directories.contains(&drag.source)
                                    && pane_drop_directories.contains(&target_pane_id)
                            })
                    })
                    .on_drag_move::<PaneReorderPayload>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<PaneReorderPayload>, _window, cx| {
                            let drag = *event.drag(cx);
                            this.update_pane_drop_hover(
                                drag,
                                target_pane_id,
                                event.bounds,
                                event.event.position,
                                cx,
                            );
                        },
                    ))
                    .on_drop::<PaneReorderPayload>(cx.listener(
                        move |this, drag, window, cx| {
                            this.commit_pane_drop(*drag, target_pane_id, window.mouse_position(), cx);
                        },
                    ));
                let header = if show_header {
                    Some(self.render_pane_header(target_pane_id, title, cx))
                } else {
                    None
                };
                div()
                    .id(("directory-pane", pane_id.0))
                    .flex()
                    .flex_col()
                    .w_full()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .children(header)
                    .child(content)
                    .children(dock_targets)
                    .into_any_element()
            }
            PaneLayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let first_content =
                    self.render_pane_layout(first, workspace_width, show_header, cx);
                let second_content =
                    self.render_pane_layout(second, workspace_width, show_header, cx);
                let (first_anchor, second_anchor) = layout.split_anchor();
                let drag = PaneResizePayload {
                    workspace: self.workspace.active,
                    first_anchor,
                    second_anchor,
                    topology_revision: self.workspace.active().topology_revision,
                };
                let current_direction = *direction;
                let handle = div()
                    .id(gpui::ElementId::Name(
                        format!("pane-split-handle-{}-{}", first_anchor.0, second_anchor.0).into(),
                    ))
                    .absolute()
                    .top_0()
                    .left_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_col_resize()
                    .when(current_direction == SplitDirection::Horizontal, |handle| {
                        handle
                            .left(gpui::relative(*ratio))
                            .ml(px(-PANE_DIVIDER_HANDLE / 2.0))
                            .w(px(PANE_DIVIDER_HANDLE))
                            .h_full()
                    })
                    .when(current_direction == SplitDirection::Vertical, |handle| {
                        handle
                            .top(gpui::relative(*ratio))
                            .mt(px(-PANE_DIVIDER_HANDLE / 2.0))
                            .w_full()
                            .h(px(PANE_DIVIDER_HANDLE))
                            .cursor_row_resize()
                    })
                    .on_drag(drag, |_, _, _, cx| cx.new(|_| gpui::Empty))
                    .child(
                        div()
                            .id(gpui::ElementId::Name(
                                format!(
                                    "pane-split-direction-{}-{}",
                                    first_anchor.0, second_anchor.0
                                )
                                .into(),
                            ))
                            .w(px(PANE_DIVIDER_HANDLE))
                            .h(px(PANE_DIVIDER_HANDLE))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(3.0))
                            .rounded(px(RADIUS_SM))
                            .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                            .hover(|style| {
                                style.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong)
                            })
                            .text_color(gpui_filer_ui::theme_palette(cx).text)
                            .text_size(px(11.0))
                            .child(
                                Icon::new(IconId::SplitPane)
                                    .size(px(14.0))
                                    .color(gpui_filer_ui::theme_palette(cx).text.into()),
                            )
                            .child(if current_direction == SplitDirection::Horizontal {
                                "縦"
                            } else {
                                "横"
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    cx.stop_propagation();
                                    if event.standard_click() {
                                        this.toggle_split_direction(drag, window, cx);
                                    }
                                },
                            )),
                    );
                let divider = match direction {
                    SplitDirection::Horizontal => div()
                        .w(px(PANE_DIVIDER_WIDTH))
                        .h_full()
                        .flex_shrink_0()
                        .bg(gpui_filer_ui::theme_palette(cx).border),
                    SplitDirection::Vertical => div()
                        .w_full()
                        .h(px(PANE_DIVIDER_WIDTH))
                        .flex_shrink_0()
                        .bg(gpui_filer_ui::theme_palette(cx).border),
                };
                let mut split = div()
                    .id(gpui::ElementId::Name(
                        format!("pane-split-{}-{}", first_anchor.0, second_anchor.0).into(),
                    ))
                    .flex()
                    .w_full()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .relative()
                    .overflow_hidden();
                split = match direction {
                    SplitDirection::Horizontal => split.flex_row(),
                    SplitDirection::Vertical => split.flex_col(),
                };
                let first = div()
                    .flex_basis(gpui::relative(*ratio))
                    .min_w_0()
                    .min_h_0()
                    .child(first_content);
                let second = div()
                    .flex_basis(gpui::relative(1.0 - *ratio))
                    .min_w_0()
                    .min_h_0()
                    .child(second_content);
                split
                    .children([first, divider, second])
                    .child(handle)
                    .on_drag_move::<PaneResizePayload>(cx.listener(
                        |this, event: &gpui::DragMoveEvent<PaneResizePayload>, window, cx| {
                            let drag = *event.drag(cx);
                            this.handle_pane_resize_move(
                                drag,
                                event.event.position,
                                event.bounds,
                                window,
                                cx,
                            );
                        },
                    ))
                    .on_drop::<PaneResizePayload>(cx.listener(|this, _drag, _window, cx| {
                        this.save_requested = true;
                        cx.notify();
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_file_info_pane(
        &mut self,
        width: f32,
        source_view: gpui::Entity<DirectoryListDemo>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut body = div()
            .w(px(width))
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(12.0))
            .border_l_1()
            .border_color(gpui_filer_ui::theme_palette(cx).border)
            .bg(gpui_filer_ui::theme_palette(cx).surface_base)
            .text_color(gpui_filer_ui::theme_palette(cx).text);
        let Some(projection) = self.file_info_projection.as_ref() else {
            return body
                .child(self.translator.text(Message::FileInfoEmpty).into_owned())
                .into_any_element();
        };
        if projection.selected_count > 1 {
            body = body.child(self.translator.text(Message::FileInfoMultiple).into_owned());
            let aggregate = projection.aggregate.as_ref().map(|aggregate| {
                (
                    aggregate.file_count,
                    aggregate.folder_count,
                    aggregate.total_size,
                    aggregate.unknown_size_count,
                )
            });
            for line in self
                .translator
                .multiple_selection_rows(projection.selected_count, aggregate)
            {
                body = body.child(line);
            }
            body = body.child(div().child(format!(
                "{}: {}",
                self.translator.text(Message::FileInfoHash),
                self.translator.text(Message::FileInfoHashUnavailable)
            )));
            if let Some(token) = projection.copy_token.as_ref() {
                let source_id = source_view.entity_id();
                let name_click = cx.listener({
                    let token = token.clone();
                    move |this, event: &gpui::ClickEvent, _window, cx| {
                        if event.standard_click() {
                            this.copy_file_info_text(
                                source_id,
                                token.clone(),
                                crate::InfoCopyField::Name,
                                cx,
                            );
                        }
                    }
                });
                let path_click = cx.listener({
                    let token = token.clone();
                    move |this, event: &gpui::ClickEvent, _window, cx| {
                        if event.standard_click() {
                            this.copy_file_info_text(
                                source_id,
                                token.clone(),
                                crate::InfoCopyField::Location,
                                cx,
                            );
                        }
                    }
                });
                let key_name = cx.listener({
                    move |_this, event: &gpui::KeyDownEvent, _window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                        {
                            cx.stop_propagation();
                        }
                    }
                });
                let key_path = cx.listener({
                    move |_this, event: &gpui::KeyDownEvent, _window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                        {
                            cx.stop_propagation();
                        }
                    }
                });
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .id("file-info-copy-names")
                                .track_focus(&self.file_info_copy_names_focus)
                                .focusable()
                                .h(px(44.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(gpui_filer_ui::theme_palette(cx).border)
                                .rounded(px(4.0))
                                .child(self.translator.text(Message::FileInfoCopyNames).into_owned())
                                .on_click(name_click)
                                .on_key_down(key_name),
                        )
                        .child(
                            div()
                                .id("file-info-copy-paths")
                                .track_focus(&self.file_info_copy_paths_focus)
                                .focusable()
                                .h(px(44.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(gpui_filer_ui::theme_palette(cx).border)
                                .rounded(px(4.0))
                                .child(self.translator.text(Message::FileInfoCopyPaths).into_owned())
                                .on_click(path_click)
                                .on_key_down(key_path),
                        ),
                );
            }
            return body.into_any_element();
        }
        let Some(item) = projection.item.as_ref() else {
            return body
                .child(self.translator.text(Message::FileInfoEmpty).into_owned())
                .child(format!(
                    "{}: {}",
                    self.translator.text(Message::FileInfoHash),
                    self.translator.text(Message::FileInfoHashUnavailable)
                ))
                .into_any_element();
        };
        let row = |label: String, value: String| {
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                        .text_size(px(12.0))
                        .child(label),
                )
                .child(div().min_w_0().whitespace_normal().child(value))
        };
        let location = item
            .path
            .parent()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        body = body
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(item.name.clone()),
            )
            .child(row(
                self.translator.text(Message::FileInfoKind).into_owned(),
                item.kind.clone(),
            ))
            .child(row(
                self.translator.text(Message::FileInfoSize).into_owned(),
                self.translator.size(item.size),
            ))
            .child(row(
                self.translator.text(Message::FileInfoModified).into_owned(),
                self.translator.modified(item.modified),
            ))
            .child(row(
                self.translator.text(Message::FileInfoLocation).into_owned(),
                location,
            ));
        if let Some(target) = item.link_target.as_ref() {
            body = body.child(row(
                self.translator
                    .text(Message::FileInfoLinkTarget)
                    .into_owned(),
                target.display().to_string(),
            ));
        }
        body = body.child(row(
            self.translator.text(Message::FileInfoHash).into_owned(),
            match self.file_info_hash.state() {
                crate::file_info_hash::State::Unavailable => self
                    .translator
                    .text(Message::FileInfoHashUnavailable)
                    .into_owned(),
                crate::file_info_hash::State::Loading => self
                    .translator
                    .text(Message::FileInfoHashLoading)
                    .into_owned(),
                crate::file_info_hash::State::Ready(digest) => digest.clone(),
                crate::file_info_hash::State::Failed => self
                    .translator
                    .text(Message::FileInfoHashFailed)
                    .into_owned(),
            },
        ));
        if projection.shortcut_target.is_some() {
            match self.file_info_shortcut.state() {
                crate::file_info_shortcut::State::Unavailable => {}
                crate::file_info_shortcut::State::Loading => {
                    body = body.child(row(
                        self.translator.text(Message::FileInfoShortcut).into_owned(),
                        self.translator
                            .text(Message::FileInfoShortcutLoading)
                            .into_owned(),
                    ));
                }
                crate::file_info_shortcut::State::Failed => {
                    body = body.child(row(
                        self.translator.text(Message::FileInfoShortcut).into_owned(),
                        self.translator
                            .text(Message::FileInfoShortcutFailed)
                            .into_owned(),
                    ));
                }
                crate::file_info_shortcut::State::Ready(details) => {
                    body = body.child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(self.translator.text(Message::FileInfoShortcut).into_owned()),
                    );
                    body = body.child(row(
                        self.translator
                            .text(Message::FileInfoShortcutTarget)
                            .into_owned(),
                        details.target.display().to_string(),
                    ));
                    body = body.child(row(
                        self.translator
                            .text(Message::FileInfoShortcutStatus)
                            .into_owned(),
                        self.translator
                            .text(if details.target_exists {
                                Message::FileInfoShortcutValid
                            } else {
                                Message::FileInfoShortcutBroken
                            })
                            .into_owned(),
                    ));
                    if let Some(working_directory) = &details.working_directory {
                        body = body.child(row(
                            self.translator
                                .text(Message::FileInfoShortcutWorkingDirectory)
                                .into_owned(),
                            working_directory.display().to_string(),
                        ));
                    }
                    if let Some(arguments) = &details.arguments {
                        body = body.child(row(
                            self.translator
                                .text(Message::FileInfoShortcutArguments)
                                .into_owned(),
                            arguments.clone(),
                        ));
                    }
                }
            }
        }
        if item
            .hash_target
            .as_ref()
            .is_some_and(|target| crate::file_info_exif::is_candidate(&target.path))
        {
            match self.file_info_exif.state() {
                crate::file_info_exif::State::Unavailable => {}
                crate::file_info_exif::State::Loading => {
                    body = body.child(row(
                        self.translator.text(Message::FileInfoExif).into_owned(),
                        self.translator
                            .text(Message::FileInfoExifLoading)
                            .into_owned(),
                    ))
                }
                crate::file_info_exif::State::Failed => {
                    body = body.child(row(
                        self.translator.text(Message::FileInfoExif).into_owned(),
                        self.translator
                            .text(Message::FileInfoExifFailed)
                            .into_owned(),
                    ))
                }
                crate::file_info_exif::State::Ready(fields) if fields.is_empty() => {
                    body = body.child(row(
                        self.translator.text(Message::FileInfoExif).into_owned(),
                        self.translator.text(Message::FileInfoExifNone).into_owned(),
                    ))
                }
                crate::file_info_exif::State::Ready(fields) => {
                    body = body.child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(self.translator.text(Message::FileInfoExif).into_owned()),
                    );
                    for (field, value) in fields {
                        let message = match field {
                            crate::file_info_exif::Field::Make => Message::FileInfoExifMake,
                            crate::file_info_exif::Field::Model => Message::FileInfoExifModel,
                            crate::file_info_exif::Field::DateTimeOriginal => {
                                Message::FileInfoExifDateTimeOriginal
                            }
                            crate::file_info_exif::Field::LensModel => {
                                Message::FileInfoExifLensModel
                            }
                            crate::file_info_exif::Field::ExposureTime => {
                                Message::FileInfoExifExposureTime
                            }
                            crate::file_info_exif::Field::FNumber => Message::FileInfoExifFNumber,
                            crate::file_info_exif::Field::PhotographicSensitivity => {
                                Message::FileInfoExifPhotographicSensitivity
                            }
                            crate::file_info_exif::Field::FocalLength => {
                                Message::FileInfoExifFocalLength
                            }
                        };
                        body = body.child(row(
                            self.translator.text(message).into_owned(),
                            value.clone(),
                        ));
                    }
                }
            }
        }
        if let Some(token) = item
            .copy_token
            .as_ref()
            .filter(|token| {
                matches!(token.key.as_ref(), Some(crate::entry_identity::EntryKey::Path { .. }))
            })
        {
            let token = token.clone();
            let source_id = source_view.entity_id();
            let name_click = cx.listener({
                let token = token.clone();
                move |this, event: &gpui::ClickEvent, _window, cx| {
                    if !event.standard_click() {
                        return;
                    }
                    this.copy_file_info_text(
                        source_id,
                        token.clone(),
                        crate::InfoCopyField::Name,
                        cx,
                    );
                }
            });
            let path_click = cx.listener({
                let token = token.clone();
                move |this, event: &gpui::ClickEvent, _window, cx| {
                    if !event.standard_click() {
                        return;
                    }
                    this.copy_file_info_text(
                        source_id,
                        token.clone(),
                        crate::InfoCopyField::Location,
                        cx,
                    );
                }
            });
            let key_name = cx.listener({
                let token = token.clone();
                move |this, event: &gpui::KeyDownEvent, _window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                    {
                        cx.stop_propagation();
                        this.copy_file_info_text(
                            source_id,
                            token.clone(),
                            crate::InfoCopyField::Name,
                            cx,
                        );
                    }
                }
            });
            let key_path = cx.listener({
                let token = token.clone();
                move |this, event: &gpui::KeyDownEvent, _window, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                    {
                        cx.stop_propagation();
                        this.copy_file_info_text(
                            source_id,
                            token.clone(),
                            crate::InfoCopyField::Location,
                            cx,
                        );
                    }
                }
            });
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("file-info-copy-name")
                            .track_focus(&self.file_info_focus)
                            .focusable()
                            .h(px(44.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(gpui_filer_ui::theme_palette(cx).border)
                            .rounded(px(4.0))
                            .child(self.translator.text(Message::FileInfoCopyName).into_owned())
                            .on_click(name_click)
                            .on_key_down(key_name),
                    )
                    .child(
                        div()
                            .id("file-info-copy-location")
                            .track_focus(&self.file_info_focus)
                            .focusable()
                            .h(px(44.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(gpui_filer_ui::theme_palette(cx).border)
                            .rounded(px(4.0))
                            .child(
                                self.translator
                                    .text(Message::FileInfoCopyLocation)
                                    .into_owned(),
                            )
                            .on_click(path_click)
                            .on_key_down(key_path),
                    ),
            );
        }
        body.into_any_element()
    }
}
