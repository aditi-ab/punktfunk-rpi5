//! Windows service: a SYSTEM supervisor that launches the streaming host into the **active
//! interactive console session** and keeps it tracking session switches — the end-user replacement
//! for the ad-hoc PsExec / VBS / scheduled-task launch chain used during bring-up.
//!
//! Why a supervisor and not just "run the host as a service": the host must run **as SYSTEM in the
//! interactive session** (session 1+). Capturing the secure (Winlogon/UAC/lock) desktop and
//! `SendInput` both need SYSTEM; capture and injection both need the *interactive* session, which
//! a plain session-0 service is not in. So this service (itself in session 0) never captures — it
//! duplicates its own LocalSystem token, retargets it to the active console session, and
//! `CreateProcessAsUserW`s the host there. This is the Sunshine/Apollo model. The host captures the
//! virtual display in-process via IDD direct-push (no helper process).
//!
//! Subcommands (Windows only):
//! ```text
//!   punktfunk-host service run        SCM entry point (registered as binPath; not run by hand)
//!   punktfunk-host service install    register an auto-start LocalSystem service + firewall rules
//!   punktfunk-host service uninstall  stop + delete the service + remove firewall rules
//!   punktfunk-host service start|stop|status   convenience wrappers over the SCM
//! ```
//! Config lives in `%ProgramData%\punktfunk\host.env` (the Windows analogue of `scripts/host.env`),
//! loaded into the service's environment and carried to the host child. Logs land in
//! `%ProgramData%\punktfunk\logs\`.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsString};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    SECURITY_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_DATA, OPEN_ALWAYS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, ResetEvent, SetEvent,
    TerminateProcess, WaitForMultipleObjects, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// SCM service name (the key under HKLM\SYSTEM\CurrentControlSet\Services). Stable identity.
const SERVICE_NAME: &str = "PunktfunkHost";
const SERVICE_DISPLAY: &str = "punktfunk streaming host";
const SERVICE_DESCRIPTION: &str =
    "Low-latency desktop/game streaming host. Launches the punktfunk host into the active session.";

/// The host subcommand the service launches, overridable via `PUNKTFUNK_HOST_CMD` in host.env.
/// `serve --gamestream` runs the native punktfunk/1 QUIC host (always on) PLUS the GameStream
/// (Moonlight) compat planes — the unified host a Windows end user typically wants (Moonlight is the
/// common Windows client). Drop `--gamestream` for a secure native-only host (no plain-HTTP pairing /
/// legacy GCM nonce reuse — security-review #5/#9; native clients only).
const DEFAULT_HOST_CMD: &str = "serve --gamestream";

/// The STOP and SESSION manual-reset events, shared between the SCM control handler (a capture-free
/// `'static` closure that SIGNALS them) and the supervision loop (which WAITS on them). They live in
/// `OnceLock`s — a static the handler can reach without capturing a non-`Send` `HANDLE` — and each owns
/// its handle (`OwnedHandle`) for the process lifetime: the service process exits right after
/// `run_service` returns, so the OS reaps them at exit, and owning them past the handler's last possible
/// call avoids the close-then-signal window the old raw-`isize` statics had. Set once, in `run_service`.
static STOP_EVENT: OnceLock<OwnedHandle> = OnceLock::new();
static SESSION_EVENT: OnceLock<OwnedHandle> = OnceLock::new();

/// Borrow an event's handle for the control handler's `SetEvent`. `None` until `run_service` creates the
/// events — but the handler is registered only AFTER they're set, so in practice this is always `Some`.
fn event_handle(ev: &OnceLock<OwnedHandle>) -> Option<HANDLE> {
    ev.get().map(|h| HANDLE(h.as_raw_handle()))
}

/// Dispatch `service <sub>`.
pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("run") => run(),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("start") => sc(&["start", SERVICE_NAME]),
        Some("stop") => sc(&["stop", SERVICE_NAME]),
        Some("status") => sc(&["query", SERVICE_NAME]),
        _ => {
            eprintln!(
                "punktfunk-host service — Windows service control\n\n\
                 USAGE:\n\
                 \x20   punktfunk-host service install [--gamestream=on|off]\n\
                 \x20                                      register the auto-start service + firewall rules\n\
                 \x20                                      (--gamestream sets host.env's PUNKTFUNK_HOST_CMD)\n\
                 \x20   punktfunk-host service uninstall   stop + remove the service + firewall rules\n\
                 \x20   punktfunk-host service start       start the service now\n\
                 \x20   punktfunk-host service stop        stop the service\n\
                 \x20   punktfunk-host service status      query the service\n\n\
                 Config: %ProgramData%\\punktfunk\\host.env   Logs: %ProgramData%\\punktfunk\\logs\\"
            );
            Ok(())
        }
    }
}

