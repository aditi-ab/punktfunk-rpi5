@echo off
rem punktfunk web console launcher — the action the PunktfunkWeb scheduled task runs at boot.
rem
rem Lays out next to the installed payload: {app}\web\web-run.cmd, {app}\web\.output\... and
rem {app}\bun\bun.exe (so %~dp0 = {app}\web\). Auto-wires the console the same way the Linux
rem systemd unit does: it sources the host's mgmt bearer token + the console login password from
rem %ProgramData%\punktfunk\, points the /api proxy at the host's loopback HTTPS mgmt API, and runs
rem the (self-contained, no-node_modules) Nitro server on :3000 with the bundled bun. No env editing.
setlocal EnableExtensions

set "PFDATA=%ProgramData%\punktfunk"
set "TOKENFILE=%PFDATA%\mgmt-token"
set "PWFILE=%PFDATA%\web-password"

rem The host's `serve` writes the mgmt token on first run. Until it exists the proxy has no
rem credential, so fail and let the task's restart-on-failure retry (mirrors the Linux unit's
rem Restart=on-failure waiting for the host to create it).
if not exist "%TOKENFILE%" (
  echo [punktfunk-web] mgmt token not present yet at "%TOKENFILE%" - waiting for the host service.
  exit /b 1
)

rem Both files are single KEY=VALUE lines (LF), written 0600/ACL'd: PUNKTFUNK_MGMT_TOKEN=... and
rem PUNKTFUNK_UI_PASSWORD=... . Split on the first '=' and import each into the environment.
for /f "usebackq tokens=1* delims==" %%A in ("%TOKENFILE%") do set "%%A=%%B"
if exist "%PWFILE%" for /f "usebackq tokens=1* delims==" %%A in ("%PWFILE%") do set "%%A=%%B"

rem Fixed deployment wiring (the Windows analogue of scripts/punktfunk-web.service).
set "PORT=3000"
set "HOST=0.0.0.0"
set "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990"
set "NODE_TLS_REJECT_UNAUTHORIZED=0"

set "BUN=%~dp0..\bun\bun.exe"
set "SERVER=%~dp0.output\server\index.mjs"
if not exist "%BUN%" (
  echo [punktfunk-web] bundled bun runtime missing at "%BUN%".
  exit /b 1
)
"%BUN%" "%SERVER%"
