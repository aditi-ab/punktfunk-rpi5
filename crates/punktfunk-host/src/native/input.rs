//! Client→host input thread and the per-pad virtual-gamepad router.
//!
//! `serve_session` spawns [`input_thread`] and feeds it [`ClientInput`]. Pointer and
//! keyboard go through [`InputRoute`]; gamepad frames go through [`Pads`], which fans
//! mixed controller kinds to uinput/UHID (Linux) or XUSB/UMDF (Windows). Rumble and
//! HID-output pump on the same thread.
//!
//! Pin `PUNKTFUNK_RUMBLE_ENVELOPE=0`, `PUNKTFUNK_RUMBLE_TTL_MS`, `PUNKTFUNK_XBOX_BACKEND=xusb`.
//! Evidence: `design/gamescope-multiuser.md`, `design/rumble-envelope-plan.md`,
//! `design/trigger-rumble-plane.md`, `design/pen-tablet-input.md`.

use super::*;

/// Pointer/keyboard injector target, re-pointable without restarting [`input_thread`].
/// Isolated gamescope sessions pin their own [`crate::inject::InjectorService`]; everyone
/// else shares the host-lifetime one (`design/gamescope-multiuser.md`). A `Mutex<Sender>`
/// per event, not a second channel: input is a few kHz and the clone is cheap.
#[derive(Clone)]
pub(crate) struct InputRoute(std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Sender<InputEvent>>>);

impl InputRoute {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<InputEvent>) -> InputRoute {
        InputRoute(std::sync::Arc::new(std::sync::Mutex::new(tx)))
    }

    /// Send error means the injector is gone. Input is lossy — drop it.
    pub(crate) fn send(
        &self,
        ev: InputEvent,
    ) -> Result<(), std::sync::mpsc::SendError<InputEvent>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).send(ev)
    }

    /// Mid-stream compositor switch. Does not restart [`input_thread`].
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set(&self, tx: std::sync::mpsc::Sender<InputEvent>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = tx;
    }
}

/// Incremental wire events (one button/axis per datagram) folded into the full frame the
/// virtual xpad applies. Snapshot clients replace the whole state ([`PadState::set_snapshot`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PadState {
    buttons: u32,
    left_trigger: u8,
    right_trigger: u8,
    ls_x: i16,
    ls_y: i16,
    rs_x: i16,
    rs_y: i16,
}

impl PadState {
    /// `false` = unknown axis id; the event is dropped.
    fn apply(&mut self, ev: &InputEvent) -> bool {
        if ev.kind == InputKind::GamepadButton {
            if ev.x != 0 {
                self.buttons |= ev.code;
            } else {
                self.buttons &= !ev.code;
            }
            return true;
        }
        use punktfunk_core::input::gamepad::*;
        let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let trigger = ev.x.clamp(0, 255) as u8;
        match ev.code {
            AXIS_LS_X => self.ls_x = stick,
            AXIS_LS_Y => self.ls_y = stick,
            AXIS_RS_X => self.rs_x = stick,
            AXIS_RS_Y => self.rs_y = stick,
            AXIS_LT => self.left_trigger = trigger,
            AXIS_RT => self.right_trigger = trigger,
            _ => return false,
        }
        true
    }

    fn set_snapshot(&mut self, s: &punktfunk_core::input::GamepadSnapshot) {
        self.buttons = s.buttons;
        self.left_trigger = s.left_trigger;
        self.right_trigger = s.right_trigger;
        self.ls_x = s.ls_x;
        self.ls_y = s.ls_y;
        self.rs_x = s.rs_x;
        self.rs_y = s.rs_y;
    }

    fn frame(&self, index: usize, active_mask: u16) -> punktfunk_core::input::GamepadFrame {
        punktfunk_core::input::GamepadFrame {
            index: index as i16,
            active_mask,
            buttons: self.buttons,
            left_trigger: self.left_trigger,
            right_trigger: self.right_trigger,
            ls_x: self.ls_x,
            ls_y: self.ls_y,
            rs_x: self.rs_x,
            rs_y: self.rs_y,
        }
    }
}

/// Highest wire pad index (`flags` / snapshot `pad`). The uinput manager caps creation separately.
const MAX_WIRE_PADS: usize = punktfunk_core::input::MAX_PADS;

/// The per-OS virtual-pad backends. Off Linux and Windows a pad is Xbox360 or nothing.
#[cfg(target_os = "linux")]
#[path = "input/linux.rs"]
mod backends;
#[cfg(target_os = "windows")]
#[path = "input/windows.rs"]
mod backends;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod backends {
    #[derive(Default)]
    pub(super) struct PadBackends;
    impl PadBackends {
        pub(super) fn route_handle(
            &mut self,
            _kind: super::GamepadPref,
            _ev: &punktfunk_core::input::GamepadEvent,
            _dev: &Option<std::path::PathBuf>,
        ) -> bool {
            false
        }
        pub(super) fn apply_rich(
            &mut self,
            _kind: super::GamepadPref,
            _rich: punktfunk_core::quic::RichInput,
        ) {
        }
        pub(super) fn sc2_active(&self) -> bool {
            false
        }
        pub(super) fn pump(
            &mut self,
            _rumble: &mut impl FnMut(u16, u16, u16, u16, u16),
            _hidout: &mut impl FnMut(punktfunk_core::quic::HidOutput),
        ) {
        }
        pub(super) fn heartbeat(&mut self) {}
    }
}
use backends::PadBackends;

/// Per-pad virtual-gamepad router. Each index uses the kind declared in
/// [`InputKind::GamepadArrival`]; undeclared pads keep the Hello session default.
///
/// Managers are created lazily and own only the indices routed to them. An index
/// another manager owns is `None` here, so an `active_mask` unplug sweep never
/// tears down another kind's device.
///
/// [`resolve_pad_kind`] folds any kind the build cannot construct into one it can.
struct Pads {
    /// Wire index → host-wide OS slot ([`crate::inject::pad_pool`]).
    /// Every client numbers its first pad 0; using the wire index as the OS
    /// identity (mailbox, instance id, pairing MAC) would collide two sessions.
    /// Claimed on first present frame, released on unplug.
    slots: crate::inject::pad_pool::PadSlotMap<'static>,
    /// Slot mask as last reported ([`Pads::take_slot_change`]). The OS slot is the
    /// player number, so the console and the client are told when it moves — once
    /// per change, not once per frame.
    published: u16,
    /// One warn per session when OS slots are exhausted — not one per frame.
    slots_exhausted_warned: bool,
    /// Wire pads whose device is on its unplug grace. The pool slot follows the device out
    /// (`reap_pending`), not the removal frame: released early, a second claimant lands on the
    /// lingering mailbox and blocks 200 ms on a false "owned elsewhere".
    pending_release: [Option<std::time::Instant>; MAX_WIRE_PADS],
    /// Resolved kind per pad; session default until a `GamepadArrival`.
    kinds: [GamepadPref; MAX_WIRE_PADS],
    /// What the client asked for, before [`resolve_pad_kind`] folded it. `None` until
    /// this pad declares. Reported by the Controllers feed, never used for routing.
    declared: [Option<GamepadPref>; MAX_WIRE_PADS],
    /// Manager that holds a built device at this index (`None` = none). Stays put
    /// if `kinds[idx]` later changes (arrival-after-first-frame), so a pad is
    /// never duplicated and removal always hits the manager that owns it.
    owner: [Option<GamepadPref>; MAX_WIRE_PADS],
    xbox360: Option<crate::inject::gamepad::GamepadManager>,
    /// Every other identity, per OS.
    backends: PadBackends,
    /// Where this session's pads are exposed for its seat's Steam to open them
    /// (`pf_vdisplay::seat_device_dir`). `None` leaves them where every process of the user
    /// sees them, which is every host without the seat filter.
    seat_dev: Option<std::path::PathBuf>,
}

