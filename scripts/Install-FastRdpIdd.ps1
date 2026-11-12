<#
.SYNOPSIS  Stages the FastRdpIdd remote IDD driver package in the driver store (elevated).
.DESCRIPTION
  - Trusts the self-signed FastRDP driver certificate made by Build-FastRdpIdd.ps1: LocalMachine\Root (chain)
    + LocalMachine\TrustedPublisher (silent PnP install). Only certificates with the expected subject are
    accepted; certificates of earlier builds are removed. Uninstall-FastRdpIdd.ps1 removes the trust again.
  - Does NOT enable testsigning, does NOT change Secure Boot.
  - pnputil /add-driver only: no device is created here. The RD stack creates
    SWD\RemoteDisplayEnum\FastRDP_RemoteIDD&SessionId_N per session when the provider returns that HW ID.
  - Grants LOCAL SERVICE (WUDFHost account) write access to %ProgramData%\FastRDP\logs for idd.log.
  - Enables the Microsoft-Windows-DriverFrameworks-UserMode/Operational log (previous state recorded).
#>
param(
    [Parameter(Mandatory)][string]$PackageDir,
    [Parameter(Mandatory)][string]$CertFile
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
$inf = Join-Path (Resolve-Path $PackageDir) 'FastRdpIdd.inf'
$stateFile = Join-Path $FastRdp.DataDir 'idd-state.json'

$cert = [Security.Cryptography.X509Certificates.X509Certificate2]::new((Resolve-Path $CertFile).Path)
if ($cert.Subject -ne $FastRdp.DriverCertSubject) { throw "Unexpected certificate subject $($cert.Subject)" }
foreach ($store in 'Root', 'TrustedPublisher') {
    $s = [Security.Cryptography.X509Certificates.X509Store]::new($store, 'LocalMachine')
    $s.Open('ReadWrite')
    $s.Certificates | Where-Object { $_.Subject -eq $FastRdp.DriverCertSubject -and $_.Thumbprint -ne $cert.Thumbprint } |
        ForEach-Object { $s.Remove($_); Write-Host "Removed older FastRDP driver certificate $($_.Thumbprint) from LocalMachine\$store" }
    $s.Close()
}
foreach ($store in 'Root', 'TrustedPublisher') {
    $s = [Security.Cryptography.X509Certificates.X509Store]::new($store, 'LocalMachine')
    $s.Open('ReadWrite'); $s.Add($cert); $s.Close()
}
Write-Host "Trusted driver certificate $($cert.Thumbprint) in LocalMachine\Root + TrustedPublisher"

$sig = Get-AuthenticodeSignature (Join-Path $PackageDir 'FastRdpIdd.dll')
Write-Host "DLL signature: $($sig.Status) ($($sig.SignerCertificate.Subject))"

New-Item -ItemType Directory -Force $FastRdp.LogDir | Out-Null
& icacls.exe $FastRdp.LogDir /grant '*S-1-5-19:(OI)(CI)M' | Out-Null   # LOCAL SERVICE

$ch = 'Microsoft-Windows-DriverFrameworks-UserMode/Operational'
$wasEnabled = (Get-WinEvent -ListLog $ch).IsEnabled
& wevtutil.exe sl $ch /e:true

# /install: present FastRDP devices (live sessions) switch to this package if it ranks higher
$out = & pnputil.exe /add-driver $inf /install 2>&1
$out | ForEach-Object { Write-Host "  $_" }
$published = ($out | Select-String -Pattern 'oem\d+\.inf' | Select-Object -First 1).Matches.Value
if (-not $published -or $LASTEXITCODE -ne 0) { throw "pnputil /add-driver failed (exit $LASTEXITCODE)" }

# Older FastRdpIdd packages and stale device nodes: a re-appearing SWD\RemoteDisplayEnum devnode
# (same session id as before) keeps its previously installed driver, so an update would not load.
$older = Get-ChildItem "$env:windir\INF\oem*.inf" | Where-Object { $_.Name -ne $published } |
    Where-Object { Select-String -Path $_.FullName -SimpleMatch 'FastRDP_RemoteIDD' -Quiet }
foreach ($o in $older) {
    & pnputil.exe /delete-driver $o.Name /uninstall /force | Out-Null
    Write-Host "Removed older package $($o.Name) (exit $LASTEXITCODE)"
}
$stale = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\REMOTEDISPLAYENUM\FASTRDP_REMOTEIDD*' -and -not $_.Present }
foreach ($d in $stale) { & pnputil.exe /remove-device $d.InstanceId | Out-Null }
if ($stale) { Write-Host "Removed $(@($stale).Count) stale FastRDP display device node(s)" }

$prev = if (Test-Path $stateFile) { Get-Content $stateFile | ConvertFrom-Json } else { $null }
[pscustomobject]@{
    PublishedInf = $published
    CertThumbprint = $cert.Thumbprint
    DfxLogWasEnabled = if ($prev) { $prev.DfxLogWasEnabled } else { $wasEnabled }
    Staged = (Get-Date).ToString('s')
} | ConvertTo-Json | Set-Content $stateFile
Write-Host "Staged as $published. Set the provider HW ID to FastRDP_RemoteIDD and connect to load it."
