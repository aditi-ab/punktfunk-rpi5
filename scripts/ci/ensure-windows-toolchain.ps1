# Idempotent pre-flight for punktfunk's Windows CI dependencies: WDK + cargo-wdk (driver builds),
# the x64 FFmpeg tree (host amf-qsv only), Inno Setup, and the aarch64-pc-windows-msvc rustup target. Run at the
# start of every Windows CI job so ANY runner - freshly built from unom/infra's windows-runner/
# template, rebuilt, or a new one added later - self-provisions on first real use, instead of
# needing a human to remember to dispatch a separate provisioning workflow first (and instead of
# racing which runner a manually-dispatched provisioning workflow happens to land on, when more
# than one Windows runner shares the windows-amd64 label).
#
# Each underlying script already does its own existence checks (Test-Path/Get-Command) before
# installing anything, so this is a fast no-op on a runner that's already fully provisioned - the
# only cost is on a genuinely fresh box, where it's the first job's problem to pay once.
$ErrorActionPreference = "Stop"
trap {
  Write-Host "FATAL: $_"
  Write-Host $_.ScriptStackTrace
  exit 1
}

# --- reclaim disk before building -----------------------------------------------------------------
# The windows-amd64 runner's system volume is intentionally small (100 GB) and a full Windows CI pass
# writes ~50 GB of cargo target output into C:\t (x64) / C:\t-a64 (arm64). Left to accumulate across
# runs that overflows the disk and the build dies with "no space on device" (os error 112) - exactly
# what took the Windows host build down. The runner bakes in a reclaimer + a scheduled task that keeps
# an idle box lean (unom/infra's setup-gitea-runner-base.ps1 ->
# C:\Users\Public\act-runner\clean-runner-disk.ps1); call it here too so THIS job starts with headroom
# regardless of when that task last ran. Threshold mode (no -Force): it only prunes when actually low,
# so incremental-compile caches survive when there's room. Best-effort - a cleanup hiccup must never
# fail the build.
$reclaimer = 'C:\Users\Public\act-runner\clean-runner-disk.ps1'
try {
  if (Test-Path $reclaimer) {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $reclaimer
  }
  else {
    # Fallback for a runner not yet re-baked with the infra reclaimer: prune the big target dirs when low.
    $freeGb = [math]::Round((Get-PSDrive C).Free / 1GB, 1)
    Write-Host "[ensure-toolchain] clean-runner-disk.ps1 absent; C: free ${freeGb} GB"
    if ($freeGb -lt 35) {
      foreach ($d in 'C:\t', 'C:\t-a64') {
        if (Test-Path $d) { Write-Host "  reclaiming $d"; Remove-Item $d -Recurse -Force -ErrorAction SilentlyContinue }
      }
    }
  }
}
catch { Write-Warning "disk reclaim step failed (non-fatal): $_" }

$ciDir = $PSScriptRoot
& "$ciDir\provision-windows-wdk.ps1"
& "$ciDir\provision-windows-punktfunk-extras.ps1"

# --- sccache: shared compile cache (RustFS S3 over the LAN). The Windows workflows set
# RUSTC_WRAPPER=sccache, so the binary must exist on any runner regardless of when the
# template was last re-baked. GITHUB_PATH makes it visible to every later step.
$sccacheDir = 'C:\Users\Public\sccache'
$sccacheExe = Join-Path $sccacheDir 'sccache.exe'
if (-not (Test-Path $sccacheExe)) {
  $v = '0.10.0'
  $tarball = Join-Path $env:TEMP "sccache-$v.tar.gz"
  Invoke-WebRequest -Uri "https://github.com/mozilla/sccache/releases/download/v$v/sccache-v$v-x86_64-pc-windows-msvc.tar.gz" -OutFile $tarball
  New-Item -ItemType Directory -Force -Path $sccacheDir | Out-Null
  tar -xzf $tarball -C $sccacheDir --strip-components=1
  Remove-Item $tarball -ErrorAction SilentlyContinue
}
& $sccacheExe --version
if ($env:GITHUB_PATH) { Add-Content -Path $env:GITHUB_PATH -Value $sccacheDir }

# --- zstd: what actions/cache compresses with. Without it the toolkit falls back to gzip,
# and Git's GNU tar then shells out to a `gzip` that is NOT on the runner's PATH — every
# cache SAVE died with "Child returned status 127" / "Cannot write: Broken pipe" / exit 2,
# reported only as a ::warning:: so runs stayed green while nothing was ever cached
# (measured 2026-07-30 on windows-host). Own directory on purpose: putting Git's usr\bin on
# PATH would shadow Windows' find/sort/echo and break unrelated steps.
$zstdDir = 'C:\Users\Public\zstd'
$zstdExe = Join-Path $zstdDir 'zstd.exe'
if (-not (Test-Path $zstdExe)) {
  try {
    $v = '1.5.6'
    $zip = Join-Path $env:TEMP "zstd-$v.zip"
    Invoke-WebRequest -Uri "https://github.com/facebook/zstd/releases/download/v$v/zstd-v$v-win64.zip" -OutFile $zip
    $tmp = Join-Path $env:TEMP 'zstd-extract'
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $zstdDir | Out-Null
    Get-ChildItem -Path $tmp -Recurse -Filter 'zstd.exe' | Select-Object -First 1 |
      ForEach-Object { Copy-Item $_.FullName $zstdExe -Force }
    $gitGzip = 'C:\Program Files\Git\usr\bin\gzip.exe'
    if ((Test-Path $gitGzip) -and -not (Test-Path (Join-Path $zstdDir 'gzip.exe'))) {
      Copy-Item $gitGzip (Join-Path $zstdDir 'gzip.exe') -Force
    }
    Remove-Item $zip, $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
  catch { Write-Warning "zstd install failed (cache saves will fall back to gzip): $_" }
}
if (Test-Path $zstdExe) {
  & $zstdExe --version
  if ($env:GITHUB_PATH) { Add-Content -Path $env:GITHUB_PATH -Value $zstdDir }
}
