//! The IDD-push tick: the wrapper's order around the remote source's phases, and what it
//! decides between them.
//!
//! [`Tick::step`] takes what the shell sampled or was answered ([`Inputs`]) and returns what
//! it runs next, in order ([`Action`]). `Sample`, `Supervise` and `Admit` each ask for the
//! next input; a parked rung or a refused admit ends the tick where it stands. The source
//! keeps the delivery gate and the supervisor; this keeps the recover-or-drop window, the
//! heartbeat maximum between fresh frames and the stall watch.
//!
//! Pure, so the table below runs on every target. A row per tick defect that reached a user.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::time::{Duration, Instant};

use crate::windows::stall_model::{Recovery, StallWatch};

/// A presentation restart that composes no frame within this ends the session.
pub(crate) const RECOVER_OR_DROP: Duration = Duration::from_secs(3);

/// The session's end when a presentation restart never resumes.
pub(crate) const NO_FRAME_AFTER_RESTART: &str = "IDD-push: the display was restarted in place \
     and no frame followed within 3s — dropping the session so the client reconnects";

/// A dead WUDFHost and an idle desktop both stop the source counter, so the death probe runs
/// only past this much staleness.
const STALE: Duration = Duration::from_secs(2);
/// At most one 0 ms wait per this.
const PROBE_EVERY: Duration = Duration::from_secs(1);

/// Whether the driver-death watch probes WUDFHost now: stale past [`STALE`], and the last
/// probe older than [`PROBE_EVERY`].
pub(crate) fn driver_probe_due(now: Instant, last_fresh: Instant, probed: Instant) -> bool {
    now.saturating_duration_since(last_fresh) > STALE
        && now.saturating_duration_since(probed) > PROBE_EVERY
}

