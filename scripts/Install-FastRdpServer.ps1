<#
.SYNOPSIS  Installs the FastRDP server service ("FastRdpServer", LOCAL SERVICE) and the session input agent.
.DESCRIPTION
  - Copies fastrdp-server.exe, fastrdp-agent.exe (and fastrdp-update.exe if present) to %ProgramFiles%\FastRDP\
  - %ProgramData%\FastRDP\server (TLS key + cert): inheritance removed; SYSTEM + Administrators full,
    LOCAL SERVICE modify. Nobody else can read the private key.
  - Service: LOCAL SERVICE, automatic start, restart on failure: --service --bind <Bind> --listener <Listener>
  - Scheduled task "FastRDP Input Agent": runs fastrdp-agent.exe as the session user at logon and on
    every remote connect (one instance per session).
  - Firewall: inbound UDP <port> for fastrdp-server.exe ("FastRDP Server (QUIC)"). Profiles: -FirewallProfile,
    else those of the existing rule (an update keeps the user's choice), else Private. Warns about
    connected networks the rule does not cover (Windows puts new networks in the Public profile).
  Undo: Uninstall-FastRdpServer.ps1
#>
param(
    [Parameter(Mandatory)][string]$BinDir,
    [string]$Bind = '0.0.0.0:33890',
    [string]$Listener = 'FastRDP',
    [ValidateSet('Domain', 'Private', 'Public', 'Any')][string[]]$FirewallProfile
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
$svcName = 'FastRdpServer'
$server = Join-Path $FastRdp.InstallDir 'fastrdp-server.exe'
$agent = Join-Path $FastRdp.InstallDir 'fastrdp-agent.exe'

$existing = Get-Service $svcName -ErrorAction SilentlyContinue
if ($existing -and $existing.Status -ne 'Stopped') { Stop-Service $svcName -Force; (Get-Service $svcName).WaitForStatus('Stopped', '00:00:20') }
Get-Process fastrdp-agent -ErrorAction SilentlyContinue | Stop-Process -Force
New-Item -ItemType Directory -Force $FastRdp.InstallDir | Out-Null
Copy-Item (Join-Path $BinDir 'fastrdp-server.exe') $server -Force
Copy-Item (Join-Path $BinDir 'fastrdp-agent.exe') $agent -Force
# fastrdp-update.exe may be the process running this install: a running exe cannot be overwritten,
# but it can be renamed
$updater = Join-Path $FastRdp.InstallDir 'fastrdp-update.exe'
$newUpdater = Join-Path $BinDir 'fastrdp-update.exe'
if (Test-Path $newUpdater) {
    Get-ChildItem "$updater.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    try { Copy-Item $newUpdater $updater -Force }
    catch {
        Move-Item $updater "$updater.old-$(Get-Date -Format yyyyMMddHHmmss)" -Force
        Copy-Item $newUpdater $updater -Force
    }
}

$dataDir = Join-Path $FastRdp.DataDir 'server'
New-Item -ItemType Directory -Force $dataDir | Out-Null
& icacls.exe $dataDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-19:(OI)(CI)M' | Out-Null
New-Item -ItemType Directory -Force $FastRdp.LogDir | Out-Null
& icacls.exe $FastRdp.LogDir /grant '*S-1-5-19:(OI)(CI)M' | Out-Null

$bin = "`"$server`" --service --bind $Bind --listener $Listener"
if (-not $existing) {
    New-Service -Name $svcName -BinaryPathName $bin -DisplayName 'FastRDP Server' -StartupType Automatic `
        -Description 'FastRDP QUIC endpoint: authentication relay to the FastRDP protocol provider, video encoding/streaming, input relay.' | Out-Null
} else {
    & sc.exe config $svcName binPath= $bin start= auto | Out-Null
}
& sc.exe config $svcName obj= 'NT AUTHORITY\LocalService' password= '' | Out-Null
& sc.exe failure $svcName reset= 86400 actions= restart/5000/restart/5000/restart/30000 | Out-Null
& sc.exe sidtype $svcName unrestricted | Out-Null

# Input agent: per-session, as the session user.
$taskName = 'FastRDP Input Agent'
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
$action = New-ScheduledTaskAction -Execute $agent
$logon = New-ScheduledTaskTrigger -AtLogOn
$cls = Get-CimClass -Namespace Root/Microsoft/Windows/TaskScheduler -ClassName MSFT_TaskSessionStateChangeTrigger
$remote = New-CimInstance -CimClass $cls -ClientOnly
$remote.StateChange = 3      # TASK_REMOTE_CONNECT
$remote.Enabled = $true
$principal = New-ScheduledTaskPrincipal -GroupId 'S-1-5-32-545' -RunLevel Limited   # BUILTIN\Users
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances Parallel
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger @($logon, $remote) -Principal $principal -Settings $settings `
    -Description 'FastRDP: keyboard/mouse injection agent for the current session (connects to \\.\pipe\FastRDP.Input).' | Out-Null

$port = [int]($Bind.Split(':')[-1])
$ruleName = 'FastRDP Server (QUIC)'
$oldRule = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Select-Object -First 1
$profiles = if ($FirewallProfile) { $FirewallProfile }
            elseif ($oldRule) { "$($oldRule.Profile)" -split ',\s*' }
            else { @('Private') }
Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Protocol UDP -LocalPort $port -Action Allow `
    -Program $server -Profile $profiles | Out-Null
Write-Host "Firewall: UDP $port allowed on $($profiles -join ', ') networks"
foreach ($c in Get-NetConnectionProfile -ErrorAction SilentlyContinue) {
    $cat = if ("$($c.NetworkCategory)" -eq 'DomainAuthenticated') { 'Domain' } else { "$($c.NetworkCategory)" }
    if ($profiles -notcontains 'Any' -and $profiles -notcontains $cat) {
        Write-Warning ("Network '$($c.Name)' ($($c.InterfaceAlias)) is a $cat network: FastRDP cannot be reached through it. " +
            "Make it a Private network (Settings > Network & internet > $($c.InterfaceAlias) > Private network) " +
            "or allow $cat networks: Set-NetFirewallRule -DisplayName '$ruleName' -Profile $((@($profiles) + $cat) -join ',')")
    }
}

Start-Service $svcName
Start-Sleep 2
$fp = Get-Content (Join-Path $dataDir 'cert.sha256') -ErrorAction SilentlyContinue
Get-Service $svcName | Select-Object Name, Status, StartType | Format-Table -AutoSize
Write-Host "Server certificate SHA-256 (clients pin this): $fp"
