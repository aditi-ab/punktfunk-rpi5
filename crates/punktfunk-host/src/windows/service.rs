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
//! The service also supervises a SECOND child: the web management console (bun/Nitro on :47992),
//! spawned plainly into session 0 — see the "web console child" section below for why it is not a
//! Task Scheduler task anymore.
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

use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsString};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, ResetEvent, ResumeThread, SetEvent, TerminateProcess, WaitForMultipleObjects,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// SCM service name (the key under HKLM\SYSTEM\CurrentControlSet\Services). Stable identity.
const SERVICE_NAME: &str = "PunktfunkHost";
const SERVICE_DISPLAY: &str = "Punktfunk Host";
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
        Some("restart") => restart(),
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
                 \x20   punktfunk-host service restart     stop, wait for exit, start again\n\
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
    let dir = pf_paths::config_dir().join("logs");
    // DACL-locked (no create) so a local user can't pre-plant SYSTEM log files as reparse points /
    // hardlinks to redirect the SYSTEM service's writes (security-review #11). `create_secret_dir`,
    // not `create_private_dir`: the config dir's inheritable `BUILTIN\Users:(RX)` reached these
    // files too, and a host log carries webhook URLs and launched command lines — the operator
    // reads them through the console, not off disk (security-review 2026-08-25).
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("service.log")
}

fn host_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("host.log")
}

/// One-generation size cap for the append-forever logs: at each (re)open, a file over this size is
/// renamed to `<name>.old` (replacing the previous generation) — so a crash-restart loop or a
/// `RUST_LOG=debug` left in host.env can't grow them without bound.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rotate `path` to `path.old` when it has outgrown [`LOG_ROTATE_BYTES`]. Only called right before
/// an open (service start for service.log, each host (re)launch for host.log) — never while a live
/// handle appends: renaming under an appender would silently redirect its writes into the `.old`
/// file. Best-effort; a failed rename just means one more un-rotated run.
fn rotate_if_large(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_ROTATE_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, std::path::Path::new(&old));
    }
}

/// Initialise tracing to the service log file (the SCM gives the service no console/stderr). Falls
/// back to stderr if the file can't be opened. Called from `main()` only for `service run`.
/// Also tees into the in-memory log ring (`log_capture`), like the stderr path in `main()` — the
/// supervisor serves no mgmt API itself, but the layer is harmless and keeps both inits uniform.
pub fn init_file_logging(filter: tracing_subscriber::EnvFilter) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;
    let ring =
        crate::log_capture::RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
    let log_path = service_log_path();
    rotate_if_large(&log_path);
    // `install_global` inits the log bridge with `ignore_crate("wasapi")` (see its doc) and sets
    // the subscriber global — replacing `SubscriberInitExt::init`, which would bridge every crate.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(file) => {
            crate::log_capture::install_global(
                tracing_subscriber::registry().with(ring).with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(move || file.try_clone().expect("clone service log handle"))
                        .with_filter(filter),
                ),
            );
        }
        Err(_) => {
            crate::log_capture::install_global(
                tracing_subscriber::registry().with(ring).with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(filter),
                ),
            );
        }
    }
}

// ── host.env config ─────────────────────────────────────────────────────────────────────────────

