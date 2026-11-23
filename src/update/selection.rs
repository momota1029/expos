use crate::channel::Channel;
use install_layout::VersionDir;
use serde::Deserialize;

use super::{source::MAX_ZIP_BYTES, without_utf8_bom};

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    pub assets: Vec<Asset>,
    #[serde(skip)]
    pub metadata: Option<ReleaseMetadata>,
}

#[derive(Debug)]
pub(crate) struct ReleaseMetadata {
    pub(super) manifest: ChannelManifest,
    pub(super) sha256sums: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct Offer {
    pub version: VersionDir,
    pub tag: String,
    pub zip_url: String,
    pub sha256: String,
    pub size: u64,
    pub is_downgrade: bool,
    #[allow(dead_code)] // Available to callers that surface release notes.
    pub notes_url: String,
    pub(super) sums_url: String,
    pub(super) zip_name: String,
}

pub(super) struct ReleaseAssets<'a> {
    pub zip: &'a Asset,
    pub sums: &'a Asset,
    pub manifest: &'a Asset,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChannelManifest {
    channel: String,
    version: String,
    zip: String,
    sha256: String,
}

pub(super) fn candidates(
    installed: &VersionDir,
    choice: Channel,
    switch: bool,
    releases: &[Release],
) -> Vec<(VersionDir, usize)> {
    let mut candidates = releases
        .iter()
        .enumerate()
        .filter_map(|(index, release)| {
            let version = VersionDir::parse(&release.tag_name)?;
            let prerelease = !version.version().pre.is_empty();
            if release.draft
                || release.prerelease != prerelease
                || (prerelease && choice == Channel::Stable)
            {
                return None;
            }
            let order = version.version().cmp_precedence(installed.version());
            // Beta accepts stable successors even though the running binary becomes stable.
            ((order.is_gt() || switch) && release_assets(release).is_some())
                .then_some((version, index))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(a, _), (b, _)| b.cmp(a));
    candidates
}

pub(super) fn release_assets(release: &Release) -> Option<ReleaseAssets<'_>> {
    let find = |name: &str| {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let asset = matches.next()?;
        (matches.next().is_none() && asset.size > 0 && !asset.browser_download_url.is_empty())
            .then_some(asset)
    };
    let zip = find(&format!("expos-{}-windows-x86_64.zip", release.tag_name))?;
    find(&format!("expos-{}-source-vendored.zip", release.tag_name))?;
    (zip.size <= MAX_ZIP_BYTES).then_some(())?;
    Some(ReleaseAssets {
        zip,
        sums: find("SHA256SUMS")?,
        manifest: find("channel.json")?,
    })
}

pub(crate) fn pick_update(
    installed_version: &str,
    channel_choice: Channel,
    releases: &[Release],
) -> Option<Offer> {
    pick(
        installed_version,
        channel_choice,
        Channel::of_version(installed_version) != channel_choice,
        releases,
    )
}

pub(super) fn pick(
    installed_version: &str,
    channel_choice: Channel,
    switch: bool,
    releases: &[Release],
) -> Option<Offer> {
    let installed = VersionDir::from_version(installed_version)?;
    for (version, index) in candidates(&installed, channel_choice, switch, releases) {
        if let Some(offer) = offer_for(&installed, version, &releases[index]) {
            return (offer
                .version
                .version()
                .cmp_precedence(installed.version())
                .is_ne())
            .then_some(offer);
        }
    }
    None
}

pub(super) fn offer_for(
    installed: &VersionDir,
    version: VersionDir,
    release: &Release,
) -> Option<Offer> {
    let assets = release_assets(release)?;
    let metadata = release.metadata.as_ref()?;
    let manifest = &metadata.manifest;
    let channel = Channel::of_version(&manifest.version);
    if manifest.channel != channel.as_str()
        || VersionDir::from_version(&manifest.version).as_ref() != Some(&version)
        || manifest.zip != assets.zip.name
    {
        return None;
    }
    let sha256 = checksum_for(&metadata.sha256sums, &assets.zip.name)?;
    if !sha256.eq_ignore_ascii_case(&manifest.sha256) {
        return None;
    }
    Some(Offer {
        is_downgrade: version
            .version()
            .cmp_precedence(installed.version())
            .is_lt(),
        version,
        tag: release.tag_name.clone(),
        zip_url: assets.zip.browser_download_url.clone(),
        sha256,
        size: assets.zip.size,
        notes_url: format!(
            "https://github.com/momota1029/expos/releases/tag/{}",
            release.tag_name
        ),
        sums_url: assets.sums.browser_download_url.clone(),
        zip_name: assets.zip.name.clone(),
    })
}

