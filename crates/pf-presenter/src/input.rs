//! Presenter input: SDL events → capture state and wire `InputEvent`s.
//!
//! Capture is reversible. Engage at stream start and on click-into-video (that
//! click is not forwarded). Release on Ctrl+Alt+Shift+Q or focus loss; held
//! keys/buttons flush as ups. While captured, SDL relative mouse mode hides,
//! confines, and feeds raw deltas as `MouseMove`. Focus-loss re-engages on
//! gain; the chord stays released until the user opts in.
//!
//! Keys are SDL scancodes → VK via `keymap_sdl` (layout-independent). Relative
//! motion coalesces to one summed `MouseMove` per loop — a 1000 Hz mouse would
//! otherwise send a datagram per event.
//!
//! Desktop mode (`design/remote-desktop-sweep.md`) reuses engage/release but
//! never locks: the local cursor moves freely (hidden over the window) and
//! motion is latest-wins `MouseMoveAbs` through the letterbox. Gamescope EIS
//! is relative-only; those sessions pin to capture ([`Capture::new`] `abs_ok`).

use crate::keymap_sdl;
use crate::touch::{Abs, Act, Gestures};
use pf_client_core::trust::{MouseMode, TouchMode};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{classify, GRANT_KEYBOARD, GRANT_POINTER};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

impl Act {
    /// Motion/scroll packing for the wire. Lives here, not in `touch.rs`: the
    /// gesture engine must not take the platform-gated core dependency.
    pub fn wire(self) -> Option<(InputKind, u32, i32, i32, u32)> {
        match self {
            Act::MoveRel { dx, dy } => Some((InputKind::MouseMove, 0, dx, dy, 0)),
            Act::MoveAbs(a) => Some((
                InputKind::MouseMoveAbs,
                0,
                a.x,
                a.y,
                ((a.w & 0xffff) << 16) | (a.h & 0xffff),
            )),
            Act::Scroll { axis, delta } => Some((InputKind::MouseScroll, axis, delta, 0, 0)),
            Act::Button { .. }
            | Act::CycleStats
            | Act::Dial { .. }
            | Act::DialCommit
            | Act::DialCancel => None,
        }
    }
}

/// One SDL finger event. SDL never batches fingers in a single event.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FingerPhase {
    Down,
    Move,
    Up,
}

pub struct Capture {
    connector: Arc<NativeClient>,
    captured: bool,
    /// Chord release: focus-gain must not re-engage.
    user_released: bool,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
    /// Relative motion not yet on the wire, summed per loop iteration.
    pending_rel: (i32, i32),
    /// Desktop-model position not yet on the wire, latest-wins per loop iteration.
    pending_abs: Option<Abs>,
    /// Never true unless `abs_ok`.
    desktop: bool,
    /// Host injector accepts `MouseMoveAbs` (any compositor but gamescope).
    abs_ok: bool,
    /// Fractional remainder per axis in 120-unit WHEEL_DELTA space — precision
    /// surfaces deliver sub-unit deltas; truncating each event drops the tail.
    scroll_acc: (f64, f64),
    /// SDL finger id → compact host slot (`TouchDown`). SDL ids are opaque and
    /// large; slots reuse after up, flush on release. [`TouchMode::Touch`] only.
    touch_slots: HashMap<u64, u32>,
    touch_mode: TouchMode,
    invert_scroll: bool,
    gestures: Gestures,
    /// Session access mask. `send` uses the same [`classify`] as the host filter
    /// so a new `InputKind` cannot slip one side. Live via [`Capture::set_grants`].
    grants: u32,
}

