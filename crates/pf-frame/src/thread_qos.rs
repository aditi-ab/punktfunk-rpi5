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
        // and send (non-critical) threads, nothing else. Silently no-ops without CAP_SYS_NICE / a
        // raised RLIMIT_NICE, which is fine. We deliberately do NOT use SCHED_RR/FIFO by default: a
        // realtime CPU class can preempt the compositor AND the game's own render thread, adding the
        // very frame-time we refuse to add (opt-in only — see PUNKTFUNK_SCHED_RR).
        let nice = if critical { -10 } else { -5 };
        // SAFETY: `setpriority` takes three by-value integers and no pointers, so there is nothing to
        // alias or outlive. `PRIO_PROCESS` with `who == 0` targets the calling task on Linux and
        // `nice` is in range; the call only adjusts this thread's scheduling nice value and returns an
        // `int` we inspect. No memory is touched.
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
        if rc == 0 {
            tracing::debug!(critical, nice, "thread nice raised");
        } else {
            tracing::debug!(
                critical,
                "setpriority(nice) no-op (needs CAP_SYS_NICE / RLIMIT_NICE)"
            );
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = critical;
    }
}
