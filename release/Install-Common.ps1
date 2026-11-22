# Shared filesystem rules for the Windows installer and uninstaller.
$ExposInstallMarker = "expos-install-v1`n"

function Test-ExposVersion([string]$Version) {
    if ($Version.Length -ge 255) { return $false }
    # Match Rust semver, including UInt64 core components and numeric prerelease rules.
    $number = '(0|[1-9][0-9]*)'
    $pre = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
    $build = '[0-9A-Za-z-]+'
    $pattern = "\A$number\.$number\.$number(?:-$pre(?:\.$pre)*)?(?:\+$build(?:\.$build)*)?\z"
    $match = [regex]::Match($Version, $pattern)
    if (!$match.Success) { return $false }
    foreach ($index in 1..3) {
        $value = [uint64]0
        if (![uint64]::TryParse($match.Groups[$index].Value, [ref]$value)) { return $false }
    }
    return $true
}

function Get-ExposErrorCode($Exception) {
    $cause = $Exception.GetBaseException()
    if ($cause -is [ComponentModel.Win32Exception]) { return $cause.NativeErrorCode }
    return ($cause.HResult -band 0xffff)
}

function Get-ExposAttributes([string]$Path, [switch]$Directory) {
    try {
        $attributes = [IO.File]::GetAttributes($Path)
    } catch {
        if ((Get-ExposErrorCode $_.Exception) -in @(2, 3)) { return $null }
        throw
    }
    if ($attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Reparse point is not allowed: $Path"
    }
    if ([bool]($attributes -band [IO.FileAttributes]::Directory) -ne [bool]$Directory) {
        throw "Unexpected file type: $Path"
    }
    return $attributes
}

function Initialize-ExposNativePaths {
    if ('ExposInstall.NativePaths' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace ExposInstall {
    public static class NativePaths {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(string path, uint access, uint share,
            IntPtr security, uint disposition, uint flags, IntPtr template);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        private static extern uint GetFinalPathNameByHandleW(SafeFileHandle handle,
            StringBuilder path, uint length, uint flags);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetFileInformationByHandle(SafeFileHandle handle,
            int infoClass, ref byte deleteFile, uint bufferSize);

        public static string DirectoryIdentity(string path) {
            const uint backupSemantics = 0x02000000;
            const uint openReparsePoint = 0x00200000;
            const uint volumeNameGuid = 1;
            using (var handle = CreateFileW(path, 0, 7, IntPtr.Zero, 3,
                       backupSemantics | openReparsePoint, IntPtr.Zero)) {
                if (handle.IsInvalid) throw new Win32Exception();
                var name = new StringBuilder(32768);
                uint size = GetFinalPathNameByHandleW(handle, name, (uint)name.Capacity, volumeNameGuid);
                if (size == 0) throw new Win32Exception();
                if (size >= name.Capacity) throw new InvalidOperationException("Path is too long.");
                return name.ToString();
            }
        }

        // Take exclusive ownership only after the read/write-shared byte lock is closed.
        // This lets uninstall retire the lock file without racing a newly acquired lock.
        public static SafeFileHandle OpenLockForRemoval(string path) {
            const uint deleteAccess = 0x00010000;
            const uint openReparsePoint = 0x00200000;
            var handle = CreateFileW(path, deleteAccess, 0, IntPtr.Zero, 3,
                openReparsePoint, IntPtr.Zero);
            if (!handle.IsInvalid) return handle;
            int error = Marshal.GetLastWin32Error();
            handle.Dispose();
            throw new Win32Exception(error);
        }

        public static void DeleteLockOnClose(SafeFileHandle handle) {
            const int fileDispositionInfo = 4;
            byte deleteFile = 1;
            if (!SetFileInformationByHandle(handle, fileDispositionInfo, ref deleteFile, 1))
                throw new Win32Exception();
        }
    }
}
'@
}

function Resolve-ExposFileSystemPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'A filesystem path is empty.' }
    return [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path))
}

