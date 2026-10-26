use super::*;

/// Everything は directory session の入力ではない。検索の request / 結果 / 選択は
/// Shell だけが持ち、activation の瞬間に current tab へ immutable path を渡す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchPhase {
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SearchError {
    ProviderChanged,
    Provider(String),
}

#[derive(Clone)]
pub(super) struct SearchRequest {
    pub(super) id: u64,
    pub(super) query: String,
    pub(super) cancellation: Arc<AtomicBool>,
    pub(super) provider_revision: everything::ProviderRevision,
    #[cfg(test)]
    pub(super) test_command: Option<everything::TestSearchCommand>,
    #[cfg(test)]
    pub(super) test_outcome: Option<everything::SearchOutcome>,
}

pub(super) struct SearchUi {
    pub(super) input: gpui::Entity<rename_editor::RenameInput>,
    _input_events: gpui::Subscription,
    next_request_id: u64,
    #[cfg(test)]
    next_test_command: Option<everything::TestSearchCommand>,
    #[cfg(test)]
    test_completion_ack: Option<async_channel::Sender<()>>,
    /// 表示へ commit できる唯一の最新request。実行中/待機中とは別に持ち、旧結果を
    /// directory stateへ混ぜずに捨てるための authority になる。
    desired: Option<SearchRequest>,
    /// semaphore待ちを含めて実体taskを持つのは一つだけ。古いrequestがIPCを止められ
    /// なくても、入力連打で待機taskが無限に積まれない。
    running: Option<SearchRequest>,
    /// running中に届いた最新の一つだけを残す。runningのterminal callbackがこれを
    /// launchしてから次のslotを待つ。
    pending: Option<SearchRequest>,
    /// `results` と同じ query の世代。可視 index を click callback に捕捉しない。
    visible_request_id: Option<u64>,
    phase: SearchPhase,
    error: Option<SearchError>,
    results: Vec<everything::SearchResult>,
    selected: Option<usize>,
    pub(super) focus_on_render: bool,
}

pub(super) fn same_search_identity(
    current: Option<&SearchRequest>,
    candidate: &SearchRequest,
) -> bool {
    current.is_some_and(|current| {
        current.id == candidate.id
            && current.query == candidate.query
            && Arc::ptr_eq(&current.cancellation, &candidate.cancellation)
    })
}

pub(super) fn same_search_request(
    current: Option<&SearchRequest>,
    candidate: &SearchRequest,
) -> bool {
    same_search_identity(current, candidate)
        && current.is_some_and(|current| current.provider_revision == candidate.provider_revision)
}

pub(super) fn search_request_is_current(
    current: Option<&SearchRequest>,
    candidate: &SearchRequest,
) -> bool {
    same_search_request(current, candidate)
        && candidate.provider_revision == everything::ProviderRevision::current()
        && !candidate.cancellation.load(Ordering::Acquire)
}

impl FilerShell {
    #[cfg(test)]
    pub(super) fn set_next_search_command_for_test(
        &mut self,
        command: everything::TestSearchCommand,
    ) {
        let search = self.search.as_mut().expect("test Search overlay is open");
        assert!(
            search.next_test_command.is_none(),
            "test command already set"
        );
        search.next_test_command = Some(command);
    }

    #[cfg(test)]
    pub(super) fn set_search_completion_ack_for_test(&mut self, ack: async_channel::Sender<()>) {
        let search = self.search.as_mut().expect("test Search overlay is open");
        assert!(
            search.test_completion_ack.is_none(),
            "completion ack already set"
        );
        search.test_completion_ack = Some(ack);
    }

    #[cfg(test)]
    pub(super) fn start_search_request_for_test(
        &mut self,
        request: SearchRequest,
        cx: &mut Context<Self>,
    ) {
        if self.search.is_none() {
            self.open_search(cx);
        }
        let search = self.search.as_mut().expect("test search overlay opened");
        search.desired = Some(request.clone());
        search.running = Some(request.clone());
        search.phase = SearchPhase::Loading;
        self.launch_search_request(request, cx);
    }

    #[cfg(test)]
    pub(super) fn replace_search_request_for_test(&mut self, request: SearchRequest) {
        let search = self.search.as_mut().expect("test search overlay is open");
        Self::cancel_search_request(search);
        search.desired = Some(request.clone());
        search.pending = Some(request);
        search.phase = SearchPhase::Loading;
    }

