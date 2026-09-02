//! Best-effort priority for the client's audio feeder threads (decode, pad-audio, WASAPI
//! render/mic). Device callbacks already run on the OS realtime path; these threads do not.
//! A decode thread descheduled past ring depth becomes an underrun.
//!
//! Linux, first success wins: `setpriority(-10)` (needs `RLIMIT_NICE`; a no-op when the limit
//! is 0); then the Realtime portal when `/.flatpak-info` exists (rtkit looks up `/proc/<pid>`
//! with sandbox numbers and gets ENOENT); otherwise rtkit `MakeThreadHighPriorityWithPID` on
//! the system bus. Polkit `acquire-high-priority` allows the user's session and session-less
//! user services, not ssh. Never `setcap`/`SCHED_RR`: `cap_sys_nice` took down KDE sessions,
//! and nice is enough.
//!
//! Windows: MMCSS "Pro Audio" plus `THREAD_PRIORITY_HIGHEST`. The MMCSS handle is leaked
//! (thread-lifetime; the OS reverts at exit), matching `pf_frame::session_tuning::on_hot_thread`.
//! Refusal leaves the thread as it was.

/// `-10` sits inside rtkit's default `MinNiceLevel` (−15), matching the host hot threads.
#[cfg(target_os = "linux")]
const NICE: i32 = -10;

/// Call at thread start, before audio state; the bus calls block.
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
    /// One-shot system-bus rtkit call. Do not cache the connection: a wedged daemon would pin a
    /// socket for the whole session. Mirrors `pf_frame::thread_qos`.
    pub(super) fn rtkit_high_priority(pid: u64, tid: u64, nice: i32) -> Result<(), zbus::Error> {
        let conn = zbus::blocking::Connection::system()?;
        // `MakeThreadHighPriorityWithPID(u64, u64, i32)`: the i32 is a nice level, floored by
        // MinNiceLevel. WithPID + our pid means this thread of this process.
        conn.call_method(
            Some("org.freedesktop.RealtimeKit1"),
            "/org/freedesktop/RealtimeKit1",
            Some("org.freedesktop.RealtimeKit1"),
            "MakeThreadHighPriorityWithPID",
            &(pid, tid, nice),
        )?;
        Ok(())
    }

    /// Same request via the Realtime portal on the session bus. Pass the sandbox pid/tid; the
    /// portal maps them onto the host before calling rtkit.
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

#[cfg(windows)]
pub fn boost_current_thread() -> Boost {
    // Local FFI, not the `windows` crate: two stable Win32 exports, same spelling as
    // `pf_frame::session_tuning`.
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

/// Logged only; every path is best-effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boost {
    #[cfg(target_os = "linux")]
    Setpriority,
    #[cfg(target_os = "linux")]
    Portal,
    #[cfg(target_os = "linux")]
    Rtkit,
    #[cfg(windows)]
    Mmcss,
    Refused(String),
}

impl Boost {
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
