<#
.SYNOPSIS  Removes the FastRdpServer service, the input-agent task, firewall rule and binaries.
           -RemoveIdentity also deletes the server TLS key/certificate.
#>
param([switch]$RemoveIdentity)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
$svcName = 'FastRdpServer'
if (Get-Service $svcName -ErrorAction SilentlyContinue) {
    Stop-Service $svcName -Force -ErrorAction SilentlyContinue
    & sc.exe delete $svcName | Out-Null
    Write-Host "Service $svcName removed"
}
Unregister-ScheduledTask -TaskName 'FastRDP Input Agent' -Confirm:$false -ErrorAction SilentlyContinue
Get-Process fastrdp-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Get-NetFirewallRule -DisplayName 'FastRDP Server (QUIC)' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Start-Sleep 1
foreach ($f in 'fastrdp-server.exe', 'fastrdp-agent.exe') {
    $p = Join-Path $FastRdp.InstallDir $f
    if (Test-Path $p) { Remove-Item $p -Force }
}
if ($RemoveIdentity) { Remove-Item (Join-Path $FastRdp.DataDir 'server') -Recurse -Force -ErrorAction SilentlyContinue }
Write-Host 'FastRDP server removed.'
