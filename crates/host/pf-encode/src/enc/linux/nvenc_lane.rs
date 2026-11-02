//! NVENC's lane, blend and retrieve decisions: which path a frame takes into the encoder, where
//! its pointer is drawn, and when the session switches between sync and pipelined retrieve.
//!
//! Pure: no NVENC, CUDA or Vulkan types, so the table below runs on every target. [`step`] takes
//! what the shell saw ([`Observation`]) under the [`Facts`] fixed at open and answers what it does
//! next ([`Action`]). The shell is `nvenc_cuda.rs`; it samples the frame and the session, steps,
//! and executes. The cursor bitmaps stay in their [`crate::cursor_cache::CursorCache`]s: this
//! model says where a pointer is drawn, the cache says whether its bitmap uploads.
//!
//! Invariants: a CUDA frame copies into a slot; a raw frame is read in place only while that
//! lane is open and no pointer shows, and the lane closes on its first failure; the worker lane
//! needs the Vulkan slots; every rebuild uploads both cursor bitmaps again; the retrieve
//! switches only at a drained safe point. Each row of `lane_table` names the fix it pins.
//! Design: `design/pf-encode-overhaul.md` §4.4.

/// What the session was opened with. Never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The colour of a 10-bit input: BT.2020 PQ, or BT.709 for 10-bit SDR.
    pub(crate) hdr_asked: bool,
    /// The depth the session negotiated. An 8-bit capture reaches 10 only as packed RGB.
    pub(crate) depth_asked: u8,
    /// Raw dmabufs go through the convert worker (`PUNKTFUNK_NVENC_RAW`).
    pub(crate) raw_wanted: bool,
    /// LINEAR raw frames may be read in place (`PUNKTFUNK_NVENC_DIRECT`, under the raw lane).
    pub(crate) direct_wanted: bool,
    /// Cursor overlays expected. Without them and without the raw lane, no slot device.
    pub(crate) blend_wanted: bool,
    /// The operator allows stream-ordered submit.
    pub(crate) stream_ordered_allowed: bool,
}

/// The path a frame takes into NVENC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    /// A CUDA buffer, captured or uploaded from CPU pixels, copied into a ring slot.
    Copy,
    /// A LINEAR raw dmabuf NVENC reads where the producer wrote it.
    InPlace,
    /// A raw dmabuf the convert worker writes into a Vulkan slot: converted with the pointer
    /// burnt in when `fused`, else a producer's NV12 or P010 copied as is.
    Worker { fused: bool },
}

/// Whether LINEAR raw frames still try the in-place lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InPlace {
    /// `refusal_logged`: a LINEAR frame the lane cannot read was logged.
    Open { refusal_logged: bool },
    /// Never wanted, or failed once.
    Closed,
}

/// The Vulkan slot device a pointer is drawn through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slots {
    /// Bring-up has not run: before the first session, or for good when nothing wants it.
    Untried,
    /// Up, and the ring is Vulkan-imported.
    Up,
    /// Bring-up failed, the ring fell back to CUDA surfaces, or a wedge leaked the device.
    Down,
}

/// The slot device and the blend-warn latch: one warning per failure streak, and a blend that
/// lands ends the streak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Blend {
    pub(crate) slots: Slots,
    pub(crate) warned: bool,
}

/// The retrieve the host asked for. The session follows at a drained safe point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retrieve {
    /// Sync on the encode thread, unless the operator pins the retrieve thread.
    Sync,
    /// Escalated: every session from the next drained point opens with the retrieve thread.
    Pipelined,
    /// Winding back: the next drained point rebuilds a pipelined session sync.
    Winding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) in_place: InPlace,
    pub(crate) blend: Blend,
    pub(crate) retrieve: Retrieve,
}

impl State {
    pub(crate) fn new(facts: &Facts) -> Self {
        Self {
            in_place: if facts.direct_wanted {
                InPlace::Open {
                    refusal_logged: false,
                }
            } else {
                InPlace::Closed
            },
            blend: Blend {
                slots: Slots::Untried,
                warned: false,
            },
            retrieve: Retrieve::Sync,
        }
    }

