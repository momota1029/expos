use super::*;
use std::{
    borrow::Cow,
    collections::VecDeque,
    time::{Duration, Instant},
};

pub(super) struct OperationSpeedHistory {
    token: AttemptToken,
    samples: VecDeque<(Instant, u64)>,
    graph_samples: VecDeque<OperationSpeedSample>,
    last_graph_sample: Option<(Instant, u64)>,
}

#[derive(Clone, Copy)]
struct OperationSpeedSample {
    progress_ratio: f32,
    bytes_per_second: f64,
}

const OPERATION_SPEED_WINDOW: Duration = Duration::from_secs(3);
const OPERATION_GRAPH_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

impl FilerShell {
    pub(super) fn job_progress_selection_is_current(&self) -> bool {
        self.job_progress_selection.is_some_and(|selection| {
            self.workspace.active == selection.workspace
                && self.workspace.active().job_progress.as_ref().is_some_and(|job| {
                    job.pane.id == selection.pane && job.generation == selection.generation
                })
        })
    }

    pub(super) fn clear_job_progress_selection(&mut self) {
        self.job_progress_selection = None;
    }

    pub(super) fn activate_job_progress_selection(
        &mut self,
        selection: JobProgressSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.active != selection.workspace
            || self.workspace.active().job_progress.as_ref().is_none_or(|job| {
                job.pane.id != selection.pane || job.generation != selection.generation
            })
        {
            return;
        }
        self.job_progress_selection = Some(selection);
        self.pending_focus = false;
        self.job_progress_rail_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn reconcile_floating_job_progress(&mut self, cx: &mut Context<Self>) {
        let has_live_rows = cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime
            .has_progress_rows();
        let active_workspace = self.workspace.active;
        let mut placement_changed = false;
        for (workspace_id, workspace) in &mut self.workspace.ordered {
            if has_live_rows {
                let had_pane = workspace.job_progress.is_some();
                workspace.ensure_floating_job_progress();
                placement_changed |= !had_pane && workspace.job_progress.is_some();
            } else if workspace.job_progress.as_ref().is_some_and(|pane| {
                matches!(pane.placement, JobProgressPlacement::Floating { .. })
            }) {
                let closing_id = workspace.job_progress.as_ref().map(|job| job.pane.id);
                workspace.job_progress = None;
                if *workspace_id == active_workspace {
                    self.job_progress_focus_reconcile = true;
                }
                placement_changed = true;
                if let Some(id) = closing_id {
                    if self.job_progress_dock_contact.is_some_and(|contact| {
                        contact.payload.workspace == active_workspace && contact.payload.pane == id
                    }) {
                        self.job_progress_dock_contact = None;
                    }
                }
            }
        }
        if placement_changed {
            let generation = self.job_progress_dock_contact.take().map_or(self.next_job_progress_dock_contact, |contact| contact.generation);
            self.next_job_progress_dock_contact = generation.wrapping_add(1).max(1);
            self.job_progress_dock_admission.set(None);
        }
    }

    pub(super) fn close_floating_job_progress(
        &mut self,
        workspace_id: WorkspaceId,
        pane_id: PaneId,
        topology_revision: u64,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.active != workspace_id
            || self.workspace.active().topology_revision != topology_revision
            || self
                .workspace
                .active()
                .job_progress
                .as_ref()
                .is_none_or(|job| job.pane.id != pane_id)
        {
            return;
        }
        let workspace = self.workspace.active_mut();
        if matches!(workspace.job_progress.as_ref().map(|job| &job.placement), Some(JobProgressPlacement::Docked)) {
            if workspace.pane_layout.remove_pane(pane_id).is_none() {
                return;
            }
            workspace.bump_topology_revision();
        }
        workspace.job_progress = None;
        self.clear_job_progress_selection();
        self.job_progress_focus_reconcile = true;
        let generation = self.job_progress_dock_contact.take().map_or(self.next_job_progress_dock_contact, |contact| contact.generation);
        self.next_job_progress_dock_contact = generation.wrapping_add(1).max(1);
        self.job_progress_dock_admission.set(None);
        self.save_requested = true;
        cx.notify();
    }

    pub(super) fn start_job_progress_dock_contact(&mut self, payload: JobProgressDockPayload, cx: &mut Context<Self>) {
        let Some(job) = self.workspace.active().job_progress.as_ref() else { return };
        if self.workspace.active != payload.workspace
            || self.workspace.active().topology_revision != payload.topology_revision
            || job.pane.id != payload.pane
            || job.generation != payload.generation
            || payload.contact_generation != self.next_job_progress_dock_contact
            || self.job_progress_dock_contact.is_some()
            || !matches!(job.placement, JobProgressPlacement::Floating { .. })
        { return; }
        self.job_progress_dock_contact = Some(JobProgressDockContact {
            payload,
            generation: payload.contact_generation,
        });
        self.job_progress_dock_admission.set(Some(payload.contact_generation));
        cx.notify();
    }

    pub(super) fn clear_job_progress_dock_contact(&mut self, cx: &mut Context<Self>) {
        let Some(contact) = self.job_progress_dock_contact.take() else { return };
        self.next_job_progress_dock_contact = contact.generation.wrapping_add(1).max(1);
        self.job_progress_dock_admission.set(None);
        cx.notify();
    }

    pub(super) fn render_job_progress_dock_targets(&self, target: PaneId, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(contact) = self.job_progress_dock_contact else { return Vec::new() };
        let payload = contact.payload;
        let workspace_id = self.workspace.active;
        if self.workspace.active != payload.workspace
            || self.workspace.active().topology_revision != payload.topology_revision
            || !self.workspace.active().panes.iter().any(|pane| pane.id == target)
        { return Vec::new(); }
        let edge = |direction, after, label: &'static str, bounds: fn(gpui::Div) -> gpui::Div| {
            let captured = payload;
            bounds(div()).id((label, target.0))
                .bg(gpui_filer_ui::theme_palette(cx).accent.opacity(0.28))
                .can_drop(move |value, _, _| value.downcast_ref::<JobProgressDockPayload>().is_some_and(|drag| *drag == captured && drag.workspace == workspace_id))
                .on_drop::<JobProgressDockPayload>(cx.listener(move |this, drag, _, cx| {
                    this.dock_job_progress(*drag, target, direction, after, cx);
                }))
                .into_any_element()
        };
        vec![
            edge(SplitDirection::Vertical, false, "top", |e| e.absolute().top_0().left_0().right_0().h(px(36.0))),
            edge(SplitDirection::Vertical, true, "bottom", |e| e.absolute().bottom_0().left_0().right_0().h(px(36.0))),
            edge(SplitDirection::Horizontal, false, "left", |e| e.absolute().left_0().top_0().bottom_0().w(px(36.0))),
            edge(SplitDirection::Horizontal, true, "right", |e| e.absolute().right_0().top_0().bottom_0().w(px(36.0))),
        ]
    }

