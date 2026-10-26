//! ユーザー設定テーマの読み込み境界。Shell は選択名だけを保持する。

use std::collections::HashMap;

use gpui::Rgba;
use gpui_filer_ui::ThemePalette;
use serde::Deserialize;

const DEFAULT_THEME: &str = "default";
const LIGHT_THEME: &str = "light";
const BUILTIN_LIGHT: &str = r##"
[palette]
gray_100 = "#1A1A1A"
gray_500 = "#A3A3A3"
gray_600 = "#C7C7C7"
gray_700 = "#D4D4D4"
gray_750 = "#E0E0E0"
gray_800 = "#E8E8E8"
gray_850 = "#F3F3F3"
gray_875 = "#F7F7F7"
gray_925 = "#FCFCFC"
gray_1000 = "#FFFFFF"
blue_900 = "#DBEAFE"
blue_800 = "#BFDBFE"
blue_500 = "#3B82F6"
blue_400 = "#2563EB"

[roles.chrome]
text = "palette.gray_100"
bg = "palette.gray_875"
border = "palette.gray_750"
[roles.chrome.hover]
bg = "palette.gray_800"
[roles.nav]
text = "palette.gray_100"
bg = "palette.gray_850"
border = "palette.gray_700"
[roles.nav.hover]
bg = "palette.gray_800"
[roles.content]
text = "palette.gray_100"
bg = "palette.gray_1000"
border = "palette.gray_750"
[roles.content.hover]
bg = "palette.gray_875"
[roles.content.selected]
bg = "palette.blue_900"
[roles.content.active]
bg = "palette.blue_800"
[roles.selected]
bg = "palette.blue_900"
[roles.accent]
text = "palette.gray_1000"
bg = "palette.blue_500"
border = "palette.blue_400"
[roles.accent.hover]
bg = "palette.blue_400"
[roles.muted]
text = "palette.gray_500"
[roles.warning]
text = "#D97706"

[components.file_item]
base = "content"
bg = "transparent"
border = "transparent"
[components.file_item.hover]
bg = "palette.gray_850"
[components.file_item.selected]
bg = "palette.blue_900"
[components.file_item.active]
bg = "palette.blue_800"
border = "palette.blue_500"
"##;

#[derive(Deserialize, Default)]
struct ThemeFile {
    #[serde(default)]
    palette: HashMap<String, String>,
    #[serde(default)]
    roles: HashMap<String, RoleFile>,
    #[serde(default)]
    components: HashMap<String, RoleFile>,
}

#[derive(Clone, Deserialize, Default)]
struct RoleFile {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    border: Option<String>,
    #[serde(default, flatten)]
    states: HashMap<String, RoleFile>,
}

/// `theme_name` は拡張子の無い basename。外部文字列を path として連結しない。
pub(super) fn safe_name(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':'])
        && !value.contains(['\0']))
    .then_some(value)
}

pub(super) fn directory() -> Option<std::path::PathBuf> {
    Some(crate::app_paths::root()?.join("themes"))
}