    #[cfg(test)]
    pub(super) fn search_request_state_for_test(
        &self,
    ) -> (
        bool,
        Option<u64>,
        Option<u64>,
        Option<u64>,
        &'static str,
        Option<String>,
        usize,
    ) {
        self.search
            .as_ref()
            .map_or((false, None, None, None, "absent", None, 0), |search| {
                (
                    true,
                    search.running.as_ref().map(|request| request.id),
                    search.desired.as_ref().map(|request| request.id),
                    search.pending.as_ref().map(|request| request.id),
                    match search.phase {
                        SearchPhase::Idle => "idle",
                        SearchPhase::Loading => "loading",
                        SearchPhase::Ready => "ready",
                        SearchPhase::Failed => "failed",
                    },
                    search.error.as_ref().map(|error| match error {
                        SearchError::ProviderChanged => "provider changed".to_owned(),
                        SearchError::Provider(message) => message.clone(),
                    }),
                    search.results.len(),
                )
            })
    }

    #[cfg(test)]
    pub(super) fn search_results_for_test(&self) -> Vec<everything::SearchResult> {
        self.search
            .as_ref()
            .map_or_else(Vec::new, |search| search.results.clone())
    }

    #[cfg(test)]
    pub(super) fn search_input_state_for_test(
        &self,
        cx: &gpui::App,
    ) -> Option<(String, &'static str)> {
        self.search.as_ref().map(|search| {
            (
                search.input.read(cx).text().to_owned(),
                match search.phase {
                    SearchPhase::Idle => "idle",
                    SearchPhase::Loading => "loading",
                    SearchPhase::Ready => "ready",
                    SearchPhase::Failed => "failed",
                },
            )
        })
    }

    #[cfg(test)]
    pub(super) fn search_input_for_test(&self) -> Option<gpui::Entity<rename_editor::RenameInput>> {
        self.search.as_ref().map(|search| search.input.clone())
    }

    #[cfg(test)]
    pub(super) fn search_cancellation_for_test(&self) -> Option<Arc<AtomicBool>> {
        self.search
            .as_ref()?
            .running
            .as_ref()
            .map(|request| request.cancellation.clone())
    }

