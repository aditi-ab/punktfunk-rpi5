# 0001 — The capture source contract

Status: accepted, 2026-10-20.

## Context

`pf-capture` has two kinds of source. Pixel sources hand the stream loop pixels: PipeWire,
direct Wayland capture, the synthetic patterns. Remote-encoder sources hand it the news that an
encoder they do not run took a frame: IDD-push (the encoder runs in the pf-vdisplay driver) and
WGC (it runs in the capture worker). One `Capturer` trait had grown as the union of their needs:
26 methods, 14 of them with one implementor, so both loops called no-ops on most sources and
every new need landed on all of them. The decisions fixed most often were OS calls interleaved
with choices, and environment knobs were read where they were used.

## Decision

1. **The core.** `Capturer` holds what every source answers: `next_frame(FirstFrame)`,
   `try_latest`, `is_alive`, `events`, `colour`, and one accessor per role, defaulting to `None`.
2. **Roles.** A capability that one kind of source has is a role trait reached through its
   accessor. The core gains a method only when every kind of source answers it. A role with one
   implementor is allowed; the core is measured without the accessors.

   | Role | Implementors | The loop |
   | --- | --- | --- |
   | `Feed` | PipeWire, direct Wayland | paces on arrivals, pauses between streams, takes the output back |
   | `CursorSource` | PipeWire, IDD-push | forwards the pointer or blends it |
   | `DisplayControl` | IDD-push | resizes and restores presentation in place |
   | `Supervised` | IDD-push, WGC | feeds encoder clocks, publishes health, answers rungs |
   | `RemoteEncoder` | IDD-push, WGC | opens the session encoder there |

3. **Events and values.** What a source asks of the loop is a `SourceEvent` (`ReferenceRisk`,
   `RecoveredOutage`, `RunStage`), drained by one call. What the loop tells a source goes through
   a role. A fact the loop reads is a value: `ColourFacts`.
4. **Model and shell.** A decision fixed more than once is a pure model: an enum state, a `step`
   from an observation to actions, and a table test whose rows name the fixes they pin. The shell
   samples the OS, steps the model, executes the actions.
5. **Knobs once.** Every `PUNKTFUNK_*` read in the crate is in `knobs.rs`, resolved once per open
   and passed as a value.
6. **Crate edges.** pf-capture depends on pf-frame, pf-zerocopy, the display leaves and
   pf-encode-core (the `Encoder` contract), never on pf-encode or the host. What only the host
   knows arrives at open: a value (`ZeroCopyPolicy`) or a closure (the driver IOCTL senders, the
   gamescope cursor targets).

## Consequences

- A new source implements the core and the roles it has; neither stream loop changes.
- The host opens a remote encoder through the source, so it no longer chooses between the
  driver and the worker.
- Loop tests script a `FakeCapturer` (`test-support`) instead of writing their own sources.
- Measure the core with the crate-overhaul matrix script, without `FakeCapturer`: at most ten
  methods, and no non-accessor method with one production implementor.
- Rejected: a capabilities struct (it borrows every role at once), downcasting (the host would
  name the source types), renaming the trait (no new information).

Evidence: `design/pf-capture-overhaul.md` §3–4 in the planning repository.
