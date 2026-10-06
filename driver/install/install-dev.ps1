# install-dev.ps1
#
# Development install of the ASLC virtual audio driver (test-signed).
#
#   * enables test signing (if not already; a reboot may be required)
#   * creates/uses a self-signed code-signing cert and trusts it (Root + TrustedPublisher)
#   * signs the driver package (.sys + .cat)
#   * installs the root-enumerated device via devcon
#
# Run elevated. This is a *development* path; production signing is a later concern.
#Requires -RunAsAdministrator

$ErrorActionPreference = 'Stop'

$pkg = Join-Path $PSScriptRoot '..\aslc-audio\x64\Release\aslc-audio'
$sys = Join-Path $pkg 'aslc-audio.sys'
$inf = Join-Path $pkg 'aslc-audio.inf'
$cat = Join-Path $pkg 'aslc-audio.cat'
foreach ($f in @($sys, $inf, $cat)) {
    if (-not (Test-Path $f)) { throw "missing package file: $f  (run driver/build/build-driver.ps1 first)" }
}

$certName = 'ASLC Virtual Audio Driver (Test)'
$needReboot = $false

# --- 1. test signing ------------------------------------------------------------------------
$bcd = (& bcdedit /enum '{current}') -join "`n"
if ($bcd -notmatch 'testsigning\s+Yes') {
    Write-Host 'Enabling test signing (reboot required for it to take effect).'
    & bcdedit /set testsigning on | Out-Null
    $needReboot = $true
}

# --- 2. self-signed code-signing cert -------------------------------------------------------
$cert = Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq "CN=$certName" } | Select-Object -First 1
if (-not $cert) {
    Write-Host "Creating self-signed cert '$certName'"
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=$certName" `
        -CertStoreLocation Cert:\LocalMachine\My -KeyUsage DigitalSignature `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3')
}
$cerPath = Join-Path $env:TEMP 'aslc-audio-dev.cer'
Export-Certificate -Cert $cert -FilePath $cerPath -Force | Out-Null
Import-Certificate -FilePath $cerPath -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath $cerPath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
Write-Host "Trusted cert thumbprint: $($cert.Thumbprint)"

# --- 3. sign the package --------------------------------------------------------------------
$signtool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
if (-not $signtool) { throw 'signtool.exe not found' }
Write-Host "Signing with $signtool"
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint /sm /s My $sys
if ($LASTEXITCODE -ne 0) { throw "signtool failed to sign $sys ($LASTEXITCODE)" }
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint /sm /s My $cat
if ($LASTEXITCODE -ne 0) { throw "signtool failed to sign $cat ($LASTEXITCODE)" }

# --- 4. install the root-enumerated device --------------------------------------------------
$devcon = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Tools' -Recurse -Filter devcon.exe -ErrorAction SilentlyContinue |
          Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
if (-not $devcon) { throw 'devcon.exe not found' }
Write-Host "Installing device Root\ASLC via $devcon"
& $devcon install $inf 'Root\ASLC'
if ($LASTEXITCODE -ne 0) { throw "devcon install failed ($LASTEXITCODE)" }

Write-Host ''
if ($needReboot) {
    Write-Host 'DONE. Test signing was just enabled - REBOOT before the driver can load.'
} else {
    Write-Host 'DONE. Test signing already enabled.'
}
