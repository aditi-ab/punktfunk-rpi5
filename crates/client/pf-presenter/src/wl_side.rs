//! What the helpers on SDL's own Wayland connection share: the window's raw `wl_display` and
//! `wl_surface`, a private event queue on that display, the surface as a proxy, the registry's
//! teardown, the clock move, and inside gamescope the display SDL is handed. `wl_feedback`,
//! `wl_native` and `wayland_scroll` keep their protocols, "unsupported" answers and logs.
//!
//! SDL owns the display and reads the socket. Every connection made here borrows that display,
//! so a caller holds the window's `WindowContext` in a field declared after every proxy, queue
//! and connection it makes: the display then outlives the last of them.

use anyhow::{Context as _, Result};
use std::ffi::c_void;
use std::marker::PhantomData;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy};

/// SDL's Wayland objects for one window. All three are borrowed from the window, and this
/// value cannot outlive the borrow.
pub(crate) struct SdlWayland<'w> {
    display: *mut c_void,
    pub(crate) surface: *mut c_void,
    /// SDL's `wp_viewport` on the surface: null where SDL made none.
    pub(crate) viewport: *mut c_void,
    window: PhantomData<&'w sdl3::video::Window>,
}

impl<'w> SdlWayland<'w> {
    /// The window's display and surface, or `None` where SDL exposes no Wayland ones.
    pub(crate) fn of(window: &'w sdl3::video::Window) -> Option<SdlWayland<'w>> {
        // SAFETY: the live window owns the pointers; none transfers ownership.
        let (display, surface, viewport) = unsafe {
            let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
            let get = |name| {
                sdl3::sys::properties::SDL_GetPointerProperty(props, name, std::ptr::null_mut())
            };
            (
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_VIEWPORT_POINTER),
            )
        };
        (!display.is_null() && !surface.is_null()).then_some(SdlWayland {
            display,
            surface,
            viewport,
            window: PhantomData,
        })
    }

    /// A private connection on SDL's display with its registry read. `what` is the error
    /// context when that fails.
    pub(crate) fn connect<S>(
        &self,
        what: &'static str,
    ) -> Result<(Connection, GlobalList, EventQueue<S>)>
    where
        S: Dispatch<wl_registry::WlRegistry, GlobalListContents> + 'static,
    {
        // SAFETY: the caller retains the window context until after this backend and every
        // object made from it are dropped (module doc).
        let backend = unsafe { Backend::from_foreign_display(self.display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<S>(&conn).context(what)?;
        Ok((conn, globals, queue))
    }

    /// SDL's `wl_surface` as a proxy on `conn`.
    pub(crate) fn surface_proxy(&self, conn: &Connection) -> Result<wl_surface::WlSurface> {
        // SAFETY: SDL's live wl_surface proxy on this display; the interface matches.
        let id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), self.surface.cast()) }
                .context("SDL wl_surface id")?;
        wl_surface::WlSurface::from_id(conn, id).context("SDL wl_surface proxy")
    }
}

/// Destroy the registry the connection's `GlobalList` holds, then flush. The registry has no
/// destructor request, so the backend drops the object by id.
pub(crate) fn release_registry(globals: &mut Option<GlobalList>, conn: &Connection) {
    if let Some(globals) = globals.take() {
        let id = globals.registry().id();
        globals.destroy();
        let _ = conn.backend().destroy_object(&id);
    }
    let _ = conn.flush();
}

/// Points SDL's Wayland driver at `socket` instead of `WAYLAND_DISPLAY`. Runs before SDL
/// starts video. `false` when the socket refuses or libwayland is missing.
pub(crate) fn hand_sdl_display(socket: &std::path::Path) -> bool {
    let Some(backend) = std::os::unix::net::UnixStream::connect(socket)
        .ok()
        .and_then(|stream| Backend::connect(stream).ok())
    else {
        return false;
    };
    let display = backend.display_ptr();
    // SDL never closes a display it was handed. Leaked, it outlives every window.
    std::mem::forget(backend);
    // SAFETY: `display` is a live wl_display that is never closed.
    unsafe {
        sdl3::sys::properties::SDL_SetPointerProperty(
            sdl3::sys::properties::SDL_GetGlobalProperties(),
            sdl3::sys::video::SDL_PROP_GLOBAL_VIDEO_WAYLAND_WL_DISPLAY_POINTER,
            display.cast(),
        )
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
fn monotonic_now_ns() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// A `CLOCK_MONOTONIC` instant on the session's realtime clock.
pub(crate) fn monotonic_to_realtime(mono_ns: u64) -> u64 {
    let now_mono = monotonic_now_ns();
    let now_real = pf_client_core::session::now_ns();
    now_real.wrapping_add(mono_ns).wrapping_sub(now_mono)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane's realtime stamp moves with the wall clock, never with the monotonic offset
    /// alone: a presented time "now" lands at "now" on the session clock.
    #[test]
    fn a_presented_now_lands_on_the_session_clock_now() {
        let mono = monotonic_now_ns();
        let real = pf_client_core::session::now_ns();
        let got = monotonic_to_realtime(mono);
        assert!(got.abs_diff(real) < 50_000_000, "{got} vs {real}");
    }
}
