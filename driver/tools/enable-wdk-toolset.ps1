# enable-wdk-toolset.ps1
#
# VS Build Tools does not ship the WDK `WindowsKernelModeDriver10.0` MSBuild toolset, and the
# WDK's WDK.vsix refuses to attach to Build Tools (VSIXInstaller error 2003 = no compatible
# product). The .vsix is a ZIP that bundles the exact MSBuild overlay the VS installer would
# deploy (under `$MSBuild\Microsoft\VC\v170\...`). This script deploys that overlay into the
# VS Build Tools MSBuild tree, enabling `PlatformToolset=WindowsKernelModeDriver10.0`.
#
# Run once, elevated (it writes under Program Files).

#Requires -RunAsAdministrator

$ErrorActionPreference = 'Stop'

# Locate the WDK VSIX (VS2022 payload).
$vsix = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Vsix\VS2022' -Recurse -Filter 'WDK.vsix' -ErrorAction Stop |
        Sort-Object FullName -Descending | Select-Object -First 1
if (-not $vsix) { throw "WDK.vsix not found under Windows Kits\10\Vsix\VS2022" }
Write-Host "Using $($vsix.FullName)"

# Find the VS Build Tools MSBuild root (VCTargetsPath parent = ...\MSBuild).
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found" }
$vsInstall = & $vswhere -latest -products * -property installationPath
$msbuildRoot = Join-Path $vsInstall 'MSBuild'
if (-not (Test-Path $msbuildRoot)) { throw "MSBuild root not found: $msbuildRoot" }

# Extract the VSIX (a ZIP) and deploy its $MSBuild overlay.
$tmp = Join-Path $env:TEMP ("aslc_wdk_vsix_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
Copy-Item $vsix.FullName (Join-Path $tmp 'wdk.zip')
Expand-Archive (Join-Path $tmp 'wdk.zip') -DestinationPath (Join-Path $tmp 'x') -Force

$overlay = Join-Path $tmp 'x\$MSBuild'
if (-not (Test-Path $overlay)) { throw "Unexpected VSIX layout: $overlay missing" }

Write-Host "Deploying WDK MSBuild overlay -> $msbuildRoot"
Copy-Item -Path (Join-Path $overlay '*') -Destination $msbuildRoot -Recurse -Force

$toolset = Join-Path $msbuildRoot 'Microsoft\VC\v170\Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.props'
if (Test-Path $toolset) {
    Write-Host "OK: WindowsKernelModeDriver10.0 toolset installed."
} else {
    throw "Deployment finished but the toolset is still missing: $toolset"
}

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