    pub(super) fn render_docked_job_progress(&self, pane_id: PaneId, cx: &mut Context<Self>) -> gpui::AnyElement {
        let workspace_id = self.workspace.active;
        let topology_revision = self.workspace.active().topology_revision;
        let palette = gpui_filer_ui::theme_palette(cx);
        let rows = cx.global::<operations::application::OperationApplicationOwner>().runtime.progress_rows();
        let rows = rows.into_iter().map(|row| {
            let token = row.token;
            let can_cancel = matches!(row.state, operations::model::AttemptState::Preparing | operations::model::AttemptState::Running);
            let can_retry = row.retryable;
            div().flex().items_center().justify_between().gap(px(8.0)).py(px(6.0))
                .child(render_job_progress_row_label(&row, &self.translator, palette, cx))
                .when(can_cancel, |item| item.child(job_progress_button(format!("docked-progress-cancel-{token:?}"), self.translator.text(Message::ConflictCancel).into_owned(), palette)
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| if event.standard_click() {
                        let _ = cx.global_mut::<operations::application::OperationApplicationOwner>().runtime.cancel(token);
                        this.record_active_operation(cx); cx.notify();
                    }))))
                .when(can_retry, |item| item.child(job_progress_button(format!("docked-progress-retry-{token:?}"), self.translator.text(Message::Retry).into_owned(), palette).focusable().tab_stop(true)
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| if event.standard_click() {
                        this.retry_operation(token, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                        if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                            cx.stop_propagation();
                            this.retry_operation(token, window, cx);
                        }
                    }))))
                .into_any_element()
        });
        div().id(("docked-job-progress", pane_id.0)).flex().flex_col().size_full().min_w_0().min_h_0()
            .bg(palette.surface_base).text_color(palette.text)
            .child(div().h(px(PANE_HEADER_HEIGHT)).flex_shrink_0().flex().items_center().justify_between().px(px(12.0))
                .border_b_1().border_color(palette.border).child(self.translator.text(Message::JobProgressTitle).into_owned())
                .child(job_progress_button("docked-progress-close", self.translator.text(Message::JobProgressClose).into_owned(), palette)
                    .track_focus(&self.job_progress_close_focus).focusable().tab_stop(true)
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| if event.standard_click() {
                        this.close_floating_job_progress(workspace_id, pane_id, topology_revision, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                            cx.stop_propagation();
                            this.close_floating_job_progress(workspace_id, pane_id, topology_revision, cx);
                        }
                    }))))
            .child(div().id("docked-progress-rows").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.job_progress_scroll).occlude().flex().flex_col().px(px(12.0)).children(rows))
            .occlude().into_any_element()
    }

    pub(super) fn dock_job_progress(
        &mut self,
        payload: JobProgressDockPayload,
        target: PaneId,
        direction: SplitDirection,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        let valid_contact = self.job_progress_dock_contact.is_some_and(|contact| {
            contact.payload == payload
                && contact.generation == payload.contact_generation
                && payload.contact_generation == self.next_job_progress_dock_contact
        });
        if !valid_contact || self.workspace.active != payload.workspace { return; }
        let workspace = self.workspace.active_mut();
        let Some(job) = workspace.job_progress.as_ref() else { return };
        if workspace.topology_revision != payload.topology_revision
            || job.pane.id != payload.pane
            || job.generation != payload.generation
            || !matches!(job.placement, JobProgressPlacement::Floating { .. })
            || !workspace.panes.iter().any(|pane| pane.id == target)
            || !workspace.pane_layout.contains(target)
            || workspace.pane_layout.contains(payload.pane)
        { return; }
        if !workspace.pane_layout.split_next_to(target, payload.pane, direction, after) { return; }
        workspace.job_progress.as_mut().expect("validated progress pane").placement = JobProgressPlacement::Docked;
        workspace.bump_topology_revision();
        self.job_progress_dock_contact = None;
        self.next_job_progress_dock_contact = payload.contact_generation.wrapping_add(1).max(1);
        self.job_progress_dock_admission.set(None);
        self.save_requested = true;
        cx.notify();
    }

    pub(super) fn render_floating_job_progress(
        &self,
        viewport_width: f32,
        viewport_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let job = self.workspace.active().job_progress.as_ref()?;
        let JobProgressPlacement::Floating { right, top, width, max_height } = job.placement else { return None };
        let pane_id = job.pane.id;
        let top = top.min((viewport_height - 76.0).max(8.0));
        let width = width.min(((viewport_width - 36.0) / 2.0).clamp(120.0, 400.0));
        let max_height = max_height.min((viewport_height - top - 16.0).max(0.0));
        let rows_max_height = (max_height - 78.0).max(0.0);
        let workspace_id = self.workspace.active;
        let topology_revision = self.workspace.active().topology_revision;
        let generation = job.generation;
        let palette = gpui_filer_ui::theme_palette(cx);
        let title = self.translator.text(Message::JobProgressTitle).into_owned();
        let drag_title = title.clone();
        // GPUI dispatches threshold-crossing mouse movement synchronously; bind the
        // candidate nonce before pointer-down so the drag payload matches its capture.
        let dock_payload = JobProgressDockPayload {
            workspace: workspace_id,
            pane: pane_id,
            generation,
            topology_revision,
            contact_generation: self.next_job_progress_dock_contact,
        };
        let dock_admission = self.job_progress_dock_admission.clone();
        let rows = cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime
            .progress_rows();
        let rows = rows.into_iter().map(|row| {
            let token = row.token;
            let can_cancel = matches!(
                row.state,
                operations::model::AttemptState::Preparing
                    | operations::model::AttemptState::Running
            );
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .py(px(6.0))
                .child(render_job_progress_row_label(&row, &self.translator, palette, cx))
                .when(can_cancel, |row| {
                    row.child(
                        job_progress_button(
                            format!("job-progress-cancel-{token:?}"),
                            self.translator.text(Message::ConflictCancel).into_owned(),
                            palette,
                        )
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                                if event.standard_click() {
                                    let _ = cx.global_mut::<operations::application::OperationApplicationOwner>().runtime.cancel(token);
                                    this.record_active_operation(cx);
                                    cx.notify();
                                }
                            })),
                    )
                })
                .when(row.retryable, |row| {
                    row.child(
                        job_progress_button(
                            format!("job-progress-retry-{token:?}"),
                            self.translator.text(Message::Retry).into_owned(),
                            palette,
                        )
                            .focusable()
                            .tab_stop(true)
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                if event.standard_click() {
                                    this.retry_operation(token, window, cx);
                                }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                                if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation();
                                    this.retry_operation(token, window, cx);
                                }
                            })),
                    )
                })
                .into_any_element()
        });
        Some(
            div()
                .id("floating-job-progress")
                .absolute()
                .right(px(right))
                .top(px(top))
                .w(px(width))
                .max_h(px(max_height))
                .p(px(12.0))
                .rounded(px(RADIUS_MD))
                .border_1()
                .border_color(palette.text.opacity(0.16))
                .bg(palette.surface_raised)
                .text_color(palette.text)
                .shadow_lg()
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .on_mouse_down(gpui::MouseButton::Right, cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .child(
                    div().flex().items_center().justify_between().gap(px(8.0)).mb(px(8.0))
                        .child(div()
                        .id("job-progress-title")
                        .flex_1()
                        .min_w_0()
                        .cursor_move()
                        .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            if event.source == gpui::MouseInputSource::Mouse && !cx.has_active_drag() {
                                this.start_job_progress_dock_contact(dock_payload, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .on_drag_with_threshold_when(
                            dock_payload,
                            px(8.0),
                            move |down| {
                                down.source == gpui::MouseInputSource::Mouse
                                    && down.button == gpui::MouseButton::Left
                                    && dock_admission.get() == Some(dock_payload.contact_generation)
                            },
                            move |_, _, _, cx| {
                                cx.new(|_| PaneReorderGhost { title: drag_title.clone() })
                            },
                        )
                        .child(div().text_size(px(14.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(title)))
                        .child(div().flex().items_center().gap(px(2.0))
                        .child(
                            job_progress_button(
                                "job-progress-details",
                                self.translator.text(Message::JobProgressDetails).into_owned(),
                                palette,
                            )
                                .track_focus(&self.job_progress_details_focus)
                                .focusable()
                                .tab_stop(true)
                                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _window, cx| {
                                    if event.standard_click() {
                                        this.operation_progress_open = true;
                                        cx.notify();
                                    }
                                }))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                    if !event.keystroke.modifiers.modified()
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                                    {
                                        cx.stop_propagation();
                                        this.operation_progress_open = true;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            job_progress_button(
                                "job-progress-close",
                                self.translator.text(Message::JobProgressClose).into_owned(),
                                palette,
                            )
                                .track_focus(&self.job_progress_close_focus)
                                .focusable()
                                .tab_stop(true)
                                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                                    if event.standard_click() {
                                        this.close_floating_job_progress(workspace_id, pane_id, topology_revision, cx);
                                    }
                                }))
                                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                    if !event.keystroke.modifiers.modified()
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                                    {
                                        cx.stop_propagation();
                                        this.close_floating_job_progress(workspace_id, pane_id, topology_revision, cx);
                                    }
                                })),
                        )),
                )
                .child(
                    div()
                        .id("job-progress-rows")
                        .max_h(px(rows_max_height))
                        .overflow_y_scroll()
                        .track_scroll(&self.job_progress_scroll)
                        .occlude()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .occlude()
                .into_any_element(),
        )
    }

    pub(super) fn toggle_directory_move_review(
        &mut self,
        token: operations::model::AttemptToken,
        cx: &mut Context<Self>,
    ) {
        if self.operation_directory_move_review_open.remove(&token) {
            cx.global_mut::<operations::application::OperationApplicationOwner>()
                .cancel_directory_move_review(token);
            cx.notify();
            return;
        }
        self.operation_directory_move_review_open.insert(token);
        let request = cx
            .global_mut::<operations::application::OperationApplicationOwner>()
            .begin_directory_move_review(token);
        if let Some((generation, key, receiver)) = request {
            cx.spawn(async move |this, cx| {
                let result = receiver
                    .recv()
                    .await
                    .unwrap_or_else(|_| Err("operation journal writer is unavailable".into()));
                let probe = this.update(cx, |_, cx| {
                    let owner = cx.global_mut::<operations::application::OperationApplicationOwner>();
                    let committed = owner.commit_directory_move_review(token, generation, &key, result);
                    if committed { owner.directory_move_root_probe(token, generation, &key) } else { None }
                }).ok().flatten();
                if let Some(probe) = probe {
                    let observations = cx.background_executor()
                        .spawn(async move { operations::executor::inspect_directory_move_roots(&probe) })
                        .await;
                    let _ = this.update(cx, |_, cx| {
                        let committed = cx.global_mut::<operations::application::OperationApplicationOwner>()
                            .finish_directory_move_root_probe(token, generation, &key, observations);
                        if committed { cx.notify(); }
                    });
                } else {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn toggle_copy_failure_review(
        &mut self,
        token: operations::model::AttemptToken,
        ticket: operations::model::InspectionTicket,
        cx: &mut Context<Self>,
    ) {
        let runtime = &cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime;
        let latest_attempt = runtime
            .job(token)
            .and_then(|job| job.attempts.last().map(|attempt| attempt.token));
        let current = latest_attempt == Some(token)
            && runtime
                .latest_copy_failure_inspection(token)
                .is_some_and(|(latest_ticket, result)| latest_ticket == ticket && result.is_some());
        if !current {
            return;
        }
        let key = (token, ticket);
        if !self.operation_copy_review_open.remove(&key) {
            self.operation_copy_review_open.insert(key);
        }
        cx.notify();
    }

    pub(super) fn observe_operation_progress(
        &mut self,
        token: AttemptToken,
        update: &ProgressUpdate,
    ) {
        let now = Instant::now();
        let history = self
            .operation_speed
            .get_or_insert_with(|| OperationSpeedHistory {
                token,
                samples: VecDeque::new(),
                graph_samples: VecDeque::new(),
                last_graph_sample: None,
            });
        if history.token != token {
            history.token = token;
            history.samples.clear();
            history.graph_samples.clear();
            history.last_graph_sample = None;
        }
        if history
            .samples
            .back()
            .is_some_and(|(_, bytes)| *bytes > update.counters.processed_bytes)
        {
            // A different attempt must never inherit a faster attempt's byte
            // baseline.  The model rejects stale sequence numbers; this is a
            // presentation-only guard for a new attempt that has just begun.
            history.samples.clear();
            history.graph_samples.clear();
            history.last_graph_sample = None;
        }
        if history.last_graph_sample.is_none() {
            history.last_graph_sample = Some((now, update.counters.processed_bytes));
        }
        if let (Some(total), Some((previous_at, previous_bytes))) =
            (update.counters.total_bytes, history.last_graph_sample)
            && total > 0
            && update.counters.processed_bytes > previous_bytes
            && now.saturating_duration_since(previous_at) >= OPERATION_GRAPH_SAMPLE_INTERVAL
        {
            let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
            if elapsed > 0.0 {
                history.graph_samples.push_back(OperationSpeedSample {
                    progress_ratio: (update.counters.processed_bytes as f64 / total as f64)
                        .clamp(0.0, 1.0) as f32,
                    bytes_per_second: (update.counters.processed_bytes - previous_bytes) as f64
                        / elapsed,
                });
                while history.graph_samples.len() > 240 {
                    history.graph_samples.pop_front();
                }
                history.last_graph_sample = Some((now, update.counters.processed_bytes));
            }
        }
        history
            .samples
            .push_back((now, update.counters.processed_bytes));
        while history.samples.len() > 2
            && history
                .samples
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > OPERATION_SPEED_WINDOW)
        {
            history.samples.pop_front();
        }
    }

    fn operation_speed(&self, token: AttemptToken) -> Option<u64> {
        let history = self
            .operation_speed
            .as_ref()
            .filter(|history| history.token == token)?;
        let (start_at, start_bytes) = history.samples.front()?;
        let (end_at, end_bytes) = history.samples.back()?;
        let elapsed = end_at.checked_duration_since(*start_at)?;
        (elapsed.as_secs_f64() > 0.0)
            .then(|| end_bytes.saturating_sub(*start_bytes) as f64 / elapsed.as_secs_f64())
            .map(|bytes_per_second| bytes_per_second.round() as u64)
    }

    fn operation_graph(&self, token: AttemptToken) -> Option<Vec<OperationSpeedSample>> {
        self.operation_speed
            .as_ref()
            .filter(|history| history.token == token)
            .map(|history| history.graph_samples.iter().copied().collect())
    }

    /// 進捗は tab に属さない。一つの小さな shell projection として出し、origin tab を
    /// 閉じても cancel せずに残す。操作実体は Application owner だけが持つ。
    pub(super) fn render_operation_surface(
        &self,
        progress_pane_open: bool,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let job = cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime
            .active_job()?;
        let attempt = job.attempts.last()?;
        let state = match attempt.state {
            AttemptState::Preparing => "Preparing…",
            AttemptState::Running => match job.request.kind {
                OperationKind::Copy => "Copying…",
                OperationKind::Move => "Moving…",
                OperationKind::ExternalMoveCleanup => "Cleaning up moved files…",
                OperationKind::CopyStageCleanup => "Deleting retained Copy stage…",
                OperationKind::CreateSymbolicLink => "Creating symbolic links…",
                OperationKind::CreateJunction => "Creating junctions…",
                OperationKind::CreateFolder => "Creating folder…",
                OperationKind::CreateFile => "Creating file…",
                OperationKind::CreateShortcut => "Creating shortcut…",
                OperationKind::Trash if cfg!(target_os = "linux") => "Moving to Trash…",
                OperationKind::Trash => "Moving to Recycle Bin…",
                OperationKind::Rename => "Renaming…",
                OperationKind::BatchRename => "Renaming items…",
                OperationKind::BatchRenameUndo => "Undoing batch rename…",
                OperationKind::CreateZip => "Creating ZIP…",
                OperationKind::Create7z => "Creating 7z archive…",
                OperationKind::ExtractZip => "Extracting ZIP…",
                OperationKind::Extract7z => "Extracting 7z…",
            },
            AttemptState::WaitingConflict => "Waiting for a conflict decision…",
            AttemptState::CancelRequested => "Cancelling…",
            AttemptState::Queued | AttemptState::Terminal => return None,
        };
        let state = if attempt.state == AttemptState::WaitingConflict {
            self.translator.text(Message::ConflictWaiting).into_owned()
        } else if attempt.state == AttemptState::Running
            && job.request.kind == OperationKind::CreateShortcut
        {
            self.translator.text(Message::OperationCreatingShortcut).into_owned()
        } else if attempt.state == AttemptState::Running
            && job.request.kind == OperationKind::CopyStageCleanup
        {
            self.translator.text(Message::CopyStageCleanupRunning).into_owned()
        } else {
            state.to_owned()
        };
        let token = attempt.token;
        let (progress, transferred, current, fraction, discovered_fraction) =
            attempt.progress.as_ref().map_or_else(
                || ("Starting…".to_owned(), None, None, None, false),
                |progress| {
                    let items = match progress.counters.total_items {
                        Some(total) => {
                            format!("{} / {} items", progress.counters.processed_items, total)
                        }
                        None if progress.counters.discovered_items > 0 => format!(
                            "{} / {} discovered files copied",
                            progress.counters.processed_items,
                            progress
                                .counters
                                .discovered_items
                                .max(progress.counters.processed_items)
                        ),
                        None => "Discovering files…".to_owned(),
                    };
                    let discovered_bytes = progress
                        .counters
                        .discovered_bytes
                        .max(progress.counters.processed_bytes);
                    let bytes = match progress.counters.total_bytes {
                        Some(total) => format!(
                            "{} / {}",
                            format_operation_bytes(progress.counters.processed_bytes),
                            format_operation_bytes(total)
                        ),
                        None if discovered_bytes > 0 => format!(
                            "{} / {} discovered",
                            format_operation_bytes(progress.counters.processed_bytes),
                            format_operation_bytes(discovered_bytes)
                        ),
                        None => format_operation_bytes(progress.counters.processed_bytes),
                    };
                    let current = progress.current_item.and_then(|item| {
                        job.request
                            .sources
                            .get(item.0 as usize)
                            .and_then(|path| path.file_name())
                            .map(|name| name.to_string_lossy().into_owned())
                    });
                    let (fraction, discovered_fraction) = match progress.counters.total_bytes {
                        Some(total) if total > 0 => (
                            Some(
                                (progress.counters.processed_bytes as f32 / total as f32)
                                    .clamp(0.0, 1.0),
                            ),
                            false,
                        ),
                        _ if discovered_bytes > 0 => (
                            Some(
                                (progress.counters.processed_bytes as f32
                                    / discovered_bytes as f32)
                                    .clamp(0.0, 1.0),
                            ),
                            true,
                        ),
                        _ => (None, false),
                    };
                    (items, Some(bytes), current, fraction, discovered_fraction)
                },
            );
        let speed = self
            .operation_speed(token)
            .map(|bytes| format!("{} / s", format_operation_bytes(bytes)));
        let speed_graph = fraction.and_then(|fraction| {
            self.operation_graph(token)
                .filter(|samples| !samples.is_empty())
                .map(|samples| render_operation_speed_graph(samples, fraction))
        });
        let cancellable = matches!(
            attempt.state,
            AttemptState::Preparing | AttemptState::Running
        );
        Some(
            div()
                .id("operation-progress")
                .absolute()
                .bottom(px(12.0))
                .w(px(width))
                .when(progress_pane_open, |surface| surface.left(px(12.0)))
                .when(!progress_pane_open, |surface| surface.right(px(12.0)))
                .p(px(12.0))
                .rounded(px(RADIUS_MD))
                .border_1()
                .border_color(BORDER)
                .bg(SURFACE_RAISED)
                .text_color(TEXT)
                .occlude()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(state)
                        .when_some(current, |column, current| {
                            column.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(TEXT_MUTED)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(current),
                            )
                        })
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(TEXT_MUTED)
                                .child(progress),
                        )
                        .when_some(transferred, |column, transferred| {
                            column.child(div().text_size(px(11.0)).text_color(TEXT_MUTED).child(
                                match speed {
                                    Some(speed) => format!("{transferred} · {speed}"),
                                    None => transferred,
                                },
                            ))
                        })
                        // Directory tree は合計が未確定なので、割合を捏造しない。regular
                        // file batch の確定総量だけを横バーにして、数値表示と同じ raw
                        // progress counters から描く。
                        .when_some(fraction, |column, fraction| {
                            column
                                .when(discovered_fraction, |column| {
                                    column.child(
                                        div()
                                            .mt(px(6.0))
                                            .text_size(px(10.0))
                                            .text_color(TEXT_MUTED)
                                            .child("Discovered so far"),
                                    )
                                })
                                .child(
                                    div()
                                        .mt(if discovered_fraction {
                                            px(3.0)
                                        } else {
                                            px(6.0)
                                        })
                                        .w_full()
                                        .h(px(4.0))
                                        .rounded(px(2.0))
                                        .bg(BORDER)
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .w(relative(fraction))
                                                .rounded(px(2.0))
                                                .bg(ACCENT),
                                        ),
                                )
                                .when_some(speed_graph, |column, graph| column.child(graph))
                        }),
                )
                .child(
                    div()
                        .id("operation-details")
                        .track_focus(&self.operation_details_focus)
                        .focusable()
                        .min_w(px(64.0))
                        .h(px(44.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(RADIUS_SM))
                        .hover(|style| style.bg(CHROME_HOVER))
                        .child("Details")
                        .on_click(cx.listener(|this, event: &gpui::ClickEvent, _window, cx| {
                            if event.standard_click() {
                                this.operation_progress_open = true;
                                cx.notify();
                            }
                        }))
                        .on_key_down(cx.listener(
                            |this, event: &gpui::KeyDownEvent, _window, cx| {
                                if !event.keystroke.modifiers.modified()
                                    && matches!(
                                        event.keystroke.key.as_str(),
                                        "enter" | "space" | " "
                                    )
                                {
                                    cx.stop_propagation();
                                    this.operation_progress_open = true;
                                    cx.notify();
                                }
                            },
                        )),
                )
                .child(
                    div()
                        .id("operation-cancel")
                        .min_w(px(44.0))
                        .h(px(32.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(RADIUS_SM))
                        .when(cancellable, |button| {
                            button
                                .hover(|style| style.bg(CHROME_HOVER))
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, _window, cx| {
                                        if event.standard_click() {
                                            let _ = cx.global_mut::<operations::application::OperationApplicationOwner>().runtime.cancel(token);
                                            this.record_active_operation(cx);
                                            cx.notify();
                                        }
                                    },
                                ))
                        })
                        .when(!cancellable, |button| button.text_color(TEXT_MUTED))
                        .child("Cancel"),
                )
                .into_any_element(),
        )
    }

    /// This is deliberately a shell overlay, not the Phase F pane model. It
    /// reads the owner snapshot every frame; closing it cannot discard jobs.
    pub(super) fn render_operation_history_overlay(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.operation_progress_open {
            return None;
        }
        let palette = gpui_filer_ui::theme_palette(cx);
        let divider = palette.text.opacity(0.10);
        let header_height = 56.0;
        let viewport_height = window.viewport_size().height.as_f32();
        // 小さいウィンドウでも、アクション付きの行をスクロールで表示できる高さを確保する。
        let panel_max_height = (viewport_height * 0.4)
            .max(header_height + 72.0)
            .min((viewport_height - 24.0).max(0.0));
        let body_max_height = (panel_max_height - header_height - 2.0).max(0.0);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        // Clone only the bounded window-wide history projection for this frame.
        let jobs = cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime
            .recent_jobs(6);
        let copy_review_keys: std::collections::HashSet<_> = jobs
            .iter()
            .filter(|job| job.request.kind == OperationKind::Copy)
            .filter_map(|job| job.attempts.last())
            .filter(|attempt| attempt.state == AttemptState::Terminal)
            .filter_map(|attempt| {
                cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .latest_copy_failure_inspection(attempt.token)
                    .map(|(ticket, _)| (attempt.token, ticket))
            })
            .collect();
        let copy_retry_keys: std::collections::HashSet<_> = jobs
            .iter()
            .filter(|job| job.request.kind == OperationKind::Copy)
            .filter_map(|job| job.attempts.last())
            .filter_map(|attempt| {
                let (ticket, result) = cx
                    .global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .latest_copy_failure_inspection(attempt.token)?;
                let _result = result?;
                let eligible = cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .verified_copy_retry_guards(attempt.token, ticket)
                    .is_some_and(|guards| !guards.is_empty())
                    || cx.global::<operations::application::OperationApplicationOwner>()
                        .runtime
                        .verified_copy_directory_retry_guards(attempt.token, ticket)
                        .is_some_and(|guards| !guards.is_empty());
                eligible.then_some((attempt.token, ticket))
            })
            .collect();
        let copy_cleanup_keys: std::collections::HashSet<_> = jobs
            .iter()
            .filter(|job| job.request.kind == OperationKind::Copy)
            .filter_map(|job| job.attempts.last())
            .flat_map(|attempt| {
                let runtime = &cx
                    .global::<operations::application::OperationApplicationOwner>()
                    .runtime;
                let Some((ticket, Some(_))) = runtime.latest_copy_failure_inspection(attempt.token)
                else { return Vec::new(); };
                runtime
                    .verified_copy_stage_cleanup_items(attempt.token, ticket)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|item| (attempt.token, ticket, item))
                    .collect::<Vec<_>>()
            })
            .collect();
        let focused_action_disappears =
            self.operation_cancel_focus.iter().any(|(job_id, focus)| {
                focus.is_focused(window)
                    && jobs
                        .iter()
                        .find(|job| job.id == *job_id)
                        .and_then(|job| job.attempts.last())
                        .map_or(true, |attempt| {
                            !matches!(
                                attempt.state,
                                AttemptState::Preparing | AttemptState::Running
                            )
                        })
            }) || self.operation_retry_focus.iter().any(|(job_id, focus)| {
                focus.is_focused(window)
                    && jobs
                        .iter()
                        .find(|job| job.id == *job_id)
                        .and_then(|job| job.attempts.last())
                        .map_or(true, |attempt| {
                            attempt.state != AttemptState::Terminal
                                || !attempt.terminal.as_ref().is_some_and(|report| {
                                    matches!(
                                        &report.retry,
                                        operations::model::RetryPlan::Explicit(items)
                                            if !items.is_empty()
                                    )
                                })
                        })
            }) || self
                .operation_move_review_focus
                .iter()
                .any(|(job_id, focus)| {
                    focus.is_focused(window) && !jobs.iter().any(|job| job.id == *job_id)
                })
                || self
                    .operation_move_confirm_focus
                    .iter()
                    .any(|(job_id, focus)| {
                        focus.is_focused(window) && !jobs.iter().any(|job| job.id == *job_id)
                    })
                || self.operation_copy_review_focus.iter().any(|(key, focus)| {
                    focus.is_focused(window) && !copy_review_keys.contains(key)
                })
                || self.operation_copy_retry_focus.iter().any(|(key, focus)| {
                    focus.is_focused(window) && !copy_retry_keys.contains(key)
                })
                || self.operation_copy_cleanup_focus.iter().any(|(key, focus)| {
                    focus.is_focused(window) && !copy_cleanup_keys.contains(key)
                })
                || self.operation_copy_cleanup_path_focus.iter().any(|(key, focus)| {
                    focus.is_focused(window) && !copy_cleanup_keys.contains(key)
                });
        if focused_action_disappears {
            self.operation_history_close_focus.focus(window, cx);
        }
        self.operation_retry_focus
            .retain(|job_id, _| jobs.iter().any(|job| job.id == *job_id));
        self.operation_cancel_focus
            .retain(|job_id, _| jobs.iter().any(|job| job.id == *job_id));
        self.operation_move_review_focus
            .retain(|job_id, _| jobs.iter().any(|job| job.id == *job_id));
        self.operation_move_confirm_focus
            .retain(|job_id, _| jobs.iter().any(|job| job.id == *job_id));
        self.operation_copy_review_focus
            .retain(|key, _| copy_review_keys.contains(key));
        self.operation_copy_retry_focus
            .retain(|key, _| copy_retry_keys.contains(key));
        self.operation_copy_cleanup_focus
            .retain(|key, _| copy_cleanup_keys.contains(key));
        self.operation_copy_cleanup_path_focus
            .retain(|key, _| copy_cleanup_keys.contains(key));
        self.operation_copy_cleanup_scroll
            .retain(|key, _| copy_cleanup_keys.contains(key));
        self.operation_copy_review_scroll
            .retain(|key, _| copy_review_keys.contains(key));
        self.operation_copy_review_open
            .retain(|key| copy_review_keys.contains(key));
        for (index, job) in jobs.into_iter().enumerate() {
            let retry_focus = self
                .operation_retry_focus
                .entry(job.id)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let cancel_focus = self
                .operation_cancel_focus
                .entry(job.id)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let move_review_focus = self
                .operation_move_review_focus
                .entry(job.id)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let move_confirm_focus = self
                .operation_move_confirm_focus
                .entry(job.id)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let review_open = self.operation_move_review_open.contains(&job.id);
            let Some(attempt) = job.attempts.last() else {
                continue;
            };
            let token = attempt.token;
            let copy_inspection_state = if job.request.kind == OperationKind::Copy
                && attempt.state == AttemptState::Terminal
            {
                cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .latest_copy_failure_inspection(token)
                    .map(|(ticket, result)| (ticket, result.cloned()))
            } else {
                None
            };
            let copy_review_key = copy_inspection_state
                .as_ref()
                .map(|(ticket, _)| (token, *ticket));
            let copy_review_focus = copy_review_key.map(|key| {
                self.operation_copy_review_focus
                    .entry(key)
                    .or_insert_with(|| cx.focus_handle())
                    .clone()
            });
            let copy_retry_focus = copy_review_key.map(|key| {
                self.operation_copy_retry_focus
                    .entry(key)
                    .or_insert_with(|| cx.focus_handle())
                    .clone()
            });
            let copy_review_open = copy_review_key
                .is_some_and(|key| self.operation_copy_review_open.contains(&key));
            let copy_review_scroll = copy_review_key.map(|key| {
                self.operation_copy_review_scroll
                    .entry(key)
                    .or_insert_with(gpui::ScrollHandle::new)
                    .clone()
            });
            let row_index = rows.len();
            if move_confirm_focus.is_focused(window) && review_open {
                // Expanded inspection details are a separate scroll child, so
                // the confirmation action remains revealable even when the
                // details are taller than the viewport.
                self.operation_history_scroll.scroll_to_item(row_index + 2);
            } else if cancel_focus.is_focused(window)
                || retry_focus.is_focused(window)
                || move_review_focus.is_focused(window)
                || copy_review_focus
                    .as_ref()
                    .is_some_and(|focus| focus.is_focused(window) && !copy_review_open)
            {
                self.operation_history_scroll.scroll_to_item(row_index);
            }
            let inspection_state = if job.request.kind == OperationKind::Move
                && attempt.state == AttemptState::Terminal
            {
                cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .latest_guarded_move_inspection(token)
                    .map(|(ticket, result)| (ticket, result.cloned()))
            } else {
                None
            };
            let published_move_inspection_state = if job.request.kind == OperationKind::Move
                && attempt.state == AttemptState::Terminal
            {
                cx.global::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .latest_published_move_inspection(token)
                    .map(|(ticket, result)| (ticket, result.cloned()))
            } else { None };
            let cross_volume_move_inspection_state = if job.request.kind == OperationKind::Move && attempt.state == AttemptState::Terminal {
                cx.global::<operations::application::OperationApplicationOwner>().runtime
                    .latest_cross_volume_move_inspection(token).map(|(ticket, result)| (ticket, result.cloned()))
            } else { None };
            let directory_move_review_available = job.request.kind == OperationKind::Move
                && attempt.state == AttemptState::Terminal
                && attempt.terminal.as_ref().is_some_and(|terminal| {
                    matches!(terminal.outcome, operations::model::TerminalOutcome::Partial | operations::model::TerminalOutcome::Unknown)
                        && terminal.retry == operations::model::RetryPlan::RequiresReinspection
                })
                && inspection_state.is_none()
                && published_move_inspection_state.is_none()
                && cross_volume_move_inspection_state.is_none();
            let directory_move_review = cx.global::<operations::application::OperationApplicationOwner>()
                .directory_move_review(token).map(|(_, state)| state.clone());
            let directory_move_review_open = self.operation_directory_move_review_open.contains(&token);
            let verified_cross_volume_move_guards = cross_volume_move_inspection_state.as_ref().and_then(|(ticket, result)| {
                result.as_ref().and_then(|_| cx.global::<operations::application::OperationApplicationOwner>().runtime
                    .verified_cross_volume_move_retry_guards(token, *ticket))
            });
            let verified_move_items = inspection_state.as_ref().and_then(|(ticket, result)| {
                result.as_ref().and_then(|_| {
                    cx.global::<operations::application::OperationApplicationOwner>()
                        .runtime
                        .verified_guarded_move_retry_items(token, *ticket)
                })
            });
            let verified_copy_retry_guards = copy_inspection_state
                .as_ref()
                .and_then(|(ticket, result)| {
                    result.as_ref().and_then(|_| {
                        cx.global::<operations::application::OperationApplicationOwner>()
                            .runtime
                            .verified_copy_retry_guards(token, *ticket)
                    })
                });
            let verified_copy_directory_retry_guards = copy_inspection_state
                .as_ref()
                .and_then(|(ticket, result)| {
                    result.as_ref().and_then(|_| {
                        cx.global::<operations::application::OperationApplicationOwner>()
                            .runtime
                            .verified_copy_directory_retry_guards(token, *ticket)
                    })
                });
            let verified_copy_cleanup_items = copy_inspection_state
                .as_ref()
                .and_then(|(ticket, result)| {
                    result.as_ref().and_then(|_| {
                        cx.global::<operations::application::OperationApplicationOwner>()
                            .runtime
                            .verified_copy_stage_cleanup_items(token, *ticket)
                    })
                });
            let verified_copy_cleanup_item_set: std::collections::HashSet<_> =
                verified_copy_cleanup_items.iter().flatten().copied().collect();
            let verified_move_item_set: std::collections::HashSet<_> =
                verified_move_items.iter().flatten().copied().collect();
            let review_visible = inspection_state.is_some() || cross_volume_move_inspection_state.is_some() || published_move_inspection_state.is_some() || directory_move_review_available;
            let confirmation_visible = review_open
                && (verified_move_items.as_ref().is_some_and(|items| !items.is_empty())
                    || verified_cross_volume_move_guards.as_ref().is_some_and(|guards| !guards.is_empty()));
            let copy_retry_visible = copy_review_open
                && (verified_copy_retry_guards.as_ref().is_some_and(|guards| !guards.is_empty())
                    || verified_copy_directory_retry_guards.as_ref().is_some_and(|guards| !guards.is_empty()));
            if (move_review_focus.is_focused(window) && !review_visible)
                || (move_confirm_focus.is_focused(window) && !confirmation_visible)
                || (copy_retry_focus
                    .as_ref()
                    .is_some_and(|focus| focus.is_focused(window) && !copy_retry_visible))
            {
                self.operation_history_close_focus.focus(window, cx);
            }
            let cancellable = matches!(
                attempt.state,
                AttemptState::Preparing | AttemptState::Running
            );
            let retryable = attempt.state == AttemptState::Terminal
                && attempt.terminal.as_ref().is_some_and(|report| {
                    matches!(
                        &report.retry,
                        operations::model::RetryPlan::Explicit(items) if !items.is_empty()
                    )
                });
            let mut expanded_detail = None;
            let mut copy_expanded_detail = None;
            let mut confirmation_row = None;
            rows.push(
                div()
                    .id(("operation-history-row", index))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(16.0))
                    .py(px(12.0))
                    .border_b_1()
                    .border_color(divider)
                    .child(render_operation_history_row(
                        &job.request,
                        attempt,
                        &self.translator,
                        palette,
                    ))
                    .when(cancellable, |row| {
                        row.child(
                            job_progress_button(
                                ("operation-history-cancel", index),
                                self.translator.text(Message::ConflictCancel).into_owned(),
                                palette,
                            )
                                .debug_selector(|| "operation-history-cancel-action".to_owned())
                                .track_focus(&cancel_focus)
                                .focusable()
                                .when(cancel_focus.is_focused(window), |style| style.border_color(palette.text))
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, _window, cx| {
                                        if event.standard_click() {
                                            let _ = cx.global_mut::<operations::application::OperationApplicationOwner>().runtime.cancel(token);
                                            this.record_active_operation(cx);
                                            cx.notify();
                                        }
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    move |this, event: &gpui::KeyDownEvent, _window, cx| {
                                        if !event.keystroke.modifiers.modified()
                                            && matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space" | " "
                                            )
                                        {
                                            cx.stop_propagation();
                                            let _ = cx.global_mut::<operations::application::OperationApplicationOwner>().runtime.cancel(token);
                                            this.record_active_operation(cx);
                                            cx.notify();
                                        }
                                    },
                                )),
                        )
                    })
                    .when(retryable, |row| {
                        row.child(
                            job_progress_button(
                                ("operation-history-retry", index),
                                self.translator.text(Message::Retry).into_owned(),
                                palette,
                            )
                                .track_focus(&retry_focus)
                                .focusable()
                                .when(retry_focus.is_focused(window), |style| style.border_color(palette.text))
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, window, cx| {
                                        if event.standard_click() {
                                            this.retry_operation(token, window, cx);
                                        }
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    move |this, event: &gpui::KeyDownEvent, window, cx| {
                                        if !event.keystroke.modifiers.modified()
                                            && matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space" | " "
                                            )
                                        {
                                            cx.stop_propagation();
                                            this.retry_operation(token, window, cx);
                                        }
                                    },
                                )),
                        )
                    })
                    .when_some(inspection_state, |row, (ticket, result)| {
                let Some(result) = result else {
                            return row.child(div().text_size(px(11.0)).text_color(palette.text_muted)
                                .child(self.translator.text(Message::MoveInspectionPending).into_owned()));
                        };
                        let opened = review_open;
                        let retryable_count = verified_move_item_set.len();
                        let token = result.attempt;
                        let review_button = job_progress_button(
                            ("operation-move-review", index),
                            self.translator.text(Message::MoveInspectionReview).into_owned(),
                            palette,
                        )
                            .debug_selector(|| "operation-move-review-action".to_owned())
                            .track_focus(&move_review_focus)
                            .focusable()
                            .when(move_review_focus.is_focused(window), |style| style.border_color(palette.text))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                if event.standard_click() {
                                    if !this.operation_move_review_open.remove(&job.id) { this.operation_move_review_open.insert(job.id); }
                                    cx.notify();
                                }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation();
                                    if !this.operation_move_review_open.remove(&job.id) { this.operation_move_review_open.insert(job.id); }
                                    cx.notify();
                                }
                            }));
                        let mut detail = div().debug_selector(|| "operation-move-inspection-detail".to_owned()).flex().flex_col().gap(px(4.0)).px(px(6.0)).children(
                            result.candidates.iter().map(|candidate| {
                                let safe = verified_move_item_set.contains(&candidate.proof.item);
                                let identity_match = self.translator.text(if candidate.source_matches { Message::MoveIdentityMatches } else { Message::MoveIdentityMismatch }).into_owned();
                                let parent_match = self.translator.text(if candidate.destination_parent_matches { Message::MoveIdentityMatches } else { Message::MoveIdentityMismatch }).into_owned();
                                let destination = self.translator.text(match candidate.destination {
                                    operations::model::GuardedMoveDestinationState::Absent => Message::MoveDestinationAbsent,
                                    operations::model::GuardedMoveDestinationState::Present => Message::MoveDestinationPresent,
                                    operations::model::GuardedMoveDestinationState::Indeterminate => Message::MoveDestinationIndeterminate,
                                }).into_owned();
                                div().flex().flex_col().min_w_0().whitespace_normal().text_size(px(11.0))
                                    .child(format!("{} → {}", candidate.proof.source.display(), candidate.proof.destination.display()))
                                    .child(format!("{}: {identity_match}", self.translator.text(Message::MoveSourceIdentity)))
                                    .child(format!("{}: {parent_match}", self.translator.text(Message::MoveDestinationParentIdentity)))
                                    .child(format!("{}: {destination}", self.translator.text(Message::MoveDestinationState)))
                                    .child(self.translator.text(if safe { Message::MoveCandidateVerified } else { Message::MoveCandidateUnavailable }).into_owned())
                            })
                        );
                        detail = detail.when(retryable_count == 0, |d| {
                            d.child(div().text_size(px(11.0)).text_color(palette.text_muted)
                                .child(self.translator.text(Message::MoveInspectionUnavailable).into_owned()))
                        });
                        if opened && retryable_count > 0 {
                            confirmation_row = Some(div().w_full().flex().justify_end().px(px(10.0)).py(px(4.0))
                                .child(job_progress_button(
                                    ("operation-move-retry-verified", index),
                                    self.translator.text(Message::MoveRetryVerified).into_owned(),
                                    palette,
                                )
                                    .debug_selector(|| "operation-move-confirm-action".to_owned())
                                    .track_focus(&move_confirm_focus)
                                    .focusable()
                                    .when(move_confirm_focus.is_focused(window), |style| style.border_color(palette.text))
                                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                                        if event.standard_click() { this.retry_verified_move_items(token, ticket, cx); }
                                    }))
                                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _window, cx| {
                                        if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                            cx.stop_propagation(); this.retry_verified_move_items(token, ticket, cx);
                                        }
                                    }))
                                ).into_any_element());
                        }
                        if opened {
                            // Keep the potentially tall inspection projection
                            // out of the same scroll child as its keyboard
                            // actions. ScrollHandle can then reveal each 44px
                            // action using its measured direct-child bounds.
                            expanded_detail = Some(detail.into_any_element());
                        }
                        row.child(review_button)
                    })
                    .when_some(cross_volume_move_inspection_state, |row, (ticket, result)| {
                        let Some(result) = result else {
                            return row.child(div().debug_selector(|| "operation-cross-volume-move-inspection-pending".to_owned())
                                .text_size(px(11.0)).text_color(palette.text_muted)
                                .child(self.translator.text(Message::MoveInspectionPending).into_owned()));
                        };
                        let guards = verified_cross_volume_move_guards.as_ref().map_or(0, Vec::len);
                        let mut detail = div().w_full().flex().flex_col().gap(px(4.0)).px(px(10.0)).py(px(5.0))
                            .text_size(px(11.0)).text_color(palette.text_muted)
                            .child(self.translator.text(Message::CrossVolumeMoveRetrySummary).into_owned())
                            .child(format!("{}: {guards}", self.translator.text(Message::FileOperationsEligibleItems)));
                        for candidate in &result.candidates {
                            use operations::model::GuardedMoveDestinationState;

                            let source = self.translator.text(if candidate.source_matches {
                                Message::MoveIdentityMatches
                            } else {
                                Message::FileOperationsIdentityUnverified
                            });
                            let parent = self.translator.text(if candidate.destination_parent_matches {
                                Message::MoveIdentityMatches
                            } else {
                                Message::FileOperationsIdentityUnverified
                            });
                            let destination = self.translator.text(match candidate.destination {
                                GuardedMoveDestinationState::Absent => Message::MoveDestinationAbsent,
                                GuardedMoveDestinationState::Present => Message::MoveDestinationPresent,
                                GuardedMoveDestinationState::Indeterminate => Message::MoveDestinationIndeterminate,
                            });
                            let state = format!("{} · {}: {source} · {}: {parent} · {}: {destination}",
                                candidate.proof.source.display(),
                                self.translator.text(Message::MoveSourceIdentity),
                                self.translator.text(Message::MoveDestinationParentIdentity),
                                self.translator.text(Message::MoveDestinationState));
                            detail = detail.child(div().child(state));
                        }
                        let token = result.attempt;
                        let action = job_progress_button(("operation-cross-volume-move-retry", index), self.translator.text(Message::CrossVolumeMoveRetryVerified).into_owned(), palette)
                            .debug_selector(|| "operation-cross-volume-move-retry-action".to_owned())
                            .track_focus(&move_confirm_focus).focusable()
                            .when(move_confirm_focus.is_focused(window), |style| style.border_color(palette.text))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                if event.standard_click() { this.retry_verified_move_items(token, ticket, cx); }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation(); this.retry_verified_move_items(token, ticket, cx);
                                }
                            }));
                        detail = detail.when(guards > 0, |d| d.child(action));
                        row.child(detail)
                    })
                    .when_some(published_move_inspection_state, |row, (ticket, result)| {
                        let Some(result) = result else {
                            return row.child(div().debug_selector(|| "operation-published-move-inspection-pending".to_owned())
                                .text_size(px(11.0)).text_color(palette.text_muted)
                                .child(self.translator.text(Message::MoveInspectionPending).into_owned()));
                        };
                        let review_button = job_progress_button(("operation-published-move-review", index), self.translator.text(Message::PublishedMoveReview).into_owned(), palette)
                            .debug_selector(|| "operation-published-move-review-action".to_owned())
                            .track_focus(&move_review_focus).focusable()
                            .when(move_review_focus.is_focused(window), |style| style.border_color(palette.text))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                if event.standard_click() {
                                    if !this.operation_move_review_open.remove(&job.id) { this.operation_move_review_open.insert(job.id); }
                                    cx.notify();
                                }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation();
                                    if !this.operation_move_review_open.remove(&job.id) { this.operation_move_review_open.insert(job.id); }
                                    cx.notify();
                                }
                            }));
                        if review_open {
                            let details = result.candidates.iter().map(|candidate| {
                                use operations::model::PublishedMoveInspectionState as State;
                                let source = self.translator.text(match candidate.source {
                                    State::Matches => Message::MoveIdentityMatches,
                                    State::Changed => Message::MoveIdentityMismatch,
                                    State::Absent => Message::MoveDestinationAbsent,
                                    State::Indeterminate => Message::MoveDestinationIndeterminate,
                                }).into_owned();
                                let parent = self.translator.text(match candidate.destination_parent {
                                    State::Matches => Message::MoveIdentityMatches,
                                    State::Changed => Message::MoveIdentityMismatch,
                                    State::Absent => Message::MoveDestinationAbsent,
                                    State::Indeterminate => Message::MoveDestinationIndeterminate,
                                }).into_owned();
                                let destination = self.translator.text(match candidate.destination {
                                    State::Matches => Message::PublishedMoveDestinationMatches,
                                    State::Changed => Message::PublishedMoveDestinationChanged,
                                    State::Absent => Message::PublishedMoveDestinationAbsent,
                                    State::Indeterminate => Message::PublishedMoveDestinationIndeterminate,
                                }).into_owned();
                                div().flex().flex_col().min_w_0().whitespace_normal().text_size(px(11.0))
                                    .child(self.translator.text(if result.synthesized_unknown { Message::PublishedMoveDispositionAccepted } else { Message::PublishedMoveSourceRetained }).into_owned())
                                    .child(format!("{} → {}", candidate.proof.source.display(), candidate.proof.destination.display()))
                                    .child(format!("{}: {source}", self.translator.text(Message::MoveSourceIdentity)))
                                    .child(format!("{}: {parent}", self.translator.text(Message::MoveDestinationParentIdentity)))
                                    .child(format!("{}: {destination}", self.translator.text(Message::MoveDestinationState)))
                            });
                            expanded_detail = Some(div().debug_selector(|| "operation-published-move-inspection-detail".to_owned())
                                .flex().flex_col().gap(px(4.0)).px(px(6.0)).children(details).into_any_element());
                        }
                        let _ = ticket;
                        row.child(review_button)
                    })
                    .when(directory_move_review_available, |row| {
                        let opened = directory_move_review_open;
                        let button = job_progress_button(("operation-directory-move-review", index), self.translator.text(Message::DirectoryMoveReview).into_owned(), palette)
                            .debug_selector(|| "operation-directory-move-review-action".to_owned())
                            .track_focus(&move_review_focus)
                            .focusable()
                            .when(move_review_focus.is_focused(window), |style| style.border_color(palette.text))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                if event.standard_click() { this.toggle_directory_move_review(token, cx); }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified() && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation();
                                    this.toggle_directory_move_review(token, cx);
                                }
                            }));
                        if opened {
                            let detail = if let Some(operations::model::DirectoryMoveReviewState::Available(review)) = directory_move_review.as_ref() {
                                let label = |message| self.translator.text(message);
                                let no_record = label(Message::RecoveryBackupRootNotRecorded);
                                let observe = |state| label(operation_inspection_observation(state));
                                div()
                                    .debug_selector(|| "operation-directory-move-review-detail".to_owned())
                                    .flex().flex_col().gap(px(4.0)).px(px(6.0)).text_size(px(11.0))
                                    .child(div().debug_selector(|| "operation-directory-move-historical-label".to_owned())
                                        .child(label(Message::FileOperationsHistoricalMove).into_owned()))
                                    .child(format!("{} → {}", review.source.display(), review.destination.display()))
                                    .child(format!("{}: {} · {}: {}",
                                        label(Message::MoveSourceIdentity), review.source_root_identity.as_deref().unwrap_or(&no_record),
                                        label(Message::FileOperationsDestinationIdentity), review.destination_root_identity.as_deref().unwrap_or(&no_record)))
                                    .child(format!("{}: {}/{}", self.translator.text(Message::DirectoryMoveChildSummary), review.children_dispositions_recorded, review.children_total))
                                    .child(format!("{}: {}", self.translator.text(Message::DirectoryMoveRootSummary), self.translator.text(if review.root_disposition_recorded { Message::DirectoryMoveRecorded } else { Message::DirectoryMoveNotRecorded })))
                                    .child(div().debug_selector(|| "operation-directory-move-cleanup-label".to_owned()).child(format!("{}: {}", self.translator.text(Message::DirectoryMoveCleanupSummary), self.translator.text(if review.cleanup_complete { Message::DirectoryMoveRecorded } else { Message::DirectoryMoveNotRecorded }))))
                                    .child(format!("{}: {}", self.translator.text(Message::DirectoryMovePendingChild), self.translator.text(if review.pending_child { Message::DirectoryMovePending } else { Message::DirectoryMoveNotPending })))
                                    .child(format!("{}: {}", self.translator.text(Message::DirectoryMovePendingRoot), self.translator.text(if review.pending_root { Message::DirectoryMovePending } else { Message::DirectoryMoveNotPending })))
                                    .child(div().debug_selector(|| "operation-directory-move-fresh-roots".to_owned())
                                        .child(label(Message::FileOperationsRootOnlyObservations).into_owned()))
                                    .child(div().debug_selector(|| "operation-directory-move-fresh-source-root".to_owned())
                                        .child(format!("{}: {}", label(Message::RecoverySourceRootNow), observe(review.source_root_observation))))
                                    .child(div().debug_selector(|| "operation-directory-move-fresh-destination-root".to_owned())
                                        .child(format!("{}: {}", label(Message::RecoveryDestinationRootNow), observe(review.destination_root_observation))))
                                    .when_some(review.replace.as_ref(), |detail, replace| detail
                                        .child(div().debug_selector(|| "operation-directory-move-replace-historical-label".to_owned())
                                            .child(label(Message::FileOperationsHistoricalReplace).into_owned()))
                                        .child(format!("{}: {:?}", label(Message::RecoveryMovePhase), replace.phase))
                                        .child(format!("{}: {} · {}: {}",
                                            label(Message::RecoveryMoveStagePath), replace.stage.display(),
                                            label(Message::RecoveryBackupPath), replace.backup.display()))
                                        .child(format!("{}: {} · {}: {}",
                                            label(Message::FileOperationsStageIdentity), replace.stage_identity.as_deref().unwrap_or(&no_record),
                                            label(Message::FileOperationsDisplacedIdentity), replace.displaced_identity.as_deref().unwrap_or(&no_record)))
                                        .child(format!("{}: {}", label(Message::RecoveryBackupRootDisposal), label(if replace.backup_root_disposition_recorded { Message::DirectoryMoveRecorded } else { Message::DirectoryMoveNotRecorded })))
                                        .child(format!("{}: {}", label(Message::FileOperationsBackupCleanup), label(if replace.backup_cleanup_complete { Message::DirectoryMoveRecorded } else { Message::DirectoryMoveNotRecorded })))
                                        .child(div().debug_selector(|| "operation-directory-move-replace-backup-children-historical".to_owned())
                                            .child(format!("{}: {}/{}", label(Message::FileOperationsBackupChildReceipts), replace.backup_children_dispositions_recorded, replace.backup_children_total)))
                                        .child(div().debug_selector(|| "operation-directory-move-replace-pending-backup-historical".to_owned())
                                            .child(format!("{}: {}", label(Message::FileOperationsPendingBackupIntent), label(if replace.pending_backup_intent { Message::DirectoryMovePending } else { Message::DirectoryMoveNotPending }))))
                                        .child(div().debug_selector(|| "operation-directory-move-fresh-stage-root".to_owned())
                                            .child(format!("{}: {}", label(Message::FileOperationsStageRootNow), observe(review.stage_root_observation))))
                                        .child(div().debug_selector(|| "operation-directory-move-fresh-backup-root".to_owned())
                                            .child(format!("{}: {}", label(Message::FileOperationsBackupRootNow), observe(review.backup_root_observation)))))
                            } else if matches!(directory_move_review, Some(operations::model::DirectoryMoveReviewState::Unavailable)) {
                                div().debug_selector(|| "operation-directory-move-review-unavailable".to_owned())
                                    .text_size(px(11.0)).text_color(palette.text_muted)
                                    .child(self.translator.text(Message::DirectoryMoveUnavailable).into_owned())
                            } else {
                                div().debug_selector(|| "operation-directory-move-review-pending".to_owned())
                                    .text_size(px(11.0)).text_color(palette.text_muted)
                                    .child(self.translator.text(Message::MoveInspectionPending).into_owned())
                            };
                            expanded_detail = Some(detail.into_any_element());
                        }
                        row.child(button)
                    })
                    .when_some(copy_inspection_state, |row, (ticket, result)| {
                        let Some(result) = result else {
                            return row.child(
                                div()
                                    .debug_selector(|| "operation-copy-inspection-pending".to_owned())
                                    .text_size(px(11.0))
                                    .text_color(palette.text_muted)
                                    .child(self.translator.text(Message::CopyInspectionPending).into_owned()),
                            );
                        };
                        let review_token = result.attempt;
                        let review_key = (review_token, ticket);
                        let opened = copy_review_open;
                        let Some(focus) = copy_review_focus.as_ref() else {
                            return row;
                        };
                        let review = job_progress_button(
                            ("operation-copy-review", index),
                            self.translator.text(Message::CopyInspectionReview).into_owned(),
                            palette,
                        )
                            .debug_selector(|| "operation-copy-review-action".to_owned())
                            .track_focus(focus)
                            .focusable()
                            .when(focus.is_focused(window), |style| style.border_color(palette.text))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                if event.standard_click() {
                                    this.toggle_copy_failure_review(
                                        review_key.0,
                                        review_key.1,
                                        cx,
                                    );
                                }
                            }))
                            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified()
                                    && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                                {
                                    cx.stop_propagation();
                                    this.toggle_copy_failure_review(review_token, ticket, cx);
                                }
                            }));
                        if opened {
                            let mut details =
                                div()
                                    .id(("operation-copy-inspection-detail", index))
                                    .debug_selector(|| "operation-copy-inspection-detail".to_owned())
                                    .max_h(px(body_max_height.min(240.0)))
                                    .overflow_y_scroll()
                                    .track_scroll(copy_review_scroll.as_ref().unwrap())
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .px(px(6.0))
                                    .children(result.candidates.iter().enumerate().map(|(candidate_index, candidate)| {
                                        use operations::model::{
                                            CopyInspectionState as State,
                                            CopyPublicationState as Publication,
                                        };
                                        let publication = match candidate.provenance.publication {
                                            Publication::NotPublished => Message::CopyPublicationNotPublished,
                                            Publication::Published => Message::CopyPublicationPublished,
                                            Publication::Unknown => Message::CopyPublicationUnknown,
                                        };
                                        let publication_selector = match candidate.provenance.publication {
                                            Publication::NotPublished => "operation-copy-publication-not-published",
                                            Publication::Published => "operation-copy-publication-published",
                                            Publication::Unknown => "operation-copy-publication-unknown",
                                        };
                                        let content_selector = match candidate.stage_content {
                                            State::Matches => "operation-copy-content-matches",
                                            State::Changed => "operation-copy-content-changed",
                                            State::Absent => "operation-copy-content-absent",
                                            State::Indeterminate => "operation-copy-content-indeterminate",
                                        };
                                        let candidate_item = candidate.provenance.item;
                                        let cleanup_key = (token, ticket, candidate_item);
                                        let cleanup_scroll = self
                                            .operation_copy_cleanup_scroll
                                            .entry(cleanup_key)
                                            .or_insert_with(gpui::ScrollHandle::new)
                                            .clone();
                                        let observation = |state| self.translator.text(operation_inspection_observation(state));
                                        let cleanup_eligible = verified_copy_cleanup_item_set.contains(&candidate_item);
                                        let mut details = div()
                                            .id(("operation-copy-cleanup-candidate", candidate_item.0))
                                            .debug_selector(move || format!("operation-copy-cleanup-candidate-{}", candidate_item.0))
                                            .max_h(px(if cleanup_eligible { 72.0 } else { 240.0 }))
                                            .overflow_y_scroll()
                                            .track_scroll(&cleanup_scroll)
                                            .flex()
                                            .flex_col()
                                            .min_w_0()
                                            .whitespace_normal()
                                            .text_size(px(11.0))
                                            .child(format!(
                                                "{} → {}",
                                                candidate.provenance.source.display(),
                                                candidate.provenance.destination.display(),
                                            ));
                                        if cleanup_eligible {
                                            if let Some(stage_path) = candidate.provenance.stage.as_ref().map(|stage| stage.path.display().to_string()) {
                                                let cleanup_focus = self
                                                    .operation_copy_cleanup_focus
                                                    .entry(cleanup_key)
                                                    .or_insert_with(|| cx.focus_handle())
                                                    .clone();
                                                let path_focus = self
                                                    .operation_copy_cleanup_path_focus
                                                    .entry(cleanup_key)
                                                    .or_insert_with(|| cx.focus_handle())
                                                    .clone();
                                                details = details
                                                    .child(div()
                                                        .id(("operation-copy-cleanup-explanation", candidate_item.0))
                                                        .debug_selector(move || format!("operation-copy-cleanup-explanation-{}", candidate_item.0))
                                                        .min_w_0()
                                                        .whitespace_normal()
                                                        .child(self.translator.text(Message::CopyCleanupExplanation).into_owned()))
                                                    .child(div()
                                                        .id(("operation-copy-cleanup-path", candidate_item.0))
                                                        .debug_selector(move || format!("operation-copy-cleanup-path-{}", candidate_item.0))
                                                        .track_focus(&path_focus)
                                                        .focusable()
                                                        .min_w_0()
                                                        .whitespace_normal()
                                                        .child(format!("{}: {stage_path}", self.translator.text(Message::CopyCleanupStagePath)))
                                                        .on_key_down(cx.listener(move |_this, event: &gpui::KeyDownEvent, window, cx| {
                                                            if event.keystroke.key.as_str() == "tab"
                                                                && !event.keystroke.modifiers.modified()
                                                            {
                                                                cx.stop_propagation();
                                                                cleanup_focus.focus(window, cx);
                                                            }
                                                        })));
                                            }
                                            let cleanup_focus = self
                                                .operation_copy_cleanup_focus
                                                .get(&cleanup_key)
                                                .expect("eligible cleanup path installs its action focus")
                                                .clone();
                                            let path_focus = self.operation_copy_cleanup_path_focus.get(&cleanup_key);
                                            if cleanup_focus.is_focused(window)
                                                || path_focus.is_some_and(|focus| focus.is_focused(window))
                                            {
                                                copy_review_scroll.as_ref().unwrap().scroll_to_item(candidate_index);
                                                self.operation_history_scroll.scroll_to_item(row_index + 1);
                                            }
                                            if let Some(cleanup_scroll) = self.operation_copy_cleanup_scroll.get(&cleanup_key) {
                                                if path_focus.is_some_and(|focus| focus.is_focused(window)) {
                                                    cleanup_scroll.scroll_to_item(1);
                                                } else if cleanup_focus.is_focused(window) {
                                                    cleanup_scroll.scroll_to_item(3);
                                                }
                                            }
                                            let cleanup_token = token;
                                            let cleanup_ticket = ticket;
                                            let cleanup_item = candidate_item;
                                            details = details.child(
                                                job_progress_button(
                                                    ("operation-copy-cleanup", candidate_item.0),
                                                    self.translator.text(Message::CopyInspectionDeleteStage).into_owned(),
                                                    palette,
                                                )
                                                    .debug_selector(move || format!("operation-copy-cleanup-action-{}", cleanup_item.0))
                                                    .track_focus(&cleanup_focus)
                                                    .focusable()
                                                    .when(cleanup_focus.is_focused(window), |style| style.border_color(palette.text))
                                                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                                        if event.standard_click() {
                                                            this.cleanup_verified_copy_stage(cleanup_token, cleanup_ticket, cleanup_item, cx);
                                                        }
                                                    }))
                                                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                                                        if !event.keystroke.modifiers.modified()
                                                            && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
                                                        {
                                                            cx.stop_propagation();
                                                            this.cleanup_verified_copy_stage(cleanup_token, cleanup_ticket, cleanup_item, cx);
                                                        }
                                                    })),
                                            );
                                        }
                                        details = details
                                            .child(div()
                                                .debug_selector(move || publication_selector.to_owned())
                                                .child(format!(
                                                    "{}: {}",
                                                    self.translator.text(Message::CopyPublicationHistory),
                                                    self.translator.text(publication),
                                                )))
                                            .child(format!(
                                                "{}: {}",
                                                self.translator.text(Message::CopyObservationSource),
                                                observation(candidate.source),
                                            ))
                                            .child(format!(
                                                "{}: {}",
                                                self.translator.text(Message::CopyObservationParent),
                                                observation(candidate.destination_parent),
                                            ))
                                            .child(format!(
                                                "{}: {}",
                                                self.translator.text(Message::CopyObservationDestination),
                                                observation(candidate.destination),
                                            ))
                                            .child(format!(
                                                "{}: {}",
                                                self.translator.text(Message::CopyObservationStage),
                                                observation(candidate.stage),
                                            ))
                                            .child(div()
                                                .debug_selector(move || content_selector.to_owned())
                                                .child(format!(
                                                    "{}: {}",
                                                    self.translator.text(Message::CopyObservationStageContent),
                                                    observation(candidate.stage_content),
                                                )));
                                        details
                                    }))
                                    .children(result.directory_candidates.iter().map(|candidate| {
                                        use operations::model::CopyDirectoryInspectionEvidence as Evidence;

                                        let label = |message| self.translator.text(message);
                                        let no_record = label(Message::RecoveryBackupRootNotRecorded);
                                        let observe = |state| label(operation_inspection_observation(state));
                                        let (stage, destination, facts) = match &candidate.evidence {
                                            Evidence::Publication(e) => (
                                                String::from_utf16_lossy(&e.stage_path_units),
                                                String::from_utf16_lossy(&e.destination_path_units),
                                                vec![
                                                    format!("{}: {:?}", label(Message::FileOperationsRecordedPublicationPhase), e.phase),
                                                    format!("{}: {}", label(Message::FileOperationsReason), e.detail),
                                                    format!("{}: {}", label(Message::FileOperationsExpectedRootIdentity), e.expected_root_identity),
                                                    format!("{}: {}", label(Message::FileOperationsExpectedNodeCount), e.expected_node_count),
                                                    format!("{}: {}", label(Message::FileOperationsExpectedTreeCommitment), e.expected_inventory_sha256),
                                                    format!("{}: {}", label(Message::FileOperationsPublishedRootIdentity), e.observed_root_identity.as_deref().unwrap_or(&no_record)),
                                                    format!("{}: {}", label(Message::FileOperationsPublishedNodeCount), e.observed_node_count.map_or_else(|| no_record.to_string(), |count| count.to_string())),
                                                    format!("{}: {}", label(Message::FileOperationsPublishedTreeCommitment), e.observed_inventory_sha256.as_deref().unwrap_or(&no_record)),
                                                ],
                                            ),
                                            Evidence::RetainedStage(e) => (
                                                String::from_utf16_lossy(&e.stage_path_units),
                                                String::from_utf16_lossy(&e.destination_path_units),
                                                vec![
                                                    format!("{}: {:?}", label(Message::FileOperationsRecordedStagePhase), e.phase),
                                                    format!("{}: {}", label(Message::FileOperationsReason), e.detail),
                                                    format!("{}: {}", label(Message::FileOperationsCreationRootIdentity), e.expected_root_identity),
                                                ],
                                            ),
                                        };
                                        div().debug_selector(|| "operation-copy-directory-evidence".to_owned())
                                            .flex().flex_col().gap(px(2.0)).min_w_0().whitespace_normal()
                                            .text_size(px(11.0)).text_color(palette.text_muted)
                                            .children(facts.into_iter().map(|fact| div().child(fact)))
                                            .child(format!("{}: {stage}", label(Message::FileOperationsRecordedStagePath)))
                                            .child(format!("{}: {destination}", label(Message::RecoveryRecordedDestination)))
                                            .child(format!("{}: {}", label(Message::FileOperationsStageRootNow), observe(candidate.stage_root)))
                                            .child(format!("{}: {}", label(Message::RecoveryDestinationRootNow), observe(candidate.destination_root)))
                                            .child(label(Message::FileOperationsRootMatchCaveat).into_owned())
                                    }));
                            if copy_retry_visible {
                                let retry_focus = copy_retry_focus.as_ref().unwrap().clone();
                                let retry_token = token;
                                let retry_ticket = ticket;
                                details = details.child(
                                    job_progress_button(
                                        ("operation-copy-retry", index),
                                        self.translator.text(Message::CopyInspectionRetry).into_owned(),
                                        palette,
                                    )
                                        .debug_selector(|| "operation-copy-retry-action".to_owned())
                                        .track_focus(&retry_focus)
                                        .focusable()
                                        .when(retry_focus.is_focused(window), |style| style.border_color(palette.text))
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, _, cx| {
                                                if event.standard_click() {
                                                    this.retry_verified_copy_items(
                                                        retry_token,
                                                        retry_ticket,
                                                        cx,
                                                    );
                                                }
                                            },
                                        ))
                                        .on_key_down(cx.listener(
                                            move |this, event: &gpui::KeyDownEvent, _, cx| {
                                                if !event.keystroke.modifiers.modified()
                                                    && matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space" | " "
                                                    )
                                                {
                                                    cx.stop_propagation();
                                                    this.retry_verified_copy_items(
                                                        retry_token,
                                                        retry_ticket,
                                                        cx,
                                                    );
                                                }
                                            },
                                        )),
                                );
                            }
                            copy_expanded_detail = Some(details.into_any_element());
                        }
                        row.child(review)
                    })
                    .into_any_element(),
            );
            if let Some(detail) = expanded_detail {
                rows.push(detail);
            }
            if let Some(detail) = copy_expanded_detail {
                rows.push(detail);
            }
            if let Some(confirmation) = confirmation_row {
                rows.push(confirmation);
            }
        }
        Some(
            div()
                .id("operation-history")
                .debug_selector(|| "operation-history-panel".to_owned())
                .absolute()
                .left(px(12.0))
                .right(px(12.0))
                .bottom(px(12.0))
                .max_h(px(panel_max_height))
                .rounded(px(RADIUS_MD))
                .border_1()
                .border_color(palette.text.opacity(0.16))
                .bg(palette.surface_raised)
                .text_color(palette.text)
                .shadow_lg()
                .occlude()
                .overflow_hidden()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(header_height))
                        .px(px(16.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(divider)
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(self.translator.text(Message::FileOperationsTitle).into_owned()),
                        )
                        .child(
                            job_progress_button(
                                "operation-history-close",
                                self.translator.text(Message::JobProgressClose).into_owned(),
                                palette,
                            )
                                .debug_selector(|| "operation-history-close-action".to_owned())
                                .track_focus(&self.operation_history_close_focus)
                                .focusable()
                                .when(
                                    self.operation_history_close_focus.is_focused(window),
                                    |style| style.border_color(palette.text),
                                )
                                .on_click(cx.listener(
                                    |this, event: &gpui::ClickEvent, window, cx| {
                                        if event.standard_click() {
                                            this.operation_progress_open = false;
                                            this.operation_details_focus.focus(window, cx);
                                            cx.notify();
                                        }
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, window, cx| {
                                        if !event.keystroke.modifiers.modified()
                                            && matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space" | " "
                                            )
                                        {
                                            cx.stop_propagation();
                                            this.operation_progress_open = false;
                                            this.operation_details_focus.focus(window, cx);
                                            cx.notify();
                                        }
                                    },
                                )),
                        ),
                )
                .child(
                    div()
                        .id("operation-history-rows")
                        .debug_selector(|| "operation-history-rows".to_owned())
                        .max_h(px(body_max_height))
                        .overflow_y_scroll()
                        .track_scroll(&self.operation_history_scroll)
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

fn job_progress_button(
    id: impl Into<gpui::ElementId>,
    label: String,
    palette: gpui_filer_ui::ThemePalette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .min_w(px(44.0))
        .h(px(44.0))
        .px(px(6.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(RADIUS_SM))
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(palette.text)
        .hover(|style| style.bg(palette.chrome_hover))
        .focus_visible(|style| style.border_color(palette.text))
        .child(label)
}

fn job_progress_kind(kind: OperationKind) -> Message {
    match kind {
        OperationKind::Copy => Message::RecoveryKindCopy,
        OperationKind::Move => Message::RecoveryKindMove,
        OperationKind::CreateSymbolicLink => Message::RecoveryKindSymbolicLink,
        OperationKind::CreateJunction => Message::RecoveryKindJunction,
        OperationKind::CreateFolder => Message::RecoveryKindNewFolder,
        OperationKind::CreateFile => Message::RecoveryKindNewFile,
        OperationKind::CreateShortcut => Message::RecoveryKindCreateShortcut,
        OperationKind::CreateZip => Message::RecoveryKindCreateZip,
        OperationKind::Create7z => Message::RecoveryKindCreate7z,
        OperationKind::ExtractZip => Message::RecoveryKindExtractZip,
        OperationKind::Extract7z => Message::JobProgressExtract7z,
        OperationKind::Trash => Message::RecoveryKindTrash,
        OperationKind::Rename => Message::RecoveryKindRename,
        OperationKind::BatchRename => Message::RecoveryKindBatchRename,
        OperationKind::BatchRenameUndo => Message::RecoveryKindBatchRenameUndo,
        OperationKind::ExternalMoveCleanup => Message::JobProgressExternalMoveCleanup,
        OperationKind::CopyStageCleanup => Message::JobProgressCopyStageCleanup,
    }
}

fn operation_inspection_observation(state: operations::model::CopyInspectionState) -> Message {
    use operations::model::CopyInspectionState;

    match state {
        CopyInspectionState::Matches => Message::CopyObservationMatches,
        CopyInspectionState::Changed => Message::CopyObservationChanged,
        CopyInspectionState::Absent => Message::CopyObservationAbsent,
        CopyInspectionState::Indeterminate => Message::CopyObservationIndeterminate,
    }
}

fn render_job_progress_row_label(
    row: &operations::model::ProgressRowSnapshot,
    translator: &Translator,
    palette: gpui_filer_ui::ThemePalette,
    cx: &gpui::App,
) -> gpui::Div {
    use operations::model::{AttemptState, TerminalOutcome};

    let kind = job_progress_kind(row.kind);
    let (status, marker) = match row.terminal_outcome {
        Some(TerminalOutcome::Failed) => (Message::RecoveryOutcomeFailed, palette.error_text),
        Some(TerminalOutcome::Partial) => (Message::RecoveryOutcomePartial, palette.warning_text),
        Some(TerminalOutcome::Unknown) => (Message::RecoveryOutcomeUnknown, palette.warning_text),
        Some(TerminalOutcome::Succeeded) => (Message::RecoveryOutcomeSucceeded, palette.text_muted),
        Some(TerminalOutcome::Cancelled) => (Message::RecoveryOutcomeCancelled, palette.text_muted),
        None => match row.state {
            AttemptState::Queued => (Message::JobProgressQueued, palette.accent_hover),
            AttemptState::Preparing => (Message::JobProgressPreparing, palette.accent_hover),
            AttemptState::Running => (Message::JobProgressRunning, palette.accent_hover),
            AttemptState::WaitingConflict => {
                (Message::JobProgressWaitingConflict, palette.warning_text)
            }
            AttemptState::CancelRequested => {
                (Message::JobProgressCancelRequested, palette.accent_hover)
            }
            AttemptState::Terminal => (Message::RecoveryOutcomeUnknown, palette.warning_text),
        },
    };
    let progress = match row.terminal_outcome {
        Some(TerminalOutcome::Failed | TerminalOutcome::Partial | TerminalOutcome::Unknown) => cx
            .global::<operations::application::OperationApplicationOwner>()
            .runtime
            .current_terminal_receipts(row.token)
            .and_then(|receipts| operation_history_failure(receipts, translator))
            .map(|(_, reason)| reason),
        Some(TerminalOutcome::Succeeded) => {
            Some(translator.text(Message::FileOperationsCompleted).into_owned())
        }
        Some(TerminalOutcome::Cancelled) => {
            Some(translator.text(Message::FileOperationsCancelled).into_owned())
        }
        None => row.counters.map(|counters| {
            translator.job_progress_counters(
                counters.processed_items,
                &format_operation_bytes(counters.processed_bytes),
            )
        }).or_else(|| match row.state {
            AttemptState::Queued | AttemptState::Preparing => {
                Some(translator.text(Message::JobProgressWaiting).into_owned())
            }
            AttemptState::Running => Some(format!("{}…", translator.text(status))),
            _ => None,
        }),
    };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(6.0))
        .text_size(px(12.0))
        .child(
            div()
                .mt(px(5.0))
                .size(px(7.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(marker),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .child(format!("{} · {}", translator.text(kind), translator.text(status)))
                .children(progress.map(|text| div().text_color(palette.text_muted).child(text))),
        )
}

fn render_operation_history_row(
    request: &OperationRequest,
    attempt: &operations::model::AttemptSnapshot,
    translator: &Translator,
    palette: gpui_filer_ui::ThemePalette,
) -> gpui::Div {
    use operations::model::TerminalOutcome;

    let failure = attempt
        .terminal
        .as_ref()
        .and_then(|report| operation_history_failure(&report.receipts, translator));
    let item = failure.as_ref().map(|(item, _)| *item);
    let kind = translator.text(match request.kind {
        OperationKind::Rename => Message::FileOperationsRename,
        kind => job_progress_kind(kind),
    });
    let title = operation_history_item_name(request, attempt, item)
        .map_or_else(|| kind.to_string(), |name| format!("{kind}: {name}"));
    let (status, icon, color) = match attempt.state {
        AttemptState::Terminal => match attempt.terminal.as_ref().map(|report| report.outcome) {
            Some(TerminalOutcome::Succeeded) => (
                Message::FileOperationsCompleted,
                IconId::OperationCompleted,
                palette.text_muted,
            ),
            Some(TerminalOutcome::Cancelled) => (
                Message::FileOperationsCancelled,
                IconId::OperationCancelled,
                palette.text_muted,
            ),
            Some(TerminalOutcome::Partial) => (
                Message::RecoveryOutcomePartial,
                IconId::Warning,
                palette.warning_text,
            ),
            Some(TerminalOutcome::Failed) => (
                Message::RecoveryOutcomeFailed,
                IconId::OperationFailed,
                palette.error_text,
            ),
            Some(TerminalOutcome::Unknown) | None => (
                Message::FileOperationsRecoveryRequired,
                IconId::Warning,
                palette.warning_text,
            ),
        },
        AttemptState::WaitingConflict => (
            Message::JobProgressWaitingConflict,
            IconId::Warning,
            palette.warning_text,
        ),
        state => (
            match state {
                AttemptState::Queued => Message::JobProgressQueued,
                AttemptState::Preparing => Message::JobProgressPreparing,
                AttemptState::CancelRequested => Message::JobProgressCancelRequested,
                _ => Message::JobProgressRunning,
            },
            IconId::OperationInProgress,
            palette.accent_hover,
        ),
    };
    let status = translator.text(status);
    let secondary = failure.map_or_else(
        || status.to_string(),
        |(_, reason)| format!("{status} · {reason}"),
    );
    let counters = attempt
        .terminal
        .as_ref()
        .map(|report| report.final_counters)
        .or_else(|| attempt.progress.as_ref().map(|progress| progress.counters));

    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px(10.0))
        .child(
            div()
                .mt(px(2.0))
                .flex_shrink_0()
                .child(Icon::new(icon).color(color.into())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(palette.text_muted)
                        .line_clamp(2)
                        .child(secondary),
                ),
        )
        .when_some(
            counters
                .and_then(|counters| operation_history_bytes(request.kind, counters, translator)),
            |row, bytes| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .self_center()
                        .text_size(px(12.0))
                        .text_color(palette.text_muted)
                        .child(bytes),
                )
            },
        )
}

