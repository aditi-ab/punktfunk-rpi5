<#
.SYNOPSIS
  Assemble, pack and sign the punktfunk Windows client as a signed MSIX.

.DESCRIPTION
  Builds a packaging layout from a release `cargo build` output (exe + the reactor/SDL3 auto-staged
  DLLs + resources.pri + the checked-in Assets + the manifest), runs makeappx, and
  signs with signtool. Idempotent; safe to re-run.

  NO FFmpeg DLLs since M10 (design/client-native-decode.md §6): the client decodes natively
  (pf-vkdecode / pf-dxvadec / openh264+rav1d) and link-imports no libav* at all, so the
  wildcard copy and its LGPL notice are gone with it. The HOST installer is unchanged —
  packaging/windows/pack-host-installer.ps1 still ships them for its amf-qsv encode path.

  Signing cert precedence:
    0. Azure Artifact Signing (formerly Trusted Signing) when AZURE_CODESIGNING_ENDPOINT/_ACCOUNT/
       _PROFILE are all set. HSM-backed, so there is no .pfx and nothing to export: the chain is
       publicly trusted, so no .cer is produced and MSIX_CER_PATH stays unset.
    1. -PfxBase64 / -PfxPassword  (a real or shared code-signing cert, e.g. from CI secrets) — the
       cert's subject DN MUST match -Publisher (which is stamped into the manifest Identity).
    2. otherwise an EPHEMERAL self-signed code-signing cert with subject = -Publisher is generated
       in-process. The package installs only where that cert is trusted, so the matching public
       .cer is exported next to the .msix for the user to import (Trusted People) before install.
       This fallback is for canary/CI/dev ONLY: on a v* tag build a missing cert is a hard failure
       (-RequireSignedCert), never a silent downgrade to a throwaway cert.

  WHICHEVER mode runs, the signed .msix is read back and its signer subject compared to -Publisher;
  a mismatch fails the build. MSIX package identity is Name + Publisher, so a publisher that does
  not match the signer is not a cosmetic problem — Add-AppxPackage rejects the package outright,
  and it would only be discovered by a user trying to install the release.

  Run on the Windows runner (or the dev VM) with the MSVC/Windows SDK present.

.EXAMPLE
  # x64 (default arch):
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix
  # arm64 (point -TargetDir at the ARM64 build):
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -Arch arm64 -TargetDir C:\t-a64\aarch64-pc-windows-msvc\release -OutDir C:\t-a64\msix
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                     # 4-part numeric, e.g. 0.2.137.0
    [Parameter(Mandatory = $true)][string]$TargetDir,                   # cargo --release output dir (has the exe)
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',                 # package ProcessorArchitecture + artifact suffix
    [string]$OutDir = (Join-Path $TargetDir 'msix'),
    # MUST equal the signing cert subject DN — this is the verified subject the Azure 'unom-io'
    # certificate profile issues. The 'ü' is written as an escape, not a literal: this file is UTF-8
    # with no BOM, and read by anything other than pwsh 7 a literal would silently mojibake into a
    # publisher that no longer matches the signer, which surfaces only as an Add-AppxPackage refusal
    # on a user's machine. Verified against the real signer after signing below.
    [string]$Publisher = "CN=unom - Enrico B$([char]0xFC)hler, O=unom - Enrico B$([char]0xFC)hler, L=Rottweil, S=Baden-W$([char]0xFC)rttemberg, C=DE",
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                        # optional: base64 of a code-signing .pfx
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    # Azure Artifact Signing. All three select it, ahead of any .pfx. Credentials arrive through the
    # environment via DefaultAzureCredential (AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET)
    # rather than as arguments, so they cannot leak into a process listing or a transcript.
    [string]$AzureEndpoint = $env:AZURE_CODESIGNING_ENDPOINT,           # e.g. https://neu.codesigning.azure.net/
    [string]$AzureAccount = $env:AZURE_CODESIGNING_ACCOUNT,             # signing account name
    [string]$AzureProfile = $env:AZURE_CODESIGNING_PROFILE,             # certificate profile name
    [string]$AzureDlib = $env:AZURE_CODESIGNING_DLIB,                   # path to Azure.CodeSigning.Dlib.dll
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto'
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
# Azure.CodeSigning.Dlib.dll ships in the Microsoft.Trusted.Signing.Client NuGet package, which has
# no installer and no fixed location — hence an explicit override first, then the paths the runner
# setup uses (packaging/windows/README.md). Newest wins, so a package update needs no edit here.
function Find-AzureDlib([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path $Explicit)) { throw "AZURE_CODESIGNING_DLIB points at a missing file: $Explicit" }
        return (Resolve-Path $Explicit).Path
    }
    $roots = @(
        (Join-Path $env:USERPROFILE '.nuget\packages\microsoft.trusted.signing.client'),
        'C:\trusted-signing\microsoft.trusted.signing.client'
    ) | Where-Object { $_ -and (Test-Path $_) }
    $hit = $roots | ForEach-Object { Get-ChildItem -Path $_ -Recurse -Filter 'Azure.CodeSigning.Dlib.dll' -ErrorAction SilentlyContinue } |
        Where-Object { $_.FullName -match '\\bin\\x64\\' } |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $hit) {
        throw ("Azure.CodeSigning.Dlib.dll not found. Install the signing client on this box, e.g. " +
               "``nuget install Microsoft.Trusted.Signing.Client -OutputDirectory " +
               "`$env:USERPROFILE\.nuget\packages``, or set AZURE_CODESIGNING_DLIB to its full path.")
    }
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