    #[cfg(test)]
    pub(super) fn has_pending_test_search_command(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|search| search.next_test_command.is_some())
    }

    /// Everything overlay を開く。conflict の選択を隠してしまわないよう、そちらが
    /// active の間は開かない。入力欄の focus は overlay が layout された次の render
    /// で一度だけ渡す。
    pub(super) fn open_search(&mut self, cx: &mut Context<Self>) {
        let policy = overlays::open_policy(overlays::OverlayKind::Search, self.open_overlays(cx));
        if !policy.allowed {
            return;
        }
        if policy.close.menu {
            self.leave_menu(cx);
        }
        if policy.close.folder_tree {
            self.close_folder_tree_menu(cx);
        }
        if let Some(search) = self.search.as_mut() {
            search.focus_on_render = true;
            cx.notify();
            return;
        }
        let input = cx.new(|cx| rename_editor::RenameInput::new(cx, String::new(), false, 0..0));
        let input_events = cx.subscribe(&input, |this, _, event, cx| match event {
            rename_editor::RenameInputEvent::TextChanged => this.search_text_changed(cx),
            rename_editor::RenameInputEvent::CopyPlainText(text) => {
                this.write_text_clipboard(text.clone(), cx);
            }
        });
        self.search = Some(SearchUi {
            input,
            _input_events: input_events,
            next_request_id: 0,
            #[cfg(test)]
            next_test_command: None,
            #[cfg(test)]
            test_completion_ack: None,
            desired: None,
            running: None,
            pending: None,
            visible_request_id: None,
            phase: SearchPhase::Idle,
            error: None,
            results: Vec::new(),
            selected: None,
            focus_on_render: true,
        });
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    /// 次の query を開始する前に、queue 待ちか完了待ちの request を無効にする。running は
    /// terminal callback が lane を解放するまで残し、pending task は作らず値だけ捨てる。
    /// Everything 自身の同期IPCを途中停止はできないので、返ってきた値は ticket の照合で捨てる。
    pub(super) fn cancel_search_request(search: &mut SearchUi) {
        for request in [&search.desired, &search.running, &search.pending]
            .into_iter()
            .flatten()
        {
            request.cancellation.store(true, Ordering::Release);
        }
        search.desired = None;
        search.pending = None;
    }

    /// 実 task は searchごとに最大一つだけ作る。semaphore待ちもこの一つなので、入力を
    /// 連打しても無制限の cancelled waiter / captured query を残さない。
    pub(super) fn launch_search_request(&mut self, request: SearchRequest, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        #[cfg(test)]
        let completion_ack = self
            .search
            .as_mut()
            .and_then(|search| search.test_completion_ack.take());
        cx.spawn(async move |_this, cx| {
            let query = request.query.clone();
            let cancellation = request.cancellation.clone();
            let background_executor = cx.background_executor().clone();
            #[cfg(test)]
            let test_command = request.test_command.clone();
            #[cfg(test)]
            let test_outcome = request.test_outcome.clone();
            let result = if request.cancellation.load(Ordering::Acquire) {
                None
            } else {
                Some(
                    cx.background_executor()
                        .spawn(async move {
                            #[cfg(test)]
                            if let Some(outcome) = test_outcome {
                                return outcome;
                            }
                            #[cfg(test)]
                            if let Some(test_command) = test_command {
                                return everything::search_with_test_command(
                                    background_executor,
                                    query,
                                    cancellation,
                                    request.provider_revision,
                                    test_command,
                                )
                                .await;
                            }
                            everything::search(
                                background_executor,
                                query,
                                cancellation,
                                request.provider_revision,
                            )
                            .await
                        })
                        .await,
                )
            };
            // Cancelled waiter も必ずここまで戻る。ここを early-returnするとrunning lane
            // が永久に残り、最新pending queryを開始できなくなる。
            let _ = shell.update(cx, |shell, cx| {
                let Some(search) = shell.search.as_mut() else {
                    return;
                };
                if !same_search_request(search.running.as_ref(), &request) {
                    return;
                }
                search.running = None;
                // 成功値は provider epoch も一致していなければ表示しない。一方、
                // unavailable/timeout は provider 自身が epoch を進めるため、同じ
                // request の terminal error まで stale として消してはいけない。
                let current = same_search_request(search.desired.as_ref(), &request)
                    && !request.cancellation.load(Ordering::Acquire);
                if current {
                    search.desired = None;
                    match result.expect("current request was not cancelled") {
                        outcome
                            if outcome.terminal == everything::ProviderTerminal::Completed
                                && outcome.revision == everything::ProviderRevision::current() =>
                        {
                            search.visible_request_id = Some(request.id);
                            // adapter / provider のどちらが壊れても overlay の上限は守る。
                            search.results = outcome
                                .results
                                .into_iter()
                                .take(everything::SEARCH_MAX_RESULTS as usize)
                                .collect();
                            search.selected = (!search.results.is_empty()).then_some(0);
                            search.phase = SearchPhase::Ready;
                        }
                        outcome if outcome.terminal == everything::ProviderTerminal::Completed => {
                            // 別の provider request が epoch を進めた場合、成功値は stale。
                            // それを結果として表示せず、current query を明示的な失敗で終端する。
                            search.visible_request_id = None;
                            search.results.clear();
                            search.selected = None;
                            search.phase = SearchPhase::Failed;
                            search.error = Some(SearchError::ProviderChanged);
                        }
                        outcome => {
                            search.visible_request_id = None;
                            search.results.clear();
                            search.selected = None;
                            search.phase = SearchPhase::Failed;
                            search.error = outcome.error.map(SearchError::Provider);
                        }
                    }
                }
                let next = search.pending.take().and_then(|candidate| {
                    if search_request_is_current(search.desired.as_ref(), &candidate) {
                        return Some(candidate);
                    }
                    if !same_search_identity(search.desired.as_ref(), &candidate)
                        || candidate.cancellation.load(Ordering::Acquire)
                    {
                        return None;
                    }
                    let current_revision = everything::ProviderRevision::current();
                    if candidate.provider_revision == current_revision {
                        Some(candidate)
                    } else {
                        // provider failure中に入力された最新queryは、実行されていないので、
                        // 新しいepochを捕捉してそのまま一度だけ実行する。
                        let refreshed = SearchRequest {
                            id: candidate.id,
                            query: candidate.query,
                            cancellation: Arc::new(AtomicBool::new(false)),
                            provider_revision: current_revision,
                            #[cfg(test)]
                            test_command: candidate.test_command,
                            #[cfg(test)]
                            test_outcome: candidate.test_outcome,
                        };
                        search.desired = Some(refreshed.clone());
                        Some(refreshed)
                    }
                });
                if let Some(next) = next {
                    search.running = Some(next.clone());
                    shell.launch_search_request(next, cx);
                }
                cx.notify();
            });
            // Test acknowledgement means Shell's detached completion callback has returned.
            #[cfg(test)]
            if let Some(ack) = completion_ack {
                let _ = ack.try_send(());
            }
        })
        .detach();
    }

    pub(super) fn search_text_changed(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        // TSF preedit では provider を叩かない。確定 replacement が届いたイベントだけが
        // search request を作るので、IME中の候補移動を file-list のキーへ漏らさない。
        if search.input.read(cx).is_composing() {
            return;
        }
        let query = search.input.read(cx).text().trim().to_owned();
        Self::cancel_search_request(search);
        search.visible_request_id = None;
        search.results.clear();
        search.selected = None;
        search.error = None;
        if query.is_empty() {
            search.phase = SearchPhase::Idle;
            cx.notify();
            return;
        }
        search.next_request_id = search.next_request_id.wrapping_add(1);
        let request = SearchRequest {
            id: search.next_request_id,
            query,
            cancellation: Arc::new(AtomicBool::new(false)),
            provider_revision: everything::ProviderRevision::current(),
            #[cfg(test)]
            test_command: search.next_test_command.take(),
            #[cfg(test)]
            test_outcome: None,
        };
        search.phase = SearchPhase::Loading;
        search.desired = Some(request.clone());
        let launch_now = if search.running.is_none() {
            search.running = Some(request.clone());
            Some(request)
        } else {
            search.pending = Some(request);
            None
        };
        cx.notify();
        if let Some(request) = launch_now {
            self.launch_search_request(request, cx);
        }
    }

    pub(super) fn close_search(&mut self, cx: &mut Context<Self>) {
        let Some(mut search) = self.search.take() else {
            return;
        };
        Self::cancel_search_request(&mut search);
        // overlay の EntityInputHandler を落とした次の render で active list へ戻す。
        // Conflict への昇格中は、直後に conflict_focus が実 focus を取るので予約しない。
        if self.active_conflict(cx).is_none() {
            self.pending_focus = true;
        }
        self.sync_overlay_modal(cx);
        cx.notify();
    }

    pub(super) fn activate_search_result(
        &mut self,
        request_id: u64,
        result: everything::SearchResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let valid = self.search.as_ref().is_some_and(|search| {
            search.visible_request_id == Some(request_id)
                && search.results.iter().any(|candidate| candidate == &result)
        });
        if !valid {
            return;
        }
        // 先に modal を外し、activation 時点の active tab を読む。検索開始時の tab id
        // を捕捉しないので、その間に切替えても正しい view に届く。
        self.close_search(cx);
        if let Some(view) = self.active_view() {
            view.update(cx, |view, cx| {
                view.activate_search_result(result.path, result.is_folder, window, cx)
            });
        }
    }

    /// Text input は実 focus を維持しつつ、検索結果だけに logical highlight を持つ。
    /// IME composition 中の Enter/Escape/矢印は input system へ渡す。
    pub(super) fn handle_search_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(search) = self.search.as_ref() else {
            return false;
        };
        if search.input.read(cx).is_composing() {
            return false;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.close_search(cx);
                true
            }
            "up" | "down" => {
                let delta = if keystroke.key == "up" { -1_isize } else { 1 };
                if let Some(search) = self.search.as_mut()
                    && !search.results.is_empty()
                {
                    let current = search.selected.unwrap_or(0) as isize;
                    search.selected =
                        Some((current + delta).rem_euclid(search.results.len() as isize) as usize);
                    cx.notify();
                }
                true
            }
            "enter" => {
                let activation = self.search.as_ref().and_then(|search| {
                    let index = search.selected?;
                    Some((
                        search.visible_request_id?,
                        search.results.get(index)?.clone(),
                    ))
                });
                if let Some((request_id, result)) = activation {
                    self.activate_search_result(request_id, result, window, cx);
                }
                true
            }
            // modal の外へ届くと directory binding が動くキーは、text input の action
            // ではない限りここで消費する。通常文字や編集キーは RenameInput に任せる。
            "tab" | "f2" | "delete" | "pageup" | "pagedown" | "home" | "end" => true,
            _ => false,
        }
    }

    pub(super) fn render_search_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let search = self.search.as_ref()?;
        let request_id = search.visible_request_id;
        let status = match search.phase {
            SearchPhase::Idle => self
                .translator
                .text(super::i18n::Message::SearchTypeToSearch)
                .into_owned(),
            SearchPhase::Loading => self
                .translator
                .text(super::i18n::Message::SearchSearching)
                .into_owned(),
            SearchPhase::Ready => self.translator.search_result_count(search.results.len()),
            SearchPhase::Failed => match search.error.as_ref() {
                Some(SearchError::ProviderChanged) => self
                    .translator
                    .text(super::i18n::Message::SearchProviderChanged)
                    .into_owned(),
                Some(SearchError::Provider(error)) => error.clone(),
                None => self
                    .translator
                    .text(super::i18n::Message::SearchFailed)
                    .into_owned(),
            },
        };
        let mut rows = div().flex().flex_col().w_full();
        for (index, result) in search.results.iter().cloned().enumerate() {
            let selected = search.selected == Some(index);
            let row_request_id = request_id.expect("visible search results have a request id");
            let path_text = result.path.display().to_string();
            let size = result.size.map(format_operation_bytes).unwrap_or_default();
            let activation_result = result.clone();
            rows = rows.child(
                div()
                    .id(("everything-result", index as u64))
                    .w_full()
                    .min_h(px(44.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .when(selected, |row| {
                        row.bg(gpui_filer_ui::theme_palette(cx).chrome_hover)
                    })
                    .hover(|row| row.bg(gpui_filer_ui::theme_palette(cx).chrome_hover))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            if event.standard_click() {
                                this.activate_search_result(
                                    row_request_id,
                                    activation_result.clone(),
                                    window,
                                    cx,
                                );
                            }
                        }),
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
                                    .truncate()
                                    .text_color(gpui_filer_ui::theme_palette(cx).text)
                                    .child(result.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.0))
                                    .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                                    .child(path_text),
                            ),
                    )
                    .when(!size.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                                .child(size),
                        )
                    }),
            );
        }
        let show_guidance = search.results.is_empty()
            && matches!(search.phase, SearchPhase::Idle | SearchPhase::Ready);
        let results = if show_guidance {
            rows.child(
                div()
                    .px(px(14.0))
                    .py(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                    .child(
                        self.translator
                            .text(super::i18n::Message::SearchUsage)
                            .into_owned(),
                    )
                    .child(
                        self.translator
                            .text(super::i18n::Message::SearchExamples)
                            .into_owned(),
                    )
                    .child(
                        self.translator
                            .text(super::i18n::Message::SearchActivationHint)
                            .into_owned(),
                    ),
            )
        } else {
            rows
        };
        let input = search.input.clone();
        Some(
            div()
                .id("everything-search-overlay")
                .absolute()
                .size_full()
                .occlude()
                // backdrop は click-through させない。閉じるのは Escape だけにして、
                // result を選ぶ途中の誤クリックで query を失わないようにする。
                .on_mouse_down(gpui::MouseButton::Left, |_event, _window, _cx| {})
                .child(div().absolute().size_full().bg(gpui::rgba(0x00000088)))
                .child(
                    div()
                        .size_full()
                        .flex()
                        .justify_center()
                        .items_start()
                        .pt(px(72.0))
                        .child(
                            div()
                                .id("everything-search-card")
                                .w(px(640.0))
                                .max_w(px(640.0))
                                .flex()
                                .flex_col()
                                .overflow_hidden()
                                .rounded(px(RADIUS_MD))
                                .border_1()
                                .border_color(gpui_filer_ui::theme_palette(cx).border)
                                .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                                .child(
                                    div()
                                        .h(px(42.0))
                                        .px(px(14.0))
                                        .flex()
                                        .items_center()
                                        .text_color(gpui_filer_ui::theme_palette(cx).text)
                                        .child(
                                            self.translator
                                                .text(super::i18n::Message::SearchTitle)
                                                .into_owned(),
                                        ),
                                )
                                .child(
                                    div()
                                        .mx(px(12.0))
                                        .mb(px(8.0))
                                        .h(px(42.0))
                                        .px(px(10.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(RADIUS_SM))
                                        .border_1()
                                        .border_color(gpui_filer_ui::theme_palette(cx).border)
                                        .text_color(gpui_filer_ui::theme_palette(cx).text)
                                        .child(input),
                                )
                                .child(
                                    div()
                                        .h(px(28.0))
                                        .px(px(14.0))
                                        .flex()
                                        .items_center()
                                        .text_size(px(12.0))
                                        .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                                        .child(status),
                                )
                                .child(
                                    div()
                                        .id("everything-search-results")
                                        .max_h(px(352.0))
                                        .overflow_y_scroll()
                                        .border_t_1()
                                        .border_color(gpui_filer_ui::theme_palette(cx).border)
                                        .child(results),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