    /// Any payload's pointer is drawn while the slot device is up, and is expected to be
    /// before bring-up tried it.
    pub(crate) fn blends_cursor(&self) -> bool {
        self.blend.slots != Slots::Down
    }
}

/// Where a pointer is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Where {
    /// The host's Vulkan blend into the ring slot.
    Slot,
    /// The convert worker's fused pass.
    Worker,
}

/// A session's input layout, as its depth sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    /// Packed 10-bit RGB or a producer's P010.
    TenBit,
    /// Packed 8-bit RGB, which NVENC widens to 10 bits.
    PackedRgb,
    /// NV12 or YUV444, which a 10-bit session refuses to register.
    Planar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Payload {
    Cuda,
    Cpu,
    /// A held dmabuf. `linear`: modifier 0. `fits`: NVENC can read it in place (layout, span,
    /// chroma; no reframe). `planar`: a producer's NV12 or P010.
    Dmabuf {
        linear: bool,
        fits: bool,
        planar: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) payload: Payload,
    /// A cursor overlay rides the frame, visible or not.
    pub(crate) overlay: bool,
    /// The overlay is visible.
    pub(crate) pointer: bool,
}

/// What `PUNKTFUNK_NVENC_ASYNC` pins: `1` forces the retrieve thread, `0` vetoes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pin {
    Adaptive,
    Forced,
    Vetoed,
}

/// The session at a safe point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) inited: bool,
    pub(crate) retrieving: bool,
    /// Nothing in flight.
    pub(crate) drained: bool,
}

/// How a slot blend went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Landed,
    Failed,
    /// No Vulkan slot to draw into.
    NoSlot,
}

/// What the shell saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// A raw dmabuf reached `submit`.
    Raw,
    /// A frame, its session open and its slot free.
    Frame(Frame),
    /// This frame's in-place encode failed.
    InPlaceFailed(Frame),
    /// The fused pass did not land within its bound.
    Stalled,
    /// A slot blend ran.
    Blended(Outcome),
    /// The session opens on this input.
    Opening(Input),
    /// The session opened. `forced`: the operator pins the retrieve thread.
    Opened { forced: bool },
    /// The slot device is up (`true`) or gone: bring-up failed, or the ring fell back to CUDA.
    SlotDevice(bool),
    /// The capture's size or input format changed under an open session.
    Changed,
    /// The top of a submit.
    SafePoint(Session),
    /// The host asks for pipelined retrieve (`on`) or for sync.
    Asked {
        on: bool,
        pin: Pin,
        session: Session,
    },
    /// The host's reset rung.
    Reset,
    /// A teardown closed the session.
    Rebuilt,
    /// The retrieve thread stayed in the driver past its bound.
    Wedged,
}

/// A log line the shell writes with its own level and fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    InPlaceRefused,
    NoBlend,
    BlendFailed,
    PlanarEightBit,
    Escalated,
    WoundBack,
}

/// What the shell does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Encode the frame by this lane.
    Encode(Lane),
    /// The lane failed: log the reason, then submit the frame again, which takes `to`.
    FallBack {
        to: Lane,
        reason: &'static str,
    },
    /// Hand the dmabuf back to capture's latch for this reason and fail the frame.
    Reject(&'static str),
    /// Unmap in-place imports idle past their bound.
    Sweep,
    /// Drop every in-place import: its buffer went with the old size or format.
    DropImports,
    /// Tear the session down; the next submit opens it again.
    Rebuild,
    /// The next frame is an IDR.
    ForceIdr,
    /// Bring the Vulkan slot device up, then step [`Observation::SlotDevice`].
    BringUpSlots,
    /// Start the retrieve thread on the new session.
    StartRetrieve,
    /// Bind NVENC's IO streams to the copy stream: stream-ordered submit.
    BindStreams,
    /// Encode at this depth and colour.
    Depth {
        bits: u8,
        hdr: bool,
    },
    /// Draw the pointer there.
    BlendCursor(Where),
    /// The bitmap held there is gone: the next pointer frame uploads it again.
    UploadCursor(Where),
    /// Satisfy the fused pass's wait and retire its worker, so the copy stream runs on.
    ReleaseConvert,
    /// Leak the session, its inputs and the thread; open no session again.
    Abandon,
    Log(Note),
}

