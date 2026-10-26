//! 外部 keymap ファイルの読み込みと、既定表への重ね方。
//!
//! v5 の `keymap.toml` と同じく TOML の `[[binding]]` を読む。ただし v5 は
//! ファイルがそのまま表だったのに対し、ここは**既定表に重ねる差分**として扱う。
//! `Bindings::resolve` は登録順の最初の一致を採るので、user の binding を前に置くと
//! 同じ chord / 同じ scope で user が勝つ。配送順（focus → hover）は変えない。
//!
//! このモジュールは GPUI を知らない。scope 名と action 名の語彙は呼び出し側
//! （`input` adapter）が [`Table`] として渡す。条件式の評価器はここに作らず、
//! 既存の [`Condition`] の木へ落とすだけにする。
//!
//! 1 件でも問題のあるファイルは**丸ごと採らない**。部分適用すると、typo した 1 行が
//! 残りの binding を黙って消す。読めなければ既定の binding だけになり、理由を返す。

use gpui_filer_input::{
    ActionId, Condition, KeyBinding, KeyChord, Modifiers, MouseBinding, MouseButton, MouseChord,
    MouseGesture, ScopeId,
};
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;

/// この版が読む keymap の形式。これ以外は読まない。
const SCHEMA_VERSION: u32 = 1;
const MAX_KEYMAP_BYTES: u64 = 1024 * 1024;
const MAX_BINDINGS: usize = 4096;

/// 読み込んだ binding。既定表を組むときに読む。
#[derive(Default)]
pub(super) struct Loaded {
    pub(super) keys: Vec<KeyBinding>,
    pub(super) mouse: Vec<MouseBinding>,
}

/// keymap が名前で参照できる語彙。
///
/// 語彙の source of truth は `input` adapter 側に置く。ここに第二の一覧を作らない。
#[derive(Clone, Copy)]
pub(super) struct Table {
    pub(super) scopes: &'static [(&'static str, ScopeId)],
    pub(super) actions: &'static [&'static str],
    /// Actions whose adapter has a defined interpretation for blank-surface drag chords.
    pub(super) mouse_drag_actions: &'static [&'static str],
}

/// keymap を読む。1 件 1 行の診断を返す（空なら問題なし）。
///
/// ファイルが無いのは普通なので診断にしない。壊れていたときは何も設置せず、
/// 既定の binding だけで動く。
///
pub(super) fn load(path: Option<PathBuf>, table: Table) -> Result<Loaded, Vec<String>> {
    let Some(path) = path else {
        return Ok(Loaded::default());
    };
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(error) => {
            let reason = format!("{}: {error}", path.display());
            eprintln!("[keymap] {reason}");
            return Err(vec![reason]);
        }
    };
    let mut source = String::new();
    if let Err(error) = file
        .by_ref()
        .take(MAX_KEYMAP_BYTES.saturating_add(1))
        .read_to_string(&mut source)
    {
        let reason = format!("{}: {error}", path.display());
        eprintln!("[keymap] {reason}");
        return Err(vec![reason]);
    }
    if source.len() as u64 > MAX_KEYMAP_BYTES {
        let reason = format!(
            "{}: keymap is larger than {MAX_KEYMAP_BYTES} bytes",
            file_name(&path)
        );
        eprintln!(
            "[keymap] {}: keymap is larger than {MAX_KEYMAP_BYTES} bytes",
            path.display()
        );
        return Err(vec![reason]);
    }
    match compile(&source, table) {
        Ok(loaded) => Ok(loaded),
        Err(diagnostics) => {
            // 全部を起動ログに残す。画面の行にはファイル名だけを添える。
            // path を出すと長さで理由が読めなくなる。
            for diagnostic in &diagnostics {
                eprintln!("[keymap] {}: {diagnostic}", path.display());
            }
            let name = file_name(&path);
            Err(diagnostics
                .into_iter()
                .map(|diagnostic| format!("{name}: {diagnostic}"))
                .collect())
        }
    }
}

