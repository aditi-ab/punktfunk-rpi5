//! Client-side cursor rendering (design/remote-desktop-sweep.md M2): the host forwards the
//! pointer's SHAPE (reliable control stream, cached by serial) and per-frame STATE (lossy
//! `0xD0` — position/visibility), and WE draw it as a real OS cursor — pointer feel stops
//! paying the video round-trip (the Parsec/RDP model). Active only when the session
//! negotiated it (`HOST_CAP_CURSOR` in the Welcome — the host stopped compositing then) and
//! only applied while the DESKTOP mouse model is engaged: under capture the pointer is
//! relative-locked (SDL hides it) and games draw their own cursor in-frame.
//!
//! The host sends the bitmap in host-FRAMEBUFFER pixels, whose size tracks the host virtual
//! display's DPI scaling (32 px at 100%, 96 px at 300%). Drawn 1:1 it balloons on a high-DPI
//! host; instead we scale it by the aspect-fit factor the video is drawn at
//! (`min(window_px/mode)`) TIMES the client display's content scale (SDL shows the surface at
//! ~1:1 physical pixels on every backend, so without the second factor a 200 % client's
//! pointer came out half the size of its native ones — see the run loop's `cursor_scale`).
//! SDL cursors are fixed-size from their surface (no draw-time scaling), so we cache
//! shapes RAW and resample per install — rebuilding when the serial OR the scale changes.

use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{CursorState, HOST_CAP_CURSOR};
use sdl3::mouse::{Cursor, MouseUtil, SystemCursor};
use sdl3::pixels::PixelFormat;
use sdl3::surface::Surface;
use std::collections::HashMap;
use std::time::Duration;

/// Shape serials cached at most — cursors cycle through a handful of shapes (arrow, I-beam,
/// resize…); a runaway host can't grow the map past this (the cache resets, shapes re-arrive
/// on the reliable stream via the serial-miss path).
const SHAPE_CACHE_MAX: usize = 64;

/// A forwarded cursor shape held RAW (host-framebuffer-pixel bytes + hotspot), so it can be
/// rebuilt into a scaled OS cursor whenever the video-fit changes (a window resize). Caching a
/// fixed-size `Cursor` instead would freeze the pointer at its build-time size.
struct RawShape {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
}

