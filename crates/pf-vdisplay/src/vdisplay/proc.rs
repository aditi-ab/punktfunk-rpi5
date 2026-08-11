//! Time-bounded child-process helpers.
//!
//! Every compositor query this crate makes shells out to a helper (`kscreen-doctor`, `systemctl`,
//! `pw-dump`, …), and most of them are *clients of the very thing being diagnosed*: `kscreen-doctor`
//! is a Wayland client, so against a wedged KWin it blocks in its own connect and **never returns**.
//! `Command::status()` / `Command::output()` have no timeout, so one hung helper pinned the calling
//! thread forever — and on the host that thread is the session's stream thread, whose only way to
//! end a session is to return. A stuck query therefore became a permanently stuck session.
//!
//! These wrappers bound the wait: poll for exit until the budget runs out, then kill the child and
//! report [`std::io::ErrorKind::TimedOut`], so callers see a plain "the helper failed" error and
//! take their existing failure path instead of hanging.
//!
//! What the budget bounds is the whole **process tree**, not just the process we spawned — see
//! [`tree`] for why that distinction is the entire difference on Windows, and for the one Unix
//! case (a unit the *user manager* forks for us) that even a process group cannot reach.

// `Read` is in scope for `Take::read_to_end` below — a `Take<R>` is a concrete type, so the
// generic bound alone does not bring the trait's methods with it.
use std::io::{Error, ErrorKind, Read, Result};
use std::process::{Command, ExitStatus, Output};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Poll interval while waiting for a child to exit. Short enough that a fast helper (the normal
/// case — `kscreen-doctor` answers in tens of ms) isn't measurably delayed.
const POLL: Duration = Duration::from_millis(20);

/// Ceiling on how long [`output_within`] waits for its two reader threads once the child **and the
/// process group under it** are dead.
///
/// This is not a working budget — with every write end we can reach closed, the readers hit EOF
/// within a scheduler slice — it is the bound on the one case we cannot reach. [`tree`] ends a
/// *group*, so a descendant that deliberately left it keeps the write end open: `systemd-run
/// --pipe` (the gamescope bind probe) hands our pipes to a transient unit the **user manager**
/// forks, in its own group and session, and `killpg` by construction cannot touch it. Waiting on
/// that reader would pin the caller — on the host, the session's stream thread — for as long as
/// the unit lives, which is exactly the unbounded wait this module exists to prevent. So the
/// *call* is bounded here and the reader thread, not the call, is what gets left behind. The
/// price, paid only in that case, is that a call can return up to this much after its own budget —
/// still a bound, which an unreachable EOF is not.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Ceiling on what one drained pipe may buffer.
///
/// `read_to_end` is unbounded in memory, and a reader thread that outlived its call (see
/// [`DRAIN_GRACE`]) has nobody left to stop it — the cap is what keeps such a thread finite in
/// both memory and lifetime, and closing its read end is also what finally gives the escaped
/// writer an EPIPE. 16 MiB is an order of magnitude above the largest `pw-dump` a populated
/// PipeWire graph produces, so hitting it means a helper that ran away rather than one that was
/// busy; it is logged instead of being returned as quietly short output.
const DRAIN_CAP: u64 = 16 * 1024 * 1024;

/// Run `cmd` to completion, killing it if it outlives `budget`.
///
/// Stdout/stderr are left as the caller configured them (inherited by default), so this is for
/// commands run for their exit status alone — see [`output_within`] when the output is read.
pub(crate) fn status_within(cmd: &mut Command, budget: Duration) -> Result<ExitStatus> {
    tree::prepare(cmd);
    let mut child = cmd.spawn()?;
    let tree = tree::Guard::attach(&child);
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait()? {
            Some(status) => {
                // The helper is gone; anything it left running is not something the caller asked
                // for and still holds the stdio it inherited from us.
                tree.terminate();
                return Ok(status);
            }
            None if Instant::now() >= deadline => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait(); // reap it — never leave a zombie behind
                return Err(timed_out(cmd, budget));
            }
            None => std::thread::sleep(POLL),
        }
    }
}

