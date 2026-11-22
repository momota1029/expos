# Windows installation

Use Windows PowerShell 5.1 or PowerShell 7 on Windows. Keep
`Install-Expos.ps1`, `Uninstall-Expos.ps1`, and `Install-Common.ps1` in the
same directory. Pass the Windows release ZIP directly to the installer.

```powershell
.\release\Install-Expos.ps1 -Zip .\expos-v0.1.0-windows-x86_64.zip -Sha256File .\SHA256SUMS
.\release\Uninstall-Expos.ps1
```

`-Sha256File` looks up the ZIP's filename in `SHA256SUMS`. To supply the
expected digest directly, use `-Sha256 <64-digit hex digest>`. The checksum
arguments are optional and mutually exclusive.

By default, both scripts use `%LOCALAPPDATA%\expos`. An explicit `-Root`
takes precedence over `EXPOS_INSTALL_ROOT`, which takes precedence over the
default. Use a dedicated local directory for a custom root. Set the same
`EXPOS_INSTALL_ROOT` when running its launcher. With
`EXPOS_LAUNCHER_WAIT=1`, the launcher waits for the app and returns its exit
code.

```text
%LOCALAPPDATA%\expos\
  expos.exe        launcher
  current          active version directory name, for example v0.1.0
  .lock            shared installer and launcher lock
  .expos-install   install ownership marker
  v0.1.0\          versioned release contents, including expos.exe
  data\stable\    stable channel data
  data\beta\      prerelease channel data
```

Installing a new version keeps older payloads and switches `current` after
publishing the payload and launcher. Use `-Force` to replace an installed
version after closing it. Use `-LauncherExe <path>` to supply a launcher
when the ZIP lacks `launcher\expos-launcher.exe`.

The scripts manage `%APPDATA%\Microsoft\Windows\Start Menu\Programs\expos.lnk`
by default. Use `-StartMenuDir <path>` to redirect the shortcut, or
`-NoShortcut` to skip shortcut handling. Close all running versions before
uninstalling. An in-use executable blocks uninstall even with `-Force`.

Uninstall removes the shortcut and the managed installation files and
versions. It keeps `<root>\data` and removes the install root if it is empty.
Reinstalling into that root reuses the preserved data. To delete both
channels' data, run the following while the installation is still marked.
PowerShell asks for confirmation. Add `-Force` to skip the prompt.

```powershell
.\release\Uninstall-Expos.ps1 -RemoveData
```

## Test with a temporary install