function Get-ExposDirectoryIdentity([string]$Path) {
    # Check every existing ancestor before asking Windows to resolve drive and 8.3 aliases.
    $missing = [Collections.Generic.Stack[string]]::new()
    $existing = $null
    for ($part = $Path; $part; $part = [IO.Path]::GetDirectoryName($part)) {
        if ($null -eq (Get-ExposAttributes $part -Directory)) {
            $missing.Push([IO.Path]::GetFileName($part))
        } elseif ($null -eq $existing) {
            $existing = $part
        }
    }
    if ($null -eq $existing) { throw "No existing parent for $Path" }
    try {
        $identity = [ExposInstall.NativePaths]::DirectoryIdentity($existing).TrimEnd('\')
    } catch {
        if ($_.Exception.GetBaseException() -isnot [ComponentModel.Win32Exception]) { throw }
        # Windows may not expose a volume GUID for mapped shares or virtual drives.
        return $null
    }
    foreach ($segment in $missing) {
        $identity += "\$segment"
    }
    return $identity
}

function Test-ExposPathWithin([string]$Path, [string]$Parent) {
    return ($Path.Equals($Parent, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith("$Parent\", [StringComparison]::OrdinalIgnoreCase))
}

function Resolve-ExposManagedDirectory([string]$Path, [switch]$InstallRoot) {
    $full = (Resolve-ExposFileSystemPath $Path).TrimEnd('\', '/')
    if ($full -notmatch '\A[A-Za-z]:\\[^:]+\z' -or $full -match '[. ](?:\\|$)') {
        throw "Use an ordinary local Windows directory: $Path"
    }
    if ($InstallRoot -and $full.Substring(3).Length -lt 5) {
        throw "The install root is too short: $full"
    }
    Initialize-ExposNativePaths
    $identity = Get-ExposDirectoryIdentity $full
    foreach ($appData in @($env:LOCALAPPDATA, [Environment]::GetFolderPath('LocalApplicationData'))) {
        if ([string]::IsNullOrWhiteSpace($appData)) { continue }
        $dataPath = (Resolve-ExposFileSystemPath (Join-Path $appData 'gpui-filer')).TrimEnd('\', '/')
        $dataIdentity = Get-ExposDirectoryIdentity $dataPath
        $candidate = $identity
        $data = $dataIdentity
        if (!$identity -or !$dataIdentity) {
            # Compare paths in the same form when either native lookup fails.
            $candidate = $full
            $data = $dataPath
        }
        if ((Test-ExposPathWithin $candidate $data) -or
            ($InstallRoot -and (Test-ExposPathWithin $data $candidate))) {
            throw "Directory overlaps the expos data directory: $full"
        }
    }
    return $full
}

function Get-ExposDefaultRoot {
    if ($env:EXPOS_INSTALL_ROOT) { return $env:EXPOS_INSTALL_ROOT }
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set. Specify -Root.' }
    return (Join-Path $env:LOCALAPPDATA 'expos')
}

function Get-ExposShortcutPath([string]$StartMenuDir) {
    if (-not $StartMenuDir) {
        if (-not $env:APPDATA) { throw 'APPDATA is not set. Specify -StartMenuDir or -NoShortcut.' }
        $StartMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
    }
    return (Join-Path (Resolve-ExposManagedDirectory $StartMenuDir) 'expos.lnk')
}

function Test-ExposMarker([string]$Path) {
    if ($null -eq (Get-ExposAttributes $Path)) { return $false }
    if ([IO.FileInfo]::new($Path).Length -ne $ExposInstallMarker.Length -or
        [IO.File]::ReadAllText($Path) -cne $ExposInstallMarker) {
        throw "Invalid expos install marker: $Path"
    }
    return $true
}

function Enter-ExposInstallLock([string]$Root) {
    $path = Join-Path $Root '.lock'
    $null = Get-ExposAttributes $path
    $stream = [IO.File]::Open($path, 'OpenOrCreate', 'ReadWrite', 'ReadWrite')
    try {
        $timer = [Diagnostics.Stopwatch]::StartNew()
        while ($true) {
            try {
                $stream.Lock(0, 1)
                return $stream
            } catch {
                if ((Get-ExposErrorCode $_.Exception) -notin @(32, 33)) { throw }
                if ($timer.Elapsed.TotalSeconds -ge 10) {
                    throw "Timed out waiting for the expos install lock: $path"
                }
                Start-Sleep -Milliseconds 100
            }
        }
    } catch {
        $stream.Dispose()
        throw
    }
}

function Test-ExposExeInUse([string]$Path) {
    if ($null -eq (Get-ExposAttributes $Path)) { return $false }
    try {
        $stream = [IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')
        $stream.Dispose()
        return $false
    } catch {
        if ((Get-ExposErrorCode $_.Exception) -in @(32, 33, 1224)) { return $true }
        throw
    }
}

function Assert-ExposExeIdle([string]$Path) {
    if (Test-ExposExeInUse $Path) {
        throw "expos executable is in use: $Path. Close it and retry."
    }
}

function Assert-ExposTreeSafe([string]$Directory) {
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Directory)) {
        $attributes = [IO.File]::GetAttributes($path)
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Reparse point is not allowed: $path"
        }
        if ($attributes -band [IO.FileAttributes]::Directory) {
            Assert-ExposTreeSafe $path
        }
    }
}

function Write-ExposNewText([string]$Path, [string]$Value) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
    $stream = [IO.File]::Open($Path, 'CreateNew', 'Write', 'None')
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
}

function Publish-ExposFile([string]$Source, [string]$Destination, [string]$Backup) {
    if ($null -eq (Get-ExposAttributes $Destination)) {
        [IO.File]::Move($Source, $Destination)
    } elseif ($Backup) {
        [IO.File]::Replace($Source, $Destination, $Backup)
    } else {
        [IO.File]::Replace($Source, $Destination, [NullString]::Value)
    }
}
