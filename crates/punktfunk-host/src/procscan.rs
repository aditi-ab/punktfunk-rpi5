//! Find a launched game's processes from its store signals ([`crate::library::DetectSpec`]).
//!
//! Read-only: enumerate processes and read metadata already visible. No ptrace, no
//! injection, no handles held open. Linux sees only its own uid; Windows runs as
//! SYSTEM and can see everything, which is why the two rules are load-bearing:
//!
//! 1. **Never adopt a process that predates the launch.** Filter by start time
//!    against [`launch_stamp`], taken before anything spawns.
//! 2. **Never trust a bare pid.** Every remembered process carries its start time
//!    and is re-verified ([`Scanner::alive`]) before it is counted running or signalled.
//!
//! `/proc` on Linux, Toolhelp on Windows; same [`Scanner`] surface.
//! [`crate::gamelease`] is platform-neutral.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Scanner;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Scanner;

/// Adopted process: pid plus a start stamp that pins that pid to *this* process.
///
/// `start` is compared only for equality against a later read of the same pid.
/// Units differ: clock ticks since boot on Linux, a creation `FILETIME` on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcRef {
    pub pid: u32,
    pub start: u64,
}

/// Slack on the "started after the launch" test, in seconds.
///
/// Start times are quantized (~10 ms on Linux) and a launcher can race the host,
/// so an exact comparison would reject the real game. Two seconds is far below
/// any launcher's bring-up, so a pre-existing instance still fails the filter.
pub const START_SLACK_SECS: f64 = 2.0;

/// Reference instant for adopting a launch's processes, in seconds on the
/// platform process-start timeline (since boot on Linux, Windows epoch on
/// Windows). Compared only to a process start time on the same platform; never
/// a wall clock and never persisted.
///
/// Call **before** anything spawns ([`crate::gamelease::LeaseRequest::launch_stamp`]).
/// `None` (no matcher, or unread clock) disables the start-time filter.
pub fn launch_stamp() -> Option<f64> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().now_stamp()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// Pin a pid the host just spawned (rule 2). See [`Scanner::resolve`].
/// Platform-neutral so [`crate::gamelease`] stays free of `cfg`s. `None` with
/// no matcher, or if the pid is gone or unqueryable.
pub fn resolve(pid: u32) -> Option<ProcRef> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().resolve(pid)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Re-verify remembered processes. Empty with no matcher. Platform-neutral
/// so [`crate::gamelease`] stays free of `cfg`s.
pub fn alive(procs: &[ProcRef]) -> Vec<ProcRef> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().alive(procs)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = procs;
        Vec::new()
    }
}

/// Diagnostics only: short names in `procs` order. Not part of [`ProcRef`],
/// which is compared for equality.
pub fn names(procs: &[ProcRef]) -> Vec<String> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let scanner = Scanner::system();
        procs.iter().map(|p| scanner.name_of(*p)).collect()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = procs;
        Vec::new()
    }
}

/// Out-of-band opinion on whether a spec's game is still running.
///
/// Used **only to veto** declaring it gone — never as a primary signal.
/// `Some(true)` hold off; `Some(false)` agrees it is gone; `None` no opinion.
///
/// Linux has none: Steam's launch reaper is already a process the scan sees.
/// Windows has no reaper, which is why a second opinion exists.
pub fn running_hint(spec: &crate::library::DetectSpec) -> Option<bool> {
    #[cfg(windows)]
    {
        spec.steam_appid.and_then(windows::steam_running_hint)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        None
    }
}
