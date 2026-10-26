//! キー入力のプラットフォーム非依存な表現。
//!
//! プラットフォームのイベントはアダプタでここへ正規化してから binding と照合する。
//! このモジュールは GPUI、Win32、ファイルシステムのいずれも参照しない。

/// 修飾キーの状態。`platform` は Windows キー(Super)を表す。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

/// 正規化済みのキー名。
///
/// 大文字小文字はプラットフォームやレイアウトで揺れるため、常に小文字へ寄せて保持する。
/// binding の照合はこの形だけで行う。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyName(String);

impl KeyName {
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(name.as_ref().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 修飾キーとキー名の組。binding を引く単位。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub modifiers: Modifiers,
    pub key: KeyName,
}

impl KeyChord {
    pub fn new(modifiers: Modifiers, key: impl AsRef<str>) -> Self {
        Self {
            modifiers,
            key: KeyName::new(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_compare_regardless_of_case() {
        assert_eq!(KeyName::new("Escape"), KeyName::new("escape"));
        assert_ne!(KeyName::new("enter"), KeyName::new("numpad enter"));
        assert_eq!(KeyName::new("F5").as_str(), "f5");
    }

    #[test]
    fn chords_distinguish_modifiers() {
        let plain = KeyChord::new(Modifiers::default(), "tab");
        let control = KeyChord::new(
            Modifiers {
                control: true,
                ..Default::default()
            },
            "tab",
        );
        let with_shift = KeyChord::new(
            Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            "tab",
        );

        assert_ne!(plain, control);
        assert_ne!(control, with_shift);
        assert_eq!(
            control,
            KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "TAB"
            )
        );
    }
}
