param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$')]
    [string] $Tag
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$releaseRoot = (Resolve-Path $PSScriptRoot).Path
if (-not $releaseRoot.StartsWith($repoRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Release directory must remain inside the repository.'
}

$head = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not resolve checked-out commit.' }
$tagCommit = (& git rev-parse "refs/tags/$Tag^{commit}").Trim()
if ($LASTEXITCODE -ne 0) { throw 'The requested tag does not resolve to a commit.' }
if ($head -ne $tagCommit) { throw 'The requested tag does not point to the checked-out commit.' }

# Cargo metadata reads the working tree, so reject manifest edits that differ from the committed tag.
& git diff --quiet HEAD -- Cargo.toml
if ($LASTEXITCODE -ne 0) { throw 'Cargo.toml has local changes; package the committed tag source only.' }
$metadataText = & cargo metadata --locked --no-deps --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$metadata = ($metadataText -join "`n") | ConvertFrom-Json
$packages = @($metadata.packages | Where-Object { $_.name -ceq 'expos' })
if ($packages.Count -ne 1) { throw 'Expected exactly one expos package in the workspace.' }
if ($packages[0].version -cne $Tag.Substring(1)) {
    throw "Tag version $($Tag.Substring(1)) does not match Cargo package version $($packages[0].version)."
}

$version = $Tag.Substring(1)
$target = 'x86_64-pc-windows-msvc'
$staging = Join-Path $releaseRoot 'staging'
$sourceRoot = Join-Path $staging 'source'
$binaryBundle = Join-Path $staging 'windows'
$dist = Join-Path $releaseRoot 'dist'
$gitArchive = Join-Path $staging 'tag-source.zip'
$sourceArchive = Join-Path $dist "expos-$Tag-source-vendored.zip"
$windowsArchive = Join-Path $dist "expos-$Tag-windows-x86_64.zip"

foreach ($path in @($staging, $sourceRoot, $binaryBundle, $dist)) {
    if (Test-Path -LiteralPath $path) {
        throw "Refusing to reuse existing release staging path: $path"
    }
}

New-Item -ItemType Directory -Path $staging | Out-Null
New-Item -ItemType Directory -Path $sourceRoot, $binaryBundle, $dist | Out-Null

# archive HEAD only after the workflow verifies that HEAD is exactly the requested tag.
& git archive --format=zip "--output=$gitArchive" HEAD
if ($LASTEXITCODE -ne 0) { throw 'Could not archive the committed tag source.' }
Expand-Archive -LiteralPath $gitArchive -DestinationPath $sourceRoot
Remove-Item -LiteralPath $gitArchive

$cargoConfigDirectory = Join-Path $sourceRoot '.cargo'
New-Item -ItemType Directory -Path $cargoConfigDirectory | Out-Null
Push-Location $sourceRoot
$previousCargoTargetDir = $env:CARGO_TARGET_DIR
try {
    $vendorConfig = & cargo vendor --locked vendor
    if ($LASTEXITCODE -ne 0) { throw 'Could not vendor every locked Cargo dependency.' }
    $configPath = Join-Path $cargoConfigDirectory 'config.toml'
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllLines($configPath, [string[]] $vendorConfig, $utf8NoBom)
    [System.IO.File]::AppendAllText($configPath, "`n[net]`noffline = true`n", $utf8NoBom)

    $env:CARGO_TARGET_DIR = Join-Path $staging 'target'
    & cargo build --locked --offline --release --target $target --package expos
    if ($LASTEXITCODE -ne 0) { throw 'Offline release build from vendored tag source failed.' }
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

$builtExecutable = Join-Path $staging "target\$target\release\expos.exe"
if (-not (Test-Path -LiteralPath $builtExecutable -PathType Leaf)) {
    throw 'The Windows release build did not produce expos.exe.'
}

Copy-Item -LiteralPath $builtExecutable -Destination (Join-Path $binaryBundle 'expos.exe')
foreach ($file in @('README.md', 'RELEASE_NOTES.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt')) {
    Copy-Item -LiteralPath (Join-Path $sourceRoot $file) -Destination $binaryBundle
}
$artworkLicenseDirectory = Join-Path $binaryBundle 'LICENSES\Fluent11'
New-Item -ItemType Directory -Path $artworkLicenseDirectory | Out-Null
foreach ($file in @('LICENSE', 'AUTHORS.txt')) {
    Copy-Item -LiteralPath (Join-Path $sourceRoot "crates\resources\assets\licenses\fluent11\$file") -Destination $artworkLicenseDirectory
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

$checksumLines = @(
    Get-ChildItem -LiteralPath $dist -File -Filter '*.zip' |
        Sort-Object Name |
        ForEach-Object {
            $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            "$hash  $($_.Name)"
        }
)
if ($checksumLines.Count -ne 2) { throw 'Expected exactly two release archives for checksum generation.' }
[System.IO.File]::WriteAllLines(
    (Join-Path $dist 'SHA256SUMS'),
    [string[]] $checksumLines,
    [System.Text.UTF8Encoding]::new($false)
)
