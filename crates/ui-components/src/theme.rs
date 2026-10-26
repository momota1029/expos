//! 見た目の値の置き場。
//!
//! ここは「今の見た目」を1箇所に集めるだけで、テーマの仕組みではない。設定から
//! 読めるようにするのは、実際に変えたい値が2つ以上出てきたあとでよい。
//! そのときはこのモジュールの中身を差し替えるだけで済むようにしておく。
//!
//! 選択色とアクセントカラーは意図的に別の定数にしてある。エクスプローラーのように
//! 「選択は灰色、アクセントは青」という組み合わせを許したいため。

use gpui::{App, Global, Rgba};

/// 角丸。小さい部品は `SM`、アイコンを囲む容器は `MD` を使う。
pub const RADIUS_SM: f32 = 4.0;
pub const RADIUS_MD: f32 = 6.0;

/// `gpui::rgb` は `const fn` ではないので、定数から使える形をここに置く。
const fn hex(value: u32) -> Rgba {
    let [_, r, g, b] = value.to_be_bytes();
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// 選択中の項目の背景。アクセントカラーとは独立に選べる。
pub const SELECTION_BG: Rgba = hex(0x10325c);
/// 選択中の項目にフォーカスが重なったときの背景。
pub const SELECTION_FOCUS_BG: Rgba = hex(0x1a4a80);
/// 選択中の項目をホバーしたときの背景。
pub const SELECTION_HOVER_BG: Rgba = hex(0x194575);
/// フォーカスだけがある項目の背景。選択とは別の場所を示す。
pub const FOCUS_BG: Rgba = hex(0x2f3742);
/// ホバー中の項目の背景。
pub const HOVER_BG: Rgba = hex(0x242a32);

/// UI のアクセントカラー。選択中のタブやチェックボックスの縁に使う。
/// チェックボックスの塗りと同じ値にして、選択まわりの青を1つに揃える。
pub const ACCENT: Rgba = hex(0x2463a0);
/// アクセントカラーを敷いたものをホバーしたときの色。
///
/// 灰色へ落とすと選択中なのかホバー中なのか読めなくなるので、同じ色相のまま
/// 少し明るくする。
pub const ACCENT_HOVER: Rgba = hex(0x2f78bd);
/// アクセントカラーを背景に敷いたときの文字色。
///
/// 背景の明るさから自動で決めるのは、実際に色を選べるようになってからにする。
pub const ON_ACCENT: Rgba = hex(0xffffff);

// ここから下は UI のクローム（パンくず、タブバー、ヘッダ、一覧の下地）。
// 彩度を持たせず純灰の階調にしてある。アクセントカラーだけが色を持つことで、
// 選択や選択中のタブが浮き上がって見える。

/// 一覧の下地。最も暗い。
pub const SURFACE_BASE: Rgba = hex(0x121212);
/// タブバーとステータスバー。
pub const SURFACE_RAISED: Rgba = hex(0x1a1a1a);
/// パンくず。
pub const SURFACE_BAR: Rgba = hex(0x1e1e1e);
/// 列ヘッダ。
pub const SURFACE_HEADER: Rgba = hex(0x262626);
/// クロームの区切り線。
pub const BORDER: Rgba = hex(0x333333);
/// クロームのホバー。
pub const CHROME_HOVER: Rgba = hex(0x2e2e2e);
/// クロームの強いホバー（パンくずのチップ、新規タブボタン）。
pub const CHROME_HOVER_STRONG: Rgba = hex(0x3a3a3a);
/// 閉じるボタンのホバー。v5 の `window_titlebar_close.hover`（palette の rose_900）と同じ。
///
/// ウィンドウボタンの中でここだけ色を持つ。他のクロームと同じ濃さのホバーにすると、
/// 「閉じる」だけが危険な操作だと読めないため。
pub const CLOSE_HOVER: Rgba = hex(0x7f1d1d);
/// クロームの副次的な文字。
pub const TEXT_MUTED: Rgba = hex(0xa0a0a0);
/// クロームのアイコン。
pub const TEXT_ICON: Rgba = hex(0x9a9a9a);
/// 主文字。
pub const TEXT: Rgba = hex(0xffffff);
/// スクロールバーのつまみ。
pub const SCROLLBAR_THUMB: Rgba = hex(0x8a8a8a);
/// スクロールバーのつまみのホバー。
pub const SCROLLBAR_THUMB_HOVER: Rgba = hex(0xbdbdbd);

/// Shell が選んだテーマから作る、全 UI が読む不変な色の投影。
#[derive(Clone, Copy)]
pub struct ThemePalette {
    pub selection_bg: Rgba,
    pub selection_focus_bg: Rgba,
    pub selection_hover_bg: Rgba,
    pub focus_bg: Rgba,
    pub hover_bg: Rgba,
    pub accent: Rgba,
    pub accent_hover: Rgba,
    pub on_accent: Rgba,
    pub surface_base: Rgba,
    pub surface_raised: Rgba,
    pub surface_bar: Rgba,
    pub surface_header: Rgba,
    pub border: Rgba,
    pub chrome_hover: Rgba,
    pub chrome_hover_strong: Rgba,
    pub close_hover: Rgba,
    pub text_muted: Rgba,
    pub text_icon: Rgba,
    pub text: Rgba,
    pub scrollbar_thumb: Rgba,
    pub scrollbar_thumb_hover: Rgba,
    pub warning_text: Rgba,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            selection_bg: SELECTION_BG,
            selection_focus_bg: SELECTION_FOCUS_BG,
            selection_hover_bg: SELECTION_HOVER_BG,
            focus_bg: FOCUS_BG,
            hover_bg: HOVER_BG,
            accent: ACCENT,
            accent_hover: ACCENT_HOVER,
            on_accent: ON_ACCENT,
            surface_base: SURFACE_BASE,
            surface_raised: SURFACE_RAISED,
            surface_bar: SURFACE_BAR,
            surface_header: SURFACE_HEADER,
            border: BORDER,
            chrome_hover: CHROME_HOVER,
            chrome_hover_strong: CHROME_HOVER_STRONG,
            close_hover: CLOSE_HOVER,
            text_muted: TEXT_MUTED,
            text_icon: TEXT_ICON,
            text: TEXT,
            scrollbar_thumb: SCROLLBAR_THUMB,
            scrollbar_thumb_hover: SCROLLBAR_THUMB_HOVER,
            warning_text: hex(0xd2b585),
        }
    }
}

impl Global for ThemePalette {}

/// App を読むだけの呼び出し側向け。Shell 以外からの書き込み API は公開しない。
pub fn theme_palette(app: &App) -> ThemePalette {
    app.try_global::<ThemePalette>()
        .copied()
        .unwrap_or_default()
}
