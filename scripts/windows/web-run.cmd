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

rem Supervise bun instead of exiting with it. The task registers RestartCount=10/RestartInterval=PT1M,
rem but Task Scheduler honours restart-on-failure only when the action CRASHES or fails to start -
rem never for a plain non-zero exit - so before this loop any bun exit parked the console until the
rem next boot or interactive logon. Seen on glass 2026-07-30 (0.22.3 upgrade): `web setup` started the
rem task one second after the upgraded host service came up, bun exited at once (Last Run Result
rem 0xFFFFFFFF), and the console stayed down for hours on a box whose host was streaming perfectly -
rem which reads to the operator as "the host is gone". A single `schtasks /run /tn PunktfunkWeb`
rem revived it, i.e. whatever blocked the start was transient. That is exactly what a retry is for.
rem
rem Restart indefinitely for a console that served for a while and then died, but refuse to spin
rem forever on an install that can never work: PFFAILS counts only CONSECUTIVE fast exits and any run
rem lasting >= PFGOODSEC resets it, so a genuine crash-loop still surfaces as a failed task after
rem ~1 min of trying (the boot + logon triggers remain the outer backstop). Same shape as the Linux
rem unit's Restart= + StartLimitIntervalSec=0.
rem
rem INVARIANT for anything that STOPS the console (the installer's StopBunRuntimes, the host's
rem `stop_web_console`): end the TASK - `schtasks /end /tn PunktfunkWeb` / `Stop-ScheduledTask` -
rem which takes this launcher down with it. Killing bun.exe ALONE is now undone by the loop below,
rem which will relaunch it and re-lock the file you were trying to replace.
set /a PFMAXFAILS=10
set /a PFGOODSEC=60
rem `timeout` needs a console this task does not have; `ping -n N` sleeps about N-1 seconds.
set /a PFDELAYPINGS=6
set /a PFFAILS=0

:pfrun
call :pfnow PFT0
"%BUN%" "%SERVER%"
set "PFRC=%ERRORLEVEL%"
call :pfnow PFT1
set /a PFUPSEC=PFT1-PFT0
if %PFUPSEC% LSS 0 set /a PFUPSEC=0
if %PFUPSEC% GEQ %PFGOODSEC% (set /a PFFAILS=0) else (set /a PFFAILS+=1)
echo [punktfunk-web] bun exited rc=%PFRC% after %PFUPSEC%s - consecutive fast exits %PFFAILS%/%PFMAXFAILS%
if %PFFAILS% GEQ %PFMAXFAILS% (
  echo [punktfunk-web] giving up after %PFMAXFAILS% consecutive fast exits - check that nothing else holds TCP 47992.
  exit /b 1
)
rem The payload disappearing is a deliberate stop, not a crash: an uninstall deletes it, and an
rem upgrade replaces it. Exit 0 rather than respawning into a half-written {app} - `web setup` starts
rem the task again at the end of an upgrade, and an uninstall wants us gone.
if not exist "%BUN%" (
  echo [punktfunk-web] "%BUN%" is gone - an install or uninstall is in progress; exiting instead of respawning.
  exit /b 0
)
if not exist "%SERVER%" (
  echo [punktfunk-web] "%SERVER%" is gone - an install or uninstall is in progress; exiting instead of respawning.
  exit /b 0
)
ping -n %PFDELAYPINGS% 127.0.0.1 >nul 2>&1
goto pfrun

rem Unix seconds, for the uptime measurement above. Deliberately NOT parsed out of %TIME%: that is
rem locale-formatted - 12-hour locales append " PM" and many others use ',' as the decimal separator
rem - which makes hand-parsing it a portability trap in a launcher that ships worldwide. This runs
rem only after bun has already exited, so the ~0.3 s cost is irrelevant. `-Command` with an inline
rem expression is unaffected by a Restricted ExecutionPolicy (that gates script FILES). Should it
rem ever fail, PFN is empty, `set /a` reads that as 0, and the run is counted as a fast exit - the
rem conservative direction.
:pfnow
setlocal EnableExtensions
set "PFN="
for /f "usebackq tokens=* delims=" %%s in (`powershell -NoProfile -Command "[DateTimeOffset]::UtcNow.ToUnixTimeSeconds()"`) do set "PFN=%%s"
endlocal & set "%~1=%PFN%"
goto :eof
