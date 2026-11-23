[CmdletBinding(DefaultParameterSetName = 'NoHash')]
param(
    [Parameter(Mandatory = $true, Position = 0, ParameterSetName = 'NoHash')]
    [Parameter(Mandatory = $true, Position = 0, ParameterSetName = 'Hash')]
    [Parameter(Mandatory = $true, Position = 0, ParameterSetName = 'HashFile')]
    [string]$Zip,
    [Parameter(Mandatory = $true, ParameterSetName = 'Directory')]
    [string]$FromDir,
    [Parameter(ParameterSetName = 'Directory')]
    [ValidateRange(0, 2147483647)]
    [int]$KeepDev,
    [Parameter(ParameterSetName = 'Hash', Mandatory = $true)]
    [ValidatePattern('\A[0-9A-Fa-f]{64}\z')]
    [string]$Sha256,
    [Parameter(ParameterSetName = 'HashFile', Mandatory = $true)]
    [string]$Sha256File,
    [string]$Root,
    [switch]$Force,
    [switch]$NoShortcut,
    [string]$StartMenuDir,
    [string]$LauncherExe
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Install-Common.ps1')

function Expand-ExposZip([IO.Stream]$Stream, [string]$Destination) {
    $archive = [IO.Compression.ZipArchive]::new($Stream, 'Read', $true)
    try {
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName.Replace('/', '\')
            if ([IO.Path]::IsPathRooted($name)) { throw "Unsafe ZIP entry path: $($entry.FullName)" }
            $directory = $name.EndsWith('\', [StringComparison]::Ordinal)
            if ($directory) { $name = $name.Substring(0, $name.Length - 1) }
            foreach ($segment in $name.Split('\')) {
                if (!$segment -or $segment.EndsWith('.') -or $segment.EndsWith(' ') -or
                    $segment.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or
                    $segment -match '\A(?:CON|PRN|AUX|NUL|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3]|CONIN\$|CONOUT\$)(?:\.|\z)') {
                    throw "Unsafe ZIP entry path: $($entry.FullName)"
                }
            }
            $attributes = $entry.ExternalAttributes
            $unixType = ($attributes -shr 16) -band 0xf000
            if (($attributes -band 0x400) -or $unixType -notin @(0, 0x4000, 0x8000)) {
                throw "ZIP entry is a link, reparse point, or special file: $($entry.FullName)"
            }
            if (!$seen.Add($name)) { throw "Duplicate ZIP entry: $($entry.FullName)" }
            $target = [IO.Path]::Combine($Destination, $name)
            if ($directory) {
                $null = [IO.Directory]::CreateDirectory($target)
                continue
            }
            $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target))
            $inputStream = $entry.Open()
            try {
                $outputStream = [IO.File]::Open($target, 'CreateNew', 'Write', 'None')
                try {
                    $inputStream.CopyTo($outputStream)
                } finally {
                    $outputStream.Dispose()
                }
            } finally {
                $inputStream.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }
}

function Copy-ExposPayload([string]$Source, [string]$Stage) {
    $null = Get-ExposAttributes (Join-Path $Source 'launcher') -Directory
    $null = [IO.Directory]::CreateDirectory((Join-Path $Stage 'launcher'))
    foreach ($name in @('expos.exe', 'VERSION.txt', 'launcher\expos-launcher.exe')) {
        $path = Join-Path $Source $name
        if ($null -eq (Get-ExposAttributes $path)) {
            if ($name -eq 'launcher\expos-launcher.exe' -and $LauncherExe) { continue }
            throw "Missing payload file: $path"
        }
        [IO.File]::Copy($path, (Join-Path $Stage $name))
    }
}

function Remove-ExposOldDevVersions([string]$Root, [string]$Current, [int]$Keep) {
    $versions = foreach ($directory in [IO.Directory]::EnumerateDirectories($Root)) {
        $name = [IO.Path]::GetFileName($directory)
        $match = [regex]::Match($name, '\Av.+-alpha\.dev\.([0-9]{8}T[0-9]{6}(?:[0-9]{9})?)(?:\.[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})?\z')
        if (!$match.Success -or !(Test-ExposVersion ($name.Substring(1)))) { continue }
        $stamp = $match.Groups[1].Value.PadRight(24, '0')
        try {
            $null = Get-ExposAttributes $directory -Directory
            $exe = Join-Path $directory 'expos.exe'
            if ($null -ne (Get-ExposAttributes $exe) -and
                $null -ne (Get-ExposAttributes (Join-Path $directory 'VERSION.txt')) -and
                (Read-ExposVersion $directory) -ceq $name) {
                [pscustomobject]@{ Path = $directory; Name = $name; Stamp = $stamp; Exe = $exe }
            }
        } catch {
            Write-Warning "Skipping developer directory $directory. $($_.Exception.Message)"
        }
    }
    foreach ($version in ($versions | Sort-Object -Property Stamp -Descending | Select-Object -Skip $Keep)) {
        if ($version.Name -eq $Current) { continue }
        try {
            if (Test-ExposExeInUse $version.Exe) {
                Write-Host "Kept running developer version $($version.Name)"
                continue
            }
            Assert-ExposTreeSafe $version.Path
            [IO.Directory]::Delete($version.Path, $true)
            Write-Host "Pruned developer version $($version.Name)"
        } catch {
            Write-Warning "Could not prune $($version.Path). $($_.Exception.Message)"
        }
    }
}

function Read-ExposVersion([string]$Stage, [string]$ZipName) {
    $path = Join-Path $Stage 'VERSION.txt'
    if ($null -ne (Get-ExposAttributes $path)) {
        if ([IO.FileInfo]::new($path).Length -gt 1024) { throw 'VERSION.txt is too long.' }
        $version = [IO.File]::ReadAllText($path) -replace '\r?\n\z', ''
    } elseif ($ZipName -cmatch '\Aexpos-v(.+)-windows-x86_64\.zip\z') {
        $version = $Matches[1]
    } elseif ($ZipName) {
        throw 'The ZIP has no VERSION.txt and its name does not contain a release version.'
    } else {
        throw 'The payload has no VERSION.txt.'
    }
    if (!(Test-ExposVersion $version)) { throw "Invalid expos version: $version" }
    return "v$version"
}

function Get-ExposExpectedHash([string]$ZipName) {
    if (!$Sha256File) { return $Sha256 }
    $found = @(
        foreach ($line in [IO.File]::ReadLines((Resolve-ExposFileSystemPath $Sha256File))) {
            if ($line -match '\A([0-9A-Fa-f]{64}) [ *](.+)\z' -and $Matches[2] -eq $ZipName) {
                $Matches[1]
            }
        }
    )
    if ($found.Count -ne 1) {
        throw "Expected exactly one SHA-256 entry for $ZipName in $Sha256File."
    }
    return $found[0]
}

function Assert-ExposInstallRoot([string]$Path, [string]$Marker) {
    foreach ($item in [IO.Directory]::EnumerateFileSystemEntries($Path)) {
        $name = [IO.Path]::GetFileName($item)
        if ($name -eq '.lock') { continue }
        # A previous uninstall may have kept channel data and retired the marker.
        if ($name -eq 'data' -and $null -ne (Get-ExposAttributes $item -Directory)) { continue }
        if ($null -ne (Get-ExposAttributes $Marker)) { return }
        throw "Install root has unmanaged files and no expos install marker: $Path"
    }
}

function Publish-ExposLauncher([string]$Source, [string]$Destination, [string]$Backup) {
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        try {
            Publish-ExposFile $Source $Destination $Backup
            return
        } catch {
            if ((Get-ExposErrorCode $_.Exception) -notin @(5, 32, 33, 1175, 1224)) { throw }
            if ($attempt -eq 6) {
                throw "Could not replace the launcher at $Destination. Close any running launcher and check write permissions. $($_.Exception.Message)"
            }
            Start-Sleep -Milliseconds 200
        }
    }
}

function Initialize-ExposShortcutProperties {
    if ('ExposInstall.ShortcutProperties' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

namespace ExposInstall {
    public static class ShortcutProperties {
        private static readonly Guid ShellLink = new Guid("00021401-0000-0000-C000-000000000046");
        private static readonly Guid AppUserModel = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3");
        private const ushort VT_LPWSTR = 31;
        private const int STGM_READ = 0;
        private const int STGM_READWRITE = 2;

        [StructLayout(LayoutKind.Sequential)]
        private struct PropertyKey {
            public Guid Format;
            public uint Id;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct PropVariant {
            public ushort Type;
            private ushort reserved1, reserved2, reserved3;
            public IntPtr String;
            // The native union also has two-word members on x86 and x64.
            private IntPtr unionPadding;
        }

        [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99")]
        [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
        private interface IPropertyStore {
            void GetCount(out uint count);
            void GetAt(uint index, out PropertyKey key);
            void GetValue(ref PropertyKey key, out PropVariant value);
            void SetValue(ref PropertyKey key, ref PropVariant value);
            void Commit();
        }

        [DllImport("ole32.dll")]
        private static extern int PropVariantClear(ref PropVariant value);

        public static void SetAppUserModelId(string path, string id) {
            object link = Activator.CreateInstance(Type.GetTypeFromCLSID(ShellLink));
            try {
                var file = (IPersistFile)link;
                file.Load(path, STGM_READWRITE);
                var store = (IPropertyStore)link;
                var key = new PropertyKey { Format = AppUserModel, Id = 5 };
                var value = new PropVariant { Type = VT_LPWSTR, String = Marshal.StringToCoTaskMemUni(id) };
                try {
                    store.SetValue(ref key, ref value);
                    store.Commit();
                    file.Save(path, true);
                } finally {
                    PropVariantClear(ref value);
                }
            } finally {
                Marshal.FinalReleaseComObject(link);
            }
        }

        public static string GetAppUserModelId(string path) {
            object link = Activator.CreateInstance(Type.GetTypeFromCLSID(ShellLink));
            try {
                ((IPersistFile)link).Load(path, STGM_READ);
                var key = new PropertyKey { Format = AppUserModel, Id = 5 };
                PropVariant value;
                ((IPropertyStore)link).GetValue(ref key, out value);
                try {
                    return value.Type == VT_LPWSTR ? Marshal.PtrToStringUni(value.String) : null;
                } finally {
                    PropVariantClear(ref value);
                }
            } finally {
                Marshal.FinalReleaseComObject(link);
            }
        }
    }
}
'@
}

function Set-ExposShortcut([string]$Path, [string]$Launcher, [string]$WorkingDirectory) {
    Initialize-ExposShortcutProperties
    $appId = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'AppUserModelID.txt')).Trim()
    $parent = [IO.Path]::GetDirectoryName($Path)
    $null = [IO.Directory]::CreateDirectory($parent)
    $temporary = Join-Path $parent ("expos-{0}.lnk" -f [guid]::NewGuid().ToString('N'))
    try {
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($temporary)
        $shortcut.TargetPath = $Launcher
        $shortcut.WorkingDirectory = $WorkingDirectory
        $shortcut.Save()
        [ExposInstall.ShortcutProperties]::SetAppUserModelId($temporary, $appId)
        Publish-ExposFile $temporary $Path
    } finally {
        [IO.File]::Delete($temporary)
    }
}

if (!$PSBoundParameters.ContainsKey('Root')) { $Root = Get-ExposDefaultRoot }
$Root = Resolve-ExposManagedDirectory $Root -InstallRoot
$shortcutPath = if (!$NoShortcut) { Get-ExposShortcutPath $StartMenuDir }
$zipName = $null
$zipStream = $null
$expectedHash = $null
if ($FromDir) {
    $FromDir = Resolve-ExposFileSystemPath $FromDir
    if ($null -eq (Get-ExposAttributes $FromDir -Directory)) {
        throw "Payload directory not found: $FromDir"
    }
} else {
    Add-Type -AssemblyName System.IO.Compression
    $zipPath = Resolve-ExposFileSystemPath $Zip
    $zipName = [IO.Path]::GetFileName($zipPath)
    $expectedHash = Get-ExposExpectedHash $zipName
    $zipStream = [IO.File]::Open($zipPath, 'Open', 'Read', 'Read')
}
try {
    if ($expectedHash) {
        $actualHash = (Get-FileHash -InputStream $zipStream -Algorithm SHA256).Hash
        if ($actualHash -ne $expectedHash) {
            throw "SHA-256 mismatch for $zipName. Expected $expectedHash, found $actualHash."
        }
        $zipStream.Position = 0
    }

    $marker = Join-Path $Root '.expos-install'
    $null = [IO.Directory]::CreateDirectory($Root)
    if ($null -eq (Get-ExposAttributes $marker)) { Assert-ExposInstallRoot $Root $marker }
    $lock = Enter-ExposInstallLock $Root
    try {
        if (!(Test-ExposMarker $marker)) {
            Assert-ExposInstallRoot $Root $marker
            Write-ExposNewText $marker $ExposInstallMarker
        }

        # An uninstaller holding the same lock cannot remove a stage while it is being written.
        $staging = Join-Path $Root '.staging'
        $null = Get-ExposAttributes $staging -Directory
        $id = [guid]::NewGuid().ToString('N')
        $stage = Join-Path $staging $id
        $oldStage = Join-Path $staging "old-$id"
        $oldLauncher = Join-Path $staging "launcher-$id.old"
        $oldCurrent = Join-Path $staging "current-$id.old"
        $launcherTemp = Join-Path $Root 'expos.exe.new'
        $currentTemp = Join-Path $Root "current-$id.new"
        $launcher = Join-Path $Root 'expos.exe'
        $current = Join-Path $Root 'current'
        $movedOld = $false
        $movedNew = $false
        $publishedLauncher = $false
        $copiedLauncher = $false
        $committed = $false
        try {
            $null = [IO.Directory]::CreateDirectory($stage)
            if ($FromDir) {
                Copy-ExposPayload $FromDir $stage
            } else {
                Expand-ExposZip $zipStream $stage
            }
            $version = Read-ExposVersion $stage $zipName
            if ($null -eq (Get-ExposAttributes (Join-Path $stage 'expos.exe'))) {
                throw 'The payload does not contain expos.exe at its root.'
            }
            $launcherSource = if ($PSBoundParameters.ContainsKey('LauncherExe')) {
                Resolve-ExposFileSystemPath $LauncherExe
            } else {
                Join-Path $stage 'launcher\expos-launcher.exe'
            }
            if ($null -eq (Get-ExposAttributes $launcherSource)) {
                throw "Launcher executable not found: $launcherSource. Include the launcher or specify -LauncherExe."
            }

            $versionPath = Join-Path $Root $version
            $replaceVersion = $null -ne (Get-ExposAttributes $versionPath -Directory)
            if ($replaceVersion -and !$Force) {
                throw "$version is already installed. Use -Force to replace it."
            }
            if ($replaceVersion) {
                Assert-ExposExeIdle (Join-Path $versionPath 'expos.exe')
                Assert-ExposTreeSafe $versionPath
            }

            if (!$FromDir -or $null -eq (Get-ExposAttributes $launcher) -or
                (Get-FileHash -LiteralPath $launcherSource).Hash -ne (Get-FileHash -LiteralPath $launcher).Hash) {
                if ($null -ne (Get-ExposAttributes $launcherTemp)) { [IO.File]::Delete($launcherTemp) }
                $copiedLauncher = $true
                [IO.File]::Copy($launcherSource, $launcherTemp)
            }
            Write-ExposNewText $currentTemp "$version`n"
            if ($replaceVersion) {
                [IO.Directory]::Move($versionPath, $oldStage)
                $movedOld = $true
            }
            [IO.Directory]::Move($stage, $versionPath)
            $movedNew = $true
            if ($copiedLauncher) {
                Publish-ExposLauncher $launcherTemp $launcher $oldLauncher
                $publishedLauncher = $true
            }
            try {
                Publish-ExposFile $currentTemp $current $oldCurrent
            } catch {
                throw "Could not update the active version at $current. $($_.Exception.Message)"
            }
            $committed = $true
        } catch {
            $failure = $_
            $rollbackErrors = @(
                try {
                    if ($movedNew) { [IO.Directory]::Move($versionPath, $stage) }
                    if ($movedOld) { [IO.Directory]::Move($oldStage, $versionPath) }
                } catch {
                    $_.Exception.Message
                }
                try {
                    if ([IO.File]::Exists($oldLauncher)) {
                        Publish-ExposLauncher $oldLauncher $launcher
                    } elseif ($publishedLauncher) {
                        [IO.File]::Delete($launcher)
                    }
                } catch {
                    $_.Exception.Message
                }
                try {
                    if ([IO.File]::Exists($oldCurrent)) {
                        Publish-ExposFile $oldCurrent $current
                    }
                } catch {
                    $_.Exception.Message
                }
            )
            if ($rollbackErrors.Count) {
                throw "Install failed: $($failure.Exception.Message) Rollback also failed: $($rollbackErrors -join ' ') Backups may remain at $staging."
            }
            throw $failure
        } finally {
            if ([IO.Directory]::Exists($stage)) { [IO.Directory]::Delete($stage, $true) }
            if ($committed -and $movedOld) { [IO.Directory]::Delete($oldStage, $true) }
            if ($committed) {
                [IO.File]::Delete($oldLauncher)
                [IO.File]::Delete($oldCurrent)
            }
            if ($copiedLauncher) { [IO.File]::Delete($launcherTemp) }
            [IO.File]::Delete($currentTemp)
            if ([IO.Directory]::Exists($staging) -and ![IO.Directory]::GetFileSystemEntries($staging).Length) {
                [IO.Directory]::Delete($staging)
            }
        }

        if ($PSBoundParameters.ContainsKey('KeepDev')) {
            try {
                Remove-ExposOldDevVersions $Root $version $KeepDev
            } catch {
                Write-Warning "Could not prune developer versions in $Root. $($_.Exception.Message)"
            }
        }

        if ($FromDir) {
            Write-Host "Version directory: $versionPath"
            Write-Host "current: $([IO.File]::ReadAllText($current).TrimEnd())"
        }

        if ($shortcutPath) {
            try {
                Set-ExposShortcut $shortcutPath $launcher $Root
            } catch {
                throw "Installed expos $version in $Root, but could not save the Start Menu shortcut. $($_.Exception.Message)"
            }
        }
    } finally {
        $lock.Dispose()
    }
} finally {
    if ($zipStream) { $zipStream.Dispose() }
}

Write-Host "Installed expos $version in $Root"
if ($shortcutPath) { Write-Host "Start Menu shortcut: $shortcutPath" }
