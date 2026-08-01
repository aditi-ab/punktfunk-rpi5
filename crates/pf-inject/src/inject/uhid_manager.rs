//! The generic stateful virtual-pad manager ([`UhidManager`]) shared by the five backends that
//! keep a full per-pad report state (Linux UHID DualSense / DualShock 4 / Steam Deck, Windows UMDF
//! DualSense / DualShock 4): event routing, the frame merge, rich-input application, the silence
//! heartbeat, and the feedback pump with rumble + hidout dedup are written once here; a backend
//! supplies only its per-controller pieces via [`PadProto`]. The stateless backends (Linux uinput,
//! Windows XUSB) write frames straight through with no state vec / heartbeat / rich plane, so they
//! use [`PadSlots`] directly instead.

use crate::hidout_dedup::HidoutDedup;
use crate::pad_slots::PadSlots;
use anyhow::Result;
use punktfunk_core::input::{GamepadEvent, GamepadFrame, MAX_PADS};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::{Duration, Instant};

/// What one feedback pass extracted from a pad's driver/kernel channel. `rumble` rides the
/// universal 0xCA plane (deduped against the last-forwarded level); `hidout` carries the rich
/// 0xCD feedback events (lightbar / player LEDs / adaptive triggers), deduped via [`HidoutDedup`].
#[derive(Default)]
pub struct PadFeedback {
    /// `(low, high)` motor levels (0..=0xFF00), if the pass saw a rumble report.
    pub rumble: Option<(u16, u16)>,
    pub hidout: Vec<HidOutput>,
    /// Whether the game drove this pad's RUMBLE plane this poll — at least one output report
    /// asserted the vibration fields (valid-flag set, including an explicit zero), not merely any
    /// report. Backends uphold `rumble_drove == Some(true)` ⇔ `rumble.is_some()`; the separate
    /// field exists so `None` can mean "backend does not track activity" (force-off disabled).
    /// Keying the abandoned-rumble force-off in [`UhidManager::pump`] on the rumble plane
    /// specifically — not on general output traffic — is what keeps a title that streams
    /// LED/adaptive-trigger reports every frame from feeding the watchdog forever while a latched
    /// rumble level never re-asserts (the lost-stop case).
    pub rumble_drove: Option<bool>,
    /// The backend's driver-channel drain OVERFLOWED and dropped reports this poll — downstream
    /// feedback state is unknown. [`UhidManager::pump`] resyncs: it forwards a rumble stop
    /// (bounded and imperceptible — a game still rumbling re-asserts the level within a poll or
    /// two) and re-arms the rich-plane dedup so the next LED/trigger state re-forwards. Only the
    /// Windows ring drains can set it; Linux kernel channels drain losslessly.
    pub resync: bool,
}

/// The per-controller half of a stateful virtual-pad backend — everything [`UhidManager`] cannot
/// share because it differs per protocol: the transport open, the report-state model and its
/// GameStream/rich-input mappers, the state write, and the feedback poll.
///
/// The `&mut self` receivers let a backend carry configuration (the Steam-paddle remap policy, a
/// pad identity); most implementations are otherwise stateless.
pub trait PadProto {
    /// The per-pad transport (a UHID fd, a UMDF shared-memory channel, the Deck transport enum).
    type Pad;
    /// The pad's full report state (`DsState`, `SteamState`) — `Copy` like both of those, so the
    /// manager can hand a snapshot to [`write_state`](Self::write_state) without borrow gymnastics.
    type State: Copy;

    /// Backend tag in the shared lifecycle log lines, e.g. `"DualSense/Windows"`.
    const LABEL: &'static str;
    /// Device name in the create-failure line ("virtual `<DEVICE>` creation failed …").
    const DEVICE: &'static str;
    /// Suffix for the create-failure line — empty on Linux, the driver-install hint on Windows.
    const CREATE_HINT: &'static str;

