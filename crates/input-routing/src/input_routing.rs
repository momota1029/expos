use std::collections::HashMap;

mod binding;
mod key;
mod mouse;

pub use binding::{ActionId, Bindings, Condition, KeyBinding, MouseBinding};
pub use key::{KeyChord, KeyName, Modifiers};
pub use mouse::{MouseButton, MouseChord, MouseGesture};

/// Intent for a foreign filesystem drag, decided from the native Drop modifier snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForeignFileDropIntent {
    #[default]
    Default,
    Copy,
    Move,
    Link,
}

pub fn foreign_file_drop_intent(modifiers: Modifiers) -> ForeignFileDropIntent {
    if modifiers.alt || (modifiers.control && modifiers.shift) {
        ForeignFileDropIntent::Link
    } else if modifiers.shift {
        ForeignFileDropIntent::Move
    } else if modifiers.control {
        ForeignFileDropIntent::Copy
    } else {
        ForeignFileDropIntent::Default
    }
}

#[cfg(test)]
mod foreign_drop_tests {
    use super::{ForeignFileDropIntent, Modifiers, foreign_file_drop_intent};

    #[test]
    fn foreign_drop_modifiers_select_link_without_move_fallback() {
        assert_eq!(
            foreign_file_drop_intent(Modifiers {
                alt: true,
                ..Modifiers::default()
            }),
            ForeignFileDropIntent::Link
        );
        assert_eq!(
            foreign_file_drop_intent(Modifiers {
                control: true,
                shift: true,
                ..Modifiers::default()
            }),
            ForeignFileDropIntent::Link
        );
        assert_eq!(
            foreign_file_drop_intent(Modifiers {
                shift: true,
                ..Modifiers::default()
            }),
            ForeignFileDropIntent::Move
        );
        assert_eq!(
            foreign_file_drop_intent(Modifiers::default()),
            ForeignFileDropIntent::Default
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeId(u64);

impl ScopeId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PointerId(u64);

impl PointerId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractionPath {
    scopes: Vec<ScopeId>,
}

impl InteractionPath {
    pub fn new(scopes: impl IntoIterator<Item = ScopeId>) -> Option<Self> {
        let scopes = scopes.into_iter().collect::<Vec<_>>();
        (!scopes.is_empty()).then_some(Self { scopes })
    }

    pub fn leaf(&self) -> ScopeId {
        self.scopes[self.scopes.len() - 1]
    }

    pub fn contains_path(&self, ancestor: &Self) -> bool {
        self.scopes.starts_with(&ancestor.scopes)
    }

    /// root→leaf のスコープ列。binding の条件評価にそのまま渡す。
    pub fn scopes(&self) -> &[ScopeId] {
        &self.scopes
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusKind {
    #[default]
    Regular,
    TextInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPolicy {
    FocusedOnly,
    FocusedThenHovered,
    HoveredOnly,
    PointerLocal(PointerId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRoute {
    Modal,
    ActivePointer,
    Focus,
    Hover,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputTarget {
    pub path: InteractionPath,
    pub route: InputRoute,
}

impl InputTarget {
    pub fn scope(&self) -> ScopeId {
        self.path.leaf()
    }
}

/// binding が選ばれた結果。配信先とアクション名を返す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedInput {
    pub action: ActionId,
    pub target: InputTarget,
}

/// 最後に届いた入力の種類。
///
/// タッチは離したあとも最後の接触位置が hover として残るため、そのままでは
/// 意味のない hover が居座る。キーボード操作中も同じで、ポインタ位置の hover が
/// 生きていると「今の操作対象」が読めなくなる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputDevice {
    #[default]
    Mouse,
    Touch,
    Pen,
    Keyboard,
}

impl InputDevice {
    /// この入力のあと、ポインタ位置の hover を表示してよいか。
    /// マウスとペンだけが「ポインタを動かして指し示す」操作になる。
    pub fn keeps_hover(self) -> bool {
        matches!(self, Self::Mouse | Self::Pen)
    }

    /// この入力のあと、hover 位置へ入力（キーやクリック）を配信してよいか。
    ///
    /// 表示の [`Self::keeps_hover`] とは別に持つ。両方を一つの述語で見ると、
    /// キーボードを押した瞬間に配送まで止まり、`file_item` のような hover 条件の
    /// binding が永久に当たらなくなる。
    ///
    /// タッチは離したあとも最後の接触位置が残るので、そのままでは意味のない位置へ
    /// 配信してしまう。だから配信しない。キーボードは「ポインタはまだその項目の上に
    /// ある」ので配信してよい。
    pub fn keeps_hover_routing(self) -> bool {
        !matches!(self, Self::Touch)
    }
}

#[derive(Clone, Debug)]
struct FocusedPath {
    path: InteractionPath,
    kind: FocusKind,
}

#[derive(Default)]
pub struct InteractionRouter {
    modal: Option<InteractionPath>,
    focused: Option<FocusedPath>,
    hovered: Option<InteractionPath>,
    active_pointers: HashMap<PointerId, InteractionPath>,
    last_device: InputDevice,
}

impl InteractionRouter {
    pub fn set_modal(&mut self, path: Option<InteractionPath>) {
        self.modal = path;
    }

    pub fn set_focused(&mut self, path: Option<InteractionPath>, kind: FocusKind) {
        self.focused = path.map(|path| FocusedPath { path, kind });
    }

    pub fn set_hovered(&mut self, path: Option<InteractionPath>) {
        self.hovered = path;
    }

    /// 最後に届いた入力の種類を記録する。
    ///
    /// マウスが動けば hover が復活する。タッチやキーボードのあとは、
    /// ポインタ位置が残っていても hover を配信しない。
    pub fn set_last_device(&mut self, device: InputDevice) {
        self.last_device = device;
    }

    pub fn last_device(&self) -> InputDevice {
        self.last_device
    }

    /// 今 hover を配信してよいか。
    pub fn hover_enabled(&self) -> bool {
        self.last_device.keeps_hover()
    }

    pub fn begin_pointer(&mut self, pointer: PointerId, path: InteractionPath) {
        self.active_pointers.insert(pointer, path);
    }

    pub fn end_pointer(&mut self, pointer: PointerId) -> Option<InteractionPath> {
        self.active_pointers.remove(&pointer)
    }

    pub fn candidates(&self, policy: InputPolicy) -> Vec<InputTarget> {
        let mut candidates = match policy {
            InputPolicy::FocusedOnly => self.focus_candidate().into_iter().collect(),
            InputPolicy::FocusedThenHovered => {
                let mut candidates = Vec::with_capacity(2);
                if let Some(focused) = self.focus_candidate() {
                    candidates.push(focused);
                }
                if self
                    .focused
                    .as_ref()
                    .is_none_or(|focused| focused.kind != FocusKind::TextInput)
                    && let Some(hovered) = self.hover_candidate()
                    && candidates
                        .last()
                        .is_none_or(|(path, _)| path.leaf() != hovered.0.leaf())
                {
                    candidates.push(hovered);
                }
                candidates
            }
            InputPolicy::HoveredOnly => self.hover_candidate().into_iter().collect(),
            InputPolicy::PointerLocal(pointer) => self
                .active_pointers
                .get(&pointer)
                .map(|path| (path, InputRoute::ActivePointer))
                .into_iter()
                .collect(),
        };

        let Some(modal) = &self.modal else {
            return Self::targets(candidates);
        };
        candidates.retain(|(path, _)| path.contains_path(modal));
        // capture のない pointer event を modal root の接触として扱わない。
        if candidates.is_empty() && !matches!(policy, InputPolicy::PointerLocal(_)) {
            candidates.push((modal, InputRoute::Modal));
        }
        Self::targets(candidates)
    }

    /// 配信順の候補へ binding を当て、最初に一致したものを返す。
    ///
    /// 候補ごとに表を先頭から探すので、focus で一致しなければ hover が拾われる。
    /// v5 の focus binding → hover binding の順序と同じ規則になる。
    pub fn resolve_key(
        &self,
        bindings: &Bindings,
        chord: &KeyChord,
        policy: InputPolicy,
    ) -> Option<ResolvedInput> {
        self.candidates(policy).into_iter().find_map(|target| {
            bindings
                .resolve(chord, target.path.scopes())
                .map(|binding| ResolvedInput {
                    action: binding.action,
                    target: target.clone(),
                })
        })
    }

    fn focus_candidate(&self) -> Option<(&InteractionPath, InputRoute)> {
        self.focused
            .as_ref()
            .map(|focused| (&focused.path, InputRoute::Focus))
    }

    fn hover_candidate(&self) -> Option<(&InteractionPath, InputRoute)> {
        // 表示と同じ述語を使わない。キーボードのあとも hover 条件の binding は当たる。
        if !self.last_device.keeps_hover_routing() {
            return None;
        }
        self.hovered
            .as_ref()
            .map(|hovered| (hovered, InputRoute::Hover))
    }

    fn targets(candidates: Vec<(&InteractionPath, InputRoute)>) -> Vec<InputTarget> {
        candidates
            .into_iter()
            .map(|(path, route)| InputTarget {
                path: path.clone(),
                route,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(scopes: &[u64]) -> InteractionPath {
        InteractionPath::new(scopes.iter().copied().map(ScopeId::new)).unwrap()
    }

    #[test]
    fn focus_precedes_hover_and_hover_is_a_fallback() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);
        router.set_hovered(Some(path(&[1, 3])));

        assert_eq!(
            router.candidates(InputPolicy::FocusedThenHovered),
            vec![
                InputTarget {
                    path: path(&[1, 2]),
                    route: InputRoute::Focus,
                },
                InputTarget {
                    path: path(&[1, 3]),
                    route: InputRoute::Hover,
                },
            ]
        );
    }

    #[test]
    fn text_input_focus_suppresses_hover_fallback() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::TextInput);
        router.set_hovered(Some(path(&[1, 3])));

        assert_eq!(
            router.candidates(InputPolicy::FocusedThenHovered),
            vec![InputTarget {
                path: path(&[1, 2]),
                route: InputRoute::Focus,
            }]
        );
    }

    #[test]
    fn active_pointer_keeps_its_down_target_when_hover_moves() {
        let mut router = InteractionRouter::default();
        let pointer = PointerId::new(7);
        router.begin_pointer(pointer, path(&[1, 2]));
        router.set_hovered(Some(path(&[1, 3])));

        assert_eq!(
            router.candidates(InputPolicy::PointerLocal(pointer)),
            vec![InputTarget {
                path: path(&[1, 2]),
                route: InputRoute::ActivePointer,
            }]
        );
    }

    #[test]
    fn modal_rejects_targets_from_the_background() {
        let mut router = InteractionRouter::default();
        router.set_modal(Some(path(&[1, 4])));
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);

        assert_eq!(
            router.candidates(InputPolicy::FocusedOnly),
            vec![InputTarget {
                path: path(&[1, 4]),
                route: InputRoute::Modal,
            }]
        );
    }

    #[test]
    fn modal_keeps_targets_from_its_own_subtree() {
        let mut router = InteractionRouter::default();
        router.set_modal(Some(path(&[1, 4])));
        router.set_focused(Some(path(&[1, 4, 5])), FocusKind::Regular);

        assert_eq!(
            router.candidates(InputPolicy::FocusedOnly),
            vec![InputTarget {
                path: path(&[1, 4, 5]),
                route: InputRoute::Focus,
            }]
        );
    }

    #[test]
    fn ending_a_pointer_releases_its_local_route() {
        let mut router = InteractionRouter::default();
        let pointer = PointerId::new(7);
        let down_path = path(&[1, 2]);
        router.set_modal(Some(path(&[1, 4])));
        router.begin_pointer(pointer, down_path.clone());

        assert_eq!(router.end_pointer(pointer), Some(down_path));
        assert!(
            router
                .candidates(InputPolicy::PointerLocal(pointer))
                .is_empty()
        );
    }

    #[test]
    fn resolve_key_prefers_the_focused_candidate() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);
        router.set_hovered(Some(path(&[1, 3])));
        let chord = KeyChord::new(Modifiers::default(), "escape");
        let bindings = Bindings::new(vec![
            // 登録順は hover 側が先でも、配信候補の順序では focus が先になる。
            KeyBinding {
                chord: chord.clone(),
                when: Condition::Scope(ScopeId::new(3)),
                action: ActionId::new("hover.close"),
            },
            KeyBinding {
                chord: chord.clone(),
                when: Condition::Scope(ScopeId::new(2)),
                action: ActionId::new("focus.clear"),
            },
        ]);

        let resolved = router
            .resolve_key(&bindings, &chord, InputPolicy::FocusedThenHovered)
            .expect("a focused binding");

        assert_eq!(resolved.action, ActionId::new("focus.clear"));
        assert_eq!(resolved.target.scope(), ScopeId::new(2));
        assert_eq!(resolved.target.route, InputRoute::Focus);
    }

    #[test]
    fn resolve_key_falls_back_to_the_hovered_candidate() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);
        router.set_hovered(Some(path(&[1, 3])));
        let chord = KeyChord::new(Modifiers::default(), "delete");
        let bindings = Bindings::new(vec![KeyBinding {
            chord: chord.clone(),
            when: Condition::Scope(ScopeId::new(3)),
            action: ActionId::new("hover.delete"),
        }]);

        let resolved = router
            .resolve_key(&bindings, &chord, InputPolicy::FocusedThenHovered)
            .expect("the hovered binding");

        assert_eq!(resolved.target.scope(), ScopeId::new(3));
        assert_eq!(resolved.target.route, InputRoute::Hover);
    }

    #[test]
    fn touch_and_keyboard_suppress_the_hover_candidate() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);
        router.set_hovered(Some(path(&[1, 3])));

