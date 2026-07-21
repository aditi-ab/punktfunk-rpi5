//! Client-side cursor rendering (design/remote-desktop-sweep.md M2): the host forwards the
//! pointer's SHAPE (reliable control stream, cached by serial) and per-frame STATE (lossy
//! `0xD0` — position/visibility), and WE draw it as a real OS cursor — pointer feel stops
//! paying the video round-trip (the Parsec/RDP model). Active only when the session
//! negotiated it (`HOST_CAP_CURSOR` in the Welcome — the host stopped compositing then) and
//! only applied while the DESKTOP mouse model is engaged: under capture the pointer is
//! relative-locked (SDL hides it) and games draw their own cursor in-frame.

use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{CursorState, HOST_CAP_CURSOR};
use sdl3::mouse::{Cursor, MouseUtil, SystemCursor};
use std::collections::HashMap;
use std::time::Duration;

/// Shape serials cached at most — cursors cycle through a handful of shapes (arrow, I-beam,
/// resize…); a runaway host can't grow the map past this (the cache resets, shapes re-arrive
/// on the reliable stream via the serial-miss path).
const SHAPE_CACHE_MAX: usize = 64;

pub struct CursorChannel {
    /// The Welcome carried `HOST_CAP_CURSOR` — the host forwards instead of compositing.
    negotiated: bool,
    /// Serial → built OS cursor. An SDL `Cursor` must outlive its `set()`, so the cache owns
    /// every shape ever applied this session (bounded by [`SHAPE_CACHE_MAX`]).
    shapes: HashMap<u32, Cursor>,
    /// The serial currently installed via `Cursor::set` (`None` = default/system cursor).
    installed: Option<u32>,
    /// Latest `0xD0` state (latest-wins across a drained batch).
    state: Option<CursorState>,
}

impl CursorChannel {
    pub fn new(connector: &NativeClient) -> CursorChannel {
        let negotiated = connector.host_caps() & HOST_CAP_CURSOR != 0;
        if negotiated {
            tracing::info!("cursor channel negotiated — host cursor renders locally");
        }
        CursorChannel {
            negotiated,
            shapes: HashMap::new(),
            installed: None,
            state: None,
        }
    }

    /// Whether the host forwards the cursor this session (it no longer composites one).
    pub fn negotiated(&self) -> bool {
        self.negotiated
    }

    /// Drain the two planes and apply the newest state — once per run-loop iteration.
    /// `desktop_active` = the desktop mouse model is engaged (captured + desktop): only then
    /// do we own the local cursor's shape/visibility; under capture SDL's relative mode owns
    /// it, and released the system cursor must look normal.
    pub fn pump(&mut self, connector: &NativeClient, mouse: &MouseUtil, desktop_active: bool) {
        if !self.negotiated {
            return;
        }
        while let Ok(shape) = connector.next_cursor_shape(Duration::ZERO) {
            if self.shapes.len() >= SHAPE_CACHE_MAX {
                // Degenerate host: reset — live shapes re-install via the serial-miss path.
                self.shapes.clear();
                self.installed = None;
            }
            let mut data = shape.rgba;
            let built = sdl3::surface::Surface::from_data(
                &mut data,
                shape.w as u32,
                shape.h as u32,
                shape.w as u32 * 4,
                sdl3::pixels::PixelFormat::RGBA32,
            )
            .map_err(|e| e.to_string())
            .and_then(|surf| {
                Cursor::from_surface(&surf, shape.hot_x as i32, shape.hot_y as i32)
                    .map_err(|e| e.to_string())
            });
            match built {
                Ok(cursor) => {
                    // A re-sent serial replaces its entry; force re-install if it's current.
                    if self.installed == Some(shape.serial) {
                        self.installed = None;
                    }
                    self.shapes.insert(shape.serial, cursor);
                }
                Err(e) => tracing::warn!(error = %e, w = shape.w, h = shape.h,
                    "cursor shape rejected by SDL — keeping the previous cursor"),
            }
        }
        while let Ok(st) = connector.next_cursor_state(Duration::ZERO) {
            self.state = Some(st); // latest wins
        }

        if !desktop_active {
            // Capture mode / released: hand the cursor back to the system default so a
            // released pointer over the window doesn't wear the host's shape.
            if self.installed.take().is_some() {
                Cursor::from_system(SystemCursor::Arrow)
                    .map(|c| c.set())
                    .ok();
            }
            return;
        }
        let Some(st) = self.state else { return };
        if st.visible() && self.installed != Some(st.serial) {
            if let Some(cursor) = self.shapes.get(&st.serial) {
                cursor.set();
                self.installed = Some(st.serial);
            }
            // Serial miss: the (reliable) shape hasn't landed yet — keep the previous
            // cursor for the RTT rather than flashing default.
        }
        // Visibility follows the host (a host app hid its pointer ⇒ ours hides too). Queried,
        // not shadowed, so apply_capture's own show/hide calls can never desync us.
        if mouse.is_cursor_showing() != st.visible() {
            mouse.show_cursor(st.visible());
        }
    }
}
