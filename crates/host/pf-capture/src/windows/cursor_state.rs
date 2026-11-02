//! The IDD-push cursor render model: who draws the pointer, and what the driver is told.
//!
//! One [`CursorModel`] per capturer, built by [`CursorModel::open`] and moved by
//! [`CursorModel::step`]. Both return what the driver must hear;
//! `idd/cursor_model.rs` sends it. The driver's forward flag has one answer at every
//! site, [`CursorModel::forward`]: down while the secure desktop is up or the driver blends,
//! up while the client draws.
//!
//! Pure, so the table below runs on every target. It carries a row per cursor defect that
//! reached a user: a new one adds its row before it changes a transition.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// Who puts the pointer on the client's screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Draws {
    /// DWM, into the frames it composes: no hardware cursor is declared on this target.
    Desktop,
    /// The client, from the shape this side forwards.
    Client,
    /// The driver, blending the declared cursor into what it encodes. A client ask undoes it.
    Driver,
    /// The driver, and no ask changes it: the target carries an earlier session's declare and
    /// no client draws. `channel` is whether this session's delivery gave the blend a shape.
    Pinned { channel: bool },
}

/// What a session knows about its pointer when it opens.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opening {
    /// The target still carries an earlier session's hardware-cursor declare, which keeps the
    /// pointer out of DWM's frames.
    pub(crate) excluded: bool,
    /// This session delivered a cursor channel, so the driver has a shape to declare or blend.
    pub(crate) channel: bool,
    /// The session negotiated a pointer the client draws. The channel alone does not say:
    /// an excluded target gets one as the blend's shape source.
    pub(crate) client_draws: bool,
}

/// What moves the model after the open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Whether the secure desktop (UAC, Winlogon) is up, sampled every tick.
    Secure(bool),
    /// The client asks to draw the pointer itself, or to have it in the video.
    ClientDraws(bool),
    /// The monitor re-arrived. The driver's cursor worker does not survive that.
    Rearrived,
}

/// What the driver must hear. Every flag here is [`CursorModel::forward`] after the step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    /// The secure desktop came up: stand the declare down, or UAC renders no pointer.
    StandDown,
    /// The secure desktop went away. `Some` re-applies the model. `None` leaves the flag down:
    /// a session with no channel never wanted the declare.
    Restore(Option<bool>),
    /// The client's ask changed who draws: send the flag.
    Flip(bool),
    /// Deliver the channel again, then send the flag if the driver takes it.
    Redeliver(bool),
}

/// Where the client's pointer shape is read from ([`CursorModel::source`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The GDI poller: every shape, masked and monochrome included.
    Poller,
    /// The driver's section, alpha shapes only. `first` on the call that latches it.
    Section {
        first: bool,
    },
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorModel {
    draws: Draws,
    /// The secure desktop is up and the declare is stood down.
    secure: bool,
    /// [`Source::Section`] was read once. The poller's serials and the section's are separate
    /// namespaces, so the client never sees the poller's again.
    section_latched: bool,
}

impl CursorModel {
    /// The model a session starts in, and the forward flag to reset the driver with: an
    /// earlier session may have left it down. `None` without a channel.
    pub(crate) fn open(o: Opening) -> (Self, Option<bool>) {
        let draws = match (o.excluded && !(o.client_draws && o.channel), o.channel) {
            (true, channel) => Draws::Pinned { channel },
            (false, true) => Draws::Client,
            (false, false) => Draws::Desktop,
        };
        let model = Self {
            draws,
            secure: false,
            section_latched: false,
        };
        (model, o.channel.then(|| model.forward()))
    }

    pub(crate) fn step(&mut self, event: Event) -> Action {
        match event {
            Event::Secure(up) if up == self.secure || !self.declared() => Action::Nothing,
            Event::Secure(up) => {
                self.secure = up;
                if up {
                    Action::StandDown
                } else {
                    Action::Restore(self.channel().then(|| self.forward()))
                }
            }
            Event::ClientDraws(on) => {
                let next = match (self.draws, on) {
                    (Draws::Client, false) => Draws::Driver,
                    (Draws::Driver, true) => Draws::Client,
                    (same, _) => same,
                };
                if next == self.draws {
                    return Action::Nothing;
                }
                self.draws = next;
                Action::Flip(self.forward())
            }
            Event::Rearrived if self.channel() => Action::Redeliver(self.forward()),
            Event::Rearrived => Action::Nothing,
        }
    }

    /// The flag to leave with the driver as the session ends. A declare left stood down would
    /// have the next session adopt the target undeclared.
    pub(crate) fn close(&self) -> Option<bool> {
        (self.secure && self.channel()).then_some(true)
    }