fn operation_history_item_name<'a>(
    request: &'a OperationRequest,
    attempt: &'a operations::model::AttemptSnapshot,
    failed_item: Option<operations::model::ItemId>,
) -> Option<Cow<'a, str>> {
    use operations::model::ItemOutcome;

    if matches!(
        request.kind,
        OperationKind::CreateFile | OperationKind::CreateFolder
    ) {
        let receipt = attempt
            .terminal
            .as_ref()?
            .receipts
            .iter()
            .find(|receipt| receipt.outcome == ItemOutcome::Completed)?;
        return match request.kind {
            OperationKind::CreateFile => receipt.detail.as_deref().map(Cow::Borrowed),
            _ => receipt
                .created_folder_path
                .as_deref()?
                .file_name()
                .map(|name| name.to_string_lossy()),
        };
    }
    let index = failed_item.map_or(0, |item| item.0 as usize);
    if let Some(source) = request
        .sources
        .get(index)
        .or_else(|| request.sources.first())
    {
        return Some(
            source
                .file_name()
                .unwrap_or(source.as_os_str())
                .to_string_lossy(),
        );
    }
    let items = &request.virtual_source.as_ref()?.items;
    items
        .get(index)
        .or_else(|| items.first())
        .map(|item| Cow::Borrowed(item.name.as_str()))
}

