//! Controller mouse: pads the embedder flags drive the host's pointer, buttons, scroll and keys
//! instead of its virtual pad (`design/controller-mouse-mode.md`).
//!
//! The translator is pure — pad state in, `InputEvent`s out — so the input task owns every send.
//! Buttons are levels, not edges: each fold recomputes the outputs a pad wants held and emits only
//! the difference, so two sources on one output (A and RT on the left button) cannot double-press.
//! Buttons already held when a pad enters stay ignored until they release, so the B that closed
//! the dial never lands as Escape. A chord borrows its buttons: while all of them are down none
//! drives its plain output, and a chord that fired keeps them silent on the way up. Stick speed
//! scales with the stream height: the same hand-feel on a 1080p and a 4K desktop.
//!
//! [`Layout`] holds the whole table — buttons, chords, tunables — as a document the embedder
//! hands in, [`Layout::default`] being the shipped one. A pad keeps the layout it entered with,
//! so an edit lands on the next entry instead of mid-drag.

use crate::error::{PunktfunkError, Result};
use crate::input::gamepad::*;
use crate::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};
use crate::input::{key_vk, GamepadSnapshot, InputEvent, InputKind, MAX_PADS};
use crate::quic::{GRANT_KEYBOARD, GRANT_POINTER};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Stick travel below this fraction does nothing (the 7000/32767 a daily user tuned).
const DEADZONE: f64 = 0.2;
/// Full deflection: 1.25 stream heights per second ≈ 1350 px/s at 1080p.
const POINTER_HEIGHTS_PER_S: f64 = 1.25;
const SCROLL_HEIGHTS_PER_S: f64 = 0.3;
/// A hold reaching this is a long press: past a stiff button's travel, short enough to chain.
const LONG_PRESS_MS: f64 = 400.0;
const TRIGGER_ON: u8 = 96;
const TRIGGER_OFF: u8 = 64;
/// Pointer cadence while a stick is deflected or a chord counts. A resting pad sends nothing.
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

/// When a chord's keys fire, timed from the moment its last button goes down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Press {
    /// On press.
    Any,
    /// On release, if the hold stayed under `long_press_ms`.
    Short,
    /// Once, on reaching `long_press_ms`.
    Long,
    /// Down on reaching `long_press_ms`, up when the chord releases.
    Hold,
}

/// Buttons that must all be down, and the keys they send. Same buttons may carry a `short` and a
/// `long` chord at once — that pair is the point.
struct Chord {
    buttons: u32,
    press: Press,
    keys: Vec<u8>,
}

/// The shipped table: source bits → output, one entry per output bit. Back and the paddles are
/// left to the client.
const DEFAULT_BUTTONS: [(u32, Out); 14] = [
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

/// One controller-mouse table. `pointer` and `scroll` multiply the shipped speeds.
pub(crate) struct Layout {
    pointer: f64,
    scroll: f64,
    deadzone: f64,
    long_press_ms: f64,
    /// Source bits → output. One entry per output bit, so two sources share one level.
    buttons: Vec<(u32, Out)>,
    chords: Vec<Chord>,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            pointer: 1.0,
            scroll: 1.0,
            deadzone: DEADZONE,
            long_press_ms: LONG_PRESS_MS,
            buttons: DEFAULT_BUTTONS.to_vec(),
            chords: Vec::new(),
        }
    }
}

/// Wire button name → source bits. `LT`/`RT` are the virtual trigger bits, so a chord holding a
/// trigger gets the same hysteresis a plain output does.
fn button_bits(name: &str) -> Option<u32> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "a" => BTN_A,
        "b" => BTN_B,
        "x" => BTN_X,
        "y" => BTN_Y,
        "lb" => BTN_LB,
        "rb" => BTN_RB,
        "lt" => TRIGGER_L,
        "rt" => TRIGGER_R,
        "ls" => BTN_LS_CLICK,
        "rs" => BTN_RS_CLICK,
        "guide" => BTN_GUIDE,
        "start" => BTN_START,
        "back" => BTN_BACK,
        "up" => BTN_DPAD_UP,
        "down" => BTN_DPAD_DOWN,
        "left" => BTN_DPAD_LEFT,
        "right" => BTN_DPAD_RIGHT,
        _ => return None,
    })
}