pub(crate) const FALLBACK: &str =
    "NVENC in-place input failed — this encoder goes back to the worker's copy";

pub(crate) fn step(state: &mut State, facts: &Facts, obs: Observation) -> Vec<Action> {
    match obs {
        Observation::Raw if facts.raw_wanted => Vec::new(),
        Observation::Raw => vec![Action::Reject("this session takes no raw dmabuf")],
        Observation::Frame(frame) => frame_plan(state, frame),
        Observation::InPlaceFailed(frame) => {
            state.in_place = InPlace::Closed;
            let planar = matches!(frame.payload, Payload::Dmabuf { planar: true, .. });
            vec![Action::FallBack {
                to: Lane::Worker { fused: !planar },
                reason: FALLBACK,
            }]
        }
        Observation::Stalled => vec![Action::Reject("fused pass stalled"), Action::ReleaseConvert],
        Observation::Blended(Outcome::Landed) => {
            state.blend.warned = false;
            Vec::new()
        }
        Observation::Blended(Outcome::Failed) => warn_once(&mut state.blend, Note::BlendFailed),
        Observation::Blended(Outcome::NoSlot) => warn_once(&mut state.blend, Note::NoBlend),
        Observation::Opening(input) => depth(facts, input),
        Observation::Opened { forced } => {
            let mut out = Vec::new();
            if state.blend.slots == Slots::Untried && (facts.blend_wanted || facts.raw_wanted) {
                out.push(Action::BringUpSlots);
            }
            // The IO-stream binding's output wait would serialise a pipelined session.
            if state.retrieve == Retrieve::Pipelined || forced {
                out.push(Action::StartRetrieve);
            } else if facts.stream_ordered_allowed {
                out.push(Action::BindStreams);
            }
            out
        }
        Observation::SlotDevice(up) => {
            state.blend.slots = if up { Slots::Up } else { Slots::Down };
            Vec::new()
        }
        Observation::Changed => vec![Action::Rebuild, Action::DropImports],
        Observation::SafePoint(session) => safe_point(&mut state.retrieve, session),
        Observation::Asked { on, pin, session } => ask(&mut state.retrieve, on, pin, session),
        Observation::Reset => vec![Action::Rebuild, Action::ForceIdr],
        Observation::Rebuilt => vec![
            Action::UploadCursor(Where::Slot),
            Action::UploadCursor(Where::Worker),
        ],
        Observation::Wedged => {
            if state.blend.slots == Slots::Up {
                state.blend.slots = Slots::Down;
            }
            vec![Action::Abandon]
        }
    }
}

/// A frame's lane, the in-place lane's upkeep, and where its pointer is drawn.
fn frame_plan(state: &mut State, frame: Frame) -> Vec<Action> {
    let mut out = Vec::new();
    let lane = match frame.payload {
        Payload::Cuda | Payload::Cpu => Lane::Copy,
        Payload::Dmabuf {
            linear,
            fits,
            planar,
        } => {
            let open = match &mut state.in_place {
                // A closed lane no longer reaches the sweep its imports would get.
                InPlace::Closed => {
                    out.push(Action::Sweep);
                    false
                }
                InPlace::Open { refusal_logged } => {
                    if !fits && linear && !*refusal_logged {
                        *refusal_logged = true;
                        out.push(Action::Log(Note::InPlaceRefused));
                    }
                    true
                }
            };
            // A visible pointer is drawn into a slot: the producer's buffer is not ours.
            if open && fits && !frame.pointer {
                out.push(Action::Encode(Lane::InPlace));
                return out;
            }
            if state.blend.slots != Slots::Up {
                out.push(Action::Reject("no Vulkan input slots"));
                return out;
            }
            Lane::Worker { fused: !planar }
        }
    };
    out.push(Action::Encode(lane));
    match lane {
        Lane::Worker { fused: true } if frame.pointer => {
            out.push(Action::BlendCursor(Where::Worker));
        }
        Lane::Worker { fused: true } => {}
        _ if frame.overlay => out.push(Action::BlendCursor(Where::Slot)),
        _ => {}
    }
    out
}

