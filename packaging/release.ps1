# Prepares a release: bumps the version in Cargo.toml and Cargo.lock, runs the
# tests exactly as CI does, then commits and tags. -Push also pushes main and
# the tag, which starts the GitHub release build.
#   .\packaging\release.ps1 1.0.4 [-Push]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [switch]$Push
)
$ErrorActionPreference = 'Stop'
$applicationRoot = Split-Path -Parent $PSScriptRoot

# Runs a tool and judges it by its exit code only. Cargo and git print
# progress on stderr, which Windows PowerShell would otherwise treat as an error.
function Invoke-Tool([string]$what, [scriptblock]$command) {
    $preference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $command 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $preference }
    if ($LASTEXITCODE -ne 0) { throw "$what failed." }
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.0.4, not '$Version'." }
Push-Location -LiteralPath $applicationRoot
try {
    $branch = (& git rev-parse --abbrev-ref HEAD).Trim()
    if ($branch -ne 'main') { throw "Releases are made from main; this is '$branch'." }
    if (& git status --porcelain) { throw 'Commit or discard your changes first; the release commit should only bump the version.' }
    Invoke-Tool 'git fetch' { git fetch --quiet origin main --tags }
    if ((& git rev-parse HEAD).Trim() -ne (& git rev-parse origin/main).Trim()) { throw 'main differs from origin/main. Pull or push first.' }
    if (& git tag --list "v$Version") { throw "Tag v$Version already exists." }

    $manifest = Join-Path $applicationRoot 'Cargo.toml'
    $text = [IO.File]::ReadAllText($manifest)
    $current = [regex]::Match($text, '(?m)^version = "(.+)"').Groups[1].Value
    if ([version]$Version -le [version]$current) { throw "Version $Version is not newer than $current." }

    $committed = $false
    try {
        $text = ([regex]'(?m)^version = ".+"').Replace($text, "version = `"$Version`"", 1)
        [IO.File]::WriteAllText($manifest, $text, (New-Object Text.UTF8Encoding $false))
        # Without this, Cargo.lock keeps the old version and CI's --locked fails.
        Invoke-Tool 'cargo update' { cargo update -p sticky-notes-plus-slint --offline }
        Invoke-Tool 'Tests' { cargo test --locked }
        Invoke-Tool 'git add' { git add Cargo.toml Cargo.lock }
        Invoke-Tool 'git commit' { git commit --quiet -m "Release $Version" }
        $committed = $true
        Invoke-Tool 'git tag' { git tag "v$Version" }
    } catch {
        if (-not $committed) {
            Invoke-Tool 'git checkout' { git checkout -- Cargo.toml Cargo.lock }
            Write-Warning 'Nothing was committed; Cargo.toml and Cargo.lock are back as they were.'
        }
        throw
    }

    if ($Push) {
        Invoke-Tool 'git push' { git push origin main }
        Invoke-Tool 'git push of the tag' { git push origin "v$Version" }
        Write-Output "Pushed v$Version. GitHub Actions is building the release."
    } else {
        Write-Output "Committed and tagged v$Version. To publish it:"
        Write-Output '  git push origin main'
        Write-Output "  git push origin v$Version"
    }
} finally { Pop-Location }
