<#
.SYNOPSIS
  Pack + sign the punktfunk Windows client as an Inno Setup setup.exe (the default download) and a
  portable .zip, from the layout pack-msix.ps1 already assembled.

.DESCRIPTION
  Runs AFTER pack-msix.ps1 in the same job and consumes its $OutDir\layout verbatim — one assembly,
  three artifacts (.msix, setup.exe, portable .zip). Why the installer exists at all: the MSIX
  install shape (WindowsApps ACLs + alias-only activation) breaks Steam's non-Steam-game picker,
  the Steam overlay's injection, and Big Picture launching — see punktfunk-client.iss's header.

  Steps:
    1. stage the runtime file set from -LayoutDir (drops AppxManifest.xml + the tile Assets),
    2. sign the four exes individually (the MSIX only signs its container),
    3. zip the stage -> the portable build,
    4. ISCC punktfunk-client.iss over the same stage, sign the setup.exe,
    5. emit CLIENT_SETUP_PATH / CLIENT_ZIP_PATH to GITHUB_ENV for the publish step.

  Signing backend precedence is identical to pack-msix.ps1 / pack-host-installer.ps1 (Azure
  Artifact Signing -> supplied .pfx -> ephemeral self-signed; fail closed on v* tags). No .cer is
  exported here: unlike an MSIX, a plain exe RUNS regardless of signer trust — an untrusted
  signature only costs a SmartScreen warning, so canary self-signed builds need nothing imported.

