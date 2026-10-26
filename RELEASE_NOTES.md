# expos beta release notes

This beta snapshot contains the current v6 Windows file manager: directory views, selection and navigation, previews, rename, archive browsing, and file operations. Local and quick search work independently; titlebar Search requires Everything to be running. Archive browsing covers ZIP and optional 7-Zip-backed `.7z` files, not arbitrary archive formats. This note summarizes known limits rather than claiming a complete release validation.

## Known limitations and validation gaps

- Copy, move, and delete workflows can produce partial results. Some items may complete before another fails or is cancelled. Interruption recovery remains incomplete, and operation undo/recovery evidence is not a guarantee across application restarts.
- Drag and drop is partial. Several Explorer OLE routes, failure paths, and target combinations have not received physical Windows validation.
- Touch and pen interaction has outstanding hardware validation; some touch gestures and docking/drop paths are unsupported or incomplete.
- Video still-frame previews require `ffmpeg` on `PATH`; `ffprobe` is optional for metadata and seeking. Other preview capabilities do not require these tools.
- Validation is ongoing. Automated regressions cover selected routes, while physical hardware behavior and some file-operation failure/recovery cases remain unverified.

Build and run instructions are in `README.md`.
