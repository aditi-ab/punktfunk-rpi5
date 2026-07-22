//! Cursor-forward channel, host side (design/remote-desktop-sweep.md M2).
//!
//! When the session negotiated the cursor channel (client `CLIENT_CAP_CURSOR` met our
//! `HOST_CAP_CURSOR`), the encoder stops blending the pointer into the video
//! (`SessionPlan::cursor_blend = false`) and the encode loop forwards it out-of-band instead:
//! the SHAPE (bitmap + hotspot, rare) rides the reliable control stream via the control-task
//! bridge, per-tick STATE (position/visibility, 14 B) rides a lossy `0xD0` datagram — resent
//! every iteration so loss self-heals with no refresh timer.

use punktfunk_core::quic::{
    encode_cursor_state_datagram, CursorShape, CursorState, CURSOR_RELATIVE_HINT,
    CURSOR_SHAPE_MAX_SIDE, CURSOR_VISIBLE,
};

/// Per-session forward state, owned by the encode loop (the thread that binds frames).
pub(super) struct CursorForwarder {
    /// Serial of the last shape handed to the control-task bridge (`None` = none yet).
    sent_serial: Option<u64>,
    /// Last visible pointer position (hotspot point, frame px) — held across hidden spans so
    /// a hide still states WHERE the pointer was (the M3 reappear position).
    last_pos: (i32, i32),
}

impl CursorForwarder {
    pub(super) fn new() -> CursorForwarder {
        CursorForwarder {
            sent_serial: None,
            last_pos: (0, 0),
        }
    }

    /// Called once per encode-loop iteration with the bound frame's overlay (also on repeat
    /// iterations — the state datagram is the plane's loss heal, so it goes out every tick).
    /// Flag mapping (M3): a visible overlay states VISIBLE; a hidden-but-known overlay
    /// (an app grabbed/hid the pointer) states RELATIVE_HINT — the client should flip to
    /// captured relative; `None` (no bitmap has EVER arrived) states neither — never hint
    /// off a cold start, only off an observed hide.
    pub(super) fn tick(
        &mut self,
        cursor: Option<&pf_frame::CursorOverlay>,
        conn: &quinn::Connection,
        shape_tx: &tokio::sync::mpsc::UnboundedSender<CursorShape>,
    ) {
        let flags = match cursor {
            Some(ov) if ov.visible => {
                if self.sent_serial != Some(ov.serial) {
                    if let Some(shape) = shape_from_overlay(ov) {
                        // Bridge full ⇒ control task gone ⇒ session is tearing down anyway.
                        let _ = shape_tx.send(shape);
                        self.sent_serial = Some(ov.serial);
                    }
                }
                self.last_pos = (ov.x + ov.hot_x as i32, ov.y + ov.hot_y as i32);
                CURSOR_VISIBLE
            }
            Some(_) => CURSOR_RELATIVE_HINT,
            None => 0,
        };
        let state = CursorState {
            serial: self.sent_serial.unwrap_or(0) as u32,
            flags,
            x: self.last_pos.0,
            y: self.last_pos.1,
        };
        let _ = conn.send_datagram(encode_cursor_state_datagram(&state).into());
    }
}

/// Build the wire shape from a capture overlay, integer-downscaling (nearest-neighbor) anything
/// over [`CURSOR_SHAPE_MAX_SIDE`] so the message always fits the u16-length control frame.
/// Real cursors are far under the cap — the scale path is a correctness backstop for XL
/// accessibility cursors, not a quality path. `None` on a malformed overlay (short buffer).
fn shape_from_overlay(ov: &pf_frame::CursorOverlay) -> Option<CursorShape> {
    let px = (ov.w as usize).checked_mul(ov.h as usize)?.checked_mul(4)?;
    if ov.w == 0 || ov.h == 0 || ov.rgba.len() < px {
        return None;
    }
    let max = CURSOR_SHAPE_MAX_SIDE as u32;
    let f = ov.w.max(ov.h).div_ceil(max).max(1);
    let (w, h) = (ov.w.div_ceil(f), ov.h.div_ceil(f));
    let rgba = if f == 1 {
        ov.rgba.as_ref().clone()
    } else {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = ((x * f).min(ov.w - 1), (y * f).min(ov.h - 1));
                let o = ((sy * ov.w + sx) * 4) as usize;
                out.extend_from_slice(&ov.rgba[o..o + 4]);
            }
        }
        out
    };
    Some(CursorShape {
        serial: ov.serial as u32,
        w: w as u16,
        h: h as u16,
        hot_x: (ov.hot_x / f).min(w - 1) as u16,
        hot_y: (ov.hot_y / f).min(h - 1) as u16,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn overlay(w: u32, h: u32, hot: (u32, u32)) -> pf_frame::CursorOverlay {
        pf_frame::CursorOverlay {
            x: 10,
            y: 20,
            w,
            h,
            rgba: Arc::new((0..w * h * 4).map(|i| i as u8).collect()),
            serial: 3,
            hot_x: hot.0,
            hot_y: hot.1,
            visible: true,
        }
    }

    #[test]
    fn small_shape_passes_through() {
        let s = shape_from_overlay(&overlay(32, 32, (4, 5))).unwrap();
        assert_eq!((s.w, s.h, s.hot_x, s.hot_y, s.serial), (32, 32, 4, 5, 3));
        assert_eq!(s.rgba.len(), 32 * 32 * 4);
        // Encodes within the u16 control-frame cap.
        assert!(s.encode().len() <= u16::MAX as usize);
    }

    #[test]
    fn oversize_shape_downscales_with_hotspot() {
        // 256² → f = ceil(256/120) = 3 → 86² (256.div_ceil(3)), hotspot scales with it.
        let s = shape_from_overlay(&overlay(256, 256, (255, 0))).unwrap();
        assert!(s.w <= CURSOR_SHAPE_MAX_SIDE && s.h <= CURSOR_SHAPE_MAX_SIDE);
        assert_eq!(s.rgba.len(), s.w as usize * s.h as usize * 4);
        assert!(s.hot_x < s.w && s.hot_y < s.h);
        assert!(s.encode().len() <= u16::MAX as usize);
        // The scaled message must decode (dims within the cap).
        assert_eq!(CursorShape::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn short_buffer_rejected() {
        let mut ov = overlay(8, 8, (0, 0));
        ov.rgba = Arc::new(vec![0; 8]);
        assert!(shape_from_overlay(&ov).is_none());
    }
}
