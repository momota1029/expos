//! ScrollHandleとホバー／ドラッグのGPUI adapter。ファイル操作には依存しない。
use super::*;

impl DirectoryListDemo {
    fn render_scrollbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let bounds = self.scroll_handle.bounds();
        let height = bounds.size.height.as_f32();
        let content = self.heights.total_height() + 44.0;
        if height <= 0.0
            || content <= height
            || (!self.list_hovered && self.scrollbar_drag.is_none())
        {
            return div().into_any_element();
        }
        let thumb_height = (height * height / content).max(40.0).min(height);
        let travel = height - thumb_height;
        let max_scroll = content - height;
        let top = (-self.scroll_handle.offset().y.as_f32() / max_scroll).clamp(0.0, 1.0) * travel;
        let track_bounds = Rc::new(RefCell::new(None));
        if let Some(geometry) = self.item_menu_geometry.borrow_mut().as_mut() {
            geometry.occluding_surfaces.push(track_bounds.clone());
        }
        BoundsCapture {
            bounds: track_bounds,
            child: div()
                .id("scrollbar-track")
                .absolute()
                .right_0()
                .top_0()
                .h_full()
                .w(px(20.0))
                // overlay scrollbar は track を塗らず、thumb だけを content の上へ出す。
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.last_click_was_blank = false;
                        if event.standard_click() && travel > 0.0 {
                            this.scroll_revision = this.scroll_revision.wrapping_add(1);
                            let position = (event.position().y.as_f32()
                                - bounds.origin.y.as_f32()
                                - thumb_height * 0.5)
                                .clamp(0.0, travel);
                            this.scroll_handle.set_offset(gpui::point(
                                px(0.0),
                                -px(position / travel * max_scroll),
                            ));
                            cx.notify();
                        }
                    }),
                )
                .child(
                    div()
                        .id("scrollbar-thumb")
                        .absolute()
                        .left(px(5.0))
                        .top(px(top))
                        .w(px(10.0))
                        .h(px(thumb_height))
                        .rounded(px(5.0))
                        .bg(SCROLLBAR_THUMB)
                        .hover(|style| style.bg(SCROLLBAR_THUMB_HOVER))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.touch_item_tap_series = None;
                            this.last_click_was_blank = false;
                            cx.stop_propagation();
                        }))
                        .on_drag(ScrollbarDrag, {
                            let entity = cx.entity();
                            move |_, offset, _window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.scrollbar_drag = Some(offset.y.as_f32());
                                    this.touch_item_tap_series = None;
                                    this.last_click_was_blank = false;
                                    this.scroll_revision = this.scroll_revision.wrapping_add(1);
                                    cx.notify();
                                });
                                cx.new(|_| gpui::Empty)
                            }
                        }),
                )
                .into_any_element(),
        }
        .into_any_element()
    }

    pub(super) fn render_scroll_area(
        &self,
        rows: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let external_drop_shell = self.external_drop_shell.clone();
        div()
            .id("list-frame")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            // マウスを動かしたら hover を復活させる。タッチやキーのあとでも、
            // ポインタを実際に動かせばその位置の hover が意味を持つ。
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, window, cx| {
                    if this.interaction.last_device() != InputDevice::Mouse {
                        this.interaction.set_last_device(InputDevice::Mouse);
                        cx.notify();
                    }

                    // WindowsのFileDrop PendingはFileDropContextを付けてmouse moveとして配送する。
                    // 一覧frameに置くことで行上と空白上のどちらのmoveも同じScrollHandleへ届く。
                    let owns_file_drag_handoff = window
                        .file_drop_context()
                        .and_then(|context| context.local_drag_session)
                        .is_some_and(|token| {
                            external_drop_shell.as_ref().is_some_and(|shell| {
                                shell
                                    .read_with(cx, |shell, _| {
                                        shell.owns_active_file_drag_handoff(token.value())
                                    })
                                    .unwrap_or(false)
                            })
                        });
                    if owns_file_drag_handoff && this.update_file_drag_edge_scroll(event.position) {
                        cx.notify();
                    }
                }),
            )
            .on_hover(cx.listener(|this, hovered, _window, cx| {
                this.list_hovered = *hovered;
                cx.notify();
            }))
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<ScrollbarDrag>, _window, cx| {
                    let Some(delta) = this.scrollbar_drag else {
                        return;
                    };
                    this.scroll_revision = this.scroll_revision.wrapping_add(1);
                    let bounds = this.scroll_handle.bounds();
                    let height = bounds.size.height.as_f32();
                    let content = this.heights.total_height() + 44.0;
                    if content <= height || height <= 0.0 {
                        return;
                    }
                    let travel = height - (height * height / content).max(40.0).min(height);
                    if travel <= 0.0 {
                        return;
                    }
                    let top = (event.event.position.y.as_f32() - bounds.origin.y.as_f32() - delta)
                        .clamp(0.0, travel);
                    this.scroll_handle
                        .set_offset(gpui::point(px(0.0), -px(top / travel * (content - height))));
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<RectangleDrag>, _window, cx| {
                    if this.rectangle_drag.is_none() {
                        return;
                    }
                    this.update_rectangle_drag_pointer(
                        event.event.position.x.as_f32(),
                        event.event.position.y.as_f32(),
                    );
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .on_drop(cx.listener(|this, _: &ScrollbarDrag, _window, cx| {
                this.scrollbar_drag = None;
                cx.notify();
            }))
            .on_drop(cx.listener(|this, _: &RectangleDrag, _window, cx| {
                this.finish_rectangle_drag(cx);
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    if this.scrollbar_drag.take().is_some() | this.finish_rectangle_drag(cx) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    if this.scrollbar_drag.take().is_some() | this.finish_rectangle_drag(cx) {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .id("directory-list")
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.scroll_handle)
                    .on_drag_move(cx.listener(
                        |this, event: &gpui::DragMoveEvent<InternalDragPayload>, _window, cx| {
                            if !matches!(
                                event.source,
                                gpui::MouseInputSource::Touch | gpui::MouseInputSource::Mouse
                            ) {
                                return;
                            }
                            let accepts_file_drag = {
                                let drag = event.drag(cx);
                                if drag.source != cx.entity().entity_id() {
                                    return;
                                }
                                drag.snapshot.borrow().as_ref().is_some_and(|snapshot| {
                                    this.accepts_internal_drag_source(snapshot)
                                })
                            };
                            if !accepts_file_drag {
                                return;
                            }
                            if this.update_file_drag_edge_scroll(event.event.position) {
                                cx.notify();
                            }
                        },
                    ))
                    .on_drag_move(cx.listener(
                        |this, event: &gpui::DragMoveEvent<gpui::ExternalPaths>, window, cx| {
                            if !external_file_drag_context_matches(
                                event.drag(cx),
                                window.file_drop_context(),
                            ) {
                                return;
                            }
                            if this.update_file_drag_edge_scroll(event.event.position) {
                                cx.notify();
                            }
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, _: &gpui::ScrollWheelEvent, _window, _cx| {
                            this.note_user_scroll();
                        },
                    ))
                    .on_mouse_down(
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
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        if !event.standard_click() {
                            return;
                        }
                        this.touch_item_tap_series = None;
                        this.focus_handle.focus(window, cx);
                        if event.click_count() == 2 && this.last_click_was_blank {
                            this.selection
                                .apply(SelectionIntent::Clear, this.entries.len());
                            cx.notify();
                        }
                        this.last_click_was_blank = true;
                    }))
                    // 余白の右クリックは、表の binding でmenu を出す。
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.apply_background_press(event, window, cx);
                        }),
                    )
                    // 親ScrollHandleの既定処理より先に、子でページ単位入力を補正する。
                    .child(rows.on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            // 通常 wheel / touch pan の offset 更新は GPUI に任せるが、
                            // 保留中の refresh anchor を失効させる世代だけは進める。
                            this.scroll_revision = this.scroll_revision.wrapping_add(1);
                            // WindowsのWHEEL_PAGESCROLL(u32::MAX)をGPUIが行数のまま渡す場合のadapter。
                            if let gpui::ScrollDelta::Lines(delta) = event.delta
                                && delta.y.abs() >= 1_000_000.0
                            {
                                let viewport = this.scroll_handle.bounds().size.height.as_f32();
                                let offset = -this.scroll_handle.offset().y.as_f32()
                                    - delta.y.signum() * (viewport - 44.0).max(44.0);
                                this.scroll_handle.set_offset(gpui::point(
                                    px(0.0),
                                    -px(clamp_scroll_offset(
                                        offset,
                                        this.heights.total_height() + 44.0,
                                        viewport,
                                    )),
                                ));
                                cx.stop_propagation();
                                // 通常の wheel / touch pan は overflow container が ScrollHandle を
                                // 更新して invalidation する。ここで全 view を毎イベント notify すると、
                                // edge で動かない wheel まで layout / preview を作り直してしまう。
                                // 自前で page offset を変えた sentinel のときだけ必要になる。
                                cx.notify();
                            }
                        },
                    ))),
            )
            .child(self.render_scrollbar(cx))
            .when_some(self.sticky_heading_row(), |frame, row| {
                // heading は通常の virtual row と同じ selection UI を使う。overlay は
                // position だけを変え、selection authority を複製しない。
                let bounds = Rc::new(RefCell::new(None));
                if let Some(geometry) = self.item_menu_geometry.borrow_mut().as_mut() {
                    geometry.occluding_surfaces.push(bounds.clone());
                }
                frame.child(
                    BoundsCapture {
                        bounds,
                        child: div()
                            .id(("sticky-section-heading", row))
                            .absolute()
                            .top_0()
                            .left_0()
                            .right(px(20.0))
                            .bg(SURFACE_RAISED)
                            .border_b_1()
                            .border_color(BORDER)
                            .child(self.render_group(row, false, "sticky", cx))
                            .into_any_element(),
                    }
                    .into_any_element(),
                )
            })
            .when_some(
                self.rectangle_visual_bounds(),
                |frame, (left, top, width, height)| {
                    frame.child(
                        div()
                            .absolute()
                            .left(px(left))
                            .top(px(top))
                            .w(px(width))
                            .h(px(height))
                            .border_1()
                            .border_color(ACCENT)
                            .bg(gpui::rgba(0x2b78c433)),
                    )
                },
            )
            .into_any_element()
    }

    fn update_file_drag_edge_scroll(&mut self, position: gpui::Point<Pixels>) -> bool {
        let bounds = self.scroll_handle.bounds();
        let viewport = bounds.size.height.as_f32();
        if viewport <= 0.0 || !bounds.contains(&position) {
            return false;
        }

        let local_y = position.y.as_f32() - bounds.origin.y.as_f32();
        let delta = file_drag_edge_scroll_delta(local_y, viewport);
        if delta == 0.0 {
            return false;
        }

        let content = self.heights.total_height() + 44.0;
        let current = (-self.scroll_handle.offset().y.as_f32()).max(0.0);
        let next = clamp_scroll_offset(current + delta, content, viewport);
        if (next - current).abs() <= 0.01 {
            return false;
        }

        // v5 と同様、edge band の境界では停止し、端へ近づくほど速くする。
        // 静止中は進めず、ドラッグ更新ごとに最大 36px だけ動かす。
        self.note_user_scroll();
        self.scroll_handle
            .set_offset(gpui::point(px(0.0), -px(next)));
        true
    }
}