fn operation_history_failure(
    receipts: &[operations::model::ItemReceipt],
    translator: &Translator,
) -> Option<(operations::model::ItemId, String)> {
    use operations::model::{
        DIRECTORY_COPY_EVIDENCE_PREFIX, DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX, ItemOutcome,
    };

    let issues = receipts.iter().filter(|receipt| {
        matches!(
            receipt.outcome,
            ItemOutcome::Failed | ItemOutcome::Partial | ItemOutcome::Unknown
        )
    });
    let (item, detail) = issues.clone().find_map(|receipt| {
        receipt
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .map(|detail| (receipt.item, detail))
    })?;
    let reason = match detail {
        "DestinationExists"
        | "Io(\"DestinationExists\")"
        | "output already exists"
        | "output folder already exists" => translator.text(Message::ConflictHeading),
        "AccessDenied" => translator.text(Message::FileOperationsAccessDenied),
        "Unsupported" => translator.text(Message::FileOperationsUnsupported),
        _ => {
            // 記録の JSON は検査用。行には実行結果の説明だけを表示する。
            let detail = [
                DIRECTORY_COPY_EVIDENCE_PREFIX,
                DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX,
            ]
            .into_iter()
            .find_map(|prefix| {
                detail
                    .split_once(prefix)
                    .map(|(summary, _)| summary.trim_end_matches("; "))
            })
            .unwrap_or(detail);
            Cow::Borrowed(detail)
        }
    };
    let remaining = issues.count() - 1;
    let reason = if remaining > 0 {
        format!(
            "{reason} · {}",
            translator.other_operation_issues(remaining)
        )
    } else {
        reason.into_owned()
    };
    Some((item, reason))
}

