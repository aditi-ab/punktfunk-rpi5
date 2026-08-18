//! Best-effort scheduling priority for the client's audio threads.
//!
//! The device callbacks already run where the OS puts realtime audio: the PipeWire playback
//! callback is on the graph's data loop (`RT_PROCESS`), and WASAPI's event-driven render loop
//! is woken by the engine. The threads that FEED them are ordinary threads: the decode leg
//! (`punktfunk-audio-rx` — receive, conceal, decode, queue), the pad-audio renderer, and on
//! Windows the render/mic loops themselves. Their lateness is absorbed by the jitter ring — a
//! decode thread descheduled past the ring depth is a drought the callback conceals — but on a
//! Steam Deck the same four cores decode 1440p120 and present it, and on a loaded Windows box
//! the render loop competes with the game and the compositor. This module is the one place that
//! asks the OS for priority for those threads, on the sanctioned unprivileged paths only.
//!
//! **Linux.** Three rungs, first one that works wins:
//! 1. `setpriority(-10)` — honoured wherever `RLIMIT_NICE` allows (a developer's shell, most
//!    desktops). On SteamOS the user's `RLIMIT_NICE` is 0 and this is a no-op.
//! 2. Inside a flatpak (`/.flatpak-info` exists): the **Realtime portal**
//!    (`org.freedesktop.portal.Realtime` on the session bus). The sandbox has its own PID
//!    namespace, and rtkit-daemon (0.14 verified on the Deck) does NOT translate — it looks up
//!    `/proc/<pid>/task/<tid>/stat` with the numbers it is given, so a direct call from a
//!    sandbox is answered with ENOENT. The portal maps the sandboxed pid/tid to the host's and
//!    calls rtkit on the app's behalf; portals are reachable from every sandbox without a
//!    `--talk-name`. This is the same split PipeWire's own `module-rt` makes.
//! 3. Otherwise **rtkit** directly (`org.freedesktop.RealtimeKit1` on the system bus,
//!    `MakeThreadHighPriorityWithPID`) — what gives PipeWire's data loop its priority on the
//!    Deck, and what `pf_frame::thread_qos` uses on the host.
//!
//! Both bus rungs are gated by polkit's `acquire-high-priority` action with the TARGET process
//! as the subject — allowed for the user's active session and for their session-less user
//! services (a client launched by Steam is one), refused for a remote (ssh) session. Verified on
//! the Deck 2026-08-18 by renicing a live active-session thread and a `steam` user-service thread
//! through both rungs, and restoring them.
//!
//! **Never** `setcap`/`SCHED_RR` here — the `cap_sys_nice` route is the one that killed KDE
//! sessions in the field, and a nice level is all the decode leg needs.
//!
//! **Windows.** MMCSS "Pro Audio" + `THREAD_PRIORITY_HIGHEST` for the calling thread — the
//! same pair every audio engine on the platform uses; the MMCSS handle is intentionally leaked
//! (thread-lifetime; the OS reverts it at exit), as `pf_frame::session_tuning::on_hot_thread`
//! does on the host.
//!
//! Every path is best-effort and logs at debug what it got; a refusal is exactly what the thread
//! had before this existed.

/// Nice level asked for on Linux. `-10` is comfortably inside rtkit's default `MinNiceLevel`
/// (−15 on the Deck) and what the host's own hot threads ask for.
#[cfg(target_os = "linux")]
const NICE: i32 = -10;

