[CmdletBinding()]
param(
    [string]$Root,
    [switch]$Force,
    [switch]$RemoveData,
    [switch]$NoShortcut,
    [string]$StartMenuDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Install-Common.ps1')

function Remove-ExposOwnedShortcut([string]$Path, [string]$Launcher) {
    if (!$Path -or $null -eq (Get-ExposAttributes $Path)) { return }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    if ($shortcut.TargetPath -and (Resolve-ExposFileSystemPath $shortcut.TargetPath) -eq $Launcher) {
        [IO.File]::Delete($Path)
    }
}

function Get-ExposRemovals([string]$Path, [switch]$IncludeData) {
    foreach ($name in @('expos.exe', 'current')) {
        $file = Join-Path $Path $name
        if ($null -ne (Get-ExposAttributes $file)) {
            [pscustomobject]@{ Path = $file; IsDirectory = $false }
        }
    }
    $staging = Join-Path $Path '.staging'
    if ($null -ne (Get-ExposAttributes $staging -Directory)) {
        [pscustomobject]@{ Path = $staging; IsDirectory = $true }
    }
    foreach ($directory in [IO.Directory]::EnumerateDirectories($Path)) {
        $name = [IO.Path]::GetFileName($directory)
        if ($name -like 'v*' -and (Test-ExposVersion ($name.Substring(1)))) {
            $null = Get-ExposAttributes $directory -Directory
            [pscustomobject]@{ Path = $directory; IsDirectory = $true }
        }
    }
    $data = Join-Path $Path 'data'
    if ($IncludeData -and $null -ne (Get-ExposAttributes $data -Directory)) {
        [pscustomobject]@{ Path = $data; IsDirectory = $true }
    }
}

function Remove-ExposRoot([string]$Path, [string]$Marker, [string]$LockPath, [switch]$IncludeData) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        try {
            $handle = [ExposInstall.NativePaths]::OpenLockForRemoval($LockPath)
            break
        } catch {
            if ((Get-ExposErrorCode $_.Exception) -notin @(32, 33)) { throw }
            if ($timer.Elapsed.TotalSeconds -ge 5) {
                throw "Could not remove the install lock at $LockPath. Close any running expos launcher and retry."
            }
            Start-Sleep -Milliseconds 100
        }
    }
    try {
        # A concurrent install may have finished after the shared lock closed.
        if (@(Get-ExposRemovals $Path -IncludeData:$IncludeData).Count) {
            throw "Install root changed during uninstall: $Path. Run uninstall again to remove it."
        }
        if (!(Test-ExposMarker $Marker)) { throw "Missing expos install marker: $Marker" }
        [ExposInstall.NativePaths]::DeleteLockOnClose($handle)
        [IO.File]::Delete($Marker)
    } finally {
        $handle.Dispose()
    }
    # Preserved data or a newly started installer can keep the root nonempty.
    if (![IO.Directory]::GetFileSystemEntries($Path).Length) {
        [IO.Directory]::Delete($Path)
    }
}

if (!$PSBoundParameters.ContainsKey('Root')) { $Root = Get-ExposDefaultRoot }
$Root = Resolve-ExposManagedDirectory $Root -InstallRoot
$marker = Join-Path $Root '.expos-install'
if (!(Test-ExposMarker $marker)) { throw "Refusing to uninstall without an expos install marker: $marker" }
$shortcutPath = if (!$NoShortcut) { Get-ExposShortcutPath $StartMenuDir }
$dataPath = Join-Path $Root 'data'
if ($RemoveData -and !$Force -and
    !$PSCmdlet.ShouldContinue("Permanently delete all expos channel data in $dataPath?", 'Remove expos data')) {
    Write-Host 'Uninstall cancelled.'
    return
}
$launcher = Join-Path $Root 'expos.exe'
$lockPath = Join-Path $Root '.lock'
$lock = Enter-ExposInstallLock $Root
try {
    if (!(Test-ExposMarker $marker)) { throw "Missing expos install marker: $marker" }
    $removals = @(Get-ExposRemovals $Root -IncludeData:$RemoveData)
    # -Force never bypasses the in-use checks.
    Assert-ExposExeIdle $launcher
    foreach ($item in $removals) {
        if (!$item.IsDirectory) { continue }
        if ([IO.Path]::GetFileName($item.Path) -like 'v*') {
            Assert-ExposExeIdle (Join-Path $item.Path 'expos.exe')
        }
        Assert-ExposTreeSafe $item.Path
    }
    Remove-ExposOwnedShortcut $shortcutPath $launcher
    foreach ($item in $removals) {
        if ($item.IsDirectory) {
            [IO.Directory]::Delete($item.Path, $true)
        } else {
            [IO.File]::Delete($item.Path)
        }
    }
} finally {
    $lock.Dispose()
}

Remove-ExposRoot $Root $marker $lockPath -IncludeData:$RemoveData
Write-Host "Uninstalled expos from $Root"
if (!$RemoveData -and [IO.Directory]::Exists($dataPath)) {
    Write-Host "Kept expos data in $dataPath"
}