impl Pads {
    /// Every pad starts on the session kind ([`resolve_gamepad`]) until it declares otherwise.
    /// `id` names the device and the player slot it asked for, so its pads land on the
    /// same OS slots they had last connect ([`crate::inject::pad_pool::PadIdentity`]).
    fn new(
        default: GamepadPref,
        id: crate::inject::pad_pool::PadIdentity,
        seat_dev: Option<std::path::PathBuf>,
    ) -> Pads {
        let default = resolve_pad_kind(default);
        tracing::info!(
            default = default.as_str(),
            preferred_slot = ?id.preferred,
            "gamepad backends: per-pad router (session default)"
        );
        Pads {
            slots: crate::inject::pad_pool::PadSlotMap::new(id),
            published: 0,
            slots_exhausted_warned: false,
            pending_release: [None; MAX_WIRE_PADS],
            kinds: [default; MAX_WIRE_PADS],
            declared: [None; MAX_WIRE_PADS],
            owner: [None; MAX_WIRE_PADS],
            xbox360: None,
            backends: {
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                {
                    PadBackends::default()
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    PadBackends
                }
            },
            seat_dev,
        }
    }

    /// Record a declared kind (resolved to a buildable backend). Takes effect on
    /// the next frame. A device already built keeps its owner until re-plug —
    /// no live swap, even if arrival lands after the first frame. The unresolved
    /// kind is kept beside it: the Controllers page names both.
    fn set_kind(&mut self, idx: usize, kind: GamepadPref) {
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let resolved = resolve_pad_kind(kind);
        if self.kinds[idx] != resolved {
            tracing::info!(
                pad = idx,
                kind = resolved.as_str(),
                "gamepad kind declared (per-pad)"
            );
        }
        self.kinds[idx] = resolved;
        self.declared[idx] = Some(kind);
    }

    /// This pad as the Controllers feed reports it: the device the host built, the
    /// kind the client asked for, and the state this thread just applied.
    fn feed_frame(&self, idx: usize, state: &PadState, mask: u16) -> crate::pad_feed::PadFrame {
        crate::pad_feed::PadFrame {
            pad: idx as u8,
            ts_ms: crate::pad_feed::now_ms(),
            device: self.kinds[idx].as_str().to_string(),
            declared: self.declared[idx].map(|k| k.as_str().to_string()),
            slot: self.slots.slot_of(idx),
            present: mask & (1 << idx) != 0,
            buttons: state.buttons,
            left_trigger: state.left_trigger,
            right_trigger: state.right_trigger,
            ls_x: state.ls_x,
            ls_y: state.ls_y,
            rs_x: state.rs_x,
            rs_y: state.rs_y,
        }
    }

    fn handle(&mut self, ev: &punktfunk_core::input::GamepadEvent) {
        use punktfunk_core::input::GamepadEvent;
        // Present = mask bit set (create/update). Cleared bit is the `GamepadRemove` frame.
        let (idx, present) = match ev {
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                (idx, f.active_mask & (1 << idx) != 0)
            }
            GamepadEvent::Arrival { index, .. } => (*index as usize, true),
        };
        if idx >= MAX_WIRE_PADS {
            return;
        }
        // The only wire→OS slot translation. Claim on present; a removal must not
        // mint a slot for a pad that is going away.
        let slot = if present {
            self.slots.claim_for(idx)
        } else {
            self.slots.slot_of(idx)
        };
        let Some(slot) = slot else {
            if present && !self.slots_exhausted_warned {
                self.slots_exhausted_warned = true;
                tracing::warn!(
                    pad = idx,
                    max = MAX_WIRE_PADS,
                    "no host pad slot left — every OS slot is held by a live session, so this pad \
                     gets no device. It appears when a slot frees (another session ending, or one \
                     of its pads unplugging)."
                );
            }
            return;
        };
        let had_device = self.owner[idx].is_some();
        let (kind, new_owner) = route_decision(self.owner[idx], self.kinds[idx], present);
        self.owner[idx] = new_owner;
        self.route_handle(kind, &self.re_index(ev, slot));
        if present {
            self.pending_release[idx] = None;
        } else {
            self.note_removed(idx, had_device, std::time::Instant::now());
        }
    }

    /// `pad_slots::SWEEP_GRACE` plus one backend pump: the device, and its mailbox, are gone
    /// before the slot is offered again.
    const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

    /// A removal frame. A built device lingers its grace, so its slot lingers too: a re-plug
    /// inside it re-mints the same slot ([`PadSlotMap::claim_for`] memoizes) and the sweep hands
    /// back the live pad. A pad with no device has nothing to wait for.
    fn note_removed(&mut self, idx: usize, had_device: bool, now: std::time::Instant) {
        if had_device {
            self.pending_release[idx] = Some(now);
        } else {
            self.pending_release[idx] = None;
            self.slots.release(idx);
        }
    }

    fn reap_pending(&mut self) {
        self.reap_pending_at(std::time::Instant::now());
    }

    fn reap_pending_at(&mut self, now: std::time::Instant) {
        for idx in 0..MAX_WIRE_PADS {
            if self.pending_release[idx]
                .is_some_and(|t| now.duration_since(t) >= Self::RELEASE_GRACE)
            {
                self.pending_release[idx] = None;
                self.slots.release(idx);
            }
        }
    }

    /// A wire pad that declared itself, and so reserved a slot, but never sent a frame has no
    /// device to linger: its removal releases the slot at once.
    fn release_unbuilt(&mut self, idx: usize) {
        if idx < MAX_WIRE_PADS && self.owner[idx].is_none() {
            self.pending_release[idx] = None;
            self.slots.release(idx);
        }
    }

    /// Rewrites wire numbering into the host-wide slot the backends create under.
    /// `active_mask` is translated too: a wire-space mask would unplug another
    /// session's pad, or spare one this session had dropped.
    fn re_index(
        &self,
        ev: &punktfunk_core::input::GamepadEvent,
        slot: u8,
    ) -> punktfunk_core::input::GamepadEvent {
        use punktfunk_core::input::GamepadEvent;
        match ev {
            GamepadEvent::State(f) => {
                let mut f = *f;
                f.index = i16::from(slot);
                f.active_mask = self.slots.os_mask(f.active_mask);
                GamepadEvent::State(f)
            }
            GamepadEvent::Arrival {
                kind,
                capabilities,
                audio_caps,
                ..
            } => GamepadEvent::Arrival {
                index: slot,
                kind: *kind,
                capabilities: *capabilities,
                audio_caps: *audio_caps,
            },
        }
    }

    /// Reserve this wire pad's OS slot without building a device — the identity every
    /// per-pad host resource is named by ([`crate::inject::pad_pool`]), which a pad-audio
    /// streamer needs before the pad's first frame. Idempotent: the frame that follows
    /// claims the same slot. `None` once every slot on the host is held.
    fn claim_os_slot(&mut self, wire: usize) -> Option<u8> {
        self.slots.claim_for(wire)
    }

    /// The OS slots this session holds, when they have moved since the last call.
    /// Bit `n` = slot `n` = player `n + 1`.
    fn take_slot_change(&mut self) -> Option<u16> {
        let now = self.slots.held_mask();
        (now != self.published).then(|| {
            self.published = now;
            now
        })
    }

    /// [`Self::re_index`] for the rich plane (touchpad, motion, raw HID reports).
    /// `None` when this wire pad holds no slot: rich never creates a device.
    ///
    /// A client numbers its first pad 0 and the slot is claimed on that pad's first
    /// frame, so the pad that moves first takes slot 0 whatever its wire index. Skip
    /// this and two pads swap devices — every SC2 raw report, and the rumble Steam
    /// answers it with, lands on the other player's controller.
    fn rich_in_slot_space(
        &self,
        mut rich: punktfunk_core::quic::RichInput,
    ) -> Option<punktfunk_core::quic::RichInput> {
        rich.set_pad(self.slots.slot_of(rich.pad() as usize)?);
        Some(rich)
    }

    fn route_handle(&mut self, kind: GamepadPref, ev: &punktfunk_core::input::GamepadEvent) {
        if !self.backends.route_handle(kind, ev, &self.seat_dev) {
            let dev = &self.seat_dev;
            self.xbox360
                .get_or_insert_with(|| {
                    let mut m = crate::inject::gamepad::GamepadManager::new();
                    m.expose_in(dev.clone());
                    m
                })
                .handle(ev);
        }
    }

    /// Touchpad / motion for the pad's manager. No device yet → no-op. Xbox has no rich plane.
    fn apply_rich(&mut self, rich: punktfunk_core::quic::RichInput) {
        let idx = rich.pad() as usize;
        // Same wire→OS-slot rewrite `re_index` does for events: the managers index their
        // slot table in OS space. No slot = no device for this pad, and rich never
        // creates one.
        let Some(rich) = self.rich_in_slot_space(rich) else {
            return;
        };
        // Owner, else declared kind (pre-first-frame). After a kind change, rich must not
        // land on the wrong backend.
        let kind = self
            .owner
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| self.kinds.get(idx).copied())
            .unwrap_or(GamepadPref::Xbox360);
        self.backends.apply_rich(kind, rich);
    }

    /// Triton USB OUT is 1 kHz; poll haptics at 1 ms so trackpad pulses do not sit 4 ms
    /// then burst. Other backends stay at 4 ms to avoid idle churn.
    fn feedback_poll_interval(&self) -> std::time::Duration {
        if self.backends.sc2_active() {
            return std::time::Duration::from_millis(1);
        }
        std::time::Duration::from_millis(4)
    }

    /// Pump every live backend. `rumble` is `(pad, low, high, lt, rt)` on 0xCA;
    /// `hidout` is lightbar / LEDs / adaptive triggers on UHID/UMDF. Closures
    /// re-borrow to satisfy `FnMut`.
    ///
    /// Only the Windows HID Xbox managers ever report non-zero trigger levels;
    /// every other backend's source packet has no field for them, so v3 goes
    /// out as v2 plus a zero tail (`PadFeedback::rumble`).
    fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16, u16, u16),
        mut hidout: impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        self.reap_pending();
        // Reverse of `re_index`: backends tag OS slots; the client only knows its wire
        // index. Miss this and rumble lands on another session's pad.
        // Snapshot first: the callbacks borrow `&mut self.<manager>`, so they
        // cannot also borrow `self.slots`.
        let mut wire_of = [None; MAX_WIRE_PADS];
        for (slot, wire) in wire_of.iter_mut().enumerate() {
            *wire = self.slots.wire_of(slot as u8);
        }
        // No wire index = not this session's pad; drop the feedback.
        let mut rumble = |pad: u16, low, high, lt, rt| {
            if let Some(wire) = wire_of.get(pad as usize).copied().flatten() {
                rumble(wire as u16, low, high, lt, rt);
            }
        };
        let mut hidout = |h: punktfunk_core::quic::HidOutput| {
            if let Some(wire) = wire_of.get(h.pad() as usize).copied().flatten() {
                hidout(h.with_pad(wire as u16));
            }
        };
        if let Some(m) = &mut self.xbox360 {
            m.pump_rumble(&mut rumble); // Xbox has no rich-feedback plane
        }
        self.backends.pump(&mut rumble, &mut hidout);
    }

    /// Re-emit HID reports so kernel/SDL do not drop a held-steady UHID/UMDF pad.
    /// Xbox evdev holds last-known state — no heartbeat. Cadence is each manager's
    /// gap timer, not this per-tick call.
    fn heartbeat(&mut self) {
        self.backends.heartbeat();
    }
}

