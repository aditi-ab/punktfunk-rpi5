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
//! state) auto-revert at thread exit (= session end); the process-wide bits are refcounted over
//! the hot threads and revert when the LAST one exits — the host must not keep HIGH priority and
//! a 1 ms global timer while a local game runs and nobody streams (2026-08-12 field report).
//! See `design/host-latency-plan.md` Tier 3A.

#[cfg(target_os = "windows")]
mod imp {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::Mutex;

    type Handle = *mut c_void;
    type Bool = i32;

    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(uPeriod: u32) -> u32;
        fn timeEndPeriod(uPeriod: u32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn SetPriorityClass(hProcess: Handle, dwPriorityClass: u32) -> Bool;
        fn SetThreadExecutionState(esFlags: u32) -> u32;
        fn PowerCreateRequest(Context: *const ReasonContext) -> Handle;
        fn PowerSetRequest(PowerRequest: Handle, RequestType: i32) -> Bool;
        fn PowerClearRequest(PowerRequest: Handle, RequestType: i32) -> Bool;
        fn CloseHandle(hObject: Handle) -> Bool;
    }

    /// `REASON_CONTEXT` (minwinbase.h), simple-string flavour: `Version` (ULONG), `Flags` (DWORD),
    /// then the union collapses to `SimpleReasonString` (LPWSTR) under
    /// `POWER_REQUEST_CONTEXT_SIMPLE_STRING` — same size/alignment as the C layout (4+4, 8-aligned
    /// pointer).
    #[repr(C)]
    struct ReasonContext {
        version: u32,
        flags: u32,
        simple_reason: *const u16,
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmEnableMMCSS(fEnableMMCSS: Bool) -> i32; // HRESULT
    }
    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(TaskName: *const u16, TaskIndex: *mut u32) -> Handle;
    }

    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
    const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    const POWER_REQUEST_CONTEXT_VERSION: u32 = 0; // DIAGNOSTIC_REASON_VERSION
    const POWER_REQUEST_CONTEXT_SIMPLE_STRING: u32 = 0x0000_0001;
    const POWER_REQUEST_DISPLAY_REQUIRED: i32 = 0;
    const POWER_REQUEST_SYSTEM_REQUIRED: i32 = 1;
    const INVALID_HANDLE_VALUE: isize = -1;

    /// RAII display+system availability request (`PowerRequestDisplayRequired`, visible in
    /// `powercfg /requests`) — the service-grade "someone is watching this screen" assertion,
    /// held for a capture session so the console cannot drop into display-off mid-stream. This is
    /// object-lifetime (unlike the thread-bound `ES_*` flags in [`on_hot_thread`], which the OS
    /// reverts at thread exit), so a capturer can hold it across whatever threads serve the
    /// session. PREVENTION only: no power request turns an already-off display back ON — that
    /// wake needs input, which is the virtual-mouse compose kick's job.
    pub struct DisplayWakeRequest(Handle);

    // SAFETY: the wrapped power-request HANDLE is a kernel object handle — a plain opaque value
    // that any thread may use; this type never aliases it (set at new, cleared+closed at drop).
    unsafe impl Send for DisplayWakeRequest {}

    impl DisplayWakeRequest {
        /// Create + set the request. `None` when the kernel refuses (best-effort — the caller
        /// streams without the assertion, exactly the pre-existing behavior).
        pub fn new() -> Option<DisplayWakeRequest> {
            let reason: Vec<u16> = "punktfunk streaming session\0".encode_utf16().collect();
            let ctx = ReasonContext {
                version: POWER_REQUEST_CONTEXT_VERSION,
                flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                simple_reason: reason.as_ptr(),
            };
            // SAFETY: `ctx` (and the reason buffer it points into) outlives the call, which copies
            // the string into the kernel object; the returned handle is owned here and released in
            // Drop. PowerSetRequest takes the just-created handle + a plain enum value.
            unsafe {
                let h = PowerCreateRequest(&ctx);
                if h.is_null() || h as isize == INVALID_HANDLE_VALUE {
                    return None;
                }
                PowerSetRequest(h, POWER_REQUEST_DISPLAY_REQUIRED);
                PowerSetRequest(h, POWER_REQUEST_SYSTEM_REQUIRED);
                Some(DisplayWakeRequest(h))
            }
        }
    }

    impl Drop for DisplayWakeRequest {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the owned, still-open power-request handle (created in `new`,
            // dropped exactly once); clear + close are plain handle calls.
            unsafe {
                PowerClearRequest(self.0, POWER_REQUEST_DISPLAY_REQUIRED);
                PowerClearRequest(self.0, POWER_REQUEST_SYSTEM_REQUIRED);
                CloseHandle(self.0);
            }
        }
    }

    /// Live hot (session) threads. A Mutex, not an atomic: the 0↔1 transitions carry the
    /// apply/revert side effects, and an interleaved fetch_add/fetch_sub pair could otherwise
    /// finish with a running session untuned (transitions are rare — thread start/exit only).
    static HOT_THREADS: Mutex<usize> = Mutex::new(0);

    /// Process-wide tuning, applied when the FIRST hot thread registers. Best-effort: each call
    /// is independent and a failure is ignored (e.g. a non-elevated host may not get HIGH class).
    fn tune_process() {
        // SAFETY: each call is a C-ABI FFI into winmm/kernel32/dwmapi declared with a matching
        // `extern "system"` signature; every argument is a plain integer (no pointers/buffers escape),
        // and `GetCurrentProcess()` returns the current-process pseudo-handle (a constant, always valid,
        // never closed).
        unsafe {
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
        }
    }

    /// The mirror of [`tune_process`], run when the LAST hot thread exits. Leaving the tuning in
    /// place used to be the design ("reverts at process exit") — but the host is a 24/7 service,
    /// so after one stream it competed at HIGH class with a 1 ms global timer against whatever
    /// the user played locally, forever.
    ///
    /// 🛑 **Nothing in here may log, or touch anything that logs.** This runs from
    /// [`HotThreadGuard`]'s `Drop`, which is a **TLS destructor** — and by then this thread's
    /// *other* thread-locals may already be gone, including the ones `tracing_subscriber`'s
    /// registry keeps (it is `sharded-slab`-backed, and the slab's per-thread registration is a
    /// `thread_local!` read with `LocalKey::with`). Emitting an event here panicked with "cannot
    /// access a Thread Local Storage value during or after destruction", and **a panic that
    /// escapes a TLS destructor is fatal in Rust** — `fatal runtime error: thread local panicked
    /// on drop, aborting`. So one `info!` line killed the whole host on session teardown and the
    /// SCM restarted it ~6 s later, which read in the field as a mystery reconnect (on glass,
    /// .173: four aborts, every one of them a session teardown).
    ///
    /// The revert itself is only FFI and stays here, inside the refcount lock, so it remains
    /// atomic against a session starting concurrently. The counterpart "applied" line in
    /// [`tune_process`] runs on a live thread and is kept — that one is safe.
    fn untune_process() {
        // SAFETY: same FFI surface as `tune_process` — plain-integer arguments, constant
        // pseudo-handle, no pointers or buffers. Sound in a TLS destructor: no Rust TLS is read.
        unsafe {
            timeEndPeriod(1); // pairs the timeBeginPeriod(1)
            DwmEnableMMCSS(0);
            SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
        }
    }

    /// One per hot thread, parked in TLS by [`on_hot_thread`]; its Drop runs at thread exit
    /// (= session teardown), the same lifetime the MMCSS/execution-state effects already ride.
    struct HotThreadGuard;

    impl Drop for HotThreadGuard {
        fn drop(&mut self) {
            // ⚠ TLS DESTRUCTOR. Everything reached from here must be panic-free and must not log —
            // see [`untune_process`] for what a single `info!` here cost. A poisoned lock skips the
            // revert (best-effort, like every call here) rather than panicking.
            if let Ok(mut n) = HOT_THREADS.lock() {
                *n -= 1;
                if *n == 0 {
                    untune_process();
                }
            }
        }
    }

    thread_local! {
        static HOT_THREAD: std::cell::OnceCell<HotThreadGuard> =
            const { std::cell::OnceCell::new() };
    }

    /// Call at the start of each capture/encode/send (hot stream) thread. Registers the thread in
    /// the process-tuning refcount (first in applies, last out reverts), registers it with MMCSS
    /// ("Games"), and asserts the display/system must stay awake for as long as this thread lives.
    /// The MMCSS handle is intentionally leaked and the execution-state assertion is bound to this
    /// thread — both are reverted by the OS when the thread exits, and the refcount guard's TLS
    /// Drop runs there too, so a session that ends tears everything down without explicit
    /// bookkeeping.
    pub fn on_hot_thread() {
        HOT_THREAD.with(|slot| {
            if slot.get().is_none() {
                {
                    let mut n = HOT_THREADS.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        tune_process();
                    }
                }
                let _ = slot.set(HotThreadGuard);
            }
        });
        // SAFETY: C-ABI FFI declared with matching `extern "system"` signatures. SetThreadExecutionState
        // takes only flag bits. `task` is a local NUL-terminated UTF-16 buffer ("Games\0") alive for the
        // whole block, so `task.as_ptr()` is a valid LPCWSTR for the call, and `&mut idx` is a live local
        // u32 the call writes the task index into. The returned MMCSS handle is intentionally leaked (the
        // OS reverts the characteristics at thread exit), so there is nothing to free or double-free.
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
pub use imp::{on_hot_thread, DisplayWakeRequest};

/// No-op on non-Windows (Linux uses `setpriority` nice + CUDA stream priority instead — see
/// `native::boost_thread_priority` and `zerocopy::cuda`).
#[cfg(not(target_os = "windows"))]
pub fn on_hot_thread() {}
