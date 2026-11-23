# expos beta

expos is a Windows file manager built with Rust and GPUI. This beta provides directory browsing in Rows, Tiles, and Icons views, selection and keyboard navigation, previews, rename, archive browsing, and file operations.

Press **/** to search filenames in the current folder. Matches are highlighted in the file list. The titlebar search button opens Everything Search on Windows and in-folder Quick Search on Linux. Everything Search requires Everything to be running.

Rows keeps filenames on one line and shows the full name on hover. As the pane narrows, Rows hides Kind, Date modified, and Size in that order, then restores them as space returns.
Rows alternates item background colors by default. Toggle it under **View → Alternate row colors**.

In Quick Preview, the header shows the filename. Close it with the header's close button or **Escape**.

Copyright in expos original project code is held by expos contributors. That original code is licensed under GNU GPL version 3 only; see `LICENSE`. Bundled Fluent11 artwork retains its own license, with the license text in `crates/resources/assets/licenses/fluent11/LICENSE` and attribution in `crates/resources/assets/licenses/fluent11/AUTHORS.txt`. The two folder-layer SVGs were split from upstream artwork in this project on 2026-09-16 (source Git revision `ae7000b73`). Third-party dependencies retain their own licenses and notices; consult each component's source or package metadata and `THIRD_PARTY_NOTICES.txt`.

Archive browsing covers ZIP files and, when an optional 7-Zip provider is available, `.7z` files. Other archive formats are not generally supported.

Interface controls use bundled Fluent UI System Icons across platforms. Their MIT license is in `crates/resources/assets/licenses/fluent-system/LICENSE` and `THIRD_PARTY_NOTICES.txt`.

## Install on Windows

The [Windows installation guide](release/README.md) covers installing a
release ZIP, switching versions, and uninstalling.
Taskbar pins created from an installed expos window follow the active version.
Use **Updates** in launcher-installed builds, including WSL developer installs,
to check the stable or beta channel.
See the [updater guide](release/README.md#updater) for channel behavior and
offline testing.

## Build and run from source

Install a Rust toolchain with Cargo and build from the repository root in PowerShell:

```powershell
cargo run -p expos
cargo build -p expos
```

On Windows, expos started by the installed launcher stores settings, session state, operation history, themes, and caches in `%LOCALAPPDATA%\expos\data\stable\` for stable releases and `%LOCALAPPDATA%\expos\data\beta\` for prereleases. On its first run, each channel copies existing data from `%LOCALAPPDATA%\gpui-filer\v6\` and rebuilds its cache. The legacy files stay in place. Running the executable directly uses `%LOCALAPPDATA%\gpui-filer\v6\`. Set `EXPOS_DATA_DIR` to an absolute path to override the Windows data directory.

On Linux and other non-Windows platforms, expos uses these XDG directories. Unset, empty, or relative XDG values use the listed defaults.

| Contents | Directory | Default |
| --- | --- | --- |
| Keymaps, custom themes, and the folder-preference file | `$XDG_CONFIG_HOME/expos/` | `~/.config/expos/` |
| Session and operation journal paths, including recovery records | `$XDG_STATE_HOME/expos/` | `~/.local/state/expos/` |
| Thumbnail cache | `$XDG_CACHE_HOME/expos/` | `~/.cache/expos/` |

On Windows and Linux, expos saves and restores the session, including open tabs and application options such as the selected theme and language. On Linux, a session save failure during close is logged and the window can still close. Headless screenshots bypass session storage. Operation journal persistence and per-folder preference updates are Windows-only.

On non-Windows platforms, **System default** takes the first nonempty setting from `LC_ALL`, `LC_MESSAGES`, and `LANG`. English locales select English. Japanese, `C`, `POSIX`, and unsupported locales use Japanese.

On Linux, **Open in Terminal** starts a terminal in the selected folder or the current folder when invoked from the blank area. expos uses `TERMINAL` as an executable name or path, then tries `x-terminal-emulator` and common terminal emulators on `PATH`.

On Linux, **Delete** moves ordinary files and directories to the desktop Trash. **Ctrl+Z** restores items with a confirmed receipt from the current app session. If the original name is occupied, Undo leaves the item in Trash. The operation details show when an item was moved but its receipt could not be confirmed.

On Linux, the PC view lists Home, existing XDG user directories, and **File System** (`/`). Removable and remote mounts are deferred.

In inline **Rename**, **Enter** saves and **Escape** cancels. Clicking elsewhere in the app saves a changed, nonempty name. **Tab** and **Shift+Tab** save and edit the adjacent item in the displayed order. At either end of the list, editing finishes. A failed Tab rename keeps the current editor open for correction.

On Linux, inline **Rename** changes the name of an ordinary file or folder in its current directory. It refuses existing destinations and supports case-only changes on case-sensitive filesystems. Filesystem Undo for Linux rename is unavailable.

## Video preview support

Video still-frame previews require `ffmpeg` on `PATH`. `ffprobe` is optional and provides metadata and seeking support. Install FFmpeg separately, add its executable directory to `PATH`, then restart expos. Other preview capabilities do not require these tools.

## Beta status

Open **Details** in Job progress to see recent file operations, their status, and any recorded failure reason. The panel also shows byte counts for operations that report transfer progress.

File operations can end partially: some items may complete while another item fails or is cancelled. Incomplete-operation recovery is still under development; do not assume an interrupted operation can be resumed or undone after restarting the application.

Drag and drop has partial coverage. Physical Windows validation remains incomplete for several OLE routes and failure cases. Touch and pen behavior also has outstanding physical-device validation; support varies by gesture.

This is a beta build with validation gaps. See `RELEASE_NOTES.md` for current known limitations.

## Creating a draft release

Maintainers can create a draft Windows release from GitHub Actions by running the **Release** workflow manually and entering an existing `v`-prefixed version tag. The workflow checks that the local and remote tag point to its checked-out commit and that its version matches the `expos` package version in `Cargo.toml`. It packages only the committed tag contents, builds the x86_64 Windows application, includes `RELEASE_NOTES.md` and bundled artwork attribution, vendors the locked Rust dependencies into a source archive, and uploads SHA-256 checksums with both archives.

The workflow refuses to reuse an existing GitHub release. It creates a draft and marks semantic-version prereleases as prereleases; a maintainer should review the generated release notes and assets before publishing the draft. The vendored source archive includes `.cargo/config.toml` configured for offline Cargo use, the locked dependency sources, and the source tree from the tag.