pub(super) fn list(selected: &str) -> Vec<String> {
    let Some(directory) = directory() else {
        return Vec::new();
    };
    let mut names = std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.is_file() && path.extension().is_some_and(|ext| ext == "toml"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .filter(|name| safe_name(name).is_some())
        .collect::<Vec<_>>();
    if !names.iter().any(|name| name == DEFAULT_THEME) {
        names.push(DEFAULT_THEME.into());
    }
    if !names.iter().any(|name| name == LIGHT_THEME) {
        names.push(LIGHT_THEME.into());
    }
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    names.dedup();
    names.sort_by_key(|name| (name != selected, name.to_lowercase(), name.clone()));
    names
}

pub(super) fn load(name: &str) -> ThemePalette {
    let Some(name) = safe_name(name) else {
        return ThemePalette::default();
    };
    if name == DEFAULT_THEME {
        return ThemePalette::default();
    }
    if let Some(directory) = directory()
        && let Ok(source) = std::fs::read_to_string(directory.join(format!("{name}.toml")))
        && let Some(palette) = parse(&source)
    {
        return palette;
    }
    if name == LIGHT_THEME {
        return parse(BUILTIN_LIGHT).unwrap_or_default();
    }
    ThemePalette::default()
}

fn parse(source: &str) -> Option<ThemePalette> {
    let theme: ThemeFile = toml::from_str(source).ok()?;
    let mut roles = theme.roles;
    roles.extend(theme.components.clone());
    let components = theme.components;
    let mut colors = HashMap::<String, Rgba>::new();
    let mut pending = theme.palette;
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|key, value| {
            let resolved = parse_color(value).or_else(|| {
                let reference = value.strip_prefix("palette.").unwrap_or(value);
                colors.get(reference).copied()
            });
            if let Some(color) = resolved {
                colors.insert(key.clone(), color);
                false
            } else {
                true
            }
        });
        if pending.len() == before {
            break;
        }
    }
    let mut result = ThemePalette::default();
    assign_role(&mut result, &roles, &colors, "chrome", "", |p, c| {
        p.surface_raised = c.bg;
        p.text = c.text;
        p.border = c.border;
    });
    assign_role(&mut result, &roles, &colors, "nav", "", |p, c| {
        p.surface_header = c.bg;
        p.text_icon = c.text;
    });
    assign_role(&mut result, &roles, &colors, "nav", "hover", |p, c| {
        p.chrome_hover_strong = c.bg;
    });
    assign_role(&mut result, &roles, &colors, "content", "", |p, c| {
        p.surface_base = c.bg;
        p.text = c.text;
        p.border = c.border;
    });
    assign_role(&mut result, &roles, &colors, "content", "hover", |p, c| {
        p.hover_bg = c.bg
    });
    assign_role(
        &mut result,
        &roles,
        &colors,
        "content",
        "selected",
        |p, c| p.selection_bg = c.bg,
    );
    assign_role(
        &mut result,
        &roles,
        &colors,
        "content",
        "selected.hover",
        |p, c| p.selection_hover_bg = c.bg,
    );
    assign_role(&mut result, &roles, &colors, "content", "active", |p, c| {
        p.selection_focus_bg = c.bg
    });
    assign_role(&mut result, &roles, &colors, "selected", "", |p, c| {
        p.selection_bg = c.bg
    });
    let file_item_hover = component_color(&components, &roles, &colors, "file_item", "hover", "bg");
    let file_item_selected =
        component_color(&components, &roles, &colors, "file_item", "selected", "bg");
    result.hover_bg = file_item_hover.unwrap_or(result.hover_bg);
    result.selection_bg = file_item_selected.unwrap_or(result.selection_bg);
    result.selection_focus_bg =
        component_color(&components, &roles, &colors, "file_item", "active", "bg")
            .unwrap_or(result.selection_focus_bg);
    // v6 に alt 専用 row token がないため、通常 item state が無い場合だけ共有 token を使う。
    if file_item_hover.is_none() {
        result.hover_bg =
            component_color(&components, &roles, &colors, "file_item_alt", "hover", "bg")
                .unwrap_or(result.hover_bg);
    }
    if file_item_selected.is_none() {
        result.selection_bg = component_color(
            &components,
            &roles,
            &colors,
            "file_item_alt",
            "selected",
            "bg",
        )
        .unwrap_or(result.selection_bg);
    }
    assign_role(&mut result, &roles, &colors, "accent", "", |p, c| {
        p.accent = c.bg;
        p.on_accent = c.text;
        p.border = c.border;
    });
    assign_role(&mut result, &roles, &colors, "accent", "hover", |p, c| {
        p.accent_hover = c.bg
    });
    assign_role(&mut result, &roles, &colors, "muted", "", |p, c| {
        p.text_muted = c.text
    });
    assign_role(&mut result, &roles, &colors, "warning", "", |p, c| {
        p.warning_text = c.text
    });
    assign_role(
        &mut result,
        &roles,
        &colors,
        "window_titlebar_close",
        "hover",
        |p, c| p.close_hover = c.bg,
    );
    result.surface_bar = result.surface_header;
    assign_role(&mut result, &roles, &colors, "chrome", "hover", |p, c| {
        p.chrome_hover = c.bg;
    });
    result.scrollbar_thumb = result.text_muted;
    result.scrollbar_thumb_hover = result.text;
    Some(result)
}