const FILE_DRAG_EDGE_BAND: f32 = 48.0;
const MAX_FILE_DRAG_EDGE_SCROLL: f32 = 36.0;

fn external_file_drag_context_matches(
    payload: &gpui::ExternalPaths,
    context: Option<&gpui::FileDropContext>,
) -> bool {
    !payload.paths().is_empty()
        && context.is_some_and(|context| context.local_drag_session.is_none())
}

fn file_drag_edge_scroll_delta(local_y: f32, viewport: f32) -> f32 {
    if viewport <= 0.0 {
        return 0.0;
    }

    let edge_band = FILE_DRAG_EDGE_BAND.min(viewport / 2.0);
    if local_y < edge_band {
        -MAX_FILE_DRAG_EDGE_SCROLL * ((edge_band - local_y) / edge_band).clamp(0.0, 1.0)
    } else if local_y > viewport - edge_band {
        MAX_FILE_DRAG_EDGE_SCROLL * ((local_y - (viewport - edge_band)) / edge_band).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{external_file_drag_context_matches, file_drag_edge_scroll_delta};

    #[test]
    fn file_drag_edge_scroll_uses_v5_band_and_per_move_limit() {
        assert_eq!(file_drag_edge_scroll_delta(48.0, 800.0), 0.0);
        assert!(file_drag_edge_scroll_delta(8.0, 800.0) < 0.0);
        assert!(file_drag_edge_scroll_delta(792.0, 800.0) > 0.0);
        assert_eq!(file_drag_edge_scroll_delta(0.0, 800.0), -36.0);
        assert_eq!(file_drag_edge_scroll_delta(800.0, 800.0), 36.0);
    }

    #[test]
    fn file_drag_edge_scroll_keeps_small_viewport_bands_separate() {
        assert!(file_drag_edge_scroll_delta(5.0, 20.0) < 0.0);
        assert_eq!(file_drag_edge_scroll_delta(10.0, 20.0), 0.0);
        assert!(file_drag_edge_scroll_delta(15.0, 20.0) > 0.0);
    }

    #[test]
    fn external_drag_scroll_requires_unambiguous_nonempty_external_paths() {
        let payload = gpui::ExternalPaths(
            [std::path::PathBuf::from("C:/external.txt")]
                .into_iter()
                .collect(),
        );
        let context = gpui::FileDropContext {
            paths: Some(payload.clone()),
            ..Default::default()
        };
        assert!(external_file_drag_context_matches(&payload, Some(&context)));
        let pending_context = gpui::FileDropContext {
            paths: None,
            ..Default::default()
        };
        assert!(external_file_drag_context_matches(
            &payload,
            Some(&pending_context)
        ));
        assert!(!external_file_drag_context_matches(&payload, None));

        let local_context = gpui::FileDropContext {
            local_drag_session: Some(gpui::LocalDragSessionToken::new(7)),
            paths: Some(payload.clone()),
            ..Default::default()
        };
        assert!(!external_file_drag_context_matches(
            &payload,
            Some(&local_context)
        ));
        assert!(!external_file_drag_context_matches(
            &gpui::ExternalPaths::default(),
            Some(&context)
        ));
    }
}