.EXAMPLE
  pwsh -File pack-client-installer.ps1 -Version 0.2.137.0 -Arch x64 `
    -LayoutDir C:\t\msix\layout -OutDir C:\t\installer
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                 # 4-part numeric, same as the MSIX
    [Parameter(Mandatory = $true)][string]$LayoutDir,               # pack-msix.ps1's $OutDir\layout
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',
    [string]$OutDir = (Join-Path (Split-Path -Parent $LayoutDir) 'installer'),
    # Subject for the EPHEMERAL self-signed fallback only; Azure/pfx carry their own subjects.
    [string]$Publisher = "CN=unom - Enrico B$([char]0xFC)hler, O=unom - Enrico B$([char]0xFC)hler, L=Rottweil, S=Baden-W$([char]0xFC)rttemberg, C=DE",
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                    # reuse the client's signing secret
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    [string]$AzureEndpoint = $env:AZURE_CODESIGNING_ENDPOINT,
    [string]$AzureAccount = $env:AZURE_CODESIGNING_ACCOUNT,
    [string]$AzureProfile = $env:AZURE_CODESIGNING_PROFILE,
    [string]$AzureDlib = $env:AZURE_CODESIGNING_DLIB,
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    [switch]$NoSign,                                                # skip signing (local debug)
    # M4 (design/installer-v2-windows.md D1): pack with the unelevated punktfunk-setup-client
    # twin instead of ISCC. Same stage, same output name, same signing. Since M5 the workflow
    # passes it for the published artifact; ISCC (the default here) is the one-release revert
    # path.
    [switch]$Engine
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Keep the "check $LASTEXITCODE myself" model (see pack-host-installer.ps1): pwsh 7.4 must not
# turn a non-zero native exit into a terminating error before Sign-File's timestamp retry runs.
$PSNativeCommandUseErrorActionPreference = $false

if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Version must be 4-part numeric (Major.Minor.Build.Revision); got '$Version'."
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$iss = Join-Path $here 'punktfunk-client.iss'

# --- locate ISCC (Inno Setup) + signtool (Windows SDK) — same finders as the sibling scripts ---
function Find-Iscc {
    foreach ($p in @(
            'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
            'C:\Program Files\Inno Setup 6\ISCC.exe')) {
        if (Test-Path $p) { return $p }
    }
    $c = Get-Command iscc -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    throw "ISCC.exe (Inno Setup 6, any 6.x) not found - install it (choco install innosetup -y)."
}
function Find-SdkTool([string]$name) {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $hit = Get-ChildItem -Path $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(10\.0\.\d+\.\d+)\\x64\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(10\.0\.\d+\.\d+)\\x64\\').Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$name not found under $root - install the Windows 10/11 SDK." }
    $hit.FullName
}
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
$iscc = if ($Engine) { $null } else { Find-Iscc }
if ($iscc) { Write-Host "ISCC: $iscc" }

# --- stage the runtime file set (the portable layout = what the installer lays down) ----------
# Explicit list, not a wildcard copy: the MSIX layout also holds AppxManifest.xml and the tile
# Assets, which mean nothing outside a package (the exes embed their icons via build.rs).
# The ONE Assets\ file that does matter unpackaged is the Lucide icon font: the shell loads it
# via ms-appx:///Assets/lucide.ttf (app/lucide.rs), and unpackaged that URI resolves to the exe
# directory — without Assets\lucide.ttf every icon in the shell renders as a private-use box.
$required = @('punktfunk-client.exe', 'punktfunk-session.exe', 'punktfunk-console.exe', 'punktfunk.exe',
              'Microsoft.WindowsAppRuntime.Bootstrap.dll', 'SDL3.dll', 'resources.pri',
              'Assets\lucide.ttf')
$stage = Join-Path $OutDir 'portable'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'Assets') | Out-Null
foreach ($f in $required) {
    $src = Join-Path $LayoutDir $f
    if (-not (Test-Path $src)) { throw "missing '$f' in $LayoutDir (did pack-msix.ps1 run first?)" }
    Copy-Item $src (Join-Path $stage $f) -Force
}
$licSrc = Join-Path $LayoutDir 'licenses'
if (-not (Test-Path $licSrc)) { throw "missing licenses\ in $LayoutDir (did pack-msix.ps1 run first?)" }
Copy-Item $licSrc (Join-Path $stage 'licenses') -Recurse -Force

# --- signing backend, same precedence + fail-closed rule as pack-msix.ps1 ---------------------
$requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
               else { [Convert]::ToBoolean($RequireSignedCert) }
if ($NoSign -and $requireCert) {
    throw "release build ($env:GITHUB_REF) with -NoSign - refusing to publish an unsigned installer."
}
$pfxPath = Join-Path $OutDir 'signing.pfx'
$azureMetadata = Join-Path $OutDir 'azure-codesigning.json'
$signMode = 'none'
$signtool = $null
if (-not $NoSign) {
    $signtool = Find-SdkTool 'signtool.exe'
    Write-Host "signtool: $signtool"
    if ($AzureEndpoint -and $AzureAccount -and $AzureProfile) {
        $signMode = 'azure'
        $AzureDlib = Find-AzureDlib $AzureDlib
        @{
            Endpoint               = $AzureEndpoint
            CodeSigningAccountName = $AzureAccount
            CertificateProfileName = $AzureProfile
        } | ConvertTo-Json | Set-Content -Path $azureMetadata -Encoding utf8
        Write-Host "signing via Azure Artifact Signing: $AzureAccount/$AzureProfile at $AzureEndpoint"
        foreach ($v in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
            if (-not [Environment]::GetEnvironmentVariable($v)) {
                throw ("Azure signing selected but $v is not set. The dlib authenticates with " +
                       "DefaultAzureCredential; without the service-principal trio it falls through to " +
                       "an interactive login that cannot complete on a runner and hangs the build.")
            }
        }
    }
    elseif ($PfxBase64) {
        $signMode = 'pfx'
        Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
        [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
    }
    elseif ($requireCert) {
        throw ("release build ($env:GITHUB_REF) with neither AZURE_CODESIGNING_* nor MSIX_CERT_PFX_B64 - " +
               "refusing to fall back to an ephemeral self-signed cert. Restore the signing secrets " +
               "(packaging/windows/README.md), or pass -RequireSignedCert false if this really is a test build.")
    }
    else {
        $signMode = 'selfsigned'
        Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
        if (-not $PfxPassword) { $PfxPassword = 'punktfunk' }
        $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName 'punktfunk client installer (self-signed)' `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        $sec = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $pfxPath -Password $sec | Out-Null
        Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
    }
}

# Timestamp policy matches the sibling scripts: best-effort for a long-lived .pfx, MANDATORY under
# Azure signing (those leaf certs expire in ~3 days; untimestamped signatures die with them).
function Sign-File([string]$Path) {
    if ($NoSign) { return }
    if ($signMode -eq 'azure') {
        $signArgs = @('sign', '/fd', 'SHA256', '/dlib', $AzureDlib, '/dmdf', $azureMetadata)
        $ts = 'http://timestamp.acs.microsoft.com'
    }
    else {
        $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
        if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
        $ts = 'http://timestamp.digicert.com'
    }
    & $signtool ($signArgs + @('/tr', $ts, '/td', 'SHA256', $Path))
    if ($LASTEXITCODE -eq 0) { return }
    if ($signMode -eq 'azure') {
        throw ("timestamped sign failed for $Path ($LASTEXITCODE) - NOT retrying without a timestamp. " +
               "An Azure signing cert is valid for ~3 days; an untimestamped signature would go " +
               "untrusted within days of release.")
    }
    Write-Warning "timestamped sign failed for $Path - retrying without a timestamp"
    & $signtool ($signArgs + @($Path))
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed for $Path ($LASTEXITCODE)" }
}

