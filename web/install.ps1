# FastRDP setup for the computer you connect to (Windows 11 Pro, x64).
#
#   install / update : irm https://fastrdp.dev | iex
#   uninstall        : & ([scriptblock]::Create((irm https://fastrdp.dev))) -Uninstall
#   options          : -Yes (no questions)  -AutoUpdate On|Off  -AllowPublicNetwork  -Purge (uninstall: also server key, logs)
#                      -ClsidMode Native|RdpCompatible (Native: sessions not shared with Microsoft Remote Desktop)
#                      -ReplaceForeignClsid (another program registered FastRDP's RDP-compatible CLSID: replace it)
#
# Source and release files: https://github.com/othelot/fastrdp
# This file is a template: tools/New-FastRdpRelease.ps1 fills in the version and the package hash.
param(
    [switch]$Uninstall,
    [switch]$Yes,
    [switch]$Purge,
    [ValidateSet('Keep', 'On', 'Off')][string]$AutoUpdate = 'Keep',
    # also accept connections on networks Windows classifies as Public
    [switch]$AllowPublicNetwork,
    [ValidateSet('Keep', 'RdpCompatible', 'Native')][string]$ClsidMode = 'Keep',
    [switch]$ReplaceForeignClsid
)

function Invoke-FastRdpSetup {
    param([switch]$Uninstall, [switch]$Yes, [switch]$Purge, [string]$AutoUpdate, [switch]$AllowPublicNetwork, [string]$ClsidMode, [switch]$ReplaceForeignClsid)
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # Windows PowerShell downloads are very slow with a progress bar

    $version = '@VERSION@'
    $zipSha256 = '@ZIP_SHA256@'
    $repo = 'https://github.com/othelot/fastrdp'
    $zipUrl = "$repo/releases/download/v$version/fastrdp-$version-x64.zip"
    $selfUrl = "https://fastrdp.dev/v$version"
    $installDir = Join-Path $env:ProgramFiles 'FastRDP'

    function Confirm-Step([string]$question) {
        if ($Yes) { return }
        $a = Read-Host "$question [y/N]"
        if ($a -notmatch '^(y|yes)$') { throw 'Cancelled.' }
    }

    Write-Host "FastRDP $version setup" -ForegroundColor Cyan

    # ---- environment
    if (-not [Environment]::Is64BitProcess) { throw 'Run this from 64-bit PowerShell.' }
    $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    if ([int]$cv.CurrentBuildNumber -lt 22000 -or $cv.InstallationType -ne 'Client') { throw 'FastRDP requires Windows 11.' }
    if ($cv.EditionID -like 'Core*') { throw "Windows 11 Home cannot host remote sessions; Pro, Enterprise or Education is required (this is $($cv.EditionID))." }

    # ---- elevation: start the same version again in an elevated window
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) {
        $cmd = "& ([scriptblock]::Create((irm '$selfUrl'))) -AutoUpdate $AutoUpdate"
        if ($Uninstall) { $cmd += ' -Uninstall' }
        if ($Yes) { $cmd += ' -Yes' }
        if ($Purge) { $cmd += ' -Purge' }
        if ($AllowPublicNetwork) { $cmd += ' -AllowPublicNetwork' }
        $cmd += " -ClsidMode $ClsidMode"
        if ($ReplaceForeignClsid) { $cmd += ' -ReplaceForeignClsid' }
        $enc = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cmd))
        Write-Host 'Administrator rights are needed - opening an elevated PowerShell window ...'
        Start-Process powershell.exe -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -NoExit -EncodedCommand $enc"
        return
    }
    Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass -Force   # the bundled scripts are files

    $session = "$env:SESSIONNAME"
    if ($session -like 'RDP-Tcp*' -or $session -like 'FastRDP*') {
        Write-Warning "You are connected remotely ($session). Restarting the Remote Desktop service will drop this connection; setup continues in the background (log: %ProgramData%\FastRDP\logs\setup.log)."
        Confirm-Step 'Continue?'
    }

    $work = Join-Path $env:TEMP "fastrdp-setup-$version"
    function Get-Package {
        if (Test-Path $work) { Remove-Item $work -Recurse -Force }
        New-Item -ItemType Directory -Force $work | Out-Null
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $zip = Join-Path $work 'fastrdp.zip'
        Write-Host "Downloading $zipUrl"
        Invoke-WebRequest -UseBasicParsing -Uri $zipUrl -OutFile $zip
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash
        if ($hash -ne $zipSha256) { throw "Package hash mismatch: expected $zipSha256, got $hash. Nothing was installed." }
        Expand-Archive $zip -DestinationPath $work -Force
        $pkg = Join-Path $work "fastrdp-$version-x64"
        Get-ChildItem $pkg -Recurse -File | Unblock-File
        $pkg
    }

    try {
        if ($Uninstall) {
            Confirm-Step 'Remove FastRDP from this computer?'
            $script = Join-Path $installDir 'scripts\Uninstall-FastRdp.ps1'
            if (-not (Test-Path $script)) { $script = Join-Path (Get-Package) 'scripts\Uninstall-FastRdp.ps1' }
            & $script -Purge:$Purge
            Write-Host 'FastRDP removed.' -ForegroundColor Green
            return
        }

        Write-Host @'

This installs:
  - the FastRDP display driver. It is signed with FastRDP's own (self-signed) certificate,
    which is added to this computer's Trusted Root and Trusted Publisher stores;
  - the FastRDP protocol provider: a "FastRDP" listener next to Windows' RDP listener
    (RDP-Tcp is not changed). It is registered so that Windows treats it like RDP: FastRDP and
    Microsoft's Remote Desktop client reconnect to each other's sessions. Sessions opened with
    FastRDP 0.1.3 or older are not compatible: sign out of them once. The Remote Desktop service
    is restarted;
  - the FastRdpServer service (LOCAL SERVICE), a per-session helper task and a firewall rule
    for UDP 33890 on Private networks;
  - automatic updates (signed releases, installed while nobody is connected remotely);
    turn them off with -AutoUpdate Off.
Everything is removed again with -Uninstall.

'@
        Confirm-Step 'Install FastRDP?'
        $fw = @{}
        $public = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object { "$($_.NetworkCategory)" -eq 'Public' })
        if ($AllowPublicNetwork) { $fw.FirewallProfile = @('Private', 'Public') }
        elseif ($public -and -not $Yes) {
            Write-Warning "This computer is connected to a network Windows treats as Public: $(($public | ForEach-Object { "'$($_.Name)' ($($_.InterfaceAlias))" }) -join ', ')."
            Write-Host '  FastRDP is only reachable on Private networks unless you allow Public ones.'
            Write-Host '  Better for a home/office network: Settings > Network & internet > (network) > Private network.'
            $a = Read-Host 'Allow FastRDP on Public networks too? [y/N]'
            if ($a -match '^(y|yes)$') { $fw.FirewallProfile = @('Private', 'Public') }
        }
        $logDir = Join-Path $env:ProgramData 'FastRDP\logs'
        New-Item -ItemType Directory -Force $logDir | Out-Null
        Start-Transcript -Path (Join-Path $logDir 'setup.log') -Append | Out-Null
        try {
            $pkg = Get-Package
            & (Join-Path $pkg 'scripts\Install-FastRdp.ps1') -PackageDir $pkg -AutoUpdate $AutoUpdate @fw -ClsidMode $ClsidMode -ReplaceForeignClsid:$ReplaceForeignClsid
            $fp = Get-Content (Join-Path $env:ProgramData 'FastRDP\server\cert.sha256') -ErrorAction SilentlyContinue
            Write-Host ''
            Write-Host "FastRDP $version installed." -ForegroundColor Green
            Write-Host "Server certificate SHA-256: $fp"
            Write-Host 'Compare it with the fingerprint the client shows on the first connection.'
            Write-Host "Client: $repo/releases/tag/v$version"
        }
        finally { Stop-Transcript | Out-Null }
    }
    finally {
        if (Test-Path $work) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

Invoke-FastRdpSetup -Uninstall:$Uninstall -Yes:$Yes -Purge:$Purge -AutoUpdate $AutoUpdate -AllowPublicNetwork:$AllowPublicNetwork -ClsidMode $ClsidMode -ReplaceForeignClsid:$ReplaceForeignClsid
