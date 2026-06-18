<#
.SYNOPSIS
  Download + verify the SudoVDA virtual-display driver and the nefcon device tool, and stage the
  files the installer bundles into -OutDir.

.DESCRIPTION
  The host uses SudoVDA (the SudoMaker Indirect Display Driver the Apollo Sunshine-fork ships) for
  client-native-resolution virtual outputs (crates/punktfunk-host/src/vdisplay/sudovda.rs). The
  driver isn't in this repo; CI fetches a PINNED release at pack time so the installer can carry it.
  nefcon (nefarius/nefcon) provides `nefconc.exe`, used to create the root-enumerated device node
  (pnputil cannot create a software/root device node from nothing).

  Output (consumed by punktfunk-host.iss): -OutDir gets the driver payload (*.inf/*.cat/*.dll/*.sys
  + the signing *.cer) and nefconc.exe, flattened. install-sudovda.ps1 is copied in by the packer.

  PINNING: the URLs + SHA-256s below are the single source of truth for what ships. If a Sha256 is
  left empty, the script downloads, prints the computed hash, and continues with a loud warning —
  fill it in to lock the release. Override any field with the params for a one-off build.

.EXAMPLE
  pwsh -File fetch-sudovda.ps1 -OutDir C:\t\out\stage
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,

    # --- PINNED SudoVDA release (https://github.com/SudoMaker/SudoVDA/releases) ---------------
    # Baseline validated by the host backend: SudoVDA 0.2.1 (sudovda.rs header). Confirm the latest
    # asset name/URL at impl time; fill Sha256 to lock it.
    [string]$SudoVdaTag = 'v0.2.1',
    [string]$SudoVdaUrl = 'https://github.com/SudoMaker/SudoVDA/releases/download/v0.2.1/SudoVDA-v0.2.1.zip',
    [string]$SudoVdaSha256 = '',

    # --- PINNED nefcon release (https://github.com/nefarius/nefcon/releases) ------------------
    [string]$NefconUrl = 'https://github.com/nefarius/nefcon/releases/download/v1.0.2.0/nefconw.zip',
    [string]$NefconSha256 = ''
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Get-Verified([string]$Url, [string]$ExpectedSha256, [string]$Dest) {
    Write-Host "==> downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    $got = (Get-FileHash -Algorithm SHA256 -Path $Dest).Hash.ToLowerInvariant()
    if ($ExpectedSha256) {
        if ($got -ne $ExpectedSha256.ToLowerInvariant()) {
            throw "SHA-256 mismatch for $Url`n  expected $ExpectedSha256`n  got      $got"
        }
        Write-Host "    sha256 ok ($got)"
    }
    else {
        Write-Warning "no pinned SHA-256 for $Url — computed $got (PIN THIS in fetch-sudovda.ps1)"
    }
}

# fresh staging dir
if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$work = Join-Path $OutDir '_work'
New-Item -ItemType Directory -Force -Path $work | Out-Null

# --- SudoVDA driver payload -------------------------------------------------------------------
$sudoZip = Join-Path $work 'sudovda.zip'
Get-Verified -Url $SudoVdaUrl -ExpectedSha256 $SudoVdaSha256 -Dest $sudoZip
$sudoEx = Join-Path $work 'sudovda'
Expand-Archive -Path $sudoZip -DestinationPath $sudoEx -Force

# Flatten the driver files we need into -OutDir. SudoVDA is a user-mode IDD (a .dll registered by an
# .inf); pull the .inf/.cat/.dll plus a .sys if present and the signing .cer.
$driverFiles = Get-ChildItem -Path $sudoEx -Recurse -Include *.inf, *.cat, *.dll, *.sys, *.cer
if (-not ($driverFiles | Where-Object Extension -eq '.inf')) {
    throw "no .inf found in the SudoVDA archive ($SudoVdaUrl) — wrong asset?"
}
$driverFiles | ForEach-Object { Copy-Item $_.FullName (Join-Path $OutDir $_.Name) -Force }

# --- nefcon (nefconc.exe) ---------------------------------------------------------------------
$nefZip = Join-Path $work 'nefcon.zip'
Get-Verified -Url $NefconUrl -ExpectedSha256 $NefconSha256 -Dest $nefZip
$nefEx = Join-Path $work 'nefcon'
Expand-Archive -Path $nefZip -DestinationPath $nefEx -Force
# Prefer the x64 console build.
$nefc = Get-ChildItem -Path $nefEx -Recurse -Filter 'nefconc.exe' |
    Where-Object { $_.FullName -match '(?i)(x64|amd64)' } |
    Select-Object -First 1
if (-not $nefc) { $nefc = Get-ChildItem -Path $nefEx -Recurse -Filter 'nefconc.exe' | Select-Object -First 1 }
if (-not $nefc) { throw "nefconc.exe not found in $NefconUrl" }
Copy-Item $nefc.FullName (Join-Path $OutDir 'nefconc.exe') -Force

Remove-Item -Recurse -Force $work
Write-Host "==> staged SudoVDA + nefcon in $OutDir :"
Get-ChildItem $OutDir -File | ForEach-Object { "    $($_.Name)" }