/// Per-pad 0xD1 streamers (`super::pad_audio`). `spawn` refuses pads past 0..4.
/// Opened when a DualSense-family arrival declares renderer bits; reaped on
/// remove / re-declare / teardown.
struct PadAudioSlots {
    /// `(kinds, handle)` per running pad. `kinds` makes an identical re-arrival
    /// (resent against datagram loss) a no-op.
    slots: [Option<(u8, pad_audio::PadAudioHandle)>; MAX_WIRE_PADS],
    /// Streamer starts inside the current window, first one included. Arrivals are client-
    /// triggered; without a ceiling the client decides how many WASAPI captures open.
    starts: [u8; MAX_WIRE_PADS],
    /// When the window's first start happened; `None` before any.
    window_start: [Option<std::time::Instant>; MAX_WIRE_PADS],
}

/// Captures one pad may open inside [`START_WINDOW`]. A real pad declares once and identical
/// re-arrivals no-op; a flaky link re-plugs a few times an hour, a client cycling kinds or
/// declare/stop would open WASAPI captures forever.
const MAX_PAD_AUDIO_STARTS: u8 = 8;
const START_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl PadAudioSlots {
    fn new() -> PadAudioSlots {
        PadAudioSlots {
            slots: std::array::from_fn(|_| None),
            starts: [0; MAX_WIRE_PADS],
            window_start: [None; MAX_WIRE_PADS],
        }
    }

    /// Same kinds → keep; changed → restart; idle → spawn. A slot with no endpoint
    /// stays empty (arrivals retry a few times). Only an actual open spends
    /// [`MAX_PAD_AUDIO_STARTS`]. `edge` selects DualSense Edge on Linux; Windows
    /// endpoints are pre-stamped so it is ignored there.
    ///
    /// Two index spaces: this table and the client's datagrams use `pad`, while the
    /// endpoint / card / sink a streamer captures is named by the pad's OS `slot`.
    fn ensure(
        &mut self,
        conn: &super::link::SessionLink,
        pad: u8,
        slot: u8,
        kinds: u8,
        edge: bool,
    ) {
        let idx = pad as usize;
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let running = self.slots[idx].as_ref().map(|(have, _)| *have);
        if running == Some(kinds) {
            return;
        }
        // Client-driven: cycling kinds or declare/stop would spawn WASAPI captures
        // forever. Gate before `stop` so a pad at the ceiling keeps the streamer
        // it has instead of losing it to the last request.
        if self.window_start[idx].is_some_and(|t| t.elapsed() >= START_WINDOW) {
            self.starts[idx] = 0;
            self.window_start[idx] = None;
        }
        if self.starts[idx] >= MAX_PAD_AUDIO_STARTS {
            tracing::warn!(
                pad = idx,
                "pad-audio streamer already started {MAX_PAD_AUDIO_STARTS} times — ignoring; the \
                 pad keeps whatever streamer it has for this session"
            );
            return;
        }
        if running.is_some() {
            tracing::info!(
                pad = idx,
                starts = self.starts[idx],
                "pad-audio kinds changed — restarting the streamer"
            );
            self.stop(idx);
        }
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(h) = pad_audio::spawn(conn.clone(), pad, slot, kinds, edge, stop) {
            // Charge only an open that happened. A slot with no endpoint must not
            // spend the ceiling on arrival re-sends.
            self.starts[idx] += 1;
            self.window_start[idx].get_or_insert_with(std::time::Instant::now);
            self.slots[idx] = Some((kinds, h));
        }
    }

    /// Signal and reap on a detached thread. A quiet capturer can sit ~5 s in recv;
    /// this thread must keep ≤4 ms (games block on GET_REPORT). Failed spawn
    /// falls back to the handle's drop (signal + join).
    fn stop(&mut self, idx: usize) {
        if let Some((_, h)) = self.slots.get_mut(idx).and_then(|s| s.take()) {
            h.signal();
            let _ = std::thread::Builder::new()
                .name("punktfunk1-padreap".into())
                .spawn(move || h.stop());
        }
    }

    /// Signal every streamer first so they wind down concurrently, then join.
    /// Worst case is one ~5 s quiet-endpoint timeout, inside the session's 10 s
    /// side-thread grace — not one timeout per pad.
    fn stop_all(&mut self) {
        for s in self.slots.iter().flatten() {
            s.1.signal();
        }
        for s in &mut self.slots {
            if let Some((_, h)) = s.take() {
                h.stop();
            }
        }
    }
}

/// Both input planes on one channel so the thread wakes on either. A second
/// rich channel drained after the 4 ms recv timeout quantized every gyro sample.
pub(super) enum ClientInput {
    /// 0xC8: pointer / keyboard / gamepad button+axis.
    Event(InputEvent),
    /// 0xCC: touchpad contacts + motion samples.
    Rich(punktfunk_core::quic::RichInput),
    /// 0xCC/0x05 stylus batches, diffed into a per-session virtual tablet
    /// (`design/pen-tablet-input.md`).
    Pen(punktfunk_core::quic::PenBatch),
}

/// Per-session stylus: [`PenTracker`](punktfunk_core::quic::PenTracker) diffs
/// batches into transitions on a lazily-created [`crate::inject::pen::VirtualPen`].
/// No ink → no device; the tablet dies with the session.
struct PenSession {
    tracker: punktfunk_core::quic::PenTracker,
    dev: Option<crate::inject::pen::VirtualPen>,
    /// Create failed once — do not retry at 240 Hz. The tracker still consumes
    /// batches so its state stays coherent.
    create_failed: bool,
    last_rx: std::time::Instant,
    /// Reused transition buffer (a batch yields a few).
    out: Vec<punktfunk_core::quic::PenTransition>,
}

