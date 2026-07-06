//! Android Adaptive Performance Framework (ADPF) — CPU performance hints for the decode thread.
//!
//! ADPF lets a latency-critical app tell the platform "these threads run a repeating workload with
//! this per-cycle deadline, and here's how long they *actually* took." The kernel's CPU governor
//! (on Qualcomm Snapdragon in particular — its ADPF backend is among the most responsive) then keeps
//! those threads on the fast cores at high clocks instead of migrating them to a little core or
//! down-clocking between frames. For a stream client the win is on the in-process hot path we
//! control — the `pf-decode` feed/drain/present loop — *not* the hardware codec itself (that decodes
//! in the mediacodec service, a separate process we can't hint); keeping our loop from being
//! scheduled late directly trims the jitter between "AU received" and "buffer released to the
//! Surface." It complements the codec-side `operating-rate`/`priority` hints, which push the codec's
//! own clocks.
//!
//! The `APerformanceHint_*` API arrived in NDK **API level 33**. minSdk is 31, so we CANNOT link the
//! symbols directly: a `libpunktfunk_android.so` carrying an unresolved
//! `APerformanceHint_createSession` import fails to load on API 31/32 devices
//! (`System.loadLibrary` throws) even if the code path is never taken. Instead we resolve the
//! entry points from `libandroid.so` with `dlsym` at runtime — absent on < 33 ⇒
//! [`HintSession::create`] returns `None` and the decode loop simply runs without hints.

use std::ffi::c_void;
use std::os::raw::c_int;

// `APerformanceHint_*` function-pointer types. The manager/session handles are opaque, so we treat
// them as `*mut c_void`.
type GetManagerFn = unsafe extern "C" fn() -> *mut c_void;
type CreateSessionFn = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type ReportFn = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type UpdateTargetFn = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type CloseFn = unsafe extern "C" fn(*mut c_void);
type SetPreferPowerEfficiencyFn = unsafe extern "C" fn(*mut c_void, bool) -> c_int;

/// The entry points we use, resolved once from `libandroid.so`, plus the process-wide manager.
struct Api {
    create_session: CreateSessionFn,
    report: ReportFn,
    update_target: UpdateTargetFn,
    close: CloseFn,
    /// `APerformanceHint_setPreferPowerEfficiency` — NDK **API 35**, so `Option`al even when the
    /// rest of ADPF resolved (a 33/34 device has the session API but not this one).
    set_prefer_power_efficiency: Option<SetPreferPowerEfficiencyFn>,
    manager: *mut c_void,
}

/// Resolve the ADPF entry points + the process manager, or `None` on API < 33 (symbols absent) or if
/// the manager is unavailable.
fn resolve_api() -> Option<Api> {
    // SAFETY: `dlopen` of an always-present system library with a NUL-terminated name; it returns
    // null on failure (checked below). `libandroid.so` is already mapped into every app process, so
    // this only bumps its refcount — we intentionally never `dlclose` (process-lifetime handle).
    let lib = unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW) };
    if lib.is_null() {
        return None;
    }
    // SAFETY: `dlsym` on the valid handle above with NUL-terminated symbol names; each returns null
    // when the symbol is absent (device API < 33), which we check before transmuting the non-null
    // pointer to its fn-pointer type (layout-compatible; a resolved symbol is a valid code address).
    unsafe {
        let get_manager = libc::dlsym(lib, c"APerformanceHint_getManager".as_ptr());
        let create_session = libc::dlsym(lib, c"APerformanceHint_createSession".as_ptr());
        let report = libc::dlsym(lib, c"APerformanceHint_reportActualWorkDuration".as_ptr());
        let update_target = libc::dlsym(lib, c"APerformanceHint_updateTargetWorkDuration".as_ptr());
        let close = libc::dlsym(lib, c"APerformanceHint_closeSession".as_ptr());
        if get_manager.is_null()
            || create_session.is_null()
            || report.is_null()
            || update_target.is_null()
            || close.is_null()
        {
            return None; // device API < 33 — no ADPF
        }
        let get_manager = std::mem::transmute::<*mut c_void, GetManagerFn>(get_manager);
        let manager = get_manager();
        if manager.is_null() {
            return None;
        }
        // Optional (API 35): resolve if present, else `None` — the session still works without it.
        let set_prefer_power_efficiency =
            libc::dlsym(lib, c"APerformanceHint_setPreferPowerEfficiency".as_ptr());
        let set_prefer_power_efficiency = (!set_prefer_power_efficiency.is_null()).then(|| {
            std::mem::transmute::<*mut c_void, SetPreferPowerEfficiencyFn>(
                set_prefer_power_efficiency,
            )
        });
        Some(Api {
            create_session: std::mem::transmute::<*mut c_void, CreateSessionFn>(create_session),
            report: std::mem::transmute::<*mut c_void, ReportFn>(report),
            update_target: std::mem::transmute::<*mut c_void, UpdateTargetFn>(update_target),
            close: std::mem::transmute::<*mut c_void, CloseFn>(close),
            set_prefer_power_efficiency,
            manager,
        })
    }
}

