param([string]$WixDll)
$ErrorActionPreference = 'Stop'
$applicationRoot = Split-Path -Parent $PSScriptRoot
$wixTool = Get-Command wix -ErrorAction SilentlyContinue
if (-not $wixTool -and -not $WixDll) {
    $localTool = Join-Path $applicationRoot 'target\wix-4.0.6\tools\net6.0\any\wix.dll'
    if (Test-Path -LiteralPath $localTool) { $WixDll = $localTool }
}
if (-not $wixTool -and (-not $WixDll -or -not (Test-Path -LiteralPath $WixDll))) { throw 'WiX 4 or newer is required to build the MSI. No installer or Windows registration has been changed.' }
Push-Location -LiteralPath $applicationRoot
try {
    & cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $releaseDirectory = Join-Path $applicationRoot 'target\release'
    $installerDirectory = Join-Path $applicationRoot 'target\installer'
    New-Item -ItemType Directory -Path $installerDirectory -Force | Out-Null
    $buildArguments = @('build', (Join-Path $PSScriptRoot 'installer.wxs'), '-arch', 'x64', '-d', "SourceDir=$releaseDirectory", '-d', "IdentityFile=$(Join-Path $PSScriptRoot 'notification-identity.txt')", '-o', (Join-Path $installerDirectory 'StickyNotesPlus.msi'))
    if ($wixTool) { & $wixTool.Source @buildArguments } else { & dotnet $WixDll @buildArguments }
    if ($LASTEXITCODE -ne 0) { throw 'Installer build failed.' }
    Write-Output (Join-Path $installerDirectory 'StickyNotesPlus.msi')
} finally { Pop-Location }
