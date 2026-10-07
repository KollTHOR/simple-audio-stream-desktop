# Install-time helper: remove any previously published ASLC AOA driver package(s) so a stale or
# invalidly-signed catalog cannot outrank the freshly signed one. Runs elevated (Inno [Run]).
$ErrorActionPreference = 'SilentlyContinue'

$txt = (pnputil /enum-drivers) -join "`n"
$matches = [regex]::Matches($txt, 'Published Name:\s*(oem\d+\.inf)[\s\S]*?Original Name:\s*aslc_aoa\.inf')
foreach ($m in $matches) {
    $pub = $m.Groups[1].Value
    Write-Host "removing previous ASLC driver package $pub"
    pnputil /delete-driver $pub /uninstall /force | Out-Null
}
exit 0