/// Raise the CALLING thread's priority for audio work. Call at the top of the thread, before
/// any audio state is touched, from a plain worker thread (the bus calls block); returns what
/// happened for the caller's log line.
#[cfg(target_os = "linux")]
pub fn boost_current_thread() -> Boost {
    // SAFETY: three by-value integers, no pointers; `PRIO_PROCESS` with `who == 0` targets the
    // calling thread on Linux and only adjusts its nice value.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, NICE) } == 0 {
        return Boost::Setpriority;
    }
    // SAFETY: `gettid` takes no arguments, touches no memory, and returns the calling thread's
    // kernel tid — always valid on Linux.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
    let pid = u64::from(std::process::id());
    if std::path::Path::new("/.flatpak-info").exists() {
        match linux_bus::portal_high_priority(pid, tid, NICE) {
            Ok(()) => Boost::Portal,
            Err(e) => Boost::Refused(format!("realtime portal: {e}")),
        }
    } else {
        match linux_bus::rtkit_high_priority(pid, tid, NICE) {
            Ok(()) => Boost::Rtkit,
            Err(e) => Boost::Refused(format!("rtkit: {e}")),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_bus {
    /// One-shot blocking system-bus call to rtkit. Per-call connection rather than cached: this
    /// runs a handful of times per session (thread starts), and holding a bus connection for the
    /// session's lifetime to save microseconds is a bad trade against a wedged bus daemon
    /// pinning a socket in every session forever. Mirrors `pf_frame::thread_qos`.
    pub(super) fn rtkit_high_priority(pid: u64, tid: u64, nice: i32) -> Result<(), zbus::Error> {
        let conn = zbus::blocking::Connection::system()?;
        // `MakeThreadHighPriorityWithPID(u64 process, u64 thread, i32 priority)` — priority is a
        // nice level, floored by rtkit's MinNiceLevel. The WithPID variant with our own pid is
        // the explicit spelling of "this thread of this process"; rtkit still authenticates the
        // caller via the bus and hands the target process to polkit.
        conn.call_method(
            Some("org.freedesktop.RealtimeKit1"),
            "/org/freedesktop/RealtimeKit1",
            Some("org.freedesktop.RealtimeKit1"),
            "MakeThreadHighPriorityWithPID",
            &(pid, tid, nice),
        )?;
        Ok(())
    }

    /// The same request through the Realtime portal on the SESSION bus — the sandbox's own
    /// pid/tid, which the portal maps before it calls rtkit. Same method name and signature
    /// (`tti`), on `org.freedesktop.portal.Desktop` at `/org/freedesktop/portal/desktop`.
    pub(super) fn portal_high_priority(pid: u64, tid: u64, nice: i32) -> Result<(), zbus::Error> {
        let conn = zbus::blocking::Connection::session()?;
        conn.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Realtime"),
            "MakeThreadHighPriorityWithPID",
            &(pid, tid, nice),
        )?;
        Ok(())
    }
}

/// Raise the CALLING thread's priority for audio work: MMCSS "Pro Audio" plus the highest
/// normal-class thread priority. Returns what happened for the caller's log line.
#[cfg(windows)]
pub fn boost_current_thread() -> Boost {
    // Declared here rather than through the `windows` crate's feature list: two calls, both
    // stable Win32 exports, and `pf_frame::session_tuning` already spells
    // `AvSetMmThreadCharacteristicsW` this way. A raw HANDLE is a pointer-sized integer; NULL
    // means failure for AvSet…, and SetThreadPriority returns a BOOL.
    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    }
    const THREAD_PRIORITY_HIGHEST: i32 = 2;
    // SAFETY: C-ABI FFI declared with matching `extern "system"` signatures. `task` is a local
    // NUL-terminated UTF-16 buffer alive for the whole call, so `task.as_ptr()` is a valid
    // LPCWSTR; `&mut idx` is a live local u32 the call writes the task index into. The returned
    // MMCSS handle is intentionally leaked — the OS reverts the characteristics at thread exit —
    // so there is nothing to free. `GetCurrentThread` returns a pseudo-handle that needs no
    // closing; `SetThreadPriority` takes only that handle and a flag.
    let (mmcss, prio) = unsafe {
        let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut idx: u32 = 0;
        let h = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx);
        let p = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        (h != 0, p != 0)
    };
    match (mmcss, prio) {
        (true, true) => Boost::Mmcss,
        (true, false) => Boost::Refused("MMCSS ok, SetThreadPriority refused".into()),
        (false, true) => Boost::Refused("SetThreadPriority ok, MMCSS refused".into()),
        (false, false) => Boost::Refused("MMCSS and SetThreadPriority refused".into()),
    }
}

/// What [`boost_current_thread`] managed. Logged, never acted on: every path is best-effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boost {
    /// Linux: `setpriority` was honoured (RLIMIT_NICE allowed it).
    #[cfg(target_os = "linux")]
    Setpriority,
    /// Linux, sandboxed: the Realtime portal granted the nice level.
    #[cfg(target_os = "linux")]
    Portal,
    /// Linux: rtkit granted the nice level after `setpriority` was refused (SteamOS).
    #[cfg(target_os = "linux")]
    Rtkit,
    /// Windows: MMCSS "Pro Audio" plus `THREAD_PRIORITY_HIGHEST`.
    #[cfg(windows)]
    Mmcss,
    /// Nothing was granted; the thread runs exactly as it did before. The string says why.
    Refused(String),
}

impl Boost {
    /// The one-word tag for a log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            Boost::Setpriority => "setpriority",
            #[cfg(target_os = "linux")]
            Boost::Portal => "portal",
            #[cfg(target_os = "linux")]
            Boost::Rtkit => "rtkit",
            #[cfg(windows)]
            Boost::Mmcss => "mmcss",
            Boost::Refused(_) => "refused",
        }
    }
}

/// Boost the calling thread and log the outcome under `what` — the shape every audio thread
/// start uses.
pub fn boost_and_log(what: &'static str) {
    match boost_current_thread() {
        Boost::Refused(why) => {
            tracing::debug!(thread = what, why = %why, "audio thread priority refused");
        }
        got => tracing::debug!(
            thread = what,
            via = got.as_str(),
            "audio thread priority raised"
        ),
    }
}