impl PenSession {
    fn new() -> PenSession {
        PenSession {
            tracker: punktfunk_core::quic::PenTracker::default(),
            dev: None,
            create_failed: false,
            last_rx: std::time::Instant::now(),
            out: Vec::new(),
        }
    }

    fn apply(&mut self, batch: &punktfunk_core::quic::PenBatch) {
        self.last_rx = std::time::Instant::now();
        if self.dev.is_none() && !self.create_failed {
            match crate::inject::pen::VirtualPen::create() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    // Welcome advertised HOST_CAP_PEN from the same probe; permissions
                    // can still change between then and first ink.
                    self.create_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "pen: virtual tablet creation failed — dropping pen input this session"
                    );
                }
            }
        }
        self.out.clear();
        self.tracker.apply(batch, &mut self.out);
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }

    /// Dead-client failsafe ([`PEN_TOUCH_TIMEOUT_MS`](punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS)).
    /// Clients repeat the last sample (≤100 ms) while in range, including a
    /// stationary touch, so silence means gone — do not leave the stroke down.
    /// The input loop caps recv at 100 ms while the pen is active so this runs.
    fn check_timeout(&mut self) {
        if self.tracker.is_active()
            && self.last_rx.elapsed().as_millis()
                >= punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS as u128
        {
            tracing::debug!("pen: sample stream went silent — force-releasing the stroke");
            self.release_all();
        }
    }

    /// Lift buttons, then tip, then proximity. Session end and the timeout.
    fn release_all(&mut self) {
        self.out.clear();
        self.tracker.force_release(&mut self.out);
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }

    fn active(&self) -> bool {
        self.tracker.is_active()
    }
}

/// Default 0xCA envelope TTL: the client silences unless renewed. Covers 2–3
/// lost renewals and caps an abandoned rumble on every client. Override with
/// `PUNKTFUNK_RUMBLE_TTL_MS`, floored at [`RUMBLE_TTL_FLOOR_MS`].
const RUMBLE_TTL_MS: u16 = 400;
/// `PUNKTFUNK_RUMBLE_TTL_MS` floor. Below this, ~50 ms client ticks make expiry audible
/// (`design/rumble-envelope-plan.md`).
const RUMBLE_TTL_FLOOR_MS: u16 = 150;
/// `PUNKTFUNK_RUMBLE_TTL_MS` ceiling. A multi-second lease is no longer prompt,
/// and staying well under `u16::MAX` avoids a client treating TTL as a sentinel.
const RUMBLE_TTL_CEIL_MS: u16 = 5_000;
/// Floor for renew = ttl × 3/10, so an aggressive TTL hatch cannot spin faster.
const RUMBLE_RENEW_FLOOR_MS: u64 = 60;
/// Stop re-sends on later renewal ticks after the immediate zero datagram.
/// Covers stop loss for legacy clients; a v2 client also self-silences at TTL.
/// Immediate send + this many = 3 zeros total.
const RUMBLE_STOP_BURST: u8 = 2;

/// Drop a removed pad's rumble lease (level, seen, stop burst) so a re-plug
/// on the same wire index cannot buzz the new device.
///
/// Do not take or reset `rumble_seq`. The client gates with a wrapping
/// half-space compare and never resets (`client/pump/datagram_task.rs`);
/// resetting here is the bug in [`tests::rumble_seq_survives_a_removal_so_the_client_gate_accepts`].
/// The session encoder's framing of the captured picture. Default maps nothing.
pub(super) type FrameMap = Arc<std::sync::Mutex<punktfunk_core::video_fit::Reframe>>;

/// Moves absolute pointer and touch input from a cropped picture back to the source. A scale
/// alone needs nothing: injectors already normalise by the event's own extent.
fn reframe_input(ev: &mut InputEvent, map: &punktfunk_core::video_fit::Reframe) {
    let absolute = matches!(
        ev.kind,
        InputKind::MouseMoveAbs | InputKind::TouchDown | InputKind::TouchMove
    );
    let extent = (f64::from(ev.flags >> 16), f64::from(ev.flags & 0xffff));
    if !absolute || !map.is_cropped() || extent.0 == 0.0 || extent.1 == 0.0 {
        return;
    }
    let (x, y) = map.to_source(f64::from(ev.x), f64::from(ev.y), extent);
    ev.x = x.round() as i32;
    ev.y = y.round() as i32;
    ev.flags = (map.source.0.min(0xffff) << 16) | map.source.1.min(0xffff);
}

/// [`reframe_input`] for a stylus batch, whose samples are `0..=1` of the picture.
fn reframe_pen(
    batch: &punktfunk_core::quic::PenBatch,
    map: &punktfunk_core::video_fit::Reframe,
) -> punktfunk_core::quic::PenBatch {
    if !map.is_cropped() {
        return *batch;
    }
    let (sw, sh) = (f64::from(map.source.0), f64::from(map.source.1));
    let mut samples = batch.samples().to_vec();
    for s in &mut samples {
        let (x, y) = map.to_source(f64::from(s.x), f64::from(s.y), (1.0, 1.0));
        (s.x, s.y) = ((x / sw) as f32, (y / sh) as f32);
    }
    punktfunk_core::quic::PenBatch::new(batch.seq, &samples)
}

fn clear_pad_feedback(state: &mut RumbleLevels, seen: &mut bool, stop_burst: &mut u8) {
    *state = (0, 0, 0, 0);
    *seen = false;
    *stop_burst = 0;
}

/// `(low, high, left_trigger, right_trigger)`, `0..=0xFFFF`, 0xCA order. One
/// value because they share one `seq` and one TTL on the wire.
type RumbleLevels = (u16, u16, u16, u16);

/// All four motors zero. A `(low, high)`-only test stamps trigger-only rumble
/// (racing titles drive triggers with silent handles) as `ttl = 0`; the client
/// silences on arrival with no error ([`tests::a_trigger_only_rumble_gets_a_live_ttl`]).
fn rumble_silent(lv: RumbleLevels) -> bool {
    lv == (0, 0, 0, 0)
}

/// 0xCA rumble. `envelope_on` selects v3 (default) or v1 (`PUNKTFUNK_RUMBLE_ENVELOPE=0`).
/// Best-effort, like every side-plane datagram.
///
/// v3 is unconditional while the envelope is on — not "only if a trigger is
/// non-zero". A history-dependent wire form is a sequence bug; pre-v3 clients
/// read the 10-byte prefix and ignore the tail.
///
/// The v1 hatch has no trigger tail. "Trigger rumble stopped" is an expected
/// symptom of the hatch — do not bisect a trigger bug into it.
fn send_rumble(
    conn: &super::link::SessionLink,
    envelope_on: bool,
    pad: u16,
    lv: RumbleLevels,
    seq: u8,
    ttl_ms: u16,
) {
    let (low, high, lt, rt) = lv;
    let d: Vec<u8> = if envelope_on {
        punktfunk_core::quic::encode_rumble_datagram_v3(pad, low, high, seq, ttl_ms, lt, rt)
            .to_vec()
    } else {
        punktfunk_core::quic::encode_rumble_datagram(pad, low, high).to_vec()
    };
    conn.send_datagram(d);
}

