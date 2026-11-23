# Headless screenshot determinism

Run two independent captures of the same scenario on the same Linux host.

```bash
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/rows.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/navigation.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/tab-pin.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/views.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/keyboard.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/rename-tab.json
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/basic tests/shots/rename-blur.json
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
gets modification time `2020-01-02 03:04:05 UTC` by default. An optional
`fixtures/<name>.mtimes` sidecar overrides individual dates. Each record contains
a UTC timestamp in `YYYY-MM-DD HH:MM:SS` format, one tab, and an existing path
relative to the fixture. Paths can contain spaces and Japanese characters.
Blank lines and lines starting with `#` are ignored. Empty `.shot-empty` markers
keep empty folders in Git and are removed from the copy before dates are set.

Breadcrumbs and sidebar tab subtitles inside the fixture start at `fixture`
and show any subdirectories beneath it. The File Info location uses the same
display path. The address editor shows the real path for navigation.

The runner waits for the active directory and visible thumbnails before saving
a PNG. It clears hover by default and preserves keyboard focus styling.
Capture windows start at a fixed origin with reduced motion enabled.

The places scenario opens the PC view. The runner maps the fixture's Documents
and Pictures directories through a private XDG user-dirs file.

## Visual review scenarios

Run a single scenario with the fixture from the table.

~~~bash
bash scripts/shot.sh --repeat 2 tests/shots/fixtures/rich tests/shots/views-rich.json
~~~

Each screenshot stem in the table produces both `-light.png` and `-dark.png`.
The viewport is 1280 × 800 except for `narrow` at 640 × 700,
`sort-and-columns` at 1920 × 1080, and `icons-thumbs` at both 1280 × 800 and
1920 × 1080.

| Scenario | Fixture | Screenshot stems |
| --- | --- | --- |
| `menus.json` | `basic` | `menus-bar`, `menus-file`, `menus-edit`, `menus-view`, `menus-view-layout`, `menus-view-columns`, `menus-go` |
| `context-menu.json` | `basic` | `context-menu-file`, `context-menu-compression`, `context-menu-folder`, `context-menu-background`, `context-menu-creation` |
| `job-progress.json` | `basic` | `job-progress-rename-collision`, `job-progress-details`, `job-progress-details-populated` |
| `views-rich.json` | `rich` | `views-rich-rows-top`, `views-rich-rows-scrolled`, `views-rich-tiles-top`, `views-rich-tiles-scrolled`, `views-rich-icons-top`, `views-rich-icons-scrolled` |
| `icons-thumbs.json` | `rich` | `icons-thumbs-1280`, `icons-thumbs-1920`, `icons-thumbs-min-zoom`, `icons-thumbs-tiles` |
| `rows-stripes.json` | `rich` | `rows-stripes-top`, `rows-stripes-scrolled` |
| `long-names.json` | `rich` | `long-names-rows`, `long-names-tiles` |
| `narrow.json` | `rich` | `narrow-rows`, `narrow-tiles` |
| `hover.json` | `basic` | `hover-row`, `hover-layout-button`, `hover-group-button`, `hover-tab`, `hover-cleared` |
| `selection.json` | `basic` | `selection-range`, `selection-all` |
| `search.json` | `basic` | `search-everything-idle`, `search-everything-unavailable`, `search-in-folder` |
| `settings-panels.json` | `basic` | `settings-themes`, `settings-language` |
| `tabs-and-panes.json` | `basic` | `tabs-vertical`, `tabs-vertical-pinned`, `tabs-and-panes-split` |
| `sort-and-columns.json` | `rich` | `sort-name-ascending`, `sort-name-descending`, `sort-kind`, `sort-size`, `sort-date`, `group-by-date`, `group-by-kind` |
| `file-info-and-preview.json` | `basic` | `file-info-text`, `quick-preview-text`, `quick-preview-image` |
| `editors-and-empty.json` | `editors-and-empty` | `rename-editor-active`, `rename-invalid-name`, `empty-folder`, `folder-rename-editor` |

The rich fixture includes 41 root entries with separate long-name and empty
folders. The runner generates `long-names/` and its five files in the staged
copy before setting timestamps. Generating them at run time keeps the checked-in
fixture paths within Windows' `MAX_PATH` limit. Text files range from 2 bytes to
about 65 KB. The ZIP entries are plain-text filename samples, and the PNGs reuse
the basic fixture's swatch.

The Edit menu exposes keymap reload. View exposes the theme and language
submenus and File Info. The hamburger has no standalone Settings, Keymaps, or
About panels. Native Properties and the Everything provider require Windows.
The Linux search scenario exercises the resulting unavailable UI.
Grouping cycles among None, Modified Date, and Kind on click.
The `cycle_grouping` step uses the same cycle, and `icon_scale` accepts a
scale from 0.75 to 2.5.

The Linux executor does not support Create Folder, so the scenario omits
captures of the folder creation flow. It opens the rename editor with F2 on
`folders/New Folder`, which is already present in the fixture.

Job progress captures a failed rename inline and in the compact panel. The
Details action opens the File operations sheet for one collision, then a
reverse collision adds a second row to the same sheet.

Tabs and panes checks that the pinned tab survives `Ctrl+W`. The scenario uses
`down down enter` after right-clicking `projects` in the basic fixture to choose
**Open in New Pane**. The final screenshots show the folder opened as a new
vertical tab, with `ideas.md` selected.

## Pointer and readiness steps

Steps use integer coordinates in logical pixels from the top-left of the
viewport. `click` defaults to the left button.

~~~json
{"action": "click", "x": 335, "y": 282, "button": "right"}
{"action": "hover", "x": 335, "y": 282}
{"action": "wait_idle", "entry": "ideas.md"}
~~~

`hover` moves the pointer without pressing a button. Put it immediately before
the screenshot that should retain hover. The runner clears the pointer
after that capture. `wait_idle` waits for directory enumeration, pending rename
reconciliation, and three consecutive frames with the same directory geometry.
It can also require an exact entry name in the active listing. `screenshot`
includes visible thumbnails in that wait. Search and Quick Preview use explicit
settling waits for their background results.
