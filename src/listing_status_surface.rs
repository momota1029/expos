use gpui::{
    Context, InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::FluentBuilder as _, px,
};

use super::{
    ActionId, CHROME_FONT_SIZE, DirectoryListDemo, Location, Message, RADIUS_SM, commands,
    directory_session, input,
};

/// last-good を表示できる refresh は一覧の flow を動かさない。
/// 初回 loading と失敗は従来どおり状態行を使う。
pub(super) fn show_listing_status_row(
    status: &directory_session::ListingStatus,
    has_last_good: bool,
) -> bool {
    !matches!(status, directory_session::ListingStatus::Refreshing) || !has_last_good
}

impl DirectoryListDemo {
    /// 一覧の状態行に出す文字列。表示するものが無ければ `None`。
    ///
    /// 読取失敗と watch 失敗が同時に起きるときは読取失敗を優先する。フォルダが
    /// 無いなら「読み込み失敗」が本題で、watch が張れないのはその結果だから。
    fn listing_status_text(&self) -> Option<String> {
        // PC の行は背景で解決する。届くまでだけ状態を出す（読取失敗も watch も無い）。
        if self.is_this_pc() {
            return (!self.pc_loaded).then(|| self.translator.text(Message::Loading).into_owned());
        }
        if matches!(
            self.session.location(),
            Some(Location::ShellRoot(_) | Location::ShellItem { .. })
        ) {
            return self
                .library_error
                .clone()
                .map(|error| format!("{}: {error}", self.translator.text(Message::LoadFailed)))
                .or_else(|| {
                    (!self.library_loaded)
                        .then(|| self.translator.text(Message::Loading).into_owned())
                });
        }
        let status = self.session.status();
        let has_last_good = self.session.listing().is_some();
        let read = show_listing_status_row(status, has_last_good)
            .then(|| {
                self.translator.listing_status(
                    status.is_loading(),
                    status.error().map(|error| error.0.as_str()),
                    has_last_good,
                )
            })
            .flatten();
        if read.is_some() {
            return read;
        }
        // 読取が成功しているときだけ、自動更新が無いことを出す。
        match self.session.watch_state() {
            directory_session::WatchState::Degraded(reason) => {
                Some(self.translator.watch_degraded(reason))
            }
            _ => None,
        }
    }

    pub(super) fn render_listing_status(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let status = self.listing_status_text()?;
        let failed = self.session.status().error().is_some();
        Some(
            div()
                .px(px(12.0))
                .py(px(4.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(CHROME_FONT_SIZE))
                .text_color(if failed {
                    gpui_filer_ui::theme_palette(cx).warning_text
                } else {
                    gpui_filer_ui::theme_palette(cx).text_muted
                })
                .child(status)
                // 失敗中は再試行を出す。F5 と同じ入口を通す。
                .when(failed, |row| {
                    row.child(
                        div()
                            .id("retry-listing")
                            .h(px(28.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .rounded(px(RADIUS_SM))
                            .bg(gpui_filer_ui::theme_palette(cx).chrome_hover)
                            .hover(|style| {
                                style.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong)
                            })
                            .child(self.translator.text(Message::Retry).into_owned())
                            .on_click(cx.listener(
                                |this, event: &gpui::ClickEvent, _window, cx| {
                                    if !event.standard_click() {
                                        return;
                                    }
                                    // 描画から時間が経っている。実行の直前にもう一度見る。
                                    if !commands::available(
                                        ActionId::new(input::REFRESH),
                                        &this.command_context(),
                                        false,
                                    ) {
                                        return;
                                    }
                                    this.refresh_listing(cx);
                                },
                            )),
                    )
                }),
        )
    }
}