fn operation_history_bytes(
    kind: OperationKind,
    counters: operations::model::ProgressCounters,
    translator: &Translator,
) -> Option<String> {
    if !matches!(
        kind,
        OperationKind::Copy
            | OperationKind::Move
            | OperationKind::Trash
            | OperationKind::ExtractZip
            | OperationKind::Extract7z
    ) {
        return None;
    }
    let processed = format_operation_bytes(counters.processed_bytes);
    if let Some(total) = counters.total_bytes {
        return Some(format!("{processed} / {}", format_operation_bytes(total)));
    }
    let known_bytes = counters.discovered_bytes.max(counters.processed_bytes);
    (known_bytes > 0).then(|| {
        format!(
            "{processed} / {} {}",
            format_operation_bytes(known_bytes),
            translator.text(Message::FileOperationsDiscovered)
        )
    })
}

#[cfg(test)]
mod operation_history_tests {
    use super::*;
    use crate::operations::model::{ItemId, ItemOutcome, ItemReceipt, ProgressCounters};
    use crate::shell::i18n::Locale;

    #[test]
    fn reports_the_localized_rename_collision_from_the_terminal_receipt() {
        let receipt = ItemReceipt {
            item: ItemId(0),
            outcome: ItemOutcome::Failed,
            detail: Some("DestinationExists".to_owned()),
            created_folder_path: None,
            created_archive_path: None,
            move_receipt: None,
            replace_receipt: None,
            copy_undo: None,
        };
        let ja = Translator::new(Locale::Japanese);
        assert_eq!(
            operation_history_failure(&[receipt], &ja),
            Some((ItemId(0), "同じ名前の項目が既にあります".to_owned())),
        );
    }