fn host_env_path() -> PathBuf {
    pf_paths::config_dir().join("host.env")
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
            // Allow-list, matching `interactive::merged_env_block`'s filter at the child-spawn
            // boundary: import ONLY `PUNKTFUNK_*` / `RUST_LOG` into the LocalSystem service's own
            // environment. Without this, EVERY key was injected — including `SystemRoot`, from which
            // `pf_paths::icacls_path()` / the powershell warner build the absolute program paths a
            // privileged service must never resolve through a poisoned env — so a `host.env` planted
            // in the user-writable %ProgramData% before install yielded code execution as SYSTEM.
            // security-review 2026-08-15 finding 3. (Note: `PUNKTFUNK_HOST_CMD` / `PUNKTFUNK_CONFIG_DIR`
            // are legitimate installer-written knobs and still pass; a PLANTED file redirecting THEM
            // is closed separately by distrusting a non-admin-owned host.env — findings 3c/4.)
            let allowed = k.starts_with("PUNKTFUNK_") || k == "RUST_LOG";
            if !k.is_empty() && allowed {
                // SAFETY: called from the service main before this process spawns any thread —
                // the network-profile warner and the supervisor's host child both start after
                // `load_host_env` returns, so nothing reads the environment concurrently.
                unsafe { std::env::set_var(k, v) };
                n += 1;
            } else if !k.is_empty() {
                tracing::warn!(key = %k, "host.env: ignoring non-allow-listed key");
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
    tracing::info!(
        "punktfunk service started — supervising the host (active console session) and the web \
         console (session 0)"
    );

    // BEFORE the warner thread below: `load_host_env` mutates the process env, and the warner
    // spawns a child process (which snapshots the env block) — the write must not run beside it.
    load_host_env();

    // Best-effort: warn if this network is Public (streaming ports are firewalled off there unless
    // the operator opted in). Own thread — a slow `Get-NetConnectionProfile` never delays the host.
    std::thread::spawn(warn_if_public_network);
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
/// The web-console child rides along in every wait (`WebSlot::wait`), so it is (re)started and
/// reaped no matter which arm of the host state machine is currently blocking — without the host
/// logic knowing it exists.
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
    let job = make_job(JOB_OBJECT_LIMIT_BREAKAWAY_OK).context("create job object")?;

    // The web console child (session 0). Serviced transparently by every `web.wait` below, so it is
    // supervised no matter where the host state machine is — including while no one is logged in.
    let mut web = WebSlot::new(&exe);

    let mut restarts: u32 = 0;
    // One-shot latch for the update boot-loop rollback (plan U3.2) — a rollback that itself
    // fails must not spawn installers in a loop.
    let mut rollback_attempted = false;
    loop {
        if wait_one(stop, 0) {
            break;
        }
        // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and returns the active console session
        // id (or 0xFFFFFFFF); it passes no pointers, so the call is always sound.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == 0xFFFF_FFFF {
            // No interactive session yet (boot / fully logged out). Wait, but wake on stop/session.
            // The web console keeps being supervised meanwhile (`web.wait`) — a headless box needs
            // its management console precisely when no one is logged in.
            tracing::debug!("no active console session — waiting");
            if web.wait(&[stop, session_ev], 3000) == Some(0) {
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
                tracing::error!(
                    session,
                    "failed to launch host into the active console session: {e:#}"
                );
                if web.wait(&[stop], 3000).is_some() {
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

        // Wait on stop / session-change / child-exit (web-console exits are absorbed by `web.wait`).
        let reason = web.wait(&[stop, session_ev, proc_h], INFINITE);
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
                let r = web.wait(&[stop, proc_h], INFINITE);
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
                tracing::warn!(
                    pid = child.pid,
                    "host process exited on its own — relaunching"
                );
            }
        }

        restarts += 1;
        maybe_boot_loop_rollback(restarts, &mut rollback_attempted);
        let backoff = restarts.min(10) * 500; // 0.5s..5s
        if web.wait(&[stop], backoff).is_some() {
            break;
        }
        // `child` drops here (end of iteration) → its process + thread handles close before relaunch.
    }

    // `job` and `web` (each owning a KILL_ON_JOB_CLOSE job) drop at function exit, closing the job
    // objects → any straggler host or console process is reaped.
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

/// A kill-on-close job object with the given limit flags, returned as an `OwnedHandle`
/// (auto-`CloseHandle` on drop). The host child's job adds `BREAKAWAY_OK` (it must spawn the WGC
/// helper outside the job); the web console's does not — nothing it runs may outlive the service.
/// Safe: the handle it creates is wrapped in an `OwnedHandle` before any fallible step — so there
/// is no precondition for a caller to uphold and no way to leak it here.
fn make_job(limits: JOB_OBJECT_LIMIT) -> Result<OwnedHandle> {
    // SAFETY: a null security descriptor and a null name are both documented as "unnamed, default
    // security"; the returned handle is checked by `?` and owned on the next line.
    let job_raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObjectW")?;
    // Own it immediately so any early return (e.g. a failed SetInformationJobObject) still closes it.
    // SAFETY: `job_raw` is the handle just created, is non-null, and is not owned anywhere else —
    // ownership passes to the `OwnedHandle`, which closes it exactly once.
    let job = unsafe { OwnedHandle::from_raw_handle(job_raw.0) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | limits;
    // SAFETY: `job` is the live job object above; `info` is a local of exactly the type the
    // `JobObjectExtendedLimitInformation` class expects, and the size argument is its `size_of`.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("SetInformationJobObject")?;
    Ok(job)
}

/// The owned handles to a spawned host child. The `process`/`thread` `OwnedHandle`s auto-`CloseHandle`
/// when the `Child` drops (or is replaced each loop iteration) — replacing the manual
/// `CloseHandle(pi.hProcess/hThread)` the supervise loop used to scatter across its match arms.
struct Child {
    process: OwnedHandle,
    /// Held mainly for its RAII `CloseHandle`; only the web-console spawn reads it (to `ResumeThread`
    /// a CREATE_SUSPENDED child) — `_`-prefixed so the `dead_code` lint (CI's `-D warnings`) doesn't
    /// flag it on paths that never read it.
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
    // SAFETY: `GetCurrentProcess` returns the pseudo-handle for this process, which needs no close;
    // `proc_token` is a live local out-param that receives an owned handle on `Ok`, closed once below.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut proc_token,
        )
    }
    .context("OpenProcessToken (service must run as SYSTEM)")?;

    let mut primary = HANDLE::default();
    // SAFETY: `proc_token` is the live token just opened; `primary` is a live local out-param that
    // receives a second owned handle on `Ok`. Both are closed exactly once in this function.
    let dup = unsafe {
        DuplicateTokenEx(
            proc_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    // SAFETY: `proc_token` is live and owned here, closed exactly once and not used after.
    let _ = unsafe { CloseHandle(proc_token) };
    dup.context("DuplicateTokenEx(TokenPrimary)")?;

    // SAFETY: `primary` is the live duplicated token; the value pointer is a local `u32` matching
    // what `TokenSessionId` expects, and the length argument is exactly its `size_of`.
    unsafe {
        SetTokenInformation(
            primary,
            TokenSessionId,
            &session_id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .context("SetTokenInformation(TokenSessionId)")?;

    // 2) The session's environment block, merged with this process's PUNKTFUNK_*/RUST_LOG (so the
    //    host runs with host.env's settings, not a bare block). Same merge the interactive launch uses.
    let mut env_block: *mut c_void = std::ptr::null_mut();
    // SAFETY: `env_block` is a live local out-param and `primary` the live token above; on success
    // the call stores an owned block pointer, destroyed exactly once below.
    let _ = unsafe { CreateEnvironmentBlock(&mut env_block, Some(primary), false) };
    // SAFETY: `env_block` is either still null (the call above failed) or the double-null-terminated
    // UTF-16 block `CreateEnvironmentBlock` just wrote — exactly the two states the helper accepts.
    let merged = unsafe { crate::interactive::merged_env_block(env_block as *const u16) };
    if !env_block.is_null() {
        // SAFETY: `env_block` is the live block from the call above, destroyed exactly once and not
        // read after — `merged` owns its own copy of the parsed entries.
        let _ = unsafe { DestroyEnvironmentBlock(env_block) };
    }

    // 3) Redirect the host's stdout+stderr to host.log (inheritable handle). The previous child has
    //    exited by the time the supervise loop relaunches, so its handle can't be live here — safe
    //    to rotate. (A leaked orphan's handle lacks FILE_SHARE_DELETE, so the rename just fails.)
    let host_log = host_log_path();
    rotate_if_large(&host_log);
    let log = open_log_handle(&host_log)?;

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

    // SAFETY: `primary` is the live retargeted token; `cmd`, `desktop` (via `si.lpDesktop`),
    // `workdir` (via `cwd`) and `merged` are live for the call and NUL-terminated as the API
    // requires — `merged` doubly so, per `merged_env_block`. `si.hStdOutput`/`hStdError` are the
    // live inheritable `log` handle. `pi` is a live local out-param; no pointer is retained.
    let created = unsafe {
        CreateProcessAsUserW(
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
        )
    };

    // SAFETY: both are live and owned here, each closed exactly once and not used after — the child
    // holds its own inherited copy of `log`.
    unsafe {
        let _ = CloseHandle(log); // the child owns its inherited copy
        let _ = CloseHandle(primary);
    }
    created.context("CreateProcessAsUserW(host)")?;

    // Best-effort: keep the host inside the kill-on-close job.
    // SAFETY: `job` is a live job object per this fn's contract, and `pi.hProcess` is the live child
    // handle just created (`created` was `Ok`), still owned here.
    let _ = unsafe { AssignProcessToJobObject(job, pi.hProcess) };

    // Take ownership of the process + thread handles the API filled into `pi`; the returned `Child`
    // closes BOTH on drop, so the supervise loop no longer hand-closes them in its match arms.
    // SAFETY: `created` was `Ok`, so `pi.hProcess` is an owned handle nothing else closes; wrapping
    // it transfers that ownership to the `OwnedHandle`, which closes it exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    // SAFETY: the same, for the distinct thread handle `CreateProcessAsUserW` filled in.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };
    Ok(Child {
        process,
        _thread: thread,
        pid: pi.dwProcessId,
    })
}

/// Open `path` for appending, as an INHERITABLE handle (so the child can use it as stdout/stderr).
/// Safe: `path` is a `&Path` and every FFI argument is built locally from it. The returned `HANDLE`
/// is owned by the caller (it is inherited by the child and closed there), which is an ownership
/// obligation, not a safety one — nothing a caller can do here causes UB.
fn open_log_handle(path: &std::path::Path) -> Result<HANDLE> {
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
    // SAFETY: `wpath` is the locally built NUL-terminated UTF-16 path and `sa` a correctly sized
    // local `SECURITY_ATTRIBUTES`; both outlive the call, and the result is checked by `?`.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wpath.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&sa),
            OPEN_ALWAYS,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .context("CreateFileW(host.log)")?;
    Ok(h)
}

// ── web console child ────────────────────────────────────────────────────────────────────────────
//
// The web management console (bun/Nitro serving HTTPS on :47992) is the service's SECOND supervised
// child. It used to be a Task Scheduler task (`PunktfunkWeb`) and was found silently dead three
// times in one week under three different last-run codes (0x1 / 0xFFFFFFFF / 0x41306) — because a
// task's lifecycle is one best-effort start per boot/logon/install with no watchdog: Task Scheduler
// never restarts an action that merely exits non-zero, and nothing in the product checked the
// console was up. Full rationale: punktfunk-planning design/windows-web-console-lifecycle.md.
//
// Differences from the host child, all deliberate:
//   * plain session-0 spawn as self — NOT `spawn_host`, whose token retargeting would put this
//     LAN-facing server on the signed-in user's window station and tear it down on every session
//     switch. A web server has no business in the interactive session.
//   * its own job object WITHOUT `BREAKAWAY_OK`: the console spawns no helpers, so everything it
//     runs dies with the service, unconditionally.
//   * session changes never touch it.
//   * it never gives up: the respawn backoff doubles 0.5 s → 60 s and stays there. Any run that
//     served ≥ `WEB_GOOD_RUN` resets the backoff, so a console that ran for a while and died comes
//     back quickly, while a genuinely broken install retries (and logs) once a minute forever
//     rather than parking silently — the failure mode that produced all three incidents.

/// Stdout/stderr of the console child — `%ProgramData%\punktfunk\logs\web.log`. Bun's output used
/// to vanish into Task Scheduler; two of the three incidents were unattributable for exactly that
/// reason.
fn web_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("web.log")
}

/// A run at least this long is "good": it resets the consecutive-failure backoff.
const WEB_GOOD_RUN: Duration = Duration::from_secs(60);
/// Backoff ceiling between respawns of a failing console.
const WEB_MAX_BACKOFF_MS: u64 = 60_000;

/// The installed console payload ({app} = the directory this exe runs from).
struct WebConfig {
    bun: PathBuf,
    server: PathBuf,
    /// Working directory for the child ({app}\web, like the old launcher's `%~dp0`).
    web_dir: PathBuf,
}

/// The supervised web-console child slot. Owned by `supervise()`; serviced by `wait`, which every
/// host-loop wait goes through, so the console is managed no matter where the host state machine
/// currently blocks.
struct WebSlot {
    /// `None` = nothing to supervise on this box (payload absent, or opted out via host.env).
    cfg: Option<WebConfig>,
    child: Option<Child>,
    /// The console's own kill-on-close (no breakaway) job. Created at first spawn; dropping it —
    /// `supervise()` returning for any reason — reaps bun and anything it spawned.
    job: Option<OwnedHandle>,
    spawned_at: Instant,
    /// Consecutive short-lived runs (spawn failures count too); drives the backoff, nothing else.
    fast_exits: u32,
    next_spawn: Instant,
    /// The soft gate's deadline: `web-password` gets this long (from the moment the hard-gate files
    /// are all present) to appear before we start without it. Without a password the console
    /// fail-closes (admits no one), so starting is safe — this only avoids a pointless
    /// "auth not configured" window during a fresh install, where `web setup` writes the password
    /// around the same time the host writes its identity cert.
    password_deadline: Option<Instant>,
    /// One log line per waiting episode (gate files / missing payload), not one per poll.
    logged_wait: bool,
}

impl WebSlot {
    /// Decide what to supervise. host.env is already loaded into this process's environment
    /// (`load_host_env` runs before `supervise`), so the opt-out is a plain env check.
    fn new(exe: &Path) -> WebSlot {
        let app = exe.parent().unwrap_or(Path::new("."));
        let cfg = WebConfig {
            bun: app.join("bun").join("bun.exe"),
            server: app
                .join("web")
                .join(".output")
                .join("server")
                .join("index.mjs"),
            web_dir: app.join("web"),
        };
        let opted_out = std::env::var("PUNKTFUNK_WEB_CONSOLE").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        });
        let cfg = if opted_out {
            tracing::info!(
                "web console disabled (PUNKTFUNK_WEB_CONSOLE in host.env) — not supervising it"
            );
            None
        } else if !cfg.bun.exists() || !cfg.server.exists() {
            // A build without the web payload (host-only / scripting-only): nothing to do. A payload
            // appearing later always comes from an installer run, which restarts this service.
            tracing::info!(
                bun = %cfg.bun.display(),
                server = %cfg.server.display(),
                "no web console payload — not supervising it"
            );
            None
        } else {
            Some(cfg)
        };
        let now = Instant::now();
        WebSlot {
            cfg,
            child: None,
            job: None,
            spawned_at: now,
            fast_exits: 0,
            next_spawn: now,
            password_deadline: None,
            logged_wait: false,
        }
    }

    /// Wait on the caller's `handles` for up to `ms`, transparently servicing the console child:
    /// (re)spawning it when due and gated, and absorbing its exits. Returns the index of the
    /// signalled caller handle, or `None` on timeout — exactly `wait_any`'s contract, which is what
    /// lets the host loop use this everywhere without knowing the console exists.
    fn wait(&mut self, handles: &[HANDLE], ms: u32) -> Option<usize> {
        let deadline =
            (ms != INFINITE).then(|| Instant::now() + Duration::from_millis(u64::from(ms)));
        loop {
            let own_wake = self.converge();
            let mut set: Vec<HANDLE> = handles.to_vec();
            if let Some(c) = &self.child {
                set.push(HANDLE(c.process.as_raw_handle()));
            }
            let caller_ms = match deadline {
                None => INFINITE,
                Some(d) => d
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            };
            let wait_ms = caller_ms.min(own_wake.unwrap_or(INFINITE));
            match wait_any(&set, wait_ms) {
                Some(i) if i < handles.len() => return Some(i),
                Some(_) => self.on_child_exit(),
                None => {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return None;
                    }
                    // Our own wake (gate poll / respawn due) — loop into `converge`.
                }
            }
        }
    }

    /// Bring the slot toward "running": spawn the child if it is due and every gate passes.
    /// Returns how soon we want waking for slot-internal reasons (`None` = no appointment — either
    /// the child runs, and its handle is in the wait set, or there is nothing to supervise).
    fn converge(&mut self) -> Option<u32> {
        let Some(cfg) = &self.cfg else { return None };
        if self.child.is_some() {
            return None;
        }
        let now = Instant::now();
        if now < self.next_spawn {
            return Some(
                (self.next_spawn - now)
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            );
        }
        // An install/uninstall replacing or removing the payload mid-run: don't spawn into a
        // half-written {app}. The installer stops this service before touching files, so this is a
        // rare belt-and-braces path, polled slowly.
        if !cfg.bun.exists() || !cfg.server.exists() {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::warn!(
                    bun = %cfg.bun.display(),
                    "web console payload missing — an install/uninstall in progress? waiting"
                );
            }
            return Some(60_000);
        }
        // Hard gate: the host writes the mgmt token at argument-parse time and the identity
        // cert/key after its RSA keygen; the console needs all three (credential + TLS material),
        // so hold its start until they exist. This replaces the old launcher's 150-iteration wait
        // loop AND `web setup`'s start-then-verify — the supervisor is the parent of both children,
        // so it simply starts the console at the right moment.
        let data = pf_paths::config_dir();
        let token = data.join("mgmt-token");
        let cert = data.join("cert.pem");
        let key = data.join("key.pem");
        if !(token.exists() && cert.exists() && key.exists()) {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::info!(
                    "waiting for the host to write its mgmt token + identity cert before starting \
                     the web console"
                );
            }
            return Some(1_000);
        }
        // Soft gate: give `web setup` a moment to write the login password on a fresh install.
        let password = data.join("web-password");
        if !password.exists() {
            let deadline = *self
                .password_deadline
                .get_or_insert_with(|| now + Duration::from_secs(60));
            if now < deadline {
                return Some(1_000);
            }
            // No password after the grace period: start anyway — the console admits no one until
            // one exists (web/server/middleware/auth.ts fail-closes), and a respawn picks it up.
        }
        self.spawn(&data);
        // Either a child now runs (waited on by handle) or the failure scheduled `next_spawn`.
        self.child.is_none().then(|| {
            (self.next_spawn.saturating_duration_since(Instant::now()))
                .as_millis()
                .min(u128::from(INFINITE)) as u32
        })
    }

    /// One spawn attempt. On success the child is stored; on failure the backoff is scheduled.
    /// Borrows `data` (= the config dir) rather than recomputing it, purely to keep converge's gate
    /// checks and the env construction reading the same paths.
    fn spawn(&mut self, data: &Path) {
        let Some(cfg) = &self.cfg else { return };
        // Create the console's job lazily (first spawn) so a console-less box never makes one.
        if self.job.is_none() {
            match make_job(JOB_OBJECT_LIMIT(0)) {
                Ok(j) => self.job = Some(j),
                Err(e) => {
                    tracing::error!("create web console job object: {e:#}");
                    self.schedule_retry();
                    return;
                }
            }
        }
        let job = HANDLE(self.job.as_ref().expect("just set").as_raw_handle());
        match spawn_web(cfg, data, job) {
            Ok(child) => {
                tracing::info!(
                    pid = child.pid,
                    "web console launched (https://<host-ip>:47992)"
                );
                self.spawned_at = Instant::now();
                self.password_deadline = None;
                self.logged_wait = false;
                self.child = Some(child);
            }
            Err(e) => {
                tracing::error!("failed to launch the web console: {e:#}");
                self.schedule_retry();
            }
        }
    }

    /// The console child exited: log with its exit code + uptime, then schedule the respawn.
    fn on_child_exit(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let mut code: u32 = 0;
        // SAFETY: `child.process` is the live OwnedHandle of the just-signalled process (owned until
        // `child` drops at the end of this fn), and `code` is a live local out-param; the call reads
        // the handle and writes the u32, retaining neither.
        let _ = unsafe { GetExitCodeProcess(HANDLE(child.process.as_raw_handle()), &mut code) };
        let uptime = self.spawned_at.elapsed();
        if uptime >= WEB_GOOD_RUN {
            self.fast_exits = 0;
        }
        self.schedule_retry();
        tracing::warn!(
            pid = child.pid,
            exit_code = format!("{code:#x}"),
            uptime_secs = uptime.as_secs(),
            retry_in_ms = (self.next_spawn - Instant::now()).as_millis() as u64,
            "web console exited — relaunching"
        );
        // `child` drops here → its process/thread handles close.
    }

    /// Count a failure and set `next_spawn` per the doubling backoff (0.5 s → 60 s cap).
    fn schedule_retry(&mut self) {
        self.fast_exits = self.fast_exits.saturating_add(1);
        let backoff_ms = (500u64 << (self.fast_exits - 1).min(7)).min(WEB_MAX_BACKOFF_MS);
        self.next_spawn = Instant::now() + Duration::from_millis(backoff_ms);
    }
}