# license/attribution payload (MSIX has no installer EULA page, so ship them as files): the
# project's own MIT/Apache texts plus the generated third-party notices, which is where every
# vendored/statically-linked dependency's attribution lives (openh264 BSD-2, rav1d BSD-2, …).
#
# The FFmpeg LGPL notice + license texts that used to be copied here went with the DLLs at M10:
# nothing in this package links libav* any more, so shipping an LGPL notice would be claiming a
# dependency that is not there.
#
# For the same reason the notices come from clients/windows/ and NOT from the repo root: the root
# file is workspace-wide, it is what the HOST ships out of, and it still lists ffmpeg-next plus the
# full FFmpeg licence text. The client-scoped file (same generator, `--packages
# punktfunk-client-windows,punktfunk-client-session,punktfunk-cli`) is the one that describes what
# is actually inside this .msix — and it is the same file the app's Licenses page shows.
$licDir = Join-Path $layout 'licenses'
New-Item -ItemType Directory -Force -Path $licDir | Out-Null
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$clientRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
foreach ($n in @('LICENSE-MIT', 'LICENSE-APACHE')) {
    $p = Join-Path $repoRoot $n
    if (Test-Path $p) { Copy-Item $p $licDir -Force }
}
$notices = Join-Path $clientRoot 'THIRD-PARTY-NOTICES.txt'
if (-not (Test-Path $notices)) {
    throw "missing $notices — run scripts/gen-third-party-notices.sh (it generates the per-client copies)"
}
Copy-Item $notices $licDir -Force

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
# FAIL CLOSED on a real release. The ephemeral fallback below exists so canary/CI/dev builds keep
# working without the secret, but it is a per-build throwaway cert: nobody can pin it, and a package
# signed with one is indistinguishable from a package signed by an attacker's. Silently falling back
# on a tag build would ship exactly that to users under the release's name — so on refs/tags/v* an
# absent MSIX_CERT_PFX_B64 is a build failure, not a downgrade. ('auto' resolves from GITHUB_REF, so
# a workflow can't forget to opt in; -RequireSignedCert true/false overrides for local testing.)
$requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
               else { [Convert]::ToBoolean($RequireSignedCert) }
