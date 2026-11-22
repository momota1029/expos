## CI

| Event | Result |
| --- | --- |
| Push to `main` with a change outside `docs/**`, `**/*.md`, `tests/shots/**`, and `daily_report/**` | Build and publish an automated beta prerelease. |
| Run **Release** manually from `main` with an existing tag such as `v0.2.0` | Build a draft stable release for review and manual publication. |
| Run **Release** manually with a prerelease tag such as `v0.2.0-rc.1` | Build a draft prerelease. |
| Push a tag | No release workflow run. |

The automated beta base is set in `channel-base.txt`. If that file is absent,
CI uses the core Cargo version. Workflow run 123 with the base `0.1.1`
produces version `0.1.1-beta.123`, tag `v0.1.1-beta.123`, and the release
title `beta 0.1.1-beta.123`. CI changes the `expos` manifest and lockfile
entries in the build checkout and copies both into the vendored source ZIP.
For manual dispatch, the checked-out tag must match the Cargo version and
still resolve to the built commit on GitHub before release creation.

The Windows runner checks the locked workspace and then builds from vendored
source with Cargo offline. Both release types include the Windows ZIP, the
vendored source ZIP, `SHA256SUMS`, and `channel.json`. The Windows ZIP includes
`VERSION.txt` and, when the tagged or built source has a launcher,
`launcher/expos-launcher.exe`. The channel manifest records the channel,
version, source commit, Windows ZIP name and SHA-256, and UTC build time.

A newer eligible `main` run cancels an older beta run. Before publication, CI
checks for a higher published beta run number and confirms the built commit
is still the tip of `main`. Even a docs-only push can move that tip while a
build is running. After publication, CI retains the 20 highest run numbers
among published prereleases with an automated beta tag, workflow marker, and
the Actions bot as author. It deletes older matching releases and their tags.
Manual runs use a separate concurrency group. If a beta run stops after
reserving its tag but before creating a release, inspect the tag and release
state before retrying.
