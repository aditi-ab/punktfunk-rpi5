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

use std::io::{Error, ErrorKind, Result};
use std::process::{Command, ExitStatus, Output};
use std::time::{Duration, Instant};

/// Poll interval while waiting for a child to exit. Short enough that a fast helper (the normal
/// case — `kscreen-doctor` answers in tens of ms) isn't measurably delayed.
const POLL: Duration = Duration::from_millis(20);

/// Run `cmd` to completion, killing it if it outlives `budget`.
///
/// Stdout/stderr are left as the caller configured them (inherited by default), so this is for
/// commands run for their exit status alone — see [`output_within`] when the output is read.
pub(crate) fn status_within(cmd: &mut Command, budget: Duration) -> Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if Instant::now() >= deadline => {
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
/// The output is read only after the child has exited, so a helper that fills the pipe buffer and
/// stalls is caught by the budget rather than deadlocking the reader (these helpers emit at most a
/// few hundred KiB, well under any real pipe pressure).
pub(crate) fn output_within(cmd: &mut Command, budget: Duration) -> Result<Output> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait()? {
            // Exited: `wait_with_output` now only drains already-buffered pipes.
            Some(_) => return child.wait_with_output(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out(cmd, budget));
            }
            None => std::thread::sleep(POLL),
        }
    }
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
}
