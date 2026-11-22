#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }

    pub(crate) fn of_version(version: &str) -> Self {
        // SemVer のビルドメタデータにもハイフンが含まれ得る。
        let version = version.split('+').next().unwrap_or(version);
        if version.contains('-') {
            Self::Beta
        } else {
            Self::Stable
        }
    }

    pub(crate) fn current() -> Self {
        Self::of_version(env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::Channel;

    #[test]
    fn versions_with_a_prerelease_use_the_beta_channel() {
        for version in ["0.1.1-beta.7", "0.2.0-rc.1", "0.2.0-alpha+build-3"] {
            assert_eq!(Channel::of_version(version), Channel::Beta);
        }
        assert_eq!(Channel::Beta.as_str(), "beta");
    }

    #[test]
    fn versions_without_a_prerelease_use_the_stable_channel() {
        for version in ["0.2.0", "0.2.0+build-3"] {
            assert_eq!(Channel::of_version(version), Channel::Stable);
        }
        assert_eq!(Channel::Stable.as_str(), "stable");
        assert_eq!(
            Channel::current(),
            Channel::of_version(env!("CARGO_PKG_VERSION"))
        );
    }
}
