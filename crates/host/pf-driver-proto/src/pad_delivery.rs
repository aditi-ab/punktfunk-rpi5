//! The host's half of the sealed pad channel: when to ask the devnode who the driver is, when to
//! duplicate the DATA section into the pid it names, and when to stop.
//!
//! Pure. pf-inject's `PadChannel` makes the OS calls (the channel-proof query, `DuplicateHandle`,
//! the exit wait on the delivered WUDFHost), feeds each outcome to [`Delivery::step`] as an
//! [`Observation`], and carries out the [`Action`] it returns. A delivery stands until its
//! process exits, a failed one is retried at the next probe, and [`MAX_DELIVERY_ATTEMPTS`] bounds
//! the handles a flapping driver can make the host mint. Pinned by the table test below.
//! Evidence: `design/gamepad-channel-sealing.md`.

use crate::gamepad::GAMEPAD_PROTO_VERSION;
use core::time::Duration;

/// Deliveries a pad may attempt before it stops duplicating handles. Each mints a handle in a
/// WUDFHost; a restarted UMDF host earns a fresh budget.
pub const MAX_DELIVERY_ATTEMPTS: u32 = 16;

/// How often an undelivered pad asks its devnode for a proof, a device open plus I/O: past a
/// healthy driver's attach (tens of ms), and idle once delivered.
pub const PROOF_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// Unanswered proof queries in a row before one operator-facing warn: about 5 s of probes.
pub const PROOF_FAILURES_BEFORE_WARN: u32 = 20;

/// Where a pad's delivery stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No live delivery. `attempts` deliveries tried since the last restart; `probed_at` the
    /// last probe, on the shell's clock.
    Waiting {
        attempts: u32,
        probed_at: Option<Duration>,
    },
    /// The WUDFHost `pid` holds the DATA section; no other process is served while it lives.
    Delivered { pid: u32 },
    /// [`MAX_DELIVERY_ATTEMPTS`] spent: no further handle is minted.
    Capped,
}

/// What the shell saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// The mailbox's `driver_proto`, read every tick; a driver writes it even when it refuses.
    DriverProto(u32),
    /// A tick at `now` since the channel was made. `exited`: the delivered WUDFHost has exited.
    Tick { now: Duration, exited: bool },
    /// The answer to [`Action::Resolve`].
    Pid(PidAnswer),
    /// [`Action::Deliver`] duplicated the section into this pid.
    Delivered(u32),
    /// [`Action::Deliver`] failed.
    DeliveryFailed,
}

/// Who the driver is, as far as the shell could tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidAnswer {
    /// The devnode's channel proof named this WUDFHost.
    Proved(u32),
    /// The devnode did not answer a proof.
    NoProof,
    /// No devnode to ask, and the operator has not opted into trusting the mailbox.
    Refused,
    /// No devnode; the operator trusts the mailbox, whose `driver_pid` this is (`0`: none yet).
    Mailbox(u32),
}

/// What the shell does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Idle,
    /// The driver speaks another gamepad protocol (once per pad).
    WarnProtoMismatch(u32),
    /// The attempt cap is reached (once).
    WarnCapped,
    /// Ask who the driver is and feed back [`Observation::Pid`]. `reattach` is the exited
    /// WUDFHost a new one replaces.
    Resolve {
        reattach: Option<u32>,
    },
    /// Duplicate the section into `pid` and feed back the outcome. `unverified`: say, once, that
    /// the pid is the mailbox's, which any local service can write.
    Deliver {
        pid: u32,
        unverified: bool,
    },
    /// The proof query failed `failures` times in a row; `warn` on the
    /// [`PROOF_FAILURES_BEFORE_WARN`]th, debug otherwise.
    NoProof {
        failures: u32,
        warn: bool,
    },
    /// No devnode to ask, so the section is refused (once).
    WarnRefused,
}

/// One pad's delivery: the [`Phase`] plus what it has already said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub phase: Phase,
    proof_failures: u32,
    warned_proto: bool,
    warned_unproven: bool,
}

impl Default for Delivery {
    fn default() -> Delivery {
        Delivery {
            phase: Phase::Waiting {
                attempts: 0,
                probed_at: None,
            },
            proof_failures: 0,
            warned_proto: false,
            warned_unproven: false,
        }
    }
}

impl Delivery {
    /// Fold one observation in and say what to do next.
    pub fn step(&mut self, observation: Observation) -> Action {
        match observation {
            Observation::DriverProto(proto) => {
                if proto == 0 || proto == GAMEPAD_PROTO_VERSION || self.warned_proto {
                    return Action::Idle;
                }
                self.warned_proto = true;
                Action::WarnProtoMismatch(proto)
            }
            Observation::Tick { now, exited } => self.tick(now, exited),
            Observation::Pid(answer) => self.resolved(answer),
            Observation::Delivered(pid) => {
                if matches!(self.phase, Phase::Waiting { .. }) {
                    self.phase = Phase::Delivered { pid };
                }
                Action::Idle
            }
            // The attempt stays counted; the next probe retries.
            Observation::DeliveryFailed => Action::Idle,
        }
    }

