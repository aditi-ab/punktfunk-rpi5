#requires -Version 5.1
<#
.SYNOPSIS
  Stage, sign, and (optionally) install the pf-vdisplay UMDF IddCx driver for local dev/test.

.DESCRIPTION
  Copies the freshly built pf_vdisplay.dll into the stage dir, signs it with the self-signed test cert,
  stamps a STRICTLY INCREASING DriverVer (date.time) into the INF, then generates and signs the catalog.
  With -Install it also pnputil /add-driver's the package and creates the Root\pf_vdisplay devnode.

  Why the DriverVer bump matters: `pnputil /add-driver /install` only replaces an already-installed
  driver binary when the INF DriverVer is higher than the installed one. Re-deploying without a bump
  silently keeps the OLD binary loaded — a trap that masks code changes during iteration.

  Build first: from pf-vdisplay/, in an MSVC dev shell with LIBCLANG_PATH set, run `cargo build`.

  NOTE: pf-vdisplay and SudoVDA register the SAME control-interface GUID, so only one may be active at a
  time. On the dev box, disable/remove the SudoVDA devnode before installing pf-vdisplay (and restore it
  after). This script does not touch SudoVDA.

.PARAMETER Install
  Also add the driver package to the store and create the root devnode.
#>
[CmdletBinding()]
param(
    [string]$Stage      = 'C:\Users\Public\pfvd-stage',
    [string]$Thumbprint = '6A52984E54376C45A1C236B1A2C8A746C5AB6131',
    [string]$Nefconc    = 'C:\Users\Public\virtual-display-rs\installer\files\nefconc.exe',
    [switch]$Install
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dll  = Join-Path $root 'target\x86_64-pc-windows-msvc\debug\pf_vdisplay.dll'
$inx  = Join-Path $root 'pf-vdisplay\pf_vdisplay.inx'
if (-not (Test-Path $dll)) { throw "driver not built: $dll  (run cargo build in pf-vdisplay/ first)" }

$kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
function Find-Tool([string]$name, [string]$arch) {
    (Get-ChildItem "$kits\*\$arch\$name" -EA SilentlyContinue | Sort-Object FullName | Select-Object -Last 1).FullName
}
$signtool = Find-Tool 'signtool.exe' 'x64'
$stampinf = Find-Tool 'stampinf.exe' 'x64'
$inf2cat  = Find-Tool 'Inf2Cat.exe'  'x86'
if (-not $signtool) { throw 'signtool.exe not found in the WDK' }
if (-not $stampinf) { throw 'stampinf.exe not found in the WDK' }
if (-not $inf2cat)  { throw 'Inf2Cat.exe not found in the WDK' }

New-Item -ItemType Directory -Force -Path $Stage | Out-Null
$stagedDll = Join-Path $Stage 'pf_vdisplay.dll'
$stagedInf = Join-Path $Stage 'pf_vdisplay.inf'
$stagedCat = Join-Path $Stage 'pf_vdisplay.cat'
Copy-Item $dll $stagedDll -Force
Copy-Item $inx $stagedInf -Force   # stampinf rewrites this copy in place

# DriverVer date+time — must strictly increase each deploy or pnputil keeps the old binary.
$now = Get-Date
$ver = '1.0.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm')

& $signtool sign /fd SHA256 /sha1 $Thumbprint $stagedDll | Out-Null
& $stampinf -f $stagedInf -d '*' -a 'amd64' -u '2.15.0' -v $ver | Out-Null
& $inf2cat /driver:$Stage /os:10_X64 /uselocaltime | Out-Null
& $signtool sign /fd SHA256 /sha1 $Thumbprint $stagedCat | Out-Null
Write-Host "staged + signed pf-vdisplay  DriverVer=$ver"

if ($Install) {
    & pnputil /add-driver $stagedInf /install | Out-Null
    $present = Get-PnpDevice -EA SilentlyContinue |
        Where-Object { $_.InstanceId -match 'PF_VDISPLAY' -or $_.FriendlyName -match 'punktfunk Virtual Display' }
    if (-not $present) {
        if (-not (Test-Path $Nefconc)) { throw "nefconc not found: $Nefconc" }
        & $Nefconc --create-device-node --hardware-id 'root\pf_vdisplay' --class-name Display --class-guid '{4d36e968-e325-11ce-bfc1-08002be10318}' | Out-Null
        Start-Sleep 2
        & pnputil /add-driver $stagedInf /install | Out-Null
    }
    Write-Host 'installed pf-vdisplay (root\pf_vdisplay devnode)'
}
