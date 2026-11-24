& {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $repository = 'momota1029/expos'
    $api = "https://api.github.com/repos/$repository/releases"
    $apiOptions = @{
        UseBasicParsing = $true
        UserAgent = 'expos-bootstrap'
        Headers = @{ Accept = 'application/vnd.github+json' }
    }

    $channel = $env:EXPOS_CHANNEL
    if (!$channel) { $channel = 'beta' }
    if ($channel -notin @('beta', 'stable')) {
        throw 'EXPOS_CHANNEL must be beta or stable.'
    }

    $tagPattern = '\Av[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z'
    $requestedTag = $env:EXPOS_VERSION
    if ($requestedTag) {
        if (!$requestedTag.StartsWith('v', [StringComparison]::Ordinal)) { $requestedTag = "v$requestedTag" }
        if ($requestedTag -cnotmatch $tagPattern) {
            throw 'EXPOS_VERSION must be a release version such as v0.1.1-beta.5.'
        }
    }

    $oldTls = [Net.ServicePointManager]::SecurityProtocol
    $tempDirectory = $null
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

        $selected = $null
        if ($requestedTag) {
            $tagSegment = [Uri]::EscapeDataString($requestedTag)
            $selected = Invoke-RestMethod @apiOptions -Uri "$api/tags/$tagSegment"
            if ($selected.draft -or $selected.tag_name -cne $requestedTag) {
                throw "Expected a published expos release tagged $requestedTag."
            }
        } else {
            $wantPrerelease = $channel -eq 'beta'
            $newest = [DateTimeOffset]::MinValue
            $page = 1
            do {
                $batch = Invoke-RestMethod @apiOptions -Uri "${api}?per_page=100&page=$page"
                foreach ($candidate in $batch) {
                    if ($candidate.draft -or $candidate.prerelease -ne $wantPrerelease) { continue }
                    $published = [DateTimeOffset]$candidate.published_at
                    if ($published -gt $newest) {
                        $selected = $candidate
                        $newest = $published
                    }
                }
                $page++
            } while (@($batch).Count -eq 100)
            if (!$selected) { throw "No published expos release found for the $channel channel." }
        }

        $tag = $selected.tag_name
        if ($tag -cnotmatch $tagPattern) { throw "Unexpected expos release tag: $tag" }
        $tagSegment = [Uri]::EscapeDataString($tag)
        $zipName = "expos-$tag-windows-x86_64.zip"
        $assetNames = @($zipName, 'SHA256SUMS')
        $publishedAssetNames = @($selected.assets | ForEach-Object { $_.name })
        foreach ($name in $assetNames) {
            if ($publishedAssetNames -cnotcontains $name) {
                throw "Release $tag is missing the required asset $name."
            }
        }

        $tempPath = Join-Path ([IO.Path]::GetTempPath()) ("expos-install-" + [guid]::NewGuid().ToString('N'))
        $tempDirectory = (New-Item -ItemType Directory -Path $tempPath).FullName
        Write-Host "Downloading expos $tag for Windows x86_64..."
        foreach ($name in $assetNames) {
            $assetSegment = [Uri]::EscapeDataString($name)
            $assetUrl = "https://github.com/$repository/releases/download/$tagSegment/$assetSegment"
            Invoke-WebRequest -UseBasicParsing -Uri $assetUrl -OutFile (Join-Path $tempDirectory $name)
        }

        $zip = Join-Path $tempDirectory $zipName
        $sums = Join-Path $tempDirectory 'SHA256SUMS'
        $expected = @(
            foreach ($line in [IO.File]::ReadLines($sums)) {
                if ($line -match '\A([0-9A-Fa-f]{64}) [ *](.+)\z' -and $Matches[2] -ieq $zipName) {
                    $Matches[1]
                }
            }
        )
        if ($expected.Count -ne 1) { throw "Expected exactly one SHA-256 entry for $zipName in SHA256SUMS." }
        if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ine $expected[0]) {
            throw "SHA-256 mismatch for $zipName. Installation aborted."
        }

        $installerFiles = @('Install-Expos.ps1', 'Install-Common.ps1', 'AppUserModelID.txt')
        $raw = "https://raw.githubusercontent.com/$repository"
        $missing = $null
        foreach ($name in $installerFiles) {
            try {
                Invoke-WebRequest -UseBasicParsing -MaximumRedirection 0 `
                    -Uri "$raw/refs/tags/$tagSegment/release/$name" -OutFile (Join-Path $tempDirectory $name)
            } catch {
                $failure = $_.Exception
                if (!$failure.PSObject.Properties['Response'] -or !$failure.Response -or
                    [int]$failure.Response.StatusCode -ne 404) { throw }
                $missing = $name
                break
            }
        }
        if ($missing) {
            # 古い ZIP にランチャーがなければ、main のインストーラーではインストールできない。
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $archive = [IO.Compression.ZipFile]::OpenRead($zip)
            try {
                $entries = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\', '/') })
                $versionEntry = $archive.GetEntry('VERSION.txt')
                if ($entries -cnotcontains 'expos.exe' -or $entries -cnotcontains 'launcher/expos-launcher.exe' -or
                    !$versionEntry -or $versionEntry.Length -gt 1024) {
                    throw "Release tag $tag is missing $missing and its ZIP is incompatible with the main installer."
                }
                $reader = [IO.StreamReader]::new($versionEntry.Open())
                try {
                    $version = $reader.ReadToEnd() -replace '\r?\n\z', ''
                    if ("v$version" -cne $tag) { throw "VERSION.txt in $zipName does not match release tag $tag." }
                } finally {
                    $reader.Dispose()
                }
            } finally {
                $archive.Dispose()
            }

            # 3 ファイルを確認済みの main の同一コミットに固定する。
            $mainInstaller = 'c0c9508b6b40c1f10426a085e2dfe4e10be7c6d2'
            Write-Warning "Release tag $tag is missing $missing. Using all installer files from expos main at $mainInstaller."
            foreach ($name in $installerFiles) {
                Invoke-WebRequest -UseBasicParsing -MaximumRedirection 0 `
                    -Uri "$raw/$mainInstaller/release/$name" -OutFile (Join-Path $tempDirectory $name)
            }
        }

        # 通常のファイル実行でインストーラーの隣にあるファイルを参照できるようにする。
        $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $installer = Join-Path $tempDirectory 'Install-Expos.ps1'
        & $powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $installer -Zip $zip -Sha256File $sums
        if ($LASTEXITCODE -ne 0) { throw "expos installer exited with code $LASTEXITCODE for $tag." }
        Write-Host "Installed release $tag ($zipName)."
    } finally {
        [Net.ServicePointManager]::SecurityProtocol = $oldTls
        if ($tempDirectory) {
            try {
                Remove-Item -LiteralPath $tempDirectory -Recurse -Force
            } catch {
                Write-Warning "Could not remove temporary directory $tempDirectory. $($_.Exception.Message)"
            }
        }
    }
}