/// Run `cmd` to completion and capture its stdout/stderr, killing it if it outlives `budget`.
///
/// Both pipes are drained **concurrently with the wait**, on their own threads. Reading them only
/// after exit — the obvious shape, and what this did originally — deadlocks on any helper that
/// outtalks the pipe buffer: a pipe holds **64 KiB** on Linux (`/proc/sys/fs/pipe-max-size`'s page
/// default), not the "few hundred KiB" the old comment claimed, so a chatty helper blocks in
/// `write()`, never reaches exit, is killed at the budget, and its output is discarded as a
/// timeout. `pw-dump` on a populated PipeWire graph clears 64 KiB routinely, and it is polled from
/// the 45 s gamescope loops — so the failure was not hypothetical, it was the busiest caller.
pub(crate) fn output_within(cmd: &mut Command, budget: Duration) -> Result<Output> {
    tree::prepare(cmd);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let tree = tree::Guard::attach(&child);
    // Taken off the `Child` so the reader threads own them outright: `wait_with_output` must not
    // also be reading these, and `try_wait` below needs `&mut child` while they run.
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (out_rx, err_rx) = (drain(stdout), drain(stderr));

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait()? {
            Some(status) => {
                // The helper is gone, but a grandchild it left behind still holds the pipes' WRITE
                // ends, so the readers below would wait for an EOF that never arrives. Ending the
                // tree closes them for every descendant that stayed in the group — which is all of
                // them for a direct exec, but NOT for one that left it (see [`DRAIN_GRACE`]), so
                // the collection below is bounded rather than a plain join.
                tree.terminate();
                break status;
            }
            None if Instant::now() >= deadline => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait(); // reap it — never leave a zombie behind

                // Reap the READERS too. This arm used to just drop their handles, i.e. detach two
                // threads still blocked in `read_to_end` and still owning the pipes' read ends —
                // so a writer that escaped the group (a `systemd-run --pipe` unit) never even got
                // the EPIPE the pre-drain implementation gave it by closing those fds with the
                // `Child`. Joining unconditionally instead would be worse: it would hand the
                // escaped writer the caller's thread, forever, which is the failure this whole
                // module exists to prevent. So: a bounded collection, and an honest log when one
                // of them cannot be reclaimed.
                let until = Instant::now() + DRAIN_GRACE;
                let (out, err) = (collect(&out_rx, until), collect(&err_rx, until));
                if out.is_none() || err.is_none() {
                    stuck_reader(cmd, "killed at its budget");
                }
                return Err(timed_out(cmd, budget));
            }
            None => std::thread::sleep(POLL),
        }
    };
    // Both halves of the output, or none: a caller parsing half a `pw-dump` is a caller being lied
    // to, and its failure path is the one it already has for a helper that did not answer.
    let until = Instant::now() + DRAIN_GRACE;
    let (Some(stdout), Some(stderr)) = (collect(&out_rx, until), collect(&err_rx, until)) else {
        stuck_reader(cmd, "exited");
        let program = cmd.get_program().to_string_lossy().to_string();
        return Err(Error::new(
            ErrorKind::TimedOut,
            format!(
                "`{program}` exited but its output could not be drained within {DRAIN_GRACE:?}"
            ),
        ));
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one of a child's pipes on its own thread, so the child never blocks in `write()` waiting
/// for us to catch up, and hand the result back over a channel — not a `JoinHandle`, because the
/// caller must be able to give up on a reader it cannot unblock (see [`DRAIN_GRACE`]) and a
/// `join` offers no way to. Returns whatever was read; a read error yields the partial buffer,
/// because the caller's failure signal is the budget, not a short pipe.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = pipe {
            let mut r = r.take(DRAIN_CAP);
            let _ = r.read_to_end(&mut buf);
            if buf.len() as u64 >= DRAIN_CAP {
                tracing::warn!(
                    cap_bytes = DRAIN_CAP,
                    "a helper outran the drain cap — its output is truncated here, which the \
                     caller sees as an unparseable answer (i.e. a failed query)"
                );
            }
        }
        // The receiver is gone whenever the call has already returned — a timeout, or a grace that
        // ran out. That is the only way this send fails, and it is a case we chose.
        let _ = tx.send(buf);
    });
    rx
}

/// Take one drained pipe, waiting no longer than `until`. `None` means the reader is still parked
/// on a write end nothing we can signal is holding open.
fn collect(rx: &Receiver<Vec<u8>>, until: Instant) -> Option<Vec<u8>> {
    match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(buf) => Some(buf),
        // The reader panicked: that loses its half of the output, never the call.
        Err(RecvTimeoutError::Disconnected) => Some(Vec::new()),
        Err(RecvTimeoutError::Timeout) => None,
    }
}

/// Say plainly what a stuck reader costs, because the thread is genuinely leaked and there is no
/// portable way to unblock a thread already inside `read()` on a pipe (a `dup2` over the fd does
/// not re-target a read in flight, and closing it under the thread is a use-after-free waiting for
/// an fd number to be reused). It ends when the escaped writer closes or [`DRAIN_CAP`] is reached.
fn stuck_reader(cmd: &Command, what: &str) {
    tracing::warn!(
        program = %cmd.get_program().to_string_lossy(),
        grace_ms = DRAIN_GRACE.as_millis() as u64,
        "helper {what} but its pipes never reached EOF — something it started is outside our \
         process group and still holds the write end (`systemd-run --pipe` is the known case). \
         The call is bounded; the reader thread is detached until that writer closes."
    );
}

fn timed_out(cmd: &Command, budget: Duration) -> Error {
    let program = cmd.get_program().to_string_lossy().to_string();
    tracing::warn!(
        program,
        budget_ms = budget.as_millis() as u64,
        "helper did not exit within its budget — killed it (a wedged compositor/session bus is the \
         usual cause); treating it as a failed query"
    );
    Error::new(
        ErrorKind::TimedOut,
        format!("`{program}` did not exit within {budget:?}"),
    )
}

/// The calling process's real uid.
///
/// Every session/gamescope lookup that derives `/run/user/<uid>` (or filters `/proc` to "our"
/// processes) needs this, and each one used to open its own `unsafe` block carrying a verbatim
/// copy of the same SAFETY note. `getuid()` is parameterless, always succeeds, and touches no
/// memory, so there is no contract for a caller to uphold — which makes it exactly the shape that
/// belongs behind a safe wrapper instead of being restated at every call site. One `unsafe` here,
/// none at the callers.
#[cfg(target_os = "linux")]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: parameterless POSIX call that always succeeds and touches no memory — it just
    // returns the calling process's real uid. Nothing is aliased, read, or freed.
    unsafe { libc::getuid() }
}

/// The longest `/proc/<pid>/comm` the kernel will report: `TASK_COMM_LEN` is 16 *including* the
/// NUL, so a name of exactly this many bytes may be a truncation of a longer one.
#[cfg(target_os = "linux")]
const COMM_MAX: usize = 15;

