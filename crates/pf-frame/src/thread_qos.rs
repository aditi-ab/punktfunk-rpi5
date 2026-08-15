//! Per-thread OS scheduling QoS for the data plane (plan §W1/§W6 — now in the shared `pf-frame`
//! leaf). The capture/encode and send threads raise their own priority so a CPU-saturating game
//! can't deschedule them; the native, GameStream, and direct-NVENC send threads all reach this the
//! same way (`pf_frame::thread_qos::boost_thread_priority`).

/// Raise the current thread's OS scheduling priority so a CPU-heavy game can't deschedule our
/// capture/encode/send threads. This matters even though our GPU work is already HIGH priority: the
/// GPU scheduler can only favour commands we've actually SUBMITTED, so if a normal-priority thread is
/// descheduled by the game it submits the convert/encode late and the GPU priority never bites. Apollo
/// does the same (capture thread CRITICAL, encoder ABOVE_NORMAL). The Linux host needs this too: an
/// uncapped GPU-saturating title (e.g. CS2 direct on a virtual output, not capped by gamescope) is
/// also a CPU hog and can deschedule our submit threads. `critical` → highest non-realtime class
/// (the capture+encode loop); otherwise above-normal (the send/relay thread).
pub fn boost_thread_priority(critical: bool) {
    // Windows host-process/thread session tuning (timer 1ms, DWM MMCSS, HIGH class once; MMCSS +
    // keep-display-awake per thread). No-op off Windows. Both stream threads call us, so this covers
    // capture/encode (critical) and send (non-critical).
    crate::session_tuning::on_hot_thread();
    #[cfg(target_os = "windows")]
    // SAFETY: `GetCurrentThread()` returns the constant pseudo-handle for the calling thread — always
    // valid, thread-local in meaning, and never closed (no leak/double-close). `SetThreadPriority`
    // takes that handle plus a `THREAD_PRIORITY_*` value the windows crate defines (HIGHEST or
    // ABOVE_NORMAL here); it only reprioritizes this OS thread, borrows no Rust memory, and its
    // `Result` is matched (a failure is logged, never UB). No pointers, lifetimes, or aliasing.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
            THREAD_PRIORITY_HIGHEST,
        };
        let prio = if critical {
            THREAD_PRIORITY_HIGHEST
        } else {
            THREAD_PRIORITY_ABOVE_NORMAL
        };
        match SetThreadPriority(GetCurrentThread(), prio) {
            Ok(()) => tracing::debug!(critical, "thread priority raised"),
            Err(e) => {
                tracing::debug!(critical, error = ?e, "SetThreadPriority failed")
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Best-effort nice of the CALLING thread. On Linux `setpriority(PRIO_PROCESS, 0, …)` acts on
        // the calling thread (the kernel resolves who==0 to the current task/tid), and both call
        // sites run inside their worker thread — so this nices exactly the capture/encode (critical)
        // and send (non-critical) threads, nothing else. We deliberately do NOT use SCHED_RR/FIFO by
        // default: a realtime CPU class can preempt the compositor AND the game's own render thread,
        // adding the very frame-time we refuse to add (opt-in only — see PUNKTFUNK_SCHED_RR).
        let nice = if critical { -10 } else { -5 };
        // SAFETY: `setpriority` takes three by-value integers and no pointers, so there is nothing to
        // alias or outlive. `PRIO_PROCESS` with `who == 0` targets the calling task on Linux and
        // `nice` is in range; the call only adjusts this thread's scheduling nice value and returns an
        // `int` we inspect. No memory is touched.
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
        if rc == 0 {
            tracing::debug!(critical, nice, "thread nice raised");
        } else {
            // The direct call needs CAP_SYS_NICE or a raised RLIMIT_NICE, and the host binary can
            // NEVER carry a file capability (a capped process's /proc/<pid>/exe is unreadable to
            // KWin, which kills desktop streaming — the 0.26.0-1 field incident). RealtimeKit is
            // the sanctioned unprivileged path: the same broker PipeWire's clients use, present on
            // effectively every desktop install. Packaging also ships a `user@.service.d`
            // LimitNICE drop-in so the direct call works on rtkit-less boxes — but only from the
            // next login, and existing installs upgrade the binary alone; rtkit is what fixes the
            // installed base. A 2026-08-14 field log showed exactly this rung missing: every
            // fresh-launch shader storm descheduled the unprioritized audio/send threads.
            match linux_rtkit::make_high_priority(nice) {
                Ok(()) => tracing::debug!(critical, nice, "thread nice raised via rtkit"),
                Err(e) => tracing::debug!(
                    critical,
                    reason = %e,
                    "setpriority(nice) no-op (needs CAP_SYS_NICE / RLIMIT_NICE, and rtkit \
                     was unavailable)"
                ),
            }
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = critical;
    }
}

/// What the OS is actually giving the CALLING thread: `(policy, rt_priority, nice)`.
///
/// Exists because a boost we *asked for* and a boost the hot thread *has* turned out to be
/// different questions. Callbacks handed to a library — PipeWire's `RT_PROCESS` streams above all
/// — run on a thread that library created and schedules, so a `boost_thread_priority` call in our
/// own setup path can log a cheerful success about a thread that never touches audio. A
/// 2026-08-15 measurement found exactly that shape: our loop thread at SCHED_OTHER/0 while the
/// data loop actually running the capture callback sat at SCHED_RR/20, both in the same process.
///
/// Report this from inside the hot callback, where "the calling thread" is the one that matters.
#[cfg(target_os = "linux")]
pub fn current_thread_sched() -> (&'static str, i32, i32) {
    // SAFETY: all three calls take by-value integers (plus, for `sched_getparam`, a pointer to a
    // fully-initialised local we own and outlive) and return integers. `0` means "the calling
    // task" on Linux, so nothing outside this thread is read or written, and no allocation,
    // locking or blocking happens — which is what makes this callable from an RT callback.
    unsafe {
        let policy = libc::sched_getscheduler(0);
        let mut param: libc::sched_param = std::mem::zeroed();
        let rt_priority = if libc::sched_getparam(0, &mut param) == 0 {
            param.sched_priority
        } else {
            -1
        };
        // `getpriority` legitimately returns -1, so errno is the only way to tell a nice of -1
        // from a failure.
        *libc::__errno_location() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, 0);
        let nice = if *libc::__errno_location() == 0 {
            nice
        } else {
            0
        };
        let policy = match policy {
            libc::SCHED_FIFO => "SCHED_FIFO",
            libc::SCHED_RR => "SCHED_RR",
            libc::SCHED_OTHER => "SCHED_OTHER",
            libc::SCHED_BATCH => "SCHED_BATCH",
            libc::SCHED_IDLE => "SCHED_IDLE",
            _ => "unknown",
        };
        (policy, rt_priority, nice)
    }
}

