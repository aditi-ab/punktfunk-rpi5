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

$ciDir = $PSScriptRoot
& "$ciDir\provision-windows-wdk.ps1"
& "$ciDir\provision-windows-punktfunk-extras.ps1"