/// The executable name to identify a process by, with nixpkgs wrapper decoration undone.
///
/// `comm` is the kernel's name for the **executed file**, truncated to [`COMM_MAX`] bytes — it is
/// not `argv[0]` and not the command line. nixpkgs wraps essentially every graphical binary:
/// `wrapProgram` moves the real ELF aside to `.<name>-wrapped` and installs a shell wrapper under
/// the original name, and that wrapper `exec -a "$0"`s the hidden file. So `ps`/`pgrep -a` show a
/// perfectly ordinary `kwin_wayland` (they read argv) while the kernel reports `.kwin_wayland-w`
/// — 15 bytes of `.kwin_wayland-wrapped`, which can never equal `kwin_wayland`.
///
/// That is not a KDE-only detail. On NixOS `kwin_wayland`, `gamescope`, `gnome-shell` and
/// `Hyprland` are all wrapped, so an exact `comm` comparison made [`super::session`]'s probe
/// answer [`crate::ActiveKind::None`] on a visibly running desktop — and because the probe is the
/// *only* input to that decision, no environment variable could reach it: `WAYLAND_DISPLAY` was
/// correct, capture worked the moment detection was satisfied, and a `PUNKTFUNK_COMPOSITOR` pin
/// turned the miss into a hard error via `pinned_at_a_dead_session`. (sway survives by accident —
/// nixpkgs' wrapper execs a real binary that is itself still called `sway`.)
///
/// The `comm` fast path is kept for every ordinary distro: one read, no readlink. Only a name that
/// *could* be decorated or truncated — it starts with `.`, or it is exactly [`COMM_MAX`] bytes —
/// is re-resolved, first through `/proc/<pid>/exe` and then, when the kernel refuses that link,
/// through `argv[0]`.
///
/// 🛑 **That last rung is not defensive padding — without it this resolver misses the exact box it
/// was written for.** Reading `/proc/<pid>/exe` is *not* merely a matter of owning the process: the
/// kernel gates it behind `cap_ptrace_access_check`, which demands the reader's effective set be a
/// superset of the target's PERMITTED set. A compositor holding a capability is therefore opaque to
/// our (deliberately uncapped — see the KWin identification note in `pf-encode`) host, same uid or
/// not. And NixOS's own Plasma module ships exactly that:
/// `security.wrappers.kwin_wayland = { capabilities = "cap_sys_nice+ep"; }`. So on NixOS + KDE the
/// two traps compose — the name needs `exe` *because* nixpkgs wrapped it, and `exe` is denied
/// *because* NixOS capped it — and the session probe went straight back to
/// [`crate::ActiveKind::None`] on a running desktop.
///
/// Measured (Linux 6.x, same-uid reader, target holding `cap_sys_nice`), for a file capability and
/// for the ambient-capability form `security.wrappers` actually uses, identically:
///
/// | probe | capped target |
/// |---|---|
/// | `/proc/<pid>` owner | ✅ still the real uid — the uid filter upstream is unaffected |
/// | `comm` | ✅ readable (decorated/truncated, so still unusable on its own) |
/// | `exe` | ❌ **EACCES** |
/// | `cmdline` (`argv[0]`) | ✅ readable |
///
/// `argv[0]` is only consulted when the kernel has refused the authoritative answer, because it is
/// the process's own claim about itself rather than the kernel's: a same-uid process can set it to
/// anything. The exposure that buys is small and one-directional — the worst a spoof achieves is
/// aiming detection at a compositor backend that then fails its own availability probe — whereas
/// without the rung a capped compositor is simply invisible. It reads correctly here for the same
/// reason `ps` does: make-wrapper's generated wrapper `exec -a "$0"`s the hidden binary, so
/// `argv[0]` survives the decoration that `comm` does not.
///
/// `pid_path` is a `/proc/<pid>` directory. `None` when the process vanished mid-scan.
#[cfg(target_os = "linux")]
pub(crate) fn match_name(pid_path: &std::path::Path) -> Option<String> {
    let comm = std::fs::read_to_string(pid_path.join("comm")).ok()?;
    let comm = comm.trim();
    // An undecorated name short enough to be complete is already the answer.
    if !comm.starts_with('.') && comm.len() < COMM_MAX {
        return Some(comm.to_string());
    }
    // The authoritative rung: the kernel's own record of the executed file, untruncated. Absent for
    // a kernel thread and for a process exiting under us, and REFUSED for a capability-holding one.
    let exe = std::fs::read_link(pid_path.join("exe")).ok();
    if let Some(full) = exe
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
    {
        return Some(undecorate(full).to_string());
    }
    // Refused or gone: fall back to what the process calls itself, then to the truncated `comm`.
    match argv0_name(pid_path) {
        Some(name) => {
            tracing::debug!(
                comm = %comm,
                resolved = %name,
                "/proc/<pid>/exe unreadable (a capability-holding process refuses it); \
                 identified via argv[0]"
            );
            Some(name)
        }
        None => Some(comm.to_string()),
    }
}

/// The file name in `argv[0]`, with nixpkgs decoration undone — the last rung of [`match_name`].
///
/// `/proc/<pid>/cmdline` is NUL-separated, so the first field is `argv[0]` whole, with no splitting
/// on whitespace to get wrong. `None` when it is unreadable or empty, which is the normal state for
/// a kernel thread and for a zombie.
#[cfg(target_os = "linux")]
fn argv0_name(pid_path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read(pid_path.join("cmdline")).ok()?;
    let argv0 = raw.split(|b| *b == 0).next()?;
    // A process that rewrote its own argv (setproctitle-style) can leave anything here, including
    // something that is not a path at all — `file_name` simply yields it unchanged and it fails to
    // match any compositor name, which is the correct outcome.
    let argv0 = std::str::from_utf8(argv0).ok()?;
    let name = std::path::Path::new(argv0).file_name()?.to_str()?;
    (!name.is_empty()).then(|| undecorate(name).to_string())
}