# --- sign the inner exes, zip the stage (portable build), then build + sign the installer ------
foreach ($f in $required | Where-Object { $_ -like '*.exe' }) {
    Sign-File (Join-Path $stage $f)
}

$zip = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}-portable.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Write-Host "==> portable zip: $zip"

# Stage the .iss + branding next to each other under $OutDir: ISCC is a 32-bit process, and on the
# SYSTEM-profile runner WOW64 redirection breaks reads from the checkout path (see
# pack-host-installer.ps1's staging note) — everything ISCC touches must live under C:\t.
$issLocal = Join-Path $OutDir 'punktfunk-client.iss'
Copy-Item -LiteralPath $iss -Destination $issLocal -Force
$brandSrc = (Resolve-Path (Join-Path $here '..\..\..\packaging\windows\branding')).Path
$brandStage = Join-Path $OutDir 'branding'
if (Test-Path $brandStage) { Remove-Item $brandStage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $brandStage | Out-Null
Copy-Item (Join-Path $brandSrc '*.bmp') $brandStage -Force
Copy-Item (Join-Path $brandSrc 'punktfunk.ico') $brandStage -Force

$defines = @(
    "/DMyAppVersion=$Version",
    "/DArch=$Arch",
    "/DLayoutDir=$stage",
    "/DBrandingDir=$brandStage",
    "/DOutputDir=$OutDir"
)
$setup = Join-Path $OutDir "punktfunk-client-setup-${Version}_${Arch}.exe"
if ($Engine) {
    # The wizard crate builds for this arch into a target dir of its own; the packer takes the
    # self-contained runtime staged there and the CLIENT twin (asInvoker - never elevates).
    $triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $repoRoot = (Resolve-Path (Join-Path $here '..\..\..')).Path
    $wizTarget = Join-Path $OutDir 'wizard-target'
    Write-Host "==> building punktfunk-setup-client ($triple) -> $wizTarget"
    $prevTarget = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $wizTarget
    Push-Location $repoRoot
    & cargo build --release -p punktfunk-setup-win --target $triple
    $wizExit = $LASTEXITCODE
    if ($wizExit -eq 0 -and $triple -ne 'x86_64-pc-windows-msvc') {
        # The packer itself runs on the (x64) runner, whatever arch it packs for.
        & cargo build --release -p punktfunk-setup-win --bin punktfunk-setup-pack --target x86_64-pc-windows-msvc
        $wizExit = $LASTEXITCODE
    }
    Pop-Location
    if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    if ($wizExit -ne 0) { throw "punktfunk-setup-win build failed ($wizExit)" }
    $wizRel = Join-Path $wizTarget "$triple\release"
    $wizExe = Join-Path $wizRel 'punktfunk-setup-client.exe'
    $packer = Join-Path $wizTarget 'x86_64-pc-windows-msvc\release\punktfunk-setup-pack.exe'
    # D6: the payload-less uninstaller lands in {app} (the stage IS the {app} tree; the portable
    # zip above was cut before it arrived), signed before it is packed.
    $unins = Join-Path $stage 'unins000.exe'
    & $packer pack-uninstaller --exe $wizExe --runtime $wizRel --version $Version --artifact client --out $unins
    if ($LASTEXITCODE -ne 0) { throw "pack-uninstaller failed ($LASTEXITCODE)" }
    Sign-File $unins
    & $packer pack --exe $wizExe --runtime $wizRel --app $stage --version $Version --artifact client --out $setup
    if ($LASTEXITCODE -ne 0) { throw "pack failed ($LASTEXITCODE)" }
    & $packer inspect $setup
    if ($LASTEXITCODE -ne 0) { throw "inspect failed ($LASTEXITCODE)" }
}
else {
    Write-Host "==> ISCC $($defines -join ' ') $issLocal"
    & $iscc @defines $issLocal
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
}
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }
Sign-File $setup
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue
Remove-Item $azureMetadata -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if ($signMode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA."
}
elseif ($signMode -ne 'none') {
    Write-Host "==> $signMode-signed: the exe still runs everywhere; expect a SmartScreen prompt on canary builds."
}
if ($env:GITHUB_ENV) {
    "CLIENT_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "CLIENT_ZIP_PATH=$zip" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
}
