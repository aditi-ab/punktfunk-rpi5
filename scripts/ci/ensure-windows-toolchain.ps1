# Idempotent pre-flight for punktfunk's Windows CI dependencies: WDK + cargo-wdk (driver builds),
# FFmpeg x64/ARM64 trees, Inno Setup, and the aarch64-pc-windows-msvc rustup target. Run at the
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
