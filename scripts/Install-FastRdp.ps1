<#
.SYNOPSIS  Installs or updates FastRDP from an extracted release package (elevated).
.DESCRIPTION
  Used by the one-line installer (https://fastrdp.dev) and by fastrdp-update.exe.
    1. display driver   (Install-FastRdpIdd.ps1)
    2. server service, session agent task (Install-FastRdpServer.ps1), fastrdp-update.exe
    3. keeps this package in %ProgramFiles%\FastRDP\packages (rollback / uninstall; current + previous)
       and its scripts in %ProgramFiles%\FastRDP\scripts
    4. HKLM\SOFTWARE\FastRDP: InstalledVersion, AutoUpdate; scheduled task "FastRDP Update" (SYSTEM)
    5. protocol provider (Install-FastRdpProvider.ps1) - last, because restarting the Remote Desktop
       service drops remote connections. The caller is responsible for warning about that.
    6. checks that the server service and the Remote Desktop service run
  Undo: Uninstall-FastRdp.ps1
.PARAMETER AutoUpdate
  On / Off, or Keep (default): keep the current setting; a first install turns updates on.
#>
param(
    [Parameter(Mandatory)][string]$PackageDir,
    [ValidateSet('Keep', 'On', 'Off')][string]$AutoUpdate = 'Keep',
    # firewall profiles for UDP 33890 (default: keep the existing rule's, else Private)
    [ValidateSet('Domain', 'Private', 'Public', 'Any')][string[]]$FirewallProfile,
    # started by fastrdp-update.exe (the update task must not be re-registered while it runs)
    [switch]$Unattended
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\FastRdpCommon.ps1"
Assert-FastRdpAdmin
$PackageDir = (Resolve-Path $PackageDir).Path.TrimEnd('\')
$version = (Get-Content (Join-Path $PackageDir 'VERSION') -Raw).Trim()
$settingsKey = 'HKLM:\SOFTWARE\FastRDP'
$current = Get-ItemProperty $settingsKey -ErrorAction SilentlyContinue
$previous = $current.InstalledVersion
$auto = switch ($AutoUpdate) {
    'On' { 1 }
    'Off' { 0 }
    default { if ($null -eq $current.AutoUpdate) { 1 } else { [int]$current.AutoUpdate } }
}
Write-Host "Installing FastRDP $version$(if ($previous) { " (installed: $previous)" })"

# 1 + 2
& "$PSScriptRoot\Install-FastRdpIdd.ps1" -PackageDir (Join-Path $PackageDir 'driver\FastRdpIdd') -CertFile (Join-Path $PackageDir 'driver\FastRdpDriverPublisher.cer')
$fw = @{}
if ($FirewallProfile) { $fw.FirewallProfile = $FirewallProfile }
& "$PSScriptRoot\Install-FastRdpServer.ps1" -BinDir (Join-Path $PackageDir 'bin') @fw

# 3
$packages = Join-Path $FastRdp.InstallDir 'packages'
$keep = Join-Path $packages "fastrdp-$version-x64"
New-Item -ItemType Directory -Force $packages | Out-Null
if ($PackageDir -ine $keep) {
    if (Test-Path $keep) { Remove-Item $keep -Recurse -Force }
    Copy-Item $PackageDir $keep -Recurse
}
Get-ChildItem $packages -Directory | Where-Object { $_.Name -notin @("fastrdp-$version-x64", "fastrdp-$previous-x64") } | Remove-Item -Recurse -Force
$scriptsDir = Join-Path $FastRdp.InstallDir 'scripts'
New-Item -ItemType Directory -Force $scriptsDir | Out-Null
Copy-Item (Join-Path $keep 'scripts\*.ps1') $scriptsDir -Force

# 4
New-Item $settingsKey -Force | Out-Null
Set-ItemProperty $settingsKey -Name InstalledVersion -Value $version
Set-ItemProperty $settingsKey -Name AutoUpdate -Value $auto -Type DWord
$taskName = 'FastRDP Update'
$task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($auto -and -not ($task -and $Unattended)) {
    $exe = Join-Path $FastRdp.InstallDir 'fastrdp-update.exe'
    $action = New-ScheduledTaskAction -Execute $exe -Argument 'apply --scheduled'
    $boot = New-ScheduledTaskTrigger -AtStartup
    $boot.Delay = 'PT10M'
    $every = New-ScheduledTaskTrigger -Once -At (Get-Date).Date.AddHours(3) -RepetitionInterval (New-TimeSpan -Hours 4) -RandomDelay (New-TimeSpan -Minutes 30)
    $principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest
    $set = New-ScheduledTaskSettingsSet -StartWhenAvailable -RunOnlyIfNetworkAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit (New-TimeSpan -Hours 1) -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger @($boot, $every) -Principal $principal -Settings $set -Force `
        -Description 'FastRDP: installs signed FastRDP updates when no remote session is active. Log: %ProgramData%\FastRDP\logs\update.log' | Out-Null
    Write-Host 'Automatic updates: on'
} elseif (-not $auto) {
    if ($task) { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
    Write-Host 'Automatic updates: off'
}

# 5
& "$PSScriptRoot\Install-FastRdpProvider.ps1" -DllPath (Join-Path $PackageDir 'bin\fastrdp_provider.dll') -RestartTermService -AcknowledgeRdpDisconnect

# 6
Start-Sleep 2
$problems = @()
if ((Get-Service FastRdpServer).Status -ne 'Running') { $problems += 'FastRdpServer service is not running' }
if ((Get-Service TermService).Status -ne 'Running') { $problems += 'Remote Desktop Services (TermService) is not running' }
if (-not (Test-Path (Join-Path $FastRdp.WinStationsKey 'FastRDP'))) { $problems += 'FastRDP listener is not registered' }
if ($problems) { throw "FastRDP $version installed with problems: $($problems -join '; ')" }
Write-Host "FastRDP $version installed."
