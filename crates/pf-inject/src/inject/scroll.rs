//! Normalized scroll ([`InputKind::Scroll`]) → per-backend primitive plans.
//!
//! Pure and target-independent: the injectors execute the ops their
//! [`ScrollMapper`] emits, so tests on any platform assert the same mapping
//! production runs. A plan is a flat op list; [`ScrollOp::Frame`] marks the
//! backend's frame/flush boundary — on ei a nonzero scroll and a stop for the
//! same axis may not share a frame.
//!
//! Sign inside a plan is backend-native: the Wayland-family backends (libei,
//! gamescope, KWin, wlroots) take positive-down on the vertical axis — the
//! wire's positive-up is negated there — while the horizontal axis and the
//! Windows wheel deltas stay positive.

use punktfunk_core::input::scroll::{
    ScrollEvent, ScrollPhase, ScrollSource, ScrollUnits, SCROLL_DIP_PER_DETENT, SCROLL_SCALE,
};
use punktfunk_core::input::InputEvent;

/// Injection backend a plan is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollBackend {
    /// libei (`reis`), portal or Mutter direct.
    Libei,
    /// gamescope's EIS socket — counts clicks, sees no stops.
    Gamescope,
    /// KWin `org_kde_kwin_fake_input` — a bare axis value, nothing else.
    Kwin,
    /// Windows `SendInput` `WHEEL`/`HWHEEL` deltas.
    Windows,
    /// wlroots `zwlr_virtual_pointer_v1`.
    Wlr,
}

/// wlroots `axis_source` the next axis ops in the frame are counted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
    Continuous,
}

/// One primitive in backend-native units and sign.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollOp {
    /// Continuous distance: ei `scroll`, wl `axis`, KWin `axis`. Logical pixels
    /// on ei/wl, the 10-per-detent unit on KWin.
    Continuous { horizontal: bool, value: f64 },
    /// Whole v120 units: ei `scroll_discrete`, Windows `mouseData`.
    Discrete120 { horizontal: bool, value: i32 },
    /// wl `axis_discrete`: surface distance plus whole detents.
    DiscreteDetents {
        horizontal: bool,
        value: f64,
        detents: i32,
    },
    /// wl `axis_source`, ahead of the axis ops it describes.
    AxisSource(AxisSource),
    /// End or cancel the axis interaction: ei `scroll_stop`, wl `axis_stop`.
    /// wl cannot tell a cancel from an end; `cancel` is informational there.
    Stop { horizontal: bool, cancel: bool },
    /// Emit the backend frame (`ei_device.frame`, wl `frame`) before more ops.
    /// Backends with no frame primitive ignore it.
    Frame,
}

/// Stateful lowering of normalized scroll events onto one backend's
/// primitives. Lives on the injector — the integer residue it keeps per axis
/// is what lets sub-detent deltas still reach a click.
pub struct ScrollMapper {
    backend: ScrollBackend,
    /// Unsent integer fraction per axis, in v120 units. A source switch or a
    /// gesture boundary clears it, so wheel residue is never repriced as DIP.
    rem: [f64; 2],
    /// Source of the last event per axis.
    last_source: [Option<ScrollSource>; 2],
    /// libei/wlr: the axis has an open continuous scroll interaction, held by
    /// this source. A different source's movement or an explicit `Begin`
    /// cancels it first — never in the same frame as the new delta.
    ongoing: [Option<ScrollSource>; 2],
}

/// Wire delta in the source's own unit — v120 for Wheel/Unknown, DIP for the
/// rest (same number, different meaning; [`ScrollEvent::units`] picks the rate).
fn delta_units(se: &ScrollEvent) -> f64 {
    f64::from(se.delta) / SCROLL_SCALE
}

impl ScrollMapper {
    pub fn new(backend: ScrollBackend) -> Self {
        ScrollMapper {
            backend,
            rem: [0.0; 2],
            last_source: [None; 2],
            ongoing: [None; 2],
        }
    }

