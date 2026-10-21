[CmdletBinding()]
param(
    [string]$OutputDirectory = '',
    [string]$ExpectedTag = '',
    [switch]$SkipTests
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $IsWindows) { throw 'Build this script on Windows with the MSVC toolchain.' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $projectRoot 'dist' }
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
$releaseDirectory = Join-Path $outputRoot 'release'
New-Item -ItemType Directory -Force -Path $releaseDirectory | Out-Null

function Invoke-Tool([string]$File, [string[]]$Arguments) {
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$File failed with exit code $LASTEXITCODE" }
}

# Use Microsoft's installed developer-shell script, not a third-party setup action.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) { throw 'Visual Studio vswhere.exe is required.' }
$vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $vsRoot) { throw 'Visual Studio x64 C++ tools are required.' }
& (Join-Path $vsRoot 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
$env:RUSTFLAGS = '-C target-feature=+crt-static'
$target = 'x86_64-pc-windows-msvc'

Push-Location $projectRoot
try {
    $metadataText = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $metadata = ($metadataText -join "`n") | ConvertFrom-Json
    $version = @($metadata.packages | Where-Object name -eq 'wechat-winh')[0].version
    if ($version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') { throw 'Invalid Cargo version.' }
    if ($ExpectedTag -and $ExpectedTag -ne "v$version") { throw "Tag $ExpectedTag does not match Cargo v$version" }
    if (-not $SkipTests) {
        Invoke-Tool 'cargo' @('test', '--locked', '--workspace', '--lib', '--bins', '--target', $target)
    }
    Invoke-Tool 'cargo' @('build', '--locked', '--release', '--target', $target,
        '-p', 'wechat-winh', '-p', 'wechat-winh-bridge', '-p', 'wechat-winh-gui')

    $portableName = "weiwin-$version-windows-x64-portable.zip"
    $installerName = "weiwin-$version-windows-x64-setup.exe"
    $releaseNames = @($portableName, $installerName, 'SHA256SUMS.txt')
    $unexpected = @(Get-ChildItem -LiteralPath $releaseDirectory | Where-Object { $_.Name -notin $releaseNames })
    if ($unexpected.Count) { throw 'Output release directory contains unrelated files; choose a fresh output directory.' }
    $stage = Join-Path $outputRoot ("stage-$version-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    $binaries = Join-Path $projectRoot "target\$target\release"
    # Explicit files only. Never recursively copy a build, repository, or profile directory.
    Copy-Item -LiteralPath (Join-Path $binaries '微win.exe') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $binaries 'wechat-winh.exe') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $binaries 'wechat_winh_bridge.dll') -Destination (Join-Path $stage "wechat_winh_bridge-$version.dll")
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $stage
    Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE') -Destination $stage
    foreach ($name in @('Start.cmd', 'Stop.cmd')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $stage
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'licenses\OCTICONS-LICENSE.txt') -Destination $stage
    Invoke-Tool 'python' @((Join-Path $PSScriptRoot 'verify_payload.py'), '--stage', $stage, '--version', $version)
    $portable = Join-Path $releaseDirectory $portableName
    Compress-Archive -LiteralPath @(Get-ChildItem -LiteralPath $stage -File | ForEach-Object FullName) -DestinationPath $portable -Force
    Invoke-Tool 'python' @((Join-Path $PSScriptRoot 'verify_payload.py'), '--zip', $portable, '--version', $version)

    $innoCandidates = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
    )
    $inno = @($innoCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1)
    if (-not $inno.Count) { throw 'Inno Setup 6 is absent. No system tools were installed automatically.' }
    Invoke-Tool $inno[0] @('/Qp', "/DAppVersion=$version", "/DStageDir=$stage", "/O$releaseDirectory", (Join-Path $PSScriptRoot 'weiwin.iss'))
    $installer = Join-Path $releaseDirectory $installerName
    if (-not (Test-Path -LiteralPath $installer)) { throw 'Installer output is missing.' }
    $sumLines = foreach ($file in @($portable, $installer)) {
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant()
        "$hash  $([IO.Path]::GetFileName($file))"
    }
    [IO.File]::WriteAllText((Join-Path $releaseDirectory 'SHA256SUMS.txt'), ($sumLines -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
    if ($env:GITHUB_OUTPUT) {
        [IO.File]::AppendAllText($env:GITHUB_OUTPUT, "version=$version`n", [Text.UTF8Encoding]::new($false))
    }
    Write-Host "Release files: $releaseDirectory"
} finally {
    Pop-Location
}