    /// Open the virtual pad for wire index `idx`, logging its own success line (it knows the
    /// transport detail worth printing); failures are logged by the manager's create gate.
    fn open(&mut self, idx: u8) -> Result<Self::Pad>;
    /// The all-neutral report state a fresh or unplugged pad (re)starts from.
    fn neutral(&self) -> Self::State;
    /// Fold one decoded button/stick frame into a new state, preserving from `prev` every field
    /// that arrives on the rich plane instead (touch contacts / clicks, motion) — the G2 hook, in
    /// one place per backend. Paddle remap policy is applied here too.
    fn merge_frame(&self, prev: &Self::State, f: &GamepadFrame) -> Self::State;
    /// Apply one rich client→host event (touchpad contact / motion sample) to the state.
    fn apply_rich(&self, st: &mut Self::State, rich: RichInput);
    /// Write the full state to the pad (best-effort; the next frame or heartbeat re-syncs).
    fn write_state(&self, pad: &mut Self::Pad, st: &Self::State);
    /// Poll the pad's driver/kernel channel: answer any pending handshake and return the feedback
    /// it carried. `idx` is the wire pad index (the DualSense GET_REPORT replies need it).
    fn service(&self, pad: &mut Self::Pad, idx: u8) -> PadFeedback;
    /// Whether this pad needs a heartbeat write NOW regardless of the silence gap (the Steam
    /// backend streams through its gamepad-mode-entry pulse).
    fn force_heartbeat(&self, _pad: &Self::Pad) -> bool {
        false
    }
}

/// All virtual pads of one stateful backend, driven from decoded controller events — the shared
/// skeleton of the five UHID/UMDF managers. Method surface (`new` / `handle` / `apply_rich` /
/// `pump` / `heartbeat`) is exactly what the session input thread already drives, so each backend
/// re-exports itself as a `pub type … = UhidManager<…Proto>;` alias.
pub struct UhidManager<B: PadProto> {
    backend: B,
    slots: PadSlots<B::Pad>,
    /// Each pad's current full report — buttons/sticks merged with persisted rich-plane fields.
    state: Vec<B::State>,
    /// Last rumble forwarded per pad, so a report that only changes rich feedback doesn't re-send it.
    last_rumble: Vec<(u16, u16)>,
    /// Last rich feedback forwarded per pad, so an output report that only changed the rumble
    /// doesn't re-send unchanged lightbar/LED/trigger state.
    hidout_dedup: Vec<HidoutDedup>,
    /// When each pad last wrote an input report — drives [`heartbeat`](Self::heartbeat).
    last_write: Vec<Instant>,
    /// When the game last drove each pad's rumble plane (a backend that reports `rumble_drove`
    /// saw a vibration-asserting output report). A non-zero `last_rumble` older than
    /// [`RUMBLE_IDLE_TIMEOUT`] against this is a residual the game abandoned — see
    /// [`pump`](Self::pump).
    last_active: Vec<Instant>,
    /// Per-pad rate limiter for the ring-overflow WARN — see [`OverflowWarn`].
    overflow_warn: Vec<OverflowWarn>,
}

/// Rate limiter for the per-poll ring-overflow WARN. A sustained >2 kHz output-report writer
/// against a pre-v2.2 8-slot driver ring overflows EVERY ~4 ms poll; unlimited, that is ~230
/// WARN lines/s — a field log's 5000-line web-console ring was 96 % this one line, which evicted
/// the whole session history it was needed to diagnose (2026-07-30). One line per
/// [`Self::PERIOD`] with a `suppressed` count keeps the signal and the log.
#[derive(Default, Clone)]
struct OverflowWarn {
    /// When the last line was emitted (`None` = never — the next overflow logs immediately).
    last: Option<Instant>,
    /// Overflow polls swallowed since `last` — carried on the next emitted line.
    suppressed: u32,
}

impl OverflowWarn {
    const PERIOD: Duration = Duration::from_secs(1);

