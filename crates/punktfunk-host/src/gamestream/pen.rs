//! Translate Moonlight's edge-based `SS_PEN` / `SS_TOUCH` events (design/pen-tablet-input.md
//! §4) into punktfunk's state-full pen model and wire-touch events.
//!
//! Pen: each packet becomes one complete [`PenSample`] (packet fields merged over the last
//! known state, since e.g. `BUTTON_ONLY` carries no position by spec), fed through the same
//! [`PenTracker`] → [`VirtualPen`] chain the native plane uses — so Moonlight iOS with an
//! Apple Pencil exercises the exact injection path our own clients will.
//!
//! No stroke timeout here, deliberately: the native plane's 200 ms failsafe exists because
//! datagrams are lossy and clients heartbeat; Moonlight's control stream is ordered/reliable
//! ENet and its clients do NOT heartbeat a stationary pen — a timeout would lift a held
//! stroke. Lost-client cleanup is the ENet disconnect (the control loop re-news this
//! translator, and destroying the uinput device releases everything kernel-side).
//!
//! Touch: forwarded as the ordinary wire touch kinds (`TouchDown/Move/Up`, normalized onto a
//! synthetic 65535² surface) through the injector service — pressure/area are dropped v1
//! (the wire touch has no field for them; `RICH_TOUCH` is the design's §8 upgrade).

use super::input::{SsPen, SsPointer, SsTouch};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, PenTracker, PenTransition, PEN_ANGLE_UNKNOWN, PEN_BARREL1, PEN_BARREL2,
    PEN_IN_RANGE, PEN_TILT_UNKNOWN, PEN_TOUCHING,
};

// moonlight-common-c Limelight.h event/tool/button vocabulary.
const LI_TOUCH_EVENT_HOVER: u8 = 0x00;
const LI_TOUCH_EVENT_DOWN: u8 = 0x01;
const LI_TOUCH_EVENT_UP: u8 = 0x02;
const LI_TOUCH_EVENT_MOVE: u8 = 0x03;
const LI_TOUCH_EVENT_CANCEL: u8 = 0x04;
const LI_TOUCH_EVENT_BUTTON_ONLY: u8 = 0x05;
const LI_TOUCH_EVENT_HOVER_LEAVE: u8 = 0x06;
const LI_TOUCH_EVENT_CANCEL_ALL: u8 = 0x07;
const LI_TOOL_TYPE_PEN: u8 = 0x01;
const LI_TOOL_TYPE_ERASER: u8 = 0x02;
const LI_PEN_BUTTON_PRIMARY: u8 = 0x01;
const LI_PEN_BUTTON_SECONDARY: u8 = 0x02;
const LI_ROT_UNKNOWN: u16 = 0xFFFF;
const LI_TILT_UNKNOWN: u8 = 0xFF;

/// Synthetic reference extent for forwarded touches: coordinates arrive normalized, so map
/// them onto a full-range surface and let the injector rescale to its output (the
/// `MouseMoveAbs`/touch `flags = (w << 16) | h` contract).
const TOUCH_SURFACE: u32 = 65535;

/// Most concurrently-tracked touch pointers (for `CANCEL_ALL` replay). A flood of distinct
/// ids can't grow memory — untracked ids still forward down/up verbatim, they just miss a
/// later `CANCEL_ALL` (which a real client never has more than ~10 contacts for anyway).
const MAX_TOUCH_IDS: usize = 32;

/// Per-GameStream-session pen + touch translator. Owned by the control loop next to the
/// gamepad manager; re-created on client disconnect (the dropped [`VirtualPen`] destroys the
/// uinput device, which releases any held tool/tip kernel-side).
pub struct GsPointer {
    tracker: PenTracker,
    dev: Option<crate::inject::pen::VirtualPen>,
    create_failed: bool,
    seq: u16,
    out: Vec<PenTransition>,
    /// Last complete pen state — the merge base for packets that omit fields
    /// (`BUTTON_ONLY` ignores x/y/pressure/tilt/rotation by spec).
    last: PenSample,
    /// This client sends hover events, so an `UP` means "back to hover" rather than "gone" —
    /// clients without hover support get proximity-out on lift instead of a pen parked
    /// forever at the last point.
    saw_hover: bool,
    /// Active forwarded touch ids, for `CANCEL_ALL` replay as per-id ups.
    touch_ids: Vec<u32>,
    /// Whether this session's first SS_TOUCH was logged (the one observable breadcrumb an
    /// on-glass "touch does nothing" report needs — every later stage logs its own failure).
    touch_seen: bool,
}