// ── Logging ─────────────────────────────────────────────────────────────────────────────────────

/// `%ProgramData%\punktfunk\logs\service.log` — the service's own (supervision) log. The host child's
/// stdout/stderr are redirected to `host.log` in the same dir.
pub fn service_log_path() -> PathBuf {
    let dir = crate::gamestream::config_dir().join("logs");
    // DACL-locked (Users read-only, no create) so a local user can't pre-plant SYSTEM log files as
    // reparse points / hardlinks to redirect the SYSTEM service's writes (security-review #11).
    let _ = crate::gamestream::create_private_dir(&dir);
    dir.join("service.log")
}

fn host_log_path() -> PathBuf {
    let dir = crate::gamestream::config_dir().join("logs");
    let _ = crate::gamestream::create_private_dir(&dir);
    dir.join("host.log")
}

/// Initialise tracing to the service log file (the SCM gives the service no console/stderr). Falls
/// back to stderr if the file can't be opened. Called from `main()` only for `service run`.
/// Also tees into the in-memory log ring (`log_capture`), like the stderr path in `main()` — the
/// supervisor serves no mgmt API itself, but the layer is harmless and keeps both inits uniform.
pub fn init_file_logging(filter: tracing_subscriber::EnvFilter) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;
    let ring =
        crate::log_capture::RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(service_log_path())
    {
        Ok(file) => {
            tracing_subscriber::registry()
                .with(ring)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(move || file.try_clone().expect("clone service log handle"))
                        .with_filter(filter),
                )
                .init();
        }
        Err(_) => {
            tracing_subscriber::registry()
                .with(ring)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(filter),
                )
                .init();
        }
    }
}

// ── host.env config ─────────────────────────────────────────────────────────────────────────────

fn host_env_path() -> PathBuf {
    crate::gamestream::config_dir().join("host.env")
}

/// Load `%ProgramData%\punktfunk\host.env` (KEY=VALUE lines, `#` comments) into this process's
/// environment, so the host child inherits `PUNKTFUNK_*` / `RUST_LOG` via the merged env block.
fn load_host_env() {
    let path = host_env_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        tracing::info!(path = %path.display(), "no host.env (using defaults)");
        return;
    };
    let mut n = 0;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if !k.is_empty() {
                std::env::set_var(k, v);
                n += 1;
            }
        }
    }
    tracing::info!(path = %path.display(), vars = n, "loaded host.env");
}

// ── service run (SCM entry point) ────────────────────────────────────────────────────────────────

windows_service::define_windows_service!(ffi_service_main, service_main);

fn run() -> Result<()> {
    // Blocks until the service stops; the SCM then calls `service_main` on its own thread.
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        anyhow::anyhow!(
            "service_dispatcher failed ({e}). `service run` is launched by the Service Control \
             Manager, not by hand — use `punktfunk-host service install` then `service start`."
        )
    })
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e:#}");
    }
}

