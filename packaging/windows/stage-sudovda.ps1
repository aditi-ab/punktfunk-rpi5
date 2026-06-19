<#
.SYNOPSIS
  Stage the driver bundle the installer ships into -OutDir: the VENDORED SudoVDA driver + the
  fetched nefcon device tool.

.DESCRIPTION
  SudoVDA has no upstream release (its repo is a source-only VS solution; Apollo embeds the driver in
  its single installer), so the prebuilt, signed driver is VENDORED in this repo under
  packaging/windows/sudovda/ (MIT/CC0; SudoVDA v1.10.9.289, signer CN=sudovda@su.mk, Class=Display,
  HWID Root\SudoMaker\SudoVDA). nefcon DOES publish a pinned release, so we fetch + SHA-256-verify it
  (it provides nefconc.exe, used to create the root-enumerated device node — pnputil can't).

  Output (consumed by punktfunk-host.iss): -OutDir gets SudoVDA.inf/.cat/.dll + sudovda.cer and
  nefconc.exe (x64). pack-host-installer.ps1 also drops install-sudovda.ps1 in.

.EXAMPLE
  pwsh -File stage-sudovda.ps1 -OutDir C:\t\out\stage
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$VendorDir = (Join-Path $PSScriptRoot 'sudovda'),
    # PINNED nefcon release (https://github.com/nefarius/nefcon/releases). MIT-licensed.
    [string]$NefconUrl = 'https://github.com/nefarius/nefcon/releases/download/v1.17.40/nefcon_v1.17.40.zip',
    [string]$NefconSha256 = '812bae7ed7dfb7d6d2284bc7de2f8ccebc92ed2a0b1ae893c53b337096e50c1a'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# --- vendored SudoVDA driver ------------------------------------------------------------------
$inf = Get-ChildItem -Path $VendorDir -Filter *.inf -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $inf) { throw "no vendored SudoVDA .inf under $VendorDir — see packaging/windows/README.md" }
Copy-Item (Join-Path $VendorDir '*') $OutDir -Force
Write-Host "==> vendored SudoVDA staged from $VendorDir"

# --- nefcon (fetched + verified) --------------------------------------------------------------
$work = Join-Path ([IO.Path]::GetTempPath()) ('nefcon-' + [IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $zip = Join-Path $work 'nefcon.zip'
    Write-Host "==> downloading $NefconUrl"
    Invoke-WebRequest -Uri $NefconUrl -OutFile $zip -UseBasicParsing
    $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($NefconSha256) {
        if ($got -ne $NefconSha256.ToLowerInvariant()) {
            throw "nefcon SHA-256 mismatch`n  expected $NefconSha256`n  got      $got"
        }
        Write-Host "    sha256 ok ($got)"
    }
    else { Write-Warning "no pinned nefcon SHA-256 — computed $got (PIN THIS in stage-sudovda.ps1)" }
    Expand-Archive -Path $zip -DestinationPath $work -Force
    $nefc = Get-ChildItem -Path $work -Recurse -Filter 'nefconc.exe' |
        Where-Object { $_.FullName -match '(?i)\\x64\\' } | Select-Object -First 1
    if (-not $nefc) { $nefc = Get-ChildItem -Path $work -Recurse -Filter 'nefconc.exe' | Select-Object -First 1 }
    if (-not $nefc) { throw "nefconc.exe not found in $NefconUrl" }
    Copy-Item $nefc.FullName (Join-Path $OutDir 'nefconc.exe') -Force
}
finally { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }

Write-Host "==> staged driver bundle in $OutDir :"
Get-ChildItem $OutDir -File | ForEach-Object { "    $($_.Name)" }