/// Read the value out of a one-line `KEY=VALUE` secret file (`mgmt-token` / `web-password` form; a
/// bare-value line is accepted too, matching `mgmt_token::parse_token`).
fn read_env_file_value(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Serialize this process's environment with `overrides` (which win, case-insensitively) into the
/// double-NUL-terminated UTF-16 block `CreateProcessW` takes. Same serialization as
/// `interactive::merged_env_block`; the base here is simply our own environment (host.env is
/// already loaded into it), not a user token's block — the console runs as us, in our session.
fn env_block_with(overrides: &[(&str, String)]) -> Vec<u16> {
    let mut entries: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !overrides.iter().any(|(ok, _)| ok.eq_ignore_ascii_case(k)))
        .collect();
    entries.extend(overrides.iter().map(|(k, v)| (k.to_string(), v.clone())));
    // CreateProcess* wants the block sorted case-insensitively (alphabetical by name).
    entries.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in entries {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// Launch the console: bun serving the Nitro bundle, session 0, stdout→web.log, inside `job`.
/// The deployment wiring (ports, TLS files, mgmt URL) matches what the deleted `web-run.cmd`
/// launcher set — and the Linux `scripts/punktfunk-web.service` unit still sets.
fn spawn_web(cfg: &WebConfig, data: &Path, job: HANDLE) -> Result<Child> {
    // Secrets resolve env-over-file, the same precedence the host itself uses (`mgmt_token.rs`):
    // an operator override in host.env must give both processes the same token.
    let token = std::env::var("PUNKTFUNK_MGMT_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-token")))
        .context("read mgmt-token")?;
    let password = std::env::var("PUNKTFUNK_UI_PASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("web-password")));
    // The mgmt URL resolves env-over-file too, for the same reason the token does — except the file
    // here is written by `mgmt::publish_endpoint` on every `serve`, so a host moved off 47990 (a
    // Sunshine fork owns that port as its web UI) carries the console with it instead of leaving it
    // proxying to a port nothing is listening on. The literal stays only as the last-resort default.
    let mgmt_url = std::env::var("PUNKTFUNK_MGMT_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-endpoint")))
        .unwrap_or_else(|| "https://127.0.0.1:47990".into());

    let mut overrides: Vec<(&str, String)> = vec![
        ("PORT", "47992".into()),
        ("HOST", "0.0.0.0".into()),
        // The /api proxy hop to the host's loopback HTTPS mgmt API. The host's self-signed cert is
        // accepted only inside the proxy code (per-request TLS), never process-wide.
        ("PUNKTFUNK_MGMT_URL", mgmt_url),
        // Serve HTTPS with the host's own identity cert; mark the session cookie Secure. Names the
        // LEGACY pair — the console prefers the native sibling when it exists
        // (web/nitro-entry/tls-paths.mjs), which is also what the gate above ends up waiting for:
        // `serve` resolves the native identity before minting this one.
        (
            "PUNKTFUNK_UI_TLS_CERT",
            data.join("cert.pem").to_string_lossy().into_owned(),
        ),
        (
            "PUNKTFUNK_UI_TLS_KEY",
            data.join("key.pem").to_string_lossy().into_owned(),
        ),
        ("PUNKTFUNK_UI_SECURE", "1".into()),
        ("PUNKTFUNK_MGMT_TOKEN", token),
    ];
    if let Some(pw) = password {
        overrides.push(("PUNKTFUNK_UI_PASSWORD", pw));
    }
    let env = env_block_with(&overrides);

    let web_log = web_log_path();
    rotate_if_large(&web_log);
    let log = open_log_handle(&web_log)?;

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log,
        hStdError: log,
        ..Default::default()
    };
    let mut cmd: Vec<u16> = format!("\"{}\" \"{}\"", cfg.bun.display(), cfg.server.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd: Vec<u16> = cfg
        .web_dir
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut pi = PROCESS_INFORMATION::default();

    // CREATE_SUSPENDED: the child must be inside the kill-on-close job BEFORE its first instruction
    // — anything it spawned pre-assignment would escape the job and outlive the service.
    // SAFETY: `cmd`, `cwd`, `env` and `si` (whose handles are the live inheritable `log`) are live,
    // NUL-terminated locals for the duration of the call (`env` doubly, per `env_block_with`); `pi`
    // is a live local out-param; no pointer is retained past the call.
    let created = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true, // inherit the log handle
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            Some(env.as_ptr() as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &si,
            &mut pi,
        )
    };
    // SAFETY: `log` is live and owned here, closed exactly once and not used after — on success the
    // child holds its own inherited copy.
    let _ = unsafe { CloseHandle(log) };
    created.context("CreateProcessW(web console)")?;

    // Take ownership of the (suspended) process + thread handles first, so every path below —
    // including the assignment failure — closes them via drop.
    // SAFETY: `created` was `Ok`, so `pi.hProcess` is an owned handle nothing else closes; wrapping
    // it transfers that ownership to the `OwnedHandle`, which closes it exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    // SAFETY: the same, for the distinct thread handle `CreateProcessW` filled in.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };
    let child = Child {
        process,
        _thread: thread,
        pid: pi.dwProcessId,
    };

    // NOT best-effort (unlike the host child, whose job is breakaway-ok anyway): containment is the
    // point of this job, so a child we cannot assign is a child we do not run.
    // SAFETY: `job` is a live job object per this fn's contract; `child.process` is the live handle
    // of the still-suspended process just created.
    if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(child.process.as_raw_handle())) }
    {
        // SAFETY: `child.process` is live and owned (dropped at the end of this scope);
        // TerminateProcess only signals termination by handle. The process never ran (suspended).
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        return Err(e).context("AssignProcessToJobObject(web console)");
    }
    // SAFETY: `child._thread` is the live primary-thread handle of the process just created; a
    // suspended primary thread is exactly what ResumeThread expects.
    let resumed = unsafe { ResumeThread(HANDLE(child._thread.as_raw_handle())) };
    if resumed == u32::MAX {
        // SAFETY: as in the assignment-failure arm above — live owned handle, process torn down.
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        bail!("ResumeThread(web console) failed");
    }
    Ok(child)
}