/// `mouse:left` / `mouse:middle` / `mouse:right`, or `key:` plus a [`key_vk`] name.
fn out_of(spec: &str) -> Option<Out> {
    let s = spec.trim();
    if let Some(b) = s.strip_prefix("mouse:") {
        return Some(Out::Mouse(match b.trim().to_ascii_lowercase().as_str() {
            "left" => MOUSE_LEFT,
            "middle" => MOUSE_MIDDLE,
            "right" => MOUSE_RIGHT,
            _ => return None,
        }));
    }
    key_vk(s.strip_prefix("key:")?).map(Out::Key)
}

/// The handed-in document, mirroring the TOML in the input docs one field for one field.
#[derive(serde::Deserialize)]
struct Doc {
    #[serde(default)]
    settings: DocSettings,
    /// Absent keeps the shipped table; present replaces it whole, so `{}` means no plain buttons.
    buttons: Option<BTreeMap<String, String>>,
    #[serde(default)]
    chords: Vec<DocChord>,
}

#[derive(serde::Deserialize)]
#[serde(default)]
struct DocSettings {
    pointer: f64,
    scroll: f64,
    deadzone: f64,
    long_press_ms: f64,
}

impl Default for DocSettings {
    fn default() -> Self {
        DocSettings {
            pointer: 1.0,
            scroll: 1.0,
            deadzone: DEADZONE,
            long_press_ms: LONG_PRESS_MS,
        }
    }
}

/// A `name` here is the editor's label. The translator never reads it, so serde drops it.
#[derive(serde::Deserialize)]
struct DocChord {
    buttons: Vec<String>,
    press: String,
    keys: Vec<String>,
}

impl Layout {
    /// Read a layout document. An unknown name is an error and leaves the live layout alone; an
    /// out-of-range tunable clamps instead, so no slider can strand a pad.
    pub(crate) fn parse(json: &str) -> Result<Layout> {
        let doc: Doc = serde_json::from_str(json)
            .map_err(|_| PunktfunkError::InvalidArg("pad mouse layout is not valid JSON"))?;
        let s = doc.settings;
        let tunables = [s.pointer, s.scroll, s.deadzone, s.long_press_ms];
        if tunables.iter().any(|v| !v.is_finite()) {
            return Err(PunktfunkError::InvalidArg(
                "pad mouse tunable is not a number",
            ));
        }
        let mut buttons = DEFAULT_BUTTONS.to_vec();
        if let Some(rows) = &doc.buttons {
            buttons.clear();
            for (name, spec) in rows {
                let src = button_bits(name)
                    .ok_or(PunktfunkError::InvalidArg("unknown pad mouse button"))?;
                let out =
                    out_of(spec).ok_or(PunktfunkError::InvalidArg("unknown pad mouse output"))?;
                // One entry per output bit: a second source joins the entry it shares, so
                // neither of them can double-press the level.
                match buttons.iter_mut().find(|(_, o)| *o == out) {
                    Some(e) => e.0 |= src,
                    None => buttons.push((src, out)),
                }
            }
        }
        let mut chords = Vec::new();
        for c in &doc.chords {
            let press = match c.press.trim().to_ascii_lowercase().as_str() {
                "any" => Press::Any,
                "short" => Press::Short,
                "long" => Press::Long,
                "hold" => Press::Hold,
                _ => return Err(PunktfunkError::InvalidArg("unknown chord press kind")),
            };
            let mut bits = 0;
            for b in &c.buttons {
                bits |= button_bits(b).ok_or(PunktfunkError::InvalidArg("unknown chord button"))?;
            }
            let keys: Vec<u8> = c
                .keys
                .iter()
                .map(|k| key_vk(k))
                .collect::<Option<_>>()
                .ok_or(PunktfunkError::InvalidArg("unknown chord key"))?;
            if bits == 0 || keys.is_empty() {
                return Err(PunktfunkError::InvalidArg("chord needs a button and a key"));
            }
            chords.push(Chord {
                buttons: bits,
                press,
                keys,
            });
        }
        Ok(Layout {
            pointer: s.pointer.clamp(0.05, 10.0),
            scroll: s.scroll.clamp(0.05, 10.0),
            deadzone: s.deadzone.clamp(0.0, 0.9),
            long_press_ms: s.long_press_ms.clamp(50.0, 5_000.0),
            buttons,
            chords,
        })
    }
}

/// The embedder's request, shared between `NativeClient` and the input task.
#[derive(Default)]
pub(crate) struct PadMouseShared {
    requested: AtomicU16,
    /// Pads the host holds: declared or driven, not yet removed. Written by the input task.
    live: AtomicU16,
    /// What the next entering pad adopts.
    layout: Mutex<Arc<Layout>>,
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