    /// Ops for `ev`, in wire order. Empty when the event is malformed or the
    /// backend has nothing to say for it (a dropped phase).
    pub fn plan(&mut self, ev: &InputEvent) -> Vec<ScrollOp> {
        let Some(se) = ScrollEvent::from_event(ev) else {
            return Vec::new();
        };
        let a = se.axis as usize;
        // A host that owns kinetic gets no client tail for finger sources.
        // Dropped before any state moves: a reordered momentum cannot cancel
        // the next gesture's open interaction nor clear its residue.
        if matches!(self.backend, ScrollBackend::Libei | ScrollBackend::Wlr)
            && matches!(se.source, ScrollSource::Finger | ScrollSource::Touch)
            && se.is_momentum()
        {
            return Vec::new();
        }
        // A stale stop from a different source must not close the live
        // interaction or its residue.
        if se.is_stop() && self.ongoing[a].is_some_and(|open| open != se.source) {
            return Vec::new();
        }
        // A source switch or any gesture boundary restarts the residue — a
        // missed stop cannot leak last gesture's fraction into the new one.
        if self.last_source[a] != Some(se.source)
            || se.is_stop()
            || matches!(se.phase, ScrollPhase::Begin | ScrollPhase::MomentumBegin)
        {
            self.rem[a] = 0.0;
        }
        self.last_source[a] = Some(se.source);
        match self.backend {
            ScrollBackend::Libei | ScrollBackend::Wlr => self.native(se),
            ScrollBackend::Gamescope | ScrollBackend::Windows => self.counted(se),
            ScrollBackend::Kwin => self.kwin(se),
        }
    }

    /// Stops for every axis still mid-interaction, clearing all state. Emit on
    /// teardown so a compositor is not left holding a live gesture.
    pub fn cancel_all(&mut self) -> Vec<ScrollOp> {
        let mut ops = Vec::new();
        for (a, on) in self.ongoing.iter().enumerate() {
            if on.is_some() {
                ops.push(ScrollOp::Stop {
                    horizontal: a == 1,
                    cancel: true,
                });
                ops.push(ScrollOp::Frame);
            }
        }
        *self = ScrollMapper::new(self.backend);
        ops
    }

    /// Integer v120 the backend owes this axis now; the fraction stays in `rem`.
    fn take_v120(&mut self, a: usize, v120: f64) -> i32 {
        let total = (self.rem[a] + v120).clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let whole = total.trunc();
        self.rem[a] = total - whole;
        whole as i32
    }

    /// Whole v120 clicks the click-counter backends owe this axis now: wheel
    /// units pass through, DIP re-prices at [`SCROLL_DIP_PER_DETENT`] per
    /// detent.
    fn clicks_v120(&mut self, a: usize, se: &ScrollEvent) -> i32 {
        let v = match se.units() {
            ScrollUnits::V120 => delta_units(se),
            ScrollUnits::Dip => delta_units(se) * (120.0 / SCROLL_DIP_PER_DETENT),
        };
        self.take_v120(a, v)
    }

    /// An open interaction belongs to one source: a different source's
    /// movement — wheel clicks included — or an explicit `Begin` first cancels
    /// it, in its own frame. ei and wl both forbid a stop sharing a frame with
    /// a nonzero delta on the axis.
    fn cancel_open(&mut self, a: usize, horizontal: bool, se: &ScrollEvent) -> Vec<ScrollOp> {
        match self.ongoing[a] {
            Some(open) if open != se.source || se.phase == ScrollPhase::Begin => {
                self.ongoing[a] = None;
                vec![
                    ScrollOp::Stop {
                        horizontal,
                        cancel: true,
                    },
                    ScrollOp::Frame,
                ]
            }
            _ => Vec::new(),
        }
    }

    fn native(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        let a = se.axis as usize;
        let h = se.axis == 1;
        if se.is_stop() {
            return self.native_stop(se);
        }
        if self.backend == ScrollBackend::Wlr && se.is_momentum() && se.delta == 0 {
            return Vec::new();
        }
        let mut ops = self.cancel_open(a, h, &se);
        if se.is_wheel() {
            match self.backend {
                ScrollBackend::Libei => self.libei_wheel(se, &mut ops),
                ScrollBackend::Wlr => self.wlr_wheel(se, &mut ops),
                _ => unreachable!(),
            }
        } else if se.delta != 0 {
            self.native_distance(se, &mut ops);
        }
        if !ops.is_empty() && !matches!(ops.last(), Some(ScrollOp::Frame)) {
            ops.push(ScrollOp::Frame);
        }
        ops
    }

