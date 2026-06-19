# Provision this Windows box as the Gitea Actions runner for the Windows client + host CI/packaging.
# The Windows analogue of scripts/ci/setup-macos-runner.sh. Idempotent — safe to re-run. Run
# ELEVATED (admin) on the box, e.g. over SSH:
#
#   ssh "<box>" 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\path\setup-windows-runner.ps1 -Token <registration token>'
#
# Installs: the act_runner (gitea-runner) binary in **host mode** (jobs run directly on Windows,
# no containers — MSVC/WinUI builds need the host toolchain), Node 20 via the box's nvm4w (JS
# actions like actions/checkout run via node on PATH), and a SYSTEM scheduled task that keeps the
# daemon alive across reboots with nobody logged in. Registration happens once (.runner file); the
# token is NOT persisted.
#
# Get a **GLOBAL** registration token: Gitea **Site Administration -> Actions -> Runners** (the
# registration token shown there). The runner MUST be global/instance-scoped to pick up org-repo
# jobs like unom/punktfunk — an org- or repo-scoped token leaves it registered but unmatchable
# ("no fitting runner for windows-amd64", even though the runner shows idle). Mirrors the Linux
# runner's scope.
#
# Env/param knobs: -Instance (default https://git.unom.io), -Token (GITEA_RUNNER_TOKEN; required
# for first registration), -RunnerName (default COMPUTERNAME), -Labels (default windows-amd64:host
# — match the Windows job's runs-on), -Version (act_runner, default 1.0.8).
#
# The daemon's env wrapper hard-codes this box's MSVC build paths (cargo/rustup, NASM, CMake, LLVM,
# FFmpeg, the ASCII CARGO_HOME that SDL3's PCH needs) so the Windows workflow inherits a working
# toolchain without re-deriving dev-box specifics. Per-checkout vars (CARGO_WORKSPACE_DIR for the
# windows-reactor build.rs) are set by the workflow, not here.
param(
    [string]$Instance = $(if ($env:GITEA_INSTANCE) { $env:GITEA_INSTANCE } else { "https://git.unom.io" }),
    [string]$Version = $(if ($env:ACT_RUNNER_VERSION) { $env:ACT_RUNNER_VERSION } else { "1.0.8" }),
    [string]$RunnerName = $(if ($env:RUNNER_NAME) { $env:RUNNER_NAME } else { $env:COMPUTERNAME }),
    [string]$Labels = $(if ($env:RUNNER_LABELS) { $env:RUNNER_LABELS } else { "windows-amd64:host" }),
    [string]$Token = $env:GITEA_RUNNER_TOKEN
)
$ErrorActionPreference = "Stop"
$RunnerHome = "C:\Users\Public\act-runner"
$Exe = "$RunnerHome\act_runner.exe"
New-Item -ItemType Directory -Force -Path $RunnerHome | Out-Null

# --- act_runner binary (gitea-runner; CLI surface unchanged from act_runner) ---
$need = $true
if (Test-Path $Exe) { try { $need = -not ((& $Exe --version 2>$null) -match [regex]::Escape($Version)) } catch { } }
if ($need) {
    $url = "https://dl.gitea.com/gitea-runner/$Version/gitea-runner-$Version-windows-amd64.exe"
    Write-Host "==> downloading act_runner $Version"
    Invoke-WebRequest -Uri $url -OutFile "$Exe.tmp" -UseBasicParsing
    Move-Item -Force "$Exe.tmp" $Exe
}
& $Exe --version

# --- Node 20 (actions/checkout@v4 demands node20) via the box's nvm4w ---
if (Get-Command nvm -ErrorAction SilentlyContinue) {
    if (-not ((node --version 2>$null) -match "^v20")) {
        nvm install 20.18.0 | Out-Null
        nvm use 20.18.0 | Out-Null
    }
}
Write-Host "node $(node --version)"

# --- config + host-mode labels (empty the docker defaults so .runner's labels rule) ---
Push-Location $RunnerHome
if (-not (Test-Path config.yaml)) { & $Exe generate-config | Set-Content -Encoding ASCII config.yaml }
(Get-Content config.yaml) |
    Where-Object { $_ -notmatch "docker.gitea.com/runner-images" } |
    ForEach-Object { $_ -replace '^(\s*)labels:\s*$', '${1}labels: []' } |
    Set-Content -Encoding ASCII config.yaml
Pop-Location

# --- one-time registration (from $RunnerHome: register writes .runner to the CWD) ---
if (-not (Test-Path "$RunnerHome\.runner")) {
    if (-not $Token) {
        Write-Warning "Not registered yet. Re-run with -Token <GLOBAL registration token>."
        Write-Host "  (Gitea: Site Administration -> Actions -> Runners -> registration token; must be GLOBAL scope)"
        exit 1
    }
    Push-Location $RunnerHome
    & $Exe register --no-interactive --instance $Instance --token $Token --name $RunnerName --labels $Labels
    Pop-Location
}

# rustup toolchains under an ASCII path so nothing in the daemon env carries the non-ASCII
# username (the same hazard that breaks SDL3's PCH; here it also keeps this script ASCII-clean).
if (-not (Test-Path "C:\Users\Public\.rustup\settings.toml")) {
    Write-Host "==> copying rustup toolchains to an ASCII path"
    robocopy "$env:USERPROFILE\.rustup" "C:\Users\Public\.rustup" /E /NFL /NDL /NJH /NJS /MT:16 | Out-Null
}