    /// Layout an entering pad adopts. Locked once per entry, never per event.
    pub(crate) fn layout(&self) -> Arc<Layout> {
        self.layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_layout(&self, layout: Layout) {
        *self.layout.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(layout);
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

/// A chord's progress on one pad. `held_ms` rides [`PadMouse::tick`], never a clock read, so a
/// test drives the thresholds by handing in its own `dt`.
#[derive(Clone, Copy, Default)]
struct ChordState {
    armed: bool,
    held_ms: f64,
    /// Keys already sent: no second fire, and no plain output on the way up.
    fired: bool,
    /// `hold` only: the keys are down and owe an up.
    holding: bool,
}

#[derive(Clone, Default)]
struct ControllerScrollAxis {
    active: bool,
    remainder: f64,
}

impl ControllerScrollAxis {
    fn step(&mut self, axis: u32, velocity: f64, units: f64, pointer: bool) -> Option<InputEvent> {
        if !pointer || velocity == 0.0 {
            self.remainder = 0.0;
            return std::mem::take(&mut self.active).then(|| {
                scroll_event(
                    axis,
                    0,
                    if pointer {
                        ScrollPhase::End
                    } else {
                        ScrollPhase::Cancel
                    },
                )
            });
        }
        let delta = take(&mut self.remainder, velocity * units);
        if delta == 0 {
            return None;
        }
        let phase = if std::mem::replace(&mut self.active, true) {
            ScrollPhase::Update
        } else {
            ScrollPhase::Begin
        };
        Some(scroll_event(axis, delta, phase))
    }
}

#[derive(Clone, Default)]
struct Pad {
    on: bool,
    snap: GamepadSnapshot,
    /// Sources held at enter, dropped bit by bit as they release.
    ignore: u32,
    lt: bool,
    rt: bool,
    /// [`Layout::buttons`] entries down on the wire.
    out: u32,
    /// Sub-pixel carry for pointer x and y.
    rem: [f64; 2],
    /// Vertical and horizontal scroll own their phase and Q24.8 remainder.
    scroll: [ControllerScrollAxis; 2],
    layout: Arc<Layout>,
    /// One per [`Layout::chords`] entry, same order.
    chords: Vec<ChordState>,
}

impl Pad {
    fn sources(&mut self) -> u32 {
        self.lt = hysteresis(self.lt, self.snap.left_trigger);
        self.rt = hysteresis(self.rt, self.snap.right_trigger);
        self.snap.buttons
            | if self.lt { TRIGGER_L } else { 0 }
            | if self.rt { TRIGGER_R } else { 0 }
    }

    /// [`Layout::buttons`] entries `live` wants held, as index bits.
    fn want(&self, live: u32, grants: u32) -> u32 {
        self.layout
            .buttons
            .iter()
            .enumerate()
            .filter(|(_, (src, out))| live & *src != 0 && granted(*out, grants))
            .fold(0, |m, (i, _)| m | 1 << i)
    }

    /// Arm and release chords against `live`. Returns the buttons armed chords borrow, the
    /// buttons owed their plain output as a tap, and what arming or releasing sent.
    fn fold_chords(&mut self, live: u32, grants: u32) -> (u32, u32, Vec<InputEvent>) {
        let layout = self.layout.clone();
        let (mut borrowed, mut owed, mut silent) = (0u32, 0u32, 0u32);
        let mut evs = Vec::new();
        for (c, st) in layout.chords.iter().zip(self.chords.iter_mut()) {
            if live & c.buttons == c.buttons {
                borrowed |= c.buttons;
                if !st.armed {
                    *st = ChordState {
                        armed: true,
                        fired: c.press == Press::Any,
                        ..ChordState::default()
                    };
                    if st.fired {
                        evs.extend(tap(&c.keys, grants));
                    }
                }
                continue;
            }
            if !st.armed {
                continue;
            }
            let tapped = st.held_ms < layout.long_press_ms;
            if st.holding {
                evs.extend(key_seq(&c.keys, false, grants));
            } else if !st.fired && c.press == Press::Short && tapped {
                st.fired = true;
                evs.extend(tap(&c.keys, grants));
            }
            // A chord that fired keeps its buttons quiet. One that never did hands a button
            // released under the threshold back to its plain output.
            if st.fired {
                silent |= c.buttons;
            } else if tapped {
                owed |= c.buttons & !live;
            }
            *st = ChordState::default();
        }
        (borrowed, owed & !silent, evs)
    }

    /// Count `dt_ms` into every armed chord and fire the ones reaching the threshold.
    fn advance(&mut self, dt_ms: f64, grants: u32) -> Vec<InputEvent> {
        let layout = self.layout.clone();
        let mut evs = Vec::new();
        for (c, st) in layout.chords.iter().zip(self.chords.iter_mut()) {
            if !st.armed || st.fired || st.held_ms >= layout.long_press_ms {
                continue;
            }
            st.held_ms += dt_ms;
            if st.held_ms < layout.long_press_ms {
                continue;
            }
            match c.press {
                Press::Long => {
                    st.fired = true;
                    evs.extend(tap(&c.keys, grants));
                }
                Press::Hold => {
                    st.fired = true;
                    st.holding = true;
                    evs.extend(key_seq(&c.keys, true, grants));
                }
                // A short press is now too long to fire, and `any` fired on the way down.
                Press::Short | Press::Any => {}
            }
        }
        evs
    }

    fn deflected(&self) -> bool {
        let d = self.layout.deadzone;
        curve(self.snap.ls_x, self.snap.ls_y, d) != (0.0, 0.0)
            || curve(self.snap.rs_x, self.snap.rs_y, d) != (0.0, 0.0)
    }

    /// A chord whose threshold is still ahead. `short` needs the clock too — it is what tells a
    /// tap from a hold on the way up.
    fn counting(&self) -> bool {
        self.chords
            .iter()
            .any(|s| s.armed && !s.fired && s.held_ms < self.layout.long_press_ms)
    }
}

fn hysteresis(on: bool, v: u8) -> bool {
    if on {
        v >= TRIGGER_OFF
    } else {
        v >= TRIGGER_ON
    }
}

/// Stick → unit vector scaled by the squared travel past `deadzone`. `+y` stays up.
fn curve(x: i16, y: i16, deadzone: f64) -> (f64, f64) {
    let (fx, fy) = (f64::from(x) / 32767.0, f64::from(y) / 32767.0);
    let mag = fx.hypot(fy);
    if mag <= deadzone {
        return (0.0, 0.0);
    }
    let t = ((mag - deadzone) / (1.0 - deadzone)).min(1.0);
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

/// `keys` down in order, or up in reverse. Empty without the keyboard grant, and a key that
/// never went down owes no up.
fn key_seq(keys: &[u8], down: bool, grants: u32) -> Vec<InputEvent> {
    if grants & GRANT_KEYBOARD == 0 {
        return Vec::new();
    }
    let mut evs: Vec<_> = keys.iter().map(|&k| press(Out::Key(k), down)).collect();
    if !down {
        evs.reverse();
    }
    evs
}

/// The whole sequence down and straight back up.
fn tap(keys: &[u8], grants: u32) -> Vec<InputEvent> {
    let mut evs = key_seq(keys, true, grants);
    evs.extend(key_seq(keys, false, grants));
    evs
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

/// Normalized controller scroll: `delta` is Q24.8 DIP on wire `axis` 0 or 1.
fn scroll_event(axis: u32, delta: i32, phase: ScrollPhase) -> InputEvent {
    ScrollEvent {
        source: ScrollSource::Controller,
        phase,
        axis,
        delta,
    }
    .to_event()
}

fn tick_scroll(p: &mut Pad, dt: f64, pointer: bool, evs: &mut Vec<InputEvent>) {
    // The fixed 1080-DIP reference keeps stream resolution out of scroll speed.
    let units =
        SCROLL_HEIGHTS_PER_S * p.layout.scroll * f64::from(FALLBACK_HEIGHT) * SCROLL_SCALE * dt;
    let (sx, sy) = curve(p.snap.rs_x, p.snap.rs_y, p.layout.deadzone);
    evs.extend(p.scroll[0].step(0, sy, units, pointer));
    evs.extend(p.scroll[1].step(1, sx, units, pointer));
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

    /// Start translating `pad` from its current state under `layout`. Held buttons stay ignored
    /// until they release; a later edit reaches this pad on its next entry, not mid-drag.
    pub(crate) fn enter(&mut self, pad: usize, snap: GamepadSnapshot, layout: Arc<Layout>) {
        let mut p = Pad {
            on: true,
            snap,
            chords: vec![ChordState::default(); layout.chords.len()],
            layout,
            ..Pad::default()
        };
        p.ignore = p.sources();
        self.pads[pad] = p;
    }

    /// Release every output `pad` holds, plain and chord, and stop translating it.
    pub(crate) fn leave(&mut self, pad: usize) -> Vec<InputEvent> {
        let p = std::mem::take(&mut self.pads[pad]);
        let mut evs: Vec<_> = (0..2u32)
            .filter(|&a| p.scroll[a as usize].active)
            .map(|a| scroll_event(a, 0, ScrollPhase::Cancel))
            .collect();
        evs.extend(
            (0..p.layout.buttons.len())
                .filter(|i| p.out & 1 << i != 0)
                .map(|i| press(p.layout.buttons[i].1, false)),
        );
        for (c, st) in p.layout.chords.iter().zip(&p.chords) {
            if st.holding {
                evs.extend(key_seq(&c.keys, false, GRANT_KEYBOARD));
            }
        }
        evs
    }

    /// Fold one button/axis event and emit the output edges it causes. Plain releases go first so
    /// a borrowed modifier is up before its chord fires, and plain presses last so one still held
    /// comes back after.
    pub(crate) fn fold(&mut self, pad: usize, ev: &InputEvent, grants: u32) -> Vec<InputEvent> {
        let p = &mut self.pads[pad];
        if !p.on || !p.snap.fold(ev) {
            return Vec::new();
        }
        let sources = p.sources();
        p.ignore &= sources;
        let live = sources & !p.ignore;
        let (borrowed, owed, chord_evs) = p.fold_chords(live, grants);
        let want = p.want(live & !borrowed, grants);
        let changed = want ^ p.out;
        p.out = want;
        let layout = p.layout.clone();
        let held = |i: usize| layout.buttons[i].1;
        let ups = (0..layout.buttons.len())
            .filter(|i| changed & 1 << i != 0 && want & 1 << i == 0)
            .map(|i| press(held(i), false));
        let owed_taps = (0..layout.buttons.len())
            .filter(|&i| want & 1 << i == 0 && layout.buttons[i].0 & owed != 0)
            .filter(|&i| granted(held(i), grants))
            .flat_map(|i| [press(held(i), true), press(held(i), false)]);
        let downs = (0..layout.buttons.len())
            .filter(|i| want & changed & 1 << i != 0)
            .map(|i| press(held(i), true));
        ups.collect::<Vec<_>>()
            .into_iter()
            .chain(chord_evs)
            .chain(owed_taps)
            .chain(downs)
            .collect()
    }

    /// True while a translated pad has a stick past the deadzone, a chord still counting, or a
    /// scroll axis owing its `End`.
    pub(crate) fn ticking(&self) -> bool {
        self.pads
            .iter()
            .any(|p| p.on && (p.deflected() || p.counting() || p.scroll.iter().any(|a| a.active)))
    }

    /// Chord thresholds, pointer motion and scroll for `dt_s` on a `height`-pixel stream.
    pub(crate) fn tick(&mut self, dt_s: f64, height: u32, grants: u32) -> Vec<InputEvent> {
        let dt = dt_s.clamp(0.0, MAX_DT);
        let pointer = grants & GRANT_POINTER != 0;
        let h = f64::from(if height == 0 { FALLBACK_HEIGHT } else { height });
        let mut evs = Vec::new();
        for p in self.pads.iter_mut().filter(|p| p.on) {
            evs.extend(p.advance(dt * 1000.0, grants));
            if !pointer {
                // The grant went mid-gesture: cancel open scroll axes so the host is not left
                // holding a live interaction, and drop the carried fractions.
                tick_scroll(p, dt, false, &mut evs);
                continue;
            }
            let px = POINTER_HEIGHTS_PER_S * p.layout.pointer * h * dt;
            let (cx, cy) = curve(p.snap.ls_x, p.snap.ls_y, p.layout.deadzone);
            let dx = take(&mut p.rem[0], cx * px);
            let dy = take(&mut p.rem[1], -cy * px);
            if dx != 0 || dy != 0 {
                evs.push(event(InputKind::MouseMove, 0, dx, dy, 0));
            }
            tick_scroll(p, dt, true, &mut evs);
        }
        evs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::GRANT_ALL;

    #[test]
    fn scroll_axis_neutral_discards_fraction_without_ending_a_subunit_move() {
        let mut axis = ControllerScrollAxis::default();
        assert!(axis.step(0, 1.0, 0.4, true).is_none());
        assert!(axis.step(0, 0.0, 0.4, true).is_none());
        assert!(axis.step(0, 1.0, 0.4, true).is_none());
        assert!(axis.step(0, 1.0, 0.4, true).is_none());
        let start = axis.step(0, 1.0, 0.4, true).unwrap();
        assert_eq!(
            ScrollEvent::from_event(&start).unwrap().phase,
            ScrollPhase::Begin
        );
        assert!(axis.step(0, 0.1, 0.01, true).is_none());
        assert!(axis.active);
        let end = axis.step(0, 0.0, 1.0, true).unwrap();
        assert_eq!(
            ScrollEvent::from_event(&end).unwrap().phase,
            ScrollPhase::End
        );
        assert!(!axis.active);
    }

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
        with("{}")
    }

    /// A pad translating under the layout `json` describes.
    fn with(json: &str) -> PadMouse {
        let mut m = PadMouse::default();
        let layout = Arc::new(Layout::parse(json).expect("layout parses"));
        m.enter(0, GamepadSnapshot::default(), layout);
        m
    }

    /// `n` translator ticks of [`TICK`], the cadence the input task drives.
    fn ticks(m: &mut PadMouse, n: usize) -> Vec<InputEvent> {
        (0..n)
            .flat_map(|_| m.tick(TICK.as_secs_f64(), 1080, GRANT_ALL))
            .collect()
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
            Arc::new(Layout::default()),
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
        assert_eq!(curve(6000, 0, DEADZONE), (0.0, 0.0));
        let (half, _) = curve(19660, 0, DEADZONE);
        assert!(
            (half - 0.25).abs() < 0.01,
            "60% travel → (0.4/0.8)² = 0.25, got {half}"
        );
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 3000), GRANT_ALL);
        assert!(!m.ticking());
        assert!(m.tick(1.0, 1080, GRANT_ALL).is_empty());
    }

    #[test]
    fn right_stick_scrolls_both_axes() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        m.fold(0, &axis(AXIS_RS_X, -32767), GRANT_ALL);
        assert!(m.ticking());
        let evs = m.tick(0.01, 1000, GRANT_ALL);
        assert_eq!(evs.len(), 2);
        // 0.3 heights/s on the fixed 1080 DIP reference → 829.44 Q24.8 per 10 ms at full
        // travel; the diagonal is radial, so each axis carries 1/√2 of it.
        for (i, axis) in [0u32, 1].iter().enumerate() {
            let se = ScrollEvent::from_event(&evs[i]).unwrap();
            assert_eq!(
                (se.source, se.phase, se.axis),
                (ScrollSource::Controller, ScrollPhase::Begin, *axis)
            );
        }
        assert_eq!(evs[0].x, 586, "up is positive");
        assert_eq!(evs[1].x, -586, "left is negative");
    }

    #[test]
    fn scroll_distance_ignores_stream_height() {
        for height in [720u32, 1080, 2160] {
            let mut m = entered();
            m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
            let evs = m.tick(0.01, height, GRANT_ALL);
            let se = ScrollEvent::from_event(&evs[0]).unwrap();
            assert_eq!(se.delta, 829, "height {height}");
        }
        // Pointer motion still scales with the negotiated height.
        for (height, want) in [(720u32, -9i32), (2160, -27)] {
            let mut m = entered();
            m.fold(0, &axis(AXIS_LS_Y, 32767), GRANT_ALL);
            let dy: i32 = m.tick(0.01, height, GRANT_ALL).iter().map(|e| e.y).sum();
            assert_eq!(dy, want, "height {height}");
        }
    }

    #[test]
    fn scroll_gesture_runs_begin_update_end() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        let first = m.tick(0.01, 1080, GRANT_ALL);
        let se = ScrollEvent::from_event(&first[0]).unwrap();
        assert_eq!(
            (se.source, se.phase, se.axis),
            (ScrollSource::Controller, ScrollPhase::Begin, 0)
        );
        let next = m.tick(0.01, 1080, GRANT_ALL);
        assert_eq!(
            ScrollEvent::from_event(&next[0]).unwrap().phase,
            ScrollPhase::Update
        );
        m.fold(0, &axis(AXIS_RS_Y, 0), GRANT_ALL);
        assert!(m.ticking(), "the open axis still owes an End");
        let end = m.tick(0.01, 1080, GRANT_ALL);
        assert_eq!(end.len(), 1);
        let se = ScrollEvent::from_event(&end[0]).unwrap();
        assert_eq!((se.phase, se.delta), (ScrollPhase::End, 0));
        assert!(!m.ticking());
    }

    #[test]
    fn leave_and_grant_loss_cancel_open_scroll_axes() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        m.tick(0.01, 1080, GRANT_ALL);
        let evs = m.leave(0);
        let se = ScrollEvent::from_event(&evs[0]).unwrap();
        assert_eq!((se.phase, se.axis, se.delta), (ScrollPhase::Cancel, 0, 0));

        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        m.tick(0.01, 1080, GRANT_ALL);
        let evs = m.tick(0.01, 1080, GRANT_KEYBOARD);
        assert_eq!(evs.len(), 1);
        let se = ScrollEvent::from_event(&evs[0]).unwrap();
        assert_eq!((se.phase, se.axis, se.delta), (ScrollPhase::Cancel, 0, 0));
        assert!(m.tick(0.01, 1080, GRANT_KEYBOARD).is_empty());
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
        m.enter(3, GamepadSnapshot::default(), Arc::new(Layout::default()));
        assert_eq!(m.on_mask(), 0b1000);
    }