    fn native_stop(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        self.ongoing[se.axis as usize] = None;
        // libei cancels non-finger tails; wlroots has no distinct cancel primitive.
        let cancel = se.phase == ScrollPhase::Cancel
            || (self.backend == ScrollBackend::Libei
                && matches!(
                    se.source,
                    ScrollSource::Continuous | ScrollSource::Controller
                ));
        vec![
            ScrollOp::Stop {
                horizontal: se.axis == 1,
                cancel,
            },
            ScrollOp::Frame,
        ]
    }

    fn native_distance(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        if self.backend == ScrollBackend::Wlr {
            ops.push(ScrollOp::AxisSource(
                if matches!(se.source, ScrollSource::Finger | ScrollSource::Touch) {
                    AxisSource::Finger
                } else {
                    AxisSource::Continuous
                },
            ));
        }
        let d = delta_units(&se);
        ops.push(ScrollOp::Continuous {
            horizontal: se.axis == 1,
            value: if se.axis == 1 { d } else { -d },
        });
        self.ongoing[se.axis as usize] = Some(se.source);
    }

    fn libei_wheel(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        let h = se.axis == 1;
        let disc = self.take_v120(se.axis as usize, delta_units(&se));
        if disc != 0 {
            ops.push(ScrollOp::Discrete120 {
                horizontal: h,
                value: if h { disc } else { -disc },
            });
        }
        if se.delta != 0 {
            // libinput's 15 px per detent beside the clicks.
            let px = delta_units(&se) * (15.0 / 120.0);
            ops.push(ScrollOp::Continuous {
                horizontal: h,
                value: if h { px } else { -px },
            });
        }
    }

    fn wlr_wheel(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        let a = se.axis as usize;
        let h = se.axis == 1;
        // No axis_value120 on this protocol: accumulate whole detents before emitting clicks.
        self.rem[a] += delta_units(&se);
        let steps = (self.rem[a] / 120.0).trunc() as i32;
        if steps == 0 {
            return;
        }
        self.rem[a] -= f64::from(steps) * 120.0;
        let signed = if h { steps } else { -steps };
        ops.extend([
            ScrollOp::AxisSource(AxisSource::Wheel),
            ScrollOp::DiscreteDetents {
                horizontal: h,
                value: f64::from(signed) * 15.0,
                detents: signed,
            },
        ]);
    }

    fn counted(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        if se.is_stop() {
            return Vec::new();
        }
        // Integer wheel APIs have no stop primitive. The OS applies its own wheel preferences.
        let disc = self.clicks_v120(se.axis as usize, &se);
        if disc == 0 {
            return Vec::new();
        }
        let value = if self.backend == ScrollBackend::Gamescope && se.axis == 0 {
            -disc
        } else {
            disc
        };
        vec![
            ScrollOp::Discrete120 {
                horizontal: se.axis == 1,
                value,
            },
            ScrollOp::Frame,
        ]
    }