fn run_service() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    // Two manual-reset events: STOP (set once, never reset) and SESSION (set on a console
    // connect/disconnect, reset by the supervisor after it reacts).
    // SAFETY: CreateEventW with null attributes (None), manual-reset=true, initial-state=false and a null
    // name passes no pointers into Rust memory; it returns a fresh, owned event HANDLE (or Err, via `?`).
    // Nothing aliases or outlives the call.
    let stop_raw =
        unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.context("CreateEvent stop")?;
    // SAFETY: as above — a second fresh manual-reset event; no pointers into Rust memory, no aliasing.
    let session_raw = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .context("CreateEvent session")?;
    // Own each event handle (the OS reaps them at process exit); the handler reaches them through the
    // OnceLocks, while `supervise` waits on the borrowed `HANDLE`s. SAFETY: each is a fresh CreateEventW
    // handle we own — take ownership exactly once.
    let stop_owned = unsafe { OwnedHandle::from_raw_handle(stop_raw.0) };
    // SAFETY: `session_raw` is the other fresh CreateEventW handle nothing else owns — take ownership once.
    let session_owned = unsafe { OwnedHandle::from_raw_handle(session_raw.0) };
    let stop = HANDLE(stop_owned.as_raw_handle());
    let session = HANDLE(session_owned.as_raw_handle());
    let _ = STOP_EVENT.set(stop_owned); // set once per process
    let _ = SESSION_EVENT.set(session_owned);

    // The control handler captures nothing — it reaches the events through the statics, so it stays
    // `Fn + Send + 'static`. Lock/unlock is handled by the in-process IDD-push capture (the driver
    // composes the secure desktop into the ring), so we only flag console connect/disconnect/logon —
    // the events that change the active session.
    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
                if let Some(h) = event_handle(&STOP_EVENT) {
                    // SAFETY: `h` borrows the STOP event HANDLE from the STOP_EVENT OwnedHandle, set for
                    // the whole process lifetime and never closed before exit, so it is open here; SetEvent
                    // only signals the event and passes no Rust memory.
                    unsafe { SetEvent(h) }.ok();
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(param) => {
                use windows_service::service::SessionChangeReason::*;
                if matches!(
                    param.reason,
                    ConsoleConnect | ConsoleDisconnect | SessionLogon
                ) {
                    if let Some(h) = event_handle(&SESSION_EVENT) {
                        // SAFETY: `h` borrows the SESSION event HANDLE from the SESSION_EVENT OwnedHandle,
                        // alive for the whole process lifetime and never closed before exit; SetEvent only
                        // signals the event and passes no Rust memory.
                        unsafe { SetEvent(h) }.ok();
                    }
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)
        .context("register service control handler")?;

    let accepted = ServiceControlAccept::STOP
        | ServiceControlAccept::PRESHUTDOWN
        | ServiceControlAccept::SESSION_CHANGE;
    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle
        .set_service_status(running.clone())
        .context("set RUNNING")?;
    tracing::info!("punktfunk service started — supervising host in the active console session");

    load_host_env();
    let result = supervise(stop, session);

    // Report STOPPED regardless of how supervise returned.
    let _ = status_handle.set_service_status(ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        ..running
    });
    // The STOP/SESSION events stay owned by the OnceLocks for the process lifetime (the OS reaps them at
    // exit); NOT closing them while the SCM handler could still fire avoids a use-after-close.
    result
}