// ── install / uninstall ──────────────────────────────────────────────────────────────────────────

fn install(args: &[String]) -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
        ServiceType,
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

    // SCM crash recovery: restart at 1 s / 5 s, then every 60 s (the SCM repeats the last action);
    // the failure counter resets after a clean day. The web console lives and dies with this
    // process now, so this policy is the outer safety net for BOTH children. It fires only when the
    // service process dies without reporting SERVICE_STOPPED — a crash — never on a deliberate
    // stop. Best-effort: a recovery policy is a nicety an install must not fail over.
    // (Restart actions need SERVICE_START on the handle, hence the fresh open.)
    let recovery = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )
        .and_then(|svc| {
            svc.update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(1),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(60),
                    },
                ]),
            })
        });
    match recovery {
        Ok(()) => println!("Crash recovery: the SCM restarts the service at 1s/5s/60s."),
        Err(e) => eprintln!("warning: could not set the service recovery actions: {e}"),
    }

    ensure_default_host_env()?;
    if let Some(on) = gamestream {
        apply_gamestream_choice(on);
    }
    // Firewall scope: Domain + Private by default; `--allow-public-network` opts into Public too.
    // Persist the choice (so the startup warning respects an opt-in) and re-scope idempotently —
    // remove any prior rules first so an upgrade tightens the scope instead of leaving a stale
    // all-profiles rule behind the new one. With the flag absent (every upgrade) this resolves to
    // the previously recorded choice, so the rewrite below is a no-op rather than a silent reset.
    let allow_public = allow_public_network(args)?;
    set_fw_public_marker(allow_public);
    remove_firewall_rules();
    add_firewall_rules(allow_public);

    println!(
        "\nInstalled. Config: {}\nLogs:   {}\n\nStart now with:  punktfunk-host service start",
        host_env_path().display(),
        pf_paths::config_dir().join("logs").display()
    );
    Ok(())
}

