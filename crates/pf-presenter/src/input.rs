//! Input capture — the `ui_stream` state machine on SDL events.
//!
//! Capture is a deliberate, reversible STATE (Moonlight-style): engaged when the stream
//! starts and when the user clicks into the video (that click is suppressed toward the
//! host); released by Ctrl+Alt+Shift+Q (toggles) or focus loss — held keys/buttons are
//! flushed host-side on release so nothing sticks down. While captured the local cursor
//! is hidden (the host renders its own); while released nothing is forwarded.
//!
//! Keys are SDL scancodes → VK via `keymap_sdl`, layout-independent. Mouse is absolute
//! (`MouseMoveAbs` scaled into the negotiated mode through the letterbox transform,
//! window size packed in `flags` — the GTK client's exact contract); motion is coalesced
//! to one datagram per loop iteration. Pointer-lock relative capture is the follow-up
//! (§5.3 of the plan) — absolute first for GTK parity.

use crate::keymap_sdl;
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::HashSet;
use std::sync::Arc;

pub struct Capture {
    connector: Arc<NativeClient>,
    captured: bool,
    /// VKs / GameStream button ids currently held — flushed up on release.
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
    /// Newest absolute pointer position (window coordinates) not yet on the wire —
    /// motion events only store here; `flush_motion` sends at most one `MouseMoveAbs`
    /// per loop iteration (a 1000 Hz mouse would otherwise send a datagram per event).
    pending_abs: Option<(f32, f32)>,
    /// Fractional wheel remainder per axis (x, y) in 120-unit WHEEL_DELTA space —
    /// precision surfaces deliver sub-unit deltas; truncating each event drops the tail.
    scroll_acc: (f64, f64),
}

fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

impl Capture {
    pub fn new(connector: Arc<NativeClient>) -> Capture {
        Capture {
            connector,
            captured: false,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            pending_abs: None,
            scroll_acc: (0.0, 0.0),
        }
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// Engage capture. The caller hides the cursor (SDL state lives with the loop).
    pub fn engage(&mut self) -> bool {
        !std::mem::replace(&mut self.captured, true)
    }

    /// Release capture, flushing everything held so nothing sticks down on the host.
    /// The caller restores the cursor. Returns false if it wasn't engaged.
    pub fn release(&mut self) -> bool {
        if !std::mem::replace(&mut self.captured, false) {
            return false;
        }
        self.pending_abs = None; // never flush motion gathered while captured
        for vk in self.held_keys.drain() {
            send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
        }
        for b in self.held_buttons.drain() {
            send(&self.connector, InputKind::MouseButtonUp, b, 0, 0, 0);
        }
        true
    }

    /// Forward the coalesced pointer position, if any — one datagram per loop iteration.
    /// `win` is the window's logical size (SDL mouse coordinates' space).
    pub fn flush_motion(&mut self, win: (u32, u32)) {
        if let Some((x, y)) = self.pending_abs.take() {
            self.send_abs(x, y, win);
        }
    }

    /// Absolute position: window coordinates → video pixels through the Contain-fit
    /// letterbox (the blit uses the same math in pixel space — the ratios are identical).
    /// `flags` packs the coordinate-space size (`(w << 16) | h`) — the host normalizes
    /// against it before mapping into the EIS region; without it the event is dropped.
    fn send_abs(&self, x: f32, y: f32, win: (u32, u32)) {
        let mode = self.connector.mode();
        let (ww, wh) = (win.0.max(1) as f64, win.1.max(1) as f64);
        let (vw, vh) = (mode.width.max(1) as f64, mode.height.max(1) as f64);
        let scale = (ww / vw).min(wh / vh);
        let (ox, oy) = ((ww - vw * scale) / 2.0, (wh - vh * scale) / 2.0);
        let px = ((f64::from(x) - ox) / scale).round().clamp(0.0, vw - 1.0) as i32;
        let py = ((f64::from(y) - oy) / scale).round().clamp(0.0, vh - 1.0) as i32;
        let flags = (mode.width << 16) | (mode.height & 0xffff);
        send(&self.connector, InputKind::MouseMoveAbs, 0, px, py, flags);
    }

    pub fn on_motion(&mut self, x: f32, y: f32) {
        if self.captured {
            self.pending_abs = Some((x, y));
        }
    }

    pub fn on_key_down(&mut self, sc: sdl3::keyboard::Scancode, win: (u32, u32)) {
        if !self.captured {
            return;
        }
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Keep the wire ordered: the host must see the cursor where the user does
            // when the key lands (e.g. "press E at the crosshair").
            self.flush_motion(win);
            self.held_keys.insert(vk);
            send(&self.connector, InputKind::KeyDown, vk as u32, 0, 0, 0);
        }
    }

    pub fn on_key_up(&mut self, sc: sdl3::keyboard::Scancode) {
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Flush-on-release may have beaten us to it — only forward if still held.
            if self.held_keys.remove(&vk) {
                send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
            }
        }
    }

    /// A button press while captured. The engaging click is the caller's business (it
    /// never reaches here). The click's own coordinates supersede pending motion so the
    /// button-down lands exactly where the cursor last was.
    pub fn on_button_down(&mut self, b: sdl3::mouse::MouseButton, x: f32, y: f32, win: (u32, u32)) {
        if !self.captured {
            return;
        }
        self.pending_abs = Some((x, y));
        self.flush_motion(win);
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            self.held_buttons.insert(gs);
            send(&self.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
        }
    }

    pub fn on_button_up(&mut self, b: sdl3::mouse::MouseButton, win: (u32, u32)) {
        self.flush_motion(win); // the release must not beat the motion before it
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            if self.held_buttons.remove(&gs) {
                send(&self.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
            }
        }
    }

    /// Wheel — the wire carries WHEEL_DELTA(120) units, positive = up / right. SDL3's y
    /// is positive = up and x positive = right already (no negation — GTK's dy was the
    /// inverted one). Fractional remainders accumulate per axis.
    pub fn on_wheel(&mut self, dx: f32, dy: f32, win: (u32, u32)) {
        if !self.captured {
            return;
        }
        self.flush_motion(win); // scroll happens at the latest cursor position
        let (mut ax, mut ay) = self.scroll_acc;
        ay += f64::from(dy) * 120.0;
        ax += f64::from(dx) * 120.0;
        let vy = ay.trunc() as i32;
        if vy != 0 {
            ay -= f64::from(vy);
            send(&self.connector, InputKind::MouseScroll, 0, vy, 0, 0);
        }
        let vx = ax.trunc() as i32;
        if vx != 0 {
            ax -= f64::from(vx);
            send(&self.connector, InputKind::MouseScroll, 1, vx, 0, 0);
        }
        self.scroll_acc = (ax, ay);
    }
}
