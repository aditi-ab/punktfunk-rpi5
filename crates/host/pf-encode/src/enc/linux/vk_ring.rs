//! The Vulkan Video encode ring: which slot the next frame records into, which slots the GPU
//! still owes, and what a fence's answer does to them.
//!
//! Pure: no `ash`, no OS types, so the table below runs on every target. [`Ring::step`] takes
//! what a shell saw ([`Observation`]) and answers what it does next ([`Action`]). The shells
//! are `enqueue`, `poll`, `reset`, `flush` and the encode submit in `vulkan_video.rs`; they
//! sample the fences and execute the actions.
//!
//! Invariants: a slot leaves `in_flight` only once its fence signalled; a slot's source hold
//! lives from its use until its fence signalled or the device idled; a failed record gives its
//! slot back, so the retry records into the same one. Each row of `ring_table` names the rule
//! it pins. Design: `design/pf-encode-overhaul.md` §4.4.

use std::collections::VecDeque;

/// The ring's bookkeeping. `in_flight` is a contiguous run of slots ending just before `next`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ring {
    /// Submitted and not yet read, oldest first.
    in_flight: VecDeque<usize>,
    /// The slot the next frame records into.
    next: usize,
    /// Per slot: the frame's source hold may be live. Its length is the ring's depth.
    held: Vec<bool>,
}

/// What a shell saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// A frame needs a slot to record into.
    Acquire,
    /// The slot's fence signalled.
    Signalled(usize),
    /// The bounded wait on the slot's fence ran out.
    TimedOut(usize),
    /// The wait on, or the probe of, the slot's fence failed for another reason.
    Lost(usize),
    /// Recording or submitting into the slot failed.
    RecordFailed(usize),
    /// The encode submit failed after the slot's compute batch was queued.
    EncodeSubmitFailed(usize),
    /// The slot's encode batch is queued.
    Submitted(usize),
    /// The session resets in place.
    Reset,
    /// The in-flight fences signalled and the device is idle.
    Idle,
    /// The caller wants every AU the ring still owes.
    Flush,
}

/// What a shell does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Wait the slot's fence, bounded, and step the answer.
    Wait(usize),
    /// Drop the slot's source hold and read its AU.
    Read(usize),
    /// Take the frame's source hold for the slot, then record and submit into it.
    Use(usize),
    /// The waited slot stays owed.
    KeepInFlight,
    /// The slot goes back unused: hand the frame's armed loss and wave back for the retry.
    Return(usize),
    /// The slot's cursor upload never ran: upload again on its next use.
    ForgetCursor(usize),
    /// Wait every listed fence, bounded, before the device idles.
    WaitAll(Vec<usize>),
    /// Unsignal the slot's compute semaphore: nothing waits it.
    ConsumeSignal(usize),
    /// Fail the call. The reason names what the fence answered.
    Fail(&'static str),
}