/// The supervision loop: (re)launch the host into the active console session and wait on
/// [stop, session-change, child-exit], relaunching on child exit and on a console-session switch.
fn supervise(stop: HANDLE, session_ev: HANDLE) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let host_cmd = std::env::var("PUNKTFUNK_HOST_CMD").unwrap_or_else(|_| DEFAULT_HOST_CMD.into());
    let cmdline = format!("\"{}\" {host_cmd}", exe.to_string_lossy());
    let workdir: Vec<u16> = exe
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // Kill-on-close job so a service crash never orphans the SYSTEM host; BREAKAWAY_OK lets the host
    // still spawn the WGC helper. Owned: dropping it at function exit (KILL_ON_JOB_CLOSE) reaps any
    // straggler still inside it — no manual CloseHandle(job).
    // SAFETY: `make_job` is unsafe only for its Win32 FFI; it has no caller preconditions and creates +
    // immediately takes RAII ownership of the job object, so calling it here is sound.
    let job = unsafe { make_job() }.context("create job object")?;

    let mut restarts: u32 = 0;
    loop {
        if wait_one(stop, 0) {
            break;
        }
        // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and returns the active console session
        // id (or 0xFFFFFFFF); it passes no pointers, so the call is always sound.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == 0xFFFF_FFFF {
            // No interactive session yet (boot / fully logged out). Wait, but wake on stop/session.
            tracing::info!("no active console session — waiting");
            if wait_any(&[stop, session_ev], 3000) == Some(0) {
                break;
            }
            // SAFETY: `session_ev` is the SESSION event HANDLE borrowed from the SESSION_EVENT OwnedHandle,
            // alive for the process lifetime; ResetEvent only clears its signalled state, no Rust memory.
            unsafe { ResetEvent(session_ev) }.ok();
            continue;
        }

        // BORROW the owned job handle for AssignProcessToJobObject inside spawn_host.
        let job_h = HANDLE(job.as_raw_handle());
        // SAFETY: `spawn_host` is unsafe only for its Win32 FFI. `session` is a valid console session id
        // (checked != 0xFFFFFFFF above), `cmdline`/`workdir` are live borrows for the call, and `job_h`
        // borrows the still-live `job` OwnedHandle — every argument is valid for the call's duration.
        let child = match unsafe { spawn_host(session, &cmdline, &workdir, job_h) } {
            Ok(child) => child,
            Err(e) => {
                tracing::error!("failed to launch host into session {session}: {e:#}");
                if wait_one(stop, 3000) {
                    break;
                }
                continue;
            }
        };
        tracing::info!(pid = child.pid, session, cmd = %host_cmd, "host launched");

        // A BORROW of the owned process handle for the waits + TerminateProcess (HANDLE is Copy, so
        // `proc_h` is a plain copy that does NOT close it). `child` owns the process + thread handles
        // and auto-closes BOTH when it drops — at the end of this iteration, on `continue`, or on
        // `break` — so every match arm below only stops/terminates and lets the drop do the closing.
        let proc_h = HANDLE(child.process.as_raw_handle());

        // Wait on stop / session-change / child-exit.
        let reason = wait_any(&[stop, session_ev, proc_h], INFINITE);
        match reason {
            Some(0) => {
                // Stop: terminate the child and exit (the `child` drop closes its handles).
                // SAFETY: `proc_h` is a HANDLE copy of the still-live `child.process` OwnedHandle (not
                // dropped until end of iteration), so the process handle is open; TerminateProcess only
                // signals termination by handle and passes no Rust memory.
                unsafe {
                    let _ = TerminateProcess(proc_h, 0);
                }
                break;
            }
            Some(1) => {
                // Session change: relaunch only if the active console session actually moved.
                // SAFETY: `session_ev` borrows the process-lifetime SESSION_EVENT OwnedHandle; ResetEvent
                // only clears its signalled state and passes no Rust memory.
                unsafe { ResetEvent(session_ev) }.ok();
                // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and passes no pointers.
                let now = unsafe { WTSGetActiveConsoleSessionId() };
                if now != session {
                    tracing::info!(
                        old = session,
                        new = now,
                        "console session changed — relaunching host"
                    );
                    // SAFETY: `proc_h` copies the still-live `child.process` OwnedHandle (dropped only at
                    // end of iteration), so the handle is open; TerminateProcess only signals by handle.
                    unsafe {
                        let _ = TerminateProcess(proc_h, 0);
                    }
                    restarts = 0;
                    continue;
                }
                // Same session (e.g. a stray notification) — keep waiting on the same child.
                let r = wait_any(&[stop, proc_h], INFINITE);
                // SAFETY: `proc_h` copies the still-live `child.process` OwnedHandle (dropped only at end
                // of iteration), so the handle is open; TerminateProcess only signals by handle.
                unsafe {
                    let _ = TerminateProcess(proc_h, 0);
                }
                if r == Some(0) {
                    break;
                }
                // child exited → fall through to relaunch
            }
            _ => {
                // Child exited on its own — relaunch (with a small crash-loop backoff). The `child`
                // drop closes its (already-exited) handles.
                tracing::warn!("host process exited — relaunching");
            }
        }

        restarts += 1;
        let backoff = restarts.min(10) * 500; // 0.5s..5s
        if wait_one(stop, backoff) {
            break;
        }
        // `child` drops here (end of iteration) → its process + thread handles close before relaunch.
    }

    // `job` (OwnedHandle) drops at function exit, closing the job object → KILL_ON_JOB_CLOSE reaps
    // any straggler still inside it.
    tracing::info!("supervision loop ended");
    Ok(())
}

/// `true` if `h` is signalled within `ms`.
fn wait_one(h: HANDLE, ms: u32) -> bool {
    // SAFETY: `&[h]` is a live one-element HANDLE slice the caller keeps open across the wait; the kernel
    // reads exactly one handle (the binding derives the count from the slice length), bWaitAll=false,
    // `ms` is a timeout — no pointers escape and the array is only read for this synchronous call.
    unsafe { WaitForMultipleObjects(&[h], false, ms) == WAIT_OBJECT_0 }
}

/// Wait on several handles; returns the index of the first signalled, or `None` on timeout.
fn wait_any(handles: &[HANDLE], ms: u32) -> Option<usize> {
    // SAFETY: `handles` is a live slice the caller keeps open across the wait; WaitForMultipleObjects
    // reads exactly `handles.len()` handles (the binding derives the count from the slice), bWaitAll=false,
    // `ms` is a timeout — the array is only read for this synchronous call and no pointers escape it.
    let r = unsafe { WaitForMultipleObjects(handles, false, ms) };
    let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0);
    (idx < handles.len() as u32).then_some(idx as usize)
}

