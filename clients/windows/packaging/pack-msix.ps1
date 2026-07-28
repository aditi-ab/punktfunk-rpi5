<#
.SYNOPSIS
  Assemble, pack and sign the punktfunk Windows client as a signed MSIX.

.DESCRIPTION
  Builds a packaging layout from a release `cargo build` output (exe + the reactor/SDL3 auto-staged
  DLLs + resources.pri + FFmpeg DLLs + the checked-in Assets + the manifest), runs makeappx, and
  signs with signtool. Idempotent; safe to re-run.

  Signing cert precedence:
    1. -PfxBase64 / -PfxPassword  (a real or shared code-signing cert, e.g. from CI secrets) — the
       cert's subject DN MUST match -Publisher (which is stamped into the manifest Identity).
    2. otherwise an EPHEMERAL self-signed code-signing cert with subject = -Publisher is generated
       in-process. The package installs only where that cert is trusted, so the matching public
       .cer is exported next to the .msix for the user to import (Trusted People) before install.
       Swap in a real cert later with zero manifest changes — just pass -PfxBase64/-Publisher.

  Run on the Windows runner (or the dev VM) with the MSVC/Windows SDK present.

.EXAMPLE
  # x64 (default arch):
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix
  # arm64 (point -TargetDir + FFMPEG_DIR at the ARM64 build/tree):
  $env:FFMPEG_DIR='C:\Users\Public\ffmpeg-arm64'
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -Arch arm64 -TargetDir C:\t-a64\aarch64-pc-windows-msvc\release -OutDir C:\t-a64\msix
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                     # 4-part numeric, e.g. 0.2.137.0
    [Parameter(Mandatory = $true)][string]$TargetDir,                   # cargo --release output dir (has the exe)
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',                 # package ProcessorArchitecture + artifact suffix
    [string]$FfmpegBin = $(if ($env:FFMPEG_DIR) { Join-Path $env:FFMPEG_DIR 'bin' } else { 'C:\Users\Public\ffmpeg\bin' }),
    [string]$OutDir = (Join-Path $TargetDir 'msix'),
    [string]$Publisher = 'CN=unom',                                     # MUST equal the signing cert subject DN
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                        # optional: base64 of a code-signing .pfx
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Version must be 4-part numeric (Major.Minor.Build.Revision); got '$Version'."
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$assets = Join-Path $here 'assets'
$manifestTemplate = Join-Path $here 'AppxManifest.xml'

# --- locate the Windows SDK tools (newest makeappx/signtool under the x64 kit bin) ---
function Find-SdkTool([string]$name) {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    # match only versioned x64 kit bins (…\10\bin\10.0.NNNNN.N\x64\tool.exe) and pick the newest
    $hit = Get-ChildItem -Path $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(10\.0\.\d+\.\d+)\\x64\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(10\.0\.\d+\.\d+)\\x64\\').Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$name not found under $root — install the Windows 10/11 SDK." }
    $hit.FullName
}
$makeappx = Find-SdkTool 'makeappx.exe'
$signtool = Find-SdkTool 'signtool.exe'
Write-Host "makeappx: $makeappx"
Write-Host "signtool: $signtool"

# --- assemble the package layout ---
$layout = Join-Path $OutDir 'layout'
if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
New-Item -ItemType Directory -Force -Path (Join-Path $layout 'Assets') | Out-Null

# binaries + auto-staged runtime bits (reactor stages the App SDK bootstrap DLL + resources.pri,
# the sdl3 crate stages SDL3.dll — see crate build output). punktfunk-session.exe is the Vulkan
# session client the shell spawns for every stream (sibling resolution — see clients/windows/
# src/spawn.rs); Skia links statically and vulkan-1.dll is a GPU-driver component, so the session
# adds no DLLs of its own.
$required = @('punktfunk-client.exe', 'punktfunk-session.exe', 'punktfunk-console.exe', 'punktfunk.exe', 'Microsoft.WindowsAppRuntime.Bootstrap.dll', 'SDL3.dll', 'resources.pri')
foreach ($f in $required) {
    $src = Join-Path $TargetDir $f
    if (-not (Test-Path $src)) { throw "missing build artifact '$f' in $TargetDir (did 'cargo build --release' run?)" }
    Copy-Item $src (Join-Path $layout $f) -Force
}

# FFmpeg runtime DLLs (the exe link-imports the decode set; copy them all — small and correct).
# These are unmodified BtbN *lgpl-shared* builds, linked dynamically (replaceable DLLs) — FFmpeg is
# used under the LGPL v2.1+; the license text + notice ship in licenses\ below.
$ff = Get-ChildItem -Path $FfmpegBin -Filter *.dll -ErrorAction SilentlyContinue
if (-not $ff) { throw "no FFmpeg DLLs in $FfmpegBin" }
$ff | ForEach-Object { Copy-Item $_.FullName (Join-Path $layout $_.Name) -Force }