    /// Record one overflow poll; emit (rate-limited) the WARN for it.
    fn note(&mut self, now: Instant, backend: &'static str, index: usize) {
        if self
            .last
            .is_none_or(|t| now.duration_since(t) >= Self::PERIOD)
        {
            tracing::warn!(
                backend,
                index,
                suppressed = self.suppressed,
                "output-report ring overflow — resyncing feedback state (repeats coalesced, 1 \
                 line/s; `suppressed` = swallowed since the previous line)"
            );
            self.last = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

/// How long a latched, non-zero rumble may sit without the game driving the RUMBLE plane before it
/// is forced off. DualSense/DS4/Deck motors are level-triggered — they run until an output report
/// sets them to zero — so a game that latches a rumble and then stops asserting the vibration
/// fields (a residual left at a menu / loading screen, a forgotten stop, or a stop report lost to
/// the driver section's latest-report coalescing) would otherwise drone to the client forever: the
/// resend loop in `native.rs` renews the latched level every ~120 ms and the client's envelope
/// never expires. Keyed on the rumble plane specifically ([`PadFeedback::rumble_drove`]), not on
/// general output traffic, so an LED/adaptive-trigger stream cannot keep an abandoned rumble
/// alive — the same decay real DualSense-family firmware applies when vibration-flagged reports
/// cease.
///
/// INVARIANT: must stay comfortably above SDL's ~2 s internal rumble resend
/// (`SDL_RUMBLE_RESEND_MS`) — SDL-class writers re-assert a held level on that cadence *because*
/// real firmware decays, and that re-assert is what keeps a legitimately-held long rumble alive
/// here. The XUSB path shares this window via [`rumble_idle_timeout`] (every XUSB write IS a
/// rumble write, so its any-activity keying is already rumble-keyed by construction).
const RUMBLE_IDLE_TIMEOUT: Duration = Duration::from_millis(2500);

/// The abandoned-rumble force-off window, env-hatched: `PUNKTFUNK_RUMBLE_IDLE_MS` overrides
/// [`RUMBLE_IDLE_TIMEOUT`]; `0` disables the watchdog entirely (the pre-watchdog behavior, for
/// bisecting field reports). Non-zero overrides are floored just above SDL's ~2 s resend so the
/// hatch cannot cut legitimately-held rumble (see the INVARIANT above). Shared by the UHID/UMDF
/// pump, the Windows XUSB manager, and the Linux uinput FF mixer.
pub(crate) fn rumble_idle_timeout() -> Option<Duration> {
    static VAL: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *VAL.get_or_init(|| match std::env::var("PUNKTFUNK_RUMBLE_IDLE_MS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Duration::from_millis(ms.max(2100))),
            Err(_) => Some(RUMBLE_IDLE_TIMEOUT),
        },
        Err(_) => Some(RUMBLE_IDLE_TIMEOUT),
    })
}

impl<B: PadProto + Default> UhidManager<B> {
    pub fn new() -> UhidManager<B> {
        UhidManager::with_backend(B::default())
    }
}

impl<B: PadProto + Default> Default for UhidManager<B> {
    fn default() -> UhidManager<B> {
        UhidManager::new()
    }
}

impl<B: PadProto> UhidManager<B> {
    pub fn with_backend(backend: B) -> UhidManager<B> {
        let state = (0..MAX_PADS).map(|_| backend.neutral()).collect();
        UhidManager {
            backend,
            slots: PadSlots::new(B::LABEL, B::DEVICE, B::CREATE_HINT),
            state,
            last_rumble: vec![(0, 0); MAX_PADS],
            hidout_dedup: vec![HidoutDedup::default(); MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            last_active: vec![Instant::now(); MAX_PADS],
            overflow_warn: vec![OverflowWarn::default(); MAX_PADS],
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then merge button/stick state).
    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival ({})", B::LABEL);
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Unplugs: drop any allocated pad whose mask bit cleared, resetting its state.
                let swept = self.slots.sweep(f.active_mask);
                for i in 0..MAX_PADS {
                    if swept & (1 << i) != 0 {
                        self.reset_pad(i);
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                // Merge buttons/sticks/triggers from the frame, preserving the rich-plane fields
                // (touch + motion arrive separately and must survive a button-only frame).
                self.state[idx] = self.backend.merge_frame(&self.state[idx], f);
                self.write(idx);
            }
        }
    }

    /// Apply one rich client→host event (touchpad contact / motion sample) to an existing pad,
    /// preserving its button/stick state. Rich events never create a pad (a controller must have
    /// arrived first); they're dropped if the pad isn't present.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. }
            | RichInput::HidReport { pad, .. } => pad as usize,
        };
        if idx >= MAX_PADS || self.slots.get(idx).is_none() {
            return;
        }
        self.backend.apply_rich(&mut self.state[idx], rich);
        self.write(idx);
    }

    /// Re-emit each live pad's CURRENT report if it's been silent for `max_gap` (or the backend
    /// forces a write). The UHID/UMDF drivers treat a multi-second input silence — a held-steady
    /// stick produces no wire events — as an unplugged controller; re-sending the current state is
    /// idempotent (a stale-but-correct frame, never a phantom input).
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..MAX_PADS {
            let Some(pad) = self.slots.get(i) else {
                continue;
            };
            if self.backend.force_heartbeat(pad)
                || now.duration_since(self.last_write[i]) >= max_gap
            {
                self.write(i);
            }
        }
    }

