//! The Windows isolate ladder: the order in which one acquire on a slot preempts, adds,
//! activates, isolates, settles and commits its monitor, and what a commit or a departure owes
//! the group. Pure: no I/O and no OS types, so the table runs on every host.
//!
//! The manager drives it: `acquire` reads its slot as [`Found`] for [`Ladder::start`], runs
//! each [`Next`] as one act inside its transition, with `state` released, and feeds back what
//! the act [`Seen`] to [`Ladder::step`]. [`Next::Commit`], [`Next::Fail`] and
//! [`Next::RetireDevice`] end the climb.
//! [`regroup`] and [`SlotChange::owes`] are the group's share of a slot change;
//! [`prior_holds`] gates the IDD-push setup's preempt.
//!
//! Pin with the table test below: each row names the fixes it keeps. Design:
//! `design/pf-vdisplay-host-overhaul.md` §3 D3.

use std::time::Duration;

use crate::lifecycle;

/// The verified-state ceiling of every mode settle. A rejected mode burns all of it.
pub(crate) const SETTLE: Duration = Duration::from_millis(1500);

/// The acquire's slot in the map, read when the climb starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    Empty,
    /// Lingering or pinned: a reconnect, and a reused IddCx swap-chain is dead.
    Kept,
    /// Active. `host_alive`: its WUDFHost still runs. `resize`: the asked mode is neither the
    /// requested nor the committed one. `resolved`: it has a GDI name.
    Live {
        host_alive: bool,
        resize: bool,
        resolved: bool,
    },
}

/// Which monitor the climb ends with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Create,
    /// A mid-stream resize by REMOVE then ADD on the same slot.
    ReArrive,
    Join,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Preempt,
    Resize,
    Remove,
    /// `attempt` 1 is the re-arrival's rollback ADD at the old mode.
    Add {
        attempt: u8,
    },
    Resolve,
    Isolate,
    Settle,
    Verify,
    Done,
}

/// Why an ADD did not arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddError {
    /// The control device itself is gone; the cached handle can only keep failing.
    DeviceGone,
    Refused,
}

/// What an act observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The kept or dead monitor is torn down and its departure waited out.
    Preempted,
    /// The in-place resize committed (`true`), or the monitor must re-arrive.
    Resized(bool),
    /// The re-arrival's old monitor is removed and departed.
    Removed,
    Added(Result<(), AddError>),
    /// The activation ladder found the target's GDI name.
    Target(bool),
    Isolated,
    Settled,
    Verified,
}

/// The next act.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Tear the slot's monitor down and wait for it to depart.
    Preempt,
    ResizeInPlace,
    /// REMOVE the old monitor of a re-arrival.
    Remove,
    /// ADD at the asked mode, or at the old monitor's requested one (`rollback`).
    Add {
        rollback: bool,
    },
    ResolveTarget,
    /// Write the mode, then the group topology for the new member.
    Isolate,
    WaitSettle(Duration),
    /// Read back the mode the OS committed.
    Verify,
    Commit(SlotChange),
    Fail(Reason),
    /// Retire the control device; the acquire reopens it and climbs once more.
    RetireDevice,
}

/// The slot change a commit stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotChange {
    Create,
    Join,
    Resized,
    ReArrived,
    /// The ADD at the new mode failed and the old mode was re-ADDed. The monitor is new (a
    /// re-arrival mints a fresh target id), so the commit stores it, not its predecessor.
    RolledBack,
    /// Both ADDs failed: the slot stays empty, so the next acquire creates rather than joins
    /// a phantom.
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    /// `max_displays` slots are live or kept.
    Budget,
    /// The ADD, or a step before it, failed.
    Add,
}

/// What a commit owes the group beside its slot change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Owes {
    /// Re-arrange the members' desktop origins.
    pub(crate) layout: bool,
    /// Run [`regroup`] for a member that left.
    pub(crate) regroup: bool,
}

impl SlotChange {
    /// A changed width, or a new target in a member's place, re-arranges the row. A member
    /// lost from the map is a departure: [`regroup`] restores the desk as the last one out, or
    /// re-isolates or re-promotes the siblings, and the row closes its gap.
    pub(crate) fn owes(self) -> Owes {
        match self {
            SlotChange::Create
            | SlotChange::Resized
            | SlotChange::ReArrived
            | SlotChange::RolledBack => Owes {
                layout: true,
                regroup: false,
            },
            SlotChange::Join => Owes::default(),
            SlotChange::Lost => Owes {
                layout: true,
                regroup: true,
            },
        }
    }
}