fn component_color(
    components: &HashMap<String, RoleFile>,
    roles: &HashMap<String, RoleFile>,
    colors: &HashMap<String, Rgba>,
    component: &str,
    state: &str,
    field: &str,
) -> Option<Rgba> {
    let config = components.get(component)?;
    let state_config = nested_state(config, state);
    let state_value = state_config.and_then(|state| field_value(state, field));
    if let Some(value) = state_value.and_then(|value| color_ref(value, colors)) {
        return Some(value);
    }
    let base = config.base.as_deref()?;
    let (base_role, base_state) = base.split_once('.').unwrap_or((base, ""));
    if !base_state.is_empty() {
        return resolve_role_field(roles, colors, base_role, base_state, field, 0);
    }
    if !state.is_empty()
        && roles
            .get(base_role)
            .and_then(|base| nested_state(base, state))
            .is_some()
        && let Some(value) = resolve_role_field(roles, colors, base_role, state, field, 0)
    {
        return Some(value);
    }
    let normal_value = field_value(config, field).and_then(|value| color_ref(value, colors));
    normal_value.or_else(|| resolve_role_field(roles, colors, base_role, "", field, 0))
}

fn field_value<'a>(config: &'a RoleFile, field: &str) -> Option<&'a String> {
    match field {
        "text" => config.text.as_ref(),
        "bg" => config.bg.as_ref(),
        "border" => config.border.as_ref(),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct ColorSet {
    text: Rgba,
    bg: Rgba,
    border: Rgba,
}

fn assign_role(
    palette: &mut ThemePalette,
    roles: &HashMap<String, RoleFile>,
    colors: &HashMap<String, Rgba>,
    role: &str,
    state: &str,
    apply: impl FnOnce(&mut ThemePalette, ColorSet),
) {
    if !roles.contains_key(role) {
        return;
    }
    let base = ColorSet {
        text: palette.text,
        bg: palette.surface_base,
        border: palette.border,
    };
    let resolve = |field: &str, fallback| {
        resolve_role_field(roles, colors, role, state, field, 0).unwrap_or(fallback)
    };
    apply(
        palette,
        ColorSet {
            text: resolve("text", base.text),
            bg: resolve("bg", base.bg),
            border: resolve("border", base.border),
        },
    );
}

fn resolve_role_field(
    roles: &HashMap<String, RoleFile>,
    colors: &HashMap<String, Rgba>,
    role: &str,
    state: &str,
    field: &str,
    depth: usize,
) -> Option<Rgba> {
    if depth > 16 {
        return None;
    }
    let config = roles.get(role)?;
    let selected = nested_state(config, state).unwrap_or(config);
    let value = match field {
        "text" => &selected.text,
        "bg" => &selected.bg,
        "border" => &selected.border,
        _ => return None,
    };
    if let Some(color) = value.as_ref().and_then(|value| color_ref(value, colors)) {
        return Some(color);
    }
    let base = selected.base.as_ref().or(config.base.as_ref())?;
    let (base_role, base_state) = base.split_once('.').unwrap_or((base, ""));
    resolve_role_field(roles, colors, base_role, base_state, field, depth + 1)
}

fn nested_state<'a>(config: &'a RoleFile, state: &str) -> Option<&'a RoleFile> {
    let mut current = config;
    for part in state.split('.').filter(|part| !part.is_empty()) {
        current = current.states.get(part)?;
    }
    Some(current)
}

fn color_ref(value: &str, colors: &HashMap<String, Rgba>) -> Option<Rgba> {
    parse_color(value).or_else(|| {
        colors
            .get(value.strip_prefix("palette.").unwrap_or(value))
            .copied()
    })
}

fn parse_color(value: &str) -> Option<Rgba> {
    if value.eq_ignore_ascii_case("transparent") || value.eq_ignore_ascii_case("none") {
        return Some(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        });
    }
    let value = value.strip_prefix('#')?;
    let parsed = u32::from_str_radix(value, 16).ok()?;
    let (rgb, alpha) = match value.len() {
        6 => (parsed, 255),
        8 => (parsed >> 8, parsed & 255),
        _ => return None,
    };
    let [_, r, g, b] = rgb.to_be_bytes();
    Some(Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: alpha as f32 / 255.,
    })
}
