use gpui::{
    App, Hsla, IntoElement, ParentElement as _, Pixels, RenderOnce, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, svg,
};
use gpui_filer_resources::{FLUENT_ICON_FONT, IconId, IconSource};
mod color_icon;
mod folder_preview;
pub use folder_preview::FolderPreview;
mod navigation;
pub use navigation::{
    BreadcrumbIntent, Breadcrumbs, ReopenTabsButton, TabDirectoryDragPayload, TabIntent, TabLabel,
    TabReorderInteraction, VerticalTabs, VerticalTabsText, vertical_tab_scroll_child_index,
};
mod scrollbar;
pub use scrollbar::{STRIP_HEIGHT as SCROLLBAR_STRIP_HEIGHT, horizontal_scrollbar};
mod theme;
pub use theme::{
    ACCENT, ACCENT_HOVER, BORDER, CHROME_HOVER, CHROME_HOVER_STRONG, CLOSE_HOVER, FOCUS_BG,
    HOVER_BG, ON_ACCENT, RADIUS_MD, RADIUS_SM, SCROLLBAR_THUMB, SCROLLBAR_THUMB_HOVER,
    SELECTION_BG, SELECTION_FOCUS_BG, SELECTION_HOVER_BG, SURFACE_BAR, SURFACE_BASE,
    SURFACE_HEADER, SURFACE_RAISED, TEXT, TEXT_ICON, TEXT_MUTED, ThemePalette, theme_palette,
};

type CheckboxListener = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>;

/// チェックボックスのhit領域。タッチ用の最小値で、外側のレイアウトもこれを基準にする。
pub const CHECKBOX_HIT_SIZE: f32 = 44.0;

/// チェックボックスの見える箱を、44pxのhit領域のどこへ寄せるか。
///
/// 行表示では左端に他項目が無いので中央でよい。グリッドでは隣の項目の内容と
/// 左右が同じ距離になって帰属が読めなくなるため、自分の内容側へ寄せる。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckboxAlign {
    /// hit領域の中央（一覧の行）
    Center,
    /// hit領域の右端（グリッド。箱を右隣の内容へ寄せ、空白は左へ逃がす）
    Trailing,
}

/// 行とタイルで共用するチェックボックス。選択状態は親が所有する。
#[derive(IntoElement)]
pub struct SelectionCheckbox {
    id: gpui::ElementId,
    checked: bool,
    indeterminate: bool,
    align: CheckboxAlign,
    on_toggle: CheckboxListener,
}

impl SelectionCheckbox {
    pub fn new(
        id: impl Into<gpui::ElementId>,
        checked: bool,
        on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            checked,
            indeterminate: false,
            align: CheckboxAlign::Center,
            on_toggle: Box::new(on_toggle),
        }
    }

    /// 箱をhit領域の右端へ寄せる。空白を項目の外側（左側）へ集める。
    pub fn trailing(mut self) -> Self {
        self.align = CheckboxAlign::Trailing;
        self
    }

    /// 集合の一部だけが選ばれている状態。通常の checkbox と同じ hit 領域・箱を使い、
    /// section heading だけ別物に見えないようにする。
    pub fn indeterminate(mut self, indeterminate: bool) -> Self {
        self.indeterminate = indeterminate;
        self
    }
}

impl RenderOnce for SelectionCheckbox {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
        let palette = theme_palette(cx);
        let mut mark = div()
            .size(px(18.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .border_1()
            .border_color(if self.checked || self.indeterminate {
                palette.accent
            } else {
                palette.text_muted
            })
            .bg(if self.checked || self.indeterminate {
                palette.accent
            } else {
                palette.surface_raised
            });
        if self.checked {
            mark = mark.child(
                Icon::new(IconId::Check)
                    .size(px(12.0))
                    .color(palette.on_accent.into()),
            );
        } else if self.indeterminate {
            mark = mark.child(
                div()
                    .w(px(10.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(palette.on_accent),
            );
        }
        div()
            .id(self.id)
            .size(px(CHECKBOX_HIT_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .when(self.align == CheckboxAlign::Trailing, |hit| {
                hit.justify_end()
            })
            .when(self.align == CheckboxAlign::Center, |hit| {
                hit.justify_center()
            })
            .child(mark)
            .on_click(move |event, window, cx| {
                // 親の単一選択で、この反転結果を上書きしないようにする。
                cx.stop_propagation();
                if event.standard_click() {
                    (self.on_toggle)(event, window, cx);
                }
            })
    }
}

#[derive(IntoElement)]
pub struct Icon {
    id: IconId,
    size: Pixels,
    color: Hsla,
}

impl Icon {
    pub fn new(id: IconId) -> Self {
        Self {
            id,
            size: px(16.0),
            color: gpui::black(),
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 単色SVG・文字アイコンの色。ColorSvgは元の配色を保つため、この色を適用しない。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let source = self.id.definition().source;
        let container = div()
            .size(self.size)
            .flex()
            .items_center()
            .justify_center()
            .text_color(self.color)
            .flex_shrink_0();

        match source {
            IconSource::ColorSvg(_) => {
                let id = self.id;
                let logical_size = self.size;
                container
                    .child(
                        gpui::img(move |window: &mut Window, cx: &mut App| {
                            let physical_size = (logical_size.as_f32() * window.scale_factor())
                                .ceil()
                                .max(1.0) as u32;
                            window.use_asset::<color_icon::ColorIcon>(&(id, physical_size), cx)
                        })
                        .size(self.size),
                    )
                    .into_any_element()
            }
            IconSource::FluentGlyph(glyph) => container
                .font_family(FLUENT_ICON_FONT)
                .text_size(self.size)
                .line_height(self.size)
                .child(glyph.to_string())
                .into_any_element(),
            IconSource::TextGlyph(glyph) => container
                .text_size(self.size)
                .line_height(self.size)
                .child(glyph.to_string())
                .into_any_element(),
            IconSource::Svg(path) => container
                .child(svg().path(path).size(self.size).text_color(self.color))
                .into_any_element(),
        }
    }
}
