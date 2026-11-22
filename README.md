# expos beta

expos is a Windows file manager built with Rust and GPUI. This beta provides directory browsing in Rows, Tiles, and Icons views, selection and keyboard navigation, previews, rename, archive browsing, and file operations. Local and quick search work independently; titlebar Search requires Everything to be running.

Copyright in expos original project code is held by expos contributors. That original code is licensed under GNU GPL version 3 only; see `LICENSE`. Bundled Fluent11 artwork retains its own license, with the license text in `crates/resources/assets/licenses/fluent11/LICENSE` and attribution in `crates/resources/assets/licenses/fluent11/AUTHORS.txt`. The two folder-layer SVGs were split from upstream artwork in this project on 2026-09-16 (source Git revision `ae7000b73`). Third-party dependencies retain their own licenses and notices; consult each component's source or package metadata and `THIRD_PARTY_NOTICES.txt`.

Archive browsing covers ZIP files and, when an optional 7-Zip provider is available, `.7z` files. Other archive formats are not generally supported.

Interface controls use bundled Fluent UI System Icons across platforms. Their MIT license is in `crates/resources/assets/licenses/fluent-system/LICENSE` and `THIRD_PARTY_NOTICES.txt`.

## Build and run from source

Install a Rust toolchain with Cargo and build from the repository root in PowerShell:

```powershell
cargo run -p expos
cargo build -p expos
```

On Windows, the application stores settings, session state, operation history, themes, and caches under `%LOCALAPPDATA%/gpui-filer/v6/`.

On Linux and other non-Windows platforms, expos uses these XDG directories. Unset, empty, or relative XDG values use the listed defaults.

| Contents | Directory | Default |
| --- | --- | --- |
| Keymaps, custom themes, and the folder-preference file | `$XDG_CONFIG_HOME/expos/` | `~/.config/expos/` |
| Session and operation journal paths, including recovery records | `$XDG_STATE_HOME/expos/` | `~/.local/state/expos/` |
| Thumbnail cache | `$XDG_CACHE_HOME/expos/` | `~/.cache/expos/` |

Application options such as the selected theme and language are saved with the session. Session and journal persistence and per-folder preference updates are currently Windows-only.

On non-Windows platforms, **System default** takes the first nonempty setting from `LC_ALL`, `LC_MESSAGES`, and `LANG`. English locales select English. Japanese, `C`, `POSIX`, and unsupported locales use Japanese.

On Linux, **Open in Terminal** starts a terminal in the selected folder or the current folder when invoked from the blank area. expos uses `TERMINAL` as an executable name or path, then tries `x-terminal-emulator` and common terminal emulators on `PATH`.

On Linux, the PC view lists Home, existing XDG user directories, and **File System** (`/`). Removable and remote mounts are deferred.

## Video preview support

Video still-frame previews require `ffmpeg` on `PATH`. `ffprobe` is optional and provides metadata and seeking support. Install FFmpeg separately, add its executable directory to `PATH`, then restart expos. Other preview capabilities do not require these tools.

## Beta status

File operations can end partially: some items may complete while another item fails or is cancelled. Incomplete-operation recovery is still under development; do not assume an interrupted operation can be resumed or undone after restarting the application.

Drag and drop has partial coverage. Physical Windows validation remains incomplete for several OLE routes and failure cases. Touch and pen behavior also has outstanding physical-device validation; support varies by gesture.

This is a beta build with validation gaps. See `RELEASE_NOTES.md` for current known limitations.

## Creating a draft release

Maintainers can create a draft Windows release from GitHub Actions by running the **Release** workflow manually and entering an existing `v`-prefixed version tag. The workflow checks that the local and remote tag point to its checked-out commit and that its version matches the `expos` package version in `Cargo.toml`. It packages only the committed tag contents, builds the x86_64 Windows application, includes `RELEASE_NOTES.md` and bundled artwork attribution, vendors the locked Rust dependencies into a source archive, and uploads SHA-256 checksums with both archives.

The workflow refuses to reuse an existing GitHub release. It creates a draft and marks semantic-version prereleases as prereleases; a maintainer should review the generated release notes and assets before publishing the draft. The vendored source archive includes `.cargo/config.toml` configured for offline Cargo use, the locked dependency sources, and the source tree from the tag.