    #[test]
    fn only_measured_transfer_operations_show_bytes() {
        let ja = Translator::new(Locale::Japanese);
        let measured = ProgressCounters {
            processed_bytes: 10,
            discovered_bytes: 20,
            ..Default::default()
        };
        for kind in [
            OperationKind::Rename,
            OperationKind::CreateFile,
            OperationKind::CreateFolder,
        ] {
            assert_eq!(operation_history_bytes(kind, measured, &ja), None);
        }
        assert_eq!(
            operation_history_bytes(OperationKind::Trash, ProgressCounters::default(), &ja),
            None
        );
        assert_eq!(
            operation_history_bytes(OperationKind::Copy, measured, &ja).as_deref(),
            Some("10 B / 20 B 検出済み"),
        );
        assert_eq!(
            operation_history_bytes(
                OperationKind::ExtractZip,
                ProgressCounters {
                    total_bytes: Some(30),
                    ..measured
                },
                &ja
            )
            .as_deref(),
            Some("10 B / 30 B"),
        );
    }
}

fn render_operation_speed_graph(
    history: Vec<OperationSpeedSample>,
    current_progress_ratio: f32,
) -> gpui::Div {
    let current_progress_ratio = current_progress_ratio.clamp(0.0, 1.0);
    let mut samples: Vec<_> = history
        .into_iter()
        .filter(|sample| sample.progress_ratio <= current_progress_ratio)
        .collect();
    if let Some(first) = samples.first().copied()
        && first.progress_ratio > 0.0
    {
        samples.insert(
            0,
            OperationSpeedSample {
                progress_ratio: 0.0,
                bytes_per_second: first.bytes_per_second,
            },
        );
    }
    if let Some(last) = samples.last().copied()
        && current_progress_ratio > last.progress_ratio
    {
        samples.push(OperationSpeedSample {
            progress_ratio: current_progress_ratio,
            bytes_per_second: last.bytes_per_second,
        });
    }
    let max_value = samples
        .iter()
        .map(|sample| sample.bytes_per_second)
        .fold(1.0_f64, f64::max);

    gpui::div()
        .w_full()
        .min_w_0()
        .mt(px(6.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(0x202420))
        .overflow_hidden()
        .child(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let left = bounds.origin.x + px(6.0);
                    let right = bounds.origin.x + bounds.size.width - px(6.0);
                    let top = bounds.origin.y + px(4.0);
                    let bottom = bounds.origin.y + bounds.size.height - px(4.0);
                    let width = (right - left).max(px(1.0));
                    let height = (bottom - top).max(px(1.0));

                    let mut baseline = gpui::PathBuilder::stroke(px(1.0));
                    baseline.move_to(gpui::point(left, bottom));
                    baseline.line_to(gpui::point(right, bottom));
                    if let Ok(path) = baseline.build() {
                        window.paint_path(path, gpui::rgb(0x3A463A));
                    }
                    let mut right_edge = gpui::PathBuilder::stroke(px(1.0));
                    right_edge.move_to(gpui::point(right, top));
                    right_edge.line_to(gpui::point(right, bottom));
                    if let Ok(path) = right_edge.build() {
                        window.paint_path(path, gpui::rgb(0x4C5C4C));
                    }
                    if samples.len() < 2 {
                        return;
                    }
                    let points: Vec<_> = samples
                        .iter()
                        .map(|sample| {
                            let x = left + width * sample.progress_ratio.clamp(0.0, 1.0);
                            let ratio =
                                (sample.bytes_per_second / max_value).clamp(0.0, 1.0) as f32;
                            gpui::point(x, bottom - height * ratio)
                        })
                        .collect();
                    let mut line = gpui::PathBuilder::stroke(px(1.5));
                    for (index, current) in points.iter().copied().enumerate() {
                        if index == 0 {
                            line.move_to(current);
                        } else {
                            let previous = points[index - 1];
                            let control_dx = (current.x - previous.x) * 0.5;
                            line.cubic_bezier_to(
                                current,
                                gpui::point(previous.x + control_dx, previous.y),
                                gpui::point(current.x - control_dx, current.y),
                            );
                        }
                    }
                    if let Ok(path) = line.build() {
                        window.paint_path(path, gpui::rgb(0x86D37D));
                    }
                },
            )
            .w_full()
            .min_w_0()
            .h(px(40.0)),
        )
        .child(
            gpui::div()
                .w_full()
                .min_w_0()
                .h(px(12.0))
                .flex()
                .items_center()
                .justify_between()
                .px(px(6.0))
                .text_size(px(10.0))
                .text_color(gpui::rgb(0x7E8A7E))
                .child("0%")
                .child("100%"),
        )
}