/// `service restart`: stop, wait for the service to actually reach Stopped (a bare
/// `sc stop && sc start` races the stop — START fails with "instance already running" while the
/// old process winds down), then start. The tray icon's Restart action runs this, elevated.
fn restart() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .context("open service (run elevated)")?;
    // Best-effort stop: ERROR_SERVICE_NOT_ACTIVE just means restart == start.
    let _ = svc.stop();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = svc.query_status().context("query service status")?;
        if state.current_state == ServiceState::Stopped {
            break;
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("service did not stop within 30 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    svc.start(&[] as &[&std::ffi::OsStr])
        .context("start service")?;
    println!("Restarted service '{SERVICE_NAME}'.");
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
    // If a host.env already exists but is owned by a NON-admin account, it was pre-created by an
    // unprivileged user before this privileged install ran (`%ProgramData%` grants BUILTIN\Users
    // add-subdirectory + CREATOR OWNER). Its bytes become the SYSTEM service's environment and the
    // command line it launches, so it must NOT be adopted verbatim. Checked HERE, before
    // `create_private_dir` below re-owns it to Administrators and erases the only signal that
    // distinguishes a planted file from a legitimately-provisioned one. A legitimate host.env from a
    // prior privileged install is Administrators-owned and passes. security-review 2026-08-15 #3c.
    let planted = path.exists() && crate::install::is_admin_owned(&path) == Some(false);
    if planted {
        // Best-effort rename-aside for forensics; the security guarantee is the `!planted` skip
        // below, which drops through to overwriting the file with the default even if this fails.
        let mut aside = path.clone().into_os_string();
        aside.push(".untrusted");
        let aside = std::path::PathBuf::from(aside);
        let _ = std::fs::remove_file(&aside);
        match std::fs::rename(&path, &aside) {
            Ok(()) => tracing::warn!(
                path = %path.display(), aside = %aside.display(),
                "host.env was owned by a non-admin account (planted before install) — renamed aside; writing the default"
            ),
            Err(e) => tracing::error!(
                error = %e, path = %path.display(),
                "host.env is non-admin-owned and could not be renamed aside — overwriting it with the default"
            ),
        }
    }
    // Harden the config dir FIRST, unconditionally — before the `exists()` check, not inside the
    // branch that creates the file.
    //
    // The 2026-08-05 review's H-4: this used to return early when host.env already existed, which
    // skipped the very `create_private_dir` whose reason for existing is "so a local user can't
    // pre-create it and plant a host.env". `C:\ProgramData` grants BUILTIN\Users add-subdirectory
    // plus CREATOR OWNER full control, so an unprivileged user can create `C:\ProgramData\punktfunk`,
    // own it, and drop a host.env — and the skip meant the one case the hardening was written for was
    // the one case it never ran in. The service then loads that file verbatim into its own SYSTEM
    // environment and into the command line it launches (`PUNKTFUNK_HOST_CMD=…`).
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir).ok();
    }
    if path.exists() && !planted {
        // An existing host.env may predate the hardening (or have been planted before it ran), in
        // which case it is still owned by whoever created it — and an owner can rewrite the DACL it
        // inherited. Re-apply the SYSTEM/Administrators lock to the FILE as well as the directory.
        // (A non-admin-owned file is `planted` above and is NOT adopted — it falls through to the
        // default write below, overwriting it even if the rename-aside failed.)
        pf_paths::restrict_existing_secret_file(&path);
        return Ok(());
    }
    let default = "# punktfunk host configuration (read by the Windows service).\n\
        # KEY=VALUE per line; '#' comments. Restart the service after editing:\n\
        #   punktfunk-host service stop && punktfunk-host service start\n\
        \n\
        # Encode backend: auto (default) detects the GPU vendor — NVIDIA->nvenc, AMD->amf, Intel->qsv.\n\
        # Force one with nvenc | amf | qsv | sw (software H.264). amf/qsv need an FFmpeg-built host.\n\
        PUNKTFUNK_ENCODER=auto\n\
        PUNKTFUNK_VIDEO_SOURCE=virtual\n\
        # Virtual display = the bundled pf-vdisplay driver; capture is IDD-push from its shared ring\n\
        # (the sole capture path — zero-copy; DDA/WGC were removed). The secure desktop (UAC / lock /\n\
        # login) is always captured — there is no setting for it.\n\
        PUNKTFUNK_VDISPLAY=pf\n\
        RUST_LOG=info\n\
        \n\
        # The host subcommand the service launches (default: serve --gamestream = native + Moonlight\n\
        # compat). Use `serve` for a SECURE native-only host (no GameStream #5/#9 surface).\n\
        # PUNKTFUNK_HOST_CMD=serve --gamestream\n\
        \n\
        # The web management console (https://<this-PC>:47992) runs as a child of the service.\n\
        # Set to off to disable it:\n\
        # PUNKTFUNK_WEB_CONSOLE=off\n\
        \n\
        # Force a specific render GPU by name substring (multi-GPU boxes only):\n\
        # PUNKTFUNK_RENDER_ADAPTER=4090\n\
        \n\
        # The name this host shows up under in Moonlight and the Punktfunk clients\n\
        # (default: the machine's own computer name):\n\
        # PUNKTFUNK_HOST_NAME=Living Room\n";
    // Write host.env DACL-locked to SYSTEM/Administrators: it controls the SYSTEM service's
    // environment + launched command line, so a local user must not be able to read or tamper with
    // it (security-review 2026-06-28 #3).
    pf_paths::write_secret_file(&path, default.as_bytes())
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
    if let Err(e) = pf_paths::write_secret_file(&path, out.as_bytes()) {
        eprintln!("warning: could not write {}: {e}", path.display());
        return;
    }
    println!(
        "GameStream (Moonlight) compatibility: {} (PUNKTFUNK_HOST_CMD={desired})",
        if enable { "enabled" } else { "disabled" }
    );
}