        // 既定はマウスなので hover が出る。
        assert_eq!(router.candidates(InputPolicy::HoveredOnly).len(), 1);

        // タッチのあとは、ポインタ位置が残っていても hover を配信しない。
        router.set_last_device(InputDevice::Touch);
        assert!(router.candidates(InputPolicy::HoveredOnly).is_empty());
        assert_eq!(
            router.candidates(InputPolicy::FocusedThenHovered),
            vec![InputTarget {
                path: path(&[1, 2]),
                route: InputRoute::Focus,
            }]
        );
    }

    /// キーボードのあとは **表示だけ** を止める。配送は残す。
    ///
    /// 配送まで止めると、`file_item` のような hover 条件の binding が永久に当たらない。
    #[test]
    fn keyboard_keeps_routing_to_the_hovered_target() {
        let mut router = InteractionRouter::default();
        router.set_focused(Some(path(&[1, 2])), FocusKind::Regular);
        router.set_hovered(Some(path(&[1, 3])));
        router.set_last_device(InputDevice::Keyboard);

        // ハイライトは出さない。
        assert!(!router.hover_enabled());
        // 配送は focus → hover の順に残る。
        assert_eq!(
            router.candidates(InputPolicy::FocusedThenHovered),
            vec![
                InputTarget {
                    path: path(&[1, 2]),
                    route: InputRoute::Focus,
                },
                InputTarget {
                    path: path(&[1, 3]),
                    route: InputRoute::Hover,
                },
            ]
        );

        // マウスを動かせば表示も戻る。
        router.set_last_device(InputDevice::Mouse);
        assert_eq!(router.candidates(InputPolicy::HoveredOnly).len(), 1);
    }

    #[test]
    fn pen_keeps_hover_like_a_mouse() {
        let mut router = InteractionRouter::default();
        router.set_hovered(Some(path(&[1, 3])));
        router.set_last_device(InputDevice::Pen);
        assert!(router.hover_enabled());
        assert_eq!(router.candidates(InputPolicy::HoveredOnly).len(), 1);
    }
}