/// 診断に添えるファイル名。
fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// 読み込みのどこで問題が出たか。
#[derive(Debug)]
struct Diagnostic {
    /// 1 から数えた `[[binding]]` の位置。ファイル全体の問題では `None`。
    entry: Option<usize>,
    reason: String,
}

impl Diagnostic {
    fn whole(reason: impl Into<String>) -> Self {
        Self {
            entry: None,
            reason: reason.into(),
        }
    }

    fn entry(index: usize, reason: impl Into<String>) -> Self {
        Self {
            entry: Some(index),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.entry {
            Some(index) => write!(f, "binding {index}: {}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

/// ファイルの形。`deny_unknown_fields` で綴り違いも診断にする。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    /// この版が読める形式。
    version: u32,
    #[serde(default)]
    binding: Vec<RawBinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    /// `ctrl+shift+tab` のような chord。修飾キーを `+` で並べ、最後がキー名。
    key: Option<String>,
    /// マウス chord。`ctrl+left` と `shift+drag+left` のように指定する。
    button: Option<String>,
    /// マウスの連続クリック数。既定は 1。
    count: Option<usize>,
    /// `file_view.refresh` のような action ID。
    action: String,
    /// 適用する scope の式。省略すると常に適用。
    when: Option<String>,
}

/// 翻訳できた 1 件。キーとマウスは別の表に入る。
enum Compiled {
    Key(KeyBinding),
    Mouse(MouseBinding),
}

/// TOML を binding へ翻訳する。1 件でも問題があれば、そのファイルは採らない。
fn compile(source: &str, table: Table) -> Result<Loaded, Vec<Diagnostic>> {
    let file: File = match toml::from_str(source) {
        Ok(file) => file,
        Err(error) => return Err(vec![Diagnostic::whole(format!("{error}"))]),
    };
    if file.version != SCHEMA_VERSION {
        return Err(vec![Diagnostic::whole(format!(
            "keymap schema {} is not supported (this build reads {SCHEMA_VERSION})",
            file.version
        ))]);
    }
    if file.binding.len() > MAX_BINDINGS {
        return Err(vec![Diagnostic::whole(format!(
            "too many bindings (maximum is {MAX_BINDINGS})"
        ))]);
    }
    let mut loaded = Loaded::default();
    let mut problems = Vec::new();
    for (position, raw) in file.binding.iter().enumerate() {
        match binding(raw, table) {
            Ok(Compiled::Key(binding)) => loaded.keys.push(binding),
            Ok(Compiled::Mouse(binding)) => loaded.mouse.push(binding),
            // 全部の理由を返す。1 件直すたびに起動し直させない。
            Err(reason) => problems.push(Diagnostic::entry(position + 1, reason)),
        }
    }
    if problems.is_empty() {
        Ok(loaded)
    } else {
        Err(problems)
    }
}

fn binding(raw: &RawBinding, table: Table) -> Result<Compiled, String> {
    let action = table.action(&raw.action)?;
    let when = match raw.when.as_deref() {
        Some(text) => parse_when(text, table)?,
        None => Condition::Always,
    };
    match (&raw.key, &raw.button) {
        (Some(key), None) => {
            if raw.count.is_some() {
                return Err("`count` is only for mouse bindings".into());
            }
            Ok(Compiled::Key(KeyBinding {
                chord: parse_chord(key)?,
                when,
                action,
            }))
        }
        (None, Some(button)) => {
            let chord = parse_button(button, raw.count)?;
            if chord.gesture == MouseGesture::Drag
                && !table.mouse_drag_actions.contains(&action.as_str())
            {
                let allowed = table.mouse_drag_actions.join("`, `");
                return Err(format!(
                    "action `{}` cannot be used with a mouse drag chord; supported drag actions: `{allowed}`",
                    action.as_str()
                ));
            }
            Ok(Compiled::Mouse(MouseBinding {
                chord,
                when,
                action,
            }))
        }
        (Some(_), Some(_)) => Err("a binding takes either `key` or `button`, not both".into()),
        (None, None) => Err("a binding needs `key` or `button`".into()),
    }
}

/// `ctrl+shift+t` を chord にする。
///
/// この crate はキー名の一覧を持たないので、キー名そのものは GPUI の表記に合わせる。
/// ここで見るのは、修飾キーの綴り・重複・空の名前といった chord として成立しない形。
fn parse_chord(text: &str) -> Result<KeyChord, String> {
    let mut modifiers = Modifiers::default();
    let mut parts = text.split('+').map(str::trim).peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let key = key_of(part)?;
            return Ok(KeyChord::new(modifiers, key));
        }
        set_modifier(&mut modifiers, part)?;
    }
    Err("the key name is empty".into())
}

fn set_modifier(modifiers: &mut Modifiers, part: &str) -> Result<(), String> {
    let slot = match part.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => &mut modifiers.control,
        "alt" => &mut modifiers.alt,
        "shift" => &mut modifiers.shift,
        "super" | "win" | "cmd" | "platform" => &mut modifiers.platform,
        other => return Err(format!("unknown modifier `{other}`")),
    };
    if *slot {
        return Err(format!("`{part}` is repeated"));
    }
    *slot = true;
    Ok(())
}

/// キー名。空と空白入りは chord として成立しない。
fn key_of(part: &str) -> Result<String, String> {
    if part.is_empty() {
        return Err("the key name is empty".into());
    }
    if part.chars().any(char::is_whitespace) {
        return Err(format!("the key name `{part}` contains a space"));
    }
    Ok(part.to_ascii_lowercase())
}

/// `ctrl+left` / `shift+drag+left` を mouse chord にする。`count` はクリック数だけに使う。
fn parse_button(text: &str, count: Option<usize>) -> Result<MouseChord, String> {
    let mut modifiers = Modifiers::default();
    let mut drag = false;
    let mut parts = text.split('+').map(str::trim).peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if drag && count.is_some() {
                return Err("`count` is only for click bindings".into());
            }
            let button = button_of(part)?;
            if drag {
                return Ok(MouseChord::drag(button, modifiers));
            }
            let count = count.unwrap_or(1);
            if count == 0 {
                return Err("`count` starts at 1".into());
            }
            return Ok(MouseChord::new(button, count, modifiers));
        }
        if part.eq_ignore_ascii_case("drag") {
            if drag {
                return Err("`drag` is repeated".into());
            }
            drag = true;
        } else {
            set_modifier(&mut modifiers, part)?;
        }
    }
    Err("the button name is empty".into())
}

