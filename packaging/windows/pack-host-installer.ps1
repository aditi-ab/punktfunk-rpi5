<#
.SYNOPSIS
  Build + sign the punktfunk Windows host installer (Inno Setup setup.exe).

.DESCRIPTION
  From a release `cargo build -p punktfunk-host --features nvenc` output (the exe), this:
    1. resolves a code-signing cert (supplied stable .pfx from CI secrets OR an ephemeral self-signed
       CN=unom — same scheme as the client's pack-msix.ps1) and exports the public .cer,
    2. signs the inner punktfunk-host.exe,
    3. fetches + stages the SudoVDA driver bundle (unless -NoDriver),
    4. runs ISCC to build punktfunk-host-setup-<ver>.exe,
    5. signs the setup.exe (timestamp best-effort),
    6. emits HOST_SETUP_PATH / HOST_CER_PATH to GITHUB_ENV for the publish step.

  Idempotent; safe to re-run. Run on the Windows runner / dev box (MSVC + Windows SDK + Inno Setup).

.EXAMPLE
  pwsh -File pack-host-installer.ps1 -Version 0.2.137 -TargetDir C:\t\release -OutDir C:\t\out
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                 # e.g. 0.2.137 or 1.4.0 (free-form)
    [Parameter(Mandatory = $true)][string]$TargetDir,               # cargo --release dir (has punktfunk-host.exe)
    [string]$OutDir = (Join-Path $TargetDir 'installer'),
    [string]$Publisher = 'CN=unom',
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                    # reuse the client's signing secret
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    [switch]$NoDriver,                                              # build without the bundled SudoVDA driver
    [switch]$NoSign                                                 # skip signing (local debug)
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Keep the traditional "check $LASTEXITCODE myself" model: don't let pwsh 7.4 turn a non-zero native
# exit into a terminating error (it would bypass Sign-File's timestamp-then-retry fallback below).
$PSNativeCommandUseErrorActionPreference = $false

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$iss = Join-Path $here 'punktfunk-host.iss'
$exe = Join-Path $TargetDir 'punktfunk-host.exe'
if (-not (Test-Path $exe)) { throw "missing build artifact 'punktfunk-host.exe' in $TargetDir (did 'cargo build --release -p punktfunk-host --features nvenc' run?)" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# --- locate ISCC (Inno Setup) + signtool (Windows SDK) ---------------------------------------
function Find-Iscc {
    foreach ($p in @(
            'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
            'C:\Program Files\Inno Setup 6\ISCC.exe')) {
        if (Test-Path $p) { return $p }
    }
    $c = Get-Command iscc -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    throw "ISCC.exe (Inno Setup 6, any 6.x) not found — install it (choco install innosetup -y)."
}
function Find-SdkTool([string]$name) {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $hit = Get-ChildItem -Path $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(10\.0\.\d+\.\d+)\\x64\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(10\.0\.\d+\.\d+)\\x64\\').Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$name not found under $root — install the Windows 10/11 SDK." }
    $hit.FullName
}
$iscc = Find-Iscc
Write-Host "ISCC: $iscc"

# --- signing cert (supplied stable pfx OR ephemeral self-signed) -----------------------------
$pfxPath = Join-Path $OutDir 'signing.pfx'
$cerPath = Join-Path $OutDir "punktfunk-host-windows_${Version}.cer"
$signtool = $null
if (-not $NoSign) {
    $signtool = Find-SdkTool 'signtool.exe'
    Write-Host "signtool: $signtool"
    if ($PfxBase64) {
        Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
        [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
    }
    else {
        Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
        if (-not $PfxPassword) { $PfxPassword = 'punktfunk' }
        $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName 'punktfunk host (self-signed)' `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        $sec = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $pfxPath -Password $sec | Out-Null
        Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
    }
    # Always export the public .cer. For a self-signed cert it's the file users import once
    # (LocalMachine\TrustedPublisher) so SmartScreen/UAC trusts the signed setup.exe; for a real CA
    # cert it's a harmless extra.
    $pwsec = if ($PfxPassword) { ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText } else { $null }
    $pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $pfxPath -Password $pwsec } else { Get-PfxCertificate -FilePath $pfxPath }
    Export-Certificate -Cert $pubCert -FilePath $cerPath | Out-Null
    Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
}

function Sign-File([string]$Path) {
    if ($NoSign) { return }
    $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
    if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
    & $signtool ($signArgs + @('/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', $Path))
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "timestamped sign failed for $Path — retrying without a timestamp"
        & $signtool ($signArgs + @($Path))
        if ($LASTEXITCODE -ne 0) { throw "signtool sign failed for $Path ($LASTEXITCODE)" }
    }
}

# --- sign the inner exe before it's packed ----------------------------------------------------
Sign-File $exe

# --- resolve + validate the installer's source files (absolute paths -> ISCC /D defines) -------
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path
$hostEnv = Join-Path $repoRoot 'scripts\windows\host.env.example'
$readme = Join-Path $here 'README.md'
foreach ($p in @($exe, $hostEnv, $readme)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "installer source file missing: $p" }
    Write-Host "  source ok: $p"
}
$defines = @(
    "/DMyAppVersion=$Version",
    "/DBinDir=$TargetDir",
    "/DOutputDir=$OutDir",
    "/DHostEnv=$hostEnv",
    "/DReadme=$readme"
)

# --- stage the SudoVDA driver bundle ----------------------------------------------------------
if (-not $NoDriver) {
    $stage = Join-Path $OutDir 'stage'
    & (Join-Path $here 'stage-sudovda.ps1') -OutDir $stage
    Copy-Item (Join-Path $here 'install-sudovda.ps1') (Join-Path $stage 'install-sudovda.ps1') -Force
    $defines += "/DStageDir=$stage"
}
else { Write-Host "-NoDriver: building installer WITHOUT the bundled SudoVDA driver" }

# --- build the installer ----------------------------------------------------------------------
Write-Host "==> ISCC $($defines -join ' ') $iss"
& $iscc @defines $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

$setup = Join-Path $OutDir "punktfunk-host-setup-$Version.exe"
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }

# --- sign the setup.exe + clean up ------------------------------------------------------------
Sign-File $setup
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if (-not $NoSign) {
    Write-Host "==> trust the cert once per machine (self-signed builds), then the signed setup.exe is trusted:"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPublisher"
}
if ($env:GITHUB_ENV) {
    "HOST_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if (-not $NoSign) { "HOST_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
