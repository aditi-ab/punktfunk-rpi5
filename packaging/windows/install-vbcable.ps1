<#
.SYNOPSIS
  Silently install the bundled VB-Audio Virtual Cable (the punktfunk virtual microphone) on the host.

.DESCRIPTION
  punktfunk pipes the streaming client's microphone into a virtual audio cable's render endpoint; the
  cable's capture endpoint ("CABLE Output") then surfaces as a host microphone that games/apps record
  from (see crates/punktfunk-host/src/audio/windows/wasapi_mic.rs). On a headless host there is no real
  audio output, so a virtual cable is required. We bundle the OFFICIAL base VB-CABLE package (VB-Audio,
  https://vb-cable.com) and install it unattended:

    1. If a "CABLE Input"/"CABLE Output" endpoint already exists, do nothing (idempotent).
    2. Pre-seed VB-Audio's Authenticode signing certificate (read from the bundled signed driver) into
       LocalMachine\TrustedPublisher, so the kernel-driver-publisher prompt is suppressed and the
       install is fully silent (required for the SYSTEM/Session-0 service install).
    3. Run the official silent installer: VBCABLE_Setup_x64.exe -i -h  (arm64: the same exe name in the
       arm64 package; x86 falls back to VBCABLE_Setup.exe).
    4. Wait briefly for the audio subsystem to register the new endpoint.

  VB-CABLE is donationware by VB-Audio Software, redistributed here under VB-Audio's bundling grant
  (https://vb-audio.com/Services/licensing.htm); see {app}\licenses\VB-CABLE-NOTICE.txt. Only the base
  single cable is bundled (A+B / C+D are not redistributable).

  Best-effort: any failure is logged and returns a non-zero exit, but the caller (the installer) treats
  it as non-fatal - the host still runs (mic passthrough then needs a manually-installed cable, and the
  host falls back to auto-installing the Steam Streaming pair).

.PARAMETER Dir
  The staged VB-CABLE package directory (contains VBCABLE_Setup_x64.exe + the signed driver files).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Dir
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Test-CablePresent {
    # An active render OR capture endpoint named "CABLE ..." means VB-CABLE is already installed.
    $eps = Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue |
        Where-Object { $_.Status -eq 'OK' -and $_.FriendlyName -match 'CABLE (Input|Output|In)' }
    return [bool]$eps
}

if (Test-CablePresent) {
    Write-Host 'VB-CABLE already installed (CABLE endpoint present) - skipping.'
    exit 0
}

if (-not (Test-Path -LiteralPath $Dir)) { throw "VB-CABLE package dir not found: $Dir" }

# Pick the silent installer for this architecture. The x64 package ships both; arm64 ships an arm64
# VBCABLE_Setup_x64.exe (VB-Audio's naming); fall back to the 32-bit setup if that's all that's staged.
$setup = $null
foreach ($name in @('VBCABLE_Setup_x64.exe', 'VBCABLE_Setup.exe')) {
    $p = Join-Path $Dir $name
    if (Test-Path -LiteralPath $p) { $setup = $p; break }
}
if (-not $setup) { throw "no VBCABLE_Setup*.exe under $Dir" }
Write-Host "VB-CABLE silent installer: $setup"

# --- pre-seed VB-Audio's signing cert into LocalMachine\TrustedPublisher (unattended driver install) ---
# Read the Authenticode signer from a bundled signed file (prefer a driver .sys/.cat; fall back to the
# setup exe). Importing it into TrustedPublisher makes Windows install the signed driver with no prompt.
try {
    $signed = Get-ChildItem -LiteralPath $Dir -Recurse -Include '*.sys', '*.cat', '*.exe' -ErrorAction SilentlyContinue |
        ForEach-Object { Get-AuthenticodeSignature -LiteralPath $_.FullName -ErrorAction SilentlyContinue } |
        Where-Object { $_.Status -eq 'Valid' -and $_.SignerCertificate } |
        Select-Object -First 1
    if ($signed -and $signed.SignerCertificate) {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store('TrustedPublisher', 'LocalMachine')
        $store.Open('ReadWrite')
        $store.Add($signed.SignerCertificate)
        $store.Close()
        Write-Host "seeded VB-Audio cert into LocalMachine\TrustedPublisher (subject=$($signed.SignerCertificate.Subject))"
    }
    else {
        Write-Warning 'no valid Authenticode signer found in the VB-CABLE package - the driver-publisher prompt may appear (install may stall under SYSTEM)'
    }
}
catch {
    Write-Warning "could not pre-seed the VB-Audio cert: $($_.Exception.Message)"
}

# --- run the official silent install: -i (install) -h (hidden) -----------------------------------
# VB-Audio documents these switches; the process returns before the endpoint is fully registered.
$proc = Start-Process -FilePath $setup -ArgumentList '-i', '-h' -Wait -PassThru -WindowStyle Hidden
Write-Host "VBCABLE setup exit code: $($proc.ExitCode)"

# Give the audio subsystem time to enumerate the new endpoint, then verify.
for ($i = 0; $i -lt 10; $i++) {
    Start-Sleep -Seconds 1
    if (Test-CablePresent) { Write-Host 'VB-CABLE installed - CABLE endpoint present.'; exit 0 }
}
Write-Warning 'VB-CABLE setup ran but no CABLE endpoint appeared yet (a reboot may be required).'
# Non-fatal: the device often appears after the next session/reboot; the host retries mic open with backoff.
exit 0
