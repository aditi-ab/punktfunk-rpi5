<#
.SYNOPSIS
  Generate the NVENC import library (nvencodeapi.lib) into -OutDir, so the host links with
  `--features nvenc` on a box that has no NVIDIA Video Codec SDK and no GPU.

.DESCRIPTION
  The host links against nvencodeapi.lib (crates/punktfunk-host/build.rs). That import lib is just
  a link-time stub for two exports of nvEncodeAPI64.dll (the real DLL ships with the NVIDIA driver
  and resolves at runtime). We synthesise it from nvenc.def:

    1. llvm-dlltool  — preferred; LLVM is on the CI runner PATH (C:\Program Files\LLVM\bin) and this
                       works without a Visual Studio developer shell.
    2. MSVC lib.exe  — fallback; located via vswhere (no vcvars needed).

  Point PUNKTFUNK_NVENC_LIB_DIR at -OutDir before `cargo build --features nvenc`.

.EXAMPLE
  pwsh -File gen-nvenc-importlib.ps1 -OutDir C:\t\nvenc
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$DefPath = (Join-Path $PSScriptRoot 'nvenc.def')
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false   # check $LASTEXITCODE ourselves (pwsh 7.4 safe)

if (-not (Test-Path $DefPath)) { throw "module-definition file not found: $DefPath" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$out = Join-Path $OutDir 'nvencodeapi.lib'

# 1) llvm-dlltool (preferred) ------------------------------------------------------------------
$dlltool = Get-Command llvm-dlltool -ErrorAction SilentlyContinue
if ($dlltool) {
    Write-Host "==> llvm-dlltool -> $out"
    & $dlltool.Source -m i386:x86-64 -d $DefPath -D nvEncodeAPI64.dll -l $out
    if ($LASTEXITCODE -ne 0) { throw "llvm-dlltool failed ($LASTEXITCODE)" }
    Write-Host "    ok ($((Get-Item $out).Length) bytes)"
    return
}

# 2) MSVC lib.exe via vswhere (fallback) -------------------------------------------------------
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path $vswhere) {
    $lib = & $vswhere -latest -prerelease -products * -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\lib.exe' |
        Select-Object -First 1
    if ($lib -and (Test-Path $lib)) {
        Write-Host "==> lib.exe -> $out"
        & $lib "/def:$DefPath" /machine:x64 "/out:$out"
        if ($LASTEXITCODE -ne 0) { throw "lib.exe failed ($LASTEXITCODE)" }
        Write-Host "    ok ($((Get-Item $out).Length) bytes)"
        return
    }
}

throw "neither llvm-dlltool (LLVM bin on PATH) nor MSVC lib.exe (via vswhere) was found to build $out"
