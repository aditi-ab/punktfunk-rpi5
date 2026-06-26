#requires -Version 5.1
<#
.SYNOPSIS
  Build-stage-sign-install the NEW-tree pf-vdisplay UMDF IddCx driver (packaging/windows/drivers/) for
  local dev/test on the RTX box. The wdk-sys / windows-drivers-rs analogue of the superseded
  vdisplay-driver/deploy-dev.ps1.

.DESCRIPTION
  Stages the freshly built pf_vdisplay.dll, CLEARS its FORCE_INTEGRITY PE bit (this tree's wdk-build links
  /INTEGRITYCHECK, which a self-signed cert can't satisfy — the old wdf-umdf tree didn't), signs it with
  the self-signed test cert, stamps a STRICTLY-INCREASING DriverVer into the INF, generates + signs the
  catalog, and (with -Install) pnputil-installs it.

  Build first: from packaging/windows/drivers/, in an MSVC dev shell with LIBCLANG_PATH +
  Version_Number=10.0.26100.0, run `cargo build`.

  Re-deploying needs a HIGHER DriverVer than the installed one or pnputil silently keeps the old binary —
  hence the 9.9.MMdd.HHmm scheme (also what the installer build uses; a later-minute dev redeploy wins).
  If the host service is running it holds the driver: `punktfunk-host service stop`, deploy, then start it.
.PARAMETER Install
  Also add the driver package to the store + (if absent) create the Root\pf_vdisplay devnode via nefconc.
  Needs an ELEVATED shell.
#>
[CmdletBinding()]
param(
    [string]$Stage      = 'C:\Users\Public\pfvd-stage-deploy',
    [string]$Thumbprint = '6A52984E54376C45A1C236B1A2C8A746C5AB6131',
    [string]$Nefconc    = 'C:\Users\Public\nefcon\x64\nefconc.exe',   # pinned nefcon (stage-pf-vdisplay.ps1 fetches it)
    [switch]$Install
)
$ErrorActionPreference = 'Stop'

$root  = Split-Path -Parent $MyInvocation.MyCommand.Path
$dll   = Join-Path $root 'target\x86_64-pc-windows-msvc\debug\pf_vdisplay.dll'
$inx   = Join-Path $root 'pf-vdisplay\pf_vdisplay.inx'
$clear = Join-Path $root '..\clear-force-integrity.ps1'
if (-not (Test-Path $dll)) { throw "driver not built: $dll  (cargo build in packaging/windows/drivers first)" }

$kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
function Find-Tool([string]$name, [string]$arch) {
    (Get-ChildItem "$kits\*\$arch\$name" -EA SilentlyContinue | Sort-Object FullName | Select-Object -Last 1).FullName
}
$signtool = Find-Tool 'signtool.exe' 'x64'
$stampinf = Find-Tool 'stampinf.exe' 'x64'
$inf2cat  = Find-Tool 'Inf2Cat.exe'  'x86'
foreach ($t in @($signtool, $stampinf, $inf2cat)) { if (-not $t) { throw 'a WDK tool (signtool/stampinf/Inf2Cat) was not found' } }

if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
$stagedDll = Join-Path $Stage 'pf_vdisplay.dll'
$stagedInf = Join-Path $Stage 'pf_vdisplay.inf'
$stagedCat = Join-Path $Stage 'pf_vdisplay.cat'
Copy-Item $dll $stagedDll -Force
Copy-Item $inx $stagedInf -Force   # stampinf rewrites this copy in place

# Clear FORCE_INTEGRITY BEFORE signing (the clear edits the PE, which invalidates any signature).
& $clear -Path $stagedDll | Out-Null

# DriverVer must strictly increase past whatever is installed; 9.9.MMdd.HHmm bumps every minute.
$now = Get-Date
$ver = '9.9.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm')

& $signtool sign /fd SHA256 /sha1 $Thumbprint $stagedDll | Out-Null
& $stampinf -f $stagedInf -d '*' -a 'amd64' -u '2.15.0' -v $ver | Out-Null
& $inf2cat /driver:$Stage /os:10_X64 /uselocaltime | Out-Null
& $signtool sign /fd SHA256 /sha1 $Thumbprint $stagedCat | Out-Null
Write-Host "staged + signed pf-vdisplay (new tree)  DriverVer=$ver  ->  $Stage"

if ($Install) {
    & pnputil /add-driver $stagedInf /install
    $present = Get-PnpDevice -EA SilentlyContinue |
        Where-Object { $_.InstanceId -match 'PF_VDISPLAY' -or $_.FriendlyName -match 'punktfunk Virtual Display' }
    if (-not $present) {
        if (-not (Test-Path $Nefconc)) { throw "nefconc not found: $Nefconc" }
        & $Nefconc --create-device-node --hardware-id 'root\pf_vdisplay' --class-name Display --class-guid '{4d36e968-e325-11ce-bfc1-08002be10318}' | Out-Null
        Start-Sleep 2
        & pnputil /add-driver $stagedInf /install
    }
    Write-Host "installed pf-vdisplay  DriverVer=$ver"
}
