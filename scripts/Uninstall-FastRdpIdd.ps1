<#
.SYNOPSIS  Removes every staged FastRdpIdd package, its devnodes, and the test-certificate trust.
#>
param([switch]$KeepCertificate)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
$stateFile = Join-Path $FastRdp.DataDir 'idd-state.json'
$state = if (Test-Path $stateFile) { Get-Content $stateFile | ConvertFrom-Json } else { $null }

# Remove devnodes created by the RD stack (present or phantom).
Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\REMOTEDISPLAYENUM\FASTRDP_REMOTEIDD*' } | ForEach-Object {
    & pnputil.exe /remove-device $_.InstanceId | Out-Null
    Write-Host "Removed device $($_.InstanceId)"
}

# Delete all driver-store packages whose original name is FastRdpIdd.inf (exact match only).
$enum = (& pnputil.exe /enum-drivers) -join "`n"
$blocks = $enum -split "(?:\r?\n){2,}"
foreach ($b in $blocks) {
    if ($b -match '(?im)^\s*\S.*:\s*(oem\d+\.inf)\s*$' -and $b -match '(?im):\s*fastrdpidd\.inf\s*$') {
        $oem = [regex]::Match($b, 'oem\d+\.inf', 'IgnoreCase').Value
        & pnputil.exe /delete-driver $oem /uninstall /force | Out-Null
        Write-Host "Deleted driver package $oem"
    }
}

if (-not $KeepCertificate -and $state.CertThumbprint) {
    foreach ($store in 'Root', 'TrustedPublisher') {
        $s = [Security.Cryptography.X509Certificates.X509Store]::new($store, 'LocalMachine')
        $s.Open('ReadWrite')
        $s.Certificates | Where-Object { $_.Thumbprint -eq $state.CertThumbprint -and $_.Subject -eq $FastRdp.DriverCertSubject } |
            ForEach-Object { $s.Remove($_); Write-Host "Removed cert from LocalMachine\$store" }
        $s.Close()
    }
}
if ($state -and -not $state.DfxLogWasEnabled) {
    & wevtutil.exe sl 'Microsoft-Windows-DriverFrameworks-UserMode/Operational' /e:false
}
if (Test-Path $stateFile) { Remove-Item $stateFile }
Write-Host 'FastRdpIdd removed.'