# license/attribution payload (MSIX has no installer EULA page, so ship them as files): FFmpeg's LGPL
# notice + license text, the project's own MIT/Apache texts, and the generated third-party notices.
$licDir = Join-Path $layout 'licenses'
New-Item -ItemType Directory -Force -Path $licDir | Out-Null
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
Copy-Item (Join-Path $repoRoot 'packaging\windows\licenses\FFmpeg-LGPL-NOTICE.txt') $licDir -Force -ErrorAction SilentlyContinue
foreach ($n in @('THIRD-PARTY-NOTICES.txt', 'LICENSE-MIT', 'LICENSE-APACHE')) {
    $p = Join-Path $repoRoot $n
    if (Test-Path $p) { Copy-Item $p $licDir -Force }
}
$ffRoot = Split-Path $FfmpegBin -Parent
foreach ($lic in @('LICENSE.txt', 'LICENSE', 'COPYING.LGPLv2.1', 'COPYING.LGPLv3', 'COPYING.txt')) {
    $p = Join-Path $ffRoot $lic
    if (Test-Path $p) { Copy-Item $p $licDir -Force }
}

# tile/store assets
Copy-Item (Join-Path $assets '*') (Join-Path $layout 'Assets') -Force

# manifest with version + publisher + architecture substituted
$manifest = (Get-Content -Raw $manifestTemplate).Replace('{VERSION}', $Version).Replace('{PUBLISHER}', $Publisher).Replace('{ARCH}', $Arch)
Set-Content -Path (Join-Path $layout 'AppxManifest.xml') -Value $manifest -Encoding UTF8

# --- resource index (resources.pri) ---
# The shell resolves the manifest's logo assets through MRT, so the qualified variants
# (Square44x44Logo.targetsize-*_altform-unplated.png — the alpha-transparent taskbar icons) only
# take effect if a pri indexes them; without one the taskbar falls back to plating the base
# 44x44 onto a solid square (the white-cornered icon). makepri's default config indexes the
# layout's asset files AND merges any existing .pri it finds (reactor's staged WinUI resources)
# via its PRI indexer, yielding one combined resources.pri. Output lands outside the layout
# first — the reactor pri is an input while indexing — then replaces it.
$makepri = Find-SdkTool 'makepri.exe'
$priconfig = Join-Path $OutDir 'priconfig.xml'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
& $makepri createconfig /cf $priconfig /dq en-US /o
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed ($LASTEXITCODE)" }
$priOut = Join-Path $OutDir 'resources.pri'
if (Test-Path $priOut) { Remove-Item $priOut -Force }
& $makepri new /pr $layout /cf $priconfig /mn (Join-Path $layout 'AppxManifest.xml') /of $priOut /o
if ($LASTEXITCODE -ne 0) { throw "makepri new failed ($LASTEXITCODE)" }
Move-Item $priOut (Join-Path $layout 'resources.pri') -Force

Write-Host "layout assembled at $layout :"
Get-ChildItem $layout -Recurse -File | ForEach-Object { "  $($_.FullName.Substring($layout.Length + 1))" }

# --- pack ---
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$msix = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}.msix"
& $makeappx pack /o /d $layout /p $msix
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed ($LASTEXITCODE)" }

# --- signing cert (supplied stable pfx OR ephemeral self-signed) ---
$pfxPath = Join-Path $OutDir 'signing.pfx'
$cerPath = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}.cer"
if ($PfxBase64) {
    Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
    [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
} else {
    Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
    if (-not $PfxPassword) { $PfxPassword = 'punktfunk' }
    $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
        -KeyUsage DigitalSignature -FriendlyName 'punktfunk MSIX (self-signed)' `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
    $sec = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $pfxPath -Password $sec | Out-Null
    Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
}

# Always export the public .cer from the pfx. For a self-signed / private-trust cert it's the file
# users import once (Trusted People) — a STABLE cert (same pfx every build via the secret) means that
# import is a one-time, per-machine step that keeps working across upgrades. For a public-CA cert
# it's just an unused extra (harmless). The manifest Publisher must equal the cert's subject DN.
$pwsec = if ($PfxPassword) { ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText } else { $null }
$pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $pfxPath -Password $pwsec } else { Get-PfxCertificate -FilePath $pfxPath }
Export-Certificate -Cert $pubCert -FilePath $cerPath | Out-Null
Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
if ($pubCert.Subject -ne $Publisher) {
    Write-Warning "cert subject '$($pubCert.Subject)' != manifest Publisher '$Publisher' — Add-AppxPackage will reject the mismatch. Pass -Publisher '$($pubCert.Subject)'."
}

# --- sign (timestamp best-effort) ---
$signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
& $signtool ($signArgs + @('/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', $msix))
if ($LASTEXITCODE -ne 0) {
    Write-Warning "timestamped sign failed — retrying without a timestamp"
    & $signtool ($signArgs + @($msix))
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed ($LASTEXITCODE)" }
}
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> MSIX: $msix"
Write-Host "==> trust the cert once per machine (then it stays trusted across all future builds):"
Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
# emit paths for the workflow to publish (only under CI, where GITHUB_ENV is set)
if ($env:GITHUB_ENV) {
    "MSIX_PATH=$msix" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "MSIX_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
}
