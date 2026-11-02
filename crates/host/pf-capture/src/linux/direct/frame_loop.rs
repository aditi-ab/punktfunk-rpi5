//! The direct capture's frame loop: one frame out at a time on a free pool buffer, and what
//! each compositor event does to it.
//!
//! [`FrameLoop::step`] takes what the loop samples at its top (a quit, a stopped session, a
//! free buffer); [`FrameLoop::dispatched`] takes one dispatch's events, in the order the loop
//! acts on them. Both return what `pump.rs` does next. A [`Fault`] ends the capture and it
//! rebuilds; [`Action::Stop`] ends it cleanly.
//!
//! Pure, so the table below runs on every target. A new rule adds its row before it changes a
//! transition.

// Off Linux only the tests read this module.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// Where the loop is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameLoop {
    /// No frame out: the next free buffer arms one.
    Idle,
    /// The compositor fills buffer `idx`.
    Armed { idx: usize },
    /// A fault or a stop ended the loop. Nothing moves it again.
    Ended,
}

/// What moves the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Buffer `idx` is free: the encoder let go of it, or never had it.
    BufferFree(usize),
    /// The armed frame is filled. `presented` is its `presentation_time`, CLOCK_MONOTONIC ns.
    Ready { presented: Option<u64> },
    /// The compositor failed the frame, with its `failure_reason`.
    Failed(u32),
    /// A second constraint batch named another size: the pool no longer fits.
    Resized,
    /// The output was re-lit in another encoding: the pool's depth may be wrong now.
    ColourChanged,
    /// The compositor stopped the session.
    Stopped,
    /// The capturer is going away.
    Quit,
}

/// Why the loop ends with an error, and the capture rebuilds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The compositor failed the frame, with its `failure_reason`.
    Compositor(u32),
    Resized,
    ColourChanged,
}

/// What the shell does, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Create a frame on buffer `idx` and ask for its capture.
    Arm(usize),
    /// Destroy the filled frame and hand buffer `idx` on, stamped from `presented`.
    Deliver {
        idx: usize,
        presented: Option<u64>,
    },
    /// Destroy the armed frame and free buffer `idx`.
    Return(usize),
    Fail(Fault),
    Stop,
}

impl Event {
    /// Where an event acts among one dispatch's: a failed frame before a resize, a resize
    /// before a colour change, and any end before a delivery.
    fn rank(self) -> u8 {
        match self {
            Event::Failed(_) => 0,
            Event::Resized => 1,
            Event::ColourChanged => 2,
            Event::Ready { .. } => 3,
            Event::Stopped | Event::Quit => 4,
            Event::BufferFree(_) => 5,
        }
    }
}

impl FrameLoop {
    /// No frame is out. The shell takes a free buffer only then.
    pub(crate) fn idle(self) -> bool {
        self == FrameLoop::Idle
    }

    pub(crate) fn step(&mut self, event: Event) -> Vec<Action> {
        let armed = match *self {
            FrameLoop::Ended => return Vec::new(),
            FrameLoop::Armed { idx } => Some(idx),
            FrameLoop::Idle => None,
        };
        // An end destroys the armed frame first, except a resize or a re-lit head: their
        // pool goes with the loop.
        let end = |last| -> Vec<Action> {
            armed
                .map(Action::Return)
                .into_iter()
                .chain([last])
                .collect()
        };
        let (next, actions) = match (event, armed) {
            (Event::BufferFree(idx), None) => (FrameLoop::Armed { idx }, vec![Action::Arm(idx)]),
            (Event::Ready { presented }, Some(idx)) => {
                (FrameLoop::Idle, vec![Action::Deliver { idx, presented }])
            }
            (Event::BufferFree(_) | Event::Ready { .. }, _) => return Vec::new(),
            (Event::Failed(reason), _) => (
                FrameLoop::Ended,
                end(Action::Fail(Fault::Compositor(reason))),
            ),
            (Event::Resized, _) => (FrameLoop::Ended, vec![Action::Fail(Fault::Resized)]),
            (Event::ColourChanged, _) => {
                (FrameLoop::Ended, vec![Action::Fail(Fault::ColourChanged)])
            }
            (Event::Stopped | Event::Quit, _) => (FrameLoop::Ended, end(Action::Stop)),
        };
        *self = next;
        actions
    }

