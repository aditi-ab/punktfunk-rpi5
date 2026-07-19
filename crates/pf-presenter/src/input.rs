//! Input capture — the `ui_stream` state machine on SDL events, upgraded to real
//! pointer lock (the "stage-2 presenter's job" the GTK client deferred).
//!
//! Capture is a deliberate, reversible STATE (Moonlight-style): engaged when the stream
//! starts and when the user clicks into the video (that click is suppressed toward the
//! host); released by Ctrl+Alt+Shift+Q (toggles) or focus loss — held keys/buttons are
//! flushed host-side on release so nothing sticks down. While captured the pointer is
//! LOCKED (SDL relative mouse mode: hidden, confined, raw deltas) and motion goes on
//! the wire as RELATIVE `MouseMove` — the local cursor can't outrun or escape the
//! stream, so the only cursor you see is the host's. An auto-release from focus loss
//! re-engages on focus gain; an explicit user release (the chord) stays released until
//! the user opts back in.
//!
//! Keys are SDL scancodes → VK via `keymap_sdl`, layout-independent. Motion deltas are
//! COALESCED: one summed `MouseMove` per loop iteration (a 1000 Hz mouse would
//! otherwise send a datagram per event).

use crate::keymap_sdl;
use crate::touch::{Abs, Act, Gestures};
use pf_client_core::trust::TouchMode;
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Which transition a forwarded touchscreen finger is (SDL delivers one finger per event).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FingerPhase {
    Down,
    Move,
    Up,
}