impl GsPointer {
    pub fn new() -> GsPointer {
        GsPointer {
            tracker: PenTracker::default(),
            dev: None,
            create_failed: false,
            seq: 0,
            out: Vec::new(),
            last: PenSample::default(),
            saw_hover: false,
            touch_ids: Vec::new(),
            touch_seen: false,
        }
    }

    /// Apply one decoded pointer packet. Touch events forward through `sink` (the injector
    /// channel); pen events drive this session's virtual tablet.
    pub fn apply(&mut self, p: &SsPointer, sink: impl FnMut(InputEvent)) {
        match p {
            SsPointer::Pen(pen) => self.apply_pen(pen),
            SsPointer::Touch(touch) => self.apply_touch(touch, sink),
        }
    }

    fn apply_pen(&mut self, p: &SsPen) {
        let Some(sample) = self.pen_sample(p) else {
            return;
        };
        self.last = sample;
        if self.dev.is_none() && !self.create_failed {
            match crate::inject::pen::VirtualPen::create() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    self.create_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "gamestream pen: virtual tablet creation failed — dropping pen input"
                    );
                }
            }
        }
        self.out.clear();
        let batch = punktfunk_core::quic::PenBatch::new(self.seq, &[sample]);
        self.seq = self.seq.wrapping_add(1);
        self.tracker.apply(&batch, &mut self.out);
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }

    /// Merge one edge-based packet over the last known state into a complete sample.
    /// `None` = nothing to apply (a `BUTTON_ONLY` before any positioned event).
    fn pen_sample(&mut self, p: &SsPen) -> Option<PenSample> {
        let buttons = (if p.buttons & LI_PEN_BUTTON_PRIMARY != 0 {
            PEN_BARREL1
        } else {
            0
        }) | (if p.buttons & LI_PEN_BUTTON_SECONDARY != 0 {
            PEN_BARREL2
        } else {
            0
        });
        let tool = match p.tool {
            LI_TOOL_TYPE_PEN => PenTool::Pen,
            LI_TOOL_TYPE_ERASER => PenTool::Eraser,
            _ => PenTool::Unknown,
        };
        // BUTTON_ONLY ignores x/y/pressure/rotation/tilt by spec — reuse the last state.
        if p.event_type == LI_TOUCH_EVENT_BUTTON_ONLY {
            if !self.tracker.is_active() {
                return None;
            }
            let mut s = self.last;
            s.state = (s.state & !(PEN_BARREL1 | PEN_BARREL2)) | buttons;
            return Some(s);
        }

        let mut s = PenSample {
            state: buttons,
            tool,
            x: p.x.clamp(0.0, 1.0),
            y: p.y.clamp(0.0, 1.0),
            dt_us: 0,
            roll_deg: PEN_ANGLE_UNKNOWN, // the GameStream wire has no barrel-roll axis
            azimuth_deg: if p.rotation == LI_ROT_UNKNOWN {
                PEN_ANGLE_UNKNOWN
            } else {
                p.rotation % 360
            },
            tilt_deg: if p.tilt == LI_TILT_UNKNOWN {
                PEN_TILT_UNKNOWN
            } else {
                p.tilt.min(90)
            },
            ..PenSample::default()
        };
        match p.event_type {
            LI_TOUCH_EVENT_DOWN | LI_TOUCH_EVENT_MOVE => {
                s.state |= PEN_IN_RANGE | PEN_TOUCHING;
                // pressureOrDistance is pressure (0..1) in contact — and 0.0 means UNKNOWN
                // there, which must still ink: binary-stylus clients get full pressure.
                s.pressure = if p.pressure_or_distance <= 0.0 {
                    u16::MAX
                } else {
                    (p.pressure_or_distance.clamp(0.0, 1.0) * 65535.0) as u16
                };
                s.distance = 0;
            }
            LI_TOUCH_EVENT_HOVER => {
                self.saw_hover = true;
                s.state |= PEN_IN_RANGE;
                // Hovering: pressureOrDistance is distance, 1.0 = farthest.
                s.distance = (p.pressure_or_distance.clamp(0.0, 1.0) * 65534.0) as u16;
            }
            LI_TOUCH_EVENT_UP => {
                // A hover-capable client keeps proximity after lift (its HOVER/HOVER_LEAVE
                // events own the exit); one that never hovers would otherwise park the pen
                // in proximity forever, so lift = leave for those.
                if self.saw_hover {
                    s.state |= PEN_IN_RANGE;
                }
            }
            LI_TOUCH_EVENT_HOVER_LEAVE | LI_TOUCH_EVENT_CANCEL | LI_TOUCH_EVENT_CANCEL_ALL => {
                s.state &= !(PEN_BARREL1 | PEN_BARREL2); // out of range releases buttons too
            }
            _ => return None, // unknown future event type — drop, never guess
        }
        Some(s)
    }

    fn apply_touch(&mut self, t: &SsTouch, mut sink: impl FnMut(InputEvent)) {
        if !self.touch_seen {
            self.touch_seen = true;
            tracing::info!(
                event_type = t.event_type,
                "gamestream: touch plane active (first SS_TOUCH from this client)"
            );
        }
        let ev = |kind: InputKind, id: u32, x: f32, y: f32| InputEvent {
            kind,
            _pad: [0; 3],
            code: id,
            x: (x.clamp(0.0, 1.0) * TOUCH_SURFACE as f32) as i32,
            y: (y.clamp(0.0, 1.0) * TOUCH_SURFACE as f32) as i32,
            flags: (TOUCH_SURFACE << 16) | TOUCH_SURFACE,
        };
        match t.event_type {
            LI_TOUCH_EVENT_DOWN => {
                if !self.touch_ids.contains(&t.pointer_id) && self.touch_ids.len() < MAX_TOUCH_IDS {
                    self.touch_ids.push(t.pointer_id);
                }
                sink(ev(InputKind::TouchDown, t.pointer_id, t.x, t.y));
            }
            LI_TOUCH_EVENT_MOVE => sink(ev(InputKind::TouchMove, t.pointer_id, t.x, t.y)),
            LI_TOUCH_EVENT_UP | LI_TOUCH_EVENT_CANCEL => {
                self.touch_ids.retain(|&id| id != t.pointer_id);
                sink(ev(InputKind::TouchUp, t.pointer_id, t.x, t.y));
            }
            LI_TOUCH_EVENT_CANCEL_ALL => {
                for id in self.touch_ids.drain(..) {
                    sink(ev(InputKind::TouchUp, id, 0.0, 0.0));
                }
            }
            // Touch hover has no wire representation (and BUTTON_ONLY is pen-only in
            // practice) — nothing to forward.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen(event_type: u8, x: f32, y: f32, pod: f32) -> SsPen {
        SsPen {
            event_type,
            tool: LI_TOOL_TYPE_PEN,
            buttons: 0,
            x,
            y,
            pressure_or_distance: pod,
            rotation: 270,
            tilt: 40,
        }
    }

    #[test]
    fn pen_down_move_up_maps_to_state_full_samples() {
        let mut g = GsPointer::new();
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_DOWN, 0.25, 0.5, 0.5))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE | PEN_TOUCHING);
        assert_eq!(s.pressure, 32767);
        assert_eq!((s.tilt_deg, s.azimuth_deg), (40, 270));
        assert_eq!(s.roll_deg, PEN_ANGLE_UNKNOWN);
        // Unknown contact pressure (0.0) must still ink — binary-stylus full scale.
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_MOVE, 0.3, 0.5, 0.0))
            .unwrap();
        assert_eq!(s.pressure, u16::MAX);
        // No hover ever seen: UP = fully out of range (no parked proximity).
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_UP, 0.3, 0.5, 0.0))
            .unwrap();
        assert_eq!(s.state, 0);
    }

    #[test]
    fn hover_capable_client_keeps_proximity_on_lift() {
        let mut g = GsPointer::new();
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_HOVER, 0.2, 0.2, 0.5))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE);
        assert_eq!(s.distance, 32767);
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_UP, 0.2, 0.2, 0.0))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE);
        // HOVER_LEAVE exits.
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_HOVER_LEAVE, 0.2, 0.2, 0.0))
            .unwrap();
        assert_eq!(s.state, 0);
    }

    #[test]
    fn button_only_merges_over_last_state() {
        let mut g = GsPointer::new();
        // BUTTON_ONLY before any positioned event: nothing to merge over.
        let mut b = pen(LI_TOUCH_EVENT_BUTTON_ONLY, 0.0, 0.0, 0.0);
        b.buttons = LI_PEN_BUTTON_PRIMARY;
        assert!(g.pen_sample(&b).is_none());
        // After a DOWN is applied (through the real tracker path minus the device), the
        // button packet reuses the held position/pressure.
        g.apply_pen(&pen(LI_TOUCH_EVENT_DOWN, 0.4, 0.6, 0.8));
        let s = g.pen_sample(&b).unwrap();
        assert_eq!(s.state & (PEN_BARREL1 | PEN_BARREL2), PEN_BARREL1);
        assert_eq!(s.state & PEN_TOUCHING, PEN_TOUCHING);
        assert!((s.x - 0.4).abs() < 1e-6);
        assert_eq!(s.pressure, (0.8f32 * 65535.0) as u16);
    }

    #[test]
    fn eraser_and_unknown_tools_map() {
        let mut g = GsPointer::new();
        let mut p = pen(LI_TOUCH_EVENT_DOWN, 0.5, 0.5, 1.0);
        p.tool = LI_TOOL_TYPE_ERASER;
        assert_eq!(g.pen_sample(&p).unwrap().tool, PenTool::Eraser);
        p.tool = 0x7F;
        assert_eq!(g.pen_sample(&p).unwrap().tool, PenTool::Unknown);
        // Unknown sentinel angles pass through as our sentinels.
        p.rotation = LI_ROT_UNKNOWN;
        p.tilt = LI_TILT_UNKNOWN;
        let s = g.pen_sample(&p).unwrap();
        assert_eq!(s.azimuth_deg, PEN_ANGLE_UNKNOWN);
        assert_eq!(s.tilt_deg, PEN_TILT_UNKNOWN);
    }

    #[test]
    fn touch_forwards_wire_touch_and_cancel_all_replays_ups() {
        let mut g = GsPointer::new();
        let mut got: Vec<InputEvent> = Vec::new();
        let t = |event_type, pointer_id, x: f32| SsTouch {
            event_type,
            rotation: 0,
            pointer_id,
            x,
            y: 0.5,
            pressure_or_distance: 0.5,
        };
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 7, 0.5), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 9, 0.25), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_MOVE, 7, 0.75), |e| got.push(e));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].kind, InputKind::TouchDown);
        assert_eq!(got[0].code, 7);
        assert_eq!(got[0].x, 32767);
        assert_eq!(got[0].flags, (65535 << 16) | 65535);
        assert_eq!(got[2].kind, InputKind::TouchMove);
        assert_eq!(got[2].x, 49151);
        // CANCEL_ALL lifts every tracked contact.
        got.clear();
        g.apply_touch(&t(LI_TOUCH_EVENT_CANCEL_ALL, 0, 0.0), |e| got.push(e));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|e| e.kind == InputKind::TouchUp));
        let mut ids: Vec<u32> = got.iter().map(|e| e.code).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 9]);
        // And the set is empty now — a second CANCEL_ALL is a no-op.
        got.clear();
        g.apply_touch(&t(LI_TOUCH_EVENT_CANCEL_ALL, 0, 0.0), |e| got.push(e));
        assert!(got.is_empty());
    }
}