Run the host test to exercise the scripts with disposable ZIPs, a temporary
install root, and a temporary Start Menu directory.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\release\Test-Install.ps1
```

To try a real release, use a temporary install root and point the app at a
data directory inside it. The cleanup explicitly removes the test data.

```powershell
$root = Join-Path $env:TEMP ("expos-test-" + [guid]::NewGuid().ToString('N'))
$oldRoot = $env:EXPOS_INSTALL_ROOT
$oldWait = $env:EXPOS_LAUNCHER_WAIT
$oldData = $env:EXPOS_DATA_DIR
try {
    .\release\Install-Expos.ps1 -Zip .\expos-v0.1.0-windows-x86_64.zip -Root $root -NoShortcut
    $env:EXPOS_INSTALL_ROOT = $root
    $env:EXPOS_LAUNCHER_WAIT = '1'
    $env:EXPOS_DATA_DIR = Join-Path $root 'data\test'
    $app = Start-Process -FilePath (Join-Path $root 'expos.exe') -Wait -PassThru
    "expos exit code $($app.ExitCode)"
} finally {
    $env:EXPOS_INSTALL_ROOT = $oldRoot
    $env:EXPOS_LAUNCHER_WAIT = $oldWait
    $env:EXPOS_DATA_DIR = $oldData
    if (Test-Path -LiteralPath (Join-Path $root '.expos-install')) {
        .\release\Uninstall-Expos.ps1 -Root $root -NoShortcut -RemoveData -Force
    }
}
```

To test shortcut creation, use `-StartMenuDir` with a separate temporary
directory on both calls and omit `-NoShortcut`.

## CI

See [release automation](CI.md).

## Data directories

The launcher uses `%LOCALAPPDATA%\expos\data\stable\` for stable releases and
`%LOCALAPPDATA%\expos\data\beta\` for prereleases. Each channel seeds its data
from the legacy directory independently. An absolute `EXPOS_DATA_DIR` overrides
these defaults for test runs. Installs and forced reinstalls preserve both
channels. Install and uninstall leave the legacy data at
`%LOCALAPPDATA%\gpui-filer\v6` untouched. See the
[project README](../README.md#build-and-run-from-source) for migration and
direct-execution paths.

## Updater

Installed expos shows **Updates** in the menu bar. **Check for updates**
looks at published releases in the selected channel. Stable accepts stable
releases. Beta accepts both prereleases and stable releases, so `0.2.0`
supersedes `0.2.0-beta.12`. An ordinary check only offers a newer version.
Choosing a different channel can offer an older version and requires an extra
confirmation before installing it. Switching between stable and prerelease
builds uses the destination build's separate data directory.

**Check automatically** runs about 30 seconds after launcher startup, at most
once every 12 hours. Set `EXPOS_NO_UPDATE_CHECK=1` to skip the startup check
without disabling manual checks. The update notice installs the verified
payload in the background. A second confirmation saves the session and
restarts through the launcher.

### Exercise an update offline on Windows

Use a Windows ZIP for a baseline build that contains the updater, a launcher
and `VERSION.txt`, and another Windows ZIP for the offered build. For a stable
upgrade, use an offered version newer than the baseline. For a downgrade check,
use a prerelease baseline newer than the offered stable version and select
**Updates → Channel → Stable**. Keep the ZIP names produced by packaging.

Run the following in PowerShell from the repository root. Adjust the first
three variables to match the two packages. The script computes both checksums
from the offered ZIP and writes a GitHub-shaped fixture. The source ZIP asset
is listed for completeness and is never downloaded by the updater.

```powershell
$baselineZip = (Resolve-Path '.\expos-v0.1.1-beta.123-windows-x86_64.zip').Path
$offeredZip = (Resolve-Path '.\expos-v0.2.0-windows-x86_64.zip').Path
$version = '0.2.0'
$tag = "v$version"
$zipName = "expos-$tag-windows-x86_64.zip"
$prerelease = $version.Split('+')[0].Contains('-')
$channel = if ($prerelease) { 'beta' } else { 'stable' }
$test = Join-Path $env:TEMP ("expos-updater-" + [guid]::NewGuid().ToString('N'))
$root = Join-Path $test 'install'
$data = Join-Path $test 'data'
$fixture = Join-Path $test 'fixture'
New-Item -ItemType Directory -Path $fixture | Out-Null
$utf8 = [System.Text.UTF8Encoding]::new($false)
$zip = Join-Path $fixture $zipName
Copy-Item -LiteralPath $offeredZip -Destination $zip
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $fixture 'SHA256SUMS'), "$hash  $zipName`n", $utf8)
$manifest = @{
    channel = $channel; version = $version; commit = 'offline-fixture'
    zip = $zipName; sha256 = $hash; built_at = [DateTime]::UtcNow.ToString('o')
}
[IO.File]::WriteAllText((Join-Path $fixture 'channel.json'), ($manifest | ConvertTo-Json), $utf8)
$assets = foreach ($name in @($zipName, 'SHA256SUMS', 'channel.json')) {
    @{
        name = $name
        browser_download_url = "https://fixture.invalid/$name"
        size = (Get-Item -LiteralPath (Join-Path $fixture $name)).Length
    }
}
$assets += @{
    name = "expos-$tag-source-vendored.zip"
    browser_download_url = "https://fixture.invalid/expos-$tag-source-vendored.zip"
    size = 1
}
$releases = @(@{ tag_name = $tag; draft = $false; prerelease = $prerelease; assets = $assets })
[IO.File]::WriteAllText((Join-Path $fixture 'releases.json'), (ConvertTo-Json -InputObject $releases -Depth 6), $utf8)