/// RealtimeKit fallback for [`boost_thread_priority`]: ask the system-bus broker
/// (`org.freedesktop.RealtimeKit1`) to renice the calling thread when the direct
/// `setpriority` was refused. This is how PulseAudio/PipeWire clients get their boosts on a
/// stock desktop — no capability anywhere, which matters here because a file capability on the
/// host binary breaks KWin's client identification outright.
///
/// Only the high-priority (nice) verb is used, never `MakeThreadRealtime` — the SCHED_RR
/// reservations in [`boost_thread_priority`]'s comment apply to rtkit-granted RR too (and the
/// RT verb additionally demands an RLIMIT_RTTIME we don't set).
#[cfg(target_os = "linux")]
mod linux_rtkit {
    /// One-shot blocking D-Bus call. Must be made from a plain worker thread, never from async
    /// context — which already holds for every caller: `boost_thread_priority` acts on the
    /// calling thread, so it only ever runs inside the dedicated capture/encode/send threads.
    /// The connection is per-call rather than cached: this runs at most a handful of times per
    /// session (thread starts), and holding a system-bus connection for the session's lifetime
    /// to save microseconds at session start is a bad trade against a wedged bus daemon pinning
    /// a socket in every session forever.
    pub(super) fn make_high_priority(nice: i32) -> Result<(), zbus::Error> {
        // SAFETY: `gettid` takes no arguments, touches no memory, and returns the calling
        // thread's kernel tid — always valid on Linux.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
        let pid = u64::from(std::process::id());
        let conn = zbus::blocking::Connection::system()?;
        // `MakeThreadHighPriorityWithPID(u64 process, u64 thread, i32 priority)` — priority is a
        // nice level, floored by rtkit's MinNiceLevel (defaults well below our -10). The WithPID
        // variant with our own pid is the explicit spelling of "this thread of this process";
        // rtkit still authenticates the caller via the bus, so it grants nothing a plain
        // `setpriority` caller couldn't be granted.
        conn.call_method(
            Some("org.freedesktop.RealtimeKit1"),
            "/org/freedesktop/RealtimeKit1",
            Some("org.freedesktop.RealtimeKit1"),
            "MakeThreadHighPriorityWithPID",
            &(pid, tid, nice),
        )?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// Non-vacuity: the introspection has to come back with something the OS could actually have
    /// said. A helper whose whole job is to be quoted in a field log is worthless if it can
    /// quietly report a placeholder, and it only ever runs on hosts nobody can attach a debugger
    /// to.
    #[test]
    fn current_thread_sched_reports_a_real_policy() {
        let (policy, rt_priority, nice) = super::current_thread_sched();
        assert!(
            matches!(
                policy,
                "SCHED_OTHER" | "SCHED_RR" | "SCHED_FIFO" | "SCHED_BATCH" | "SCHED_IDLE"
            ),
            "unrecognised policy {policy}"
        );
        assert!(
            (0..=99).contains(&rt_priority),
            "rt priority {rt_priority} outside the kernel's range"
        );
        assert!((-20..=19).contains(&nice), "nice {nice} outside PRIO range");
    }
}
