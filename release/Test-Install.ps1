[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression

function Assert-That([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
}

function Assert-Refused([scriptblock]$Action, [string]$ExpectedMessage) {
    try {
        & $Action
    } catch {
        if ($_.Exception.Message -like "*$ExpectedMessage*") { return }
        throw "Expected '$ExpectedMessage', got '$($_.Exception.Message)'"
    }
    throw "Expected failure containing '$ExpectedMessage'."
}

function Run-Check([string]$Name, [scriptblock]$Check) {
    try {
        & $Check
        Write-Host "PASS $Name"
    } catch {
        $script:failures++
        Write-Host "FAIL ${Name}: $($_.Exception.Message)"
    }
}

function Add-StubExe($Archive, [string]$Name, [string]$Source) {
    $outputStream = $Archive.CreateEntry($Name).Open()
    try {
        $sourceStream = [IO.File]::OpenRead($Source)
        try {
            $sourceStream.CopyTo($outputStream)
        } finally {
            $sourceStream.Dispose()
        }
    } finally {
        $outputStream.Dispose()
    }
}

function New-StubZip([string]$Path, [string]$Version, [string]$ExtraName, [int]$ExtraAttributes = 0) {
    $file = [IO.File]::Open($Path, 'CreateNew', 'ReadWrite', 'None')
    try {
        $zipArchive = [IO.Compression.ZipArchive]::new($file, 'Create', $true)
        try {
            Add-StubExe $zipArchive 'expos.exe' $stubApp
            Add-StubExe $zipArchive 'launcher/expos-launcher.exe' $stubLauncher
            if ($Version) {
                $entryStream = $zipArchive.CreateEntry('VERSION.txt').Open()
                try {
                    $bytes = [Text.Encoding]::ASCII.GetBytes("$Version`n")
                    $entryStream.Write($bytes, 0, $bytes.Length)
                } finally {
                    $entryStream.Dispose()
                }
            }
            if ($ExtraName) {
                $entry = $zipArchive.CreateEntry($ExtraName)
                $entry.ExternalAttributes = $ExtraAttributes
                $entryStream = $entry.Open()
                try {
                    $entryStream.WriteByte(65)
                } finally {
                    $entryStream.Dispose()
                }
            }
        } finally {
            $zipArchive.Dispose()
        }
    } finally {
        $file.Dispose()
    }
}

function Assert-Current([string]$Version) {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $testRoot 'current'))
    $actual = [Text.Encoding]::UTF8.GetString($bytes)
    Assert-That ($actual -ceq "v$Version`n") "Unexpected current pointer bytes: $actual"
    Assert-That ([IO.File]::Exists((Join-Path $testRoot "v$Version\expos.exe"))) 'Missing payload'
    Assert-That (![IO.Directory]::Exists((Join-Path $testRoot '.staging'))) 'Staging directory was left behind'
}