fn warn_once(blend: &mut Blend, note: Note) -> Vec<Action> {
    if std::mem::replace(&mut blend.warned, true) {
        Vec::new()
    } else {
        vec![Action::Log(note)]
    }
}

/// Depth follows the input: a 10-bit input carries the session's colour; an 8-bit capture
/// reaches a 10-bit SDR stream only as packed RGB, which NVENC widens. A planar 8-bit surface
/// in a 10-bit session fails `register_resource`, so it stays 8-bit.
fn depth(facts: &Facts, input: Input) -> Vec<Action> {
    if input == Input::TenBit {
        return vec![Action::Depth {
            bits: 10,
            hdr: facts.hdr_asked,
        }];
    }
    let ten = facts.depth_asked >= 10;
    let bits = if ten && input == Input::PackedRgb {
        10
    } else {
        8
    };
    let mut out = Vec::new();
    if ten && bits < 10 {
        out.push(Action::Log(Note::PlanarEightBit));
    }
    out.push(Action::Depth { bits, hdr: false });
    out
}

/// The host's ask for pipelined retrieve (`on`) or for sync, unless the pin refuses it.
fn ask(retrieve: &mut Retrieve, on: bool, pin: Pin, session: Session) -> Vec<Action> {
    match (on, pin, *retrieve) {
        (true, Pin::Vetoed, _) | (false, Pin::Forced, _) => return Vec::new(),
        // The latest ask wins over a wind-back not yet landed.
        (true, _, Retrieve::Winding) if session.retrieving => *retrieve = Retrieve::Sync,
        (true, _, Retrieve::Sync | Retrieve::Winding) if !session.retrieving => {
            *retrieve = Retrieve::Pipelined;
        }
        (false, _, Retrieve::Pipelined) => *retrieve = Retrieve::Winding,
        (false, _, _) if session.retrieving => *retrieve = Retrieve::Winding,
        _ => return Vec::new(),
    }
    safe_point(retrieve, session)
}

