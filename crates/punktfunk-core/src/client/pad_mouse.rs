//! Controller mouse: pads the embedder flags drive the host's pointer, buttons, scroll and a few
//! keys instead of its virtual pad (`design/controller-mouse-mode.md`).
//!
//! The translator is pure — pad state in, `InputEvent`s out — so the input task owns every send.
//! Buttons are levels, not edges: each fold recomputes the outputs a pad wants held and emits only
//! the difference, so two sources on one output (A and RT on the left button) cannot double-press.
//! Buttons already held when a pad enters stay ignored until they release, so the B that closed
//! the dial never lands as Escape. Stick speed scales with the stream height: the same hand-feel
//! on a 1080p and a 4K desktop.

use crate::input::gamepad::*;
use crate::input::{
    GamepadSnapshot, InputEvent, InputKind, MAX_PADS, PRECISE_PX_PER_DETENT, SCROLL_FLAG_PRECISE,
};
use crate::quic::{GRANT_KEYBOARD, GRANT_POINTER};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

/// Stick travel below this fraction does nothing (the 7000/32767 a daily user tuned).
const DEADZONE: f64 = 0.2;
/// Full deflection: 1.25 stream heights per second ≈ 1350 px/s at 1080p.
const POINTER_HEIGHTS_PER_S: f64 = 1.25;
const SCROLL_HEIGHTS_PER_S: f64 = 0.3;
const TRIGGER_ON: u8 = 96;
const TRIGGER_OFF: u8 = 64;
/// Pointer cadence while a stick is deflected. A resting pad sends nothing.
pub(crate) const TICK: Duration = Duration::from_millis(4);
/// A stalled task must not fling the pointer across the desktop on its next tick.
const MAX_DT: f64 = 0.05;
const FALLBACK_HEIGHT: u32 = 1080;

/// Triggers as virtual button bits, above every wire `BTN_*`.
const TRIGGER_L: u32 = 1 << 30;
const TRIGGER_R: u32 = 1 << 31;

const MOUSE_LEFT: u32 = 1;
const MOUSE_MIDDLE: u32 = 2;
const MOUSE_RIGHT: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Out {
    Mouse(u32),
    Key(u8),
}

/// Source bits → output, one entry per output bit. Back and paddles are left to the client.
const MAP: [(u32, Out); 14] = [
    (BTN_A | TRIGGER_R, Out::Mouse(MOUSE_LEFT)),
    (BTN_X | TRIGGER_L, Out::Mouse(MOUSE_RIGHT)),
    (BTN_Y, Out::Mouse(MOUSE_MIDDLE)),
    (BTN_B, Out::Key(0x1B)),
    (BTN_DPAD_UP, Out::Key(0x26)),
    (BTN_DPAD_DOWN, Out::Key(0x28)),
    (BTN_DPAD_LEFT, Out::Key(0x25)),
    (BTN_DPAD_RIGHT, Out::Key(0x27)),
    (BTN_START, Out::Key(0x0D)),
    (BTN_LB, Out::Key(0x11)),
    (BTN_RB, Out::Key(0x12)),
    (BTN_LS_CLICK, Out::Key(0x10)),
    (BTN_RS_CLICK, Out::Key(0x20)),
    (BTN_GUIDE, Out::Key(0x5B)),
];

/// The embedder's request, shared between `NativeClient` and the input task.
#[derive(Default)]
pub(crate) struct PadMouseShared {
    requested: AtomicU16,
    /// Pads the host holds: declared or driven, not yet removed. Written by the input task.
    live: AtomicU16,
    pub(crate) changed: tokio::sync::Notify,
}

impl PadMouseShared {
    pub(crate) fn live(&self) -> u16 {
        self.live.load(Ordering::Relaxed)
    }

    pub(crate) fn set_live(&self, mask: u16) {
        self.live.store(mask, Ordering::Relaxed);
    }

    pub(crate) fn request(&self, mask: u16) {
        self.requested.store(mask, Ordering::Relaxed);
        self.changed.notify_one();
    }

    pub(crate) fn requested(&self) -> u16 {
        self.requested.load(Ordering::Relaxed)
    }