    fn kwin(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        if se.is_stop() {
            return Vec::new(); // no stop primitive; the glide is the app's own
        }
        // `fake_input` axis units are 10 per detent — v120 *10/120, DIP at
        // 10 per [`SCROLL_DIP_PER_DETENT`] — and it takes a double, so nothing
        // truncates early.
        let v = match se.units() {
            ScrollUnits::V120 => delta_units(&se) * (10.0 / 120.0),
            ScrollUnits::Dip => delta_units(&se) * (10.0 / SCROLL_DIP_PER_DETENT),
        };
        if v == 0.0 {
            return Vec::new();
        }
        vec![
            ScrollOp::Continuous {
                horizontal: se.axis == 1,
                value: if se.axis == 1 { v } else { -v },
            },
            ScrollOp::Frame,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::InputKind;

    fn ev(source: ScrollSource, phase: ScrollPhase, axis: u32, delta: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis,
            delta: (delta * SCROLL_SCALE) as i32,
        }
        .to_event()
    }

    fn plan(backend: ScrollBackend, events: &[InputEvent]) -> Vec<ScrollOp> {
        let mut m = ScrollMapper::new(backend);
        let mut ops = Vec::new();
        for e in events {
            ops.extend(m.plan(e));
        }
        ops
    }

    const V: bool = false;
    const H: bool = true;

    #[test]
    fn zero_momentum_keeps_backend_cancel_behavior() {
        let stop = vec![
            ScrollOp::Stop {
                horizontal: false,
                cancel: true,
            },
            ScrollOp::Frame,
        ];
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let mut mapper = ScrollMapper::new(backend);
            mapper.plan(&ev(ScrollSource::Controller, ScrollPhase::Update, 0, 5.0));
            let ops = mapper.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::MomentumBegin,
                0,
                0.0,
            ));
            if backend == ScrollBackend::Libei {
                assert_eq!(ops, stop);
                assert!(mapper.cancel_all().is_empty());
            } else {
                assert!(ops.is_empty());
                assert_eq!(mapper.cancel_all(), stop);
            }
        }
    }

    #[test]
    fn continuous_momentum_end_stop_policy() {
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            assert_eq!(
                plan(
                    backend,
                    &[ev(
                        ScrollSource::Continuous,
                        ScrollPhase::MomentumEnd,
                        0,
                        0.0
                    )]
                ),
                vec![
                    ScrollOp::Stop {
                        horizontal: false,
                        cancel: backend == ScrollBackend::Libei
                    },
                    ScrollOp::Frame
                ]
            );
        }
    }

    #[test]
    fn libei_wheel_pairs_clicks_with_pixels() {
        let ops = plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -120
                },
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -15.0
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn libei_unknown_fraction_and_finger_dip() {
        let ops = plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Unknown, ScrollPhase::None, 0, 0.25)],
        );
        // 0.25 v120 carries no whole unit: pixels only, residue stays.
        assert_eq!(
            ops,
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -0.03125
                },
                ScrollOp::Frame,
            ]
        );
        let ops = plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 0, 60.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -60.0
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn horizontal_keeps_sign_and_axis() {
        // True continuous channels carry the DIP distance as-is.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let ops = plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
            );
            assert!(
                ops.iter().any(|o| matches!(
                    o,
                    ScrollOp::Continuous { horizontal: H, value } if *value == -10.0
                )),
                "{backend:?} horizontal finger: {ops:?}"
            );
        }
        // The click-priced channels reprice instead: KWin at 10 units/60 DIP,
        // gamescope at 120 v120/60 DIP — same axis, unnegated sign.
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
        );
        assert!(ops.iter().any(|o| matches!(
            o,
            ScrollOp::Continuous { horizontal: H, value } if (*value + 10.0 / 6.0).abs() < 1e-9
        )));
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -20
        }));
        // Horizontal does not negate.
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 1, -120.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -120
        }));
        let ops = plan(
            ScrollBackend::Windows,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 1, -120.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -120
        }));
    }

    #[test]
    fn stops_by_backend() {
        // libei: Finger ends clean, Continuous/Controller end cancelled,
        // Cancel is always a cancel — and a zero-delta stop is not dropped.
        for (source, phase, cancel) in [
            (ScrollSource::Finger, ScrollPhase::End, false),
            (ScrollSource::Touch, ScrollPhase::End, false),
            (ScrollSource::Continuous, ScrollPhase::End, true),
            (ScrollSource::Controller, ScrollPhase::End, true),
            (ScrollSource::Finger, ScrollPhase::Cancel, true),
        ] {
            let ops = plan(ScrollBackend::Libei, &[ev(source, phase, 0, 0.0)]);
            assert_eq!(
                ops,
                vec![
                    ScrollOp::Stop {
                        horizontal: V,
                        cancel
                    },
                    ScrollOp::Frame,
                ],
                "{source:?} {phase:?}"
            );
        }
        // wlr: a stop emits axis_stop + frame; cancel is informational only.
        let ops = plan(
            ScrollBackend::Wlr,
            &[ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: false
                },
                ScrollOp::Frame,
            ]
        );
        // gamescope/kwin/windows: stops are no-ops but still clear residue.
        for backend in [
            ScrollBackend::Gamescope,
            ScrollBackend::Kwin,
            ScrollBackend::Windows,
        ] {
            assert!(plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0)]
            )
            .is_empty());
        }
    }

    #[test]
    fn libei_begin_cancels_an_open_interaction() {
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0));
        // A new Begin on the same axis cancels the stale interaction first,
        // in its own frame — ei forbids scroll + stop on one axis per frame.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 1.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -1.0
                },
                ScrollOp::Frame,
            ]
        );
        // The other axis has no interaction: a Begin there stays quiet.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 1, 0.0));
        assert!(ops.is_empty());
    }

    #[test]
    fn source_change_cancels_the_open_axis() {
        // A finger gesture still open, then wheel clicks: the interaction
        // closes in its own frame before the first click ops — ei/wl forbid a
        // stop sharing a frame with a nonzero delta on the axis.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -120
                },
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -15.0
                },
                ScrollOp::Frame,
            ]
        );
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::AxisSource(AxisSource::Wheel),
                ScrollOp::DiscreteDetents {
                    horizontal: V,
                    value: -15.0,
                    detents: -1
                },
                ScrollOp::Frame,
            ]
        );
        // Between continuous sources the same rule holds.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Touch, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0));
        assert_eq!(
            &ops[..2],
            &[
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn dropped_momentum_does_not_touch_open_interaction() {
        // A controller gesture open; a finger momentum tail (stale or
        // reordered) is dropped before any state moves on ei/wl.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Controller, ScrollPhase::Begin, 0, 10.0));
            assert!(m
                .plan(&ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 5.0))
                .is_empty());
            assert!(m
                .plan(&ev(ScrollSource::Finger, ScrollPhase::MomentumEnd, 0, 0.0))
                .is_empty());
            // The controller interaction is still open: teardown cancels it.
            let ops = m.cancel_all();
            assert!(
                ops.contains(&ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                }),
                "{backend:?}: {ops:?}"
            );
        }
        // On click-counter backends finger momentum still counts distance.
        for backend in [ScrollBackend::Gamescope, ScrollBackend::Windows] {
            let ops = plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 5.0)],
            );
            assert!(
                ops.iter()
                    .any(|o| matches!(o, ScrollOp::Discrete120 { value: v, .. } if v.abs() == 10)),
                "{backend:?}: {ops:?}"
            );
        }
    }

    #[test]
    fn stale_stop_does_not_close_other_source() {
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0));
        // A Touch End while the finger gesture is open is stale: no stop
        // emitted, and the finger interaction stays open.
        assert!(m
            .plan(&ev(ScrollSource::Touch, ScrollPhase::End, 0, 0.0))
            .is_empty());
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: false
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn cancel_all_closes_open_axes() {
        // Both interactive backends: two open axes stop in order; a second
        // teardown finds nothing.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
            m.plan(&ev(ScrollSource::Touch, ScrollPhase::Update, 1, 10.0));
            let ops = m.cancel_all();
            assert_eq!(
                ops,
                vec![
                    ScrollOp::Stop {
                        horizontal: V,
                        cancel: true
                    },
                    ScrollOp::Frame,
                    ScrollOp::Stop {
                        horizontal: H,
                        cancel: true
                    },
                    ScrollOp::Frame,
                ],
                "{backend:?}: {ops:?}"
            );
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
        // Plain wheel clicks never open an interaction — nothing to cancel.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Wlr,
            ScrollBackend::Gamescope,
            ScrollBackend::Kwin,
            ScrollBackend::Windows,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
    }

    #[test]
    fn momentum_routes_per_source_and_backend() {
        // Finger/Touch momentum is dropped on ei and wl — the host owns the
        // kinetic tail after a clean stop.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            assert!(plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 5.0)]
            )
            .is_empty());
            assert!(plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::MomentumEnd, 0, 0.0)]
            )
            .is_empty());
        }
        // Continuous drives its own tail: forwarded on ei, finished cancelled.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        let ops = m.plan(&ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0));
        assert!(ops.contains(&ScrollOp::Continuous {
            horizontal: V,
            value: -5.0
        }));
        let ops = m.plan(&ev(
            ScrollSource::Continuous,
            ScrollPhase::MomentumEnd,
            0,
            0.0,
        ));
        assert!(ops.contains(&ScrollOp::Stop {
            horizontal: V,
            cancel: true
        }));
        // Click counters forward momentum deltas as ordinary scroll (5 DIP
        // → 10 v120; ei sign on gamescope, wire sign on Windows).
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: -10
        }));
        let ops = plan(
            ScrollBackend::Windows,
            &[ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: 10
        }));
        // wlr forwards continuous momentum as Continuous-sourced axis deltas.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        let ops = m.plan(&ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::AxisSource(AxisSource::Continuous),
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -5.0
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn four_quarter_detents() {
        // 30 v120 × 4: libei/gamescope/windows emit 30 every event; wlr holds
        // sub-detent deltas until they complete a click.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Gamescope,
            ScrollBackend::Windows,
        ] {
            let mut m = ScrollMapper::new(backend);
            for _ in 0..4 {
                let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0));
                assert!(
                    ops.contains(&ScrollOp::Discrete120 {
                        horizontal: V,
                        value: -30
                    }) || ops.contains(&ScrollOp::Discrete120 {
                        horizontal: V,
                        value: 30
                    }),
                    "{backend:?}: {ops:?}"
                );
            }
        }
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        for _ in 0..3 {
            assert!(m
                .plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0))
                .is_empty());
        }
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::AxisSource(AxisSource::Wheel),
                ScrollOp::DiscreteDetents {
                    horizontal: V,
                    value: -15.0,
                    detents: -1
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn continuous_tenth_dip_adds_up() {
        // ~0.1 DIP (wire 26) × 10 ≈ 1 DIP = 2 v120: ei/wl keep native floats
        // each event; the integer backends emit the whole units they accumulate.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        for _ in 0..10 {
            let ops = m.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::Update,
                0,
                26.0 / 256.0,
            ));
            assert!(ops.contains(&ScrollOp::Continuous {
                horizontal: V,
                value: -(26.0 / 256.0)
            }));
            assert!(!ops
                .iter()
                .any(|o| matches!(o, ScrollOp::Discrete120 { .. })));
        }
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        for _ in 0..10 {
            let ops = m.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::Update,
                0,
                26.0 / 256.0,
            ));
            assert!(ops.contains(&ScrollOp::Continuous {
                horizontal: V,
                value: -(26.0 / 256.0)
            }));
            assert!(!ops
                .iter()
                .any(|o| matches!(o, ScrollOp::DiscreteDetents { .. })));
        }
        for backend in [ScrollBackend::Gamescope, ScrollBackend::Windows] {
            let mut m = ScrollMapper::new(backend);
            let mut total = 0;
            for _ in 0..10 {
                for op in m.plan(&ev(
                    ScrollSource::Continuous,
                    ScrollPhase::Update,
                    0,
                    26.0 / 256.0,
                )) {
                    if let ScrollOp::Discrete120 { value, .. } = op {
                        total += value;
                    }
                }
            }
            // 10 × 26 wire = 1.015625 DIP → 2.03125 v120 → 2 whole units out.
            assert_eq!(total.abs(), 2, "{backend:?}");
        }
    }

    #[test]
    fn kwin_table() {
        // Wheel 120 v120 → 10 units; Finger 60 DIP → 10 units; doubles, no stops.
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -10.0
                },
                ScrollOp::Frame,
            ]
        );
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 0, 60.0)],
        );
        assert!(ops.contains(&ScrollOp::Continuous {
            horizontal: V,
            value: -10.0
        }));
    }

    #[test]
    fn residue_is_per_axis_and_source() {
        let mut m = ScrollMapper::new(ScrollBackend::Windows);
        // 0.5 v120 vertical held, then a horizontal event emits on its own.
        assert!(m
            .plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 0.5))
            .is_empty());
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 1, 120.0));
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: 120
        }));
        // A source switch on axis 0 must not reprice the wheel residue: the
        // finger delta lands whole.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: 20
        }));
    }

    #[test]
    fn malformed_events_plan_nothing() {
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        // Wrong tag, bad axis, phased wheel, stop with distance.
        assert!(m
            .plan(&InputEvent {
                kind: InputKind::MouseScroll,
                _pad: [0; 3],
                code: 0,
                x: 120,
                y: 0,
                flags: 0,
            })
            .is_empty());
        assert!(m
            .plan(&ev(ScrollSource::Wheel, ScrollPhase::Begin, 0, 0.0))
            .is_empty());
        let mut bad = ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0);
        bad.code = 9;
        assert!(m.plan(&bad).is_empty());
    }
}
