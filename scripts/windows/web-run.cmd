@echo off
rem punktfunk web console launcher - the action the PunktfunkWeb scheduled task runs at boot.
rem
rem Lays out next to the installed payload: {app}\web\web-run.cmd, {app}\web\.output\... and
rem {app}\bun\bun.exe (so %~dp0 = {app}\web\). Auto-wires the console the same way the Linux
rem systemd unit does: it sources the host's mgmt bearer token + the console login password from
rem %ProgramData%\punktfunk\, points the /api proxy at the host's loopback HTTPS mgmt API, and serves
rem the (self-contained, no-node_modules) Nitro console over HTTPS (HTTP/1.1 over TLS) on :3000 with the
rem bundled bun, using the host's OWN identity cert. No env editing.
setlocal EnableExtensions

set "PFDATA=%ProgramData%\punktfunk"
set "TOKENFILE=%PFDATA%\mgmt-token"
set "PWFILE=%PFDATA%\web-password"
set "CERTFILE=%PFDATA%\cert.pem"
set "KEYFILE=%PFDATA%\key.pem"

rem The host's `serve` writes the mgmt token + its identity cert/key on first run. Until they exist
rem we have no credential and no TLS material, so WAIT rather than silently downgrading to plain HTTP.
rem
rem Wait in-process instead of exiting 1 and hoping the task's restart-on-failure retries: Task
rem Scheduler does not reliably restart on a plain non-zero exit code, so a console that started
rem before the host finished writing those files (the token lands at argument parse, the cert only
rem after the RSA-2048 keygen) used to stay down until the next reboot. ~5 min at 2 s, then give up
rem so a genuinely broken install still surfaces as a failed task rather than one that runs forever.
rem `timeout` needs a console this task does not have, so `ping -n 3` is the 2-second sleep.
set /a PFWAITS=0
:pfwait
if exist "%TOKENFILE%" if exist "%CERTFILE%" goto pfready
if %PFWAITS% GEQ 150 (
  echo [punktfunk-web] gave up waiting for "%TOKENFILE%" + "%CERTFILE%" - is the punktfunk host service running?
  exit /b 1
)
if %PFWAITS%==0 echo [punktfunk-web] waiting for the host service to write the mgmt token + identity cert...
set /a PFWAITS+=1
ping -n 3 127.0.0.1 >nul 2>&1
goto pfwait
:pfready

rem Both files are single KEY=VALUE lines (LF), written 0600/ACL'd: PUNKTFUNK_MGMT_TOKEN=... and
rem PUNKTFUNK_UI_PASSWORD=... . Split on the first '=' and import each into the environment.
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

set "BUN=%~dp0..\bun\bun.exe"
set "SERVER=%~dp0.output\server\index.mjs"
if not exist "%BUN%" (
  echo [punktfunk-web] bundled bun runtime missing at "%BUN%".
  exit /b 1
)
"%BUN%" "%SERVER%"