// ── firewall + sc helpers ────────────────────────────────────────────────────────────────────────

/// The `netsh` `profile=` scope for punktfunk's inbound rules. Default = **Domain + Private** — the
/// trusted-network profiles punktfunk is meant to run on; `allow_public` widens it to **all profiles
/// including Public** (untrusted networks like café/hotel Wi-Fi — opt-in only). Shared with the
/// web-console rule in `install.rs` so both surfaces scope the same way.
pub(crate) fn firewall_profile_arg(allow_public: bool) -> &'static str {
    if allow_public {
        "profile=any"
    } else {
        "profile=domain,private"
    }
}

/// Resolve the Public-network firewall scope for this install/upgrade (the installer's "Allow
/// connections on Public networks" task forwards the flag).
///
/// Tri-state, mirroring `--gamestream=on|off`:
///   * `--allow-public-network` or `=on`  -> `true`  — explicit opt-in. The bare form is kept for
///     back-compat with existing scripts and docs that pass it that way.
///   * `--allow-public-network=off`       -> `false` — explicit opt-out.
///   * absent                             -> whatever the previous install recorded.
///
/// The absent case is what makes an UPGRADE non-destructive. A silent upgrade (`winget upgrade`)
/// has no wizard to carry the old checkbox forward, so the installer omits the flag entirely on an
/// upgrade and the recorded choice stands. Re-applying the task default instead would quietly
/// re-scope the firewall on every upgrade, undoing an opt-in the user made once. On a first-ever
/// install there is no marker, so absence resolves to the secure default (Domain + Private only).
///
/// Strict on the value: a typo'd `=of` must NOT fall through to the marker, because the marker may
/// say `true` — i.e. a mistyped opt-OUT would silently leave Public open.
pub(crate) fn allow_public_network(args: &[String]) -> Result<bool> {
    for a in args {
        if let Some(v) = a.strip_prefix("--allow-public-network") {
            return match v {
                "" | "=on" => Ok(true),
                "=off" => Ok(false),
                _ => bail!(
                    "--allow-public-network must be 'on' or 'off' (got '{}')",
                    v.trim_start_matches('=')
                ),
            };
        }
    }
    Ok(fw_public_marker().exists())
}

