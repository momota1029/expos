//! キーの割り当て表。v5 の `keymap.toml` と `WhenExpr` に相当する部分。
//!
//! v5 との違いは二つ。条件のタグが文字列ではなく [`ScopeId`] になったことと、
//! 解決結果が GPUI の Action ではなくアプリが解釈する [`ActionId`] になったこと。
//! このモジュールは GPUI を参照しない。

use crate::{KeyChord, MouseChord, ScopeId};

/// アプリが解釈するアクション名。input crate は中身を知らず、そのまま返すだけ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActionId(&'static str);

impl ActionId {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

/// binding を適用する条件。
///
/// v5 の when 式(`A & B`, `A | B`, `!A`)を同じ形の木で持つ。
/// タグはイベント位置のスコープ列に対する所属判定になる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    Always,
    Scope(ScopeId),
    Not(Box<Condition>),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
}

impl Condition {
    /// イベントが起きた位置のスコープ列(root→leaf)に対する評価。
    pub fn matches(&self, scopes: &[ScopeId]) -> bool {
        match self {
            Self::Always => true,
            Self::Scope(scope) => scopes.contains(scope),
            Self::Not(inner) => !inner.matches(scopes),
            Self::And(lhs, rhs) => lhs.matches(scopes) && rhs.matches(scopes),
            Self::Or(lhs, rhs) => lhs.matches(scopes) || rhs.matches(scopes),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    pub chord: KeyChord,
    pub when: Condition,
    pub action: ActionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouseBinding {
    pub chord: MouseChord,
    pub when: Condition,
    pub action: ActionId,
}

/// キーとマウスを別の列で持ち、照合も別々に行う。
///
/// どちらも登録順に最初に一致したものを採用する。v5 と同じ規則なので、
/// 具体的な条件を先に並べるのは呼び出し側の責任になる。
#[derive(Default)]
pub struct Bindings {
    keys: Vec<KeyBinding>,
    mouse: Vec<MouseBinding>,
}

impl Bindings {
    pub fn new(bindings: impl IntoIterator<Item = KeyBinding>) -> Self {
        Self {
            keys: bindings.into_iter().collect(),
            mouse: Vec::new(),
        }
    }

    pub fn with_mouse(mut self, bindings: impl IntoIterator<Item = MouseBinding>) -> Self {
        self.mouse = bindings.into_iter().collect();
        self
    }

    pub fn resolve(&self, chord: &KeyChord, scopes: &[ScopeId]) -> Option<&KeyBinding> {
        self.keys
            .iter()
            .find(|binding| &binding.chord == chord && binding.when.matches(scopes))
    }

    pub fn resolve_mouse(&self, chord: &MouseChord, scopes: &[ScopeId]) -> Option<&MouseBinding> {
        self.mouse
            .iter()
            .find(|binding| &binding.chord == chord && binding.when.matches(scopes))
    }

    pub fn len(&self) -> usize {
        self.keys.len() + self.mouse.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.mouse.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Modifiers, MouseButton};

    fn scope(value: u64) -> ScopeId {
        ScopeId::new(value)
    }

    fn chord(key: &str) -> KeyChord {
        KeyChord::new(Modifiers::default(), key)
    }

    #[test]
    fn conditions_evaluate_against_the_scope_chain() {
        // v5 の "FileView & !selection_mode" に相当する形。
        let condition = Condition::And(
            Box::new(Condition::Scope(scope(1))),
            Box::new(Condition::Not(Box::new(Condition::Scope(scope(9))))),
        );

        assert!(condition.matches(&[scope(1), scope(2)]));
        assert!(!condition.matches(&[scope(1), scope(9)]));
        assert!(!condition.matches(&[scope(2)]));
        assert!(Condition::Always.matches(&[]));
    }

    #[test]
    fn or_conditions_accept_either_scope() {
        // v5 の "Tiles | Rows" に相当する形。
        let condition = Condition::Or(
            Box::new(Condition::Scope(scope(3))),
            Box::new(Condition::Scope(scope(4))),
        );

        assert!(condition.matches(&[scope(1), scope(3)]));
        assert!(condition.matches(&[scope(4)]));
        assert!(!condition.matches(&[scope(1), scope(5)]));
    }

    #[test]
    fn resolve_takes_the_first_registered_match() {
        let bindings = Bindings::new(vec![
            KeyBinding {
                chord: chord("escape"),
                when: Condition::Scope(scope(2)),
                action: ActionId::new("selection.clear"),
            },
            KeyBinding {
                chord: chord("escape"),
                when: Condition::Always,
                action: ActionId::new("preview.close"),
            },
        ]);

        let scoped = bindings
            .resolve(&chord("escape"), &[scope(1), scope(2)])
            .expect("scoped binding");
        assert_eq!(scoped.action, ActionId::new("selection.clear"));

        let fallback = bindings
            .resolve(&chord("escape"), &[scope(1)])
            .expect("fallback binding");
        assert_eq!(fallback.action, ActionId::new("preview.close"));
    }

    #[test]
    fn resolve_requires_the_exact_chord() {
        let bindings = Bindings::new(vec![KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "tab",
            ),
            when: Condition::Always,
            action: ActionId::new("tabs.next"),
        }]);

        assert!(bindings.resolve(&chord("tab"), &[]).is_none());
        assert!(
            bindings
                .resolve(
                    &KeyChord::new(
                        Modifiers {
                            control: true,
                            ..Default::default()
                        },
                        "tab"
                    ),
                    &[]
                )
                .is_some()
        );
    }

    #[test]
    fn mouse_bindings_resolve_by_chord_and_condition() {
        let bindings = Bindings::new(std::iter::empty::<KeyBinding>()).with_mouse([
            MouseBinding {
                chord: MouseChord::new(
                    MouseButton::Left,
                    1,
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                ),
                when: Condition::Scope(scope(2)),
                action: ActionId::new("selection.extend"),
            },
            MouseBinding {
                chord: MouseChord::new(MouseButton::Left, 1, Modifiers::default()),
                when: Condition::Scope(scope(2)),
                action: ActionId::new("selection.pick"),
            },
        ]);

        let plain = MouseChord::new(MouseButton::Left, 1, Modifiers::default());
        assert_eq!(
            bindings
                .resolve_mouse(&plain, &[scope(2)])
                .map(|b| b.action),
            Some(ActionId::new("selection.pick"))
        );
        // 条件に合わないスコープでは引かれない。
        assert!(bindings.resolve_mouse(&plain, &[scope(1)]).is_none());

        let shift = MouseChord::new(
            MouseButton::Left,
            1,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            bindings
                .resolve_mouse(&shift, &[scope(2)])
                .map(|b| b.action),
            Some(ActionId::new("selection.extend"))
        );
    }
}
