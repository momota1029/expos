param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Beta')]
    [ValidatePattern('\A[1-9][0-9]*\z')]
    [string] $RunNumber,

    [Parameter(Mandatory = $true, ParameterSetName = 'Manual')]
    [string] $Tag,

    [string] $RepositoryRoot = (Join-Path $PSScriptRoot '..')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Cargo.toml and Cargo.lock both have version keys outside the expos package table.
function Get-ExposVersionField {
    param([string] $Text, [string] $TableHeader, [string] $FileName)

    $tables = [regex]::Matches($Text, "(?ms)^[ \t]*$TableHeader[ \t]*(?:#[^\r\n]*)?\r?\n(?<body>.*?)(?=^[ \t]*\[|\z)")
    $packages = @($tables | Where-Object {
        $_.Groups['body'].Value -cmatch '(?m)^[ \t]*name[ \t]*=[ \t]*"expos"[ \t]*(?:#[^\r\n]*)?\r?$'
    })
    if ($packages.Count -ne 1) {
        throw "Expected exactly one expos package table in $FileName; found $($packages.Count)."
    }

    $body = $packages[0].Groups['body']
    $versions = [regex]::Matches($body.Value, '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<version>[^"\r\n]+)"[ \t]*(?:#[^\r\n]*)?\r?$')
    if ($versions.Count -ne 1) {
        throw "Expected exactly one expos version field in $FileName; found $($versions.Count)."
    }

    $version = $versions[0].Groups['version']
    return [pscustomobject] @{ Value = $version.Value; Index = $body.Index + $version.Index }
}

$root = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$manifestPath = Join-Path $root 'Cargo.toml'
$lockPath = Join-Path $root 'Cargo.lock'
$manifest = [System.IO.File]::ReadAllText($manifestPath)
$lock = [System.IO.File]::ReadAllText($lockPath)
$manifestVersion = Get-ExposVersionField $manifest '\[package\]' 'Cargo.toml'
$lockVersion = Get-ExposVersionField $lock '\[\[package\]\]' 'Cargo.lock'
if ($manifestVersion.Value -cne $lockVersion.Value) {
    throw "expos versions differ in Cargo.toml and Cargo.lock ($($manifestVersion.Value) and $($lockVersion.Value))."
}

$basePattern = '(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)'
$preId = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
$versionPattern = "\A(?<base>$basePattern)(?<prerelease>-$preId(?:\.$preId)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z"
$cargoVersion = [regex]::Match($manifestVersion.Value, $versionPattern)
if (-not $cargoVersion.Success) { throw 'The expos Cargo version must be a semantic version.' }

$head = & git -C $root rev-parse HEAD
if ($LASTEXITCODE -ne 0) { throw 'Could not resolve the checked-out commit.' }
$head = ($head -join '').Trim()

if ($PSCmdlet.ParameterSetName -eq 'Beta') {
    $base = $cargoVersion.Groups['base'].Value
    $basePath = Join-Path $root 'release/channel-base.txt'
    if (Test-Path -LiteralPath $basePath) {
        $base = [System.IO.File]::ReadAllText($basePath).Trim()
        if ($base -cnotmatch "\A$basePattern\z") {
            throw 'release/channel-base.txt must contain a major.minor.patch version.'
        }
    }

    $version = "$base-beta.$RunNumber"
    $tag = "v$version"
    $isPrerelease = $true
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    $manifest = $manifest.Remove($manifestVersion.Index, $manifestVersion.Value.Length).Insert($manifestVersion.Index, $version)
    $lock = $lock.Remove($lockVersion.Index, $lockVersion.Value.Length).Insert($lockVersion.Index, $version)
    [System.IO.File]::WriteAllText($manifestPath, $manifest, $utf8NoBom)
    [System.IO.File]::WriteAllText($lockPath, $lock, $utf8NoBom)
}
else {
    if (-not $Tag.StartsWith('v', [StringComparison]::Ordinal)) {
        throw 'Tag must be a v-prefixed semantic version.'
    }
    $version = $Tag.Substring(1)
    $tagVersion = [regex]::Match($version, $versionPattern)
    if (-not $tagVersion.Success) { throw 'Tag must be a v-prefixed semantic version.' }

    $tagCommit = & git -C $root rev-parse --verify "refs/tags/$Tag^{commit}"
    if ($LASTEXITCODE -ne 0) { throw 'The requested tag does not resolve to a commit.' }
    if (($tagCommit -join '').Trim() -cne $head) {
        throw 'The requested tag does not point to the checked-out commit.'
    }
    if ($version -cne $manifestVersion.Value) {
        throw "Tag version $version does not match Cargo package version $($manifestVersion.Value)."
    }
    $isPrerelease = $tagVersion.Groups['prerelease'].Success
}

[pscustomobject] @{
    Tag = $tag
    Version = $version
    Commit = $head
    IsPrerelease = $isPrerelease
}