# --- ARM64 cross-compile support (windows.yml / windows-msix.yml build aarch64-pc-windows-msvc off
# this x64 box; no ARM64 runner needed). Two pieces beyond the x64 toolchain:
#   1. the rustup std for the target;
#   2. an ARM64 FFmpeg import-lib/DLL tree at C:\Users\Public\ffmpeg-arm64 (the workflow matrix
#      points FFMPEG_DIR here for the aarch64 leg; the x64 tree stays at C:\Users\Public\ffmpeg).
# The x64 MSVC toolset already ships the ARM64 cross compiler — if
# VC\Tools\MSVC\<ver>\bin\Hostx64\arm64\cl.exe is missing, add the VS "MSVC v143+ ARM64/ARM64EC
# build tools" + "C++ ARM64 build tools" workload components (the cc/cmake crates need it to
# cross-build SDL3 + libopus).
$env:RUSTUP_HOME = "C:\Users\Public\.rustup"
$env:CARGO_HOME = "C:\Users\Public\.cargo"
$rustup = (Get-Command rustup -ErrorAction SilentlyContinue).Source
if (-not $rustup) { $rustup = "C:\Users\Public\.cargo\bin\rustup.exe" }
if (Test-Path $rustup) {
    Write-Host "==> rustup target add aarch64-pc-windows-msvc"
    & $rustup target add aarch64-pc-windows-msvc
} else { Write-Warning "rustup not found - install rustup then re-run (needed for the aarch64 target)." }

$ffArm = "C:\Users\Public\ffmpeg-arm64"
if (-not (Test-Path (Join-Path $ffArm 'lib\avcodec.lib'))) {
    # BtbN winarm64 shared, FFmpeg 7.x (avcodec-61) to match the x64 tree's ABI. MSVC-linkable .lib
    # import libs + headers + bin\*.dll — exactly what ffmpeg-sys-next + pack-msix.ps1 consume.
    Write-Host "==> fetching ARM64 FFmpeg (BtbN winarm64 shared)"
    $ffUrl = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n7.1-latest-winarm64-gpl-shared-7.1.zip'
    $ffZip = "C:\Users\Public\ffmpeg-arm64.zip"
    $ffTmp = "C:\Users\Public\ffmpeg-arm64-extract"
    Invoke-WebRequest -Uri $ffUrl -OutFile $ffZip -UseBasicParsing
    if (Test-Path $ffTmp) { Remove-Item -Recurse -Force $ffTmp }
    Expand-Archive -Path $ffZip -DestinationPath $ffTmp -Force   # BtbN zips have one top-level folder
    $inner = Get-ChildItem $ffTmp -Directory | Select-Object -First 1
    if (Test-Path $ffArm) { Remove-Item -Recurse -Force $ffArm }
    Move-Item -Path $inner.FullName -Destination $ffArm
    Remove-Item -Force $ffZip; Remove-Item -Recurse -Force $ffTmp -ErrorAction SilentlyContinue
}

# Inno Setup (ISCC.exe) for the host installer build (windows-host.yml). pack-host-installer.ps1
# locates it at its fixed Program Files path, so it need not be on PATH — just present.
if (-not (Test-Path "C:\Program Files (x86)\Inno Setup 6\ISCC.exe")) {
    if (Get-Command choco -ErrorAction SilentlyContinue) {
        Write-Host "==> installing Inno Setup (ISCC)"
        choco install innosetup -y --no-progress
    }
    else { Write-Warning "Inno Setup not found and choco unavailable - install it for windows-host.yml." }
}

# --- daemon env wrapper (the box's MSVC/WinUI/FFmpeg toolchain) ---
$wrapper = "$RunnerHome\run-runner.ps1"
@'
$env:NO_COLOR = "1"
$env:CARGO_HOME = "C:\Users\Public\.cargo"
$env:RUSTUP_HOME = "C:\Users\Public\.rustup"
$env:CMAKE_POLICY_VERSION_MINIMUM = "3.5"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
$env:FFMPEG_DIR = "C:\Users\Public\ffmpeg"
$env:PATH = "C:\Program Files\PowerShell\7;C:\Users\Public\.cargo\bin;C:\nvm4w\nodejs;C:\Program Files\NASM;C:\Program Files\CMake\bin;C:\Program Files\LLVM\bin;C:\Users\Public\ffmpeg\bin;" + $env:PATH
Set-Location "C:\Users\Public\act-runner"
# cmd-level redirect (>>, 2>&1) keeps the daemon's native stderr out of PowerShell's error stream.
& cmd /c "act_runner.exe daemon --config config.yaml >> runner.log 2>&1"
'@ | Set-Content -Encoding UTF8 $wrapper

# --- SYSTEM scheduled task: keep the daemon alive across reboots, no login needed ---
$taskName = "gitea-act-runner"
if (schtasks /Query /TN $taskName 2>$null) {
    schtasks /End /TN $taskName 2>$null | Out-Null
    schtasks /Delete /TN $taskName /F | Out-Null
}
$action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$wrapper`""
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
Start-Sleep -Seconds 4

Write-Host "==> runner '$RunnerName' labels=$Labels instance=$Instance"
$p = Get-Process act_runner -ErrorAction SilentlyContinue
if ($p) { Write-Host "daemon running (pid $($p.Id), session $($p.SessionId))" }
else { Write-Warning "daemon not running yet - check the gitea-act-runner task" }
