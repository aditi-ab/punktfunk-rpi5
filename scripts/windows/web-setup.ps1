<#
  Provision the punktfunk web console after the host installer has laid down its payload
  ({app}\web\.output, {app}\node\node.exe, {app}\web\web-run.cmd). Invoked elevated from the
  installer's [Run] section; idempotent (safe to re-run on upgrade).

    1. Sets the console login password file %ProgramData%\punktfunk\web-password
       (PUNKTFUNK_UI_PASSWORD=...), ACL'd to Administrators + SYSTEM only:
         - if -PasswordFile points at a non-empty temp file (a FRESH install collected one on the
           wizard page), use that;
         - else if the file already exists (UPGRADE), keep it untouched;
         - else generate a random one (fallback, so the console never boots auth-misconfigured).
    2. Registers the PunktfunkWeb scheduled task: at boot, as SYSTEM/Highest, restart-on-failure,
       no execution time limit (a long-running server), running {app}\web\web-run.cmd.
    3. Opens inbound TCP 3000 (the console port) on all profiles.
    4. Waits briefly for the host's mgmt token, then starts the task.

  The mgmt bearer token is NOT managed here — the host owns %ProgramData%\punktfunk\mgmt-token
  (crates/punktfunk-host/src/mgmt_token.rs writes it on `serve`); web-run.cmd sources it.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$AppDir,   # the installer's {app}
    [string]$PasswordFile                            # temp file with the chosen password (fresh install)
)
$ErrorActionPreference = 'Stop'

$TaskName = 'PunktfunkWeb'
$dataDir = Join-Path $env:ProgramData 'punktfunk'
$pwFile = Join-Path $dataDir 'web-password'
$tokenFile = Join-Path $dataDir 'mgmt-token'
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null

function New-RandomPassword {
    # URL/shell-safe (no /+=) so it's a clean env-file value and cmd-token, like scripts/web-init.sh.
    $bytes = New-Object byte[] 24
    ([System.Security.Cryptography.RandomNumberGenerator]::Create()).GetBytes($bytes)
    $s = [Convert]::ToBase64String($bytes) -replace '[/+=]', ''
    return $s.Substring(0, [Math]::Min(20, $s.Length))
}

# --- 1. login password -----------------------------------------------------------------------
$password = $null
if ($PasswordFile -and (Test-Path -LiteralPath $PasswordFile)) {
    $password = (Get-Content -LiteralPath $PasswordFile -Raw).Trim()
}
if (-not $password) {
    if (Test-Path -LiteralPath $pwFile) {
        Write-Host "keeping existing web console password ($pwFile)"
    }
    else {
        $password = New-RandomPassword
        Write-Host "no password supplied - generated a random web console password"
    }
}
if ($password) {
    # LF, no BOM (UTF8) so web-run.cmd's `for /f` reads a clean value.
    [IO.File]::WriteAllText($pwFile, "PUNKTFUNK_UI_PASSWORD=$password`n")
    # Lock it down: drop inheritance, grant only Administrators (S-1-5-32-544) + SYSTEM (S-1-5-18).
    & icacls $pwFile /inheritance:r /grant:r '*S-1-5-32-544:F' '*S-1-5-18:F' | Out-Null
}

# --- 2. PunktfunkWeb scheduled task ----------------------------------------------------------
$cmd = Join-Path $AppDir 'web\web-run.cmd'
if (-not (Test-Path -LiteralPath $cmd)) { throw "web launcher missing: $cmd" }
$action = New-ScheduledTaskAction -Execute $cmd
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
# RestartCount/Interval cover transient crashes + the brief post-install race before the host has
# written the mgmt token (web-run.cmd exits non-zero until then). No time limit: it's a server.
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -RestartInterval (New-TimeSpan -Minutes 1) -RestartCount 10 `
    -ExecutionTimeLimit (New-TimeSpan -Seconds 0)
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Description 'punktfunk web management console (Nitro/Node SSR on :3000)' `
    -Force | Out-Null
Write-Host "registered scheduled task $TaskName -> $cmd"

# --- 3. firewall: inbound TCP 3000 -----------------------------------------------------------
try {
    $fwName = 'PunktfunkWeb-TCP-3000'
    Get-NetFirewallRule -Name $fwName -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
    New-NetFirewallRule -Name $fwName -DisplayName 'punktfunk web console (TCP 3000)' `
        -Direction Inbound -Action Allow -Protocol TCP -LocalPort 3000 -Profile Any | Out-Null
    Write-Host "firewall: allowed inbound TCP 3000"
}
catch { Write-Warning "could not add the firewall rule for TCP 3000: $($_.Exception.Message)" }

# --- 4. wait for the host's mgmt token, then start -------------------------------------------
# The host service was installed+started just before this; give it a moment to write the token so
# the first start serves immediately (otherwise restart-on-failure picks it up within a minute).
for ($i = 0; $i -lt 30 -and -not (Test-Path -LiteralPath $tokenFile); $i++) { Start-Sleep -Seconds 1 }
Start-ScheduledTask -TaskName $TaskName
Write-Host "started $TaskName (console on http://<host-ip>:3000)"