/// Strip nixpkgs `wrapProgram` decoration: `.<name>-wrapped`, plus the `_` suffixes make-wrapper
/// appends when that hidden name is already taken (a doubly-wrapped app — Qt *and* GApps).
///
/// **Both** halves are required, and that is the load-bearing part rather than pedantry: KWin
/// ships its own real binary called `kwin_wayland_wrapper` (the session's parent process), so a
/// rule that merely stripped a `wrapper`-ish suffix would rewrite it into `kwin_wayland` and hand
/// the session probe the wrong PID. Demanding the leading `.` as well keeps it — and any genuine
/// `foo-wrapped` — under its real name.
#[cfg(target_os = "linux")]
fn undecorate(name: &str) -> &str {
    let Some(rest) = name.strip_prefix('.') else {
        return name;
    };
    match rest.trim_end_matches('_').strip_suffix("-wrapped") {
        Some(real) if !real.is_empty() => real,
        _ => name,
    }
}

/// Ending the *tree* the helper started, not just the process we spawned.
///
/// [`std::process::Child::kill`] is one `TerminateProcess` / one `SIGKILL`: it ends exactly the
/// process we launched. That is never the whole story — see the Unix twin below for why it is not
/// enough there either — but Windows is where it fails hardest.
///
/// On Windows there is no direct exec: every helper is reached through a shell
/// (`cmd /c …`, `powershell -Command "… | pnputil …"`), so the process that actually hangs is a
/// **grandchild**. Killing the shell leaves it running — holding the stdio handles and the working
/// directory it inherited from us — and a budget that leaves that behind has not bounded anything.
/// This is not theoretical: it is how a fully green `cargo test -p pf-vdisplay` still failed its CI
/// job. The suite's own hung-helper case orphaned a 60-second `ping.exe`, which kept the build
/// step's stdout pipe open past the runner's 10 s `WaitDelay` and pinned `crates\pf-vdisplay` so
/// the workspace could not be cleaned up.
///
/// A Job object is the mechanism Windows provides for exactly this: job membership is inherited
/// across `CreateProcess`, so assigning the child enrolls every descendant it goes on to spawn, and
/// one `TerminateJobObject` ends all of them. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes that hold
/// even on paths that never reach [`Guard::terminate`] — an early `?`, a panic — because closing
/// the last handle to the job is then itself the kill.
///
/// Best-effort by construction: if the job cannot be created or the child cannot be assigned, the
/// helpers degrade to the single-process kill they did before rather than failing the query, which
/// is the same stance as the already-ignored result of `Child::kill`.
#[cfg(windows)]
mod tree {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Nothing to arrange before the spawn: job membership is assigned to the live process, so
    /// [`Guard::attach`] does all of it. The Unix twin has to act here instead.
    pub(super) fn prepare(_cmd: &mut std::process::Command) {}

    /// Owns a Job object holding the spawned helper and everything it spawns. `None` when the job
    /// could not be set up (see the module doc: degrade, don't fail).
    pub(super) struct Guard(Option<HANDLE>);

    impl Guard {
        /// Enroll `child` — and, transitively, its descendants — in a fresh kill-on-close job.
        pub(super) fn attach(child: &Child) -> Self {
            // SAFETY: an unnamed job object with default security attributes; no pointer is passed
            // and none is retained. The handle it yields is owned by the `Guard` constructed from
            // it on the next line and closed exactly once, in that `Guard`'s `Drop`.
            let job = match unsafe { CreateJobObjectW(None, None) } {
                Ok(job) => job,
                Err(e) => {
                    tracing::debug!(error = %e, "no job object for this helper — a hung one's \
                         grandchildren will outlive its budget");
                    return Self(None);
                }
            };
            // Owned from here on, so both fallible steps below can bail without leaking it.
            let guard = Self(Some(job));
            if let Err(e) = guard.enroll(child) {
                tracing::debug!(error = %e, "could not enroll this helper in its job object — a \
                     hung one's grandchildren will outlive its budget");
            }
            guard
        }

        fn enroll(&self, child: &Child) -> windows::core::Result<()> {
            let Some(job) = self.0 else { return Ok(()) };
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation:
                    windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                ..Default::default()
            };
            // SAFETY: `job` is the live handle this `Guard` owns. The pointer is to a fully
            // initialised local of exactly the type `JobObjectExtendedLimitInformation` selects,
            // passed with that type's own size, and the kernel copies the limits out before
            // returning — `info` is not retained past the call.
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;
            }
            // SAFETY: `job` is the live handle this `Guard` owns. `child` is borrowed for the call,
            // so the process handle it lends is open and stays open for the duration — `Child`
            // closes it only in its own `Drop`. The kernel duplicates what it needs; we keep
            // ownership of both handles.
            unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }
        }

        /// End every process still in the job. A no-op once they have all exited, so this is safe
        /// to call on the success path as well as the timeout one.
        pub(super) fn terminate(&self) {
            let Some(job) = self.0 else { return };
            // SAFETY: `job` is this `Guard`'s live, owned handle — it is closed only in `Drop`,
            // which cannot have run while `&self` is borrowed. The exit code is arbitrary; nothing
            // reads it, because a killed tree is reported to callers as the budget's `TimedOut`.
            let _ = unsafe { TerminateJobObject(job, 1) };
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let Some(job) = self.0.take() else { return };
            // SAFETY: `job` is the handle created in `attach` and owned solely by this `Guard`;
            // `take` makes this the one and only close. Per the module doc this close is also the
            // backstop kill — with KILL_ON_JOB_CLOSE set, dropping the last handle terminates
            // whatever is still in the job, so no early return can leak the tree.
            let _ = unsafe { CloseHandle(job) };
        }
    }
}

