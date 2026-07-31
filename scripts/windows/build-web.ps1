<#
  Rebuild the web mgmt console from the CURRENT web/ source and swap it into an installed host.

    powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1

  bun is both the build tool AND the runtime: vite.config's Nitro noExternals bundles every dep
  into the self-contained .output (no node_modules, nothing for bun to fail to resolve). The
  console runs as a supervised child of the PunktfunkHost service (bun {app}\web\.output\server\
  index.mjs on :47992), so the swap is: stop the service (its kill-on-close job takes the console's
  bun down and unlocks the files), replace {app}\web\.output, start the service — the supervisor
  brings the console back by itself. Needs an elevated shell (service control + Program Files).
#>
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot)
$web  = Join-Path $repo 'web'
$bun  = 'C:\Users\Public\bun\bin\bun.exe'
$app  = 'C:\Program Files\punktfunk'
if (-not (Test-Path $bun)) { throw "bun not found at $bun" }

Set-Location $web
Write-Host "bun install + build ..."
& $bun install
& $bun run build
if ($LASTEXITCODE -ne 0) { throw "web build failed (exit $LASTEXITCODE)" }
# No .output/server install step: noExternals means the output has no externalized deps to resolve.

$appWeb = Join-Path $app 'web'
if (-not (Test-Path $appWeb)) {
  Write-Host "no installed console at $appWeb - built web\.output only (run it by hand: bun web\.output\server\index.mjs)"
  return
}

Write-Host "swapping $appWeb\.output (stopping the PunktfunkHost service) ..."
& net stop PunktfunkHost | Out-Null
try {
  Remove-Item (Join-Path $appWeb '.output') -Recurse -Force -ErrorAction SilentlyContinue
  Copy-Item (Join-Path $web '.output') -Destination $appWeb -Recurse -Force
}
finally {
  & net start PunktfunkHost | Out-Null
}

# The console serves HTTPS-only (PUNKTFUNK_UI_SECURE=1, the host's own cert) - probe with curl.exe
# (-k for the self-signed cert; Invoke-WebRequest under Windows PowerShell 5.1, which this script
# runs under, has no -SkipCertificateCheck), retrying while the service/bun cold-starts.
$code = $null
for ($i = 0; $i -lt 15; $i++) {
  Start-Sleep 2
  $code = & curl.exe -sk -o NUL -w '%{http_code}' --max-time 5 'https://127.0.0.1:47992/login' 2>$null
  if ($code -eq '200') { break }
}
if ($code -eq '200') {
  Write-Host "DONE - web /login -> HTTP $code"
} else {
  Write-Warning "console swapped but /login did not return 200 yet (last: $code) - check %ProgramData%\punktfunk\logs\web.log"
}
