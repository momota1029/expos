//! ポインター操作のプラットフォーム非依存な表現。
//!
//! ボタン・クリック/ドラッグ種別・回数・修飾キーが binding の照合単位になる。

use crate::Modifiers;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseGesture {
    Click,
    Drag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MouseChord {
    pub gesture: MouseGesture,
    pub button: MouseButton,
    pub count: usize,
    pub modifiers: Modifiers,
}

impl MouseChord {
    pub fn new(button: MouseButton, count: usize, modifiers: Modifiers) -> Self {
        Self {
            gesture: MouseGesture::Click,
            button,
            count,
            modifiers,
        }
    }

    pub fn drag(button: MouseButton, modifiers: Modifiers) -> Self {
        Self {
            gesture: MouseGesture::Drag,
            button,
            count: 1,
            modifiers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_distinguish_count_button_and_modifiers() {
        let plain = MouseChord::new(MouseButton::Left, 1, Modifiers::default());
        let double = MouseChord::new(MouseButton::Left, 2, Modifiers::default());
        let middle = MouseChord::new(MouseButton::Middle, 1, Modifiers::default());
        let control = MouseChord::new(
            MouseButton::Left,
            1,
            Modifiers {
                control: true,
                ..Default::default()
            },
        );

        assert_ne!(plain, double);
        assert_ne!(plain, middle);
        assert_ne!(plain, control);
    }
}