pub struct CursorChannel {
    /// The Welcome carried `HOST_CAP_CURSOR` — the host forwards instead of compositing.
    negotiated: bool,
    /// Serial → raw forwarded shape (bounded by [`SHAPE_CACHE_MAX`]).
    shapes: HashMap<u32, RawShape>,
    /// The serial + fit scale the currently-installed OS cursor was built at (`None` =
    /// default/system cursor). A change in EITHER forces a rebuild.
    installed: Option<(u32, f32)>,
    /// Keeps the installed `Cursor` alive — SDL requires it to outlive its `set()`.
    installed_cursor: Option<Cursor>,
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
            installed_cursor: None,
            state: None,
        }
    }

    /// Whether the host forwards the cursor this session (it no longer composites one).
    pub fn negotiated(&self) -> bool {
        self.negotiated
    }

    /// The latest drained `0xD0` state — the run loop reads `relative_hint` off it for the
    /// M3 host-driven mode flip (and `x`/`y` as the reappear position when leaving relative).
    pub fn state(&self) -> Option<CursorState> {
        self.state
    }

    /// Drain the two planes and apply the newest state — once per run-loop iteration.
    /// `desktop_active` = the desktop mouse model is engaged (captured + desktop): only then
    /// do we own the local cursor's shape/visibility; under capture SDL's relative mode owns
    /// it, and released the system cursor must look normal. `fit_scale` is host-framebuffer
    /// pixels → cursor-surface pixels (the run loop's `cursor_scale`: the aspect-fit factor
    /// the video is drawn at, times the client display's content scale); the shape is
    /// resampled by it so the pointer matches the streamed desktop at any host DPI *and*
    /// the client's native pointers at any client DPI.
    pub fn pump(
        &mut self,
        connector: &NativeClient,
        mouse: &MouseUtil,
        desktop_active: bool,
        fit_scale: f32,
    ) {
        if !self.negotiated {
            return;
        }
        while let Ok(shape) = connector.next_cursor_shape(Duration::ZERO) {
            if self.shapes.len() >= SHAPE_CACHE_MAX {
                // Degenerate host: reset — live shapes re-install via the serial-miss path.
                self.shapes.clear();
                self.installed = None;
            }
            let (w, h) = (shape.w as u32, shape.h as u32);
            if w == 0 || h == 0 || shape.rgba.len() < (w * h * 4) as usize {
                tracing::warn!(w, h, "cursor shape malformed — ignored");
                continue;
            }
            // A re-sent serial replaces its entry; force re-install if it's current.
            if matches!(self.installed, Some((s, _)) if s == shape.serial) {
                self.installed = None;
            }
            self.shapes.insert(
                shape.serial,
                RawShape {
                    rgba: shape.rgba,
                    w,
                    h,
                    hot_x: shape.hot_x as u32,
                    hot_y: shape.hot_y as u32,
                },
            );
        }
        while let Ok(st) = connector.next_cursor_state(Duration::ZERO) {
            self.state = Some(st); // latest wins
        }

        if !desktop_active {
            // Capture mode / released: hand the cursor back to the system default so a
            // released pointer over the window doesn't wear the host's shape.
            if self.installed.take().is_some() {
                if let Ok(c) = Cursor::from_system(SystemCursor::Arrow) {
                    c.set();
                    self.installed_cursor = Some(c); // keep it alive past set()
                }
            }
            return;
        }
        let Some(st) = self.state else { return };
        if st.visible() && self.installed != Some((st.serial, fit_scale)) {
            if let Some(shape) = self.shapes.get(&st.serial) {
                match build_scaled_cursor(shape, fit_scale) {
                    Ok(cursor) => {
                        cursor.set();
                        self.installed = Some((st.serial, fit_scale));
                        self.installed_cursor = Some(cursor); // outlive set()
                    }
                    Err(e) => tracing::warn!(error = %e, w = shape.w, h = shape.h,
                        "cursor shape rejected by SDL — keeping the previous cursor"),
                }
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

/// Resample a raw shape by `fit_scale` and build an SDL color cursor from it. The hotspot scales
/// with the bitmap so the click point stays true. `fit_scale <= 0` (or a degenerate result) is
/// clamped so we always hand SDL a ≥1×1 surface.
fn build_scaled_cursor(shape: &RawShape, fit_scale: f32) -> Result<Cursor, String> {
    let scale = if fit_scale.is_finite() && fit_scale > 0.0 {
        fit_scale
    } else {
        1.0
    };
    let dw = ((shape.w as f32 * scale).round() as u32).max(1);
    let dh = ((shape.h as f32 * scale).round() as u32).max(1);
    let hot_x = ((shape.hot_x as f32 * scale).round() as u32).min(dw - 1) as i32;
    let hot_y = ((shape.hot_y as f32 * scale).round() as u32).min(dh - 1) as i32;

    if dw == shape.w && dh == shape.h {
        // 1:1 fit (mode == window) — no resample, feed the bytes straight through.
        let mut data = shape.rgba.clone();
        let surf = Surface::from_data(
            &mut data,
            shape.w,
            shape.h,
            shape.w * 4,
            PixelFormat::RGBA32,
        )
        .map_err(|e| e.to_string())?;
        return Cursor::from_surface(&surf, hot_x, hot_y).map_err(|e| e.to_string());
    }

    let mut scaled = resample_rgba(&shape.rgba, shape.w, shape.h, dw, dh);
    let surf = Surface::from_data(&mut scaled, dw, dh, dw * 4, PixelFormat::RGBA32)
        .map_err(|e| e.to_string())?;
    Cursor::from_surface(&surf, hot_x, hot_y).map_err(|e| e.to_string())
}

/// Area-average resample of a straight-alpha RGBA bitmap `(sw×sh) → (dw×dh)`. Averaging is done on
/// PREMULTIPLIED colour (weighting each source texel by its alpha) so transparent-pixel colour
/// can't bleed into the fringe, then un-premultiplied back to straight alpha. Handles both down-
/// and up-scale; for a cursor the common case is a downscale (high-DPI host → smaller pointer).
fn resample_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for dy in 0..dh {
        let sy0 = (dy as f32 * fy).floor() as u32;
        let sy1 = (((dy + 1) as f32 * fy).ceil() as u32).clamp(sy0 + 1, sh);
        for dx in 0..dw {
            let sx0 = (dx as f32 * fx).floor() as u32;
            let sx1 = (((dx + 1) as f32 * fx).ceil() as u32).clamp(sx0 + 1, sw);
            let (mut r, mut g, mut b, mut a_sum, mut n) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = ((sy * sw + sx) * 4) as usize;
                    let a = src[i + 3] as f32 / 255.0;
                    r += src[i] as f32 * a;
                    g += src[i + 1] as f32 * a;
                    b += src[i + 2] as f32 * a;
                    a_sum += a;
                    n += 1.0;
                }
            }
            let di = ((dy * dw + dx) * 4) as usize;
            if a_sum > 0.0 {
                out[di] = (r / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 1] = (g / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 2] = (b / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 3] = (a_sum / n * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            // else fully transparent — already zero-filled.
        }
    }
    out
}