/// Build the `netsh advfirewall firewall add rule` argument vector for one inbound allow rule.
///
/// `program` is the whole point of this helper existing. A `dir=in action=allow` rule carrying only
/// `localport=` admits **any process on the machine** on those ports, and binding a high port on
/// Windows needs no elevation — so such a rule is a standing hole that any unprivileged program can
/// step into simply by binding first, and it does so *silently*, because our rule is exactly what
/// suppresses the "Allow this app to communicate on…" prompt Windows would otherwise raise (that
/// prompt is the UAC gate; without a matching rule there is no way in without one). Naming the
/// owning executable keeps the ports open for punktfunk and no one else. Reported by a user on
/// 2026-08-21, and correct: the fixed rules were the last any-program ones we shipped.
///
/// `ports` stays alongside it rather than being replaced by it — program AND port is strictly
/// tighter than either alone, and it is only ever dropped where the port genuinely cannot be known
/// in advance ([`add_data_plane_firewall_rule`], whose port is ephemeral per session).
///
/// `None` for `program` reproduces the old any-program rule, and every caller falls back to it
/// rather than skipping the rule when it cannot resolve its executable: a looser rule still streams,
/// no rule at all is a black screen.
pub(crate) fn fw_add_rule_args(
    name: &str,
    proto: &str,
    ports: Option<&str>,
    program: Option<&std::path::Path>,
    profile: &str,
) -> Vec<String> {
    let mut args: Vec<String> = ["advfirewall", "firewall", "add", "rule"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(format!("name={name}"));
    args.push("dir=in".into());
    args.push("action=allow".into());
    args.push(format!("protocol={proto}"));
    if let Some(p) = ports {
        args.push(format!("localport={p}"));
    }
    if let Some(exe) = program {
        args.push(format!("program={}", exe.display()));
    }
    args.push(profile.to_string());
    args
}

/// [`run_quiet`] for an arg vector built by [`fw_add_rule_args`].
pub(crate) fn run_netsh(args: &[String]) -> bool {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_quiet("netsh", &borrowed)
}

/// Inbound firewall rules for the streaming + mgmt ports (best-effort; logs but never fails the
/// install). Scoped by [`firewall_profile_arg`]: Domain + Private by default, all profiles when
/// `allow_public`, and — since 2026-08-21 — to this host executable, so the ports below are open to
/// punktfunk rather than to anything on the machine that binds them first (see
/// [`fw_add_rule_args`]). TCP 47990 is deliberate: `serve` binds the mgmt/library REST API to all
/// interfaces so paired clients can browse the game library over mTLS, and off-loopback
/// `mgmt::require_auth` exposes only the read-only status/library allowlist to a paired client cert
/// — the bearer-token admin surface stays loopback-only regardless of the bind — so opening it adds
/// no admin exposure.
fn add_firewall_rules(allow_public: bool) {
    let profile = firewall_profile_arg(allow_public);
    // Resolved once and shared with the data-plane rule below. `service install` re-runs this whole
    // remove-then-add on every upgrade, so a path recorded here cannot go stale behind a moved
    // install — which is what previously argued for leaving these rules unscoped.
    let exe = match std::env::current_exe() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!(
                "warning: could not resolve the host executable path ({e}) — the rules below stay \
                 open to any program on those ports, and the per-session data-plane rule is skipped"
            );
            None
        }
    };
    // (name suffix, protocol, ports). 47990 = mgmt/library (LAN = read-only, paired-cert only); the
    // rest are the GameStream (47984/47989/48010, 47998-48010) + native (9777) + mDNS (5353) ports.
    let rules = [
        ("TCP", "TCP", "47984,47989,48010,47990"),
        ("UDP", "UDP", "47998-48010,9777,5353"),
    ];
    for (suffix, proto, ports) in rules {
        let name = format!("Punktfunk {suffix}");
        let ok = run_netsh(&fw_add_rule_args(
            &name,
            proto,
            Some(ports),
            exe.as_deref(),
            profile,
        ));
        if ok {
            let scope = match &exe {
                Some(p) => format!(" for {}", p.display()),
                None => String::new(),
            };
            println!("Firewall rule added: {name} ({ports}{scope}) [{profile}]");
        } else {
            eprintln!("warning: could not add firewall rule '{name}' (add it manually if needed)");
        }
    }
    add_data_plane_firewall_rule(profile, exe.as_deref());
    // 5353 is now ours alone. Anything else on this machine that answered mDNS through the old
    // any-program rule needs its own — say so, because it is the one externally visible change.
    // Only when the scoping actually happened: with no exe path these rules are still wide open,
    // and claiming otherwise in installer output is worse than saying nothing.
    if exe.is_some() {
        println!(
            "Note: these rules are scoped to the punktfunk host executable, so they no longer open \
             those ports to every program on this machine. Another mDNS/GameStream application \
             that relied on punktfunk's rules to be reachable now needs a rule of its own."
        );
    }
    if !allow_public {
        println!(
            "Note: streaming ports are open on Private/Domain networks only. On a network Windows \
             classifies as Public, clients won't connect — set that network to Private, or reinstall \
             with the 'Allow connections on Public networks' option."
        );
    }
}

/// Rule name for the program-scoped data-plane rule (see [`add_data_plane_firewall_rule`]).
const FW_DATA_PLANE_RULE: &str = "Punktfunk UDP (data plane)";

/// Inbound UDP for the host executable itself, at **any** local port.
///
/// The media data plane binds an EPHEMERAL port per session (`0.0.0.0:0`, reported to the client in
/// the Welcome), so no `localport=` rule can cover it — the port-scoped rules above open the fixed
/// control/GameStream/mDNS ports and nothing else. Without this, Windows Firewall drops the client's
/// hole-punch (`PUNCH_MAGIC` → the host's data port) on EVERY session: that is what `punched=false`
/// on the host's "data plane bound" line means. The punch then never opens the return path, video
/// falls back to blind-sending at the address the client merely *reported*, and the moment anything
/// on the path needs the flow opened client-first the stream goes black while the control plane
/// stays healthy — no reconnect, no error, just a session that never shows a picture.
///
/// Program-scoped rather than a pinned port: it covers whatever port the session picks, needs no
/// second rule when the range moves, and cannot collide with another host (a pinned data port in
/// 47998-48010 would land on Sunshine/Apollo's GameStream range). This rule is the pattern the
/// fixed-port rules above now follow too — it is only the `localport=` they keep and this one
/// cannot have.
///
/// `exe` is resolved once by the caller and shared; `None` means it could not be resolved, and this
/// rule is skipped rather than widened, because a program-less "any inbound UDP on any port" rule is
/// not a looser version of this — it is an open host.
fn add_data_plane_firewall_rule(profile: &str, exe: Option<&std::path::Path>) {
    let Some(exe) = exe else {
        eprintln!(
            "warning: no host executable path — skipping the data-plane firewall rule; streams may \
             show a black picture behind a healthy connection on networks that need the client's \
             hole-punch to open the path"
        );
        return;
    };
    let ok = run_netsh(&fw_add_rule_args(
        FW_DATA_PLANE_RULE,
        "UDP",
        None,
        Some(exe),
        profile,
    ));
    if ok {
        println!(
            "Firewall rule added: {FW_DATA_PLANE_RULE} (any UDP port for {}) [{profile}]",
            exe.display()
        );
    } else {
        eprintln!(
            "warning: could not add firewall rule '{FW_DATA_PLANE_RULE}' — the per-session video \
             data port stays closed to inbound, so the client's hole-punch cannot reach it"
        );
    }
}

