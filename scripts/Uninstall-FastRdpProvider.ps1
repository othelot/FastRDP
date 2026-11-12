<#
.SYNOPSIS  Removes the FastRDP listener + COM registration (+ DLL). RDP-Tcp is never touched.
.EXAMPLE   .\Uninstall-FastRdpProvider.ps1 -RestartTermService
#>
[CmdletBinding()]
param(
    [string]$ListenerName = 'FastRDP',
    [switch]$RestartTermService,
    [switch]$RemoveLogs,
    [switch]$AcknowledgeRdpDisconnect
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
if ($ListenerName -match '^(RDP-Tcp|Console)$') { throw "Refusing to delete reserved listener '$ListenerName'." }
if ($RestartTermService -and (Test-InRdpSession) -and -not $AcknowledgeRdpDisconnect) {
    throw 'You are in an RDP session; restarting TermService will disconnect you. Re-run with -AcknowledgeRdpDisconnect.'
}
$rdpBefore = Get-RdpTcpFingerprint

$lk = Join-Path $FastRdp.WinStationsKey $ListenerName
if (Test-Path $lk) {
    $clsid = (Get-ItemProperty $lk).LoadableProtocol_Object
    if ($clsid -ne $FastRdp.Clsid) { throw "Listener '$ListenerName' does not point to the FastRDP CLSID ($clsid); not deleting." }
    Remove-Item $lk -Recurse -Force
    Write-Host "Removed listener key $lk"
}
if (Test-Path $FastRdp.ClsidKey) { Remove-Item $FastRdp.ClsidKey -Recurse -Force; Write-Host 'Removed COM registration' }

if ($RestartTermService) {
    # Restart first so svchost unloads the (pinned) DLL, then delete it.
    $null = Restart-TermServiceSafely
}
$dll = Join-Path $FastRdp.InstallDir $FastRdp.DllName
if (Test-Path $dll) {
    try { Remove-Item $dll -Force; Write-Host "Deleted $dll" }
    catch { Write-Warning "$dll still loaded (TermService not restarted). It will be removable after the next TermService restart." }
}
if ($RemoveLogs -and (Test-Path $FastRdp.LogDir)) { Remove-Item $FastRdp.LogDir -Recurse -Force }

$rdpAfter = Get-RdpTcpFingerprint
if ($rdpBefore -ne $rdpAfter) { throw 'RDP-Tcp fingerprint changed during uninstall - investigate.' }
Write-Host "RDP-Tcp fingerprint unchanged: $rdpAfter"