fn button_of(part: &str) -> Result<MouseButton, String> {
    match part.to_ascii_lowercase().as_str() {
        "left" => Ok(MouseButton::Left),
        "middle" => Ok(MouseButton::Middle),
        "right" => Ok(MouseButton::Right),
        other => Err(format!("unknown button `{other}`")),
    }
}

/// `file_view & !selection_mode` を条件式にする。
///
/// v5 の when 式と同じ `&` / `|` / `!` と括弧を受ける。優先順位は `!` > `&` > `|`。
fn parse_when(text: &str, table: Table) -> Result<Condition, String> {
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Err("the condition is empty".into());
    }
    let mut parser = WhenParser {
        tokens: &tokens,
        at: 0,
        table,
    };
    let condition = parser.or()?;
    if let Some(token) = parser.tokens.get(parser.at) {
        return Err(format!("unexpected `{}`", token.text()));
    }
    Ok(condition)
}

#[derive(Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Not,
    And,
    Or,
    Open,
    Close,
}

impl Token {
    fn text(&self) -> String {
        match self {
            Self::Word(word) => word.clone(),
            Self::Not => "!".into(),
            Self::And => "&".into(),
            Self::Or => "|".into(),
            Self::Open => "(".into(),
            Self::Close => ")".into(),
        }
    }
}