/// Per-session input thread. Pointer/keyboard go through [`InputRoute`]; gamepad
/// through [`Pads`] (Hello kind is the per-pad default). Rich input applies on
/// arrival; rumble and HID-output pump between events. Gamepads die with the
/// session; the pointer/keyboard injector (and its portal grant) outlives it.
///
/// Every pad state that reaches [`Pads`] also reaches `pad_feed`, which is what
/// the console's Controllers page draws ([`crate::pad_feed`]).
///
/// Rumble is 0xCA v3 (`[level][seq][ttl_ms][trigger levels]`). The host renews
/// an active level every ~`RUMBLE_TTL_MS × 3/10` and lets an abandoned one
/// expire client-side (`design/rumble-envelope-plan.md`,
/// `design/trigger-rumble-plane.md`). Four motors share one `seq` and one TTL.
/// `PUNKTFUNK_RUMBLE_ENVELOPE=0` reverts to v1 + a flat 500 ms refresh, which
/// drops trigger rumble ([`send_rumble`]).
///
/// Ends on `stop` or when `rx` disconnects. The pads (and their OS slots) go
/// before the streamer join, so a session that preempted this one can claim
/// them inside its 1.5 s grace.
///
/// `pad_id` names the device its pads belong to and the player slot the operator
/// picked for it; `pad_slots` and `pad_slots_tx` publish the slots it ends up with,
/// to `/status` and to the client's overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn input_thread(
    rx: std::sync::mpsc::Receiver<ClientInput>,
    conn: super::link::SessionLink,
    inj_tx: InputRoute,
    gamepad: GamepadPref,
    pad_audio_on: bool,
    pad_id: crate::inject::pad_pool::PadIdentity,
    pad_slots: Arc<std::sync::atomic::AtomicU16>,
    pad_slots_tx: Option<tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::PadSlots>>,
    // Live grant mask. Dispatch already drops non-granted traffic; the guards
    // below are deny-at-setup: without `GRANT_GAMEPAD` no arm that could create
    // a virtual pad or pad-audio streamer runs. One relaxed load per item.
    grants: Arc<AtomicU32>,
    frame_map: FrameMap,
    // This session's Controllers-page tap. Every accepted pad state goes here as well
    // as to the backends, so the page shows what was injected. Idle with nobody watching.
    pad_feed: Arc<crate::pad_feed::PadFeed>,
    // Where this session's pads are exposed so its seat's Steam can open them, and no other
    // seat's can (`pf_vdisplay::seat_device_dir`). `None` is every host without the filter.
    seat_dev: Option<std::path::PathBuf>,
    stop: Arc<AtomicBool>,
    // Session gyro totals. This thread is joined after the summary is built, so the
    // per-pad histogram below cannot be what the summary reads.
    counters: Arc<crate::session_status::SessionCounters>,
) {
    let mut pads = Pads::new(gamepad, pad_id, seat_dev);
    // 0xD1 streamers; `pad_audio_on` is the negotiated Welcome cap.
    let mut pad_streams = PadAudioSlots::new();
    // Per-pad motion cadence, always on. Summarized at `info` on session end.
    let mut motion_cadence = super::motion_cadence::MotionCadence::new();
    let mut pad_uplink = super::pad_uplink::PadUplink::new(std::time::Instant::now());
    let mut pad_state = [PadState::default(); MAX_WIRE_PADS];
    let mut pad_mask = 0u16;
    // Last applied snapshot seq (`None` until first). Older seq must not roll held state back.
    let mut pad_seq: [Option<u8>; MAX_WIRE_PADS] = [None; MAX_WIRE_PADS];
    // 0xCA v3 envelopes. `rumble_seq` wraps per pad and is bumped on changes and
    // renewals; the client gates on it. `PUNKTFUNK_RUMBLE_ENVELOPE=0` is v1 every 500 ms.
    let mut rumble_state = [(0u16, 0u16, 0u16, 0u16); MAX_WIRE_PADS];
    let mut rumble_seen = [false; MAX_WIRE_PADS];
    let mut rumble_seq = [0u8; MAX_WIRE_PADS];
    let mut rumble_stop_burst = [0u8; MAX_WIRE_PADS];
    let mut last_refresh = std::time::Instant::now();
    let rumble_envelope_on = std::env::var("PUNKTFUNK_RUMBLE_ENVELOPE").as_deref() != Ok("0");
    let rumble_ttl_ms: u16 = std::env::var("PUNKTFUNK_RUMBLE_TTL_MS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .map(|v| v.clamp(RUMBLE_TTL_FLOOR_MS, RUMBLE_TTL_CEIL_MS))
        .unwrap_or(RUMBLE_TTL_MS);
    // Renew at 30 % of TTL (≈120 ms at 400) so 2–3 renewals cover the lease.
    // Legacy mode is a flat 500 ms full-state refresh.
    let rumble_refresh_interval = if rumble_envelope_on {
        std::time::Duration::from_millis((rumble_ttl_ms as u64 * 3 / 10).max(RUMBLE_RENEW_FLOOR_MS))
    } else {
        std::time::Duration::from_millis(500)
    };
    // Injector is host-lifetime: a press left dangling stays latched in the
    // compositor (Mutter keeps the implicit grab). Matching ups go out at session
    // end. HashSet, capped at `MAX_HELD`, so a flood of never-released codes
    // cannot grow this thread's state; codes past the cap are not auto-released.
    const MAX_HELD: usize = 256;
    let mut held_buttons: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut held_keys: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut held_touch: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut pen = PenSession::new();
    loop {
        // A reconnect or steal sets `stop` while this connection is still open and
        // claims this session's OS slots 1.5 s later; the channel outlives that.
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // A console just opened the Controllers page. A held button sends no further
        // frames, so re-publish every live pad or the page draws nothing until the
        // next press. One relaxed load per wake when the page is closed.
        if pad_feed.take_resync() {
            for idx in (0..MAX_WIRE_PADS).filter(|i| pad_mask & (1 << i) != 0) {
                pad_feed.publish(|| pads.feed_frame(idx, &pad_state[idx], pad_mask));
            }
        }
        // Pen in range: wake at least every 100 ms so check_timeout can meet its 200 ms deadline.
        let poll = if pen.active() {
            pads.feedback_poll_interval()
                .min(std::time::Duration::from_millis(100))
        } else {
            pads.feedback_poll_interval()
        };
        let arrived = rx.recv_timeout(poll);
        if let Some(w) = pad_uplink.take_window(std::time::Instant::now()) {
            tracing::info!(
                updates = w.updates,
                lost = w.lost,
                max_gap_ms = w.max_gap_ms,
                "controller uplink"
            );
        }
        // Any arrival, before grant tests: a denied event still means a person is
        // here, so drop a standing suspend veto (`sleep_inhibit`).
        if arrived.is_ok() {
            crate::sleep_inhibit::note_input();
        }
        match arrived {
            Ok(ClientInput::Rich(rich))
                if grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_GAMEPAD != 0 =>
            {
                if let punktfunk_core::quic::RichInput::Motion { pad, .. } = rich {
                    counters.note_motion(motion_cadence.record(pad, std::time::Instant::now()));
                }
                pads.apply_rich(rich);
            }
            // Pointer-class (`classify`). The guard is deny-at-setup: a session
            // that never passes it never creates the virtual tablet.
            Ok(ClientInput::Pen(batch))
                if grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_POINTER != 0 =>
            {
                pen.apply(&reframe_pen(
                    &batch,
                    &frame_map.lock().unwrap_or_else(|e| e.into_inner()),
                ))
            }
            // Same classify as dispatch. Resource-creating arms (virtual pads,
            // pad-audio) stay unreachable if an upstream filter regresses.
            Ok(ClientInput::Event(ev))
                if grants.load(Ordering::Relaxed)
                    & punktfunk_core::quic::classify(ev.kind).bit()
                    != 0 =>
            {
                match ev.kind {
                    InputKind::GamepadButton | InputKind::GamepadAxis => {
                        // Bad index / unknown axis: fall through, no `continue`.
                        // The DualSense GET_REPORT handshake still has to run this tick.
                        let idx = ev.flags as usize;
                        if idx < MAX_WIRE_PADS && pad_state[idx].apply(&ev) {
                            pad_mask |= 1 << idx;
                            let frame = pad_state[idx].frame(idx, pad_mask);
                            pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                            pad_feed.publish(|| pads.feed_frame(idx, &pad_state[idx], pad_mask));
                        }
                    }
                    InputKind::GamepadState => {
                        // Snapshot: apply only if seq is newer so a reorder cannot
                        // roll held state back. Unchanged refresh (~100 ms) skips
                        // the frame emit (XInput packet-number churn) but still
                        // advances the gate.
                        use punktfunk_core::input::GamepadSnapshot;
                        if let Some(snap) = GamepadSnapshot::from_event(&ev) {
                            let idx = snap.pad as usize;
                            if let Some(g) =
                                pad_uplink.note(idx, snap.seq, std::time::Instant::now())
                            {
                                tracing::warn!(
                                    pad = g.pad,
                                    silence_ms = g.silence_ms,
                                    lost = g.lost,
                                    "controller updates stopped reaching the host — lost: \
                                     updates the client sent that never arrived. lost near \
                                     silence_ms / 100 = the path dropped them; 0 = the client \
                                     sent none"
                                );
                            }
                            if idx < MAX_WIRE_PADS
                                && GamepadSnapshot::seq_newer(snap.seq, pad_seq[idx])
                            {
                                pad_seq[idx] = Some(snap.seq);
                                let before = pad_state[idx];
                                pad_state[idx].set_snapshot(&snap);
                                let first = pad_mask & (1 << idx) == 0;
                                if first || pad_state[idx] != before {
                                    pad_mask |= 1 << idx;
                                    let frame = pad_state[idx].frame(idx, pad_mask);
                                    pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                                    pad_feed.publish(|| {
                                        pads.feed_frame(idx, &pad_state[idx], pad_mask)
                                    });
                                }
                            }
                        }
                    }
                    InputKind::GamepadRemove => {
                        // Hot-unplug, seq-gated in the same space as snapshots so a
                        // reordered snapshot cannot resurrect the pad and a later
                        // re-plug (newer seq) is accepted. Clearing `active_mask`
                        // and re-emitting fires each backend's unplug sweep.
                        let (pad, seq) = punktfunk_core::input::decode_gamepad_remove(ev.flags);
                        let idx = pad as usize;
                        if idx < MAX_WIRE_PADS
                            && punktfunk_core::input::GamepadSnapshot::seq_newer(seq, pad_seq[idx])
                        {
                            pad_seq[idx] = Some(seq);
                            pad_uplink.forget(idx);
                            if pad_mask & (1 << idx) != 0 {
                                pad_mask &= !(1 << idx);
                                pad_state[idx] = PadState::default();
                                let frame = pad_state[idx].frame(idx, pad_mask);
                                pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                                pad_feed
                                    .publish(|| pads.feed_frame(idx, &pad_state[idx], pad_mask));
                                tracing::info!(pad = idx, "gamepad unplugged (native detach)");
                            } else {
                                pads.release_unbuilt(idx);
                            }
                            // Drop the lease so a re-plug cannot buzz the new pad.
                            // Do not reset `rumble_seq`: the client gate is per-
                            // connection and has no reset (`datagram_task.rs`).
                            // `pad_seq` is kept for the same reason.
                            clear_pad_feedback(
                                &mut rumble_state[idx],
                                &mut rumble_seen[idx],
                                &mut rumble_stop_burst[idx],
                            );
                            // Streamer goes with the pad. Seq-gated so a stale
                            // removal cannot kill a re-plugged pad's stream.
                            pad_streams.stop(idx);
                        }
                    }
                    InputKind::GamepadArrival => {
                        // `code` is GamepadPref. Index is the low byte of `flags`;
                        // bits 8/9 are audio-render caps. Always
                        // `decode_gamepad_arrival` — never the whole word.
                        let (pad, audio_caps) =
                            punktfunk_core::input::decode_gamepad_arrival(ev.flags);
                        let idx = pad as usize;
                        let kind = GamepadPref::from_u8(ev.code as u8);
                        if audio_caps != 0 {
                            tracing::debug!(
                                pad = idx,
                                haptics = audio_caps & 0x01 != 0,
                                speaker = audio_caps & 0x02 != 0,
                                "pad-audio render caps declared (arrival flags bits 8/9)"
                            );
                        }
                        pads.set_kind(idx, kind);
                        // 0xD1: DualSense-family with renderer bits, if negotiated.
                        // Re-declare without bits, or a kind with no pad audio, stops it.
                        if pad_audio_on {
                            let want = if matches!(
                                kind,
                                GamepadPref::DualSense | GamepadPref::DualSenseEdge
                            ) {
                                audio_caps
                            } else {
                                0
                            };
                            // The streamer captures the pad's own endpoint / card / sink,
                            // all named by its OS slot, so reserve that here: the
                            // declaration is what starts the stream, and the first frame
                            // that would otherwise claim the slot may be seconds away.
                            // No slot means no device, so no audio either.
                            let slot = if want != 0 {
                                pads.claim_os_slot(idx)
                            } else {
                                None
                            };
                            match slot {
                                Some(slot) => pad_streams.ensure(
                                    &conn,
                                    pad,
                                    slot,
                                    want,
                                    matches!(kind, GamepadPref::DualSenseEdge),
                                ),
                                None => pad_streams.stop(idx),
                            }
                        }
                    }
                    _ => {
                        // Track press/release so a mid-press disconnect can be undone below.
                        match ev.kind {
                            InputKind::MouseButtonDown if held_buttons.len() < MAX_HELD => {
                                held_buttons.insert(ev.code);
                            }
                            InputKind::MouseButtonUp => {
                                held_buttons.remove(&ev.code);
                            }
                            InputKind::KeyDown if held_keys.len() < MAX_HELD => {
                                held_keys.insert(ev.code);
                            }
                            InputKind::KeyUp => {
                                held_keys.remove(&ev.code);
                            }
                            // A held contact is re-injected every 40 ms on purpose, which
                            // defeats Windows' own staleness lift — so only an Up ends it.
                            InputKind::TouchDown if held_touch.len() < MAX_HELD => {
                                held_touch.insert(ev.code);
                            }
                            InputKind::TouchUp => {
                                held_touch.remove(&ev.code);
                            }
                            _ => {}
                        }
                        let mut ev = ev;
                        reframe_input(
                            &mut ev,
                            &frame_map.lock().unwrap_or_else(|e| e.into_inner()),
                        );
                        // Host-lifetime injector. Send error = service gone; input is lossy.
                        let _ = inj_tx.send(ev);
                    }
                }
            }
            // Grant missed: drop. Dispatch already counted; no second counter.
            // This arm exists so the guarded matches above are exhaustive.
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        pen.check_timeout();
        // Every tick (≤1 ms Triton, ≤4 ms else): games block on EVIOCSFF and HID
        // GET_REPORT. Rumble is 0xCA; rich HID-out is 0xCD.
        pads.pump(
            |pad, low, high, lt, rt| {
                let lv: RumbleLevels = (low, high, lt, rt);
                let silent = rumble_silent(lv);
                let idx = pad as usize;
                if idx < MAX_WIRE_PADS {
                    let prev = rumble_state[idx];
                    // Silent→active once per buzz, with `lt`/`rt`, so "host never
                    // saw trigger rumble" is separable from "client never rendered it".
                    if rumble_silent(prev) && !silent {
                        tracing::debug!(
                            pad,
                            low,
                            high,
                            lt,
                            rt,
                            "rumble: forwarding to client (0xCA)"
                        );
                    }
                    rumble_state[idx] = lv;
                    rumble_seen[idx] = true;
                    // Bump seq on every change. Arm the stop burst on a fall to
                    // zero (lost stop vs legacy client); clear it if the game re-asserts.
                    rumble_seq[idx] = rumble_seq[idx].wrapping_add(1);
                    if silent {
                        rumble_stop_burst[idx] = if !rumble_silent(prev) {
                            RUMBLE_STOP_BURST
                        } else {
                            0
                        };
                    } else {
                        rumble_stop_burst[idx] = 0;
                    }
                    // Any of the four motors → live TTL. See `rumble_silent`.
                    let ttl = if silent { 0 } else { rumble_ttl_ms };
                    send_rumble(&conn, rumble_envelope_on, pad, lv, rumble_seq[idx], ttl);
                } else {
                    // Out-of-range (backends never emit this) — forward ungated.
                    send_rumble(&conn, rumble_envelope_on, pad, lv, 0, rumble_ttl_ms);
                }
            },
            |h| {
                conn.send_datagram(h.encode());
            },
        );
        // Held-steady UHID pads send no wire events; heartbeat re-emits. Xbox is a no-op.
        pads.heartbeat();
        // After `pump` reaped the unplug grace, so a re-plug inside it is not reported
        // as a pad leaving and coming back. Silent while the slots stand.
        if let Some(mask) = pads.take_slot_change() {
            pad_slots.store(mask, std::sync::atomic::Ordering::Relaxed);
            if let Some(tx) = &pad_slots_tx {
                let _ = tx.send(punktfunk_core::quic::PadSlots { slots: mask });
            }
        }
        if last_refresh.elapsed() >= rumble_refresh_interval {
            last_refresh = std::time::Instant::now();
            if rumble_envelope_on {
                // Renew an active lease (bump seq, fresh TTL). Drain the stop burst,
                // then go quiet — no perpetual zero refreshes.
                for i in 0..MAX_WIRE_PADS {
                    if !rumble_seen[i] {
                        continue;
                    }
                    let lv = rumble_state[i];
                    if !rumble_silent(lv) {
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(&conn, true, i as u16, lv, rumble_seq[i], rumble_ttl_ms);
                    } else if rumble_stop_burst[i] > 0 {
                        rumble_stop_burst[i] -= 1;
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(&conn, true, i as u16, (0, 0, 0, 0), rumble_seq[i], 0);
                    }
                }
            } else {
                // Legacy v1: re-send every seen pad every 500 ms. Trigger levels
                // are dropped — v1 has no tail (`send_rumble`).
                for (i, &(low, high, _, _)) in rumble_state.iter().enumerate() {
                    if rumble_seen[i] {
                        let d = punktfunk_core::quic::encode_rumble_datagram(i as u16, low, high);
                        conn.send_datagram(d.to_vec());
                    }
                }
            }
        }
    }
    // Lift remaining ink (buttons → tip → proximity). VirtualPen drop destroys
    // the tablet with this thread.
    pen.release_all();
    // Injector (and Mutter's implicit grab) outlives this session. Matching ups
    // here, keyed off the session — that is where a client vanishes mid-press.
    if !held_buttons.is_empty() || !held_keys.is_empty() || !held_touch.is_empty() {
        tracing::debug!(
            buttons = held_buttons.len(),
            keys = held_keys.len(),
            touch = held_touch.len(),
            "input: releasing held buttons/keys/contacts at session end"
        );
    }
    for code in held_buttons {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::MouseButtonUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    for code in held_keys {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::KeyUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    // The touch device is host-lifetime and its refresher keeps re-injecting whatever is held,
    // so a client that vanishes mid-touch leaves a finger down for every later session.
    for code in held_touch {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::TouchUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    // Slots back first: the join below can sit ~5 s on a quiet endpoint, and the
    // session that preempted this one claims after 1.5 s.
    drop(pads);
    // After the instant release sends: stop_all can block on a quiet capturer timeout.
    pad_streams.stop_all();
    // One line per motion pad, at `info`: the question is asked from a log after the fact.
    motion_cadence.log_summary();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::pad_pool::PadIdentity;
    use punktfunk_core::input::{InputEvent, InputKind};

    #[test]
    fn a_cropped_joiner_points_into_the_owner_picture() {
        use punktfunk_core::video_fit::{Reframe, VideoFit};
        let map = Reframe::plan(VideoFit::Crop, (3840, 2160), (2400, 1080));
        let mut ev = InputEvent {
            kind: InputKind::TouchDown,
            _pad: [0; 3],
            code: 1,
            x: 2400,
            y: 1080,
            flags: (2400 << 16) | 1080,
        };
        reframe_input(&mut ev, &map);
        assert_eq!((ev.x, ev.y, ev.flags), (3840, 1944, (3840 << 16) | 2160));
        // Keys and an unframed session pass through.
        let key = InputEvent {
            kind: InputKind::KeyDown,
            ..ev
        };
        let mut k = key;
        reframe_input(&mut k, &map);
        assert_eq!(k, key);
        let mut e = ev;
        reframe_input(&mut e, &Reframe::default());
        assert_eq!(e, ev);

        let pen = punktfunk_core::quic::PenSample {
            x: 0.5,
            y: 0.0,
            ..Default::default()
        };
        let out = reframe_pen(&punktfunk_core::quic::PenBatch::new(7, &[pen]), &map);
        assert_eq!(
            (out.seq, out.samples()[0].x, out.samples()[0].y),
            (7, 0.5, 0.1)
        );
    }

    /// Mid-stream compositor switch: later events land on the new target.
    #[test]
    fn input_route_swaps_targets_mid_flight() {
        let ev = InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        let (shared_tx, shared_rx) = std::sync::mpsc::channel::<InputEvent>();
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel::<InputEvent>();
        let route = InputRoute::new(shared_tx);
        route.send(ev).unwrap();
        assert_eq!(shared_rx.try_recv().unwrap().x, 1);
        route.set(pinned_tx);
        route.send(ev).unwrap();
        assert!(shared_rx.try_recv().is_err(), "old target no longer fed");
        assert_eq!(pinned_rx.try_recv().unwrap().y, 2);
    }

    /// Two pads whose OS slots were claimed out of wire order — the pad that moves
    /// first takes the lower slot whatever the client numbered it. Untranslated, one
    /// pad's raw reports drive the other's device and Steam answers the touch with
    /// rumble on the wrong controller.
    #[test]
    fn rich_input_is_re_addressed_into_slot_space() {
        use punktfunk_core::quic::{RichInput, HID_REPORT_MAX};
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), None);
        let slot1 = pads.slots.claim_for(1).expect("a free OS slot");
        let slot0 = pads.slots.claim_for(0).expect("a free OS slot");
        assert_ne!(slot0, slot1, "two wire pads share an OS slot");

        let report = |pad| RichInput::HidReport {
            pad,
            len: 1,
            data: [0x42; HID_REPORT_MAX],
        };
        assert_eq!(
            pads.rich_in_slot_space(report(1)).map(|r| r.pad()),
            Some(slot1)
        );
        assert_eq!(
            pads.rich_in_slot_space(report(0)).map(|r| r.pad()),
            Some(slot0)
        );
        // No slot = no device. Dropped, never folded onto slot 0.
        assert!(pads.rich_in_slot_space(report(2)).is_none());
    }

    /// The console and the client are told which players this session is, once per
    /// change. Resent every frame it would be a control message per pad packet.
    #[test]
    fn the_slot_map_is_reported_only_when_it_moves() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), None);
        assert_eq!(pads.take_slot_change(), None, "no pad, nothing to say");
        let slot = pads.claim_os_slot(0).expect("a free OS slot");
        assert_eq!(pads.take_slot_change(), Some(1 << slot));
        assert_eq!(pads.take_slot_change(), None, "the same slots, again");
        pads.slots.release(0);
        assert_eq!(pads.take_slot_change(), Some(0), "the pad left");
    }

    /// A pad-audio streamer starts at the arrival and captures the endpoint / card / sink
    /// named by the pad's OS slot, so the arrival must reserve the same slot the pad's
    /// first frame would have taken — never a second name for one pad.
    #[test]
    fn an_arrival_reserves_the_slot_the_first_frame_would_claim() {
        let mut pads = Pads::new(GamepadPref::DualSense, PadIdentity::anonymous(), None);
        let slot = pads.claim_os_slot(1).expect("a free OS slot");
        assert_eq!(pads.slots.claim_for(1), Some(slot), "first frame re-mints");
        assert_eq!(pads.claim_os_slot(1), Some(slot), "re-declare re-mints");
    }

    /// A reconnect preempts its zombie by setting `stop` and claims the same OS slots
    /// 1.5 s later, while the zombie's connection (and so its input channel) is still
    /// open. The thread must end on the flag alone; `PadSlotMap`'s drop frees the slots.
    #[tokio::test]
    async fn a_stopped_session_ends_its_input_thread_while_its_link_is_open() {
        use punktfunk_core::quic::endpoint;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("host side connects");
            (server, conn)
        });
        let client = endpoint::client_insecure().unwrap();
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server, host_conn) = accept.await.unwrap();

        // Held open for the whole test: `Disconnected` must not be what ends the thread.
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<ClientInput>(8);
        let (inj_tx, _inj_rx) = std::sync::mpsc::channel::<InputEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                input_thread(
                    input_rx,
                    super::super::link::SessionLink::Quic(host_conn),
                    InputRoute::new(inj_tx),
                    GamepadPref::Xbox360,
                    false,
                    PadIdentity::anonymous(),
                    Arc::new(std::sync::atomic::AtomicU16::new(0)),
                    None,
                    Arc::new(AtomicU32::new(0)),
                    Arc::new(std::sync::Mutex::new(
                        punktfunk_core::video_fit::Reframe::default(),
                    )),
                    Arc::new(crate::pad_feed::PadFeed::new()),
                    None,
                    stop,
                    Arc::new(crate::session_status::SessionCounters::default()),
                )
            })
        };
        stop.store(true, Ordering::SeqCst);
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || thread.join()),
        )
        .await;
        assert!(
            joined.is_ok(),
            "input thread still running 2 s after stop with its channel open"
        );
        drop(input_tx);
        drop(client_conn);
    }

    /// The device lingers 300 ms after its removal frame; the pool slot lingers with it, so a
    /// re-plug inside the grace lands on the same slot and no other claimant takes it.
    #[test]
    fn a_removed_pad_keeps_its_slot_through_the_unplug_grace() {
        use std::time::{Duration, Instant};
        let t = Instant::now();
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), None);
        let slot = pads.slots.claim_for(0).expect("a free OS slot");
        pads.owner[0] = Some(GamepadPref::Xbox360);
        pads.note_removed(0, true, t);
        pads.reap_pending_at(t + Pads::RELEASE_GRACE - Duration::from_millis(1));
        assert_eq!(
            pads.slots.slot_of(0),
            Some(slot),
            "released inside the grace"
        );
        // A re-plug inside the grace: the frame re-mints the memoized slot and clears the clock.
        assert_eq!(pads.slots.claim_for(0), Some(slot));
        pads.pending_release[0] = None;
        pads.reap_pending_at(t + Duration::from_secs(5));
        assert_eq!(
            pads.slots.slot_of(0),
            Some(slot),
            "a re-plugged pad lost its slot"
        );
        // No re-plug: the slot follows the device out once the grace has run.
        pads.note_removed(0, true, t);
        pads.reap_pending_at(t + Pads::RELEASE_GRACE);
        assert_eq!(pads.slots.slot_of(0), None, "held past the grace");
        // A pad that never built a device has nothing to wait for.
        pads.slots.claim_for(1).expect("a free OS slot");
        pads.release_unbuilt(1);
        assert_eq!(pads.slots.slot_of(1), None);
    }

    #[test]
    fn pad_snapshot_replaces_state_and_seq_gates() {
        use punktfunk_core::input::{gamepad, GamepadSnapshot};
        let mut state = PadState::default();
        let mut last_seq: Option<u8> = None;

        // Incremental events first, then a snapshot replaces the whole state.
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 200,
            y: 0,
            flags: 0,
        };
        assert!(state.apply(&axis));
        assert_eq!(state.left_trigger, 200);

        let snap = GamepadSnapshot {
            pad: 0,
            seq: 1,
            buttons: gamepad::BTN_A,
            left_trigger: 255,
            right_trigger: 0,
            ls_x: 100,
            ls_y: -100,
            rs_x: 0,
            rs_y: 0,
        };
        assert!(GamepadSnapshot::seq_newer(snap.seq, last_seq));
        last_seq = Some(snap.seq);
        state.set_snapshot(&snap);
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.buttons, gamepad::BTN_A);
        assert_eq!((state.ls_x, state.ls_y), (100, -100));

        // A reordered (stale) snapshot must not roll the trigger back.
        let stale = GamepadSnapshot {
            seq: 0,
            left_trigger: 10,
            ..snap
        };
        assert!(!GamepadSnapshot::seq_newer(stale.seq, last_seq));

        // Unchanged refresh: newer seq, identical payload, compares equal after apply.
        let refresh = GamepadSnapshot { seq: 2, ..snap };
        assert!(GamepadSnapshot::seq_newer(refresh.seq, last_seq));
        let before = state;
        state.set_snapshot(&refresh);
        assert_eq!(state, before);

        // Wire roundtrip must decode to the same snapshot.
        let dec =
            GamepadSnapshot::from_event(&InputEvent::decode(&snap.to_event().encode()).unwrap())
                .unwrap();
        assert_eq!(dec, snap);
    }

    fn gp(kind: InputKind, code: u32, x: i32, pad: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: pad,
        }
    }

    /// A pad re-plug must not reset `rumble_seq`.
    ///
    /// The client's `rumble_last_seq` lives for the whole QUIC connection and has
    /// no reset (`client/pump/datagram_task.rs`). Resetting the host counter on
    /// `GamepadRemove` strands every later envelope until it climbs past the
    /// stored value (up to 128 sends).
    #[test]
    fn rumble_seq_survives_a_removal_so_the_client_gate_accepts() {
        use punktfunk_core::input::GamepadSnapshot;
        use punktfunk_core::quic::{decode_rumble_envelope, encode_rumble_datagram_v2};

        // Client half: one per-pad slot, per connection, never reset.
        let deliver = |seq: u8, gate: &mut Option<u8>| {
            let d = encode_rumble_datagram_v2(0, 0x4000, 0x8000, seq, 400);
            let env = decode_rumble_envelope(&d)
                .expect("v2 envelope decodes")
                .envelope
                .expect("v2 tail present");
            if GamepadSnapshot::seq_newer(env.seq, *gate) {
                *gate = Some(env.seq);
                true
            } else {
                false
            }
        };

        // Host half: one wrapping counter, bumped on every change and every renewal.
        let mut gate: Option<u8> = None;
        let mut seq = 0u8;

        // A long rumble before the unplug pushes the client's stored seq well past zero.
        for _ in 0..100 {
            seq = seq.wrapping_add(1);
            assert!(deliver(seq, &mut gate));
        }
        assert_eq!(gate, Some(100));

        // Unplug mid-buzz: the lease is cleared, the counter is not.
        let (mut state, mut seen, mut burst) =
            ((0x1234, 0x5678, 0x9ABC, 0xDEF0), true, RUMBLE_STOP_BURST);
        clear_pad_feedback(&mut state, &mut seen, &mut burst);
        assert_eq!(
            (state, seen, burst),
            ((0, 0, 0, 0), false, 0),
            "lease not cleared"
        );

        // Re-plug on the same index: the first envelope must reach the actuator.
        seq = seq.wrapping_add(1);
        assert!(
            deliver(seq, &mut gate),
            "first envelope after a re-plug was dropped by the client's reorder gate"
        );

        // Non-vacuity: a counter restarted at 0 is rejected for the whole forward window.
        let mut stranded = Some(100u8);
        assert!(
            (1..=100).all(|s| !deliver(s, &mut stranded)),
            "test is vacuous — a restarted counter should have been gated out"
        );
    }

    /// Incremental wire events fold into the full frame the virtual xpad applies.
    #[test]
    fn gamepad_accumulator() {
        use punktfunk_core::input::gamepad::*;
        let mut s = PadState::default();
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_LB, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LS_X, -32768, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_RT, 255, 0)));
        let f = s.frame(2, 0b0100);
        assert_eq!(f.buttons, BTN_A | BTN_LB);
        assert_eq!((f.ls_x, f.right_trigger), (-32768, 255));
        assert_eq!((f.index, f.active_mask), (2, 0b0100));

        // Release folds out; axis values clamp; unknown axis ids are rejected.
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 0, 0)));
        assert_eq!(s.frame(0, 1).buttons, BTN_LB);
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LT, 9_999, 0)));
        assert_eq!(s.left_trigger, 255);
        assert!(!s.apply(&gp(InputKind::GamepadAxis, 42, 1, 0)));
    }

    /// A rumble that drives only the impulse triggers must still get a live TTL
    /// (`design/trigger-rumble-plane.md`).
    ///
    /// `(low, high) == (0, 0)` as silence stamps trigger-only rumble `ttl = 0`;
    /// the client silences on arrival with no error. Uses the real predicate and
    /// encoder/decoder.
    #[test]
    fn a_trigger_only_rumble_gets_a_live_ttl() {
        use punktfunk_core::quic::{decode_rumble_envelope, encode_rumble_datagram_v3};

        // Impulse-trigger stream: handles at rest, triggers driven.
        let trigger_only: RumbleLevels = (0, 0, 0x8000, 0);
        assert!(
            !rumble_silent(trigger_only),
            "a trigger-only level was read as silence — the ttl=0 trap"
        );
        let ttl = if rumble_silent(trigger_only) {
            0
        } else {
            RUMBLE_TTL_MS
        };
        let d = encode_rumble_datagram_v3(0, 0, 0, 1, ttl, trigger_only.2, trigger_only.3);
        let u = decode_rumble_envelope(&d).expect("v3 envelope decodes");
        assert_eq!(
            u.envelope.expect("v3 carries the v2 tail").ttl_ms,
            RUMBLE_TTL_MS,
            "trigger-only rumble was stamped with a dead lease"
        );
        assert_eq!((u.left_trigger, u.right_trigger), (0x8000, 0));
        assert_eq!((u.low, u.high), (0, 0), "handles stay at rest");

        // All-zero is the only stop, and the only thing that gets ttl = 0.
        assert!(rumble_silent((0, 0, 0, 0)));
        for lv in [
            (1, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 0, 1, 0),
            (0, 0, 0, 1),
            (0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF),
        ] {
            assert!(!rumble_silent(lv), "{lv:?} must not read as a stop");
        }
    }
}