/// One acquire's climb.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ladder {
    kind: Kind,
    phase: Phase,
    /// Slots in the map, for the budget backstop.
    live: u32,
    max: u32,
    /// The device was retired once for this acquire: a second gone ADD fails.
    reopened: bool,
    /// The re-arrival's ADD failed and the rollback ADD runs or ran.
    rolled_back: bool,
}

impl Ladder {
    /// The first act for `found`, with `live` slots in the map and a budget of `max`.
    pub(crate) fn start(found: Found, live: u32, max: u32, reopened: bool) -> (Ladder, Next) {
        let mut ladder = Ladder {
            kind: Kind::Create,
            phase: Phase::Done,
            live,
            max,
            reopened,
            rolled_back: false,
        };
        let next = match found {
            Found::Kept
            | Found::Live {
                host_alive: false, ..
            } => ladder.enter(Phase::Preempt, Next::Preempt),
            Found::Live { resize: true, .. } => {
                ladder.kind = Kind::Join;
                ladder.enter(Phase::Resize, Next::ResizeInPlace)
            }
            Found::Live {
                resolved: false, ..
            } => {
                ladder.kind = Kind::Join;
                ladder.enter(Phase::Resolve, Next::ResolveTarget)
            }
            Found::Live { .. } => Next::Commit(SlotChange::Join),
            Found::Empty => ladder.create(),
        };
        (ladder, next)
    }

    /// The next act after `seen`. A `seen` that is not the answer to the last [`Next`] is a
    /// shell bug.
    pub(crate) fn step(&mut self, seen: Seen) -> Next {
        match (self.phase, seen) {
            (Phase::Preempt, Seen::Preempted) => {
                self.live = self.live.saturating_sub(1);
                self.create()
            }
            (Phase::Resize, Seen::Resized(true)) => self.done(SlotChange::Resized),
            (Phase::Resize, Seen::Resized(false)) => {
                self.kind = Kind::ReArrive;
                self.enter(Phase::Remove, Next::Remove)
            }
            (Phase::Remove, Seen::Removed) => {
                self.enter(Phase::Add { attempt: 0 }, Next::Add { rollback: false })
            }
            (Phase::Add { .. }, Seen::Added(Ok(()))) => {
                self.enter(Phase::Resolve, Next::ResolveTarget)
            }
            (Phase::Add { attempt: 0 }, Seen::Added(Err(_))) if self.kind == Kind::ReArrive => {
                self.rolled_back = true;
                self.enter(Phase::Add { attempt: 1 }, Next::Add { rollback: true })
            }
            (Phase::Add { .. }, Seen::Added(Err(_))) if self.kind == Kind::ReArrive => {
                self.done(SlotChange::Lost)
            }
            (Phase::Add { .. }, Seen::Added(Err(AddError::DeviceGone))) if !self.reopened => {
                self.phase = Phase::Done;
                Next::RetireDevice
            }
            (Phase::Add { .. }, Seen::Added(Err(_))) => {
                self.phase = Phase::Done;
                Next::Fail(Reason::Add)
            }
            (Phase::Resolve, Seen::Target(_)) if self.kind == Kind::Join => {
                self.done(SlotChange::Join)
            }
            (Phase::Resolve, Seen::Target(false)) => self.done(self.built()),
            (Phase::Resolve, Seen::Target(true)) => self.enter(Phase::Isolate, Next::Isolate),
            (Phase::Isolate, Seen::Isolated) => self.enter(Phase::Settle, Next::WaitSettle(SETTLE)),
            (Phase::Settle, Seen::Settled) => self.enter(Phase::Verify, Next::Verify),
            (Phase::Verify, Seen::Verified) => self.done(self.built()),
            (phase, seen) => unreachable!("isolate ladder: {seen:?} does not answer {phase:?}"),
        }
    }

    /// The budget backstop, then the ADD.
    fn create(&mut self) -> Next {
        if self.live >= self.max {
            self.phase = Phase::Done;
            return Next::Fail(Reason::Budget);
        }
        self.enter(Phase::Add { attempt: 0 }, Next::Add { rollback: false })
    }

    /// The slot change an added monitor commits as.
    fn built(&self) -> SlotChange {
        match self.kind {
            Kind::ReArrive if self.rolled_back => SlotChange::RolledBack,
            Kind::ReArrive => SlotChange::ReArrived,
            Kind::Create | Kind::Join => SlotChange::Create,
        }
    }

    fn enter(&mut self, phase: Phase, next: Next) -> Next {
        self.phase = phase;
        next
    }

    fn done(&mut self, change: SlotChange) -> Next {
        self.phase = Phase::Done;
        Next::Commit(change)
    }
}

/// What the group owes once members left the map: the last one out restores the desk; the
/// survivors of an exclusive group re-isolate, a primary one re-promotes. Keyed on
/// `exclusive`, not on `saved`: a primary group stores a snapshot too. A batch of departures
/// asks once, after all of it left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Regroup {
    Restore,
    Reisolate,
    RepromotePrimary,
    Nothing,
}