/// What the cursor witness holds this tick, and whether a hardware cursor is declared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// Under a declared IddCx cursor DWM composes nothing for pointer travel: no witness for
    /// the ladder or a stall.
    pub(crate) declared: bool,
    pub(crate) gap_px: u32,
    /// `None` before the witness's first read.
    pub(crate) moved_px: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inputs {
    /// A tick begins.
    Begin,
    /// The display's pollers ran: the clock, whether the source owns a recovery episode, and
    /// the cursor witness.
    Sampled {
        now: Instant,
        owns_episode: bool,
        cursor: Cursor,
    },
    /// The supervisor ran; the drain heartbeat's age after it, `None` before its first.
    Supervised { heartbeat_age: Option<Duration> },
    /// The gate admitted a delivery at `now`; the encoder's present→arrival and whether the
    /// drain worker has stamped a heartbeat.
    Admitted {
        now: Instant,
        present_to_arrival: Option<Duration>,
        heartbeat_seen: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// The typed end the ladder decided off the capture path: `Err` ends the tick.
    TakeFault,
    /// The cursor declare's secure-desktop edge. UAC may compose nothing until it runs.
    PollSecureDesktop,
    /// One cursor-witness read, before any gate, so every gap shape accumulates travel.
    SampleWitness,
    /// The display depth pin; a re-open reaches the loop through the delivered geometry.
    PollDepth,
    /// Read the clock, the source's episode and the witness: step [`Inputs::Sampled`].
    Sample,
    EndSession(&'static str),
    /// The source's far-side news, the driver-death watch among it. A parked rung ends the tick.
    ProbeDriver,
    /// One supervisor tick with these local inputs: step [`Inputs::Supervised`].
    Supervise {
        cursor_gap_px: u32,
        recreating: bool,
    },
    /// The delivery gate. A refusal ends the tick; a delivery steps [`Inputs::Admitted`].
    Admit,
    /// A hole after active flow. The shell adds the probe and ETW windows and reports it.
    ReportStall {
        gap: Duration,
        max_heartbeat_age_ms: Option<u64>,
        cursor_moved_px: Option<u32>,
    },
    /// A degraded stretch closed: its one summary line.
    DegradedStretch(Recovery),
    /// The opt-in present-flow line, if one is due.
    FlowLine,
    Deliver,
    /// The fresh frame ends the witness's gap.
    ResetWitness,
}

/// Every tick opens with these, whatever ends it.
pub(crate) const OPENING: [Action; 5] = [
    Action::TakeFault,
    Action::PollSecureDesktop,
    Action::SampleWitness,
    Action::PollDepth,
    Action::Sample,
];

pub(crate) struct Tick {
    /// A presentation restart is in flight since then. The rungs and the loop's restart set
    /// it; a fresh frame clears it.
    pub(crate) recovering_since: Option<Instant>,
    /// Holes in DWM delivery during active flow. The shell writes its reports.
    pub(crate) stall_watch: StallWatch,
    /// The stalest drain heartbeat (µs) since the last fresh frame.
    max_hb_age_us: u64,
    /// This tick's witness sample.
    cursor: Cursor,
}

impl Tick {
    pub(crate) fn new() -> Self {
        Self {
            recovering_since: None,
            stall_watch: StallWatch::new(),
            max_hb_age_us: 0,
            cursor: Cursor::default(),
        }
    }

    pub(crate) fn step(&mut self, input: Inputs) -> Vec<Action> {
        match input {
            Inputs::Begin => Vec::from(OPENING),
            Inputs::Sampled {
                now,
                owns_episode,
                cursor,
            } => {
                // The restart's own mode commit composes a frame; an episode's deadlines are
                // the ladder's.
                let expired = self
                    .recovering_since
                    .is_some_and(|since| now.saturating_duration_since(since) > RECOVER_OR_DROP);
                if expired && !owns_episode {
                    return vec![Action::EndSession(NO_FRAME_AFTER_RESTART)];
                }
                self.cursor = cursor;
                vec![
                    Action::ProbeDriver,
                    Action::Supervise {
                        cursor_gap_px: if cursor.declared { 0 } else { cursor.gap_px },
                        recreating: self.recovering_since.is_some(),
                    },
                ]
            }
            Inputs::Supervised { heartbeat_age } => {
                // A heartbeat quiet through the hole convicts the worker; one fresh through it
                // indicts the compose path.
                if let Some(age) = heartbeat_age {
                    self.max_hb_age_us = self.max_hb_age_us.max(age.as_micros() as u64);
                }
                vec![Action::Admit]
            }
            Inputs::Admitted {
                now,
                present_to_arrival,
                heartbeat_seen,
            } => self.delivered(now, present_to_arrival, heartbeat_seen),
        }
    }

    /// A restart's gap is self-inflicted, so its frame resets the stall watch instead of
    /// reporting. An age of 0 before the first heartbeat would acquit a worker that never ran.
    fn delivered(
        &mut self,
        now: Instant,
        present_to_arrival: Option<Duration>,
        heartbeat_seen: bool,
    ) -> Vec<Action> {
        let arrival_ms = present_to_arrival.map(|d| d.as_millis() as u64);
        let mut actions = Vec::new();
        if self.recovering_since.take().is_some() {
            self.stall_watch.reset();
        } else if let Some(stall) = self.stall_watch.note_fresh(now, arrival_ms) {
            actions.push(Action::ReportStall {
                gap: stall.gap,
                max_heartbeat_age_ms: heartbeat_seen.then_some(self.max_hb_age_us / 1_000),
                cursor_moved_px: if self.cursor.declared {
                    Some(0)
                } else {
                    self.cursor.moved_px
                },
            });
        }
        actions.extend(
            self.stall_watch
                .take_recovery()
                .map(Action::DegradedStretch),
        );
        self.max_hb_age_us = 0;
        actions.extend([Action::FlowLine, Action::Deliver, Action::ResetWitness]);
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::*;
    use std::collections::VecDeque;

    /// One tick as the shell answers it, `at` ms into the row.
    #[derive(Clone, Copy)]
    struct Tk {
        at: u64,
        /// The source owns a recovery episode.
        episode: bool,
        /// The far side's news parked a rung.
        parked: bool,
        /// The supervisor ran a presentation restart.
        restart: bool,
        /// The drain heartbeat's age after the supervisor, ms.
        hb: Option<u64>,
        /// The gate admits.
        news: bool,
    }

    const fn quiet(at: u64) -> Tk {
        Tk {
            at,
            episode: false,
            parked: false,
            restart: false,
            hb: Some(4),
            news: false,
        }
    }

    const fn fresh(at: u64) -> Tk {
        Tk {
            news: true,
            ..quiet(at)
        }
    }

    /// Eight frames 16 ms apart: active flow.
    const FLOW: [Tk; 8] = [
        fresh(0),
        fresh(16),
        fresh(32),
        fresh(48),
        fresh(64),
        fresh(80),
        fresh(96),
        fresh(112),
    ];
    /// Eight frames 500 ms apart: a caret blinking on an idle desktop.
    const IDLE: [Tk; 8] = [
        fresh(0),
        fresh(500),
        fresh(1000),
        fresh(1500),
        fresh(2000),
        fresh(2500),
        fresh(3000),
        fresh(3500),
    ];

    const STILL: Cursor = Cursor {
        declared: false,
        gap_px: 0,
        moved_px: Some(0),
    };
    const MOVING: Cursor = Cursor {
        declared: false,
        gap_px: 40,
        moved_px: Some(40),
    };
    const HARDWARE: Cursor = Cursor {
        declared: true,
        gap_px: 40,
        moved_px: Some(40),
    };

    const fn supervise(cursor_gap_px: u32, recreating: bool) -> Action {
        Supervise {
            cursor_gap_px,
            recreating,
        }
    }

    const fn stall(gap_ms: u64, max_heartbeat_age_ms: Option<u64>, moved: Option<u32>) -> Action {
        ReportStall {
            gap: Duration::from_millis(gap_ms),
            max_heartbeat_age_ms,
            cursor_moved_px: moved,
        }
    }

    /// Step one whole tick, answering as the shell would, and return what follows the opening.
    fn play(tick: &mut Tick, base: Instant, cursor: Cursor, tk: Tk) -> Vec<Action> {
        assert_eq!(
            tick.step(Inputs::Begin),
            OPENING,
            "the opening at {}",
            tk.at
        );
        let now = base + Duration::from_millis(tk.at);
        let mut out = Vec::new();
        let mut next = Some(Inputs::Sampled {
            now,
            owns_episode: tk.episode,
            cursor,
        });
        while let Some(input) = next.take() {
            for action in tick.step(input) {
                let ends = matches!(action, ProbeDriver if tk.parked)
                    || matches!(action, Admit if !tk.news);
                match action {
                    Supervise { .. } => {
                        if tk.restart {
                            tick.recovering_since.get_or_insert(now);
                        }
                        next = Some(Inputs::Supervised {
                            heartbeat_age: tk.hb.map(Duration::from_millis),
                        });
                    }
                    Admit if tk.news => {
                        next = Some(Inputs::Admitted {
                            now,
                            present_to_arrival: None,
                            heartbeat_seen: tk.hb.is_some(),
                        });
                    }
                    _ => {}
                }
                out.push(action);
                if ends {
                    return out;
                }
            }
        }
        out
    }

    struct Row {
        pins: &'static str,
        cursor: Cursor,
        /// Ticks played first; only their openings are checked.
        given: &'static [&'static [Tk]],
        then: Tk,
        want: &'static [Action],
    }

    /// A tick the gate refuses: nothing after it runs.
    const QUIET: &[Action] = &[ProbeDriver, supervise(0, false), Admit];
    const DELIVERS: &[Action] = &[
        ProbeDriver,
        supervise(0, false),
        Admit,
        FlowLine,
        Deliver,
        ResetWitness,
    ];

    // 80e487b57 (a new AU restarts the assembly) lives in the AU assembly and is pinned by
    // `au_assembly`'s `a_first_mid_assembly_drops_the_prefix`; 8c4d9259e (a pointer hidden
    // before any shape) lives in the cursor section's reader. Neither is the tick's.
    const TABLE: &[Row] = &[
        Row {
            pins: "abad24788: the secure desktop is polled before every gate, a parked rung's too",
            cursor: STILL,
            given: &[],
            then: Tk {
                parked: true,
                ..quiet(0)
            },
            want: &[ProbeDriver],
        },
        Row {
            pins: "a presentation restart that never resumes ends the session past 3 s",
            cursor: STILL,
            given: &[&[Tk {
                restart: true,
                ..quiet(0)
            }]],
            then: quiet(3001),
            want: &[EndSession(NO_FRAME_AFTER_RESTART)],
        },
        Row {
            pins: "the recover-or-drop window holds through 3 s",
            cursor: STILL,
            given: &[&[Tk {
                restart: true,
                ..quiet(0)
            }]],
            then: quiet(3000),
            want: &[ProbeDriver, supervise(0, true), Admit],
        },
        Row {
            pins: "an episode the source owns stands the recover-or-drop window down",
            cursor: STILL,
            given: &[&[Tk {
                restart: true,
                ..quiet(0)
            }]],
            then: Tk {
                episode: true,
                ..quiet(5000)
            },
            want: &[ProbeDriver, supervise(0, true), Admit],
        },
        Row {
            pins: "a frame inside the recover-or-drop window resets it",
            cursor: STILL,
            given: &[&[
                Tk {
                    restart: true,
                    ..quiet(0)
                },
                fresh(1000),
            ]],
            then: quiet(5000),
            want: QUIET,
        },
        Row {
            pins: "a presentation restart's gap is no stall",
            cursor: STILL,
            given: &[
                &FLOW,
                &[Tk {
                    restart: true,
                    ..quiet(200)
                }],
            ],
            then: fresh(412),
            want: &[
                ProbeDriver,
                supervise(0, true),
                Admit,
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "a restart this tick's ladder ran already exempts this tick's frame",
            cursor: STILL,
            given: &[&FLOW],
            then: Tk {
                restart: true,
                ..fresh(412)
            },
            want: DELIVERS,
        },
        Row {
            pins: "a delivery only on news: a refusal runs no stall watch, flow line or witness",
            cursor: STILL,
            given: &[&FLOW],
            then: quiet(128),
            want: QUIET,
        },
        Row {
            pins: "a refused tick feeds the stall watch nothing, so the hole spans it",
            cursor: STILL,
            given: &[&FLOW, &[quiet(200), quiet(300)]],
            then: fresh(412),
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(300, Some(4), Some(0)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "the stall watch's per-hole line gates on prior active flow",
            cursor: STILL,
            given: &[&IDLE],
            then: fresh(3800),
            want: DELIVERS,
        },
        Row {
            pins: "the heartbeat maximum between fresh frames is the stall's evidence",
            cursor: STILL,
            given: &[
                &FLOW,
                &[
                    Tk {
                        hb: Some(400),
                        ..quiet(200)
                    },
                    Tk {
                        hb: Some(20),
                        ..quiet(300)
                    },
                ],
            ],
            then: fresh(412),
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(300, Some(400), Some(0)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "a fresh frame restarts the heartbeat maximum",
            cursor: STILL,
            given: &[
                &[Tk {
                    hb: Some(900),
                    ..quiet(0)
                }],
                &[
                    fresh(1000),
                    fresh(1016),
                    fresh(1032),
                    fresh(1048),
                    fresh(1064),
                    fresh(1080),
                    fresh(1096),
                    fresh(1112),
                ],
            ],
            then: fresh(1412),
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(300, Some(4), Some(0)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "no heartbeat yet is no evidence, not an age of 0",
            cursor: STILL,
            given: &[&FLOW],
            then: Tk {
                hb: None,
                ..fresh(412)
            },
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(300, None, Some(0)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "travel through a hole is damage for the ladder and the stall line",
            cursor: MOVING,
            given: &[&FLOW],
            then: fresh(412),
            want: &[
                ProbeDriver,
                supervise(40, false),
                Admit,
                stall(300, Some(4), Some(40)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "6ffd18548: a hardware cursor is no stall witness, nor the ladder's",
            cursor: HARDWARE,
            given: &[&FLOW],
            then: fresh(412),
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(300, Some(4), Some(0)),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "the degraded-stretch line rides the frame that closes it, after its stall line",
            cursor: STILL,
            given: &[
                &FLOW,
                &[
                    fresh(612),
                    fresh(1112),
                    fresh(1128),
                    fresh(1144),
                    fresh(1160),
                    fresh(1176),
                    fresh(1192),
                    fresh(1208),
                    fresh(1224),
                ],
            ],
            then: fresh(21224),
            want: &[
                ProbeDriver,
                supervise(0, false),
                Admit,
                stall(20000, Some(4), Some(0)),
                DegradedStretch(Recovery {
                    degraded: Duration::from_millis(1000),
                    holes: 2,
                    hole_time: Duration::from_millis(1000),
                    worst: Duration::from_millis(500),
                    arrival_last_ms: 0,
                    arrival_mean_ms: 0,
                    arrival_max_ms: 0,
                    arrival_n: 0,
                }),
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
        Row {
            pins: "the witness resets on a fresh frame, after the delivery",
            cursor: MOVING,
            given: &[],
            then: fresh(0),
            want: &[
                ProbeDriver,
                supervise(40, false),
                Admit,
                FlowLine,
                Deliver,
                ResetWitness,
            ],
        },
    ];

    #[test]
    fn each_tick_fix_holds() {
        for row in TABLE {
            let base = Instant::now();
            let mut tick = Tick::new();
            for &tk in row.given.iter().flat_map(|g| g.iter()) {
                play(&mut tick, base, row.cursor, tk);
            }
            let got = play(&mut tick, base, row.cursor, row.then);
            assert_eq!(got, row.want, "{}", row.pins);
        }
    }

    #[test]
    fn the_driver_death_probe_runs_only_while_stale_and_once_a_second() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        for (now, fresh, probed, due, why) in [
            (
                2000,
                0,
                0,
                false,
                "two seconds without a frame is not yet stale",
            ),
            (2001, 0, 0, true, "past two seconds the probe runs"),
            (2001, 0, 1001, false, "a probe a second ago holds the next"),
            (2002, 0, 1001, true, "past the second it runs again"),
            (5000, 4000, 0, false, "a fresh frame stands it down"),
        ] {
            assert_eq!(
                driver_probe_due(at(now), at(fresh), at(probed)),
                due,
                "{why}"
            );
        }
    }

    /// The rows whose fix lives in the source: the wrapper's order over the fake driver.
    mod over_the_fake {
        use super::*;
        use crate::windows::remote::fake::clock;
        use crate::windows::remote::fake::sessions::Loop;
        use crate::windows::remote::fake::FakeTransport;
        use crate::windows::remote::source::{Geometry, Local, NoRungs, RemoteSource};
        use crate::windows::remote::supervisor::Supervision;
        use crate::{Capturer, EncoderSite, SourceEvent, Supervised};
        use anyhow::{bail, Result};
        use pf_frame::recovery::Stage;
        use pf_frame::{CapturedFrame, PixelFormat};

        const HD: Geometry = Geometry {
            width: 1920,
            height: 1080,
            format: PixelFormat::Nv12,
        };

        /// The IDD wrapper and the loop's encoder over the fake driver.
        struct Session {
            tick: Tick,
            s: Loop,
        }

        impl Session {
            fn new() -> Self {
                Self {
                    tick: Tick::new(),
                    s: Loop::new(EncoderSite::Driver, Supervision::Ladder, HD),
                }
            }

            /// The loop's clocks in, then one tick as `try_consume` runs it; `true`: delivered.
            fn tick(&mut self, cursor: Cursor) -> bool {
                let tick = &mut self.tick;
                let ran = self.s.run(|src| run(tick, src, cursor)).unwrap();
                ran.0.is_some()
            }
        }

        /// `try_consume`'s order over the fake source, with the display's pollers as no-ops.
        fn run(
            tick: &mut Tick,
            src: &mut RemoteSource<FakeTransport>,
            cursor: Cursor,
        ) -> Result<Option<CapturedFrame>> {
            let now = clock::CLOCKS.now;
            let mut queue = VecDeque::from(tick.step(Inputs::Begin));
            let mut at = now();
            let mut frame = None;
            while let Some(action) = queue.pop_front() {
                let next = match action {
                    TakeFault => {
                        src.take_fault()?;
                        continue;
                    }
                    Sample => Inputs::Sampled {
                        now: now(),
                        owns_episode: src.owns_episode(),
                        cursor,
                    },
                    EndSession(why) => bail!("{why}"),
                    ProbeDriver => {
                        if src.poll()? {
                            return Ok(None);
                        }
                        continue;
                    }
                    Supervise {
                        cursor_gap_px,
                        recreating,
                    } => {
                        let local = Local {
                            cursor_gap_px,
                            recreating,
                            topology_held: false,
                        };
                        src.supervise(local, &mut NoRungs)?;
                        Inputs::Supervised {
                            heartbeat_age: src.heartbeat_age(),
                        }
                    }
                    Admit => {
                        if !src.admit(HD) {
                            return Ok(None);
                        }
                        at = now();
                        let t = src.telemetry();
                        Inputs::Admitted {
                            now: at,
                            present_to_arrival: t.and_then(|t| t.present_to_arrival),
                            heartbeat_seen: t.is_some_and(|t| t.drain_heartbeat.is_some()),
                        }
                    }
                    Deliver => {
                        frame = Some(src.deliver(at));
                        continue;
                    }
                    _ => continue,
                };
                queue.extend(tick.step(next));
            }
            Ok(frame)
        }

        #[test]
        fn a_reopened_encoders_first_frame_is_delivered() {
            let mut s = Session::new();
            assert!(s.tick(STILL), "the loop needs a frame to open the encoder");
            s.s.open(HD);
            s.s.far().frame(true);
            assert!(s.tick(STILL));
            assert!(!s.tick(STILL), "the desktop composed nothing");
            // An ABR rebuild at one geometry: the new encoder's first frame carries the number
            // its predecessor delivered last.
            clock::advance(Duration::from_millis(250));
            s.s.open(HD);
            s.s.far().frame(true);
            assert!(
                s.tick(STILL),
                "337faf10f: a re-opened encoder's first frame is delivered"
            );
        }

        #[test]
        fn the_ladder_holds_under_uac_in_a_plain_session() {
            let mut s = Session::new();
            s.tick(MOVING);
            s.s.open(HD);
            s.s.far().frame(true);
            s.tick(MOVING);
            // A wedge under the secure desktop: frames come, no access unit does, the pointer
            // moves, and no cursor declare tracks the prompt.
            s.s.far().secure = true;
            for _ in 0..80 {
                clock::advance(Duration::from_millis(250));
                s.s.far().compose();
                s.tick(MOVING);
                assert!(
                    s.s.src.events().is_empty(),
                    "fda7220e2: the ladder holds under UAC in every session"
                );
            }
            assert_eq!(s.s.src.health().class, "secure_desktop");
            // The prompt goes: the same wedge walks the ladder.
            s.s.far().secure = false;
            let mut asked = None;
            for _ in 0..80 {
                clock::advance(Duration::from_millis(250));
                s.s.far().compose();
                s.tick(MOVING);
                if let [SourceEvent::RunStage(stage)] = s.s.src.events()[..] {
                    asked = Some(stage);
                    break;
                }
            }
            assert_eq!(asked, Some(Stage::EncoderReset));
        }
    }
}