impl Ring {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            in_flight: VecDeque::with_capacity(slots),
            next: 0,
            held: vec![false; slots],
        }
    }

    /// Submitted and not yet read, oldest first.
    pub(crate) fn in_flight(&self) -> &VecDeque<usize> {
        &self.in_flight
    }

    pub(crate) fn step(&mut self, obs: Observation) -> Vec<Action> {
        match obs {
            Observation::Acquire => {
                // A full ring frees its oldest slot first; the shell asks again after.
                if self.in_flight.len() >= self.held.len() {
                    return vec![Action::Wait(self.in_flight[0])];
                }
                let slot = self.next;
                self.next = (slot + 1) % self.held.len();
                self.held[slot] = true;
                vec![Action::Use(slot)]
            }
            Observation::Signalled(slot) => {
                debug_assert_eq!(self.in_flight.front(), Some(&slot), "not the oldest slot");
                self.in_flight.pop_front();
                self.held[slot] = false;
                vec![Action::Read(slot)]
            }
            // The slot stays owed, so `reset` bounds its wait on this fence too.
            Observation::TimedOut(_) => vec![Action::KeepInFlight, Action::Fail("fence timed out")],
            Observation::Lost(_) => vec![Action::KeepInFlight, Action::Fail("fence wait failed")],
            // The hold stays until the slot's next use: an over-hold is harmless.
            Observation::RecordFailed(slot) => {
                self.next = slot;
                vec![Action::Return(slot), Action::ForgetCursor(slot)]
            }
            Observation::EncodeSubmitFailed(slot) => vec![Action::ConsumeSignal(slot)],
            Observation::Submitted(slot) => {
                self.in_flight.push_back(slot);
                Vec::new()
            }
            Observation::Reset if self.in_flight.is_empty() => Vec::new(),
            Observation::Reset => vec![Action::WaitAll(self.in_flight.iter().copied().collect())],
            Observation::Idle => {
                *self = Self::new(self.held.len());
                Vec::new()
            }
            Observation::Flush => self.in_flight.iter().map(|&s| Action::Wait(s)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Observation::*;
    use super::*;

    /// A ring of `slots` with `in_flight` owed, `next` up, and a hold on each of `held`.
    fn ring(slots: usize, in_flight: &[usize], next: usize, held: &[usize]) -> Ring {
        let mut r = Ring::new(slots);
        r.in_flight.extend(in_flight);
        r.next = next;
        for &s in held {
            r.held[s] = true;
        }
        r
    }

    const TIMED_OUT: Action = Fail("fence timed out");

    /// A row: the rule it pins, the ring it starts from, each step's answer, the ring it ends as.
    type Row = (&'static str, Ring, Vec<(Observation, Vec<Action>)>, Ring);

    #[test]
    fn ring_table() {
        let rows: Vec<Row> = vec![
            (
                "a free ring records round robin",
                Ring::new(3),
                vec![
                    (Acquire, vec![Use(0)]),
                    (Submitted(0), vec![]),
                    (Acquire, vec![Use(1)]),
                    (Submitted(1), vec![]),
                ],
                ring(3, &[0, 1], 2, &[0, 1]),
            ),
            (
                "keep a timed-out slot in flight, so reset waits it too",
                ring(2, &[0, 1], 0, &[0, 1]),
                vec![
                    (Acquire, vec![Wait(0)]),
                    (TimedOut(0), vec![KeepInFlight, TIMED_OUT]),
                    (Reset, vec![WaitAll(vec![0, 1])]),
                ],
                ring(2, &[0, 1], 0, &[0, 1]),
            ),
            (
                "a full ring reads its oldest only once it signalled",
                ring(2, &[0, 1], 0, &[0, 1]),
                vec![
                    (Acquire, vec![Wait(0)]),
                    (Signalled(0), vec![Read(0)]),
                    (Acquire, vec![Use(0)]),
                    (Submitted(0), vec![]),
                ],
                ring(2, &[1, 0], 1, &[0, 1]),
            ),
            (
                "a lost fence fails the submit without popping",
                ring(2, &[0, 1], 0, &[0, 1]),
                vec![
                    (Acquire, vec![Wait(0)]),
                    (Lost(0), vec![KeepInFlight, Fail("fence wait failed")]),
                ],
                ring(2, &[0, 1], 0, &[0, 1]),
            ),
            (
                "poll pops after the signal",
                ring(2, &[0, 1], 0, &[0, 1]),
                vec![(Signalled(0), vec![Read(0)]), (Signalled(1), vec![Read(1)])],
                ring(2, &[], 0, &[]),
            ),
            (
                "return a failed slot to the ring",
                ring(2, &[0], 1, &[0]),
                vec![
                    (Acquire, vec![Use(1)]),
                    (RecordFailed(1), vec![Return(1), ForgetCursor(1)]),
                    (Acquire, vec![Use(1)]),
                    (Submitted(1), vec![]),
                ],
                ring(2, &[0, 1], 0, &[0, 1]),
            ),
            (
                "forget a cursor upload the slot never ran, and keep its hold",
                Ring::new(2),
                vec![
                    (Acquire, vec![Use(0)]),
                    (RecordFailed(0), vec![Return(0), ForgetCursor(0)]),
                ],
                ring(2, &[], 0, &[0]),
            ),
            (
                "keep the armed loss when a record fails after a wait",
                ring(2, &[0, 1], 0, &[0, 1]),
                vec![
                    (Acquire, vec![Wait(0)]),
                    (Signalled(0), vec![Read(0)]),
                    (Acquire, vec![Use(0)]),
                    (RecordFailed(0), vec![Return(0), ForgetCursor(0)]),
                ],
                ring(2, &[1], 0, &[0, 1]),
            ),
            (
                "consume csc_sem when the encode submit fails",
                Ring::new(2),
                vec![
                    (Acquire, vec![Use(0)]),
                    (EncodeSubmitFailed(0), vec![ConsumeSignal(0)]),
                    (RecordFailed(0), vec![Return(0), ForgetCursor(0)]),
                ],
                ring(2, &[], 0, &[0]),
            ),
            (
                "reset waits only what is in flight, then idles",
                ring(3, &[2, 0], 1, &[0, 2]),
                vec![(Reset, vec![WaitAll(vec![2, 0])]), (Idle, vec![])],
                Ring::new(3),
            ),
            (
                "a reset with nothing in flight waits nothing",
                ring(2, &[], 1, &[0]),
                vec![(Reset, vec![]), (Idle, vec![])],
                Ring::new(2),
            ),
            (
                "flush reads every owed slot in order",
                ring(3, &[1, 2, 0], 1, &[0, 1, 2]),
                vec![
                    (Flush, vec![Wait(1), Wait(2), Wait(0)]),
                    (Signalled(1), vec![Read(1)]),
                    (Signalled(2), vec![Read(2)]),
                    (Signalled(0), vec![Read(0)]),
                ],
                ring(3, &[], 1, &[]),
            ),
            (
                "a flush that times out keeps the slot owed",
                ring(3, &[1, 2], 0, &[1, 2]),
                vec![
                    (Flush, vec![Wait(1), Wait(2)]),
                    (Signalled(1), vec![Read(1)]),
                    (TimedOut(2), vec![KeepInFlight, TIMED_OUT]),
                ],
                ring(3, &[2], 0, &[2]),
            ),
        ];
        for (name, mut r, steps, end) in rows {
            for (i, (obs, want)) in steps.into_iter().enumerate() {
                assert_eq!(r.step(obs), want, "{name}: step {i} ({obs:?})");
            }
            assert_eq!(r, end, "{name}: end state");
        }
    }
}