/// Switch the retrieve the host asked for once nothing is in flight. A session that has not
/// opened yet opens the right way and needs no rebuild.
fn safe_point(retrieve: &mut Retrieve, session: Session) -> Vec<Action> {
    if !session.drained {
        return Vec::new();
    }
    let note = match (*retrieve, session.retrieving) {
        (Retrieve::Pipelined, false) => Note::Escalated,
        (Retrieve::Winding, true) => {
            *retrieve = Retrieve::Sync;
            Note::WoundBack
        }
        _ => return Vec::new(),
    };
    if session.inited {
        vec![Action::Rebuild, Action::Log(note)]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Observation::*;
    use super::Slots::{Down, Untried, Up};
    use super::*;

    /// Raw lane, in place, blend, 8-bit SDR, stream ordering allowed.
    const RAW: Facts = Facts {
        hdr_asked: false,
        depth_asked: 8,
        raw_wanted: true,
        direct_wanted: true,
        blend_wanted: true,
        stream_ordered_allowed: true,
    };
    /// The import path: CUDA frames only, a cursor overlay expected.
    const CUDA: Facts = Facts {
        raw_wanted: false,
        direct_wanted: false,
        ..RAW
    };
    const OPEN: InPlace = InPlace::Open {
        refusal_logged: false,
    };
    const SHUT: InPlace = InPlace::Closed;
    const SYNC: Retrieve = Retrieve::Sync;
    const PIPED: Retrieve = Retrieve::Pipelined;

    const FUSED: Lane = Lane::Worker { fused: true };
    const COPY: Action = Encode(Lane::Copy);
    const IN_PLACE: Action = Encode(Lane::InPlace);
    const WORKER: Action = Encode(FUSED);
    const COPIED: Action = Encode(Lane::Worker { fused: false });
    const AT_SLOT: Action = BlendCursor(Where::Slot);
    const AT_WORKER: Action = BlendCursor(Where::Worker);
    const NO_SLOTS: Action = Reject("no Vulkan input slots");
    const FALL: Action = FallBack {
        to: FUSED,
        reason: FALLBACK,
    };
    const REUPLOAD: [Action; 2] = [UploadCursor(Where::Slot), UploadCursor(Where::Worker)];
    const OPENED: Observation = Opened { forced: false };

    fn state(in_place: InPlace, slots: Slots, retrieve: Retrieve) -> State {
        let blend = Blend {
            slots,
            warned: false,
        };
        State {
            in_place,
            blend,
            retrieve,
        }
    }

    fn warned(mut s: State) -> State {
        s.blend.warned = true;
        s
    }

    fn depth(bits: u8, hdr: bool) -> Action {
        Depth { bits, hdr }
    }

    fn device(payload: Payload, pointer: bool) -> Observation {
        Frame(super::Frame {
            payload,
            overlay: pointer,
            pointer,
        })
    }

    fn cuda(pointer: bool) -> Observation {
        device(Payload::Cuda, pointer)
    }

    /// A raw frame and whether its pointer shows.
    fn raw(linear: bool, fits: bool, planar: bool, pointer: bool) -> super::Frame {
        let payload = Payload::Dmabuf {
            linear,
            fits,
            planar,
        };
        super::Frame {
            payload,
            overlay: pointer,
            pointer,
        }
    }

    fn session(inited: bool, retrieving: bool, drained: bool) -> Session {
        Session {
            inited,
            retrieving,
            drained,
        }
    }

    fn asked(on: bool, pin: Pin, session: Session) -> Observation {
        Asked { on, pin, session }
    }

    /// A row: the rule it pins, the facts, the state it starts from, each step's answer, the
    /// state it ends as.
    type Row = (
        &'static str,
        Facts,
        State,
        Vec<(Observation, Vec<Action>)>,
        State,
    );

    #[test]
    fn lane_table() {
        let linear = raw(true, true, false, false);
        let tiled = raw(false, false, false, false);
        let ten_sdr = Facts {
            depth_asked: 10,
            ..CUDA
        };
        let ten_hdr = Facts {
            hdr_asked: true,
            ..ten_sdr
        };
        let eight_hdr = Facts {
            hdr_asked: true,
            ..CUDA
        };
        let planar_eight = || vec![Log(Note::PlanarEightBit), depth(8, false)];
        let rows: Vec<Row> = vec![
            (
                "sweep idle in-place imports after the lane fails",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (Frame(linear), vec![IN_PLACE]),
                    (InPlaceFailed(linear), vec![FALL]),
                    (Frame(linear), vec![Sweep, WORKER]),
                ],
                state(SHUT, Up, SYNC),
            ),
            (
                "reject a raw dmabuf the CUDA ring cannot take",
                RAW,
                state(OPEN, Down, SYNC),
                vec![
                    (Frame(tiled), vec![NO_SLOTS]),
                    (Frame(linear), vec![IN_PLACE]),
                ],
                state(OPEN, Down, SYNC),
            ),
            (
                "reject a raw dmabuf a session without the raw lane gets",
                CUDA,
                state(SHUT, Up, SYNC),
                vec![(Raw, vec![Reject("this session takes no raw dmabuf")])],
                state(SHUT, Up, SYNC),
            ),
            (
                "state one cursor blend rule for every payload",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (cuda(true), vec![COPY, AT_SLOT]),
                    (device(Payload::Cpu, true), vec![COPY, AT_SLOT]),
                    (
                        Frame(raw(false, false, false, true)),
                        vec![WORKER, AT_WORKER],
                    ),
                    (Frame(raw(false, false, true, true)), vec![COPIED, AT_SLOT]),
                    // A visible pointer keeps a frame NVENC could read in place on a slot.
                    (Frame(raw(true, true, false, true)), vec![WORKER, AT_WORKER]),
                ],
                state(OPEN, Up, SYNC),
            ),
            (
                "warn once for pointers no slot blend can draw",
                CUDA,
                state(SHUT, Untried, SYNC),
                vec![
                    (cuda(true), vec![COPY, AT_SLOT]),
                    (Blended(Outcome::NoSlot), vec![Log(Note::NoBlend)]),
                    (cuda(true), vec![COPY, AT_SLOT]),
                    (Blended(Outcome::NoSlot), vec![]),
                ],
                warned(state(SHUT, Untried, SYNC)),
            ),
            (
                "warn once per streak of failed blends",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (Blended(Outcome::Failed), vec![Log(Note::BlendFailed)]),
                    (Blended(Outcome::Failed), vec![]),
                    (Blended(Outcome::Landed), vec![]),
                    (Blended(Outcome::Failed), vec![Log(Note::BlendFailed)]),
                ],
                warned(state(OPEN, Up, SYNC)),
            ),
            (
                "re-upload the cursor after a session rebuild",
                RAW,
                state(OPEN, Up, PIPED),
                vec![
                    (
                        SafePoint(session(true, false, true)),
                        vec![Rebuild, Log(Note::Escalated)],
                    ),
                    (Rebuilt, REUPLOAD.to_vec()),
                ],
                state(OPEN, Up, PIPED),
            ),
            (
                "upload the cursor again after a session rebuild",
                eight_hdr,
                state(SHUT, Up, SYNC),
                vec![
                    (Changed, vec![Rebuild, DropImports]),
                    (Rebuilt, REUPLOAD.to_vec()),
                    (Opening(Input::PackedRgb), vec![depth(8, false)]),
                ],
                state(SHUT, Up, SYNC),
            ),
            (
                "keep NV12 capture off the 10-bit path on Linux",
                ten_sdr,
                state(SHUT, Untried, SYNC),
                vec![
                    (Opening(Input::Planar), planar_eight()),
                    (Opening(Input::PackedRgb), vec![depth(10, false)]),
                    (Opening(Input::TenBit), vec![depth(10, false)]),
                ],
                state(SHUT, Untried, SYNC),
            ),
            (
                "a 10-bit input carries the colour, an 8-bit stream is never HDR",
                ten_hdr,
                state(SHUT, Untried, SYNC),
                vec![
                    (Opening(Input::TenBit), vec![depth(10, true)]),
                    (Opening(Input::Planar), planar_eight()),
                ],
                state(SHUT, Untried, SYNC),
            ),
            (
                "a 10-bit input outranks an 8-bit ask",
                eight_hdr,
                state(SHUT, Untried, SYNC),
                vec![
                    (Opening(Input::TenBit), vec![depth(10, true)]),
                    (Opening(Input::PackedRgb), vec![depth(8, false)]),
                ],
                state(SHUT, Untried, SYNC),
            ),
            (
                "keep an incapable box off the fused lane",
                RAW,
                state(OPEN, Up, SYNC),
                vec![(cuda(false), vec![COPY]), (cuda(true), vec![COPY, AT_SLOT])],
                state(OPEN, Up, SYNC),
            ),
            (
                "rebuild when the Linux retrieve thread is gone",
                RAW,
                state(OPEN, Up, PIPED),
                vec![
                    (Reset, vec![Rebuild, ForceIdr]),
                    (Rebuilt, REUPLOAD.to_vec()),
                    (OPENED, vec![StartRetrieve]),
                ],
                state(OPEN, Up, PIPED),
            ),
            (
                "keep a forced IDR across a failed Linux encode",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    // The failed frame goes again, so the IDR the session re-armed rides it.
                    (Frame(linear), vec![IN_PLACE]),
                    (InPlaceFailed(linear), vec![FALL]),
                    (Frame(linear), vec![Sweep, WORKER]),
                    (Reset, vec![Rebuild, ForceIdr]),
                ],
                state(SHUT, Up, SYNC),
            ),
            (
                "free a copy stream stuck on a dead worker's pass",
                RAW,
                state(SHUT, Up, SYNC),
                vec![(Stalled, vec![Reject("fused pass stalled"), ReleaseConvert])],
                state(SHUT, Up, SYNC),
            ),
            (
                "bound the Linux retrieve join in a reset",
                RAW,
                state(OPEN, Up, PIPED),
                vec![(Reset, vec![Rebuild, ForceIdr]), (Wedged, vec![Abandon])],
                state(OPEN, Down, PIPED),
            ),
            (
                "wind a pipelined session back at its next drained point",
                RAW,
                state(OPEN, Up, PIPED),
                vec![
                    (
                        asked(false, Pin::Adaptive, session(true, true, false)),
                        vec![],
                    ),
                    (SafePoint(session(true, true, false)), vec![]),
                    (
                        SafePoint(session(true, true, true)),
                        vec![Rebuild, Log(Note::WoundBack)],
                    ),
                    (Rebuilt, REUPLOAD.to_vec()),
                    (OPENED, vec![BindStreams]),
                ],
                state(OPEN, Up, SYNC),
            ),
            (
                "PUNKTFUNK_NVENC_ASYNC=1 refuses the wind-back",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (asked(false, Pin::Forced, session(true, true, true)), vec![]),
                    (Opened { forced: true }, vec![StartRetrieve]),
                ],
                state(OPEN, Up, SYNC),
            ),
            (
                "PUNKTFUNK_NVENC_ASYNC=0 refuses the escalation",
                RAW,
                state(OPEN, Up, SYNC),
                vec![(asked(true, Pin::Vetoed, session(true, false, true)), vec![])],
                state(OPEN, Up, SYNC),
            ),
            (
                "escalate at the next drained point, stream ordering off",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (
                        asked(true, Pin::Adaptive, session(true, false, false)),
                        vec![],
                    ),
                    (
                        SafePoint(session(true, false, true)),
                        vec![Rebuild, Log(Note::Escalated)],
                    ),
                    (OPENED, vec![StartRetrieve]),
                ],
                state(OPEN, Up, PIPED),
            ),
            (
                "a session not yet open takes the escalation without a rebuild",
                RAW,
                state(OPEN, Untried, SYNC),
                vec![
                    (
                        asked(true, Pin::Adaptive, session(false, false, true)),
                        vec![],
                    ),
                    (OPENED, vec![BringUpSlots, StartRetrieve]),
                ],
                state(OPEN, Untried, PIPED),
            ),
            (
                "an ask while winding back keeps the running thread until a rebuild",
                RAW,
                state(OPEN, Up, Retrieve::Winding),
                vec![
                    (
                        asked(true, Pin::Adaptive, session(true, true, false)),
                        vec![],
                    ),
                    (SafePoint(session(true, true, true)), vec![]),
                    (Reset, vec![Rebuild, ForceIdr]),
                    (OPENED, vec![BindStreams]),
                ],
                state(OPEN, Up, SYNC),
            ),
            (
                "bring the slot device up once, for an overlay or the raw lane",
                RAW,
                state(OPEN, Untried, SYNC),
                vec![
                    (OPENED, vec![BringUpSlots, BindStreams]),
                    (SlotDevice(false), vec![]),
                    (OPENED, vec![BindStreams]),
                    (Frame(tiled), vec![NO_SLOTS]),
                ],
                state(OPEN, Down, SYNC),
            ),
            (
                "a session that wants no slot device never brings it up",
                Facts {
                    blend_wanted: false,
                    stream_ordered_allowed: false,
                    ..CUDA
                },
                state(SHUT, Untried, SYNC),
                vec![(OPENED, vec![])],
                state(SHUT, Untried, SYNC),
            ),
            (
                "log a LINEAR frame the in-place lane cannot read, once",
                RAW,
                state(OPEN, Up, SYNC),
                vec![
                    (
                        Frame(raw(true, false, true, false)),
                        vec![Log(Note::InPlaceRefused), COPIED],
                    ),
                    (Frame(raw(true, false, false, false)), vec![WORKER]),
                    (Frame(tiled), vec![WORKER]),
                ],
                state(
                    InPlace::Open {
                        refusal_logged: true,
                    },
                    Up,
                    SYNC,
                ),
            ),
        ];
        for (name, facts, mut s, steps, end) in rows {
            for (i, (obs, want)) in steps.into_iter().enumerate() {
                assert_eq!(
                    step(&mut s, &facts, obs),
                    want,
                    "{name}: step {i} ({obs:?})"
                );
            }
            assert_eq!(s, end, "{name}: end state");
        }
        // The same rule in `Encoder::caps`: drawn while the slot device is up or untried.
        for (slots, blends) in [(Untried, true), (Up, true), (Down, false)] {
            let s = state(OPEN, slots, SYNC);
            assert_eq!(s.blends_cursor(), blends, "blends_cursor with {slots:?}");
        }
    }
}