$pfxPath = Join-Path $OutDir 'signing.pfx'
$cerPath = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}.cer"
$azureMetadata = Join-Path $OutDir 'azure-codesigning.json'
$signMode = 'selfsigned'
if ($AzureEndpoint -and $AzureAccount -and $AzureProfile) {
    $signMode = 'azure'
    $AzureDlib = Find-AzureDlib $AzureDlib
    # signtool takes the account/profile from this file (/dmdf), not the command line.
    @{
        Endpoint               = $AzureEndpoint
        CodeSigningAccountName = $AzureAccount
        CertificateProfileName = $AzureProfile
    } | ConvertTo-Json | Set-Content -Path $azureMetadata -Encoding utf8
    Write-Host "signing via Azure Artifact Signing: $AzureAccount/$AzureProfile at $AzureEndpoint"
    Write-Host "  dlib: $AzureDlib"
    foreach ($v in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
        if (-not [Environment]::GetEnvironmentVariable($v)) {
            throw ("Azure signing selected but $v is not set. The dlib authenticates with " +
                   "DefaultAzureCredential; without the service-principal trio it falls through to an " +
                   "interactive login that cannot complete on a runner and hangs the build.")
        }
    }
} elseif ($PfxBase64) {
    $signMode = 'pfx'
    Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
    [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
} elseif ($requireCert) {
    throw ("release build ($env:GITHUB_REF) with neither AZURE_CODESIGNING_* nor MSIX_CERT_PFX_B64 — " +
           "refusing to fall back to an ephemeral self-signed cert. Restore the signing secrets " +
           "(packaging/windows/README.md), or pass -RequireSignedCert false if this really is a test build.")
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

# Export the public .cer from the pfx. For a self-signed / private-trust cert it's the file users
# import once (Trusted People) — a STABLE cert (same pfx every build via the secret) means that
# import is a one-time, per-machine step that keeps working across upgrades. Azure signing is
# HSM-backed: there is no pfx to read and its chain is publicly trusted, so no .cer is produced.
if ($signMode -ne 'azure') {
    $pwsec = if ($PfxPassword) { ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText } else { $null }
    $pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $pfxPath -Password $pwsec } else { Get-PfxCertificate -FilePath $pfxPath }
    Export-Certificate -Cert $pubCert -FilePath $cerPath | Out-Null
    Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
}

# --- sign ---
# The timestamp is best-effort for a .pfx whose cert outlives the release, but MANDATORY under Azure
# signing: those leaf certs are minted per request and expire in ~3 days, so an untimestamped
# signature stops verifying within days of shipping. Retrying without one there would produce a
# package that installs on the runner and fails for every user that weekend — so the fallback is
# gated on the mode rather than applied blindly.
if ($signMode -eq 'azure') {
    $signArgs = @('sign', '/fd', 'SHA256', '/dlib', $AzureDlib, '/dmdf', $azureMetadata)
    $ts = 'http://timestamp.acs.microsoft.com'
} else {
    $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
    if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
    $ts = 'http://timestamp.digicert.com'
}
& $signtool ($signArgs + @('/tr', $ts, '/td', 'SHA256', $msix))
if ($LASTEXITCODE -ne 0) {
    if ($signMode -eq 'azure') {
        throw ("timestamped sign failed ($LASTEXITCODE) — NOT retrying without a timestamp. An Azure " +
               "signing cert is valid for ~3 days; an untimestamped signature would go untrusted " +
               "within days of release.")
    }
    Write-Warning "timestamped sign failed — retrying without a timestamp"
    & $signtool ($signArgs + @($msix))
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed ($LASTEXITCODE)" }
}
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue
Remove-Item $azureMetadata -Force -ErrorAction SilentlyContinue

# Read the signature back off the packed .msix and hold it against the manifest Publisher. MSIX
# package identity is Name + Publisher, so a publisher that doesn't match the signer isn't cosmetic:
# Add-AppxPackage refuses the package outright. Checking the ACTUAL signer (rather than a pfx we
# happen to hold) is the only form of this check that works in every signing mode, and failing the
# build here is the difference between a red pipeline and a release nobody can install.
# Deliberately asymmetric: a subject we CAN read and that DISAGREES is a hard failure, but a subject
# we cannot read at all is only a warning. Get-AuthenticodeSignature's support for the .msix/.appx
# subject interface varies by Windows version, and signtool has already reported success by this
# point — turning "the check could not run" into a build break would trade a real defect we catch for
# an imaginary one we invent.
$signerSubject = $null
try { $signerSubject = (Get-AuthenticodeSignature $msix).SignerCertificate.Subject } catch { }
if (-not $signerSubject) {
    Write-Warning ("could not read a signer subject back from $msix, so Publisher/signer agreement is " +
                   "UNVERIFIED on this box. If the package is rejected at Add-AppxPackage time, compare " +
                   "`signtool verify /pa /v` against the manifest Publisher '$Publisher' by hand.")
} elseif ($signerSubject -ne $Publisher) {
    throw ("signer subject does not match the manifest Publisher, so this package cannot install:`n" +
           "  signer    : '$signerSubject'`n" +
           "  Publisher : '$Publisher'`n" +
           "Pass -Publisher '$signerSubject' (or fix the certificate profile) and repack.")
} else {
    Write-Host "verified signer subject matches manifest Publisher: $signerSubject"
}

Write-Host ""
Write-Host "==> MSIX: $msix"
if ($signMode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA — nothing for users to import."
} else {
    Write-Host "==> trust the cert once per machine (then it stays trusted across all future builds):"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
}
# emit paths for the workflow to publish (only under CI, where GITHUB_ENV is set)
if ($env:GITHUB_ENV) {
    "MSIX_PATH=$msix" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if ($signMode -ne 'azure') { "MSIX_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