    fn tick(&mut self, now: Duration, exited: bool) -> Action {
        let mut reattach = None;
        match self.phase {
            Phase::Capped => return Action::Idle,
            Phase::Delivered { .. } if !exited => return Action::Idle,
            Phase::Delivered { pid } => {
                reattach = Some(pid);
                self.phase = Phase::Waiting {
                    attempts: 0,
                    probed_at: None,
                };
            }
            Phase::Waiting { attempts, .. } if attempts >= MAX_DELIVERY_ATTEMPTS => {
                self.phase = Phase::Capped;
                return Action::WarnCapped;
            }
            Phase::Waiting { .. } => {}
        }
        let Phase::Waiting {
            attempts,
            probed_at,
        } = self.phase
        else {
            return Action::Idle;
        };
        if probed_at.is_some_and(|at| now.saturating_sub(at) < PROOF_PROBE_INTERVAL) {
            return Action::Idle;
        }
        self.phase = Phase::Waiting {
            attempts,
            probed_at: Some(now),
        };
        Action::Resolve { reattach }
    }

    fn resolved(&mut self, answer: PidAnswer) -> Action {
        let Phase::Waiting {
            attempts,
            probed_at,
        } = self.phase
        else {
            return Action::Idle;
        };
        let (pid, unverified) = match answer {
            PidAnswer::Proved(pid) => {
                self.proof_failures = 0;
                (pid, false)
            }
            PidAnswer::NoProof => {
                self.proof_failures += 1;
                return Action::NoProof {
                    failures: self.proof_failures,
                    warn: self.proof_failures == PROOF_FAILURES_BEFORE_WARN,
                };
            }
            PidAnswer::Refused if self.warned_unproven => return Action::Idle,
            PidAnswer::Refused => {
                self.warned_unproven = true;
                return Action::WarnRefused;
            }
            PidAnswer::Mailbox(0) => return Action::Idle,
            PidAnswer::Mailbox(pid) => (pid, !core::mem::replace(&mut self.warned_unproven, true)),
        };
        self.phase = Phase::Waiting {
            attempts: attempts + 1,
            probed_at,
        };
        Action::Deliver { pid, unverified }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    const fn tick(at: u64) -> Observation {
        Observation::Tick {
            now: ms(at),
            exited: false,
        }
    }
    const fn exit_tick(at: u64) -> Observation {
        Observation::Tick {
            now: ms(at),
            exited: true,
        }
    }
    const fn proved(pid: u32) -> Observation {
        Observation::Pid(PidAnswer::Proved(pid))
    }
    const NO_PROOF: Observation = Observation::Pid(PidAnswer::NoProof);
    const RESOLVE: Action = Action::Resolve { reattach: None };
    const fn deliver(pid: u32) -> Action {
        Action::Deliver {
            pid,
            unverified: false,
        }
    }

    /// Feed `steps` in order, asserting each action, and return the state.
    fn run(name: &str, mut d: Delivery, steps: &[(Observation, Action)]) -> Delivery {
        for (i, (observation, want)) in steps.iter().enumerate() {
            let got = d.step(*observation);
            assert_eq!(got, *want, "{name}: step {i}, {observation:?}");
        }
        d
    }

    /// A pad delivered to `pid` at t = 0.
    fn delivered(pid: u32) -> Delivery {
        run(
            "setup",
            Delivery::default(),
            &[
                (tick(0), RESOLVE),
                (proved(pid), deliver(pid)),
                (Observation::Delivered(pid), Action::Idle),
            ],
        )
    }

    /// A case: its name, the starting state, each observation with the action it must get, and
    /// the phase it must end in.
    type Case<'a> = (&'a str, Delivery, &'a [(Observation, Action)], Phase);

    #[test]
    fn delivery_table() {
        let cases: &[Case<'_>] = &[
            (
                "the first tick probes and a proved pid is delivered",
                Delivery::default(),
                &[
                    (tick(0), RESOLVE),
                    (proved(42), deliver(42)),
                    (Observation::Delivered(42), Action::Idle),
                ],
                Phase::Delivered { pid: 42 },
            ),
            (
                "a delivery stands while its process lives; another pid takes nothing over",
                delivered(42),
                &[
                    (tick(10_000), Action::Idle),
                    (proved(7), Action::Idle),
                    (Observation::Delivered(7), Action::Idle),
                ],
                Phase::Delivered { pid: 42 },
            ),
            (
                "an exited WUDFHost is replaced at once",
                delivered(42),
                &[
                    (exit_tick(100), Action::Resolve { reattach: Some(42) }),
                    (proved(43), deliver(43)),
                    (Observation::Delivered(43), Action::Idle),
                ],
                Phase::Delivered { pid: 43 },
            ),
            (
                "probes keep their interval",
                Delivery::default(),
                &[
                    (tick(0), RESOLVE),
                    (
                        NO_PROOF,
                        Action::NoProof {
                            failures: 1,
                            warn: false,
                        },
                    ),
                    (tick(100), Action::Idle),
                    (tick(249), Action::Idle),
                    (tick(250), RESOLVE),
                ],
                Phase::Waiting {
                    attempts: 0,
                    probed_at: Some(ms(250)),
                },
            ),
            (
                "a failed delivery is retried with the same pid at the next probe",
                Delivery::default(),
                &[
                    (tick(0), RESOLVE),
                    (proved(42), deliver(42)),
                    (Observation::DeliveryFailed, Action::Idle),
                    (tick(100), Action::Idle),
                    (tick(250), RESOLVE),
                    (proved(42), deliver(42)),
                ],
                Phase::Waiting {
                    attempts: 2,
                    probed_at: Some(ms(250)),
                },
            ),
            (
                "no devnode: the section is refused, said once",
                Delivery::default(),
                &[
                    (tick(0), RESOLVE),
                    (Observation::Pid(PidAnswer::Refused), Action::WarnRefused),
                    (tick(250), RESOLVE),
                    (Observation::Pid(PidAnswer::Refused), Action::Idle),
                ],
                Phase::Waiting {
                    attempts: 0,
                    probed_at: Some(ms(250)),
                },
            ),
            (
                "a trusted mailbox waits for a pid, and says once that it is unverified",
                Delivery::default(),
                &[
                    (tick(0), RESOLVE),
                    (Observation::Pid(PidAnswer::Mailbox(0)), Action::Idle),
                    (tick(250), RESOLVE),
                    (
                        Observation::Pid(PidAnswer::Mailbox(9)),
                        Action::Deliver {
                            pid: 9,
                            unverified: true,
                        },
                    ),
                    (Observation::DeliveryFailed, Action::Idle),
                    (tick(500), RESOLVE),
                    (Observation::Pid(PidAnswer::Mailbox(9)), deliver(9)),
                ],
                Phase::Waiting {
                    attempts: 2,
                    probed_at: Some(ms(500)),
                },
            ),
            (
                "a protocol mismatch is said once; zero and our own version never",
                Delivery::default(),
                &[
                    (Observation::DriverProto(0), Action::Idle),
                    (
                        Observation::DriverProto(GAMEPAD_PROTO_VERSION),
                        Action::Idle,
                    ),
                    (Observation::DriverProto(2), Action::WarnProtoMismatch(2)),
                    (Observation::DriverProto(2), Action::Idle),
                    (Observation::DriverProto(4), Action::Idle),
                ],
                Phase::Waiting {
                    attempts: 0,
                    probed_at: None,
                },
            ),
        ];
        for (name, start, steps, end) in cases {
            assert_eq!(run(name, *start, steps).phase, *end, "{name}");
        }
    }

    /// Every failed delivery spends one attempt. The cap is said once and then nothing is
    /// minted, until a delivered WUDFHost restarts on a fresh budget.
    #[test]
    fn the_cap_bounds_failed_deliveries() {
        let mut d = Delivery::default();
        let mut at = 0;
        for n in 0..MAX_DELIVERY_ATTEMPTS {
            assert_eq!(d.step(tick(at)), RESOLVE, "attempt {n}");
            assert_eq!(d.step(proved(42)), deliver(42));
            assert_eq!(d.step(Observation::DeliveryFailed), Action::Idle);
            at += 250;
        }
        assert_eq!(d.step(tick(at)), Action::WarnCapped);
        assert_eq!(d.step(tick(at + 250)), Action::Idle);
        assert_eq!(d.step(proved(42)), Action::Idle);
        assert_eq!(d.phase, Phase::Capped);

        // The last attempt before the cap succeeds: the restart after it starts from zero.
        let mut d = Delivery::default();
        for n in 0..MAX_DELIVERY_ATTEMPTS {
            d.step(tick(u64::from(n) * 250));
            d.step(proved(42));
            if n + 1 < MAX_DELIVERY_ATTEMPTS {
                d.step(Observation::DeliveryFailed);
            }
        }
        d.step(Observation::Delivered(42));
        assert_eq!(d.phase, Phase::Delivered { pid: 42 });
        assert_eq!(
            d.step(exit_tick(10_000)),
            Action::Resolve { reattach: Some(42) }
        );
        assert_eq!(
            d.phase,
            Phase::Waiting {
                attempts: 0,
                probed_at: Some(ms(10_000))
            }
        );
    }

    /// An unanswered devnode warns once, on the 20th query in a row; an answer resets the count.
    #[test]
    fn a_silent_devnode_warns_once_per_streak() {
        let mut d = Delivery::default();
        let mut at = 0;
        let mut probe = |d: &mut Delivery, answer: Observation| {
            assert_eq!(d.step(tick(at)), RESOLVE);
            at += 250;
            d.step(answer)
        };
        for n in 1..=PROOF_FAILURES_BEFORE_WARN + 1 {
            let want = Action::NoProof {
                failures: n,
                warn: n == PROOF_FAILURES_BEFORE_WARN,
            };
            assert_eq!(probe(&mut d, NO_PROOF), want);
        }
        assert_eq!(probe(&mut d, proved(5)), deliver(5));
        d.step(Observation::DeliveryFailed);
        assert_eq!(
            probe(&mut d, NO_PROOF),
            Action::NoProof {
                failures: 1,
                warn: false
            }
        );
    }
}
