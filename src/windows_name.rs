use std::path::Path;

pub(crate) fn is_single_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        // Path::components は host の規則で解釈されるため、Windows 用の検証では区切り文字を両方拒否する。
        && !name.contains(['\\', '/'])
        && !name.contains(['<', '>', ':', '"', '|', '?', '*'])
        && !name.chars().any(|character| character <= '\u{001f}')
        && !name.ends_with([' ', '.'])
        && !is_reserved_dos_device_name(name)
        && Path::new(name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn is_reserved_dos_device_name(name: &str) -> bool {
    let basename = name.split('.').next().unwrap_or(name);
    ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|reserved| basename.eq_ignore_ascii_case(reserved))
        || ["COM", "LPT"].iter().any(|prefix| {
            (1..=9).any(|number| {
                let reserved = format!("{prefix}{number}");
                basename.eq_ignore_ascii_case(&reserved)
            }) || ["¹", "²", "³"].iter().any(|suffix| {
                let reserved = format!("{prefix}{suffix}");
                basename.eq_ignore_ascii_case(&reserved)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::is_single_name;

    #[test]
    fn accepts_ordinary_unicode_names_and_rejects_reserved_names() {
        assert!(is_single_name("report 2026.txt"));
        assert!(is_single_name("日本語"));
        for name in ["CON", "nul.txt", "COM1", "LPT9.log", "COM¹.txt"] {
            assert!(!is_single_name(name), "{name}");
        }
    }

    #[test]
    fn rejects_path_syntax_controls_and_trailing_dot_or_space() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a:b",
            "a?b",
            "a\u{001f}b",
            "name.",
            "name ",
        ] {
            assert!(!is_single_name(name), "{name:?}");
        }
    }
}
