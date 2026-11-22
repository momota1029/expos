# Headless screenshot determinism

Run two independent captures of the same scenario on the same Linux host.

```bash
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/rows.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/navigation.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/views.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/keyboard.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/places tests/shots/places.json
```

An optional last argument overrides the output directory. Each pass starts with
a fresh fixture copy, Xvfb display, home directory, and app state. The check uses
ImageMagick `convert` to decode PNGs and compare dimensions and RGBA samples.
On success it publishes the first pass's PNGs to the output directory. A pixel
difference, missing PNG, or decoding failure lists the affected filenames and
returns status `1`. Existing PNGs in the output directory are ignored by the
comparison.

Every entry in the fixture copy, including directories and the fixture root,
gets modification time `2020-01-02 03:04:05 UTC`. The capture app uses `TZ=UTC`,
so rows display `2020-01-02 03:04` regardless of the host timezone. Breadcrumbs
and sidebar tab subtitles inside the fixture start at `fixture` and show any
subdirectories beneath it. The File Info location uses the same display path.
While the address field is being edited, it shows the real path for navigation.
The capture runner clears hover and waits for visible
thumbnails before saving a PNG. Keyboard focus styling stays active for keyboard
scenarios. Capture windows start at a fixed origin with reduced motion enabled.

The places scenario opens the PC view. The runner maps the fixture's Documents
and Pictures directories through a private XDG user-dirs file.