pub(super) fn checksum_for(bytes: &[u8], name: &str) -> Option<String> {
    let mut found = None;
    for line in std::str::from_utf8(without_utf8_bom(bytes)).ok()?.lines() {
        let Some((digest, filename)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let filename = filename.trim_start_matches(char::is_whitespace);
        if filename.strip_prefix('*').unwrap_or(filename) == name {
            if found.is_some()
                || digest.len() != 64
                || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return None;
            }
            found = Some(digest.to_ascii_lowercase());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn release(version: &str) -> Release {
        let tag = format!("v{version}");
        let zip = format!("expos-{tag}-windows-x86_64.zip");
        let channel = Channel::of_version(version);
        let assets = [
            zip.clone(),
            format!("expos-{tag}-source-vendored.zip"),
            "SHA256SUMS".into(),
            "channel.json".into(),
        ]
        .into_iter()
        .map(|name| Asset {
            browser_download_url: format!("https://github.com/example/{name}"),
            name,
            size: 100,
        })
        .collect();
        Release {
            tag_name: tag,
            draft: false,
            prerelease: channel == Channel::Beta,
            assets,
            metadata: Some(ReleaseMetadata {
                sha256sums: format!("{HASH}  {zip}\n").into_bytes(),
                manifest: ChannelManifest {
                    channel: channel.as_str().into(),
                    version: version.into(),
                    zip,
                    sha256: HASH.into(),
                },
            }),
        }
    }

    #[test]
    fn ordinary_stable_check_ignores_prereleases_drafts_and_same_precedence() {
        let mut draft = release("4.0.0");
        draft.draft = true;
        let releases = [
            release("1.0.0"),
            release("2.0.0"),
            release("3.0.0-beta.1"),
            draft,
        ];
        let offer = pick_update("1.0.0", Channel::Stable, &releases).unwrap();
        assert_eq!(offer.tag, "v2.0.0");
        assert!(!offer.is_downgrade);
        assert!(offer.notes_url.ends_with("/v2.0.0"));
        assert!(pick_update("2.0.0+running", Channel::Stable, &releases).is_none());
        assert!(pick_update("invalid", Channel::Stable, &releases).is_none());
    }

    #[test]
    fn beta_offers_newer_stable_and_orders_numeric_prereleases() {
        let releases = [release("2.0.0-beta.2"), release("2.0.0-beta.10")];
        assert_eq!(
            pick_update("2.0.0-beta.1", Channel::Beta, &releases)
                .unwrap()
                .tag,
            "v2.0.0-beta.10"
        );
        let releases = [release("2.0.0-beta.10"), release("2.0.0")];
        assert_eq!(
            pick_update("2.0.0-beta.1", Channel::Beta, &releases)
                .unwrap()
                .tag,
            "v2.0.0"
        );
    }

    #[test]
    fn explicit_switch_can_offer_older_but_current_precedence_wins_over_downgrade() {
        let releases = [release("2.0.0"), release("3.0.0-beta.1")];
        let offer = pick_update("3.0.0-beta.1", Channel::Stable, &releases).unwrap();
        assert_eq!(offer.tag, "v2.0.0");
        assert!(offer.is_downgrade);
        assert!(pick("3.0.0-beta.1", Channel::Stable, false, &releases).is_none());

        let releases = [release("2.0.0"), release("2.0.0-beta.1")];
        assert!(pick_update("2.0.0", Channel::Beta, &releases).is_none());
    }

    #[test]
    fn incomplete_or_inconsistent_metadata_disqualifies_release() {
        let mut missing = release("4.0.0");
        missing.assets.pop();
        let mut invalid_version = release("3.0.0");
        invalid_version.metadata.as_mut().unwrap().manifest.version = "3.0.1".into();
        let mut mismatch = release("2.0.0");
        mismatch.metadata.as_mut().unwrap().sha256sums[0] = b'b';
        let releases = [missing, invalid_version, mismatch, release("1.1.0")];
        let offer = pick_update("1.0.0", Channel::Stable, &releases).unwrap();
        assert_eq!(offer.tag, "v1.1.0");
        assert_eq!(offer.sha256, HASH);
    }
}
