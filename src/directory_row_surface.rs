use super::*;

impl DirectoryListDemo {
    fn render_entry(
        &self,
        index: usize,
        foreground: Hsla,
        measuring: bool,
        preview_size: Option<u32>,
        scale_factor: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entry = &self.entries[index];
        let display_name = self
            .rename_session
            .as_ref()
            .filter(|session| {
                session.key == entry.key
                    && session.location_generation == self.location_generation
                    && session.presentation_revision == self.presentation_revision
            })
            .map(|session| session.input.read(cx).text().to_owned())
            .or_else(|| {
                self.pending_rename_geometry
                    .as_ref()
                    .filter(|pending| {
                        pending.key == entry.key
                            && pending.location_generation == self.location_generation
                            && pending.presentation_revision == self.presentation_revision
                    })
                    .map(|pending| pending.committed_name.clone())
            })
            .unwrap_or_else(|| entry.name.clone());
        let rename_input = self.rename_session.as_ref().and_then(|session| {
            (session.key == entry.key
                && session.location_generation == self.location_generation
                && session.presentation_revision == self.presentation_revision)
                .then(|| session.input.clone())
        });
        let renaming = rename_input.is_some();
        let shell_icon_image = self.shell_association_icon(
            entry,
            if self.layout == ViewLayout::Rows {
                20.0
            } else {
                self.layout.icon_size(self.icon_scale)
            },
            scale_factor,
            measuring,
            cx,
        );
        let generation = self.location_generation;
        // 並びが変わったら、この index を捕捉した gesture は別の項目を指す。
        // checkbox も item_surface と同じ検証を通す。
        let revision = self.presentation_revision;
        let item_check_boxes_revision = self.item_check_boxes_revision;
        let key = entry.key.clone();
        let checkbox = if !self.item_check_boxes {
            div().size(px(0.0)).into_any_element()
        } else if measuring {
            div()
                .size(px(gpui_filer_ui::CHECKBOX_HIT_SIZE))
                .flex_shrink_0()
                .into_any_element()
        } else {
            // グリッドは箱を自分の内容側へ寄せる。行は左端に隣が無いので中央のまま。
            let checkbox = SelectionCheckbox::new(
                format!("select-{index}-visibility-{item_check_boxes_revision}"),
                self.selection.contains(index),
                cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                    if this.item_check_boxes_revision != item_check_boxes_revision {
                        return;
                    }
                    if this.location_generation != generation {
                        return;
                    }
                    if !this.gesture_still_applies(revision, Some(&key)) {
                        return;
                    }
                    this.touch_item_tap_series = None;
                    this.last_click_was_blank = false;
                    this.focus_handle.focus(window, cx);
                    this.invalidate_pending_restore_for_selection();
                    this.selection
                        .apply(SelectionIntent::Checkbox { index }, this.entries.len());
                    this.emit_info_projection(cx);
                    cx.notify();
                }),
            );
            if self.layout == ViewLayout::Rows {
                checkbox.into_any_element()
            } else {
                checkbox.trailing().into_any_element()
            }
        };
        let checkbox = if self.item_check_boxes && !measuring {
            self.checkbox_surface(index, checkbox)
        } else {
            checkbox
        };
        let folder_tree_group = format!("folder-tree-item-{index}");
        let cut_marked = !measuring
            && entry
                .path
                .as_ref()
                .is_some_and(|path| self.cut_paths.contains(path));
        // icon と名前のどちらへ入っても同じ folder-tree button を出す。個々の surface
        // に group を付けると兄弟同士では hover を共有できず、タイルでは icon だけが
        // button の表示 owner になってしまう。
        let mut base = div()
            .id(("row-drag", index))
            .debug_selector(|| format!("external-drop-row-{index}"))
            .w_full()
            .min_h(px(44.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .group(folder_tree_group.clone())
            .text_size(px(NAME_FONT_SIZE))
            .line_height(px(NAME_LINE_HEIGHT))
            // Row の kind/size/date は選択対象そのものではない。ここからの drag は
            // marquee の開始余白として扱う。一方 name/checkbox/editor は各 child が
            // pointer-down を止めるので既存の click と競合しない。
            .when(!measuring, |row| {
                row.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _window, _cx| {
                        this.capture_rectangle_pointer_down(event);
                    }),
                )
                .on_drag_with_threshold_when(
                    RectangleDrag,
                    px(16.0),
                    |event| event.source == gpui::MouseInputSource::Mouse,
                    {
                        let entity = cx.entity();
                        move |_, _, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.begin_rectangle_drag(window, cx);
                            });
                            cx.new(|_| gpui::Empty)
                        }
                    },
                )
            });
        if !measuring {
            let target = self
                .entries
                .get(index)
                .and_then(|entry| {
                    internal_drop_target_for_entry(
                        entry,
                        self.location_generation,
                        self.presentation_revision,
                    )
                })
                .map(ExternalDropTarget::Folder);
            let entity = cx.entity().downgrade();
            let drop_entity = entity.clone();
            let hover_target = target.clone();
            let hover_session = self.external_drag_session.clone();
            let hover_shell = self.external_drop_shell.clone();
            let hover_view_id = self.external_drop_view_id;
            let hover_source_view_entity_id = cx.entity().entity_id();
            let drag_location_generation = self.location_generation;
            let drag_presentation_revision = self.presentation_revision;
            base = base
                .drag_over::<gpui::ExternalPaths>(move |style, payload, window, cx| {
                    capture_external_drag_session(
                        &hover_session,
                        payload,
                        drag_location_generation,
                        drag_presentation_revision,
                    );
                    if window
                        .file_drop_context()
                        .is_some_and(|context| context.local_drag_session.is_none())
                        && let Some(target) = hover_target.as_ref()
                    {
                        let paths = payload.paths().to_vec();
                        let target = target.clone();
                        let _ = entity.update(cx, |this, cx| {
                            this.prepare_external_drop(&paths, &target, cx);
                        });
                    }
                    let eligible = hover_target.as_ref().is_some_and(|target| {
                        let target_is_current = entity
                            .read_with(cx, |this, _| this.accepts_external_drop_target(target))
                            .unwrap_or(false);
                        if !target_is_current {
                            return false;
                        }
                        let Some(context) = window
                            .file_drop_context()
                            .filter(|context| context.local_drag_session.is_some())
                        else {
                            return external_drag_session_matches(
                                hover_session.borrow().as_ref(),
                                payload,
                                target,
                            );
                        };
                        let (Some(token), Some(paths), Some(shell), Some(view_id)) = (
                            context.local_drag_session.map(|token| token.value()),
                            context.paths.as_ref(),
                            hover_shell.as_ref(),
                            hover_view_id,
                        ) else {
                            return false;
                        };
                        shell
                            .read_with(cx, |shell, cx| {
                                shell
                                    .local_ole_move_candidate_effect(
                                        token,
                                        view_id,
                                        hover_source_view_entity_id,
                                        target,
                                        paths.paths(),
                                        None,
                                        None,
                                        cx,
                                    )
                                    .is_some()
                            })
                            .unwrap_or(false)
                    });
                    if eligible {
                        style
                            .border_1()
                            .border_color(gpui::hsla(0.58, 0.25, 0.62, 0.55))
                    } else {
                        style
                    }
                })
                .on_drop::<gpui::ExternalPaths>(move |_payload, window, cx| {
                    let trace_context = window.file_drop_context();
                    if let Some(context) = trace_context.as_ref()
                        && context.local_drag_session.is_none()
                        && crate::foreign_file_drop_intent(context)
                            == input::ForeignFileDropIntent::Link
                    {
                        context.response.set_effect(gpui::FileDropEffect::None);
                    }
                    ole_diagnostics::record(ole_diagnostics::Event::FileDrop {
                        local: trace_context
                            .as_ref()
                            .is_some_and(|context| context.local_drag_session.is_some()),
                        source_count: trace_context
                            .as_ref()
                            .and_then(|context| context.paths.as_ref())
                            .map_or(0, |paths| paths.paths().len()),
                        target: match target.as_ref() {
                            Some(ExternalDropTarget::Folder(_)) => {
                                ole_diagnostics::TargetClass::Folder
                            }
                            Some(ExternalDropTarget::Background { .. }) => {
                                ole_diagnostics::TargetClass::Background
                            }
                            Some(ExternalDropTarget::NavigationDirectory { .. }) => {
                                ole_diagnostics::TargetClass::Missing
                            }
                            None => ole_diagnostics::TargetClass::Missing,
                        },
                    });
                    // Shell admission が保持中の drag route 経由で view を読み直すため、先に entity lease を解放する。
                    let prepared_inputs = drop_entity
                        .update(cx, |this, _| {
                            this.external_file_drop_paths.borrow_mut().take();
                            let session = take_external_drag_session(&this.external_drag_session);
                            let (Some(target), Some(context), Some(shell), Some(view_id)) = (
                                target.as_ref(),
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
                            if !this.accepts_external_drop_target(target) {
                                return None;
                            }
                            let paths = drop_paths.paths().to_vec();
                            let prepared = prepared_external_drop_from_session(
                                session.as_ref(),
                                &paths,
                                target,
                            );
                            Some((
                                paths,
                                prepared,
                                shell.clone(),
                                view_id,
                                this.current_internal_drag_source_projection(),
                            ))
                        })
                        .ok()
                        .flatten();
                    if let (
                        Some(target),
                        Some(context),
                        Some((paths, prepared, shell, view_id, source_projection_current)),
                    ) = (target.as_ref(), trace_context.as_ref(), prepared_inputs)
                    {
                        let source_view_entity_id = hover_source_view_entity_id;
                        let response = context.response.clone();
                        let target_projection_current = true;
                        let admitted_effect = shell
                            .update(cx, |shell, cx| {
                                if let Some(local_token) = context.local_drag_session {
                                    shell
                                        .admit_local_ole_move(
                                            local_token.value(),
                                            view_id,
                                            source_view_entity_id,
                                            target.clone(),
                                            paths,
                                            target_projection_current,
                                            source_projection_current,
                                            cx,
                                        )
                                        .map(|effect| match effect {
                                            shell::LocalOleDropEffect::Copy => {
                                                gpui::FileDropEffect::Copy
                                            }
                                            shell::LocalOleDropEffect::Move => {
                                                gpui::FileDropEffect::Move
                                            }
                                        })
                                } else {
                                    match crate::foreign_file_drop_intent(&context) {
                                        input::ForeignFileDropIntent::Link
                                            if context.link_allowed =>
                                        {
                                            // GPUIの未設定responseはWindowsでlegacy Copyに解決される。
                                            // Link admissionが失敗したDropは明示Noneのまま終端する。
                                            context.response.set_effect(gpui::FileDropEffect::None);
                                            shell
                                                .admit_external_symbolic_link(
                                                    view_id,
                                                    source_view_entity_id,
                                                    target.clone(),
                                                    paths,
                                                    cx,
                                                )
                                                .then_some(gpui::FileDropEffect::Link)
                                        }
                                        input::ForeignFileDropIntent::Link => {
                                            context.response.set_effect(gpui::FileDropEffect::None);
                                            None
                                        }
                                        input::ForeignFileDropIntent::Move => {
                                            let Some(feedback) = context
                                                .transfer_lease
                                                .as_ref()
                                                .and_then(gpui::FileDropTransferLease::start)
                                            else {
                                                return None;
                                            };
                                            shell
                                                .admit_external_copy(
                                                    view_id,
                                                    source_view_entity_id,
                                                    target.clone(),
                                                    paths,
                                                    false,
                                                    Some(feedback),
                                                    cx,
                                                )
                                                .then_some(gpui::FileDropEffect::Copy)
                                        }
                                        input::ForeignFileDropIntent::Default => {
                                            if let Some(prepared) =
                                                prepared.filter(|prepared| prepared.same_volume)
                                                && let Some(feedback) = context
                                                    .transfer_lease
                                                    .as_ref()
                                                    .and_then(gpui::FileDropTransferLease::start)
                                            {
                                                shell
                                                    .admit_external_prepared_move(
                                                        view_id,
                                                        source_view_entity_id,
                                                        target.clone(),
                                                        paths,
                                                        prepared,
                                                        feedback,
                                                        cx,
                                                    )
                                                    .then_some(gpui::FileDropEffect::Copy)
                                            } else {
                                                shell
                                                    .admit_external_copy(
                                                        view_id,
                                                        source_view_entity_id,
                                                        target.clone(),
                                                        paths,
                                                        false,
                                                        None,
                                                        cx,
                                                    )
                                                    .then_some(gpui::FileDropEffect::Copy)
                                            }
                                        }
                                        input::ForeignFileDropIntent::Copy => shell
                                            .admit_external_copy(
                                                view_id,
                                                source_view_entity_id,
                                                target.clone(),
                                                paths,
                                                false,
                                                None,
                                                cx,
                                            )
                                            .then_some(gpui::FileDropEffect::Copy),
                                    }
                                }
                            })
                            .ok()
                            .flatten();
                        if let Some(effect) = admitted_effect {
                            response.set_effect(effect);
                        }
                    }
                    // Invalid row drops are consumed here, including Rows metadata cells.
                    cx.stop_propagation();
                });
        }
        if self.layout == ViewLayout::Rows {
            let details = self
                .row_details
                .get(&entry.key)
                .expect("detail cache is rebuilt with the entry projection");
            let mut name_text = div().min_w_0().flex_1().w_full().overflow_hidden().child(
                if let Some(input) = rename_input.clone() {
                    let pointer_input = input.clone();
                    div()
                        .id(("rename-input", index))
                        .w_full()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |_this, event: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                pointer_input.update(cx, |input, cx| {
                                    input.begin_pointer_selection(event.position, window, cx)
                                });
                            }),
                        )
                        .on_mouse_move({
                            let pointer_input = input.clone();
                            cx.listener(move |_, event: &gpui::MouseMoveEvent, _, cx| {
                                if event.pressed_button == Some(gpui::MouseButton::Left)
                                    && pointer_input.update(cx, |input, cx| {
                                        input.extend_pointer_selection(event.position, cx)
                                    })
                                {
                                    cx.stop_propagation();
                                }
                            })
                        })
                        .on_mouse_up(
                            gpui::MouseButton::Left,
                            cx.listener({
                                let pointer_input = input.clone();
                                move |_, _, _, cx| {
                                    pointer_input
                                        .update(cx, |input, _| input.end_pointer_selection());
                                    cx.stop_propagation();
                                }
                            }),
                        )
                        .on_mouse_up_out(gpui::MouseButton::Left, {
                            let pointer_input = input.clone();
                            cx.listener(move |_, _, _, cx| {
                                pointer_input.update(cx, |input, _| input.end_pointer_selection());
                                cx.stop_propagation();
                            })
                        })
                        .child(input)
                        .into_any_element()
                } else {
                    div().child(display_name.clone()).into_any_element()
                },
            );
            if cut_marked {
                name_text = name_text.opacity(0.5);
            }
            let mut name = div()
                .relative()
                .min_w(px(rows_layout::project_columns(
                    self.layout_width,
                    self.effective_column_prefs(),
                )
                .name_width))
                .min_h(px(44.0))
                // 同じ仮想行にある cell は高さを共有するが、選択背景を持つ name
                // surface まで stretch すると、折返し内容より大きな青い面になる。
                // 横幅だけは flex で受け、縦方向は editor の内容高を保つ。
                .self_start()
                .flex_1()
                .px(px(CELL_PADDING_X))
                .py(px(CELL_PADDING_Y))
                .flex()
                .items_center()
                .gap(px(CELL_GAP))
                .overflow_hidden()
                // タイル・アイコンの名前ラベルと同じ角丸にする。
                .rounded(px(RADIUS_SM))
                .child(if measuring {
                    div().size(px(20.0)).flex_shrink_0().into_any_element()
                } else if is_shortcut_file(entry) {
                    let icon = shell_icon_image
                        .clone()
                        .map(|image| {
                            gpui::img(image)
                                .size(px(20.0))
                                .object_fit(gpui::ObjectFit::Contain)
                                .into_any_element()
                        })
                        .unwrap_or_else(|| {
                            Icon::new(self.entry_icon(entry))
                                .size(px(20.0))
                                .color(foreground)
                                .into_any_element()
                        });
                    div()
                        .relative()
                        .size(px(20.0))
                        .flex_shrink_0()
                        .child(icon)
                        .child(shortcut_badge(20.0, 0.0, self.shortcut_is_broken(entry)))
                        .into_any_element()
                } else if let Some(image) = shell_icon_image.clone() {
                    gpui::img(image)
                        .size(px(20.0))
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element()
                } else {
                    Icon::new(self.entry_icon(entry))
                        .size(px(20.0))
                        .color(foreground)
                        .into_any_element()
                })
                .child(name_text);
            if !measuring
                && matches!(entry.kind, EntryKind::Folder)
                && !entry.is_link()
                && !renaming
            {
                if let Some(path) = entry.path.clone() {
                    name = name.child(self.render_folder_tree_button(
                        index,
                        path,
                        folder_tree_group.clone(),
                        cx,
                    ));
                }
            }
            let name = self.item_surface(index, "row-name", name, measuring, cx);
            let column_prefs = self.effective_column_prefs();
            let mut kind_cell =
                rows_layout::detail_cell(details.kind.clone(), column_prefs.kind_width);
            let mut size_cell =
                rows_layout::detail_cell(details.size.clone(), column_prefs.size_width)
                    .text_right();
            let mut modified_cell =
                rows_layout::detail_cell(details.modified.clone(), column_prefs.modified_width);
            if cut_marked {
                kind_cell = kind_cell.opacity(0.5);
                size_cell = size_cell.opacity(0.5);
                modified_cell = modified_cell.opacity(0.5);
            }
            let columns = self.row_columns_for_width(self.layout_width, column_prefs);
            let mut ordered_cells = Vec::with_capacity(7);
            let mut name = Some(name.into_any_element());
            let mut kind_cell = Some(kind_cell.into_any_element());
            let mut size_cell = Some(size_cell.into_any_element());
            let mut modified_cell = Some(modified_cell.into_any_element());
            for column in columns.order {
                let visible = match column {
                    Column::Name => true,
                    Column::Kind => columns.kind,
                    Column::Size => columns.size,
                    Column::Modified => columns.modified,
                };
                if !visible {
                    continue;
                }
                if !ordered_cells.is_empty() {
                    ordered_cells.push(rows_layout::column_divider().into_any_element());
                }
                ordered_cells.push(match column {
                    Column::Name => name.take().unwrap(),
                    Column::Kind => kind_cell.take().unwrap(),
                    Column::Size => size_cell.take().unwrap(),
                    Column::Modified => modified_cell.take().unwrap(),
                });
            }
            let row = base
                .px(px(ROW_PADDING_X))
                .child(checkbox)
                .children(ordered_cells);
            return row.into_any_element();
        }

        let icons = self.layout == ViewLayout::Icons;
        let icon_size = self.layout.icon_size(self.icon_scale);
        let is_ico = is_ico_file(entry);
        let preview_reason = if measuring || self.layout == ViewLayout::Rows || is_ico {
            None
        } else {
            entry.path.as_ref().and_then(|path| {
                let epoch = thumbnails::epoch_of(path);
                let folder = matches!(entry.kind, EntryKind::Folder);
                match self.previews.get(path) {
                    Some(PreviewState::Unavailable {
                        epoch: result_epoch,
                        size: result_size,
                        folder: result_folder,
                        reason,
                    }) if *result_epoch == epoch
                        && Some(*result_size) == preview_size
                        && *result_folder == folder =>
                    {
                        Some(*reason)
                    }
                    _ => None,
                }
            })
        };
        let preview_reason_label = preview_reason.map(|reason| {
            self.translator
                .text(match reason {
                    thumbnails::PreviewFailureReason::Unsupported => {
                        Message::ThumbnailGenerationUnavailable
                    }
                    thumbnails::PreviewFailureReason::ProcessingFailed => {
                        Message::ThumbnailProcessingFailed
                    }
                    thumbnails::PreviewFailureReason::FolderUnavailable => {
                        Message::ThumbnailFolderUnavailable
                    }
                    thumbnails::PreviewFailureReason::FolderNoPreview => {
                        Message::ThumbnailFolderNoPreview
                    }
                })
                .into_owned()
        });
        let shell_visual = shell_icon_image.as_ref().map(|image| {
            gpui::img(image.clone())
                .size(px(icon_size))
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(px(RADIUS_SM))
                .into_any_element()
        });
        let preview_visual = entry
            .path
            .as_ref()
            .and_then(|path| match self.previews.get(path) {
                // 世代が進んでいても、新しい画像が届くまでは前のものを出す。
                // 再要求は `prepare_previews` が済ませている。
                Some(PreviewState::Ready { images, .. }) if !images.is_empty() => Some(images),
                _ => None,
            })
            .map(|images| {
                if matches!(entry.kind, EntryKind::Folder) {
                    gpui_filer_ui::FolderPreview::new(images.clone(), icon_size).into_any_element()
                } else {
                    // 画像は角丸を持たないので、コンテナの角丸で切る。
                    // 他のアイコンと形が揃う。
                    gpui::img(images[0].clone())
                        .size(px(icon_size))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(RADIUS_SM))
                        .into_any_element()
                }
            });
        let visual = if measuring {
            div().size(px(icon_size)).into_any_element()
        } else {
            let preferred_visual = if is_ico {
                shell_visual
            } else {
                preview_visual.or(shell_visual)
            };
            preferred_visual.unwrap_or_else(|| {
                Icon::new(self.entry_icon(entry))
                    .size(px(icon_size))
                    .color(foreground)
                    .into_any_element()
            })
        };
        let visual_surface = div()
            .relative()
            .size(px(icon_size + 16.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(RADIUS_MD))
            // サムネイルの四隅をコンテナの角丸で切る。
            .overflow_hidden()
            .child(visual)
            .when(!measuring && is_shortcut_file(entry), |surface| {
                surface.child(shortcut_badge(
                    icon_size,
                    8.0,
                    self.shortcut_is_broken(entry),
                ))
            })
            .when_some(preview_reason_label, |surface, label| {
                surface.child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .w_full()
                        .px(px(4.0))
                        .py(px(1.0))
                        .bg(gpui_filer_ui::theme_palette(cx).surface_base)
                        .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                        .text_size(px(10.0))
                        .line_height(px(12.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(label),
                )
            });
        let icon = self.item_surface(index, "visual", visual_surface, measuring, cx);
        // visual の item surface は通常 click を先に扱うため、action button はその外側へ
        // 重ねる。見た目の anchor は icon surface の右下のまま、menu button だけが先に受ける。
        let mut icon_with_anchor = div()
            .relative()
            .size(px(icon_size + 16.0))
            .flex_shrink_0()
            .child(icon);
        if !measuring
            && self.layout != ViewLayout::Rows
            && matches!(entry.kind, EntryKind::Folder)
            && !entry.is_link()
            && !renaming
        {
            if let Some(path) = entry.path.clone() {
                icon_with_anchor = icon_with_anchor.child(self.render_folder_tree_button(
                    index,
                    path,
                    folder_tree_group.clone(),
                    cx,
                ));
            }
        }
        // 名前を列幅いっぱいに伸ばさない。短い名前の右側は余白として操作できる。
        // 文字のぎりぎりではなく、上下余白と最小44pxの高さでタッチ領域を確保する。
        let mut label_content = div()
            .relative()
            .min_w_0()
            .max_w_full()
            .when(renaming && icons, |label| label.w_full())
            .when(renaming && !icons, |label| label.flex_1())
            .min_h(px(44.0))
            .px(px(6.0))
            .py(px(4.0))
            .flex()
            .items_center()
            .rounded(px(RADIUS_SM))
            .overflow_hidden()
            .when(icons, |label| label.text_center())
            .child(div().min_w_0().w_full().overflow_hidden().child(
                if let Some(input) = rename_input {
                    let pointer_input = input.clone();
                    div()
                        .id(("rename-input", index))
                        .w_full()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |_this, event: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                pointer_input.update(cx, |input, cx| {
                                    input.begin_pointer_selection(event.position, window, cx)
                                });
                            }),
                        )
                        .on_mouse_move({
                            let pointer_input = input.clone();
                            cx.listener(move |_, event: &gpui::MouseMoveEvent, _, cx| {
                                if event.pressed_button == Some(gpui::MouseButton::Left)
                                    && pointer_input.update(cx, |input, cx| {
                                        input.extend_pointer_selection(event.position, cx)
                                    })
                                {
                                    cx.stop_propagation();
                                }
                            })
                        })
                        .on_mouse_up(
                            gpui::MouseButton::Left,
                            cx.listener({
                                let pointer_input = input.clone();
                                move |_, _, _, cx| {
                                    pointer_input
                                        .update(cx, |input, _| input.end_pointer_selection());
                                    cx.stop_propagation();
                                }
                            }),
                        )
                        .on_mouse_up_out(gpui::MouseButton::Left, {
                            let pointer_input = input.clone();
                            cx.listener(move |_, _, _, cx| {
                                pointer_input.update(cx, |input, _| input.end_pointer_selection());
                                cx.stop_propagation();
                            })
                        })
                        .child(input)
                        .into_any_element()
                } else {
                    div().child(display_name).into_any_element()
                },
            ));
        if cut_marked {
            // Cut の状態は名前だけ薄くし、FolderPreview / Fluent icon の表面は
            // 見えるままにする。親カードへ opacity を掛けると、画像の内容まで透ける。
            label_content = label_content.opacity(0.5);
        }
        if !measuring
            && self.layout == ViewLayout::Rows
            && matches!(entry.kind, EntryKind::Folder)
            && !entry.is_link()
            && !renaming
        {
            if let Some(path) = entry.path.clone() {
                label_content = label_content.child(self.render_folder_tree_button(
                    index,
                    path,
                    folder_tree_group.clone(),
                    cx,
                ));
            }
        }
        let label = self.item_surface(index, "name", label_content, measuring, cx);
        if icons {
            // 箱を icon の左隣へ寄せ、反対側に同幅の空きを置く。
            // こうすると icon と名前の中心はセル中央のまま揃い、箱だけが自分の内容へ付く。
            let icon_row = div()
                .flex()
                .items_center()
                .gap(px(if self.item_check_boxes {
                    CHECKBOX_GAP
                } else {
                    0.0
                }))
                .child(checkbox)
                .child(icon_with_anchor)
                .when(self.item_check_boxes, |row| {
                    row.child(
                        div()
                            .w(px(gpui_filer_ui::CHECKBOX_HIT_SIZE))
                            .flex_shrink_0(),
                    )
                });
            base.flex_col()
                .gap(px(8.0))
                .child(icon_row)
                .child(label)
                .into_any_element()
        } else {
            // カード全体にはhit領域を置かず、checkbox・icon・名前だけが選択を受ける。
            base.gap(px(if self.item_check_boxes { 8.0 } else { 0.0 }))
                .child(checkbox)
                .child(icon_with_anchor)
                .child(label)
                .into_any_element()
        }
    }

    pub(super) fn render_group(
        &self,
        row: usize,
        measuring: bool,
        id_scope: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.render_group_with_preview_size(row, measuring, id_scope, None, 1.0, cx)
    }

    pub(super) fn render_group_with_preview_size(
        &self,
        row: usize,
        measuring: bool,
        id_scope: &'static str,
        preview_size: Option<u32>,
        scale_factor: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        if let Some(ProjectedRow::CategoryStrip { groups, .. }) = self.projected_rows.get(row) {
            let mut strip = div()
                .id(format!("{id_scope}-category-strip-{row}"))
                .w_full()
                .flex()
                .items_start()
                .gap(px(16.0))
                .px(px(12.0))
                .pb(px(16.0));
            for (column, category) in groups.iter().enumerate() {
                let index = category.index;
                let state = self.selection.range_selection_state(index, index + 1);
                let selected = state == RangeSelectionState::All;
                let checkbox = SelectionCheckbox::new(
                    format!("{id_scope}-category-select-{row}-{column}"),
                    selected,
                    {
                        let key = category.key.clone();
                        cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                            this.toggle_section(&key, cx);
                            cx.stop_propagation();
                        })
                    },
                )
                .indeterminate(state == RangeSelectionState::Some);
                let focused = matches!(self.selection.focus_target(), Some(FocusTarget::Section(focused)) if focused == &category.key);
                let heading = div()
                    .id(format!("{id_scope}-category-heading-{row}-{column}"))
                    .w_full()
                    .h(px(gpui_filer_ui::CHECKBOX_HIT_SIZE))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .when(focused, |heading| {
                        heading
                            .border_1()
                            .border_color(gpui::hsla(0.58, 0.25, 0.62, 0.8))
                    })
                    .text_size(px(14.0))
                    .gap(px(6.0))
                    .child(checkbox)
                    .child(category.label.clone());
                let heading = if measuring {
                    heading
                } else {
                    heading
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.invalidate_pending_restore_for_selection();
                            this.selection.apply(
                                SelectionIntent::SetRange {
                                    start: index,
                                    end: index + 1,
                                    selected: true,
                                },
                                this.entries.len(),
                            );
                            this.emit_info_projection(cx);
                            cx.notify();
                        }))
                        .on_drop::<gpui::ExternalPaths>(cx.listener(|this, _, window, cx| {
                            this.external_file_drop_paths.borrow_mut().take();
                            if let Some(context) = window.file_drop_context() {
                                crate::reject_foreign_link_by_default(&context);
                            }
                            cx.stop_propagation();
                        }))
                };
                strip = strip.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(heading)
                        .child(self.render_entry(
                            index,
                            gpui_filer_ui::theme_palette(cx).text.into(),
                            measuring,
                            preview_size,
                            scale_factor,
                            cx,
                        )),
                );
            }
            for _ in groups.len()..self.columns {
                strip = strip.child(div().flex_1().min_w_0());
            }
            return strip;
        }
        if let Some(ProjectedRow::Heading {
            key,
            label,
            start,
            end,
        }) = self.projected_rows.get(row)
        {
            let key = key.clone();
            let start = *start;
            let end = *end;
            let state = self.selection.range_selection_state(start, end);
            let checkbox = SelectionCheckbox::new(
                format!("{id_scope}-section-select-{row}"),
                state == RangeSelectionState::All,
                {
                    let key = key.clone();
                    cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                        this.toggle_section(&key, cx);
                        cx.stop_propagation();
                    })
                },
            )
            .indeterminate(state == RangeSelectionState::Some);
            let focused = matches!(self.selection.focus_target(), Some(FocusTarget::Section(focused)) if focused == &key);
            let heading = div()
                .id(format!("{id_scope}-section-heading-{row}"))
                .w_full()
                // hit 領域が 44px の checkbox を header の高さより大きくすると、flex の
                // baseline から上にはみ出して見える。見出し側も同じ高さにし、文字と箱を
                // 一つの中心線へ揃える。
                .h(px(gpui_filer_ui::CHECKBOX_HIT_SIZE))
                .px(px(12.0))
                .flex()
                .items_center()
                .when(focused, |heading| {
                    heading
                        .border_1()
                        .border_color(gpui::hsla(0.58, 0.25, 0.62, 0.8))
                })
                .text_size(px(14.0))
                .gap(px(8.0))
                .child(checkbox)
                .child(label.clone());
            return if measuring {
                heading
            } else {
                heading
                    .on_click(
                        cx.listener(move |this, _event: &gpui::ClickEvent, _window, cx| {
                            this.invalidate_pending_restore_for_selection();
                            this.selection.apply(
                                SelectionIntent::SetRange {
                                    start,
                                    end,
                                    selected: true,
                                },
                                this.entries.len(),
                            );
                            this.emit_info_projection(cx);
                            cx.notify();
                        }),
                    )
                    .on_drop::<gpui::ExternalPaths>(cx.listener(|this, _, window, cx| {
                        this.external_file_drop_paths.borrow_mut().take();
                        if let Some(context) = window.file_drop_context() {
                            crate::reject_foreign_link_by_default(&context);
                        }
                        cx.stop_propagation();
                    }))
            };
        }
        let mut group = div().w_full().flex().flex_shrink_0();
        // v5 と同じくタイルも左上から詰める。中央揃えは行内の空きが強調され、
        // Explorer 型の一覧としては項目が浮いて見える。
        group = group.items_start();
        if self.layout != ViewLayout::Rows {
            group = group.px(px(12.0)).pb(px(16.0)).gap(px(16.0));
        }
        let entries = self.row_entries(row);
        let entry_count = entries.len();
        for index in entries {
            group = group.child(div().flex_1().min_w_0().child(self.render_entry(
                index,
                gpui_filer_ui::theme_palette(cx).text.into(),
                measuring,
                preview_size,
                scale_factor,
                cx,
            )));
        }
        // 最終行だけ実項目が少なくても、空セルを同じ flex factor で残す。これが無いと
        // 最後のタイルだけ横幅が伸び、左詰めのグリッドとして読めなくなる。
        if self.layout != ViewLayout::Rows {
            for _ in entry_count..self.columns {
                group = group.child(div().flex_1().min_w_0());
            }
        }
        group.id(format!("{id_scope}-entry-group-{row}"))
    }
}