/// A kill-on-close + breakaway-ok job object, returned as an `OwnedHandle` (auto-`CloseHandle` on drop).
unsafe fn make_job() -> Result<OwnedHandle> {
    let job_raw = CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObjectW")?;
    // Own it immediately so any early return (e.g. a failed SetInformationJobObject) still closes it.
    let job = OwnedHandle::from_raw_handle(job_raw.0);
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    SetInformationJobObject(
        HANDLE(job.as_raw_handle()),
        JobObjectExtendedLimitInformation,
        &info as *const _ as *const c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    )
    .context("SetInformationJobObject")?;
    Ok(job)
}

/// The owned handles to a spawned host child. The `process`/`thread` `OwnedHandle`s auto-`CloseHandle`
/// when the `Child` drops (or is replaced each loop iteration) — replacing the manual
/// `CloseHandle(pi.hProcess/hThread)` the supervise loop used to scatter across its match arms.
struct Child {
    process: OwnedHandle,
    /// Held only for its RAII `CloseHandle` (the thread handle is never used after spawn) — `_`-prefixed
    /// so the `dead_code` lint (CI's `-D warnings`) doesn't flag the never-read field.
    _thread: OwnedHandle,
    pid: u32,
}

/// Launch the host as SYSTEM into `session_id`'s interactive desktop. Returns the owned child handles.
unsafe fn spawn_host(
    session_id: u32,
    cmdline: &str,
    workdir: &[u16],
    job: HANDLE,
) -> Result<Child> {
    // 1) A primary SYSTEM token retargeted to the active console session: duplicate THIS process's
    //    (LocalSystem) token, then set its session id. SYSTEM holds SE_TCB so SetTokenInformation
    //    (TokenSessionId) is permitted.
    let mut proc_token = HANDLE::default();
    OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_DUPLICATE
            | TOKEN_QUERY
            | TOKEN_ASSIGN_PRIMARY
            | TOKEN_ADJUST_DEFAULT
            | TOKEN_ADJUST_SESSIONID,
        &mut proc_token,
    )
    .context("OpenProcessToken (service must run as SYSTEM)")?;

    let mut primary = HANDLE::default();
    let dup = DuplicateTokenEx(
        proc_token,
        TOKEN_ALL_ACCESS,
        None,
        SecurityImpersonation,
        TokenPrimary,
        &mut primary,
    );
    let _ = CloseHandle(proc_token);
    dup.context("DuplicateTokenEx(TokenPrimary)")?;

    SetTokenInformation(
        primary,
        TokenSessionId,
        &session_id as *const u32 as *const c_void,
        std::mem::size_of::<u32>() as u32,
    )
    .context("SetTokenInformation(TokenSessionId)")?;

    // 2) The session's environment block, merged with this process's PUNKTFUNK_*/RUST_LOG (so the
    //    host runs with host.env's settings, not a bare block). Same merge the interactive launch uses.
    let mut env_block: *mut c_void = std::ptr::null_mut();
    let _ = CreateEnvironmentBlock(&mut env_block, Some(primary), false);
    let merged = crate::interactive::merged_env_block(env_block as *const u16);
    if !env_block.is_null() {
        let _ = DestroyEnvironmentBlock(env_block);
    }

    // 3) Redirect the host's stdout+stderr to host.log (inheritable handle).
    let log = open_log_handle(&host_log_path())?;

    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log,
        hStdError: log,
        ..Default::default()
    };
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    si.lpDesktop = PWSTR(desktop.as_mut_ptr());

    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd = (!workdir.is_empty()).then_some(PCWSTR(workdir.as_ptr()));
    let mut pi = PROCESS_INFORMATION::default();

    let created = CreateProcessAsUserW(
        Some(primary),
        None,
        Some(PWSTR(cmd.as_mut_ptr())),
        None,
        None,
        true, // inherit the log handle
        CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
        Some(merged.as_ptr() as *const c_void),
        cwd.unwrap_or(PCWSTR::null()),
        &si,
        &mut pi,
    );

    let _ = CloseHandle(log); // the child owns its inherited copy
    let _ = CloseHandle(primary);
    created.context("CreateProcessAsUserW(host)")?;

    // Best-effort: keep the host inside the kill-on-close job.
    let _ = AssignProcessToJobObject(job, pi.hProcess);

    // Take ownership of the process + thread handles the API filled into `pi`; the returned `Child`
    // closes BOTH on drop, so the supervise loop no longer hand-closes them in its match arms.
    Ok(Child {
        process: OwnedHandle::from_raw_handle(pi.hProcess.0),
        _thread: OwnedHandle::from_raw_handle(pi.hThread.0),
        pid: pi.dwProcessId,
    })
}