/// `&&` / `||` も受ける。v5 の keymap は 1 文字だったが、書き手は両方使う。
fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        match ch {
            c if c.is_whitespace() => rest = &rest[c.len_utf8()..],
            '!' => {
                tokens.push(Token::Not);
                rest = &rest[1..];
            }
            '(' => {
                tokens.push(Token::Open);
                rest = &rest[1..];
            }
            ')' => {
                tokens.push(Token::Close);
                rest = &rest[1..];
            }
            '&' => {
                tokens.push(Token::And);
                rest = rest.strip_prefix("&&").unwrap_or(&rest[1..]);
            }
            '|' => {
                tokens.push(Token::Or);
                rest = rest.strip_prefix("||").unwrap_or(&rest[1..]);
            }
            c if c.is_alphanumeric() || c == '_' => {
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                tokens.push(Token::Word(rest[..end].to_owned()));
                rest = &rest[end..];
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(tokens)
}

struct WhenParser<'a> {
    tokens: &'a [Token],
    at: usize,
    table: Table,
}

impl WhenParser<'_> {
    fn or(&mut self) -> Result<Condition, String> {
        let mut left = self.and()?;
        while self.take(Token::Or) {
            let right = self.and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Condition, String> {
        let mut left = self.unary()?;
        while self.take(Token::And) {
            let right = self.unary()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Condition, String> {
        if self.take(Token::Not) {
            return Ok(Condition::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Condition, String> {
        match self.tokens.get(self.at) {
            Some(Token::Word(word)) => {
                let scope = self.table.scope(word)?;
                self.at += 1;
                Ok(Condition::Scope(scope))
            }
            Some(Token::Open) => {
                self.at += 1;
                let inner = self.or()?;
                if !self.take(Token::Close) {
                    return Err("`)` is missing".into());
                }
                Ok(inner)
            }
            Some(token) => Err(format!("unexpected `{}`", token.text())),
            None => Err("the condition ends here".into()),
        }
    }

    fn take(&mut self, token: Token) -> bool {
        if self.tokens.get(self.at) == Some(&token) {
            self.at += 1;
            return true;
        }
        false
    }
}

impl Table {
    /// action 名を引く。大文字小文字だけ違うときは正しい綴りを添える。
    fn action(&self, name: &str) -> Result<ActionId, String> {
        if let Some(known) = self.actions.iter().copied().find(|known| *known == name) {
            return Ok(ActionId::new(known));
        }
        Err(match meant(name, self.actions.iter().copied()) {
            Some(canonical) => format!("unknown action `{name}` (did you mean `{canonical}`?)"),
            None => format!("unknown action `{name}`"),
        })
    }

    fn scope(&self, name: &str) -> Result<ScopeId, String> {
        if let Some((_, scope)) = self.scopes.iter().find(|(known, _)| *known == name) {
            return Ok(*scope);
        }
        Err(
            match meant(name, self.scopes.iter().map(|(known, _)| *known)) {
                Some(canonical) => format!("unknown scope `{name}` (did you mean `{canonical}`?)"),
                None => format!("unknown scope `{name}`"),
            },
        )
    }
}

/// 大文字小文字だけ違う候補。v5 の `FileView` のような綴りを書いた人に正解を返す。
fn meant<'a>(name: &str, known: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    known
        .into_iter()
        .find(|known| known.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: Table = Table {
        scopes: &[("file_view", ScopeId::new(2)), ("shell", ScopeId::new(1))],
        actions: &["file_view.refresh", "shell.next_tab"],
        mouse_drag_actions: &[],
    };

    fn compile_ok(source: &str) -> Loaded {
        compile(source, TABLE).expect("the keymap compiles")
    }

    fn compile_err(source: &str) -> Vec<String> {
        compile(source, TABLE)
            .err()
            .expect("the keymap is refused")
            .iter()
            .map(|diagnostic| diagnostic.to_string())
            .collect()
    }

    #[test]
    fn a_binding_compiles_to_the_routing_types() {
        let loaded = compile_ok(
            r#"
version = 1

[[binding]]
key = "Ctrl+Shift+Tab"
action = "shell.next_tab"
when = "shell"
"#,
        );

        assert_eq!(loaded.keys.len(), 1);
        let binding = &loaded.keys[0];
        assert_eq!(
            binding.chord,
            KeyChord::new(
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                "tab"
            )
        );
        assert_eq!(binding.action, ActionId::new("shell.next_tab"));
        assert!(binding.when.matches(&[ScopeId::new(1)]));
        assert!(!binding.when.matches(&[ScopeId::new(9)]));
    }

    /// マウスの binding も同じファイルで書ける。
    #[test]
    fn a_mouse_binding_compiles_with_its_count_and_modifiers() {
        let loaded = compile_ok(
            r#"
version = 1

[[binding]]
button = "left"
count = 2
action = "file_view.refresh"

[[binding]]
button = "Ctrl+Middle"
action = "shell.next_tab"
when = "file_view"
"#,
        );

        assert!(loaded.keys.is_empty());
        assert_eq!(loaded.mouse.len(), 2);
        assert_eq!(
            loaded.mouse[0].chord,
            MouseChord::new(MouseButton::Left, 2, Modifiers::default())
        );
        // `count` を書かなければ 1 回。
        assert_eq!(
            loaded.mouse[1].chord,
            MouseChord::new(
                MouseButton::Middle,
                1,
                Modifiers {
                    control: true,
                    ..Default::default()
                }
            )
        );
    }

    /// `key` と `button` はどちらか一方。両方や無しは診断になる。
    #[test]
    fn a_binding_needs_exactly_one_of_key_and_button() {
        let both = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
button = "left"
action = "file_view.refresh"
"#,
        );
        assert!(both[0].contains("either `key` or `button`"), "{both:?}");

        let neither = compile_err(
            r#"
version = 1

[[binding]]
action = "file_view.refresh"
"#,
        );
        assert!(
            neither[0].contains("needs `key` or `button`"),
            "{neither:?}"
        );

        // キーに `count` は書けない。
        let counted = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
count = 2
action = "file_view.refresh"
"#,
        );
        assert!(counted[0].contains("only for mouse"), "{counted:?}");
    }

    #[test]
    fn a_button_that_cannot_exist_is_refused() {
        for button in ["", "back", "left+", "ctril+left"] {
            let problems = compile_err(&format!(
                "version = 1\n\n[[binding]]\nbutton = \"{button}\"\naction = \"file_view.refresh\"\n"
            ));
            assert_eq!(problems.len(), 1, "button `{button}`: {problems:?}");
        }

        let zero = compile_err(
            r#"
version = 1

[[binding]]
button = "left"
count = 0
action = "file_view.refresh"
"#,
        );
        assert!(zero[0].contains("starts at 1"), "{zero:?}");
    }

    /// `when` を省いた binding はどこでも適用される。
    #[test]
    fn a_binding_without_a_condition_is_always_on() {
        let loaded = compile_ok(
            r#"
version = 1

[[binding]]
key = "f5"
action = "file_view.refresh"
"#,
        );
        assert_eq!(loaded.keys[0].when, Condition::Always);
    }

    /// `!` > `&` > `|` の順に組む。v5 の when 式と同じ読み方。
    #[test]
    fn the_condition_keeps_the_usual_precedence() {
        let loaded = compile_ok(
            r#"
version = 1

[[binding]]
key = "escape"
action = "file_view.refresh"
when = "shell | file_view & !file_view"
"#,
        );

        // `shell | (file_view & !file_view)` なので、shell だけで真になる。
        assert!(loaded.keys[0].when.matches(&[ScopeId::new(1)]));
        assert!(!loaded.keys[0].when.matches(&[ScopeId::new(2)]));
        // 括弧を足せば `(shell | file_view) & !file_view` になる。
        let loaded = compile_ok(
            r#"
version = 1

[[binding]]
key = "escape"
action = "file_view.refresh"
when = "(shell || file_view) && !file_view"
"#,
        );
        assert!(loaded.keys[0].when.matches(&[ScopeId::new(1)]));
        assert!(!loaded.keys[0].when.matches(&[ScopeId::new(2)]));
    }

    /// 1 件でも問題があれば、そのファイルは丸ごと採らない。
    ///
    /// 良い行だけ適用すると、typo した 1 行が残りの binding を黙って消す。
    #[test]
    fn one_bad_entry_refuses_the_whole_file() {
        let problems = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
action = "file_view.refresh"

[[binding]]
key = "f6"
action = "file_view.refreshh"
"#,
        );

        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("binding 2"), "{problems:?}");
        assert!(problems[0].contains("unknown action"), "{problems:?}");
    }

    #[test]
    fn every_bad_entry_is_reported_at_once() {
        let problems = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
action = "nope"

[[binding]]
key = "f6"
action = "file_view.refresh"
when = "shell &"
"#,
        );

        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("binding 1"), "{problems:?}");
        assert!(problems[1].contains("binding 2"), "{problems:?}");
    }

    #[test]
    fn a_chord_that_cannot_exist_is_refused() {
        for key in [
            "",
            "ctrl+",
            "ctrl+f5+shift",
            "ctrl+ctrl+f5",
            "ctril+f5",
            "f 5",
        ] {
            let problems = compile_err(&format!(
                "version = 1\n\n[[binding]]\nkey = \"{key}\"\naction = \"file_view.refresh\"\n"
            ));
            assert_eq!(problems.len(), 1, "key `{key}`: {problems:?}");
            assert!(
                !problems[0].contains("unknown action"),
                "key `{key}`: {problems:?}"
            );
        }
    }

    #[test]
    fn an_unknown_scope_is_refused_with_the_canonical_spelling() {
        let problems = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
action = "file_view.refresh"
when = "File_View"
"#,
        );
        assert!(problems[0].contains("unknown scope"), "{problems:?}");
        assert!(
            problems[0].contains("did you mean `file_view`"),
            "{problems:?}"
        );

        // 大文字小文字だけ違う action も同じ扱い。
        let problems = compile_err(
            r#"
version = 1

[[binding]]
key = "f5"
action = "File_View.Refresh"
"#,
        );
        assert!(
            problems[0].contains("did you mean `file_view.refresh`"),
            "{problems:?}"
        );
    }

    /// 形式が違うファイルは、ファイル全体の問題として返す。
    #[test]
    fn a_broken_file_reports_the_file_not_an_entry() {
        let problems = compile_err("version = 1\n\n[[binding]]\nkey = \"f5\"\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(!problems[0].contains("binding 1"), "{problems:?}");

        // 綴り違いの field も拒否する。
        let problems =
            compile_err("version = 1\n\n[[binding]]\nkeys = \"f5\"\naction = \"shell.next_tab\"\n");
        assert!(!problems.is_empty(), "{problems:?}");

        // 未来の形式。
        let problems = compile_err("version = 2\n");
        assert!(problems[0].contains("not supported"), "{problems:?}");

        // 空のファイル。
        let problems = compile_err("");
        assert!(!problems.is_empty(), "{problems:?}");
    }

    /// binding が 1 件も無いファイルは、既定表をそのままにする。
    #[test]
    fn a_file_without_bindings_compiles_to_nothing() {
        let loaded = compile_ok("version = 1\n");
        assert!(loaded.keys.is_empty() && loaded.mouse.is_empty());
    }
}