    /// The table as it shipped. A default layout must still press exactly this, so every other
    /// test in this file keeps measuring what it measured before the table became data.
    #[test]
    fn the_default_layout_is_the_shipped_const() {
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
        let l = Layout::default();
        assert_eq!(l.buttons, DEFAULT_BUTTONS.to_vec());
        assert_eq!(
            Layout::parse("{}").unwrap().buttons,
            DEFAULT_BUTTONS.to_vec()
        );
        assert_eq!(
            (l.pointer, l.scroll, l.deadzone, l.long_press_ms),
            (1.0, 1.0, DEADZONE, LONG_PRESS_MS)
        );
        assert!(l.chords.is_empty());
    }

    /// `RB+B`: a tap closes, a hold kills, and B never sends Escape either way.
    #[test]
    fn short_and_long_share_one_chord() {
        const DOC: &str = r#"{"chords":[
            {"name":"Close window","buttons":["RB","B"],"press":"short","keys":["Alt","F4"]},
            {"name":"Force quit","buttons":["RB","B"],"press":"long","keys":["Meta","Shift","C"]}
        ]}"#;
        let mut m = with(DOC);
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_RB, true), GRANT_ALL)),
            [(InputKind::KeyDown, 0x12)],
            "RB alone is still plain Alt"
        );
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::KeyUp, 0x12)],
            "the chord borrows both: Alt lets go, no Escape"
        );
        assert!(ticks(&mut m, 50).is_empty(), "200 ms fires nothing");
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, false), GRANT_ALL)),
            [
                (InputKind::KeyDown, 0x12),
                (InputKind::KeyDown, 0x73),
                (InputKind::KeyUp, 0x73),
                (InputKind::KeyUp, 0x12),
                (InputKind::KeyDown, 0x12),
            ],
            "Alt+F4 down in order, up in reverse, then RB's own Alt resumes"
        );

        let mut m = with(DOC);
        m.fold(0, &button(BTN_RB, true), GRANT_ALL);
        m.fold(0, &button(BTN_B, true), GRANT_ALL);
        assert!(ticks(&mut m, 99).is_empty(), "396 ms is still short");
        assert_eq!(
            kinds(&ticks(&mut m, 2)),
            [
                (InputKind::KeyDown, 0x5B),
                (InputKind::KeyDown, 0x10),
                (InputKind::KeyDown, 0x43),
                (InputKind::KeyUp, 0x43),
                (InputKind::KeyUp, 0x10),
                (InputKind::KeyUp, 0x5B),
            ],
            "Meta+Shift+C once, at the threshold"
        );
        assert!(ticks(&mut m, 200).is_empty(), "and only once");
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, false), GRANT_ALL)),
            [(InputKind::KeyDown, 0x12)],
            "the short chord is too late to fire, and B still sends no Escape"
        );
    }

    /// `LB` held is Meta; tapped it is the Ctrl the plain table gives it.
    #[test]
    fn a_hold_chord_lets_a_short_tap_through() {
        const DOC: &str = r#"{"chords":[{"buttons":["LB"],"press":"hold","keys":["Meta"]}]}"#;
        let mut m = with(DOC);
        assert!(
            m.fold(0, &button(BTN_LB, true), GRANT_ALL).is_empty(),
            "the chord borrows LB, so no Ctrl yet"
        );
        assert!(m.ticking(), "an armed chord keeps the clock running");
        assert!(ticks(&mut m, 99).is_empty());
        assert_eq!(kinds(&ticks(&mut m, 2)), [(InputKind::KeyDown, 0x5B)]);
        assert!(!m.ticking(), "a settled chord stops it again");
        assert_eq!(
            kinds(&m.leave(0)),
            [(InputKind::KeyUp, 0x5B)],
            "leaving releases what a hold holds"
        );

        let mut m = with(DOC);
        m.fold(0, &button(BTN_LB, true), GRANT_ALL);
        ticks(&mut m, 10);
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_LB, false), GRANT_ALL)),
            [(InputKind::KeyDown, 0x11), (InputKind::KeyUp, 0x11)],
            "released early, LB hands its plain Ctrl back as a tap"
        );
    }

    /// `any` needs no clock, and a chord holding a trigger rides the same hysteresis a plain
    /// output does.
    #[test]
    fn an_any_chord_fires_on_the_press() {
        let mut m =
            with(r#"{"chords":[{"buttons":["LT","B"],"press":"any","keys":["ctrl","c"]}]}"#);
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 255), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [
                (InputKind::MouseButtonUp, MOUSE_RIGHT),
                (InputKind::KeyDown, 0x11),
                (InputKind::KeyDown, 0x43),
                (InputKind::KeyUp, 0x43),
                (InputKind::KeyUp, 0x11),
            ]
        );
        assert!(!m.ticking(), "nothing to count");
        assert!(
            m.fold(0, &axis(AXIS_LT, 70), GRANT_ALL).is_empty(),
            "above TRIGGER_OFF the chord still holds"
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 10), GRANT_ALL)),
            [(InputKind::KeyDown, 0x1B)],
            "the chord is over and B, still down, is a plain Escape again"
        );
    }

    #[test]
    fn an_edited_button_lands_on_the_next_entry() {
        let shared = PadMouseShared::default();
        let mut m = PadMouse::default();
        m.enter(0, GamepadSnapshot::default(), shared.layout());
        shared.set_layout(Layout::parse(r#"{"buttons":{"B":"mouse:right"}}"#).unwrap());
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::KeyDown, 0x1B)],
            "a translating pad keeps the layout it entered with"
        );
        m.leave(0);
        m.enter(0, GamepadSnapshot::default(), shared.layout());
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
        assert!(
            m.fold(0, &button(BTN_A, true), GRANT_ALL).is_empty(),
            "a buttons table replaces the shipped one whole"
        );
        shared.set_layout(Layout::default());
        m.leave(0);
        m.enter(0, GamepadSnapshot::default(), shared.layout());
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::KeyDown, 0x1B)],
            "a reset restores the shipped table"
        );
    }

    #[test]
    fn a_layout_document_maps_names_and_refuses_what_it_cannot() {
        let l = Layout::parse(
            r#"{"settings":{"pointer":2.0,"scroll":0.5,"deadzone":0.1,"long_press_ms":250},
                "buttons":{"A":"mouse:left","RT":"mouse:left","B":"key:Escape"},
                "chords":[{"name":"Copy","buttons":["RT","LS"],"press":"any","keys":["ctrl","c"]}]}"#,
        )
        .expect("parses");
        assert_eq!(
            l.buttons.len(),
            2,
            "two sources on left click share one entry"
        );
        assert_eq!(l.buttons[0], (BTN_A | TRIGGER_R, Out::Mouse(MOUSE_LEFT)));
        assert_eq!((l.pointer, l.scroll, l.long_press_ms), (2.0, 0.5, 250.0));
        assert_eq!(l.chords[0].buttons, TRIGGER_R | BTN_LS_CLICK);
        assert_eq!(l.chords[0].keys, [0x11, 0x43]);
        let wild = Layout::parse(r#"{"settings":{"deadzone":9.0,"pointer":0.0}}"#).unwrap();
        assert_eq!(
            (wild.deadzone, wild.pointer),
            (0.9, 0.05),
            "clamped, not refused"
        );
        for bad in [
            r#"{"buttons":{"Z":"key:a"}}"#,
            r#"{"buttons":{"A":"wheel:up"}}"#,
            r#"{"buttons":{"A":"key:hyper"}}"#,
            r#"{"chords":[{"buttons":["A"],"press":"tap","keys":["a"]}]}"#,
            r#"{"chords":[{"buttons":["A"],"press":"short","keys":[]}]}"#,
            r#"{"settings":{"pointer":1e999}}"#,
            "not json",
        ] {
            assert!(Layout::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_faster_pointer_is_the_multiplier_on_the_shipped_speed() {
        let mut m = with(r#"{"settings":{"pointer":2.0,"deadzone":0.0}}"#);
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        let dx: i32 = m.tick(0.04, 1080, GRANT_ALL).iter().map(|e| e.x).sum();
        assert_eq!(dx, 108, "40 ms at twice 1350 px/s");
        let mut slow = with(r#"{"settings":{"scroll":0.5}}"#);
        slow.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        // Scroll prices against the fixed 1080-DIP reference, not the stream
        // height: half of the full-travel 829 Q24.8 units.
        let vy: i32 = slow.tick(0.01, 1000, GRANT_ALL).iter().map(|e| e.x).sum();
        assert_eq!(vy, 414, "half of 828 Q24.8 units");
    }
}