/// Open `path` for appending, as an INHERITABLE handle (so the child can use it as stdout/stderr).
unsafe fn open_log_handle(path: &std::path::Path) -> Result<HANDLE> {
    let wpath: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // Append (no FILE_WRITE_DATA → all writes go to EOF), so each relaunch's OPEN_ALWAYS reopen
    // accumulates instead of truncating from offset 0. This mirrors Rust's own `OpenOptions::append`
    // access mask (FILE_GENERIC_WRITE minus WRITE_DATA, plus APPEND_DATA + SYNCHRONIZE/READ_CONTROL);
    // bare FILE_APPEND_DATA alone produced a child handle that silently dropped writes.
    let access = (FILE_GENERIC_WRITE.0 & !FILE_WRITE_DATA.0) | FILE_APPEND_DATA.0;
    let h = CreateFileW(
        PCWSTR(wpath.as_ptr()),
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        Some(&sa),
        OPEN_ALWAYS,
        windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(host.log)")?;
    Ok(h)
}

// ── install / uninstall ──────────────────────────────────────────────────────────────────────────

fn install(args: &[String]) -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    // `--gamestream=on|off` (the installer's wizard task): None = flag absent, keep host.env as-is.
    let gamestream = match args.iter().find_map(|a| a.strip_prefix("--gamestream=")) {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(v) => bail!("--gamestream must be 'on' or 'off' (got '{v}')"),
        None => None,
    };

    let exe = std::env::current_exe().context("current_exe")?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open Service Control Manager (run from an elevated/Administrator prompt)")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // None = LocalSystem
        account_password: None,
    };

    // Create, or reconfigure if it already exists (idempotent install/upgrade).
    match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START) {
        Ok(svc) => {
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Created service '{SERVICE_NAME}' (auto-start, LocalSystem).");
        }
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(1073 /* ERROR_SERVICE_EXISTS */) =>
        {
            let svc = manager
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .context("open existing service to reconfigure")?;
            svc.change_config(&info)
                .context("reconfigure existing service")?;
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Reconfigured existing service '{SERVICE_NAME}'.");
        }
        Err(e) => return Err(e).context("create service"),
    }

    ensure_default_host_env()?;
    if let Some(on) = gamestream {
        apply_gamestream_choice(on);
    }
    add_firewall_rules();

    println!(
        "\nInstalled. Config: {}\nLogs:   {}\n\nStart now with:  punktfunk-host service start",
        host_env_path().display(),
        crate::gamestream::config_dir().join("logs").display()
    );
    Ok(())
}

fn uninstall() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let _ = sc(&["stop", SERVICE_NAME]); // best-effort stop first
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(SERVICE_NAME, ServiceAccess::DELETE)
        .context("open service for delete")?;
    svc.delete().context("delete service")?;
    remove_firewall_rules();
    println!("Removed service '{SERVICE_NAME}' and its firewall rules.");
    Ok(())
}

