<#
.SYNOPSIS  Removes FastRDP completely (elevated): update task, server, display driver, provider,
           installed packages and settings. Restarts the Remote Desktop service, which drops remote
           connections. -Purge also deletes %ProgramData%\FastRDP (server key, logs, backups).
#>
param([switch]$Purge)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin

# this script's own folder is deleted below: run from a copy
if ($PSScriptRoot -like "$($FastRdp.InstallDir)\*") {
    $tmp = Join-Path $env:TEMP "fastrdp-uninstall-$PID"
    Copy-Item $PSScriptRoot $tmp -Recurse -Force
    try { & (Join-Path $tmp 'Uninstall-FastRdp.ps1') -Purge:$Purge }
    finally { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
    return
}

Unregister-ScheduledTask -TaskName 'FastRDP Update' -Confirm:$false -ErrorAction SilentlyContinue
Get-Process fastrdp-update -ErrorAction SilentlyContinue | Stop-Process -Force
& "$PSScriptRoot\Uninstall-FastRdpServer.ps1" -RemoveIdentity:$Purge
& "$PSScriptRoot\Uninstall-FastRdpIdd.ps1"
& "$PSScriptRoot\Uninstall-FastRdpProvider.ps1" -RestartTermService -AcknowledgeRdpDisconnect

foreach ($p in 'packages', 'scripts') {
    $d = Join-Path $FastRdp.InstallDir $p
    if (Test-Path $d) { Remove-Item $d -Recurse -Force }
}
Get-ChildItem $FastRdp.InstallDir -Filter 'fastrdp-update.exe*' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
if ((Test-Path $FastRdp.InstallDir) -and -not (Get-ChildItem $FastRdp.InstallDir -Force)) { Remove-Item $FastRdp.InstallDir -Force }
Remove-Item 'HKLM:\SOFTWARE\FastRDP' -Recurse -Force -ErrorAction SilentlyContinue
if ($Purge) { Remove-Item $FastRdp.DataDir -Recurse -Force -ErrorAction SilentlyContinue }
Write-Host 'FastRDP removed.'