fn remove_firewall_rules() {
    let _ = run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FW_DATA_PLANE_RULE}"),
        ],
    );
    for suffix in ["TCP", "UDP"] {
        // Capital P is the brand; netsh matches a rule name case-INSENSITIVELY, so this still
        // reaps the lowercase rules every release up to 0.22.1 created — no orphans on upgrade.
        let name = format!("Punktfunk {suffix}");
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

/// Marker file recording that the operator opted into opening the firewall on **Public** networks
/// (`--allow-public-network`). Its presence suppresses the startup Public-network warning (they made
/// an informed choice); absence = the secure default.
fn fw_public_marker() -> std::path::PathBuf {
    pf_paths::config_dir().join("fw-allow-public")
}

/// Record (or clear) the Public-firewall opt-in marker to match this install's choice.
fn set_fw_public_marker(allow_public: bool) {
    let path = fw_public_marker();
    if allow_public {
        let _ = std::fs::write(&path, b"1\n");
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Best-effort: is any active network connection classified **Public** by Windows? Uses
/// `Get-NetConnectionProfile` (per-interface category: Public / Private / DomainAuthenticated).
/// `None` when it can't be determined — the caller then skips the warning.
fn active_network_is_public() -> Option<bool> {
    // Resolve powershell by full System32 path — this runs in the SYSTEM service, which must not trust
    // PATH / the CreateProcess search (it checks the launching EXE's OWN directory first, so a planted
    // `powershell.exe` next to the host binary would run as SYSTEM). Matches the pf_vdisplay resolver
    // and the "a privileged service must not trust PATH" rule (security-review 2026-07-17).
    let ps = std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|_| "powershell.exe".to_string());
    let out = std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-NetConnectionProfile).NetworkCategory",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.lines().any(|l| l.trim().eq_ignore_ascii_case("Public")))
}

/// One-shot startup diagnostic: if the operator did NOT opt into Public networks and this machine's
/// current network is classified Public, the streaming ports are firewalled off there — turn that
/// silent "clients can't connect" into an actionable WARN. Best-effort; meant to run on its own
/// thread so it never delays the host launch.
fn warn_if_public_network() {
    if fw_public_marker().exists() {
        return; // operator opted into Public — their informed choice, no warning
    }
    if active_network_is_public() == Some(true) {
        tracing::warn!(
            "this machine's current network is classified Public (an untrusted-network profile), so \
             punktfunk's streaming ports are firewalled off here and clients on this network can't \
             reach the host. Fix: set the network to Private (Windows Settings > Network > \
             properties) — or, only for a network you trust, reinstall with the 'Allow connections \
             on Public networks' option."
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

/// Update boot-loop rollback (planning: host-update-from-web-console.md §6, plan U3.2): a
/// FRESH update-intent record plus a crash-looping child that IS the intent's target version
/// means the just-installed host doesn't stay up. The supervisor — which survives the upgrade,
/// making it the right place — rolls back by re-running the CACHED previous installer: once
/// per intent, Authenticode-checked, outcome written where the console reads it. The service
/// process is not inside the kill-on-close job, so the spawned installer survives the service
/// stop it is about to perform.
fn maybe_boot_loop_rollback(restarts: u32, attempted: &mut bool) {
    if *attempted || restarts < 3 {
        return;
    }
    let intent_path = crate::update::jobs::intent_path();
    let Some(intent) = crate::update::jobs::read_intent(&intent_path) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // A stale intent never triggers rollbacks, and a boot-looping OLD binary (version ≠ the
    // intent's target) is not an update failure — boot reconciliation owns that story.
    if now.saturating_sub(intent.started_unix) > 30 * 60 || env!("PUNKTFUNK_VERSION") != intent.to {
        return;
    }
    *attempted = true;

    let updates = pf_paths::config_dir().join("updates");
    let previous = std::fs::read_dir(&updates)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with("punktfunk-host-setup-")
                        && n.ends_with(".exe")
                        && !n.contains(intent.to.as_str())
                })
                .unwrap_or(false)
        })
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
    let Some(previous) = previous else {
        tracing::error!(
            to = %intent.to,
            "updated host is crash-looping and no cached previous installer exists — \
             leaving the intent for reconcile; manual reinstall required"
        );
        return;
    };
    // The cached file was hash/publisher-verified when downloaded and is protected by the config
    // DACL. With no manifest here, this re-check can prove only signature integrity.
    if let Err(e) = crate::update::windows::verify_authenticode(&previous, &[], None) {
        tracing::error!(
            installer = %previous.display(),
            error = %e,
            "cached previous installer fails its signature check — not rolling back"
        );
        return;
    }

    let log = pf_paths::config_dir()
        .join("logs")
        .join(format!("update-rollback-from-{}.log", intent.to));
    let record = crate::update::jobs::ResultRecord {
        ok: false,
        from: intent.from.clone(),
        to: intent.to.clone(),
        finished_unix: now,
        stage: Some("rolled-back".into()),
        error: Some(format!(
            "the updated host crash-looped after install; rolled back via {}",
            previous.display()
        )),
        log_path: Some(log.display().to_string()),
        staged: false,
    };
    let _ = crate::update::jobs::write_json_atomic(&crate::update::jobs::result_path(), &record);
    // Delete the intent BEFORE spawning: the incoming (previous) host must boot clean, and a
    // missing intent keeps this one-shot even across a service restart.
    let _ = std::fs::remove_file(&intent_path);

    tracing::warn!(
        failed_version = %intent.to,
        back_to = %previous.display(),
        "updated host is crash-looping — rolling back via the cached previous installer"
    );
    match std::process::Command::new(&previous)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
        .arg(format!("/LOG={}", log.display()))
        .spawn()
    {
        // Detached on purpose: it stops this service and reinstalls the previous version.
        Ok(child) => drop(child),
        Err(e) => tracing::error!(error = %e, "failed to spawn the rollback installer"),
    }
}

#[cfg(test)]
mod firewall_tests {
    use super::*;
    use std::path::Path;

    /// Every fixed-port rule must carry BOTH `program=` and `localport=`. Dropping the program
    /// scope is the regression that matters: the rule still works, streaming still works, and the
    /// only visible difference is that any unprivileged process on the machine can bind those
    /// ports and be reachable from the LAN without ever raising a Windows prompt.
    #[test]
    fn fixed_port_rules_are_scoped_to_the_program_and_the_ports() {
        let exe = Path::new(r"C:\Program Files\Punktfunk\punktfunk-host.exe");
        let args = fw_add_rule_args(
            "Punktfunk UDP",
            "UDP",
            Some("47998-48010,9777,5353"),
            Some(exe),
            "profile=domain,private",
        );
        assert!(args.contains(&format!("program={}", exe.display())));
        assert!(args.contains(&"localport=47998-48010,9777,5353".to_string()));
        assert!(args.contains(&"dir=in".to_string()));
        assert!(args.contains(&"action=allow".to_string()));
        assert!(args.contains(&"profile=domain,private".to_string()));
        assert_eq!(&args[..4], &["advfirewall", "firewall", "add", "rule"]);
    }

    /// The data plane is the one rule that legitimately has no port: its socket binds `0.0.0.0:0`
    /// per session. It must therefore never lose its program scope — a program-less "any inbound
    /// UDP on any port" rule is not a looser version of this rule, it is an open host.
    #[test]
    fn the_data_plane_rule_has_a_program_but_no_port() {
        let exe = Path::new(r"C:\Program Files\Punktfunk\punktfunk-host.exe");
        let args = fw_add_rule_args(FW_DATA_PLANE_RULE, "UDP", None, Some(exe), "profile=any");
        assert!(args.contains(&format!("program={}", exe.display())));
        assert!(
            !args.iter().any(|a| a.starts_with("localport=")),
            "the per-session data port is ephemeral — pinning one would close the others"
        );
    }

    /// An unresolvable executable falls back to the old any-program rule rather than to no rule:
    /// a looser rule still streams, a missing one is a black screen. Pinned so the fallback stays
    /// deliberate rather than becoming an accident.
    #[test]
    fn a_missing_program_falls_back_to_the_port_only_rule() {
        let args = fw_add_rule_args("Punktfunk TCP", "TCP", Some("47990"), None, "profile=any");
        assert!(!args.iter().any(|a| a.starts_with("program=")));
        assert!(args.contains(&"localport=47990".to_string()));
    }
}