$failures = 0
$fixtureParent = $env:TEMP
if ($env:LOCALAPPDATA) {
    $fixtureParent = Join-Path $env:LOCALAPPDATA 'Temp'
}
$fixture = Join-Path $fixtureParent ("expos-install-test-{0}" -f [guid]::NewGuid().ToString('N'))
$testRoot = Join-Path $fixture 'install'
$menu = Join-Path $fixture 'menu'
$shortcutPath = Join-Path $menu 'expos.lnk'
$outside = Join-Path $fixture 'keep.txt'
$unrelatedMenu = Join-Path $fixture 'other-menu'
$unrelatedLink = Join-Path $unrelatedMenu 'expos.lnk'
$localData = Join-Path $fixture 'appdata'
$protected = Join-Path $localData 'gpui-filer'
$dataFile = Join-Path $protected 'v6\keep.txt'
$stableData = Join-Path $testRoot 'data\stable\keep.txt'
$betaData = Join-Path $testRoot 'data\beta\keep.txt'
$launcherPath = Join-Path $testRoot 'expos.exe'
$install = Join-Path $PSScriptRoot 'Install-Expos.ps1'
$uninstall = Join-Path $PSScriptRoot 'Uninstall-Expos.ps1'
$stubApp = Join-Path $env:SystemRoot 'System32\cmd.exe'
$stubLauncher = Join-Path $env:SystemRoot 'System32\whoami.exe'
$one = Join-Path $fixture 'first.zip'
$twoVersion = '1.2.0-rc.1+test.01'
$two = Join-Path $fixture "expos-v${twoVersion}-windows-x86_64.zip"
$shaFile = Join-Path $fixture 'SHA256SUMS'
$oldLocalAppData = $env:LOCALAPPDATA
$oldRoot = $env:EXPOS_INSTALL_ROOT
try {
    $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($dataFile))
    $null = [IO.Directory]::CreateDirectory($unrelatedMenu)
    [IO.File]::WriteAllText($outside, 'outside install root')
    [IO.File]::WriteAllText($dataFile, 'application data')
    [IO.File]::WriteAllText((Join-Path $protected '.expos-install'), "expos-install-v1`n")
    $env:LOCALAPPDATA = $localData

    $shell = New-Object -ComObject WScript.Shell
    $unrelatedShortcut = $shell.CreateShortcut($unrelatedLink)
    $unrelatedShortcut.TargetPath = $stubApp
    $unrelatedShortcut.Save()
    $unrelatedHash = (Get-FileHash -LiteralPath $unrelatedLink).Hash

    New-StubZip $one '1.1.0'
    New-StubZip $two
    $oneHash = (Get-FileHash -LiteralPath $one).Hash
    $twoHash = (Get-FileHash -LiteralPath $two).Hash
    $oneName = [IO.Path]::GetFileName($one)
    [IO.File]::WriteAllText($shaFile, "$twoHash  $([IO.Path]::GetFileName($two))`n$oneHash  $oneName`n")
    $sumHash = (Get-FileHash -LiteralPath $shaFile).Hash
    $launcherHash = (Get-FileHash -LiteralPath $stubLauncher).Hash
    $appHash = (Get-FileHash -LiteralPath $stubApp).Hash

    Run-Check 'first install verifies SHA256SUMS and skips the shortcut' {
        & $install -Zip $one -Sha256File $shaFile -Root $testRoot -NoShortcut -StartMenuDir $menu
        Assert-Current '1.1.0'
        Assert-That ([IO.File]::Exists((Join-Path $testRoot '.expos-install'))) 'Marker missing'
        Assert-That ([IO.File]::Exists((Join-Path $testRoot '.lock'))) 'Persistent lock missing'
        Assert-That (![IO.Directory]::Exists($menu)) 'Start Menu directory was created with -NoShortcut'
        Assert-That ((Get-FileHash -LiteralPath $launcherPath).Hash -eq $launcherHash) 'Wrong launcher'
    }

    Run-Check 'second install uses filename fallback, keeps both versions, and writes a shortcut' {
        $env:EXPOS_INSTALL_ROOT = $testRoot
        & $install -Zip $two -Sha256 $twoHash -StartMenuDir $menu
        Assert-Current $twoVersion
        Assert-That ([IO.File]::Exists((Join-Path $testRoot 'v1.1.0\expos.exe'))) 'Previous version removed'
        Assert-That ([IO.File]::Exists($shortcutPath)) 'Start Menu shortcut missing'
        $shortcut = $shell.CreateShortcut($shortcutPath)
        Assert-That ($shortcut.TargetPath -eq $launcherPath) 'Shortcut target is incorrect'
        Assert-That ($shortcut.WorkingDirectory -eq $testRoot) 'Shortcut working directory is incorrect'
    }

    Run-Check 'a bad SHA-256 leaves the active version and shortcut intact' {
        $shortcutHash = (Get-FileHash -LiteralPath $shortcutPath).Hash
        Assert-Refused {
            & $install -Zip $one -Sha256 ('0' * 64) -Root $testRoot -Force -StartMenuDir $menu
        } 'SHA-256 mismatch'
        Assert-Current $twoVersion
        Assert-That ((Get-FileHash -LiteralPath $shortcutPath).Hash -eq $shortcutHash) 'Shortcut changed'
    }

    Run-Check 'ZIP traversal, absolute names, and link entries are rejected' {
        $traversal = Join-Path $fixture 'traversal.zip'
        $absolute = Join-Path $fixture 'absolute.zip'
        $link = Join-Path $fixture 'link.zip'
        $reparse = Join-Path $fixture 'reparse.zip'
        New-StubZip $traversal '1.3.0' '../../../keep.txt'
        New-StubZip $absolute '1.3.0' $outside
        New-StubZip $link '1.3.0' 'link' (0xa000 -shl 16)
        New-StubZip $reparse '1.3.0' 'reparse' 0x400
        foreach ($zip in @($traversal, $absolute)) {
            Assert-Refused { & $install -Zip $zip -Root $testRoot -NoShortcut } 'Unsafe ZIP entry path'
        }
        foreach ($zip in @($link, $reparse)) {
            Assert-Refused { & $install -Zip $zip -Root $testRoot -NoShortcut } 'reparse point'
        }
        Assert-Current $twoVersion
        Assert-That (![IO.Directory]::Exists((Join-Path $testRoot 'v1.3.0'))) 'Unsafe payload was published'
    }

    Run-Check 'versions with leading zeroes, overflow, or path separators are rejected' {
        $index = 0
        foreach ($invalid in @('1.01.0', '1.2.0-rc.01', '18446744073709551616.0.0', '1.2.0/other')) {
            $badVersion = Join-Path $fixture "bad-version-$index.zip"
            New-StubZip $badVersion $invalid
            Assert-Refused { & $install -Zip $badVersion -Root $testRoot -NoShortcut } 'Invalid expos version'
            $index++
        }
        Assert-Current $twoVersion
    }

    Run-Check 'duplicate install requires -Force and accepts a launcher override' {
        $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($stableData))
        $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($betaData))
        [IO.File]::WriteAllText($stableData, 'stable settings')
        [IO.File]::WriteAllText($betaData, 'beta settings')
        Assert-Refused { & $install -Zip $two -Root $testRoot -NoShortcut } 'already installed'
        & $install -Zip $two -Root $testRoot -NoShortcut -Force -LauncherExe $stubApp
        Assert-Current $twoVersion
        Assert-That ((Get-FileHash -LiteralPath $launcherPath).Hash -eq $appHash) 'Launcher override unused'
        Assert-That ([IO.File]::ReadAllText($stableData) -ceq 'stable settings') 'Stable data changed on reinstall'
        Assert-That ([IO.File]::ReadAllText($betaData) -ceq 'beta settings') 'Beta data changed on reinstall'
    }

    Run-Check 'failed launcher replacement restores the old payload and pointer' {
        $oldPayload = Join-Path $testRoot "v$twoVersion\original.txt"
        [IO.File]::WriteAllText($oldPayload, 'old payload')
        $busyLauncher = [IO.File]::Open($launcherPath, 'Open', 'Read', 'Read')
        try {
            Assert-Refused { & $install -Zip $two -Root $testRoot -NoShortcut -Force } 'Could not replace the launcher'
        } finally {
            $busyLauncher.Dispose()
        }
        Assert-Current $twoVersion
        Assert-That ((Get-FileHash -LiteralPath $launcherPath).Hash -eq $appHash) 'Old launcher changed'
        Assert-That ([IO.File]::ReadAllText($oldPayload) -ceq 'old payload') 'Original payload was not restored'
    }

    Run-Check 'failed current replacement rolls back the new version and launcher' {
        $three = Join-Path $fixture 'third.zip'
        New-StubZip $three '1.3.0'
        $busyPointer = [IO.File]::Open((Join-Path $testRoot 'current'), 'Open', 'Read', 'Read')
        try {
            Assert-Refused { & $install -Zip $three -Root $testRoot -NoShortcut } 'Could not update the active version'
        } finally {
            $busyPointer.Dispose()
        }
        Assert-Current $twoVersion
        Assert-That (![IO.Directory]::Exists((Join-Path $testRoot 'v1.3.0'))) 'Uncommitted version was left installed'
        Assert-That ((Get-FileHash -LiteralPath $launcherPath).Hash -eq $appHash) 'Old launcher was not restored'
    }

    Run-Check 'install and uninstall both refuse an in-use payload, even with -Force' {
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = Join-Path $testRoot 'v1.1.0\expos.exe'
        $info.Arguments = '/d /q'
        $info.UseShellExecute = $false
        $info.RedirectStandardInput = $true
        $info.RedirectStandardOutput = $true
        $info.RedirectStandardError = $true
        $info.CreateNoWindow = $true
        $process = [Diagnostics.Process]::Start($info)
        try {
            Assert-That (!$process.HasExited) 'The payload did not stay running'
            Assert-Refused { & $install -Zip $one -Root $testRoot -NoShortcut -Force } 'in use'
            Assert-Refused { & $uninstall -Root $testRoot -Force -RemoveData -StartMenuDir $menu } 'in use'
            Assert-That ([IO.File]::Exists($shortcutPath)) 'Shortcut removed before uninstall refused'
            Assert-That ([IO.File]::Exists($info.FileName)) 'Running payload was removed'
            Assert-That ([IO.File]::Exists($stableData)) 'Channel data removed before uninstall refused'
        } finally {
            if (!$process.HasExited) {
                $process.Kill()
                $process.WaitForExit()
            }
            $process.Dispose()
        }
        Assert-Current $twoVersion
    }

    Run-Check 'directory install switches current and prunes only idle developer versions' {
        $devRoot = Join-Path $fixture 'dev-install'
        $payload = Join-Path $fixture 'dev-payload'
        $null = [IO.Directory]::CreateDirectory((Join-Path $payload 'launcher'))
        [IO.File]::Copy($stubApp, (Join-Path $payload 'expos.exe'))
        [IO.File]::Copy($stubLauncher, (Join-Path $payload 'launcher\expos-launcher.exe'))
        & $install -Zip $one -Root $devRoot -NoShortcut
        $oldest = '1.2.0-alpha.dev.20261123T093000000000000'
        $idle = '1.2.0-alpha.dev.20261123T093001000000000'
        $active = '1.2.0-alpha.dev.20261123T093002000000000.aa51d95d-8349-49d2-b0dd-b157f36a2c64'
        $versionFile = Join-Path $payload 'VERSION.txt'
        foreach ($version in @($oldest, $idle)) {
            [IO.File]::WriteAllText($versionFile, "$version`n")
            & $install -FromDir $payload -Root $devRoot -NoShortcut
        }
        $oldExe = Join-Path $devRoot "v$oldest\expos.exe"
        $currentPath = Join-Path $devRoot 'current'
        $pin = [IO.File]::Open($oldExe, 'Open', 'Read', 'Read')
        try {
            [IO.File]::WriteAllText($versionFile, "$active`n")
            & $install -FromDir $payload -KeepDev 0 -Root $devRoot -NoShortcut
            Assert-That ([IO.File]::ReadAllText($currentPath) -ceq "v$active`n") 'Current did not switch'
            Assert-That ([IO.File]::Exists((Join-Path $devRoot "v$active\expos.exe"))) 'Current payload was removed'
            Assert-That ([IO.File]::Exists($oldExe)) 'In-use payload was removed'
            Assert-That (![IO.Directory]::Exists((Join-Path $devRoot "v$idle"))) 'Idle payload was kept'
            Assert-That ([IO.File]::Exists((Join-Path $devRoot 'v1.1.0\expos.exe'))) 'Release payload was removed'
            Assert-That (![IO.Directory]::Exists((Join-Path $devRoot '.staging'))) 'Staging directory was left behind'
            Assert-Refused { & $install -FromDir $payload -Root $devRoot -NoShortcut } 'already installed'
            Assert-That ([IO.File]::ReadAllText($currentPath) -ceq "v$active`n") 'Duplicate changed current'
        } finally {
            $pin.Dispose()
        }
    }

    Run-Check 'uninstall rejects drive roots, unmarked roots, and protected data' {
        Assert-Refused { & $uninstall -Root ([IO.Path]::GetPathRoot($testRoot)) -NoShortcut } 'ordinary local Windows directory'
        Assert-Refused { & $uninstall -Root $unrelatedMenu -NoShortcut } 'without an expos install marker'
        Assert-Refused { & $uninstall -Root $protected -NoShortcut -RemoveData -Force } 'overlaps the expos data directory'
        Assert-Refused { & $uninstall -Root $localData -NoShortcut -RemoveData -Force } 'overlaps the expos data directory'
        Assert-Refused { & $install -Zip $one -Root $protected -NoShortcut -Force } 'overlaps the expos data directory'
    }

    Run-Check 'default uninstall removes managed files and keeps both data channels' {
        $unmanaged = Join-Path $testRoot 'videos'
        $null = [IO.Directory]::CreateDirectory($unmanaged)
        $unmanagedFile = Join-Path $unmanaged 'keep.txt'
        [IO.File]::WriteAllText($unmanagedFile, 'unmanaged')
        & $uninstall -Root $testRoot -StartMenuDir $menu
        foreach ($name in @('expos.exe', 'current', '.expos-install', '.lock', '.staging', 'v1.1.0', "v$twoVersion")) {
            Assert-That (!(Test-Path -LiteralPath (Join-Path $testRoot $name))) "Uninstall left $name behind"
        }
        Assert-That (![IO.File]::Exists($shortcutPath)) 'Shortcut still exists'
        Assert-That ([IO.File]::ReadAllText($stableData) -ceq 'stable settings') 'Stable data was lost'
        Assert-That ([IO.File]::ReadAllText($betaData) -ceq 'beta settings') 'Beta data was lost'
        Assert-That ([IO.File]::ReadAllText($unmanagedFile) -ceq 'unmanaged') 'Unmanaged directory changed'
        [IO.File]::Delete($unmanagedFile)
        [IO.Directory]::Delete($unmanaged)
    }

    Run-Check 'reinstall preserves data and -RemoveData removes both channels and the empty root' {
        & $install -Zip $one -Root $testRoot -NoShortcut
        Assert-Current '1.1.0'
        Assert-That ([IO.File]::ReadAllText($stableData) -ceq 'stable settings') 'Stable data changed on reinstall'
        Assert-That ([IO.File]::ReadAllText($betaData) -ceq 'beta settings') 'Beta data changed on reinstall'
        & $uninstall -Root $testRoot -NoShortcut -RemoveData -Force
        Assert-That (![IO.File]::Exists($stableData)) 'Stable data still exists'
        Assert-That (![IO.File]::Exists($betaData)) 'Beta data still exists'
        Assert-That (![IO.Directory]::Exists($testRoot)) 'Empty install root still exists'
    }

    Run-Check 'uninstall and rejected installs leave files outside the root intact' {
        Assert-That ([IO.File]::ReadAllText($outside) -ceq 'outside install root') 'Sibling file changed'
        Assert-That ([IO.File]::ReadAllText($dataFile) -ceq 'application data') 'Protected app data changed'
        Assert-That ((Get-FileHash -LiteralPath $unrelatedLink).Hash -eq $unrelatedHash) 'Unrelated shortcut changed'
        Assert-That ((Get-FileHash -LiteralPath $one).Hash -eq $oneHash) 'First ZIP changed'
        Assert-That ((Get-FileHash -LiteralPath $two).Hash -eq $twoHash) 'Second ZIP changed'
        Assert-That ((Get-FileHash -LiteralPath $shaFile).Hash -eq $sumHash) 'SHA256SUMS changed'
    }
} catch {
    $failures++
    Write-Host "FAIL test setup or execution: $($_.Exception.Message)"
} finally {
    $env:LOCALAPPDATA = $oldLocalAppData
    $env:EXPOS_INSTALL_ROOT = $oldRoot
    if ([IO.Directory]::Exists($fixture)) {
        try {
            [IO.Directory]::Delete($fixture, $true)
        } catch {
            $failures++
            Write-Host "FAIL test cleanup at ${fixture}: $($_.Exception.Message)"
        }
    }
}

if ($failures) {
    Write-Host "$failures check(s) failed"
    exit 1
}
Write-Host 'All installer checks passed'
exit 0
