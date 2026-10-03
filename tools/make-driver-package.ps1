# Builds the ASLC driver package: self-signed dev cert -> Inf2Cat -> signed .cat.
# Re-runnable; reuses the existing cert unless -Fresh is passed.
# NOTE: this is the no-cost stand-in for the production attestation-signed package.
param([switch]$Fresh, [string]$Os = '11_x64')
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$srcInf = Join-Path $root 'platform\winusb\aslc_aoa.inf'
$stage = Join-Path $root 'platform\winusb\build'
$outCert = Join-Path $root 'installer\cert'

# 1) Certificate: CurrentUser store, code-signing EKU, reused across runs.
$cn = 'ASLC Node Dev Driver'
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object Subject -eq "CN=$cn" | Select-Object -First 1
if ($Fresh -and $cert) { Remove-Item $cert.PSPath -Force; $cert = $null }
if (-not $cert) {
    Write-Host "creating self-signed code-signing certificate 'CN=$cn'"
    $cert = New-SelfSignedCertificate -Type CodeSigningCert `
        -Subject "CN=$cn" -CertStoreLocation Cert:\CurrentUser\My `
        -HashAlgorithm SHA256 -KeyLength 3072 -NotAfter (Get-Date).AddYears(10) `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3,1.3.6.1.4.1.311.10.3.5')
}
New-Item -ItemType Directory -Force -Path $outCert | Out-Null
Export-Certificate -Cert $cert -FilePath (Join-Path $outCert 'aslc-node-dev.cer') -Force | Out-Null
Write-Host "cert thumbprint: $($cert.Thumbprint)"

# 2) Stage the INF (fresh copy so a stale .cat never lands next to it).
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item $srcInf $stage

# 3) inf2cat (from WDK): produces the real SPC driver catalog.
$inf2cat = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Recurse -Filter inf2cat.exe -EA SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $inf2cat) { throw 'inf2cat.exe not found - install the Windows Driver Kit (winget install Microsoft.WindowsWDK.10.0.26100)' }
Write-Host "using $($inf2cat.FullName)"
& $inf2cat.FullName "/driver:$stage" "/os:$Os" | Select-Object -Last 3
if (-not (Test-Path (Join-Path $stage 'aslc_aoa.cat'))) {
    Write-Host "retrying with /os:10_x64"
    & $inf2cat.FullName "/driver:$stage" "/os:10_x64" | Select-Object -Last 3
}
$cat = Join-Path $stage 'aslc_aoa.cat'
if (-not (Test-Path $cat)) { throw 'catalog was not produced by inf2cat' }

# 4) Sign the catalog with the dev cert.
$sig = Set-AuthenticodeSignature -FilePath $cat -Certificate $cert -HashAlgorithm SHA256
if ($sig.Status -ne 'Valid') { throw "catalog signing failed: $($sig.StatusMessage)" }

# 5) Publish to the folder the installer packages from.
$dst = Join-Path $root 'platform\winusb'
Copy-Item $cat (Join-Path $dst 'aslc_aoa.cat') -Force
Write-Host 'OK: platform\winusb\aslc_aoa.cat + installer\cert\aslc-node-dev.cer ready'