/// The Unix half — a **process group**, which is what Unix offers in place of a Job object.
///
/// This used to be an empty stub whose doc said `Child::kill` "already ends the only process there
/// is". Most Linux helpers here really are a single exec — `kscreen-doctor`, `pw-dump`, `hyprctl`,
/// `swaymsg` — but not all of them: `systemd-run --user` and `systemctl --user` do their work
/// through the user manager, which forks the actual process, so what hangs is routinely something
/// `Child::kill` cannot reach. With the reader threads in [`output_within`] waiting until every
/// write end of a pipe closes, one surviving relative is all it takes to keep a "bounded" call
/// going, which is why the group exists here too.
///
/// [`prepare`] puts the child in a new process group (it becomes the leader, so the group id is its
/// pid) and [`Guard::terminate`] `killpg`s that group, reaching every descendant that has not
/// deliberately left it. `process_group` changes only the group — not the session — so the helper
/// keeps its controlling terminal and login session, which anything doing a logind/polkit session
/// lookup depends on. Note the limit that follows from this and is NOT closed here: a process the
/// **user manager** forks on our behalf (`systemd-run --pipe`, whose transient unit inherits our
/// pipe write ends) is in another group and session by construction, so `killpg` misses it — see
/// [`DRAIN_GRACE`] for how the reader side is bounded in spite of that. The crate's one privileged
/// path, `pkexec` for the DM helper, deliberately does not come through this module at all: it
/// calls `Command::output()` directly and is documented as unbounded, because a `stop`/`restore`
/// verb legitimately takes seconds and killing it mid-flight is worse than waiting.
///
/// Best-effort in the same way as the Windows half: a failed `killpg` is ignored, and the
/// single-process `Child::kill` on the timeout path still runs.
#[cfg(not(windows))]
mod tree {
    use std::os::unix::process::CommandExt;

    /// The child's process-group id, captured while the child is still ours to reap.
    pub(super) struct Guard(Option<i32>);

    /// Make the child the leader of its own process group, so its descendants are reachable as one.
    pub(super) fn prepare(cmd: &mut std::process::Command) {
        cmd.process_group(0);
    }

    impl Guard {
        pub(super) fn attach(child: &std::process::Child) -> Self {
            // `prepare` asked for `process_group(0)`, so the group id IS the child's pid.
            Self(i32::try_from(child.id()).ok())
        }

        /// End every process still in the group. A no-op once they have all exited, so this is safe
        /// to call on the success path as well as the timeout one.
        pub(super) fn terminate(&self) {
            let Some(pgid) = self.0 else { return };
            // `killpg` is a signal to a group we created and whose leader is the child we spawned;
            // it cannot name a process we did not start. The one theoretical hazard is pid reuse
            // between the leader's reap and this call, which needs a brand-new process to land on
            // exactly that pid AND be a group leader — Linux hands out pids sequentially to
            // `pid_max`, so there is no window to speak of, and the alternative (not killing) is
            // the unbounded wait this module exists to prevent.
            // SAFETY: a plain signal send by group id. No pointer is passed, nothing is aliased,
            // and the result is deliberately ignored — ESRCH just means the group is already gone.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

// `unix` gate, not `test` alone: this module is compiled on every platform (lib.rs declares it
// unconditionally), but the cases below spawn `sleep`/`true`/`echo` as EXECUTABLES. On Windows
// `echo` is a shell builtin and there is no `sleep.exe`, so an ungated module turns a green suite
// red the first time anyone runs it there. The Windows twin is below.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A helper that never exits must be killed at the budget and reported as `TimedOut` — the
    /// whole point of the module (an unbounded `status()` here is what wedged a whole session).
    #[test]
    fn a_hung_child_is_killed_at_the_budget() {
        let started = Instant::now();
        let err = status_within(Command::new("sleep").arg("30"), Duration::from_millis(150))
            .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return at its budget, not the child's lifetime (took {:?})",
            started.elapsed()
        );
    }

