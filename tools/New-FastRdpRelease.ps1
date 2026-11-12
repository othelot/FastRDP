<#
.SYNOPSIS  Builds a FastRDP release into target\release-pkg (maintainer tool, PowerShell 7).
.DESCRIPTION
  1. cargo build --workspace --release into target\dist with the build machine's paths remapped
     (--remap-path-prefix; PDB file name only, /PDBALTPATH), drivers\Build-FastRdpIdd.ps1 (signs with
     the build user's FastRDP driver certificate)
  2. fastrdp-<version>-x64.zip: VERSION, bin\ (server, agent, updater, provider DLL), driver\ (signed
     package + .cer), scripts\, README.md, LICENSE, THIRD-PARTY-NOTICES.txt
  3. fastrdp-client-<version>-x64.zip: fastrdp-client.exe, LICENSE, THIRD-PARTY-NOTICES.txt
  4. install.ps1 from web\install.ps1 with the version and the server package SHA-256
  5. SHA256SUMS.txt
  Before zipping, every file is searched for the user name, computer name and profile path of the
  build machine (ASCII and UTF-16); the release stops if one is found.
  -Publish (gh CLI, clean working tree, current commit pushed): creates the GitHub release v<version>
  as a DRAFT with these files and starts the "Sign release" workflow, which (after approval in the
  "release" environment) adds the signed manifest.json and publishes the release.
  The version is [workspace.package] version in Cargo.toml.
#>
param([switch]$SkipBuild, [switch]$Publish)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\.."
Set-Location $root
$repo = 'othelot/fastrdp'

$version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { throw 'workspace version not found in Cargo.toml' }
Write-Host "FastRDP $version" -ForegroundColor Cyan

if (-not $SkipBuild) {
    # no build-machine paths in the binaries (panic locations of dependencies, PDB path)
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    $remap = @(
        "--remap-path-prefix=$env:USERPROFILE=~",
        "--remap-path-prefix=$cargoHome=/cargo",
        "--remap-path-prefix=$rustupHome=/rustup",
        "--remap-path-prefix=$root=/fastrdp",
        '-Clink-arg=/PDBALTPATH:%_PDB%'
    ) | ForEach-Object { "'$_'" }
    # --config arrays are added to .cargo/config.toml's rustflags (+crt-static stays)
    cargo build --workspace --release --target-dir target\dist --config "target.x86_64-pc-windows-msvc.rustflags=[$($remap -join ',')]"
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    & "$root\drivers\Build-FastRdpIdd.ps1"
}

$out = Join-Path $root 'target\release-pkg'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
$rel = Join-Path $root 'target\dist\release'
$drv = Join-Path $root 'target\driver'

# ---- third-party notices
$notices = @"
FastRDP $version - third-party software

OpenH264 (fastrdp-server.exe, fastrdp-client.exe)
  Copyright (c) 2013, Cisco Systems. BSD 2-Clause License. https://github.com/cisco/openh264

Rust crates: see Cargo.lock in the source repository for the full list; each crate is used under
its own license (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 or compatible).
"@

# build-machine identity must not be in anything we publish
$needles = @($env:USERNAME, $env:COMPUTERNAME, $env:USERPROFILE) | Where-Object { $_ } | Sort-Object -Unique
function Assert-NoBuildMachineInfo([string]$dir) {
    $found = foreach ($f in Get-ChildItem $dir -Recurse -File) {
        $bytes = [IO.File]::ReadAllBytes($f.FullName)
        $texts = @([Text.Encoding]::GetEncoding(28591).GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes),
            [Text.Encoding]::Unicode.GetString($bytes, 1, [Math]::Max(0, $bytes.Length - 1)))
        foreach ($n in $needles) {
            if ($texts | Where-Object { $_.IndexOf($n, [StringComparison]::OrdinalIgnoreCase) -ge 0 }) { "$($f.FullName.Substring($dir.Length + 1)): '$n'" }
        }
    }
    if ($found) { throw "build machine information found - not publishing:`n  $($found -join "`n  ")" }
}

function New-Zip([string]$name, [scriptblock]$fill) {
    $stage = Join-Path $out $name
    New-Item -ItemType Directory -Force $stage | Out-Null
    & $fill $stage
    Assert-NoBuildMachineInfo $stage
    $zip = Join-Path $out "$name.zip"
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    Remove-Item $stage -Recurse -Force
    $zip
}

$serverZip = New-Zip "fastrdp-$version-x64" {
    param($d)
    New-Item -ItemType Directory -Force "$d\bin", "$d\driver", "$d\scripts" | Out-Null
    Copy-Item "$rel\fastrdp-server.exe", "$rel\fastrdp-agent.exe", "$rel\fastrdp-update.exe", "$rel\fastrdp_provider.dll" "$d\bin"
    Set-Content "$d\VERSION" $version -NoNewline
    Copy-Item "$drv\FastRdpIdd" "$d\driver" -Recurse
    Copy-Item "$drv\FastRdpDriverPublisher.cer" "$d\driver"
    Copy-Item "$root\scripts\*.ps1" "$d\scripts"
    Copy-Item "$root\README.md", "$root\LICENSE" $d
    Set-Content "$d\THIRD-PARTY-NOTICES.txt" $notices
}
$clientZip = New-Zip "fastrdp-client-$version-x64" {
    param($d)
    Copy-Item "$rel\fastrdp-client.exe", "$root\LICENSE" $d
    Set-Content "$d\THIRD-PARTY-NOTICES.txt" $notices
}

# ---- install.ps1 (ASCII, no BOM: served as text and piped into iex)
$sha = (Get-FileHash $serverZip -Algorithm SHA256).Hash
$install = (Get-Content "$root\web\install.ps1" -Raw).Replace('@VERSION@', $version).Replace('@ZIP_SHA256@', $sha)
if ($install -match '@[A-Z0-9_]+@') { throw "unfilled placeholder in install.ps1: $($Matches[0])" }
if ($install -match '[^\x00-\x7F]') { throw 'install.ps1 must be ASCII' }
$errs = $null
$null = [System.Management.Automation.Language.Parser]::ParseInput($install, [ref]$null, [ref]$errs)
if ($errs) { throw "install.ps1 does not parse: $($errs[0].Message)" }
$installPath = Join-Path $out 'install.ps1'
[IO.File]::WriteAllText($installPath, $install, [Text.Encoding]::ASCII)

$files = @($serverZip, $clientZip, $installPath)
($files | ForEach-Object { "$((Get-FileHash $_ -Algorithm SHA256).Hash.ToLower())  $(Split-Path $_ -Leaf)" }) -join "`n" |
    Set-Content (Join-Path $out 'SHA256SUMS.txt') -NoNewline
$files += Join-Path $out 'SHA256SUMS.txt'
Get-ChildItem $out | Select-Object Name, Length | Format-Table -AutoSize

if ($Publish) {
    if (git status --porcelain) { throw 'working tree not clean' }
    $head = git rev-parse HEAD
    if (-not (git branch -r --contains $head)) { throw "commit $head is not pushed" }
    gh release create "v$version" @files --repo $repo --target $head --draft --title "FastRDP $version" `
        --notes "Install on the computer you connect to (an elevated PowerShell window is opened automatically):`n`n    irm https://fastrdp.dev | iex`n`nClient: fastrdp-client-$version-x64.zip"
    if ($LASTEXITCODE) { throw 'gh release create failed' }
    gh workflow run sign-release.yml --repo $repo -f "tag=v$version"
    if ($LASTEXITCODE) { throw 'could not start the signing workflow' }
    Write-Host "Draft release v$version created. Approve the 'Sign release' run (environment 'release') to sign and publish it:"
    Write-Host "  https://github.com/$repo/actions/workflows/sign-release.yml"
}