$previous = @{}
$overrides = @{
    EXPOS_INSTALL_ROOT = $root
    EXPOS_UPDATE_SOURCE = "dir:$fixture"
    EXPOS_DATA_DIR = $data
    EXPOS_NO_UPDATE_CHECK = '1'
}
try {
    .\release\Install-Expos.ps1 -Zip $baselineZip -Root $root -NoShortcut
    foreach ($name in $overrides.Keys) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $overrides[$name], 'Process')
    }
    Start-Process -FilePath (Join-Path $root 'expos.exe')
    Read-Host 'Check for updates, install, restart, then close all test windows and press Enter'
    Get-Content -LiteralPath (Join-Path $root 'current')
    Get-Content -LiteralPath (Join-Path $data 'update-settings-v1.json')
} finally {
    foreach ($name in $previous.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
}
```

After an accepted install, `current` should contain the offered tag and
`<root>\v<version>\VERSION.txt` should contain the offered version. To check
rollback, change a byte in the fixture ZIP after the offer appears and before
installing it. The updater should report a checksum or size failure and leave
`current` on the baseline. To test startup checks, omit
`EXPOS_NO_UPDATE_CHECK` and wait about 30 seconds. To exercise direct launch,
keep the three path overrides and start the updater-enabled application
executable directly. Direct launch with the fixture supports manual checks
against the same temporary install. Delete the temporary test directory after
closing its application processes.

## Developer loop

From WSL2 with Windows interoperability, `cargo-xwin`, and LLVM 18 installed:

```bash
scripts/dev-install.sh
scripts/dev-install.sh --root 'C:\Users\momot\Desktop\expos-dev' --dry-run
scripts/dev-install.sh --root '/mnt/c/Users/momot/Desktop/expos-dev' --no-build --keep 3 --shortcut
```

The default root is `%LOCALAPPDATA%\expos`. A custom root must be a dedicated
directory on a local Windows drive. The installer accepts an existing managed
expos root or an empty root. If the old `expos-dev` folder contains a hand-copied
executable, choose a fresh directory for the versioned installation.

The script runs `cargo xwin build --locked -p expos -p expos-launcher --target
x86_64-pc-windows-msvc` in the debug profile and uses `target/xwin-head` for the
build output. It creates `target/xwin-tools/llvm-rc` if missing so LLVM can
resolve GPUI's resource includes. `--no-build` reuses both exes from that
output. Rebuild after changing the Cargo version or the locked Zed revision.
The release profile requires `fxc.exe` while building `gpui_windows`.

The script mirrors `shaders.hlsl`, `color_text_raster.hlsl`, and
`alpha_correction.hlsl` from the locked Cargo checkout to the corresponding
Linux path on `C:`. For example, `/home/.../gpui_windows/src` becomes
`C:\home\...\gpui_windows\src`. It also mirrors them to the install drive if
different. GPUI reads these files at debug startup. The printed PowerShell
launch command and the optional Start Menu shortcut set the working directory
to the install root so the app resolves the shaders on that drive.

Every run creates a new directory such as
`v0.1.0-alpha.dev.20261123T093012123456789.aa51d95d-8349-49d2-b0dd-b157f36a2c64`.
The core version comes from `Cargo.toml`, followed by a UTC timestamp and a
unique run ID. SemVer puts `alpha` below `beta` and below the stable version of
the same core. The launcher uses `current` when it points to a complete
payload. SemVer order determines the fallback if that pointer is unavailable.

`Install-Expos.ps1 -FromDir` stages `expos.exe`,
`launcher\expos-launcher.exe`, and `VERSION.txt` under `<root>\.staging`,
publishes the new version, and replaces `current` under the same lock as a ZIP
install. A running app keeps its own executable. The next launch through
`<root>\expos.exe` uses the new payload. `--keep N` retains the newest N
developer versions by timestamp across Cargo versions, plus the active
version and any in-use versions. The default is five. Release versions are
outside this pruning pass. `--shortcut` refreshes the shared `expos.lnk` entry
to launch this root. `--dry-run` previews the plan without building or writing
files.

On a launcher start, the app uses its Cargo version to select the data channel. For
example, a build of `0.1.0-beta.1` uses `%LOCALAPPDATA%\expos\data\beta`
even though its installed folder starts with `v0.1.0-alpha.dev.`. This applies
to custom install roots as well. The first launcher start seeds an absent
channel directory from `%LOCALAPPDATA%\gpui-filer\v6` if that legacy directory
exists. To reuse the legacy directory in place, set the override before
running the printed launch command:

```powershell
$env:EXPOS_DATA_DIR = Join-Path $env:LOCALAPPDATA 'gpui-filer\v6'
```

`--data-dir PATH` prints the equivalent PowerShell assignment for an absolute
Windows or WSL path. The override applies to the process launched from that
shell. Use the developer script to refresh these debug installs.
