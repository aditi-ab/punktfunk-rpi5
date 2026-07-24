@echo off
rem punktfunk web console launcher - DEV layout (in-repo tree). The PunktfunkWeb scheduled task
rem (boot trigger, SYSTEM, restart-on-failure) runs this at startup. It sources the host's mgmt bearer
rem token + the console login password from %ProgramData%\punktfunk\, points the /api proxy at the
rem host's loopback HTTPS mgmt API, and serves the self-contained (no-node_modules) Nitro console over
rem HTTPS (HTTP/1.1 over TLS) on :47992 with the host's identity cert. %~dp0 = <repo>\web\ .
rem
rem DEV vs the installed launcher (scripts\windows\web-run.cmd): the dev host service runs from
rem target\release (not the installed {app} tree), so this runs the in-repo web\.output. The console
rem now runs on bun (the Nitro `bun` preset + Bun.serve TLS entry), so set BUN
rem below to your bun.exe. Rebuild after a web change with `bun run build` in web\ ; no edit needed.
setlocal EnableExtensions

set "PFDATA=%ProgramData%\punktfunk"
set "TOKENFILE=%PFDATA%\mgmt-token"
set "PWFILE=%PFDATA%\web-password"
set "CERTFILE=%PFDATA%\cert.pem"
set "KEYFILE=%PFDATA%\key.pem"

rem The host's `serve` writes the mgmt token + identity cert on first run. Until they exist the proxy
rem has no credential and no TLS material, so WAIT for them (mirrors the installed launcher) rather
rem than silently serving plain HTTP - see scripts\windows\web-run.cmd for why waiting here beats
rem exiting 1 and relying on the task's restart-on-failure. ~5 min at 2 s, then give up.
set /a PFWAITS=0
:pfwait
if exist "%TOKENFILE%" if exist "%CERTFILE%" goto pfready
if %PFWAITS% GEQ 150 (
  echo [punktfunk-web] gave up waiting for "%TOKENFILE%" + "%CERTFILE%" - is the punktfunk host running?
  exit /b 1
)
if %PFWAITS%==0 echo [punktfunk-web] waiting for the host to write the mgmt token + identity cert...
set /a PFWAITS+=1
ping -n 3 127.0.0.1 >nul 2>&1
goto pfwait
:pfready

rem Both files are single KEY=VALUE lines: PUNKTFUNK_MGMT_TOKEN=... and PUNKTFUNK_UI_PASSWORD=... .
rem Split on the first '=' and import each into the environment.
for /f "usebackq tokens=1* delims==" %%A in ("%TOKENFILE%") do set "%%A=%%B"
if exist "%PWFILE%" for /f "usebackq tokens=1* delims==" %%A in ("%PWFILE%") do set "%%A=%%B"

rem Fixed deployment wiring (the Windows analogue of scripts/punktfunk-web.service).
set "PORT=47992"
set "HOST=0.0.0.0"
set "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990"
rem No NODE_TLS_REJECT_UNAUTHORIZED: the host's self-signed cert is accepted only for the loopback
rem proxy hop, scoped inside the proxy code (Bun per-request TLS), not process-wide.
rem Serve HTTPS (HTTP/1.1 over TLS) with the host's identity cert; mark the session cookie Secure.
set "PUNKTFUNK_UI_TLS_CERT=%CERTFILE%"
set "PUNKTFUNK_UI_TLS_KEY=%KEYFILE%"
set "PUNKTFUNK_UI_SECURE=1"

rem Bun runtime (override BUN if yours lives elsewhere / is on PATH as just `bun`).
if not defined BUN set "BUN=bun.exe"
set "SERVER=%~dp0.output\server\index.mjs"
if not exist "%SERVER%" (
  echo [punktfunk-web] built server missing at "%SERVER%" - build it: cd web ^&^& bun run build
  exit /b 1
)
"%BUN%" "%SERVER%"