/// Write a default `host.env` if none exists, so a fresh install streams out of the box. The encoder
/// defaults to `auto` — the host picks NVENC (NVIDIA) / AMF (AMD) / QSV (Intel) from the GPU vendor.
fn ensure_default_host_env() -> Result<()> {
    let path = host_env_path();
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        // DACL-lock the config dir on creation so a local user can't pre-create it and plant a
        // host.env (which feeds the SYSTEM service's env + command line) — security-review #3.
        crate::gamestream::create_private_dir(dir).ok();
    }
    let default = "# punktfunk host configuration (read by the Windows service).\n\
        # KEY=VALUE per line; '#' comments. Restart the service after editing:\n\
        #   punktfunk-host service stop && punktfunk-host service start\n\
        \n\
        # Encode backend: auto (default) detects the GPU vendor — NVIDIA->nvenc, AMD->amf, Intel->qsv.\n\
        # Force one with nvenc | amf | qsv | sw (software H.264). amf/qsv need an FFmpeg-built host.\n\
        PUNKTFUNK_ENCODER=auto\n\
        PUNKTFUNK_VIDEO_SOURCE=virtual\n\
        # Virtual display = the bundled pf-vdisplay driver; capture from its shared ring (the validated\n\
        # zero-copy IDD-push path; falls back to DDA if it can't attach). Set PUNKTFUNK_IDD_PUSH=0 to force WGC/DDA.\n\
        PUNKTFUNK_VDISPLAY=pf\n\
        PUNKTFUNK_IDD_PUSH=1\n\
        PUNKTFUNK_SECURE_DDA=1\n\
        RUST_LOG=info\n\
        \n\
        # The host subcommand the service launches (default: serve --gamestream = native + Moonlight\n\
        # compat). Use `serve` for a SECURE native-only host (no GameStream #5/#9 surface).\n\
        # PUNKTFUNK_HOST_CMD=serve --gamestream\n\
        \n\
        # Force a specific render GPU by name substring (multi-GPU boxes only):\n\
        # PUNKTFUNK_RENDER_ADAPTER=4090\n";
    // Write host.env DACL-locked to SYSTEM/Administrators: it controls the SYSTEM service's
    // environment + launched command line, so a local user must not be able to read or tamper with
    // it (security-review 2026-06-28 #3).
    crate::gamestream::write_secret_file(&path, default.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    println!("Wrote default config: {}", path.display());
    Ok(())
}

/// Write the installer's GameStream choice into host.env's `PUNKTFUNK_HOST_CMD`. Upgrade-safe:
/// only an absent line or one of the two canonical values (`serve` / `serve --gamestream`) is
/// rewritten — a hand-customized command line is the user's, and stays. Best-effort (warns).
fn apply_gamestream_choice(enable: bool) {
    let path = host_env_path();
    let desired = if enable {
        "serve --gamestream"
    } else {
        "serve"
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!(
            "warning: could not read {} to apply the GameStream choice",
            path.display()
        );
        return;
    };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let current = lines.iter().position(|l| {
        let t = l.trim_start();
        !t.starts_with('#') && t.starts_with("PUNKTFUNK_HOST_CMD=")
    });
    match current {
        Some(i) => {
            let value = lines[i].trim_start()["PUNKTFUNK_HOST_CMD=".len()..].trim();
            if value == desired {
                return; // already what the installer chose
            }
            if value != "serve" && value != "serve --gamestream" {
                println!(
                    "host.env has a customized PUNKTFUNK_HOST_CMD ({value}) - leaving it \
                     (installer GameStream choice not applied)"
                );
                return;
            }
            lines[i] = format!("PUNKTFUNK_HOST_CMD={desired}");
        }
        None => lines.push(format!("PUNKTFUNK_HOST_CMD={desired}")),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    // Rewrite through write_secret_file so the SYSTEM/Administrators DACL is re-asserted.
    if let Err(e) = crate::gamestream::write_secret_file(&path, out.as_bytes()) {
        eprintln!("warning: could not write {}: {e}", path.display());
        return;
    }
    println!(
        "GameStream (Moonlight) compatibility: {} (PUNKTFUNK_HOST_CMD={desired})",
        if enable { "enabled" } else { "disabled" }
    );
}

// ── firewall + sc helpers ────────────────────────────────────────────────────────────────────────

/// Inbound firewall rules for the streaming ports (best-effort; logs but never fails the install).
fn add_firewall_rules() {
    // (name suffix, protocol, ports)
    let rules = [
        ("TCP", "TCP", "47984,47989,48010,47990"),
        ("UDP", "UDP", "47998-48010,9777,5353"),
    ];
    for (suffix, proto, ports) in rules {
        let name = format!("punktfunk {suffix}");
        let ok = run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={name}"),
                "dir=in",
                "action=allow",
                &format!("protocol={proto}"),
                &format!("localport={ports}"),
            ],
        );
        if ok {
            println!("Firewall rule added: {name} ({ports})");
        } else {
            eprintln!("warning: could not add firewall rule '{name}' (add it manually if needed)");
        }
    }
}

fn remove_firewall_rules() {
    for suffix in ["TCP", "UDP"] {
        let name = format!("punktfunk {suffix}");
        let _ = run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
    }
}

/// Run an `sc.exe` command, passing its output through (used by start/stop/status).
fn sc(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("sc")
        .args(args)
        .status()
        .context("run sc.exe")?;
    if !status.success() {
        bail!("sc {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Run a command discarding output; return whether it succeeded.
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