pub(crate) fn regroup(survivors: usize, exclusive: bool, saved: bool) -> Regroup {
    match (survivors, exclusive, saved) {
        (0, ..) => Regroup::Restore,
        (_, true, _) => Regroup::Reisolate,
        (_, false, true) => Regroup::RepromotePrimary,
        (_, false, false) => Regroup::Nothing,
    }
}

/// Whether a slot whose prior session was told to stop is still that session's: active with
/// more holds than its joiners'. A joiner's hold alone is not the stopped session's, and a
/// preempt would take the joiner's display.
pub(crate) fn prior_holds(life: lifecycle::State, joined: u32) -> bool {
    matches!(life, lifecycle::State::Active { refs } if refs > joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const MAX: u32 = 4;

    /// A climb: its name, the slot, the live count, whether the device was reopened, what
    /// each act sees and the acts it must ask for.
    type Row = (&'static str, Found, u32, bool, Vec<Seen>, Vec<Next>);

    /// The acts one climb asks for, given what each act sees.
    fn climb(found: Found, live: u32, reopened: bool, seen: &[Seen]) -> Vec<Next> {
        let (mut ladder, first) = Ladder::start(found, live, MAX, reopened);
        let mut acts = vec![first];
        acts.extend(seen.iter().map(|&s| ladder.step(s)));
        acts
    }

    const fn live(host_alive: bool, resize: bool, resolved: bool) -> Found {
        Found::Live {
            host_alive,
            resize,
            resolved,
        }
    }

    const ADDED: Seen = Seen::Added(Ok(()));
    const REFUSED: Seen = Seen::Added(Err(AddError::Refused));
    const GONE: Seen = Seen::Added(Err(AddError::DeviceGone));
    const ADD: Next = Next::Add { rollback: false };
    const ROLLBACK: Next = Next::Add { rollback: true };
    const SETTLE_NEXT: Next = Next::WaitSettle(SETTLE);

    /// The rungs after a fresh ADD: activate, write mode and topology, settle, read back.
    const ACTIVATED: [Seen; 5] = [
        ADDED,
        Seen::Target(true),
        Seen::Isolated,
        Seen::Settled,
        Seen::Verified,
    ];

    #[test]
    fn the_acquire_ladder_climbs_in_order() {
        use Next::{Commit, Fail, Isolate, Preempt, Remove, ResizeInPlace, ResolveTarget, Verify};
        use SlotChange::{Create, Join, Lost, ReArrived, Resized, RolledBack};
        let created = [
            ADD,
            ResolveTarget,
            Isolate,
            SETTLE_NEXT,
            Verify,
            Commit(Create),
        ];
        let preempted = [&[Seen::Preempted][..], &ACTIVATED].concat();
        let preempt_then_create = [&[Preempt][..], &created].concat();
        let resize_fails = [Seen::Resized(false), Seen::Removed];
        let rearrive = [ResizeInPlace, Remove, ADD];
        let rows: Vec<Row> =
            vec![
            (
                "d0e9e0083 a kept slot is a reconnect: preempted, then created fresh",
                Found::Kept,
                1,
                false,
                preempted.clone(),
                preempt_then_create.clone(),
            ),
            (
                "341b31f7c an active slot whose WUDFHost died is preempted, never joined",
                live(false, true, true),
                1,
                false,
                preempted.clone(),
                preempt_then_create.clone(),
            ),
            (
                "a169f2e43 093c66cb1 a create writes mode and topology, settles, then reads back",
                Found::Empty,
                0,
                false,
                ACTIVATED.to_vec(),
                created.to_vec(),
            ),
            (
                "e1d7fa2a3 a target no rung activates commits with no mode or isolate",
                Found::Empty,
                0,
                false,
                vec![ADDED, Seen::Target(false)],
                vec![ADD, ResolveTarget, Commit(Create)],
            ),
            (
                "35d97ae6a the budget backstop refuses a create past max_displays",
                Found::Empty,
                MAX,
                false,
                vec![],
                vec![Fail(Reason::Budget)],
            ),
            (
                "35d97ae6a d0e9e0083 a preempt frees its slot before the budget is counted",
                Found::Kept,
                MAX,
                false,
                preempted.clone(),
                preempt_then_create.clone(),
            ),
            (
                "341b31f7c a device gone under the ADD is retired, and the acquire climbs again",
                Found::Empty,
                0,
                false,
                vec![GONE],
                vec![ADD, Next::RetireDevice],
            ),
            (
                "341b31f7c the climb after a reopen fails a second gone ADD",
                Found::Empty,
                0,
                true,
                vec![GONE],
                vec![ADD, Fail(Reason::Add)],
            ),
            (
                "an ADD the driver refuses fails the acquire",
                Found::Empty,
                0,
                false,
                vec![REFUSED],
                vec![ADD, Fail(Reason::Add)],
            ),
            (
                "a same-mode join of a resolved monitor is a refcount",
                live(true, false, true),
                1,
                false,
                vec![],
                vec![Commit(Join)],
            ),
            (
                "94fd5ef1e a join of a monitor with no GDI name runs the activation again",
                live(true, false, false),
                1,
                false,
                vec![Seen::Target(false)],
                vec![ResolveTarget, Commit(Join)],
            ),
            (
                "093c66cb1 a resize tries the live monitor in place first",
                live(true, true, true),
                1,
                false,
                vec![Seen::Resized(true)],
                vec![ResizeInPlace, Commit(Resized)],
            ),
            (
                "093c66cb1 a resize the OS refuses in place re-arrives at the new mode",
                live(true, true, true),
                1,
                false,
                [&resize_fails[..], &ACTIVATED].concat(),
                [&rearrive[..], &created[1..5], &[Commit(ReArrived)]].concat(),
            ),
            (
                "093c66cb1 a re-arrival whose ADD fails rolls back to the old mode",
                live(true, true, true),
                1,
                false,
                [&resize_fails[..], &[REFUSED], &ACTIVATED].concat(),
                [&rearrive[..], &[ROLLBACK], &created[1..5], &[Commit(RolledBack)]].concat(),
            ),
            (
                "68ca41997 a re-arrival whose rollback fails too is lost",
                live(true, true, true),
                1,
                false,
                [&resize_fails[..], &[REFUSED, REFUSED]].concat(),
                [&rearrive[..], &[ROLLBACK, Commit(Lost)]].concat(),
            ),
            (
                "a re-arrival never retires the device: a gone ADD rolls back like a refused one",
                live(true, true, true),
                1,
                false,
                [&resize_fails[..], &[GONE, GONE]].concat(),
                [&rearrive[..], &[ROLLBACK, Commit(Lost)]].concat(),
            ),
        ];
        for (name, found, live, reopened, seen, want) in rows {
            assert_eq!(climb(found, live, reopened, &seen), want, "{name}");
        }
    }

    #[test]
    fn a_departure_regroups_on_what_survives() {
        let rows = [
            // 2c015bba3: a primary group stores a snapshot too, and must never isolate.
            ((1, false, true), Regroup::RepromotePrimary),
            ((1, true, true), Regroup::Reisolate),
            ((1, true, false), Regroup::Reisolate),
            ((1, false, false), Regroup::Nothing),
            // 68ca41997 2c015bba3: the last member out restores the desk, whatever the group.
            ((0, true, true), Regroup::Restore),
            ((0, false, false), Regroup::Restore),
        ];
        for ((survivors, exclusive, saved), want) in rows {
            assert_eq!(
                regroup(survivors, exclusive, saved),
                want,
                "{survivors} {exclusive} {saved}"
            );
        }
    }

    #[test]
    fn a_commit_owes_the_group_its_layout_and_its_departures() {
        let layout = Owes {
            layout: true,
            regroup: false,
        };
        let rows = [
            // 2c015bba3: a changed width re-arranges the row.
            (SlotChange::Create, layout),
            (SlotChange::Resized, layout),
            (SlotChange::ReArrived, layout),
            (SlotChange::Join, Owes::default()),
            // 093c66cb1: the rolled-back monitor is a new target the row must place.
            (SlotChange::RolledBack, layout),
            // 68ca41997: a lost member regroups, the last one by restoring the desk, and its
            // siblings close the gap.
            (
                SlotChange::Lost,
                Owes {
                    layout: true,
                    regroup: true,
                },
            ),
        ];
        for (change, want) in rows {
            assert_eq!(change.owes(), want, "{change:?}");
        }
    }

    #[test]
    fn a_stopped_session_holds_an_active_slot_beyond_its_joiners() {
        use lifecycle::State;
        let rows = [
            (State::Active { refs: 1 }, 0, true),
            // A joiner with refs 2: the owner's stopped session let go, the joiner stays.
            (State::Active { refs: 1 }, 1, false),
            // The stopped session still holds beside its joiner.
            (State::Active { refs: 2 }, 1, true),
            (
                State::Lingering {
                    until: Instant::now(),
                },
                0,
                false,
            ),
            (State::Pinned, 0, false),
            (State::Idle, 0, false),
        ];
        for (life, joined, holds) in rows {
            assert_eq!(
                prior_holds(life, joined),
                holds,
                "{life:?} with {joined} joined"
            );
        }
    }
}
