# FastRdpCommon.ps1 - shared constants/helpers for the FastRDP provider scripts (dot-source).
$script:FastRdp = [ordered]@{
    # FastRDP's own CLSID (-NativeClsid) ...
    NativeClsid    = '{80999561-74B1-4880-9408-646E4A5B5187}'
    # ... or the CLSID termsrv treats as RDP (IsRDPProtocol: PROTOCOLTYPE_TSTEST, not registered by
    # Windows): sessions are then reconnected between FastRDP and Microsoft's RDP client
    RdpCompatibleClsid = '{E99FA5E4-A1C9-4495-8BF0-D0972D58E841}'
    InstallDir     = Join-Path $env:ProgramFiles 'FastRDP'
    DllName        = 'fastrdp_provider.dll'
    DataDir        = Join-Path $env:ProgramData 'FastRDP'
    LogDir         = Join-Path $env:ProgramData 'FastRDP\logs'
    BackupRoot     = Join-Path $env:ProgramData 'FastRDP\backup'
    WinStationsKey = 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations'
    # Self-signed code-signing certificate created by drivers\Build-FastRdpIdd.ps1 on the build machine
    DriverCertSubject = 'CN=FastRDP Driver Publisher, O=FastRDP (self-signed)'
}

function Assert-FastRdpAdmin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run elevated (Administrator).'
    }
}

function Get-RdpTcpFingerprint {
    # Hash of every value under WinStations\RDP-Tcp: used to prove we never modify it.
    $k = Join-Path $FastRdp.WinStationsKey 'RDP-Tcp'
    $props = Get-ItemProperty $k | Select-Object * -ExcludeProperty PS*
    $text = ($props.PSObject.Properties | Sort-Object Name | ForEach-Object {
        $v = $_.Value; if ($v -is [byte[]]) { $v = [Convert]::ToBase64String($v) }; "$($_.Name)=$v" }) -join "`n"
    $sha = [Security.Cryptography.SHA256]::Create()
    -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text)) | ForEach-Object { $_.ToString('x2') })
}

function Restart-TermServiceSafely {
    # Stops/starts TermService explicitly (Restart-Service -Force trips over UmRdpService, see Hydra notes)
    # and restores UmRdpService to its previous state. Returns $true when TermService is Running again.
    param([int]$TimeoutSec = 60)
    $um = Get-Service UmRdpService -ErrorAction SilentlyContinue
    $umWasRunning = $um -and $um.Status -eq 'Running'
    $oldPid = (Get-CimInstance Win32_Service -Filter "Name='TermService'").ProcessId
    Write-Host "Stopping TermService pid=$oldPid (UmRdpService was $($um.Status)) ..."
    Stop-Service TermService -Force -ErrorAction Continue
    # TermService can be trigger-started again immediately, so 'Stopped' may never be observed:
    # accept either Stopped or a Running instance with a different PID.
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    do {
        Start-Sleep -Milliseconds 500
        $svc = Get-CimInstance Win32_Service -Filter "Name='TermService'"
    } until ($svc.State -eq 'Stopped' -or ($svc.State -eq 'Running' -and $svc.ProcessId -ne $oldPid) -or (Get-Date) -gt $deadline)
    if ($svc.State -ne 'Running') {
        Write-Host 'Starting TermService ...'
        Start-Service TermService
        (Get-Service TermService).WaitForStatus('Running', [TimeSpan]::FromSeconds($TimeoutSec))
    }
    Write-Host "TermService pid now $((Get-CimInstance Win32_Service -Filter "Name='TermService'").ProcessId)"
    if ($umWasRunning) { Start-Service UmRdpService -ErrorAction Continue }
    $ok = (Get-Service TermService).Status -eq 'Running'
    Write-Host "TermService: $((Get-Service TermService).Status)  UmRdpService: $((Get-Service UmRdpService).Status)"
    $ok
}

function Test-InRdpSession {
    $env:SESSIONNAME -like 'RDP-Tcp*'
}

function Get-FastRdpClsidKey([string]$Clsid) { "HKLM:\SOFTWARE\Classes\CLSID\$Clsid" }