    /// Pads in mouse mode under `grants`: none without the pointer grant.
    pub(crate) fn active(&self, grants: u32) -> u16 {
        if grants & GRANT_POINTER != 0 {
            self.requested()
        } else {
            0
        }
    }

    pub(crate) fn clear(&self, pad: usize) {
        self.requested.fetch_and(!(1 << pad), Ordering::Relaxed);
    }

    pub(crate) fn clear_all(&self) {
        self.requested.store(0, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Pad {
    on: bool,
    snap: GamepadSnapshot,
    /// Sources held at enter, dropped bit by bit as they release.
    ignore: u32,
    lt: bool,
    rt: bool,
    /// `MAP` entries down on the wire.
    out: u16,
    /// Sub-unit carry: pointer x, y; scroll x, y.
    rem: [f64; 4],
}

impl Pad {
    fn sources(&mut self) -> u32 {
        self.lt = hysteresis(self.lt, self.snap.left_trigger);
        self.rt = hysteresis(self.rt, self.snap.right_trigger);
        self.snap.buttons
            | if self.lt { TRIGGER_L } else { 0 }
            | if self.rt { TRIGGER_R } else { 0 }
    }
}

fn hysteresis(on: bool, v: u8) -> bool {
    if on {
        v >= TRIGGER_OFF
    } else {
        v >= TRIGGER_ON
    }
}

/// Stick → unit vector scaled by the squared travel past the deadzone. `+y` stays up.
fn curve(x: i16, y: i16) -> (f64, f64) {
    let (fx, fy) = (f64::from(x) / 32767.0, f64::from(y) / 32767.0);
    let mag = fx.hypot(fy);
    if mag <= DEADZONE {
        return (0.0, 0.0);
    }
    let t = ((mag - DEADZONE) / (1.0 - DEADZONE)).min(1.0);
    let s = t * t / mag;
    (fx * s, fy * s)
}

fn event(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
    InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    }
}

fn press(out: Out, down: bool) -> InputEvent {
    match (out, down) {
        (Out::Mouse(b), true) => event(InputKind::MouseButtonDown, b, 0, 0, 0),
        (Out::Mouse(b), false) => event(InputKind::MouseButtonUp, b, 0, 0, 0),
        (Out::Key(vk), true) => event(InputKind::KeyDown, u32::from(vk), 0, 0, 0),
        (Out::Key(vk), false) => event(InputKind::KeyUp, u32::from(vk), 0, 0, 0),
    }
}

fn granted(out: Out, grants: u32) -> bool {
    match out {
        Out::Mouse(_) => grants & GRANT_POINTER != 0,
        Out::Key(_) => grants & GRANT_KEYBOARD != 0,
    }
}

/// Whole units out of `*rem + v`, the fraction carried.
fn take(rem: &mut f64, v: f64) -> i32 {
    *rem += v;
    let whole = rem.trunc();
    *rem -= whole;
    whole as i32
}

#[derive(Default)]
pub(crate) struct PadMouse {
    pads: [Pad; MAX_PADS],
}

impl PadMouse {
    pub(crate) fn is_on(&self, pad: usize) -> bool {
        self.pads.get(pad).is_some_and(|p| p.on)
    }

    pub(crate) fn on_mask(&self) -> u16 {
        (0..MAX_PADS)
            .filter(|&i| self.pads[i].on)
            .fold(0, |m, i| m | 1 << i)
    }

    /// Start translating `pad` from its current state. Held buttons stay ignored until released.
    pub(crate) fn enter(&mut self, pad: usize, snap: GamepadSnapshot) {
        let mut p = Pad {
            on: true,
            snap,
            ..Pad::default()
        };
        p.ignore = p.sources();
        self.pads[pad] = p;
    }

    /// Release every output `pad` holds and stop translating it.
    pub(crate) fn leave(&mut self, pad: usize) -> Vec<InputEvent> {
        let out = std::mem::take(&mut self.pads[pad]).out;
        (0..MAP.len())
            .filter(|i| out & 1 << i != 0)
            .map(|i| press(MAP[i].1, false))
            .collect()
    }

    /// Fold one button/axis event and emit the output edges it causes.
    pub(crate) fn fold(&mut self, pad: usize, ev: &InputEvent, grants: u32) -> Vec<InputEvent> {
        let p = &mut self.pads[pad];
        if !p.on || !p.snap.fold(ev) {
            return Vec::new();
        }
        let sources = p.sources();
        p.ignore &= sources;
        let live = sources & !p.ignore;
        let mut want = 0u16;
        for (i, &(src, out)) in MAP.iter().enumerate() {
            if live & src != 0 && granted(out, grants) {
                want |= 1 << i;
            }
        }
        let changed = want ^ p.out;
        p.out = want;
        let ups = (0..MAP.len()).filter(|i| changed & 1 << i != 0 && want & 1 << i == 0);
        let downs = (0..MAP.len()).filter(|i| want & changed & 1 << i != 0);
        ups.map(|i| press(MAP[i].1, false))
            .chain(downs.map(|i| press(MAP[i].1, true)))
            .collect()
    }

    /// True while a translated pad has a stick past the deadzone.
    pub(crate) fn moving(&self) -> bool {
        self.pads.iter().any(|p| {
            p.on && (curve(p.snap.ls_x, p.snap.ls_y) != (0.0, 0.0)
                || curve(p.snap.rs_x, p.snap.rs_y) != (0.0, 0.0))
        })
    }

    /// Pointer motion and scroll for `dt_s` of stick deflection on a `height`-pixel stream.
    pub(crate) fn tick(&mut self, dt_s: f64, height: u32, grants: u32) -> Vec<InputEvent> {
        let mut evs = Vec::new();
        if grants & GRANT_POINTER == 0 {
            return evs;
        }
        let h = f64::from(if height == 0 { FALLBACK_HEIGHT } else { height });
        let dt = dt_s.clamp(0.0, MAX_DT);
        let px = POINTER_HEIGHTS_PER_S * h * dt;
        let units = SCROLL_HEIGHTS_PER_S * h * 120.0 / PRECISE_PX_PER_DETENT * dt;
        for p in self.pads.iter_mut().filter(|p| p.on) {
            let (cx, cy) = curve(p.snap.ls_x, p.snap.ls_y);
            let dx = take(&mut p.rem[0], cx * px);
            let dy = take(&mut p.rem[1], -cy * px);
            if dx != 0 || dy != 0 {
                evs.push(event(InputKind::MouseMove, 0, dx, dy, 0));
            }
            let (sx, sy) = curve(p.snap.rs_x, p.snap.rs_y);
            let vy = take(&mut p.rem[3], sy * units);
            if vy != 0 {
                evs.push(event(InputKind::MouseScroll, 0, vy, 0, SCROLL_FLAG_PRECISE));
            }
            let vx = take(&mut p.rem[2], sx * units);
            if vx != 0 {
                evs.push(event(InputKind::MouseScroll, 1, vx, 0, SCROLL_FLAG_PRECISE));
            }
        }
        evs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::GRANT_ALL;

    fn button(bit: u32, down: bool) -> InputEvent {
        event(InputKind::GamepadButton, bit, down as i32, 0, 0)
    }

    fn axis(code: u32, v: i32) -> InputEvent {
        event(InputKind::GamepadAxis, code, v, 0, 0)
    }

    fn kinds(evs: &[InputEvent]) -> Vec<(InputKind, u32)> {
        evs.iter().map(|e| (e.kind, e.code)).collect()
    }

    fn entered() -> PadMouse {
        let mut m = PadMouse::default();
        m.enter(0, GamepadSnapshot::default());
        m
    }

    #[test]
    fn a_and_right_trigger_share_the_left_button() {
        let mut m = entered();
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_A, true), GRANT_ALL)),
            [(InputKind::MouseButtonDown, 1)]
        );
        assert!(
            m.fold(0, &axis(AXIS_RT, 255), GRANT_ALL).is_empty(),
            "already down"
        );
        assert!(
            m.fold(0, &button(BTN_A, false), GRANT_ALL).is_empty(),
            "RT still holds it"
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_RT, 0), GRANT_ALL)),
            [(InputKind::MouseButtonUp, 1)]
        );
    }

    #[test]
    fn layout_rows() {
        let rows = [
            (BTN_X, InputKind::MouseButtonDown, MOUSE_RIGHT),
            (BTN_Y, InputKind::MouseButtonDown, MOUSE_MIDDLE),
            (BTN_B, InputKind::KeyDown, 0x1B),
            (BTN_START, InputKind::KeyDown, 0x0D),
            (BTN_DPAD_LEFT, InputKind::KeyDown, 0x25),
            (BTN_LB, InputKind::KeyDown, 0x11),
            (BTN_GUIDE, InputKind::KeyDown, 0x5B),
        ];
        for (bit, kind, code) in rows {
            let mut m = entered();
            assert_eq!(
                kinds(&m.fold(0, &button(bit, true), GRANT_ALL)),
                [(kind, code)],
                "{bit:#x}"
            );
        }
        let mut m = entered();
        assert!(
            m.fold(0, &button(BTN_BACK, true), GRANT_ALL).is_empty(),
            "Back belongs to the dial"
        );
    }

    #[test]
    fn trigger_hysteresis_holds_between_thresholds() {
        let mut m = entered();
        assert!(
            m.fold(0, &axis(AXIS_LT, 80), GRANT_ALL).is_empty(),
            "below on"
        );
        assert_eq!(m.fold(0, &axis(AXIS_LT, 100), GRANT_ALL).len(), 1);
        assert!(
            m.fold(0, &axis(AXIS_LT, 70), GRANT_ALL).is_empty(),
            "above off"
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 10), GRANT_ALL)),
            [(InputKind::MouseButtonUp, 3)]
        );
    }

    #[test]
    fn a_button_held_at_enter_fires_only_after_a_fresh_press() {
        let mut m = PadMouse::default();
        m.enter(
            0,
            GamepadSnapshot {
                buttons: BTN_B,
                ..Default::default()
            },
        );
        assert!(m.fold(0, &axis(AXIS_LS_X, 0), GRANT_ALL).is_empty());
        assert!(
            m.fold(0, &button(BTN_B, false), GRANT_ALL).is_empty(),
            "no Escape up for a press never sent"
        );
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::KeyDown, 0x1B)]
        );
    }

    #[test]
    fn leave_releases_everything_held() {
        let mut m = entered();
        m.fold(0, &button(BTN_A, true), GRANT_ALL);
        m.fold(0, &button(BTN_LB, true), GRANT_ALL);
        let mut released = kinds(&m.leave(0));
        released.sort_by_key(|&(_, c)| c);
        assert_eq!(
            released,
            [(InputKind::MouseButtonUp, 1), (InputKind::KeyUp, 0x11)]
        );
        assert!(!m.is_on(0));
        assert!(
            m.fold(0, &button(BTN_A, false), GRANT_ALL).is_empty(),
            "off pads translate nothing"
        );
    }

    #[test]
    fn keys_need_the_keyboard_grant() {
        let mut m = entered();
        assert!(m.fold(0, &button(BTN_B, true), GRANT_POINTER).is_empty());
        assert_eq!(m.fold(0, &button(BTN_A, true), GRANT_POINTER).len(), 1);
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        assert!(
            m.tick(0.01, 1080, GRANT_KEYBOARD).is_empty(),
            "no pointer grant, no motion"
        );
    }

    #[test]
    fn full_deflection_moves_one_and_a_quarter_heights_per_second() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        m.fold(0, &axis(AXIS_LS_Y, 0), GRANT_ALL);
        let mut dx = 0;
        for _ in 0..250 {
            for ev in m.tick(0.004, 1080, GRANT_ALL) {
                assert_eq!(ev.kind, InputKind::MouseMove);
                dx += ev.x;
            }
        }
        assert!((1349..=1350).contains(&dx), "{dx} px in 1 s");
        let mut m4k = entered();
        m4k.fold(0, &axis(AXIS_LS_Y, 32767), GRANT_ALL);
        let dy: i32 = m4k.tick(0.04, 2160, GRANT_ALL).iter().map(|e| e.y).sum();
        assert_eq!(dy, -108, "stick up is screen up, twice as fast at 2160p");
    }

    #[test]
    fn deadzone_and_curve() {
        assert_eq!(curve(6000, 0), (0.0, 0.0));
        let (half, _) = curve(19660, 0);
        assert!(
            (half - 0.25).abs() < 0.01,
            "60% travel → (0.4/0.8)² = 0.25, got {half}"
        );
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 3000), GRANT_ALL);
        assert!(!m.moving());
        assert!(m.tick(1.0, 1080, GRANT_ALL).is_empty());
    }

    #[test]
    fn right_stick_scrolls_precise_both_axes() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        m.fold(0, &axis(AXIS_RS_X, -32767), GRANT_ALL);
        assert!(m.moving());
        let evs = m.tick(0.01, 1000, GRANT_ALL);
        assert_eq!(evs.len(), 2);
        // 0.3 × 1000 px/s × 12 units/px × 10 ms = 36 units at full travel; the diagonal is
        // radial, so each axis carries 1/√2 of it.
        assert_eq!(
            (evs[0].code, evs[0].x, evs[0].flags),
            (0, 25, SCROLL_FLAG_PRECISE),
            "up is positive"
        );
        assert_eq!((evs[1].code, evs[1].x), (1, -25), "left is negative");
    }

    #[test]
    fn a_long_stall_is_clamped() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        let dx: i32 = m.tick(5.0, 1080, GRANT_ALL).iter().map(|e| e.x).sum();
        assert_eq!(dx, 67, "50 ms at 1350 px/s");
    }

    #[test]
    fn shared_mask_follows_the_pointer_grant() {
        let s = PadMouseShared::default();
        s.request(0b101);
        assert_eq!(s.active(GRANT_ALL), 0b101);
        assert_eq!(s.active(GRANT_KEYBOARD), 0);
        s.clear(2);
        assert_eq!(s.requested(), 0b001);
        let mut m = PadMouse::default();
        m.enter(3, GamepadSnapshot::default());
        assert_eq!(m.on_mask(), 0b1000);
    }

    /// Every row of [`MAP`], down and up, plus both triggers. Changing any row fails here.
    #[test]
    fn every_shipped_row_presses_and_releases() {
        let shipped = [
            (BTN_A, InputKind::MouseButtonDown, MOUSE_LEFT),
            (BTN_X, InputKind::MouseButtonDown, MOUSE_RIGHT),
            (BTN_Y, InputKind::MouseButtonDown, MOUSE_MIDDLE),
            (BTN_B, InputKind::KeyDown, 0x1B),
            (BTN_DPAD_UP, InputKind::KeyDown, 0x26),
            (BTN_DPAD_DOWN, InputKind::KeyDown, 0x28),
            (BTN_DPAD_LEFT, InputKind::KeyDown, 0x25),
            (BTN_DPAD_RIGHT, InputKind::KeyDown, 0x27),
            (BTN_START, InputKind::KeyDown, 0x0D),
            (BTN_LB, InputKind::KeyDown, 0x11),
            (BTN_RB, InputKind::KeyDown, 0x12),
            (BTN_LS_CLICK, InputKind::KeyDown, 0x10),
            (BTN_RS_CLICK, InputKind::KeyDown, 0x20),
            (BTN_GUIDE, InputKind::KeyDown, 0x5B),
        ];
        for (bit, down, code) in shipped {
            let up = match down {
                InputKind::KeyDown => InputKind::KeyUp,
                _ => InputKind::MouseButtonUp,
            };
            let mut m = entered();
            assert_eq!(
                kinds(&m.fold(0, &button(bit, true), GRANT_ALL)),
                [(down, code)],
                "{bit:#x} down"
            );
            assert_eq!(
                kinds(&m.fold(0, &button(bit, false), GRANT_ALL)),
                [(up, code)],
                "{bit:#x} up"
            );
        }
        let mut m = entered();
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_RT, 255), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_LEFT)]
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 255), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
    }
}