/// Drop locally if `grants` does not cover `kind`. Shares [`classify`] with the
/// host so a new `InputKind` cannot pass one side only.
fn send(
    connector: &NativeClient,
    grants: u32,
    kind: InputKind,
    code: u32,
    x: i32,
    y: i32,
    flags: u32,
) {
    if grants & classify(kind).bit() == 0 {
        return;
    }
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
    /// Without `abs_ok` the desktop model is unavailable and `mouse_mode`
    /// silently resolves to capture.
    pub fn new(
        connector: Arc<NativeClient>,
        touch_mode: TouchMode,
        invert_scroll: bool,
        mouse_mode: MouseMode,
        abs_ok: bool,
        grants: u32,
    ) -> Capture {
        Capture {
            connector,
            captured: false,
            user_released: false,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            pending_rel: (0, 0),
            pending_abs: None,
            desktop: abs_ok && mouse_mode == MouseMode::Desktop,
            abs_ok,
            scroll_acc: (0.0, 0.0),
            touch_slots: HashMap::new(),
            touch_mode,
            invert_scroll,
            gestures: Gestures::new(touch_mode == TouchMode::Trackpad, invert_scroll),
            grants,
        }
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// Access mask the run loop feeds `apply_capture` so lock/grab track it.
    pub fn grants(&self) -> u32 {
        self.grants
    }

    /// Without POINTER or KEYBOARD, engage would lock a pointer that lands nowhere.
    pub fn can_capture(&self) -> bool {
        self.grants & (GRANT_POINTER | GRANT_KEYBOARD) != 0
    }

    /// Mid-session `AccessUpdate`. Lost classes flush held ups under the OLD
    /// mask first — the host may still honor them. The run loop then re-applies
    /// lock/grab, and releases capture if [`Capture::can_capture`] went false.
    pub fn set_grants(&mut self, grants: u32) {
        if grants == self.grants {
            return;
        }
        let lost = self.grants & !grants;
        if lost & GRANT_KEYBOARD != 0 {
            for vk in self.held_keys.drain() {
                send(
                    &self.connector,
                    self.grants,
                    InputKind::KeyUp,
                    vk as u32,
                    0,
                    0,
                    0,
                );
            }
        }
        if lost & GRANT_POINTER != 0 {
            self.pending_rel = (0, 0);
            self.pending_abs = None;
            for b in self.held_buttons.drain() {
                send(
                    &self.connector,
                    self.grants,
                    InputKind::MouseButtonUp,
                    b,
                    0,
                    0,
                    0,
                );
            }
            for slot in self.touch_slots.drain().map(|(_, slot)| slot) {
                send(
                    &self.connector,
                    self.grants,
                    InputKind::TouchUp,
                    slot,
                    0,
                    0,
                    0,
                );
            }
            self.gestures.reset();
        }
        self.grants = grants;
    }

    pub fn desktop(&self) -> bool {
        self.desktop
    }

    /// Ctrl+Alt+Shift+M. `None` if the host cannot take absolute events
    /// (gamescope). Pending motion from the old model is dropped, not sent.
    pub fn toggle_desktop(&mut self) -> Option<bool> {
        if !self.abs_ok {
            return None;
        }
        self.desktop = !self.desktop;
        self.pending_rel = (0, 0);
        self.pending_abs = None;
        Some(self.desktop)
    }

    /// Host-driven flip: `relative_hint` grabbed/hid the pointer → relative;
    /// hint clear → absolute. Same gating and pending-motion drop as
    /// [`toggle_desktop`](Self::toggle_desktop).
    pub fn set_desktop(&mut self, on: bool) -> bool {
        if !self.abs_ok || self.desktop == on {
            return false;
        }
        self.desktop = on;
        self.pending_rel = (0, 0);
        self.pending_abs = None;
        true
    }

    /// Re-engage on focus gain unless the user released via the chord.
    pub fn should_reengage(&self) -> bool {
        !self.captured && !self.user_released
    }

    /// Caller turns on SDL relative mouse only if this returns true: otherwise
    /// grants cover neither pointer nor keyboard ([`Capture::can_capture`]).
    pub fn engage(&mut self) -> bool {
        if !self.can_capture() {
            return false;
        }
        self.user_released = false;
        self.captured = true;
        true
    }

    /// Flush held keys/buttons/touches as ups. `by_user` (the chord) stays
    /// released; focus loss re-engages on gain. Caller turns off relative mouse.
    pub fn release(&mut self, by_user: bool) -> bool {
        if by_user {
            self.user_released = true;
        }
        if !std::mem::replace(&mut self.captured, false) {
            return false;
        }
        self.pending_rel = (0, 0); // never send motion gathered while captured
        self.pending_abs = None;
        for vk in self.held_keys.drain() {
            send(
                &self.connector,
                self.grants,
                InputKind::KeyUp,
                vk as u32,
                0,
                0,
                0,
            );
        }
        for b in self.held_buttons.drain() {
            send(
                &self.connector,
                self.grants,
                InputKind::MouseButtonUp,
                b,
                0,
                0,
                0,
            );
        }
        for slot in self.touch_slots.drain().map(|(_, slot)| slot) {
            send(
                &self.connector,
                self.grants,
                InputKind::TouchUp,
                slot,
                0,
                0,
                0,
            );
        }
        // Tap-drag's left button was flushed via `held_buttons`; only forget state.
        self.gestures.reset();
        true
    }

    /// One datagram per loop. Only one store is populated; the run loop routes
    /// by [`desktop`](Self::desktop).
    pub fn flush_motion(&mut self) {
        let (dx, dy) = std::mem::take(&mut self.pending_rel);
        if dx != 0 || dy != 0 {
            send(
                &self.connector,
                self.grants,
                InputKind::MouseMove,
                0,
                dx,
                dy,
                0,
            );
        }
        if let Some(a) = self.pending_abs.take() {
            send(
                &self.connector,
                self.grants,
                InputKind::MouseMoveAbs,
                0,
                a.x,
                a.y,
                Self::touch_flags(a.w, a.h),
            );
        }
    }

    pub fn on_motion(&mut self, xrel: f32, yrel: f32) {
        if self.captured && !self.desktop {
            self.pending_rel.0 += xrel as i32;
            self.pending_rel.1 += yrel as i32;
        }
    }

    /// Letterboxed content position. Latest-wins: intermediates add nothing
    /// (deltas must sum).
    pub fn on_motion_abs(&mut self, abs: Abs) {
        if self.captured && self.desktop {
            self.pending_abs = Some(abs);
        }
    }

    pub fn on_key_down(&mut self, sc: sdl3::keyboard::Scancode) {
        if !self.captured {
            return;
        }
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Host must see the cursor where the user does when the key lands.
            self.flush_motion();
            self.held_keys.insert(vk);
            send(
                &self.connector,
                self.grants,
                InputKind::KeyDown,
                vk as u32,
                0,
                0,
                0,
            );
        }
    }

    pub fn on_key_up(&mut self, sc: sdl3::keyboard::Scancode) {
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Flush-on-release may have already sent this up.
            if self.held_keys.remove(&vk) {
                send(
                    &self.connector,
                    self.grants,
                    InputKind::KeyUp,
                    vk as u32,
                    0,
                    0,
                    0,
                );
            }
        }
    }

    /// The engaging click never reaches here. Flush motion first so the down
    /// lands where the host cursor is.
    pub fn on_button_down(&mut self, b: sdl3::mouse::MouseButton) {
        if !self.captured {
            return;
        }
        self.flush_motion();
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            self.held_buttons.insert(gs);
            send(
                &self.connector,
                self.grants,
                InputKind::MouseButtonDown,
                gs,
                0,
                0,
                0,
            );
        }
    }

    pub fn on_button_up(&mut self, b: sdl3::mouse::MouseButton) {
        self.flush_motion(); // the release must not beat the motion before it
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            if self.held_buttons.remove(&gs) {
                send(
                    &self.connector,
                    self.grants,
                    InputKind::MouseButtonUp,
                    gs,
                    0,
                    0,
                    0,
                );
            }
        }
    }

    /// Wire units are WHEEL_DELTA (120), positive = up / right — same as SDL3.
    /// Fractional remainder per axis; truncating each event drops the tail.
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
            send(
                &self.connector,
                self.grants,
                InputKind::MouseScroll,
                0,
                vy,
                0,
                0,
            );
        }
        let vx = ax.trunc() as i32;
        if vx != 0 {
            ax -= f64::from(vx);
            send(
                &self.connector,
                self.grants,
                InputKind::MouseScroll,
                1,
                vx,
                0,
                0,
            );
        }
        self.scroll_acc = (ax, ay);
    }

    fn touch_slot(&mut self, finger_id: u64) -> u32 {
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            return slot;
        }
        let used: HashSet<u32> = self.touch_slots.values().copied().collect();
        let slot = (0u32..).find(|s| !used.contains(s)).unwrap_or(0);
        self.touch_slots.insert(finger_id, slot);
        slot
    }

    /// Pack client surface size so the host can rescale. Same layout as
    /// Android `nativeSendTouch`.
    fn touch_flags(w: u32, h: u32) -> u32 {
        ((w & 0xffff) << 16) | (h & 0xffff)
    }

    /// `x`/`y` are absolute in the `w`×`h` content surface, not window pixels.
    /// Ignored unless captured — the overlay is gamepad-driven.
    pub fn on_touch_down(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        let slot = self.touch_slot(finger_id);
        send(
            &self.connector,
            self.grants,
            InputKind::TouchDown,
            slot,
            x,
            y,
            Self::touch_flags(w, h),
        );
    }

    /// Skip a finger with no slot: capture engaged mid-touch has no host contact.
    pub fn on_touch_move(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            send(
                &self.connector,
                self.grants,
                InputKind::TouchMove,
                slot,
                x,
                y,
                Self::touch_flags(w, h),
            );
        }
    }

    /// Always run, even uncaptured: `release()` may have flushed the slot, but
    /// a stray up must not leave a pressed contact on the host.
    pub fn on_touch_up(&mut self, finger_id: u64) {
        if let Some(slot) = self.touch_slots.remove(&finger_id) {
            send(
                &self.connector,
                self.grants,
                InputKind::TouchUp,
                slot,
                0,
                0,
                0,
            );
        }
    }

    /// `wx`/`wy` are physical window pixels (trackpad ballistics); `abs` is the
    /// letterboxed content rect (pointer / passthrough). `Touch` goes on the
    /// wire; `Trackpad`/`Pointer` drive the gesture engine. Returns run-loop
    /// intents (`CycleStats`, dial); everything else is sent here.
    pub fn dispatch_finger(
        &mut self,
        phase: FingerPhase,
        id: u64,
        wx: f32,
        wy: f32,
        abs: Abs,
        t_ms: f64,
    ) -> Vec<Act> {
        match self.touch_mode {
            TouchMode::Touch => {
                match phase {
                    FingerPhase::Down => self.on_touch_down(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Move => self.on_touch_move(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Up => self.on_touch_up(id),
                }
                Vec::new()
            }
            TouchMode::Trackpad | TouchMode::Pointer => {
                // Down/Move only while captured. Up always runs so a lift can
                // finish a gesture after focus-loss mid-touch.
                if !self.captured && phase != FingerPhase::Up {
                    return Vec::new();
                }
                let acts = match phase {
                    FingerPhase::Down => self.gestures.down(id, wx, wy, abs, t_ms),
                    FingerPhase::Move => self.gestures.motion(id, wx, wy, abs, t_ms),
                    FingerPhase::Up => self.gestures.up(id, t_ms),
                };
                acts.into_iter()
                    .filter_map(|act| self.apply_touch_act(act))
                    .collect()
            }
        }
    }

    pub fn touch_mode(&self) -> TouchMode {
        self.touch_mode
    }

    /// Mid-stream model switch. Flush held buttons/touches first — a drag
    /// must not survive — then restart the gesture engine.
    pub fn set_touch_mode(&mut self, mode: TouchMode) {
        for b in self.held_buttons.drain() {
            send(
                &self.connector,
                self.grants,
                InputKind::MouseButtonUp,
                b,
                0,
                0,
                0,
            );
        }
        for (_, slot) in self.touch_slots.drain() {
            send(
                &self.connector,
                self.grants,
                InputKind::TouchUp,
                slot,
                0,
                0,
                0,
            );
        }
        self.touch_mode = mode;
        self.gestures = Gestures::new(mode == TouchMode::Trackpad, self.invert_scroll);
    }

    /// Down in order, up in reverse so modifiers stay held until the last key.
    pub fn send_chord(&mut self, vks: &[u8]) {
        for &vk in vks {
            send(
                &self.connector,
                self.grants,
                InputKind::KeyDown,
                u32::from(vk),
                0,
                0,
                0,
            );
        }
        for &vk in vks.iter().rev() {
            send(
                &self.connector,
                self.grants,
                InputKind::KeyUp,
                u32::from(vk),
                0,
                0,
                0,
            );
        }
    }

    /// Long-press arm, once per run-loop tick. `t_ms` is SDL ticks.
    /// [`TouchMode::Touch`] has no timer.
    pub fn tick(&mut self, t_ms: f64) {
        if !self.captured || self.touch_mode == TouchMode::Touch {
            return;
        }
        for act in self.gestures.tick(t_ms) {
            self.apply_touch_act(act);
        }
    }

    /// Track button holds in `held_buttons` so capture release flushes a
    /// tap-drag. Returns [`Act::CycleStats`] and dial intents to the run loop.
    fn apply_touch_act(&mut self, act: Act) -> Option<Act> {
        match act {
            Act::CycleStats | Act::Dial { .. } | Act::DialCommit | Act::DialCancel => {
                return Some(act)
            }
            Act::Button { gs, down } => {
                if down {
                    self.flush_motion(); // the press lands where the cursor now is
                    self.held_buttons.insert(gs);
                    send(
                        &self.connector,
                        self.grants,
                        InputKind::MouseButtonDown,
                        gs,
                        0,
                        0,
                        0,
                    );
                } else if self.held_buttons.remove(&gs) {
                    self.flush_motion();
                    send(
                        &self.connector,
                        self.grants,
                        InputKind::MouseButtonUp,
                        gs,
                        0,
                        0,
                        0,
                    );
                }
            }
            other => {
                if let Some((kind, code, x, y, flags)) = other.wire() {
                    send(&self.connector, self.grants, kind, code, x, y, flags);
                }
            }
        }
        None
    }
}