    /// A helper whose output exceeds one pipe buffer must still be captured IN FULL.
    ///
    /// This is the case that fails against a `wait_with_output`-after-exit implementation: the
    /// child blocks in `write()` with the pipe full, never exits, and the budget turns a perfectly
    /// successful query into a `TimedOut` with its output thrown away. 1 MiB is ~16× a Linux pipe
    /// (64 KiB) and ~64× the smallest macOS one, so it cannot be absorbed by a buffer on either.
    #[test]
    fn a_child_that_outruns_the_pipe_buffer_is_captured_in_full() {
        const BYTES: usize = 1024 * 1024;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("yes punktfunk | head -c {BYTES}; echo done >&2"));
        let out = output_within(&mut cmd, Duration::from_secs(20)).expect("must not time out");
        assert!(out.status.success(), "helper failed: {:?}", out.status);
        assert_eq!(out.stdout.len(), BYTES, "stdout was truncated");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "done");
    }

    /// A helper that exits while a background child of its own still holds the pipe must not park
    /// the caller: the reader waits for EOF on ALL write ends, so the grandchild's copy is what
    /// would keep it there. Ending the process group is what closes it — and the collection is
    /// bounded ([`DRAIN_GRACE`]) so that even the one relative a `killpg` cannot reach (a unit the
    /// user manager forked for us) costs a detached thread rather than the calling thread.
    #[test]
    fn a_grandchild_holding_the_pipe_does_not_park_the_caller() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo punktfunk");
        let out = output_within(&mut cmd, Duration::from_secs(10)).expect("the helper exited");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call waited on the grandchild's EOF (took {:?})",
            started.elapsed()
        );
    }

    /// The normal path is unaffected: a quick command still yields its status and its output.
    #[test]
    fn a_quick_child_returns_normally() {
        let st = status_within(&mut Command::new("true"), Duration::from_secs(5)).expect("ran");
        assert!(st.success());

        let out = output_within(
            Command::new("echo").arg("punktfunk"),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
    }
}

