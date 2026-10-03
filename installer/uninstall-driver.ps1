# Uninstall helper: removes the ASLC driver package and its trust certs, so no
# stray OEM driver or self-signed roots are left on the machine.
# Runs elevated (called from Inno's [UninstallRun] which inherits elevation).
$ErrorActionPreference = 'SilentlyContinue'

# 1) Driver package: find published oemXX.inf by original name, remove + unbind.
$pub = $null
try {
    $drv = Get-WindowsDriver -Online | Where-Object { $_.OriginalFileName -like '*aslc_aoa.inf*' }
    $pub = $drv | Select-Object -First 1 -ExpandProperty PublishedInfName
} catch { }
if (-not $pub) {
    # Fallback: parse pnputil enum output.
    $txt = (pnputil /enum-drivers) -join "`n"
    $m = [regex]::Match($txt, 'Published Name:\s*(oem\d+\.inf)[\s\S]*?Original Name:\s*aslc_aoa\.inf')
    if ($m.Success) { $pub = $m.Groups[1].Value }
}
if ($pub) { pnputil /delete-driver $pub /uninstall /force | Out-Null }

# 2) Cert trust entries (only exist if this package installed them).
certutil -delstore Root 'ASLC Node Dev Driver' | Out-Null
certutil -delstore TrustedPeople 'ASLC Node Dev Driver' | Out-Null
exit 0
