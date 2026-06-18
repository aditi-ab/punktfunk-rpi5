//! Windows host-process session tuning — parity with Apollo/Sunshine `streaming_will_start`.
//!
//! The default Windows process runs at NORMAL priority and ~15.6 ms timer granularity, and lets the
//! GPU/display idle. Under a GPU-saturating game that starves our capture/encode/send threads (the
//! "240→40 fps collapse"), and the coarse timer floors any precise frame pacing. This raises the
//! process out of the default scheduling class, gives DWM and our hot threads MMCSS priority, drops
//! the timer to 1 ms, and keeps the (virtual) display awake for the session.
//!
//! Raw C-ABI FFI (winmm/kernel32/dwmapi/avrt) rather than the `windows` crate so it builds without
//! pulling new windows-rs features. No-op on non-Windows. Per-thread effects (MMCSS, execution
//! state) auto-revert at thread exit (= session end); the process-wide bits revert at process exit.
//! See `docs/host-latency-plan.md` Tier 3A.

#[cfg(target_os = "windows")]
mod imp {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Handle = *mut c_void;
    type Bool = i32;

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(uPeriod: u32) -> u32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn SetPriorityClass(hProcess: Handle, dwPriorityClass: u32) -> Bool;
        fn SetThreadExecutionState(esFlags: u32) -> u32;
    }
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmEnableMMCSS(fEnableMMCSS: Bool) -> i32; // HRESULT
    }
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(TaskName: *const u16, TaskIndex: *mut u32) -> Handle;
    }

    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    static PROCESS_TUNED: OnceLock<()> = OnceLock::new();

    /// Process-wide tuning, applied exactly once. Reverts at process exit. Best-effort: each call is
    /// independent and a failure is ignored (e.g. a non-elevated host may not get HIGH class).
    fn tune_process_once() {
        PROCESS_TUNED.get_or_init(|| unsafe {
            // 1 ms timer granularity (default ~15.6 ms) — the floor for precise frame pacing and the
            // encode|send split's sub-ms sleeps.
            timeBeginPeriod(1);
            // Run DWM's compositor work at MMCSS priority — helps the compose-rate ceiling hold up
            // under a saturating game (capture is bounded by how often DWM composes).
            DwmEnableMMCSS(1);
            // Lift the whole host above NORMAL so a CPU-saturating game can't deschedule our
            // control/capture/encode/send threads on the CPU (Apollo does the same).
            SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            tracing::info!("windows session tuning applied (timer 1ms, DWM MMCSS, HIGH priority)");
        });
    }

    /// Call at the start of each capture/encode/send (hot stream) thread. Applies the process-wide
    /// tuning once, registers the calling thread with MMCSS ("Games"), and asserts the display/system
    /// must stay awake for as long as this thread lives. The MMCSS handle is intentionally leaked and
    /// the execution-state assertion is bound to this thread — both are reverted by the OS when the
    /// thread exits, so a session that ends tears them down without explicit bookkeeping.
    pub fn on_hot_thread() {
        tune_process_once();
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            let task: Vec<u16> = "Games\0".encode_utf16().collect();
            let mut idx: u32 = 0;
            // Leak the handle: these are session/process-lifetime worker threads; the OS reverts the
            // MMCSS characteristics at thread exit.
            let _ = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx);
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::on_hot_thread;

/// No-op on non-Windows (Linux uses `setpriority` nice + CUDA stream priority instead — see
/// `punktfunk1::boost_thread_priority` and `zerocopy::cuda`).
#[cfg(not(target_os = "windows"))]
pub fn on_hot_thread() {}
