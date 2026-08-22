//! Finding a launched game's processes, from the signals its store gave us
//! ([`crate::library::DetectSpec`]).
//!
//! Read-only by construction: the host enumerates processes and reads metadata it already has
//! permission to see. No ptrace, no injection, no handles held open. On Linux it looks only at
//! processes owned by its **own uid**; on Windows it runs as SYSTEM and therefore *can* see
//! everything, which makes the two rules below the load-bearing part rather than an afterthought.
//!
//! ### The two rules
//!
//! 1. **Never adopt a process that predates the launch.** A player may already have the game open
//!    when a session starts; treating that instance as "this session's game" would let a session end
//!    kill a process it never started. Candidates are filtered by start time against
//!    [`launch_stamp`], taken before anything spawns.
//! 2. **Never trust a bare pid.** Pids are recycled — aggressively so on Windows — and a lease can
//!    outlive its game by a grace window, so every remembered process carries its start time and is
//!    re-verified against it ([`Scanner::alive`]) before it is counted as running, or signalled.
//!
//! ### Per-OS implementations
//!
//! The two have nothing in common beyond that contract, so they are separate modules presenting the
//! same [`Scanner`] surface: `/proc` on Linux, a Toolhelp snapshot on Windows. Everything above the
//! scanner ([`crate::gamelease`]) is platform-neutral.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Scanner;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Scanner;

/// A process the matcher adopted: its pid plus a start stamp that pins that pid to *this* process,
/// so a recycled pid can never be mistaken for it.
///
/// `start` is opaque and only ever compared for equality against a later read of the same pid — its
/// units differ per platform (clock ticks since boot on Linux, a creation `FILETIME` on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcRef {
    pub pid: u32,
    pub start: u64,
}

/// Tolerance on the "started after the launch" test, in seconds.
///
/// The launch stamp is taken just before the spawn, but start times are quantized (~10 ms on Linux)
/// and a launcher can race ahead of the host's own bookkeeping, so an exact comparison would
/// occasionally reject the real game. Two seconds is far below the time any launcher takes to bring a
/// game up, so it cannot let a *pre-existing* instance through.
pub const START_SLACK_SECS: f64 = 2.0;

/// The reference instant for adopting a launch's processes, in **seconds on the platform's
/// process-start timeline** — seconds since boot on Linux, seconds since the Windows epoch on
/// Windows. Only ever compared against a process's own start time on the same platform, never
/// interpreted as a wall clock or persisted.
///
/// Call it **before** anything spawns; see [`crate::gamelease::LeaseRequest::launch_stamp`]. `None`
/// when the platform has no matcher (macOS) or the clock could not be read, which disables the
/// start-time filter rather than rejecting everything.
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

/// Pin a pid the host itself just spawned to *this* process, so it can be tracked and signalled
/// under rule 2 — see [`Scanner::resolve`]. `None` on a platform with no matcher, and for a pid that
/// has already gone or cannot be queried.
///
/// The platform-neutral wrapper, so [`crate::gamelease`] stays free of `cfg`s.
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

/// Short names for the processes a lease adopted, in `procs` order.
///
/// Diagnostics only — nothing decides anything on these, and they are deliberately not part of
/// [`ProcRef`], which is compared for equality. They exist because a launch that adopted the game
/// and a launch that adopted a *pre-launch* tree logged identically (`procs=1`), which is what left
/// the 2026-08-22 field report unclosable from its log: the one question worth asking of that line
/// is which process the lease latched onto.
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

/// An out-of-band opinion on whether a spec's game is still running, independent of the process scan.
///
/// Consulted **only to veto** declaring a game gone — never to declare it running, and never as the
/// primary signal. `Some(true)` = something else believes it is up, so hold off; `Some(false)` = that
/// something agrees it is gone; `None` = no opinion available, which is the common case.
///
/// Linux has none by design: Steam's launch reaper is both the sharpest signal and already a *process*,
/// so it is covered by the scan itself. Windows has no reaper, which is exactly where a second opinion
/// earns its keep.
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