    /// One dispatch's events, stepped by [`Event::rank`] in arrival order within a rank.
    pub(crate) fn dispatched(&mut self, mut events: Vec<Event>) -> Vec<Action> {
        events.sort_by_key(|e| e.rank());
        events.into_iter().flat_map(|e| self.step(e)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{Arm, Deliver, Fail, Return, Stop};
    use Event::{BufferFree, ColourChanged, Failed, Quit, Ready, Resized, Stopped};
    use FrameLoop::{Armed, Ended, Idle};

    struct Row {
        /// The rule this row pins, and the commit it came with.
        pins: &'static str,
        /// Each turn's events with the actions they must produce. A one-event turn is what the
        /// loop's top samples; a longer one is a dispatch.
        script: &'static [(&'static [Event], &'static [Action])],
        end: FrameLoop,
    }

    const TABLE: &[Row] = &[
        Row {
            pins: "0361f5812: a free buffer arms one frame, and only while none is out",
            script: &[(&[BufferFree(0)], &[Arm(0)]), (&[BufferFree(1)], &[])],
            end: Armed { idx: 0 },
        },
        Row {
            pins: "0361f5812: `ready` delivers the armed buffer and nothing else",
            script: &[
                (&[BufferFree(2)], &[Arm(2)]),
                (
                    &[Ready { presented: Some(7) }],
                    &[Deliver {
                        idx: 2,
                        presented: Some(7),
                    }],
                ),
                (&[BufferFree(3)], &[Arm(3)]),
            ],
            end: Armed { idx: 3 },
        },
        Row {
            pins: "5aeca9b95: the stamp is the `ready` event's presentation time, or none",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (
                    &[Ready { presented: None }],
                    &[Deliver {
                        idx: 0,
                        presented: None,
                    }],
                ),
            ],
            end: Idle,
        },
        Row {
            pins:
                "0361f5812: a failed frame frees its buffer and ends with the compositor's reason",
            script: &[
                (&[BufferFree(1)], &[Arm(1)]),
                (&[Failed(2)], &[Return(1), Fail(Fault::Compositor(2))]),
            ],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a resize ends the loop, and the pool goes with it",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (&[Resized], &[Fail(Fault::Resized)]),
            ],
            end: Ended,
        },
        Row {
            pins: "7d4d751a0: a re-lit output ends the loop",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (&[ColourChanged], &[Fail(Fault::ColourChanged)]),
            ],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a stopped session ends the loop cleanly",
            script: &[
                (&[BufferFree(3)], &[Arm(3)]),
                (&[Stopped], &[Return(3), Stop]),
            ],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a quit during an armed frame destroys it and returns the buffer",
            script: &[(&[BufferFree(1)], &[Arm(1)]), (&[Quit], &[Return(1), Stop])],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a quit with nothing armed returns nothing",
            script: &[(&[Quit], &[Stop])],
            end: Ended,
        },
        Row {
            pins: "0361f5812: `ready` with no frame armed is ignored",
            script: &[
                (&[Ready { presented: Some(1) }], &[]),
                (&[BufferFree(0)], &[Arm(0)]),
            ],
            end: Armed { idx: 0 },
        },
        Row {
            pins: "0361f5812: a failure with nothing armed still ends the loop",
            script: &[(&[Failed(0)], &[Fail(Fault::Compositor(0))])],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a failed frame outranks a resize in the same dispatch",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (
                    &[Resized, Failed(1)],
                    &[Return(0), Fail(Fault::Compositor(1))],
                ),
            ],
            end: Ended,
        },
        Row {
            pins: "7d4d751a0: a resize outranks a colour change in the same dispatch",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (&[ColourChanged, Resized], &[Fail(Fault::Resized)]),
            ],
            end: Ended,
        },
        Row {
            pins: "0361f5812: a resize in the dispatch that brought `ready` drops the frame",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (
                    &[Ready { presented: None }, Resized],
                    &[Fail(Fault::Resized)],
                ),
            ],
            end: Ended,
        },
        Row {
            pins: "7d4d751a0: a re-lit output in the dispatch that brought `ready` drops the frame",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (
                    &[Ready { presented: Some(4) }, ColourChanged],
                    &[Fail(Fault::ColourChanged)],
                ),
            ],
            end: Ended,
        },
        Row {
            pins: "0361f5812: nothing moves an ended loop",
            script: &[
                (&[BufferFree(0)], &[Arm(0)]),
                (&[Resized], &[Fail(Fault::Resized)]),
                (&[BufferFree(1)], &[]),
                (&[Ready { presented: None }], &[]),
                (&[Failed(0)], &[]),
                (&[Quit], &[]),
            ],
            end: Ended,
        },
    ];

    #[test]
    fn each_frame_loop_rule_holds() {
        for row in TABLE {
            let mut lp = Idle;
            for (i, &(events, want)) in row.script.iter().enumerate() {
                let got = lp.dispatched(events.to_vec());
                assert_eq!(got, want, "{}: turn {i} {events:?}", row.pins);
            }
            assert_eq!(lp, row.end, "{}: end", row.pins);
        }
    }

    /// One frame is out at a time, and each armed buffer leaves once: delivered, returned, or
    /// with the pool when the loop ends, whatever the order of events.
    #[test]
    fn each_armed_buffer_leaves_once() {
        let events = [
            BufferFree(0),
            BufferFree(1),
            Ready { presented: None },
            Failed(0),
            Resized,
            ColourChanged,
            Stopped,
            Quit,
        ];
        for mut code in 0..events.len().pow(5) {
            let mut lp = Idle;
            let mut out = None;
            for _ in 0..5 {
                let event = events[code % events.len()];
                code /= events.len();
                for action in lp.step(event) {
                    match action {
                        Arm(idx) => assert_eq!(out.replace(idx), None, "{event:?}"),
                        Deliver { idx, .. } | Return(idx) => {
                            assert_eq!(out.take(), Some(idx), "{event:?}")
                        }
                        Fail(_) | Stop => assert_eq!(lp, Ended),
                    }
                }
                assert_eq!(lp.idle(), lp != Ended && out.is_none(), "{event:?}");
            }
        }
    }
}