/// The `comm`-vs-real-name resolution ([`match_name`]). Linux-only, because the trap it exists for
/// is a Linux kernel detail (`comm` names the executed FILE, truncated to 15 bytes) crossed with a
/// nixpkgs packaging convention.
///
/// Driven against **fixture** `/proc/<pid>` directories rather than spawned processes, for the same
/// reason the `/proc` matcher in `punktfunk-host` learned the hard way: a stand-in has to be a real
/// ELF that tolerates being *renamed*, and `/bin/sleep` is not one. Modern coreutils (uutils on
/// Ubuntu 25.10+, busybox elsewhere) is a MULTI-CALL binary — copied to `.kwin_wayland-wrapped` it
/// prints "unknown program" and exits before `/proc` can be read, and restoring `argv[0]` does not
/// save it. That reads exactly like this resolver being broken. The truncation the fixtures encode
/// is not guessed: the strings below were measured from a live kernel (`.kwin_wayland-w`,
/// `.kwin_wayland_w`, `.gamescope-wrap` — all 15 bytes) against binaries installed and exec'd the
/// way nixpkgs does it.
#[cfg(all(test, target_os = "linux"))]
mod name_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A fake `/proc/<pid>` directory: a `comm` file and, optionally, the `exe` symlink and a
    /// `cmdline`. Removed on drop.
    ///
    /// An absent `exe` stands in for **both** ways the real link yields nothing: a process exiting
    /// under the scan, and — the case that matters here — a capability-holding one, whose link the
    /// kernel refuses with EACCES. `match_name` cannot tell those apart and does not need to.
    struct FakePid {
        dir: PathBuf,
    }

    impl FakePid {
        /// `comm` is written exactly as the kernel would report it — i.e. already truncated.
        fn new(tag: &str, comm: &str, exe: Option<&str>) -> FakePid {
            FakePid::with_cmdline(tag, comm, exe, None)
        }

        /// `cmdline` is the NUL-separated argument vector the kernel exposes; the fixture is given
        /// just `argv[0]` and appends the terminator, as a real one carries.
        fn with_cmdline(tag: &str, comm: &str, exe: Option<&str>, argv0: Option<&str>) -> FakePid {
            let dir = std::env::temp_dir().join(format!("pf-vd-name-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("comm");
            if let Some(exe) = exe {
                // The target need not exist: `read_link` reports the link's contents, and a real
                // `/proc/<pid>/exe` routinely points at a path that has since been replaced.
                std::os::unix::fs::symlink(
                    format!("/nix/store/eeee-kwin-6.5.0/bin/{exe}"),
                    dir.join("exe"),
                )
                .expect("exe symlink");
            }
            if let Some(argv0) = argv0 {
                std::fs::write(dir.join("cmdline"), format!("{argv0}\0--session\0")).expect("cmd");
            }
            FakePid { dir }
        }
        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for FakePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The decoration table. The `kwin_wayland_wrapper` rows are the ones that earn their keep: it
    /// is a REAL KWin binary (the session's parent process), so the rule must leave it under its own
    /// name in both its plain and its wrapped form rather than collapsing either into
    /// `kwin_wayland` and handing the session probe the wrong PID.
    #[test]
    fn undecorate_strips_only_a_real_nixpkgs_wrapper() {
        for (raw, want) in [
            (".kwin_wayland-wrapped", "kwin_wayland"),
            (".gamescope-wrapped", "gamescope"),
            (".gnome-shell-wrapped", "gnome-shell"),
            (".Hyprland-wrapped", "Hyprland"),
            // make-wrapper appends `_`s when the hidden name is already taken (a Qt + GApps
            // double-wrap), so the underscores come off before the suffix does.
            (".kwin_wayland-wrapped_", "kwin_wayland"),
            (".kwin_wayland-wrapped__", "kwin_wayland"),
            // Not decoration — every one of these keeps its exact name.
            ("kwin_wayland", "kwin_wayland"),
            ("kwin_wayland_wrapper", "kwin_wayland_wrapper"),
            (".kwin_wayland_wrapper-wrapped", "kwin_wayland_wrapper"),
            ("foo-wrapped", "foo-wrapped"),
            (".hidden", ".hidden"),
            (".-wrapped", ".-wrapped"),
        ] {
            assert_eq!(undecorate(raw), want, "undecorate({raw:?})");
        }
    }

    /// The whole bug. Every compositor the session probe matches on is wrapped by nixpkgs, so the
    /// kernel reports a truncated, decorated `comm` that can never equal the name being compared —
    /// which is why `detect_active_session` answered `ActiveKind::None` on a *running* KDE desktop
    /// and every connect died "no usable compositor".
    #[test]
    fn a_nixpkgs_wrapped_compositor_resolves_to_its_real_name() {
        for (tag, comm, exe, want) in [
            (
                "kwin",
                ".kwin_wayland-w",
                ".kwin_wayland-wrapped",
                "kwin_wayland",
            ),
            (
                "gamescope",
                ".gamescope-wrap",
                ".gamescope-wrapped",
                "gamescope",
            ),
            (
                "gnome",
                ".gnome-shell-wr",
                ".gnome-shell-wrapped",
                "gnome-shell",
            ),
            ("hypr", ".Hyprland-wrapp", ".Hyprland-wrapped", "Hyprland"),
        ] {
            let p = FakePid::new(tag, comm, Some(exe));
            assert_eq!(
                match_name(p.path()).as_deref(),
                Some(want),
                "a nixpkgs-wrapped {want} must resolve to the name the session probe matches"
            );
        }
    }

    /// KWin's own `kwin_wayland_wrapper` is a real binary that runs *alongside* `kwin_wayland`, and
    /// its wrapped `comm` (`.kwin_wayland_w`) differs from the compositor's by a single byte. It
    /// must NOT resolve to `kwin_wayland`: the probe would then match the parent process and carry
    /// its PID as the compositor identity, which drives restart detection.
    #[test]
    fn kwins_own_wrapper_binary_does_not_masquerade_as_the_compositor() {
        let p = FakePid::new(
            "kwrap",
            ".kwin_wayland_w",
            Some(".kwin_wayland_wrapper-wrapped"),
        );
        assert_eq!(
            match_name(p.path()).as_deref(),
            Some("kwin_wayland_wrapper")
        );
    }

    /// The other half of the 15-byte limit, with no nix involved: a long name is truncated too, and
    /// has to be recovered from `exe` rather than matched short.
    #[test]
    fn a_long_name_is_recovered_untruncated() {
        let p = FakePid::new(
            "long",
            "a-very-long-com",
            Some("a-very-long-compositor-name"),
        );
        assert_eq!(
            match_name(p.path()).as_deref(),
            Some("a-very-long-compositor-name")
        );
    }

    /// The fast path answers without consulting `exe` at all — which is what keeps this probe at one
    /// read per process on every ordinary distro, and what lets it answer for a process whose `exe`
    /// is unreadable in the first place.
    #[test]
    fn an_ordinary_short_name_never_needs_the_exe_link() {
        let p = FakePid::new("plain", "kwin_wayland", None);
        assert_eq!(match_name(p.path()).as_deref(), Some("kwin_wayland"));
    }

    /// A decorated-or-truncated name with neither `exe` nor `cmdline` to fall back on (a kernel
    /// thread, or a process exiting under the scan) degrades to the truncated `comm` instead of
    /// failing the whole entry.
    #[test]
    fn an_unreadable_exe_falls_back_to_comm() {
        let p = FakePid::new("noexe", ".kwin_wayland-w", None);
        assert_eq!(match_name(p.path()).as_deref(), Some(".kwin_wayland-w"));
    }

    /// **The NixOS + KDE field bug in one assertion.** nixpkgs wraps the binary, so `comm` is
    /// `.kwin_wayland-w` and only `exe` carries the real name — and NixOS's own Plasma module hands
    /// KWin `cap_sys_nice+ep` through `security.wrappers`, so the kernel refuses that link to our
    /// uncapped host. Both traps at once is not a hypothetical combination: it is the default
    /// install. `argv[0]` is what survives, because make-wrapper's wrapper `exec -a "$0"`s the
    /// hidden binary.
    #[test]
    fn a_capped_wrapped_compositor_is_identified_by_argv0() {
        for (tag, comm, argv0, want) in [
            // Plasma's own startup execs the wrapper by absolute path.
            (
                "capkwin",
                ".kwin_wayland-w",
                "/run/wrappers/bin/kwin_wayland",
                "kwin_wayland",
            ),
            // …and a bare name is just as ordinary.
            ("capbare", ".kwin_wayland-w", "kwin_wayland", "kwin_wayland"),
            // gamescope carries `cap_sys_nice` on a great many distros, wrapped or not.
            (
                "capgame",
                ".gamescope-wrap",
                "/nix/store/aaaa-gamescope/bin/gamescope",
                "gamescope",
            ),
            // A wrapper that passes the hidden path through as `argv[0]` still undecorates.
            (
                "capraw",
                ".kwin_wayland-w",
                "/nix/store/eeee-kwin/bin/.kwin_wayland-wrapped",
                "kwin_wayland",
            ),
        ] {
            let p = FakePid::with_cmdline(tag, comm, None, Some(argv0));
            assert_eq!(
                match_name(p.path()).as_deref(),
                Some(want),
                "a capped, wrapped {want} must still be identified from argv[0]"
            );
        }
    }

    /// `exe` outranks `argv[0]` whenever the kernel allows it: `argv[0]` is the process's own claim
    /// about itself and a same-uid process can set it to anything, so it may never override the
    /// kernel's answer — only stand in when there is none.
    #[test]
    fn a_readable_exe_outranks_a_lying_argv0() {
        let p = FakePid::with_cmdline(
            "liar",
            ".gamescope-wrap",
            Some(".gamescope-wrapped"),
            Some("kwin_wayland"),
        );
        assert_eq!(match_name(p.path()).as_deref(), Some("gamescope"));
    }

    /// A zombie's `cmdline` is empty, and `argv[0]` can be an empty string even when it is not —
    /// neither may yield an empty name (which would then be compared against, and could match, a
    /// compositor name only by accident).
    #[test]
    fn an_empty_cmdline_does_not_produce_a_name() {
        for (tag, cmdline) in [("zombie", ""), ("nulls", "\0\0")] {
            let dir = std::env::temp_dir().join(format!("pf-vd-name-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            std::fs::write(dir.join("comm"), ".kwin_wayland-w\n").expect("comm");
            std::fs::write(dir.join("cmdline"), cmdline).expect("cmdline");
            assert_eq!(match_name(&dir).as_deref(), Some(".kwin_wayland-w"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A pid directory that does not exist yields `None`, not a bogus name — the scans `continue`.
    #[test]
    fn a_vanished_process_yields_none() {
        assert_eq!(match_name(Path::new("/proc/0")), None);
    }

    /// The one thing a fixture cannot establish: that reading `/proc/<pid>/exe` is actually
    /// *permitted* for a process of our own uid. Checked against the only such process guaranteed
    /// to be running — this one. ⚠ It holds because *we* are uncapped, and says nothing about the
    /// processes being scanned: a capped target refuses this same link, which is what
    /// [`match_name`]'s `argv[0]` rung exists for.
    #[test]
    fn our_own_exe_link_is_readable() {
        let me = Path::new("/proc/self");
        let exe = std::fs::read_link(me.join("exe"))
            .expect("/proc/self/exe must be readable for our own uid");
        let name = exe.file_name().and_then(|n| n.to_str()).expect("exe name");
        let got = match_name(me).expect("our own name");
        // Whichever rung answered, it must agree with the real binary: the fast path returns the
        // (short, undecorated) comm, which is a prefix of it; the exe path returns it outright.
        assert!(
            name.starts_with(got.as_str()) || got == name,
            "resolved {got:?} disagrees with our real binary {name:?}"
        );
    }
}

/// The same two cases through `cmd /c`, so the budget logic is covered on the platform whose
/// process model differs most (job objects, no `SIGKILL`). `ping -n` is the standard Windows
/// no-extra-tooling sleep.
#[cfg(all(test, windows))]
mod tests_windows {
    use super::*;

    #[test]
    fn a_hung_child_is_killed_at_the_budget() {
        let started = Instant::now();
        let err = status_within(
            Command::new("cmd").args(["/c", "ping -n 60 127.0.0.1 >NUL"]),
            Duration::from_millis(150),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return at its budget, not the child's lifetime (took {:?})",
            started.elapsed()
        );
    }

    #[test]
    fn a_quick_child_returns_normally() {
        let st = status_within(
            Command::new("cmd").args(["/c", "exit 0"]),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(st.success());

        let out = output_within(
            Command::new("cmd").args(["/c", "echo punktfunk"]),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
    }

    /// A scratch directory for the tree test's marker files, removed on drop. `remove_dir_all` is
    /// deliberately un-asserted: if the tree *did* survive it still has this directory as its
    /// working directory and the removal fails — which is the failure the test itself reports.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("pf-vd-proc-tree-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The budget must end the whole tree, not just the process we spawned.
    ///
    /// Every Windows helper is reached through a shell, so the process that actually hangs is a
    /// grandchild that `Child::kill` cannot see. Left alive it keeps our stdio handles and our
    /// working directory — which is how a green test run still failed its CI job before the job
    /// object went in (the orphan held the build step's pipe open past the runner's `WaitDelay`
    /// and pinned the crate directory against cleanup).
    #[test]
    fn the_budget_ends_the_whole_tree_not_just_the_child() {
        let scratch = Scratch::new();
        let ran = scratch.0.join("grandchild-ran");
        let survived = scratch.0.join("grandchild-survived");
        let script = scratch.0.join("grandchild.cmd");
        // `%~dp0` — the script's own directory, resolved by cmd at run time — rather than the paths
        // interpolated in. A .cmd file is read in the OEM code page, so an absolute path baked into
        // it is mangled the moment the temp dir contains a non-ASCII character (`C:\Users\Enrico
        // Bühler\…` arrives as `B?hler`) and every redirect in it fails with "path not found".
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n\
                 echo up>\"%~dp0{}\"\r\n\
                 ping -n 4 127.0.0.1 >NUL\r\n\
                 echo up>\"%~dp0{}\"\r\n",
                ran.file_name().expect("marker name").to_string_lossy(),
                survived.file_name().expect("marker name").to_string_lossy(),
            ),
        )
        .expect("write the grandchild script");

        // `cmd /c cmd /c <script>`: the OUTER cmd is our child, the inner one is the grandchild
        // that `Child::kill` alone would leave running.
        let err = status_within(
            Command::new("cmd").args(["/c", "cmd", "/c", &script.display().to_string()]),
            Duration::from_millis(2500),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);

        // Without this the test could pass vacuously — a grandchild killed before it ever started
        // proves nothing about killing trees.
        assert!(
            ran.exists(),
            "the grandchild never got as far as its first marker, so this run proves nothing"
        );

        // Outlast the grandchild's own sleep: a surviving tree writes the second marker.
        std::thread::sleep(Duration::from_secs(5));
        assert!(
            !survived.exists(),
            "a grandchild outlived the budget — the shell was killed but not the tree under it"
        );
    }
}
