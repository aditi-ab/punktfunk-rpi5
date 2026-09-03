//! Weston repaint-clock gate for Vulkan WSI when VK_KHR_present_wait is absent.

#![cfg(target_os = "linux")]

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[repr(C)]
struct WlProxy {
    _private: [u8; 0],
}

#[repr(C)]
struct WlInterface {
    _private: [u8; 0],
}

#[repr(C)]
struct WlCallbackListener {
    done: unsafe extern "C" fn(*mut c_void, *mut WlProxy, u32),
}

#[link(name = "wayland-client")]
unsafe extern "C" {
    static wl_callback_interface: WlInterface;
    fn wl_proxy_get_version(proxy: *mut WlProxy) -> c_uint;
    fn wl_proxy_marshal_flags(
        proxy: *mut WlProxy,
        opcode: c_uint,
        interface: *const WlInterface,
        version: c_uint,
        flags: c_uint,
        ...
    ) -> *mut WlProxy;
    fn wl_proxy_add_listener(
        proxy: *mut WlProxy,
        implementation: *mut c_void,
        data: *mut c_void,
    ) -> c_int;
    fn wl_proxy_destroy(proxy: *mut WlProxy);
}

const WL_SURFACE_FRAME: c_uint = 3;

pub(super) struct WaylandFramePacer {
    surface: *mut WlProxy,
    callback: AtomicPtr<WlProxy>,
    ready: AtomicBool,
    wake: Mutex<Option<Box<dyn Fn() + Send>>>,
    arms: AtomicU64,
    completions: AtomicU64,
    report: Mutex<(Instant, u64, u64)>,
}

impl WaylandFramePacer {
    pub(super) fn new(window: &sdl3::video::Window) -> Option<Box<Self>> {
        if std::env::var("PUNKTFUNK_WAYLAND_FRAME_PACING")
            .ok()
            .as_deref()
            == Some("0")
        {
            tracing::info!("Wayland frame pacing disabled by environment");
            return None;
        }
        let key = b"SDL.window.wayland.surface\0";
        // SAFETY: SDL owns the window and property set for this call. The returned
        // wl_surface remains owned by SDL and lives for the window's lifetime.
        let surface = unsafe {
            let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
            sdl3::sys::properties::SDL_GetPointerProperty(
                props,
                key.as_ptr().cast::<c_char>(),
                ptr::null_mut(),
            )
        } as *mut WlProxy;
        if surface.is_null() {
            tracing::info!("SDL window is not Wayland; compositor frame pacing unavailable");
            return None;
        }
        tracing::info!("Wayland compositor frame pacing active");
        Some(Box::new(Self {
            surface,
            callback: AtomicPtr::new(ptr::null_mut()),
            ready: AtomicBool::new(true),
            wake: Mutex::new(None),
            arms: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            report: Mutex::new((Instant::now(), 0, 0)),
        }))
    }

    pub(super) fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub(super) fn set_wake(&self, wake: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(wake);
    }

    /// Attach a one-shot frame request to the next wl_surface commit, which is
    /// performed by vkQueuePresentKHR immediately after this call.
    pub(super) fn arm(&self) {
        if !self.ready.swap(false, Ordering::AcqRel) {
            return;
        }
        // SAFETY: `surface` is SDL's live wl_surface. FRAME creates a wl_callback
        // using the surface proxy's version and event queue (SDL's default queue).
        let callback = unsafe {
            wl_proxy_marshal_flags(
                self.surface,
                WL_SURFACE_FRAME,
                &raw const wl_callback_interface,
                wl_proxy_get_version(self.surface),
                0,
            )
        };
        if callback.is_null() {
            self.ready.store(true, Ordering::Release);
            return;
        }
        self.callback.store(callback, Ordering::Release);
        self.arms.fetch_add(1, Ordering::Relaxed);
        static LISTENER: WlCallbackListener = WlCallbackListener { done };
        // SAFETY: callback is newly created; `self` is boxed and therefore stable
        // for the presenter's lifetime. Drop destroys any outstanding callback.
        let rc = unsafe {
            wl_proxy_add_listener(
                callback,
                (&raw const LISTENER).cast_mut().cast::<c_void>(),
                (self as *const Self).cast_mut().cast::<c_void>(),
            )
        };
        if rc != 0 {
            self.callback.store(ptr::null_mut(), Ordering::Release);
            unsafe { wl_proxy_destroy(callback) };
            self.ready.store(true, Ordering::Release);
        }
    }
}

unsafe extern "C" fn done(data: *mut c_void, callback: *mut WlProxy, _time_ms: u32) {
    // SAFETY: listener data points at the boxed pacer; SDL dispatches this on the
    // presenter's main thread, and Drop removes a pending proxy before freeing it.
    let this = unsafe { &*(data.cast::<WaylandFramePacer>()) };
    this.callback.store(ptr::null_mut(), Ordering::Release);
    unsafe { wl_proxy_destroy(callback) };
    this.ready.store(true, Ordering::Release);
    let completed = this.completions.fetch_add(1, Ordering::Relaxed) + 1;
    let armed = this.arms.load(Ordering::Relaxed);
    let mut report = this.report.lock().unwrap();
    let elapsed = report.0.elapsed();
    if elapsed.as_secs_f64() >= 1.0 {
        tracing::info!(
            armed = armed - report.1,
            completed = completed - report.2,
            elapsed_ms = elapsed.as_millis() as u64,
            "Wayland frame pacing window"
        );
        *report = (Instant::now(), armed, completed);
    }
    drop(report);
    if let Some(wake) = this.wake.lock().unwrap().as_ref() {
        wake();
    }
}

impl Drop for WaylandFramePacer {
    fn drop(&mut self) {
        let callback = self.callback.swap(ptr::null_mut(), Ordering::AcqRel);
        if !callback.is_null() {
            unsafe { wl_proxy_destroy(callback) };
        }
    }
}
