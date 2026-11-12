<#
.SYNOPSIS
  Registers the FastRDP protocol provider as an ADDITIONAL TermService listener.

.DESCRIPTION
  Changes (all reversible with Uninstall-FastRdpProvider.ps1, backups in %ProgramData%\FastRDP\backup):
    1. Copies fastrdp_provider.dll to %ProgramFiles%\FastRDP\
    2. HKLM\SOFTWARE\Classes\CLSID\{80999561-...}\InprocServer32 = DLL, ThreadingModel=Both
    3. %ProgramData%\FastRDP\logs with Modify for NETWORK SERVICE (TermService account)
    4. HKLM\...\Terminal Server\WinStations\<ListenerName> with LoadableProtocol_Object=<CLSID>,
       fEnableWinStation=1 and FastRdp* settings.
  It NEVER writes WinStations\RDP-Tcp; a fingerprint of RDP-Tcp is taken before/after and compared.
  It does NOT patch termsrv.dll, change ServiceDll, or touch licensing/session policies.

  TermService is only restarted with -RestartTermService (needed for the listener to load).
  Restarting TermService drops every RDP session on the machine.

.EXAMPLE
  .\Install-FastRdpProvider.ps1 -DllPath .\fastrdp_provider.dll -RestartTermService
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$DllPath,
    [ValidateLength(1, 20)][string]$ListenerName = 'FastRDP',
    [string]$IddHardwareId = 'FastRDP_RemoteIDD',
    # virtual channels (clipboard, audio, ...) are on by default
    [switch]$NoVirtualChannels,
    [switch]$RestartTermService,
    [switch]$AcknowledgeRdpDisconnect
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
if ($ListenerName -match '^(RDP-Tcp|Console)$') { throw "Refusing to use reserved listener name '$ListenerName'." }
if ($RestartTermService -and (Test-InRdpSession) -and -not $AcknowledgeRdpDisconnect) {
    throw 'You are in an RDP session; restarting TermService will disconnect you. Re-run with -AcknowledgeRdpDisconnect.'
}
$DllPath = (Resolve-Path $DllPath).Path

# ---- backup / state ----
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$bk = Join-Path $FastRdp.BackupRoot "install-$stamp"
New-Item -ItemType Directory -Force $bk | Out-Null
& reg.exe export 'HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations' (Join-Path $bk 'WinStations.reg') /y | Out-Null
if (Test-Path $FastRdp.ClsidKey) {
    & reg.exe export "HKLM\SOFTWARE\Classes\CLSID\$($FastRdp.Clsid)" (Join-Path $bk 'clsid.reg') /y | Out-Null
}
$rdpBefore = Get-RdpTcpFingerprint
@"
FastRDP install $stamp
EMERGENCY UNDO (from an elevated console, no RDP needed):
  reg delete "HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\$ListenerName" /f
  reg delete "HKLM\SOFTWARE\Classes\CLSID\$($FastRdp.Clsid)" /f
  sc stop TermService & sc start TermService
Full restore of the WinStations subtree: reg import "$bk\WinStations.reg"
"@ | Set-Content (Join-Path $bk 'UNDO.txt')

# ---- 1. files ----
New-Item -ItemType Directory -Force $FastRdp.InstallDir | Out-Null
$target = Join-Path $FastRdp.InstallDir $FastRdp.DllName
$loaded = [bool](Get-Process -Id (Get-CimInstance Win32_Service -Filter "Name='TermService'").ProcessId -Module -ErrorAction SilentlyContinue |
        Where-Object FileName -eq $target)
$same = (Test-Path $target) -and ((Get-FileHash $target).Hash -eq (Get-FileHash $DllPath).Hash)
if ($same) {
    Write-Host 'DLL unchanged - not copying.'
} elseif ($loaded) {
    if (-not $RestartTermService) { throw "$target is loaded by TermService and differs; re-run with -RestartTermService." }
    # TermService is trigger-restarted immediately, so it cannot be kept stopped. A loaded image can be
    # renamed on NTFS: move it aside, drop in the new file, and the restart below picks it up.
    $aside = "$target.old-$stamp"
    Write-Host "DLL is loaded and differs: renaming loaded copy to $aside"
    Move-Item $target $aside -Force
    Copy-Item $DllPath $target -Force
    Get-ChildItem "$target.old-*" | Where-Object FullName -ne $aside | Remove-Item -Force -ErrorAction SilentlyContinue
} else {
    Copy-Item $DllPath $target -Force
}
$hash = (Get-FileHash $target -Algorithm SHA256).Hash

# ---- 2. COM registration (HKLM, in-proc) ----
New-Item -Force $FastRdp.ClsidKey | Out-Null
Set-ItemProperty $FastRdp.ClsidKey -Name '(default)' -Value 'FastRDP Protocol Manager (IWRdsProtocolManager)'
$inproc = Join-Path $FastRdp.ClsidKey 'InprocServer32'
New-Item -Force $inproc | Out-Null
Set-ItemProperty $inproc -Name '(default)' -Value $target
Set-ItemProperty $inproc -Name 'ThreadingModel' -Value 'Both'

# ---- 3. log directory: NETWORK SERVICE may write ----
New-Item -ItemType Directory -Force $FastRdp.LogDir | Out-Null
& icacls.exe $FastRdp.LogDir /grant '*S-1-5-20:(OI)(CI)M' | Out-Null   # S-1-5-20 = NETWORK SERVICE

# ---- 4. listener key (a NEW sibling of RDP-Tcp) ----
$lk = Join-Path $FastRdp.WinStationsKey $ListenerName
New-Item -Force $lk | Out-Null
New-ItemProperty $lk -Name 'LoadableProtocol_Object' -PropertyType String -Value $FastRdp.Clsid -Force | Out-Null
New-ItemProperty $lk -Name 'fEnableWinStation' -PropertyType DWord -Value 1 -Force | Out-Null
New-ItemProperty $lk -Name 'Comment' -PropertyType String -Value 'FastRDP custom protocol listener' -Force | Out-Null
New-ItemProperty $lk -Name 'FastRdpIddHardwareId' -PropertyType String -Value $IddHardwareId -Force | Out-Null
New-ItemProperty $lk -Name 'FastRdpVirtualChannels' -PropertyType DWord -Value ([int](-not $NoVirtualChannels)) -Force | Out-Null

$rdpAfter = Get-RdpTcpFingerprint
if ($rdpBefore -ne $rdpAfter) { throw 'RDP-Tcp fingerprint changed - this must never happen. Investigate before continuing.' }

[pscustomobject]@{
    Stamp = $stamp; Listener = $ListenerName
    Dll = $target; Sha256 = $hash; RdpTcpFingerprint = $rdpAfter; Backup = $bk
} | ConvertTo-Json | Set-Content (Join-Path $FastRdp.DataDir 'install-state.json')

Write-Host "Installed listener '$ListenerName' (\\.\pipe\FastRDP.Provider.$ListenerName), DLL sha256 $hash"
Write-Host "RDP-Tcp fingerprint unchanged: $rdpAfter"
Write-Host "Backup + emergency undo: $bk"

if ($RestartTermService) {
    $ok = Restart-TermServiceSafely
    if (-not $ok) { Write-Warning 'TermService did not come back. See UNDO.txt.' }
} else {
    Write-Host 'TermService NOT restarted: the listener loads at the next TermService start.'
}
