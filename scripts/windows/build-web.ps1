<#
  Rebuild the web mgmt console from the CURRENT web/ source and restart the PunktfunkWeb task.

    powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1

  bun is both the build tool AND the runtime: vite.config's Nitro noExternals bundles every dep
  into the self-contained .output (no node_modules, nothing for bun to fail to resolve), so the
  PunktfunkWeb task runs web\web-run.cmd -> bun .output\server\index.mjs on :47992.
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
# No .output/server install: noExternals means the output has no externalized deps to resolve.

Write-Host "restarting $task ..."
& schtasks /end /tn $task 2>$null | Out-Null
Get-CimInstance Win32_Process -Filter "Name='bun.exe' OR Name='node.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.CommandLine -match 'index\.mjs' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep 2
& schtasks /run /tn $task | Out-Null
Start-Sleep 5
try {
  $r = Invoke-WebRequest 'http://127.0.0.1:47992/login' -UseBasicParsing -TimeoutSec 10
  Write-Host "DONE - web /login -> HTTP $($r.StatusCode)"
} catch { Write-Warning "web restarted but /login check failed: $($_.Exception.Message)" }
