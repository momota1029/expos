param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('\Av(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z')]
    [string] $Tag,

    [switch] $AutoBeta,

    [string] $RepositoryRoot = (Join-Path $PSScriptRoot '..')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$releaseRoot = Join-Path $repoRoot 'release'
$version = $Tag.Substring(1)
$channel = 'beta'
if ($AutoBeta) {
    if ($version -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+-beta\.[1-9][0-9]*\z') {
        throw 'An automated beta tag must have the form v<base>-beta.<run number>.'
    }
    $commit = & git -C $repoRoot rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Could not resolve the checked-out commit.' }
    $commit = ($commit -join '').Trim()
}
else {
    & git -C $repoRoot diff --quiet HEAD -- Cargo.toml Cargo.lock
    if ($LASTEXITCODE -ne 0) { throw 'Cargo.toml or Cargo.lock differs from HEAD; package the committed tag source.' }
    $resolved = & "$PSScriptRoot/Resolve-Version.ps1" -Tag $Tag -RepositoryRoot $repoRoot
    $commit = $resolved.Commit
    if (-not $resolved.IsPrerelease) { $channel = 'stable' }
}

$target = 'x86_64-pc-windows-msvc'
$staging = Join-Path $releaseRoot 'staging'
$sourceRoot = Join-Path $staging 'source'
$binaryBundle = Join-Path $staging 'windows'
$dist = Join-Path $releaseRoot 'dist'
$gitArchive = Join-Path $staging 'committed-source.zip'
$sourceArchive = Join-Path $dist "expos-$Tag-source-vendored.zip"
$windowsArchive = Join-Path $dist "expos-$Tag-windows-x86_64.zip"

foreach ($path in @($staging, $sourceRoot, $binaryBundle, $dist)) {
    if (Test-Path -LiteralPath $path) {
        throw "Refusing to reuse existing release staging path: $path"
    }
}

New-Item -ItemType Directory -Path $staging -Force | Out-Null
New-Item -ItemType Directory -Path $sourceRoot, $binaryBundle, $dist | Out-Null

& git -C $repoRoot archive --format=zip "--output=$gitArchive" $commit
if ($LASTEXITCODE -ne 0) { throw 'Could not archive the committed source.' }
Expand-Archive -LiteralPath $gitArchive -DestinationPath $sourceRoot
Remove-Item -LiteralPath $gitArchive

# Keep the source ZIP and the executable on the same Cargo version as the beta checkout.
if ($AutoBeta) {
    foreach ($file in @('Cargo.toml', 'Cargo.lock')) {
        Copy-Item -LiteralPath (Join-Path $repoRoot $file) -Destination $sourceRoot -Force
    }
}

$hasLauncher = Test-Path -LiteralPath (Join-Path $sourceRoot 'crates/launcher/Cargo.toml') -PathType Leaf
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$cargoConfigDirectory = Join-Path $sourceRoot '.cargo'
New-Item -ItemType Directory -Path $cargoConfigDirectory | Out-Null
Push-Location $sourceRoot
$previousCargoTargetDir = $env:CARGO_TARGET_DIR
try {
    $metadataText = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
    $metadata = ($metadataText -join "`n") | ConvertFrom-Json
    $packages = @($metadata.packages | Where-Object { $_.name -ceq 'expos' })
    if ($packages.Count -ne 1 -or $packages[0].version -cne $version) {
        throw "Expected one expos package at version $version in the archived source."
    }

    $vendorConfig = & cargo vendor --locked vendor
    if ($LASTEXITCODE -ne 0) { throw 'Could not vendor every locked Cargo dependency.' }
    $configPath = Join-Path $cargoConfigDirectory 'config.toml'
    [System.IO.File]::WriteAllText($configPath, (($vendorConfig -join "`n") + "`n`n[net]`noffline = true`n"), $utf8NoBom)

    $env:CARGO_TARGET_DIR = Join-Path $staging 'target'
    & cargo build --locked --offline --release --target $target --package expos
    if ($LASTEXITCODE -ne 0) { throw 'Offline expos build from vendored source failed.' }
    if ($hasLauncher) {
        & cargo build --locked --offline --release --target $target --package expos-launcher
        if ($LASTEXITCODE -ne 0) { throw 'Offline launcher build from vendored source failed.' }
    }
}
finally {
    if ($null -eq $previousCargoTargetDir) {
        Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_TARGET_DIR = $previousCargoTargetDir
    }
    Pop-Location
}

$buildOutput = Join-Path $staging "target/$target/release"
$builtExecutable = Join-Path $buildOutput 'expos.exe'
if (-not (Test-Path -LiteralPath $builtExecutable -PathType Leaf)) {
    throw 'The Windows release build did not produce expos.exe.'
}

Copy-Item -LiteralPath $builtExecutable -Destination (Join-Path $binaryBundle 'expos.exe')
if ($hasLauncher) {
    $launcherDirectory = Join-Path $binaryBundle 'launcher'
    New-Item -ItemType Directory -Path $launcherDirectory | Out-Null
    Copy-Item -LiteralPath (Join-Path $buildOutput 'expos-launcher.exe') -Destination $launcherDirectory
}
[System.IO.File]::WriteAllText((Join-Path $binaryBundle 'VERSION.txt'), "$version`n", $utf8NoBom)
foreach ($file in @('README.md', 'RELEASE_NOTES.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt')) {
    Copy-Item -LiteralPath (Join-Path $sourceRoot $file) -Destination $binaryBundle
}
$artworkLicenseDirectory = Join-Path $binaryBundle 'LICENSES/Fluent11'
New-Item -ItemType Directory -Path $artworkLicenseDirectory | Out-Null
foreach ($file in @('LICENSE', 'AUTHORS.txt')) {
    Copy-Item -LiteralPath (Join-Path $sourceRoot "crates/resources/assets/licenses/fluent11/$file") -Destination $artworkLicenseDirectory
}

[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $binaryBundle,
    $windowsArchive,
    [System.IO.Compression.CompressionLevel]::Optimal,
    $false
)
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $sourceRoot,
    $sourceArchive,
    [System.IO.Compression.CompressionLevel]::Optimal,
    $false
)

$sourceHash = (Get-FileHash -LiteralPath $sourceArchive -Algorithm SHA256).Hash.ToLowerInvariant()
$windowsHash = (Get-FileHash -LiteralPath $windowsArchive -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceName = Split-Path -Leaf $sourceArchive
$windowsName = Split-Path -Leaf $windowsArchive
$checksums = "$sourceHash  $sourceName`n$windowsHash  $windowsName`n"
[System.IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS'), $checksums, $utf8NoBom)

$manifest = [ordered] @{
    channel = $channel
    version = $version
    commit = $commit
    zip = $windowsName
    sha256 = $windowsHash
    built_at = [DateTime]::UtcNow.ToString('o')
} | ConvertTo-Json -Compress
[System.IO.File]::WriteAllText((Join-Path $dist 'channel.json'), "$manifest`n", $utf8NoBom)
