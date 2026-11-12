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
    if ($clsid -notin @($FastRdp.NativeClsid, $FastRdp.RdpCompatibleClsid)) { throw "Listener '$ListenerName' does not point to a FastRDP CLSID ($clsid); not deleting." }
    Remove-Item $lk -Recurse -Force
    Write-Host "Removed listener key $lk"
}
foreach ($c in $FastRdp.NativeClsid, $FastRdp.RdpCompatibleClsid) {
    $ck = Get-FastRdpClsidKey $c
    # only our own registration (the DLL path ends with the FastRDP provider DLL)
    if ((Test-Path "$ck\InprocServer32") -and (Get-ItemProperty "$ck\InprocServer32").'(default)' -like "*\$($FastRdp.DllName)") {
        Remove-Item $ck -Recurse -Force; Write-Host "Removed COM registration $c"
    }
}
# a registration of another program that the installer replaced: put it back
if (Test-Path $FastRdp.ReplacedClsidBackup) {
    # reg.exe reports success on stderr: let cmd.exe swallow it (PowerShell 5.1 would treat it as an error)
    $null = & cmd.exe /c "reg.exe import `"$($FastRdp.ReplacedClsidBackup)`" >nul 2>&1"
    if ($LASTEXITCODE) { Write-Warning "Could not restore $($FastRdp.ReplacedClsidBackup) - import it manually: reg import `"$($FastRdp.ReplacedClsidBackup)`"" }
    else { Remove-Item $FastRdp.ReplacedClsidBackup -Force; Write-Host "Restored the other program's registration of $($FastRdp.RdpCompatibleClsid)" }
}

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
