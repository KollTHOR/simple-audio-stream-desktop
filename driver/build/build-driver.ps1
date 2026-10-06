# build-driver.ps1
#
# Builds the ASLC virtual audio driver (x64) with the WDK MSBuild toolset.
# Prerequisites:
#   - WDK installed
#   - driver/tools/enable-wdk-toolset.ps1 run once (elevated)
#
# Usage:  pwsh driver/build/build-driver.ps1 [-Configuration Release|Debug]

param(
    [ValidateSet('Debug','Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent   # repo root
$project = Join-Path $root 'driver\aslc-audio\aslc-audio.vcxproj'

if (-not (Test-Path $project)) { throw "driver project not found: $project" }

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsInstall = & $vswhere -latest -products * -property installationPath
$msbuild = Join-Path $vsInstall 'MSBuild\Current\Bin\MSBuild.exe'
if (-not (Test-Path $msbuild)) { throw "MSBuild not found: $msbuild" }

Write-Host "Building $project ($Configuration|x64)"
& $msbuild $project /t:Rebuild /p:Configuration=$Configuration /p:Platform=x64 /v:m /nologo
if ($LASTEXITCODE -ne 0) { throw "driver build failed ($LASTEXITCODE)" }

$out = Join-Path $root "driver\aslc-audio\x64\$Configuration\aslc-audio.sys"
Write-Host "Built: $out"
