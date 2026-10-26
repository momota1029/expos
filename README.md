# expos beta

expos is a Windows file manager built with Rust and GPUI. This beta provides directory browsing in Rows, Tiles, and Icons views, selection and keyboard navigation, previews, rename, archive browsing, and file operations. Local and quick search work independently; titlebar Search requires Everything to be running.

Copyright in expos original project code is held by expos contributors. That original code is licensed under GNU GPL version 3 only; see `LICENSE`. Bundled Fluent11 artwork retains its own license, with the license text in `crates/resources/assets/licenses/fluent11/LICENSE` and attribution in `crates/resources/assets/licenses/fluent11/AUTHORS.txt`. The two folder-layer SVGs were split from upstream artwork in this project on 2026-09-16 (source Git revision `ae7000b73`). Third-party dependencies retain their own licenses and notices; consult each component's source or package metadata and `THIRD_PARTY_NOTICES.txt`.

Archive browsing covers ZIP files and, when an optional 7-Zip provider is available, `.7z` files. Other archive formats are not generally supported.

## Build and run from source

Install a Rust toolchain with Cargo and build from the repository root in PowerShell:

```powershell
cargo run -p gpui-filer-v6
cargo build -p gpui-filer-v6
```

The application stores settings, session state, operation history, themes, and caches under `%LOCALAPPDATA%/gpui-filer/v6/`.

## Video preview support

Video still-frame previews require `ffmpeg` on `PATH`. `ffprobe` is optional and provides metadata and seeking support. Install FFmpeg separately, add its executable directory to `PATH`, then restart expos. Other preview capabilities do not require these tools.

## Beta status

File operations can end partially: some items may complete while another item fails or is cancelled. Incomplete-operation recovery is still under development; do not assume an interrupted operation can be resumed or undone after restarting the application.

Drag and drop has partial coverage. Physical Windows validation remains incomplete for several OLE routes and failure cases. Touch and pen behavior also has outstanding physical-device validation; support varies by gesture.

This is a beta build with validation gaps. See `RELEASE_NOTES.md` for current known limitations.