pub struct Capture {
    connector: Arc<NativeClient>,
    captured: bool,
    /// The user released deliberately (the chord) — focus-gain must NOT re-engage.
    user_released: bool,
    /// VKs / GameStream button ids currently held — flushed up on release.
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
    /// Relative motion not yet on the wire, summed per loop iteration.
    pending_rel: (i32, i32),
    /// Fractional wheel remainder per axis (x, y) in 120-unit WHEEL_DELTA space —
    /// precision surfaces deliver sub-unit deltas; truncating each event drops the tail.
    scroll_acc: (f64, f64),
    /// Active touchscreen contacts: SDL finger id → the small wire touch id (slot) we
    /// forward it under. SDL finger ids are opaque and large; the host wants compact,
    /// per-contact-unique ids reusable after up (input.rs::TouchDown). Slots are freed on
    /// up and flushed up on release so no contact stays pressed on the host. Only used in
    /// [`TouchMode::Touch`]; the other modes drive `gestures` instead.
    touch_slots: HashMap<u64, u32>,
    /// The touchscreen input model for this session, and — for trackpad/pointer — the
    /// gesture state machine finger events feed.
    touch_mode: TouchMode,
    /// Reverse the scroll direction sent to the host ([`Settings::invert_scroll`]).
    invert_scroll: bool,
    gestures: Gestures,
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
    pub fn new(
        connector: Arc<NativeClient>,
        touch_mode: TouchMode,
        invert_scroll: bool,
    ) -> Capture {
        Capture {
            connector,
            captured: false,
            user_released: false,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            pending_rel: (0, 0),
            scroll_acc: (0.0, 0.0),
            touch_slots: HashMap::new(),
            touch_mode,
            invert_scroll,
            gestures: Gestures::new(touch_mode == TouchMode::Trackpad),
        }
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// Whether a regained focus should re-engage: yes unless the user released
    /// deliberately (the chord keeps its meaning across an Alt-Tab).
    pub fn should_reengage(&self) -> bool {
        !self.captured && !self.user_released
    }

    /// Engage capture. The caller flips SDL relative mouse mode on (pointer lock).
    pub fn engage(&mut self) -> bool {
        self.user_released = false;
        !std::mem::replace(&mut self.captured, true)
    }

    /// Release capture, flushing everything held so nothing sticks down on the host.
    /// `by_user` = the chord (stays released); focus loss re-engages on focus gain.
    /// The caller flips SDL relative mouse mode off. Returns false if not engaged.
    pub fn release(&mut self, by_user: bool) -> bool {
        if by_user {
            self.user_released = true;
        }
        if !std::mem::replace(&mut self.captured, false) {
            return false;
        }
        self.pending_rel = (0, 0); // never flush motion gathered while captured
        for vk in self.held_keys.drain() {
            send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
        }
        for b in self.held_buttons.drain() {
            send(&self.connector, InputKind::MouseButtonUp, b, 0, 0, 0);
        }
        for slot in self.touch_slots.drain().map(|(_, slot)| slot) {
            send(&self.connector, InputKind::TouchUp, slot, 0, 0, 0);
        }
        // The gesture engine's held left button (a tap-drag in progress) rides in
        // `held_buttons` above, so it was just flushed — here we only forget its state.
        self.gestures.reset();
        true
    }

    /// Forward the coalesced motion delta, if any — one datagram per loop iteration.
    pub fn flush_motion(&mut self) {
        let (dx, dy) = std::mem::take(&mut self.pending_rel);
        if dx != 0 || dy != 0 {
            send(&self.connector, InputKind::MouseMove, 0, dx, dy, 0);
        }
    }

    /// Relative motion (SDL relative mouse mode delivers raw deltas while locked).
    pub fn on_motion(&mut self, xrel: f32, yrel: f32) {
        if self.captured {
            self.pending_rel.0 += xrel as i32;
            self.pending_rel.1 += yrel as i32;
        }
    }

    pub fn on_key_down(&mut self, sc: sdl3::keyboard::Scancode) {
        if !self.captured {
            return;
        }
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Keep the wire ordered: the host must see the cursor where the user does
            // when the key lands (e.g. "press E at the crosshair").
            self.flush_motion();
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
    /// never reaches here). Pending motion flushes first so the button-down lands where
    /// the host cursor actually is.
    pub fn on_button_down(&mut self, b: sdl3::mouse::MouseButton) {
        if !self.captured {
            return;
        }
        self.flush_motion();
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            self.held_buttons.insert(gs);
            send(&self.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
        }
    }

    pub fn on_button_up(&mut self, b: sdl3::mouse::MouseButton) {
        self.flush_motion(); // the release must not beat the motion before it
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            if self.held_buttons.remove(&gs) {
                send(&self.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
            }
        }
    }

    /// Wheel — the wire carries WHEEL_DELTA(120) units, positive = up / right. SDL3's y
    /// is positive = up and x positive = right already. Fractional remainders
    /// accumulate per axis.
    pub fn on_wheel(&mut self, dx: f32, dy: f32) {
        if !self.captured {
            return;
        }
        self.flush_motion(); // scroll happens at the latest cursor position
        let sign = if self.invert_scroll { -1.0 } else { 1.0 };
        let (mut ax, mut ay) = self.scroll_acc;
        ay += f64::from(dy) * 120.0 * sign;
        ax += f64::from(dx) * 120.0 * sign;
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

    /// The compact wire touch id for an SDL finger — its existing slot, or the lowest free
    /// one (contacts are few, so a linear scan is nothing). Held until the finger lifts.
    fn touch_slot(&mut self, finger_id: u64) -> u32 {
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            return slot;
        }
        let used: HashSet<u32> = self.touch_slots.values().copied().collect();
        let slot = (0u32..).find(|s| !used.contains(s)).unwrap_or(0);
        self.touch_slots.insert(finger_id, slot);
        slot
    }

    /// Touch flags pack the client surface size the coordinates are relative to, so the
    /// host can rescale into its output — identical layout to Android's nativeSendTouch.
    fn touch_flags(w: u32, h: u32) -> u32 {
        ((w & 0xffff) << 16) | (h & 0xffff)
    }

    /// A new touchscreen contact — `x`/`y` are absolute in the `w`×`h` content surface.
    /// Ignored unless captured (the stream owns the glass; the menu is gamepad-driven).
    pub fn on_touch_down(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        let slot = self.touch_slot(finger_id);
        send(
            &self.connector,
            InputKind::TouchDown,
            slot,
            x,
            y,
            Self::touch_flags(w, h),
        );
    }

    /// A contact moved. Only forwarded for a finger we already sent a down for — a move
    /// with no live slot (capture engaged mid-touch) would have no matching host contact.
    pub fn on_touch_move(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            send(
                &self.connector,
                InputKind::TouchMove,
                slot,
                x,
                y,
                Self::touch_flags(w, h),
            );
        }
    }

    /// A contact lifted — release its slot and the host contact. Forwarded even when not
    /// captured: a `release()` may have already flushed it (then the slot is gone and this
    /// no-ops), but a stray up must never strand a pressed contact on the host.
    pub fn on_touch_up(&mut self, finger_id: u64) {
        if let Some(slot) = self.touch_slots.remove(&finger_id) {
            send(&self.connector, InputKind::TouchUp, slot, 0, 0, 0);
        }
    }

    /// Route one forwarded touchscreen finger by the session's touch model. `wx`/`wy` are
    /// physical window pixels (the trackpad ballistics + gesture geometry); `abs` is the same
    /// finger mapped into the letterboxed content rect (pointer moves + raw passthrough). In
    /// `Touch` mode fingers go on the wire as real contacts; in `Trackpad`/`Pointer` they
    /// drive the gesture engine. Returns true when a three-finger tap asks to cycle the stats
    /// overlay — the only signal the run loop must act on.
    pub fn dispatch_finger(
        &mut self,
        phase: FingerPhase,
        id: u64,
        wx: f32,
        wy: f32,
        abs: Abs,
        t_ms: f64,
    ) -> bool {
        match self.touch_mode {
            TouchMode::Touch => {
                match phase {
                    FingerPhase::Down => self.on_touch_down(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Move => self.on_touch_move(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Up => self.on_touch_up(id),
                }
                false
            }
            TouchMode::Trackpad | TouchMode::Pointer => {
                // Down/Move only while captured (the stream owns the glass); an Up always runs
                // so a lift can conclude a gesture / release a held drag even if capture just
                // dropped (focus loss mid-touch).
                if !self.captured && phase != FingerPhase::Up {
                    return false;
                }
                let acts = match phase {
                    FingerPhase::Down => self.gestures.down(id, wx, wy, abs, t_ms),
                    FingerPhase::Move => self.gestures.motion(id, wx, wy, abs, t_ms),
                    FingerPhase::Up => self.gestures.up(id, t_ms),
                };
                let mut cycle_stats = false;
                for act in acts {
                    cycle_stats |= self.apply_touch_act(act);
                }
                cycle_stats
            }
        }
    }

    /// Send one gesture [`Act`] on the wire, tracking button holds in `held_buttons` so a
    /// capture release flushes them (a tap-drag's left button never sticks down). Returns
    /// true for [`Act::CycleStats`], which is a run-loop signal, not a wire event.
    fn apply_touch_act(&mut self, act: Act) -> bool {
        match act {
            Act::CycleStats => return true,
            Act::Button { gs, down } => {
                if down {
                    self.flush_motion(); // the press lands where the cursor now is
                    self.held_buttons.insert(gs);
                    send(&self.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
                } else if self.held_buttons.remove(&gs) {
                    self.flush_motion();
                    send(&self.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
                }
            }
            other => {
                if let Some((kind, code, x, y, flags)) = other.wire() {
                    send(&self.connector, kind, code, x, y, flags);
                }
            }
        }
        false
    }
}
