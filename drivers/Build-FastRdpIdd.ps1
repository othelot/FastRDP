<#
.SYNOPSIS  Builds, verifies, packages and signs the FastRdpIdd UMDF driver with a local self-signed certificate.
.DESCRIPTION
  1. MSBuild (WDK WindowsUserModeDriver10.0 toolset, IddCx 1.10 (runs on >= 1.4), UMDF 2.25, /W4 /WX, code analysis)
  2. Package: target\driver\FastRdpIdd\{FastRdpIdd.inf, FastRdpIdd.dll, FastRdpIdd.cat}
  3. infverif (standalone; the MSBuild-integrated task fails to load in this WDK install)
  4. Self-signed code-signing certificate "CN=FastRDP Driver Publisher, O=FastRDP (self-signed)" (build machine CurrentUser\My,
     private key NOT exportable); public part exported to target\driver\FastRdpDriverPublisher.cer, which
     Install-FastRdpIdd.ps1 adds to the target machine's trust stores. Not a Microsoft/WHQL signature.
  5. signtool: embed-sign the DLL, Inf2Cat, sign the catalog. Verify with signtool verify /pa is NOT expected
     to pass on the build machine (cert not trusted there) - trust is established on the target by the installer.
#>
param([ValidateSet('Release', 'Debug')][string]$Configuration = 'Release')
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\.."
$proj = Join-Path $PSScriptRoot 'FastRdpIdd\FastRdpIdd.vcxproj'
$kit = "${env:ProgramFiles(x86)}\Windows Kits\10"
$ver = '10.0.28000.0'
$signtool = "$kit\bin\$ver\x64\signtool.exe"
$inf2cat = "$kit\bin\$ver\x86\Inf2Cat.exe"
$infverif = "$kit\Tools\$ver\x64\infverif.exe"
$msbuild = "${env:ProgramFiles}\Microsoft Visual Studio\18\Community\MSBuild\Current\Bin\MSBuild.exe"

& $msbuild $proj -p:Configuration=$Configuration -p:Platform=x64 -v:minimal -nologo
if ($LASTEXITCODE) { throw "msbuild failed ($LASTEXITCODE)" }
$bin = Join-Path $root "target\driver\$Configuration"

$pkg = Join-Path $root 'target\driver\FastRdpIdd'
if (Test-Path $pkg) { Remove-Item $pkg -Recurse -Force }
New-Item -ItemType Directory -Force $pkg | Out-Null
Copy-Item "$bin\FastRdpIdd.inf", "$bin\FastRdpIdd.dll" $pkg
Copy-Item "$bin\FastRdpIdd.pdb" (Join-Path $root 'target\driver\') -Force

Write-Host '--- infverif /v /u'
& $infverif /v /u (Join-Path $pkg 'FastRdpIdd.inf')
if ($LASTEXITCODE) { throw "infverif failed ($LASTEXITCODE)" }

$subject = 'CN=FastRDP Driver Publisher, O=FastRDP (self-signed)'
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $subject -and $_.NotAfter -gt (Get-Date).AddDays(7) } | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation Cert:\CurrentUser\My `
        -KeyExportPolicy NonExportable -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(1)
    Write-Host "Created driver signing certificate $($cert.Thumbprint)"
}
Get-ChildItem (Join-Path $root 'target\driver') -Filter *.cer -ErrorAction SilentlyContinue | Remove-Item -Force
$cer = Join-Path $root 'target\driver\FastRdpDriverPublisher.cer'
Export-Certificate -Cert $cert -FilePath $cer -Type CERT | Out-Null

& $signtool sign /fd sha256 /sha1 $cert.Thumbprint /s My (Join-Path $pkg 'FastRdpIdd.dll')
if ($LASTEXITCODE) { throw 'signtool (dll) failed' }
& $inf2cat /driver:$pkg /os:10_CO_X64,10_NI_X64,10_GE_X64 /uselocaltime
if ($LASTEXITCODE) { throw 'Inf2Cat failed' }
& $signtool sign /fd sha256 /sha1 $cert.Thumbprint /s My (Join-Path $pkg 'FastRdpIdd.cat')
if ($LASTEXITCODE) { throw 'signtool (cat) failed' }

Get-ChildItem $pkg | Select-Object Name, Length, @{n = 'Sha256'; e = { (Get-FileHash $_.FullName).Hash.Substring(0, 16) } } | Format-Table -AutoSize
"Signer thumbprint: $($cert.Thumbprint)"