    /// The forward flag the driver should hold now.
    pub(crate) fn forward(&self) -> bool {
        !self.secure && !self.composite()
    }

    /// The driver blends the pointer into what it encodes.
    pub(crate) fn composite(&self) -> bool {
        matches!(self.draws, Draws::Driver | Draws::Pinned { .. })
    }

    /// No client ask can take the blend away.
    pub(crate) fn pinned(&self) -> bool {
        matches!(self.draws, Draws::Pinned { .. })
    }

    /// A hardware cursor is declared on the target, by this session's channel or an earlier
    /// one's. DWM then composes nothing for pointer travel, the secure desktop needs the
    /// stand-down, and the GDI poller is the client's shape source.
    pub(crate) fn declared(&self) -> bool {
        self.draws != Draws::Desktop
    }

    fn channel(&self) -> bool {
        matches!(
            self.draws,
            Draws::Client | Draws::Driver | Draws::Pinned { channel: true }
        )
    }

    /// Where this call reads the client's shape. A live poller wins, even one that has no
    /// shape yet. Once it is gone the section takes over for the rest of the session.
    pub(crate) fn source(&mut self, poller_alive: bool) -> Source {
        if self.section_latched {
            return Source::Section { first: false };
        }
        if poller_alive {
            return Source::Poller;
        }
        if !self.channel() {
            return Source::None;
        }
        self.section_latched = true;
        Source::Section { first: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{Flip, Nothing, Redeliver, Restore, StandDown};
    use Event::{ClientDraws, Rearrived, Secure};

    /// A clean target, a delivered channel, a client that draws.
    const CLIENT: Opening = Opening {
        excluded: false,
        channel: true,
        client_draws: true,
    };
    /// A clean target and no channel: DWM's own pointer.
    const PLAIN: Opening = Opening {
        excluded: false,
        channel: false,
        client_draws: false,
    };
    /// An earlier session's declare, a channel for the blend, no client that draws.
    const EXCLUDED: Opening = Opening {
        excluded: true,
        channel: true,
        client_draws: false,
    };
    /// An earlier session's declare, and this session's delivery failed.
    const EXCLUDED_NO_CHANNEL: Opening = Opening {
        excluded: true,
        channel: false,
        client_draws: true,
    };
    /// An earlier session's declare under a client that draws.
    const EXCLUDED_CLIENT: Opening = Opening {
        excluded: true,
        channel: true,
        client_draws: true,
    };

    struct Row {
        /// The fix this row pins, and the symptom it had.
        pins: &'static str,
        open: Opening,
        /// The forward flag the open resets the driver with.
        reset: Option<bool>,
        /// Each event with the action it must produce.
        script: &'static [(Event, Action)],
        /// At the end: the driver blends, and a hardware cursor is declared (so pointer travel
        /// is no damage).
        composite: bool,
        declared: bool,
    }

    const TABLE: &[Row] = &[
        Row {
            pins: "abad24788: a UAC prompt cost a capture-model session its pointer",
            open: CLIENT,
            reset: Some(true),
            script: &[
                (ClientDraws(false), Flip(false)),
                (Secure(true), StandDown),
                (Secure(true), Nothing),
                (Secure(false), Restore(Some(false))),
            ],
            composite: true,
            declared: true,
        },
        Row {
            pins: "abad24788: a flip or a re-arrival under UAC re-declared the cursor early",
            open: CLIENT,
            reset: Some(true),
            script: &[
                (Secure(true), StandDown),
                (ClientDraws(false), Flip(false)),
                (ClientDraws(true), Flip(false)),
                (Rearrived, Redeliver(false)),
                (Secure(false), Restore(Some(true))),
            ],
            composite: false,
            declared: true,
        },
        Row {
            pins: "0eba125d0: a flip to the capture model after a re-arrival blended nothing",
            open: CLIENT,
            reset: Some(true),
            script: &[
                (Rearrived, Redeliver(true)),
                (ClientDraws(false), Flip(false)),
                (Rearrived, Redeliver(false)),
            ],
            composite: true,
            declared: true,
        },
        Row {
            pins: "8db990ce1: the channel opened for the blend cancelled the blend",
            open: EXCLUDED,
            reset: Some(false),
            script: &[
                (ClientDraws(true), Nothing),
                (Secure(true), StandDown),
                (Secure(false), Restore(Some(false))),
                (Rearrived, Redeliver(false)),
            ],
            composite: true,
            declared: true,
        },
        Row {
            pins: "8db990ce1: a delivery that failed still forces the blend",
            open: EXCLUDED_NO_CHANNEL,
            reset: None,
            script: &[
                (ClientDraws(true), Nothing),
                (Secure(true), StandDown),
                (Secure(false), Restore(None)),
                (Rearrived, Nothing),
            ],
            composite: true,
            declared: true,
        },
        Row {
            pins: "8db990ce1: an excluded target whose client draws is not forced",
            open: EXCLUDED_CLIENT,
            reset: Some(true),
            script: &[
                (ClientDraws(false), Flip(false)),
                (ClientDraws(true), Flip(true)),
            ],
            composite: false,
            declared: true,
        },
        Row {
            pins: "96e5db9d3: a re-delivery read as an open and named no model",
            open: EXCLUDED_CLIENT,
            reset: Some(true),
            script: &[(Rearrived, Redeliver(true))],
            composite: false,
            declared: true,
        },
        Row {
            pins: "331d67a27: travel under a client-drawn pointer escalated to a reset",
            open: CLIENT,
            reset: Some(true),
            script: &[],
            composite: false,
            declared: true,
        },
        Row {
            pins: "331d67a27: with no declare, travel is damage and nothing is sent",
            open: PLAIN,
            reset: None,
            script: &[
                (Secure(true), Nothing),
                (ClientDraws(false), Nothing),
                (Rearrived, Nothing),
            ],
            composite: false,
            declared: false,
        },
    ];

    #[test]
    fn each_cursor_fix_holds() {
        for row in TABLE {
            let (mut model, reset) = CursorModel::open(row.open);
            assert_eq!(reset, row.reset, "{}: open", row.pins);
            for (i, &(event, want)) in row.script.iter().enumerate() {
                assert_eq!(model.step(event), want, "{}: step {i} {event:?}", row.pins);
            }
            assert_eq!(model.composite(), row.composite, "{}: composite", row.pins);
            assert_eq!(model.declared(), row.declared, "{}: declared", row.pins);
        }
    }

    /// Every flag an action carries is the model's one answer, whatever the path to it, and
    /// an undeclared target is sent nothing.
    #[test]
    fn every_flag_sent_is_the_models_one_answer() {
        let events = [
            Secure(true),
            Secure(false),
            ClientDraws(true),
            ClientDraws(false),
            Rearrived,
        ];
        for bits in 0..8u8 {
            let open = Opening {
                excluded: bits & 1 != 0,
                channel: bits & 2 != 0,
                client_draws: bits & 4 != 0,
            };
            // Every script of four events, from every opening.
            for mut code in 0..events.len().pow(4) {
                let (mut model, _) = CursorModel::open(open);
                for _ in 0..4 {
                    let event = events[code % events.len()];
                    code /= events.len();
                    let sent = match model.step(event) {
                        Nothing | Restore(None) => continue,
                        StandDown => false,
                        Restore(Some(flag)) | Flip(flag) | Redeliver(flag) => flag,
                    };
                    assert_eq!(sent, model.forward(), "{open:?} {event:?}");
                    assert!(model.declared(), "{open:?} {event:?}");
                }
            }
        }
    }

    #[test]
    fn a_session_that_ends_under_the_secure_desktop_leaves_the_declare_up() {
        let (mut model, _) = CursorModel::open(CLIENT);
        assert_eq!(model.close(), None);
        model.step(ClientDraws(false));
        model.step(Secure(true));
        assert_eq!(model.close(), Some(true), "up, whatever the model was");
        model.step(Secure(false));
        assert_eq!(model.close(), None);

        // No channel: the declare is an earlier session's, and stays as it was found.
        let (mut pinned, _) = CursorModel::open(EXCLUDED_NO_CHANNEL);
        assert_eq!(pinned.step(Secure(true)), StandDown);
        assert_eq!(pinned.close(), None);
    }

    #[test]
    fn the_section_once_read_is_the_source_for_good() {
        let (mut model, _) = CursorModel::open(CLIENT);
        assert_eq!(model.source(true), Source::Poller);
        assert_eq!(model.source(false), Source::Section { first: true });
        assert_eq!(model.source(false), Source::Section { first: false });
        assert_eq!(model.source(true), Source::Section { first: false });

        // No channel, so no section: a dead poller leaves no shape and latches nothing.
        let (mut pinned, _) = CursorModel::open(EXCLUDED_NO_CHANNEL);
        assert_eq!(pinned.source(false), Source::None);
        assert_eq!(pinned.source(true), Source::Poller);
        let (mut plain, _) = CursorModel::open(PLAIN);
        assert_eq!(plain.source(false), Source::None);
    }
}