    /// Service every pad: answer any pending driver/kernel handshake and route a game's feedback
    /// back out. `rumble` is invoked `(index, low, high)` only when the motor level *changes* (the
    /// universal 0xCA plane); `hidout` is invoked per rich feedback event that isn't an exact
    /// repeat of the last-forwarded value (the 0xCD plane). Call frequently — kernel/driver init
    /// handshakes block until answered.
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        let now = Instant::now();
        for i in 0..MAX_PADS {
            let Some(pad) = self.slots.get_mut(i) else {
                continue;
            };
            let fb = self.backend.service(pad, i as u8);
            if fb.resync {
                // The driver's output-report ring overflowed — reports were dropped and the
                // feedback state is unknown beyond what the drain salvaged from the legacy
                // latest-report slot (delivered through `fb` like any report). Conservatively
                // silence the pad FIRST (a plane the salvage didn't carry must not stay latched;
                // the salvaged state re-applies right below) and re-arm the rich-plane dedup so
                // the next LED/trigger state re-forwards. WARN through the per-pad rate limiter —
                // a storm overflows every poll and the raw line once flooded a whole log export.
                self.overflow_warn[i].note(now, B::LABEL, i);
                if self.last_rumble[i] != (0, 0) {
                    self.last_rumble[i] = (0, 0);
                    rumble(i as u16, 0, 0);
                }
                self.hidout_dedup[i] = HidoutDedup::default();
            }
            // Refresh the game-activity clock when the game drove the pad's RUMBLE plane this poll
            // (a vibration-asserting report, even at an unchanged level). LED/trigger-only traffic
            // does NOT refresh — see `PadFeedback::rumble_drove`. `None` = a backend that does not
            // track activity: treated as always-active, so the force-off below never fires there.
            if fb.rumble_drove != Some(false) {
                self.last_active[i] = now;
            }
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            } else if self.last_rumble[i] != (0, 0)
                && rumble_idle_timeout()
                    .is_some_and(|t| now.duration_since(self.last_active[i]) >= t)
            {
                // A non-zero rumble is latched but the game has not driven the rumble plane for
                // the idle window — a residual it forgot to stop (or whose stop was lost). Force it
                // off (and forward the zero) so `native.rs`'s resend loop stops droning it to the
                // client. Mirrors the XUSB path's guard; see RUMBLE_IDLE_TIMEOUT.
                tracing::info!(
                    backend = B::LABEL,
                    index = i,
                    prev_low = self.last_rumble[i].0,
                    prev_high = self.last_rumble[i].1,
                    "rumble: stale residual (game stopped driving the rumble plane) — forcing off"
                );
                self.last_rumble[i] = (0, 0);
                rumble(i as u16, 0, 0);
            }
            for h in fb.hidout {
                // Skip rich feedback that repeats the last-forwarded value (a game's output report
                // re-sends unchanged lightbar/LED/trigger state alongside every rumble update).
                if self.hidout_dedup[i].should_forward(&h) {
                    hidout(h);
                }
            }
        }
    }

    /// Write the pad's current state (if it exists) and reset its heartbeat clock — on every write
    /// (real input or heartbeat), so an actively-used pad emits no extra reports.
    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.slots.get_mut(idx) {
            self.backend.write_state(pad, &st);
        }
        self.last_write[idx] = Instant::now();
    }

    /// Gate-checked create; a FRESH pad starts from neutral state + re-armed dedups.
    fn ensure(&mut self, idx: usize) {
        let backend = &mut self.backend;
        if self.slots.ensure(idx, |i| backend.open(i)) {
            self.reset_pad(idx);
        }
    }

    /// Reset one pad's sibling state (on create and unplug) so the first frame/feedback after a
    /// (re)connect starts from scratch and is always forwarded.
    fn reset_pad(&mut self, idx: usize) {
        self.state[idx] = self.backend.neutral();
        self.last_rumble[idx] = (0, 0);
        self.hidout_dedup[idx].clear();
        self.last_write[idx] = Instant::now();
        self.last_active[idx] = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted mock: `open` fails while `fail_opens > 0`; `service` replays canned feedback;
    /// `MockState` carries a marker for the frame-merge preserve check.
    #[derive(Default)]
    struct MockProto {
        fail_opens: RefCell<u32>,
        feedback: RefCell<Vec<PadFeedback>>,
        force_hb: bool,
    }

    #[derive(Clone, Copy, Default, PartialEq, Debug)]
    struct MockState {
        buttons: u32,
        /// Stands in for the rich-plane fields (touch/motion/clicks): set by `apply_rich`,
        /// must survive `merge_frame`.
        rich_marker: u16,
    }

    /// Per-pad transport stub recording every state write.
    #[derive(Default)]
    struct MockPad {
        writes: RefCell<Vec<MockState>>,
    }

    impl PadProto for MockProto {
        type Pad = MockPad;
        type State = MockState;
        const LABEL: &'static str = "Mock";
        const DEVICE: &'static str = "mock pad";
        const CREATE_HINT: &'static str = "";

        fn open(&mut self, _idx: u8) -> Result<MockPad> {
            let mut fails = self.fail_opens.borrow_mut();
            if *fails > 0 {
                *fails -= 1;
                anyhow::bail!("scripted open failure");
            }
            Ok(MockPad::default())
        }
        fn neutral(&self) -> MockState {
            MockState::default()
        }
        fn merge_frame(&self, prev: &MockState, f: &GamepadFrame) -> MockState {
            MockState {
                buttons: f.buttons,
                rich_marker: prev.rich_marker, // the preserve-rich-fields contract
            }
        }
        fn apply_rich(&self, st: &mut MockState, rich: RichInput) {
            if let RichInput::Touchpad { x, .. } = rich {
                st.rich_marker = x;
            }
        }
        fn write_state(&self, pad: &mut MockPad, st: &MockState) {
            pad.writes.borrow_mut().push(*st);
        }
        fn service(&self, _pad: &mut MockPad, _idx: u8) -> PadFeedback {
            let mut fb = self.feedback.borrow_mut();
            if fb.is_empty() {
                PadFeedback::default()
            } else {
                fb.remove(0)
            }
        }
        fn force_heartbeat(&self, _pad: &MockPad) -> bool {
            self.force_hb
        }
    }

    fn frame(idx: i16, mask: u16, buttons: u32) -> GamepadEvent {
        GamepadEvent::State(GamepadFrame {
            index: idx,
            active_mask: mask,
            buttons,
            ..Default::default()
        })
    }

    fn touch(pad: u8, x: u16) -> RichInput {
        RichInput::Touchpad {
            pad,
            finger: 0,
            active: true,
            x,
            y: 0,
        }
    }

    fn mgr() -> UhidManager<MockProto> {
        UhidManager::new()
    }

    #[test]
    fn arrival_eager_creates_the_pad() {
        // G10 as a generic regression test: Arrival must build the device before the first frame.
        let mut m = mgr();
        m.handle(&GamepadEvent::Arrival {
            index: 2,
            kind: 1,
            capabilities: 0,
            audio_caps: 0,
        });
        assert!(m.slots.get(2).is_some());
    }

    #[test]
    fn button_frame_preserves_rich_fields_and_writes_merged_state() {
        // G2 as a generic regression test: rich-plane state must survive a button-only frame.
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(touch(0, 777));
        m.handle(&frame(0, 0b1, 0xA));
        let pad = m.slots.get(0).unwrap();
        let writes = pad.writes.borrow();
        let last = writes.last().unwrap();
        assert_eq!(last.buttons, 0xA);
        assert_eq!(last.rich_marker, 777); // preserved across the merge
    }

    #[test]
    fn removal_frame_never_recreates_the_pad_it_swept() {
        let mut m = mgr();
        m.handle(&frame(1, 0b10, 0));
        assert!(m.slots.get(1).is_some());
        // Bit 1 cleared: the first sweep only ARMS the devnode-churn grace — the pad holds (a
        // mask glitch must not flap PnP devices; see pad_slots::SWEEP_GRACE).
        m.handle(&frame(1, 0b00, 0));
        assert!(m.slots.get(1).is_some(), "inside the grace — not yet swept");
        // Grace elapsed: the frame IS pad 1's removal — sweep, then early-return (no ensure).
        m.slots.expire_grace();
        m.handle(&frame(1, 0b00, 0));
        assert!(m.slots.get(1).is_none());
    }

    #[test]
    fn rich_event_for_an_absent_pad_is_dropped_and_never_creates() {
        let mut m = mgr();
        m.apply_rich(touch(3, 42));
        assert!(m.slots.get(3).is_none());
        // …and it left no state behind: a later create starts truly neutral.
        m.handle(&frame(3, 0b1000, 0));
        assert_eq!(m.state[3].rich_marker, 0);
    }

    #[test]
    fn create_failure_backs_off_then_state_still_tracks() {
        let mut m = mgr();
        *m.backend.fail_opens.borrow_mut() = 1;
        m.handle(&frame(0, 0b1, 0x1));
        // Open failed: no pad, but the merged state is tracked (matching the old managers).
        assert!(m.slots.get(0).is_none());
        assert_eq!(m.state[0].buttons, 0x1);
        // Next frame inside the backoff window: still no pad, no panic.
        m.handle(&frame(0, 0b1, 0x3));
        assert!(m.slots.get(0).is_none());
        assert_eq!(m.state[0].buttons, 0x3);
    }

    #[test]
    fn rumble_dedup_forwards_changes_only_and_rearms_on_recreate() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(|i, lo, hi| out.borrow_mut().push((i, lo, hi)), |_| {});
            out.into_inner()
        };
        let rumble = |r| PadFeedback {
            rumble: Some(r),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        };
        *m.backend.feedback.borrow_mut() = vec![rumble((100, 0)), rumble((100, 0)), rumble((7, 7))];
        assert_eq!(collect(&mut m), vec![(0, 100, 0)]); // first value forwards
        assert_eq!(collect(&mut m), vec![]); // exact repeat deduped
        assert_eq!(collect(&mut m), vec![(0, 7, 7)]); // change forwards
                                                      // Unplug + recreate re-arms the dedup: the same level forwards again.
        m.handle(&frame(0, 0b0, 0)); // arms the sweep grace
        m.slots.expire_grace();
        m.handle(&frame(0, 0b0, 0)); // grace elapsed — actually swept
        m.handle(&frame(0, 0b1, 0));
        *m.backend.feedback.borrow_mut() = vec![rumble((7, 7))];
        assert_eq!(collect(&mut m), vec![(0, 7, 7)]);
    }

    #[test]
    fn abandoned_rumble_is_forced_off_after_idle_timeout() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(|i, lo, hi| out.borrow_mut().push((i, lo, hi)), |_| {});
            out.into_inner()
        };
        // The game latches a non-zero rumble (a fresh report drove the pad).
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![(0, 200, 0)]);

        // The game stops driving the RUMBLE plane — no output report at all, or (equivalently, the
        // confirmed stuck-ON case) a stream of LED/adaptive-trigger reports that never assert the
        // vibration fields — and never sent a stop. Before the idle window elapses, nothing is
        // forwarded — the latched level is left asserting.
        let idle = || PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(false),
            resync: false,
        };
        *m.backend.feedback.borrow_mut() = vec![idle()];
        assert_eq!(collect(&mut m), vec![]);

        // Simulate the game having abandoned the pad past the timeout: the residual is forced off
        // exactly once, then stays off (no repeated zero spam).
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![idle(), idle()];
        assert_eq!(collect(&mut m), vec![(0, 0, 0)]); // forced off
        assert_eq!(collect(&mut m), vec![]); // already zero — no repeat
    }

    #[test]
    fn asserted_rumble_survives_idle_timeout_while_game_drives() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(|i, lo, hi| out.borrow_mut().push((i, lo, hi)), |_| {});
            out.into_inner()
        };
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![(0, 200, 0)]);

        // Even with a stale clock, a poll where the game drove the rumble plane refreshes
        // activity, so the held rumble is NOT cut. Backends report that as
        // `rumble: Some(level), rumble_drove: Some(true)` (an unchanged level dedups, no forward);
        // the manager also honors the bare `rumble_drove: Some(true)` shape defensively.
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![]); // unchanged level dedups, clock refreshed
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![]);
    }

    #[test]
    fn hidout_dedup_drops_exact_repeats() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: vec![led(10), led(10), led(20)],
            rumble_drove: Some(true),
            resync: false,
        }];
        let out = RefCell::new(0u32);
        m.pump(
            |_, _, _| {},
            |_| {
                *out.borrow_mut() += 1;
            },
        );
        assert_eq!(out.into_inner(), 2); // 10 forwarded once, 20 forwarded; the repeat dropped
    }

    #[test]
    fn heartbeat_reemits_silent_pads_and_honors_force() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0x5));
        let writes = |m: &UhidManager<MockProto>| m.slots.get(0).unwrap().writes.borrow().len();
        let after_frame = writes(&m);
        // A pad written just now is NOT re-emitted under a huge gap…
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(writes(&m), after_frame);
        // …but a zero gap counts it as silent and re-emits the CURRENT state.
        m.heartbeat(Duration::ZERO);
        assert_eq!(writes(&m), after_frame + 1);
        assert_eq!(
            m.slots
                .get(0)
                .unwrap()
                .writes
                .borrow()
                .last()
                .unwrap()
                .buttons,
            0x5
        );
        // The backend's force flag overrides the gap entirely (the Steam mode-entry pulse).
        m.backend.force_hb = true;
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(writes(&m), after_frame + 2);
    }
    /// The overflow WARN limiter: a storm overflowing every ~4 ms poll must emit one line per
    /// [`OverflowWarn::PERIOD`] and carry the swallowed count — the raw per-poll line once made
    /// up 96 % of a field log export and evicted the session history around it.
    #[test]
    fn overflow_warn_coalesces_within_the_period() {
        let mut w = OverflowWarn::default();
        let t0 = Instant::now();
        w.note(t0, "Mock", 0); // first overflow logs immediately
        assert_eq!((w.last, w.suppressed), (Some(t0), 0));
        for _ in 0..250 {
            w.note(t0 + Duration::from_millis(4), "Mock", 0);
        }
        assert_eq!((w.last, w.suppressed), (Some(t0), 250), "storm coalesced");
        let t1 = t0 + OverflowWarn::PERIOD;
        w.note(t1, "Mock", 0); // period elapsed — logs (with suppressed=250) and re-arms
        assert_eq!((w.last, w.suppressed), (Some(t1), 0));
    }

    /// A ring-overflow resync must silence a latched rumble once and re-arm the rich-plane dedup,
    /// so the game's next asserted state re-forwards even when it equals the pre-overflow state.
    #[test]
    fn resync_forces_stop_and_rearms_dedup() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        let collect = |m: &mut UhidManager<MockProto>| {
            let rumbles = RefCell::new(Vec::new());
            let hidouts = RefCell::new(0u32);
            m.pump(
                |i, lo, hi| rumbles.borrow_mut().push((i, lo, hi)),
                |_| *hidouts.borrow_mut() += 1,
            );
            (rumbles.into_inner(), hidouts.into_inner())
        };

        // Latch a rumble + an LED.
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((100, 0)),
            hidout: vec![led(10)],
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 100, 0)], 1));

        // Overflow poll: no reports survived, resync flagged → forced stop, exactly once.
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(false),
            resync: true,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 0, 0)], 0));

        // The game re-asserts the SAME rumble + LED state: both must re-forward (the rumble
        // because the forced stop reset `last_rumble`, the LED because the dedup was re-armed).
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((100, 0)),
            hidout: vec![led(10)],
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 100, 0)], 1));

        // A resync with nothing latched forwards no spurious stop.
        *m.backend.feedback.borrow_mut() = vec![
            PadFeedback {
                rumble: Some((0, 0)),
                hidout: Vec::new(),
                rumble_drove: Some(true),
                resync: false,
            },
            PadFeedback {
                rumble: None,
                hidout: Vec::new(),
                rumble_drove: Some(false),
                resync: true,
            },
        ];
        assert_eq!(collect(&mut m), (vec![(0, 0, 0)], 0)); // the explicit stop
        assert_eq!(collect(&mut m), (vec![], 0)); // resync at zero — silent
    }
}
