use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(windows)]
pub(crate) fn install_root() -> Option<PathBuf> {
    std::env::var_os("EXPOS_INSTALL_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
pub(crate) fn version_dir() -> Option<PathBuf> {
    std::env::var_os("EXPOS_VERSION_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn is_running_version_dir(root: &Path, dir: &Path, version: &str) -> bool {
    matches_running_version_dir(root, dir, version, cfg!(debug_assertions))
}

fn matches_running_version_dir(root: &Path, dir: &Path, version: &str, allow_dev: bool) -> bool {
    if dir == root.join(format!("v{version}")) {
        return true;
    }
    if !allow_dev || dir.parent() != Some(root) {
        return false;
    }
    // 開発インストールは同じビルドを固有のディレクトリ名で配置する。
    let base = version.split(['-', '+']).next().unwrap_or(version);
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(&format!("v{base}-alpha.dev."))
                && install_layout::VersionDir::parse(name).is_some()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_version_directory_matches_in_both_profiles() {
        let root = Path::new("expos");
        let version = "0.1.0-beta.1";
        let dir = root.join("v0.1.0-beta.1");
        for allow_dev in [false, true] {
            assert!(matches_running_version_dir(root, &dir, version, allow_dev));
            assert!(!matches_running_version_dir(
                root,
                &Path::new("other").join(&dir),
                version,
                allow_dev,
            ));
        }
    }

    #[test]
    fn developer_alias_requires_debug_and_the_same_version_core_and_parent() {
        let root = Path::new("expos");
        let name = "v0.1.0-alpha.dev.20261123T093012123456789.aa51d95d-8349-49d2-b0dd-b157f36a2c64";
        let dir = root.join(name);
        for version in ["0.1.0", "0.1.0-beta.1", "0.1.0+build"] {
            assert!(matches_running_version_dir(root, &dir, version, true));
            assert!(!matches_running_version_dir(root, &dir, version, false));
        }

        for dir in [
            Path::new("other").join(name),
            root.join("nested").join(name),
            root.join("v0.1.1-alpha.dev.20261123T093012-0"),
            root.join("v0.1.0-beta.dev.20261123T093012-0"),
            root.join("v0.1.0-alpha.dev-extra.20261123T093012-0"),
            root.join("v0.1.0-alpha.dev."),
            root.join("0.1.0-alpha.dev.20261123T093012-0"),
        ] {
            assert!(
                !matches_running_version_dir(root, &dir, "0.1.0-beta.1", true),
                "unexpected match for {}",
                dir.display(),
            );
        }
    }
}
