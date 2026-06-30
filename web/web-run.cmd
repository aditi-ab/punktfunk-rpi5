@echo off
rem punktfunk web console launcher - DEV layout (in-repo tree). The PunktfunkWeb scheduled task
rem (boot trigger, SYSTEM, restart-on-failure) runs this at startup. It sources the host's mgmt bearer
rem token + the console login password from %ProgramData%\punktfunk\, points the /api proxy at the
rem host's loopback HTTPS mgmt API, and runs the self-contained (no-node_modules) Nitro server on :3000.
rem %~dp0 = <repo>\web\ .
rem
rem DEV vs the installed launcher (scripts\windows\web-run.cmd): the dev host service runs from
rem target\release (not the installed {app} tree), so this runs the in-repo web\.output with the
rem system node instead of {app}\bun\bun.exe + {app}\web\.output. Rebuild after a web change with
rem `bun run build` in web\ ; no edit needed here.
setlocal EnableExtensions

set "PFDATA=%ProgramData%\punktfunk"
set "TOKENFILE=%PFDATA%\mgmt-token"
set "PWFILE=%PFDATA%\web-password"

rem The host's `serve` writes the mgmt token on first run. Until it exists the proxy has no credential,
rem so fail and let the task's restart-on-failure retry (mirrors the installed launcher / Linux unit).
if not exist "%TOKENFILE%" (
  echo [punktfunk-web] mgmt token not present yet at "%TOKENFILE%" - waiting for the host service.
  exit /b 1
)

rem Both files are single KEY=VALUE lines: PUNKTFUNK_MGMT_TOKEN=... and PUNKTFUNK_UI_PASSWORD=... .
rem Split on the first '=' and import each into the environment.
for /f "usebackq tokens=1* delims==" %%A in ("%TOKENFILE%") do set "%%A=%%B"
if exist "%PWFILE%" for /f "usebackq tokens=1* delims==" %%A in ("%PWFILE%") do set "%%A=%%B"

rem Fixed deployment wiring (the Windows analogue of scripts/punktfunk-web.service).
set "PORT=3000"
set "HOST=0.0.0.0"
set "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990"
set "NODE_TLS_REJECT_UNAUTHORIZED=0"

set "NODE=C:\Users\Public\node-v22.11.0-win-x64\node.exe"
set "SERVER=%~dp0.output\server\index.mjs"
if not exist "%NODE%" (
  echo [punktfunk-web] node runtime missing at "%NODE%".
  exit /b 1
)
if not exist "%SERVER%" (
  echo [punktfunk-web] built server missing at "%SERVER%" - build it: cd web ^&^& bun run build
  exit /b 1
)
"%NODE%" "%SERVER%"