/// A live ADPF hint session bound to a set of thread ids. Dropping it closes the session. Holds raw
/// handles, so it is `!Send`/`!Sync` — created and used only on the `pf-decode` thread.
pub struct HintSession {
    api: Api,
    session: *mut c_void,
}

impl HintSession {
    /// Open a session hinting `tids` with an initial per-frame target of `target_ns` nanoseconds.
    /// `None` when ADPF is unavailable (device API < 33) or the platform declines — the caller then
    /// runs unhinted (a no-op, not an error). `prefer_performance` (the experimental low-latency
    /// mode) additionally biases the governor away from power efficiency (API 35+); off, the
    /// session runs with the platform default, as it did before the overhaul.
    pub fn create(target_ns: i64, tids: &[i32], prefer_performance: bool) -> Option<Self> {
        if target_ns <= 0 || tids.is_empty() {
            return None;
        }
        let api = resolve_api()?;
        // SAFETY: `api.manager` is the live process manager returned above; `tids` is a valid slice
        // of `len` i32s that `createSession` copies; it returns null on failure (checked).
        let session =
            unsafe { (api.create_session)(api.manager, tids.as_ptr(), tids.len(), target_ns) };
        if session.is_null() {
            return None;
        }
        // Tell the governor NOT to bias this session toward power efficiency (API 35+): our loop is
        // latency-critical, so we want it kept on fast cores at high clocks over battery savings.
        // Best-effort; absent below API 35.
        if prefer_performance {
            if let Some(f) = api.set_prefer_power_efficiency {
                // SAFETY: `session` is the live session just created; the fn takes it + a bool.
                unsafe { f(session, false) };
            }
        }
        Some(Self { api, session })
    }

    /// Report the wall-clock time the hinted thread spent producing the last displayed frame. When
    /// it exceeds the session target the governor boosts the cores running the thread; when it
    /// stays under, clocks may relax. No-op on a non-positive duration (the API rejects it).
    pub fn report_actual(&self, actual_ns: i64) {
        if actual_ns <= 0 {
            return;
        }
        // SAFETY: `self.session` is a live session for `self`'s lifetime.
        unsafe { (self.api.report)(self.session, actual_ns) };
    }

    /// Update the per-frame target (e.g. after a mid-session refresh-rate change). Unused today —
    /// the decode thread restarts on renegotiation — but kept for that path.
    #[allow(dead_code)]
    pub fn update_target(&self, target_ns: i64) {
        if target_ns <= 0 {
            return;
        }
        // SAFETY: `self.session` is a live session for `self`'s lifetime.
        unsafe { (self.api.update_target)(self.session, target_ns) };
    }
}

impl Drop for HintSession {
    fn drop(&mut self) {
        // SAFETY: `self.session` was created by `createSession` and is closed exactly once, here.
        unsafe { (self.api.close)(self.session) };
    }
}
