<#
  Rebuild the web mgmt console from the CURRENT web/ source and restart the PunktfunkWeb task.

    powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1

  bun = build tool, node = runtime (the Nitro bundle externalizes srvx/@unom for SSR, which
  bun fails to resolve at runtime). The PunktfunkWeb scheduled task runs web\web-run.cmd ->
  node .output\server\index.mjs on :3000.
#>
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot)
$web  = Join-Path $repo 'web'
$bun  = 'C:\Users\Public\bun\bin\bun.exe'
$task = 'PunktfunkWeb'
if (-not (Test-Path $bun)) { throw "bun not found at $bun" }

Set-Location $web
Write-Host "bun install + build ..."
& $bun install
& $bun run build
if ($LASTEXITCODE -ne 0) { throw "web build failed (exit $LASTEXITCODE)" }

# The Nitro server bundle externalizes its runtime deps - install them in .output/server,
# with the @unom registry .npmrc present (else @unom/* 404s on npmjs).
Write-Host "installing externalized server deps ..."
Copy-Item "$web\.npmrc" "$web\.output\server\.npmrc" -Force
Set-Location "$web\.output\server"
& $bun install

Write-Host "restarting $task ..."
& schtasks /end /tn $task 2>$null | Out-Null
Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.CommandLine -match 'index\.mjs' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep 2
& schtasks /run /tn $task | Out-Null
Start-Sleep 5
try {
  $r = Invoke-WebRequest 'http://127.0.0.1:3000/login' -UseBasicParsing -TimeoutSec 10
  Write-Host "DONE - web /login -> HTTP $($r.StatusCode)"
} catch { Write-Warning "web restarted but /login check failed: $($_.Exception.Message)" }
