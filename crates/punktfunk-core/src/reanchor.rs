//! Post-loss display freeze — the shared "freeze-until-reanchor" gate.
//!
//! After an unrecoverable reference loss the hardware decoder does **not** error: it *conceals* the
//! reference-missing delta frames (on RADV, the DPB-and-output-COINCIDE path paints a gray plate with
//! the new frame's motion on top) and returns Ok. Displaying that is the "gray frames mid-stream"
//! artifact. Instead every client freezes on the last good picture — withholds the concealed frames
//! from its presenter, which keeps redrawing the held frame — and lifts the freeze ONLY on a proven
//! clean re-anchor: a real IDR, an LTR-RFI recovery anchor ([`USER_FLAG_RECOVERY_ANCHOR`]), or the
//! second intra-refresh recovery mark ([`USER_FLAG_RECOVERY_POINT`]) since the loss.
//!
//! This module owns that decision so every embedder shares ONE implementation instead of re-deriving
//! it (the Linux/Deck pump in `pf-client-core`, the Windows in-process pump, the Android decode loops,
//! and — over the C ABI — the Apple client). The state machine is time-driven but takes `now` as a
//! parameter so it is unit-testable without a clock; the C ABI wrappers supply `Instant::now()`.
//!
//! A client whose decoder parses the bitstream ITSELF has a fourth, independent way to see a clean
//! re-anchor: the **recovery point SEI**, which an intra-refresh encoder emits to name the picture at
//! which its wave has healed the frame. [`on_local_recovery`](ReanchorGate::on_local_recovery) is
//! that path. It is strictly ADDITIONAL — a client without a local parser (Android MediaCodec, Apple
//! VideoToolbox, every FFmpeg rung, which exposes no SEI) simply never calls it and every wire
//! behaviour above is bit-for-bit unchanged.
//!
//! # The one claim a client can REFUTE
//!
//! Of the three lifts, two are self-evident to the client and one is pure hearsay. An IDR predicts
//! from nothing, so "this re-anchors decode" is a property of the picture itself. A recovery mark is
//! only *half* a re-anchor and the gate says so by requiring two. But
//! [`USER_FLAG_RECOVERY_ANCHOR`] is the HOST asserting a fact about the CLIENT's decoder — *the
//! picture I coded this P-frame against is one you still hold, intact* — and until
//! [`AnchorEvidence`] existed the client took it on faith, on the first occurrence, with no
//! scrutiny at all.
//!
//! When that assertion is wrong the failure is the worst-shaped one in this module: the anchor lifts
//! the freeze onto a picture predicted from a reference the client had to conceal, so the gray plate
//! reaches the screen AND the gate stops holding, which means it keeps reaching the screen until
//! some later signal re-arms. A re-anchor claim the client can refute is therefore worse than no
//! claim at all — no claim merely holds the last good frame until the backstop.
//!
//! So a client whose decoder parses the bitstream corroborates it: it already knows which pictures
//! this AU predicts from and whether each of those decoded from a complete reference chain, and
//! [`on_decoded_corroborated`](ReanchorGate::on_decoded_corroborated) refuses an anchor whose
//! references it can prove were damaged. Refusing can only ever make the gate hold LONGER — the
//! freeze stays up, the backstop fires on its ORIGINAL deadline, and the client escalates to a real
//! IDR — which is the direction every other rule here errs in, deliberately. Lanes that cannot
//! answer pass [`AnchorEvidence::Unavailable`] and behave exactly as they always have.
//!
//! [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
//! [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR

use crate::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};
use std::time::{Duration, Instant};

/// Consecutive no-output AUs that force a keyframe request. ~50 ms at 60 Hz — long enough not to fire
/// on a one-frame decoder hiccup, short enough that a lost initial IDR (or a mid-GOP join) unfreezes
/// almost immediately instead of never.
pub const NO_OUTPUT_KEYFRAME_STREAK: u32 = 3;

/// Longest the gate holds the last good frame waiting for a post-loss re-anchor keyframe before it
/// re-asks. After a reference loss the hardware decoder does not error — it conceals the
/// reference-missing deltas (on RADV, the DPB-and-output-COINCIDE path renders them as a gray plate
/// with the new frame's motion painted over it) and returns Ok, so displaying them is the "gray frames
/// mid-stream" artifact. We instead freeze on the last good picture until a fresh IDR re-anchors decode
/// — the behaviour NVIDIA already shows (its DISTINCT output image + different concealment reads as a
/// brief freeze, not gray). This cap only bounds the freeze when recovery genuinely stalls (host
/// ignores the request, or an RFI recovery that never emits a keyframe): the freeze is NEVER lifted to
/// the concealed picture — the deadline re-asks for a keyframe and keeps holding, so a glitch can never
/// become a permanent freeze while a clean re-anchor is what un-freezes. A recovery IDR round-trips well
/// under this on any live link.
pub const REANCHOR_FREEZE_MAX: Duration = Duration::from_millis(500);

/// How many host intra-refresh recovery marks ([`USER_FLAG_RECOVERY_POINT`]) must arrive since the
/// latest loss before the gate lifts its freeze on an IDR-free stream. TWO, not one: with a continuous
/// rolling wave the host marks phase-fixed wave boundaries, so the FIRST boundary after a loss is only
/// partially healed — stripes swept BEFORE the loss still reference the lost frame — and lifting there
/// would flash a partially-stale picture. The SECOND boundary guarantees a full wave swept entirely
/// after the loss, so the picture is clean. This stays correct under repeated loss because every fresh
/// arm resets the count. The cost is up to ~2 wave periods of holding the last good frame — the
/// deliberate "hold longer, never show garbage" trade.
///
/// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
pub const REANCHOR_MARKS_TO_LIFT: u32 = 2;

/// Backstop patience while a host intra-refresh heal is visibly in progress. Each recovery mark pushes
/// the freeze deadline out by this much, so a live mark stream (the host actively healing via its wave)
/// keeps the gate patiently holding the last good frame instead of tripping the IDR floor mid-heal.
/// Must exceed the inter-mark interval (one wave period, ~0.5 s) with margin; if the marks STOP (heal
/// stalled, or the host isn't running intra-refresh) the deadline lapses and the normal recovery-IDR
/// floor fires, so a real stall still recovers.
pub const RECOVERY_MARK_PATIENCE: Duration = Duration::from_millis(1500);

/// How long a frame-index-gap arm's expected `frames_dropped` climb stays pre-credited in
/// [`ReanchorGate::poll`]. One loss arms the gate through TWO signals: the frame-index gap the
/// instant the AU after the loss is delivered ([`ReanchorGate::arm_expecting_drops`]), and the
/// reassembler's `frames_dropped` climb once the lost frame ages out of its loss window (~120 ms
/// later, and only when at least one of its packets arrived). Without the credit, a *fast* recovery
/// — an LTR-RFI anchor typically lands within ~60 ms — lifts the freeze between the two signals,
/// and the stale climb then re-freezes a stream that is already bit-exact healed; the host swallows
/// the resulting keyframe request as an echo of the very RFI that healed it, so the picture stays
/// frozen until the [`REANCHOR_FREEZE_MAX`] overdue re-ask extracts a full IDR (the field
/// "H265 freezes on every loss, AV1 fine" signature — the slower IDR path usually lands after the
/// climb and dodged the race).
///
/// Sized to cover the reassembler's 120 ms loss window plus delivery jitter with a wide margin,
/// while staying short enough that a leftover credit (a straggler that filled the gap late, so no
/// climb ever came; or a whole-frame vanish the reassembler never saw a packet of) cannot mask a
/// genuinely unrelated future climb for long. A masked climb is also never silent in practice:
/// every unrecoverable loss reveals itself as a frame-index gap on the next delivered frame, which
/// re-arms (and re-credits) through [`ReanchorGate::arm_expecting_drops`] on its own.
pub const DROP_CREDIT_WINDOW: Duration = Duration::from_millis(1000);

/// Frames skipped when `got` arrives while `expected` was the next index, or `None` if `got` is
/// contiguous (`== expected`) or a straggler we have already passed. Frame indices are u32 counters
/// that wrap, so the "ahead" test is a wrapping subtraction split at the half-space: a small positive
/// delta is a forward gap (missing frames whose dependents will decode against absent references); a
/// delta in the top half is an index behind us.
pub fn index_gap(expected: u32, got: u32) -> Option<u32> {
    let ahead = got.wrapping_sub(expected);
    (ahead != 0 && ahead < u32::MAX / 2).then_some(ahead)
}

/// Fold one decoded frame into the re-anchor state and decide whether it lifts the post-loss freeze.
///
/// `is_keyframe` — a real IDR (always a clean re-anchor). `has_anchor` — this AU carried
/// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) **and the caller did not
/// refute it** ([`AnchorEvidence`]), the host's definitive single-frame re-anchor from an LTR-RFI
/// recovery (a clean P-frame coded against a known-good
/// reference), so it lifts on the FIRST occurrence exactly like an IDR — no two-mark wait. `has_mark` —
/// this AU carried [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT), a
/// host-signalled intra-refresh wave boundary (only *half* a re-anchor). `marks` — recovery marks seen
/// since the latest loss.
///
/// Returns `(lift, new_marks)`: `lift` clears the freeze; `new_marks` is the running count (reset to 0
/// on a lift). The two-mark rule ([`REANCHOR_MARKS_TO_LIFT`]) lives here so it is unit-tested
/// independent of the pump's channel/decoder plumbing — the first wave boundary after a loss is only
/// partially healed, so a single mark must NOT lift. An anchor (or IDR) is a *whole* re-anchor and
/// lifts immediately.
fn reanchor_after_frame(
    is_keyframe: bool,
    has_anchor: bool,
    has_mark: bool,
    marks: u32,
) -> (bool, u32) {
    let marks = if has_mark {
        marks.saturating_add(1)
    } else {
        marks
    };
    if is_keyframe || has_anchor || marks >= REANCHOR_MARKS_TO_LIFT {
        (true, 0)
    } else {
        (false, marks)
    }
}

/// What a client's OWN bitstream parser saw about intra-refresh recovery on one decoded frame — the
/// in-band counterpart of the wire's [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT).
///
/// Two facts rather than one verdict, because the gate needs both and only the gate knows how to
/// combine them. A recovery point SEI promises: *a decoder that starts at THIS AU has a correct
/// picture N frames later*. That promise covers a decoder which lost references BEFORE the SEI (the
/// wave re-codes every stripe after it, so the stale content is fully overwritten) and says nothing
/// at all about one which lost references AFTER it (the already-swept stripes still reference the
/// lost picture). So a recovery point may only lift a freeze when its SEI was observed at or after
/// the loss — which is the pairing [`ReanchorGate::on_local_recovery`] performs, since the gate is
/// the only party that knows when the loss was.
///
/// Produced by pf-vkdecode's `RecoveryWatch` on the native decode lane. Every other lane leaves it
/// [`Default`] and nothing changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalRecovery {
    /// The AU that produced this frame carried a recovery point SEI — a heal starts here.
    pub sei_here: bool,
    /// This frame IS the recovery point a previously-seen SEI named — the heal completed.
    pub is_recovery_point: bool,
}

impl LocalRecovery {
    /// Nothing observed — what every frame of a stream without recovery point SEIs reports, and what
    /// every client without a local parser passes.
    pub const NONE: LocalRecovery = LocalRecovery {
        sei_here: false,
        is_recovery_point: false,
    };
}

/// What a client's OWN parser can say about the host's re-anchor claim on one decoded frame — the
/// corroboration for [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR).
///
/// An anchor is the host asserting something about the CLIENT's decoder: *this P-frame is coded
/// against a picture you still hold, intact, so decoding it re-anchors you*. The host derives that
/// from its own slot bookkeeping — which tracks whether the client RECEIVED a frame, not whether it
/// DECODED that frame from a complete reference chain. Those two differ exactly when the client had
/// to conceal, and the gap between them is what puts a gray plate on screen with the freeze lifted.
///
/// Three states rather than a bool, for the same reason [`LocalRecovery`] is two facts: a lane that
/// *cannot* answer must be able to say so instead of being folded into "nothing wrong here". Only
/// [`Self::ReferencesDamaged`] changes any behaviour; the other two are indistinguishable to the
/// gate and differ only in what they claim at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnchorEvidence {
    /// This lane has no local bitstream parser, so it cannot corroborate or refute anything — the
    /// host's claim stands, exactly as it always has. Android MediaCodec, Apple VideoToolbox and
    /// every lane reached over the C ABI pass this, and their behaviour is bit-for-bit unchanged.
    #[default]
    Unavailable,
    /// Corroborated: every picture this AU predicts from was itself decoded from a fully-available
    /// reference chain, so the host's claim is consistent with what this decoder actually holds.
    ReferencesClean,
    /// Refuted: this AU predicts from a picture that needed concealment. Whatever the host believes,
    /// decoding this frame cannot re-anchor a decoder whose reference for it is already damaged, so
    /// the anchor does not lift the freeze.
    ReferencesDamaged,
}

impl AnchorEvidence {
    /// May an [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) on this frame
    /// be honoured? Only an outright refutation withholds it — silence is not refutation, so a lane
    /// that cannot corroborate never becomes *stricter* than it was.
    fn honours_anchor(self) -> bool {
        !matches!(self, AnchorEvidence::ReferencesDamaged)
    }
}

/// Whether a decoded frame should be shown or withheld while the gate is (or isn't) frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Present this frame — the gate is not frozen, or this frame is the clean re-anchor that lifts it.
    Present,
    /// Withhold this frame — it is a post-loss concealment; the presenter keeps the last good picture.
    Hold,
}

/// The shared post-loss freeze state machine. A client feeds it three kinds of event — an *arm* (a
/// loss was detected: a frame-index gap, a dropped-count climb, or a decoder wedge/demotion), each
/// *decoded frame* ([`on_decoded`](Self::on_decoded), which decides present-vs-hold and interprets the
/// re-anchor wire flags), and each *no-output* AU ([`on_no_output`](Self::on_no_output)) — plus a
/// periodic [`poll`](Self::poll) that folds the dropped counter and fires the overdue backstop.
///
/// The gate emits *intents* only: [`on_no_output`](Self::on_no_output) and [`poll`](Self::poll) return
/// `true` when the client should ask the host for a keyframe. The client routes that through its own
/// ~100 ms request throttle (and the precise RFI-vs-keyframe range decision stays in the loss-range
/// tracker behind [`crate::client::NativeClient::note_frame_index`]) — the gate never touches the wire.
#[derive(Debug, Clone)]
pub struct ReanchorGate {
    /// Frozen on the last good frame, withholding the decoder's concealed output until a clean
    /// re-anchor. Armed by any loss signal; cleared only by [`on_decoded`](Self::on_decoded) lifting.
    awaiting: bool,
    /// Host intra-refresh recovery marks seen since the latest arm (see [`REANCHOR_MARKS_TO_LIFT`]).
    /// Reset to 0 whenever the freeze is (re-)armed, so a fresh loss always waits out two fresh marks.
    marks: u32,
    /// When the freeze becomes overdue and [`poll`](Self::poll) re-asks for a keyframe (holding, never
    /// resuming to the concealed picture). `None` when not frozen.
    deadline: Option<Instant>,
    /// Consecutive received AUs that produced no decoded frame — a decoder wedged on missing references
    /// with no reassembler drop to trigger recovery. A short streak forces a fresh IDR.
    no_output_streak: u32,
    /// The last `frames_dropped` value [`poll`](Self::poll) observed; a climb means the reassembler
    /// declared an AU unrecoverable and the following deltas will conceal, so arm.
    last_dropped: u64,
    /// A local recovery point SEI has been observed SINCE the latest arm — the precondition for a
    /// later recovery point to be trusted ([`LocalRecovery`]). Zeroed at every arm, so a heal that
    /// began before the loss can never lift the freeze the loss raised.
    local_sei_since_arm: bool,
    /// How many times the freeze has been armed ([`Self::arms`]) — a monotonic counter, never
    /// reset. Only a client with its OWN bitstream parser needs it: `local_sei_since_arm` pairs an
    /// SEI against the arm in TIME, but a decoder can hand back a frame it decoded *before* the
    /// loss (a post-failure DPB flush does exactly that), and no wall clock separates those. Such
    /// a client stamps the decoder's decode-order watermark whenever this counter moves and
    /// discards the local recovery of anything older. Every other client ignores it.
    arms: u64,
    /// `frames_dropped` climb still expected from losses that already armed via a frame-index gap
    /// ([`Self::arm_expecting_drops`]). [`Self::poll`] consumes climbs against this before treating
    /// them as a NEW loss, so the reassembler's delayed bookkeeping of a gap-armed (and possibly
    /// already anchor-healed) loss cannot re-freeze the stream — see [`DROP_CREDIT_WINDOW`].
    drop_credit: u64,
    /// When the outstanding [`Self::drop_credit`] lapses ([`DROP_CREDIT_WINDOW`] after the latest
    /// credited arm). `None` when no credit is outstanding.
    drop_credit_expiry: Option<Instant>,
}

impl ReanchorGate {
    /// Seed the gate with the session's current `frames_dropped` so the first [`poll`](Self::poll)
    /// doesn't read the baseline as a loss.
    pub fn new(frames_dropped: u64) -> Self {
        ReanchorGate {
            awaiting: false,
            marks: 0,
            deadline: None,
            no_output_streak: 0,
            last_dropped: frames_dropped,
            local_sei_since_arm: false,
            arms: 0,
            drop_credit: 0,
            drop_credit_expiry: None,
        }
    }

    /// How many times the freeze has been armed since the gate was created — monotonic, never
    /// reset, and moved by EVERY arm site including the ones inside [`Self::on_no_output`] and
    /// [`Self::poll`] (but not by the overdue backstop, which re-asks without re-arming).
    ///
    /// A client whose decoder parses the bitstream itself watches this so it can pair a frame's
    /// [`LocalRecovery`] against the arm by DECODE ORDER rather than by arrival: a decoder that
    /// flushes its DPB after a failed AU hands back pictures decoded before the loss, and their
    /// recovery marks describe a wave that completed before it. Every other client can ignore
    /// this entirely — nothing in the gate's own behaviour reads it.
    pub fn arms(&self) -> u64 {
        self.arms
    }

    /// Arm the freeze: a loss was detected (a frame-index gap, a dropped-count climb, or a decoder
    /// wedge/demotion). Zeroes the mark count so a fresh loss waits out two fresh recovery marks, and
    /// (re-)sets the backstop deadline. Idempotent while already frozen (re-arming just re-zeroes the
    /// marks and pushes the deadline — the correct behaviour when a second loss lands mid-freeze).
    pub fn arm(&mut self, now: Instant) {
        self.awaiting = true;
        self.marks = 0;
        self.arms = self.arms.saturating_add(1);
        // A heal that was already in flight when this loss landed proves nothing about it: the
        // stripes the wave already swept still reference the picture we just lost. Only an SEI seen
        // from here on may be trusted ([`LocalRecovery`]).
        self.local_sei_since_arm = false;
        self.deadline = Some(now + REANCHOR_FREEZE_MAX);
    }

    /// [`arm`](Self::arm) for a loss detected as a **frame-index gap**, where the caller knows how
    /// many frames the gap skipped. On top of arming, it pre-credits the reassembler's
    /// `frames_dropped` climb those same lost frames will produce up to ~120 ms later (its
    /// loss-window age-out), so [`poll`](Self::poll) does not treat that delayed bookkeeping as a
    /// SECOND loss. Without the credit a fast LTR-RFI anchor lifts the freeze between the two
    /// signals and the stale climb re-freezes a healed stream — the double-arm race
    /// ([`DROP_CREDIT_WINDOW`] tells the whole story). Use plain [`arm`](Self::arm) for every
    /// non-gap loss signal (decoder wedge/demotion), which has no climb to credit.
    pub fn arm_expecting_drops(&mut self, now: Instant, expected_drops: u64) {
        self.arm(now);
        self.drop_credit = self.drop_credit.saturating_add(expected_drops);
        self.drop_credit_expiry = Some(now + DROP_CREDIT_WINDOW);
    }

    /// Fold the client's OWN recovery-point observation for one decoded frame, BEFORE handing that
    /// frame to [`on_decoded`](Self::on_decoded). Returns `true` when it lifted the freeze.
    ///
    /// This is the only re-anchor signal that does not come off the wire, and it exists because
    /// intra-refresh sessions otherwise have NO clean point a client can see. The host's wave never
    /// emits an IDR; libavcodec flags `AV_FRAME_FLAG_KEY` only for true IDRs; and the wire mark
    /// ([`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT)) is set by exactly one
    /// encoder backend — pf-encode's Linux libav-NVENC under its `PUNKTFUNK_INTRA_REFRESH` opt-in —
    /// while the other two that run a wave (Windows AMF, QSV) leave it off pending on-glass GDR
    /// validation. So a client on one of those sessions freezes on loss and holds until
    /// [`REANCHOR_FREEZE_MAX`] expires, then forces the very IDR the wave exists to avoid: half a
    /// second of frozen picture followed by a 20-40× frame, on a stream that healed itself long
    /// before. A decoder that parses the recovery point SEI can simply watch it happen — and unlike
    /// the wire flag, that signal cannot be lost separately from the picture it describes.
    ///
    /// The rule, and the reason it is a pairing rather than a single flag: a recovery point is
    /// trustworthy only when its SEI arrived at or after the loss ([`LocalRecovery`] carries the
    /// argument). A mark whose SEI predates the arm is IGNORED — silently and deliberately; the
    /// backstop still covers it, exactly as today.
    ///
    /// It lifts on the FIRST trusted recovery point, unlike the wire mark's two
    /// ([`REANCHOR_MARKS_TO_LIFT`]), and the difference is not a relaxation. The wire mark is a
    /// phase-fixed WAVE BOUNDARY with no knowledge of when the loss was, so the first boundary after
    /// a loss is only partially healed and a second must be waited out. The SEI names the recovery
    /// point of a specific wave and is only honoured for a wave that STARTED after the loss, so the
    /// picture at that point is fully swept by construction — the same guarantee an
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) gives, derived
    /// locally instead of trusted from the host.
    ///
    /// Called on a gate that is not frozen it only records the SEI; there is nothing to lift.
    pub fn on_local_recovery(&mut self, obs: LocalRecovery) -> bool {
        if obs.sei_here {
            self.local_sei_since_arm = true;
        }
        if !(obs.is_recovery_point && self.local_sei_since_arm && self.awaiting) {
            return false;
        }
        self.awaiting = false;
        self.deadline = None;
        self.marks = 0;
        // Spent: the next heal needs its own SEI. Without this a single wave's recovery point could
        // lift a freeze armed by a LATER loss, which is the one thing the pairing exists to prevent.
        self.local_sei_since_arm = false;
        true
    }

    /// Fold one decoded frame and decide whether to present or withhold it.
    ///
    /// `wire_flags` is the AU's `user_flags` word ([`crate::session::Frame::flags`] /
    /// `PunktfunkFrame.flags`); the gate reads [`FLAG_SOF`](crate::packet::FLAG_SOF) (the host sets it
    /// only on IDR AUs — the codec-agnostic keyframe signal the platform decoders don't expose),
    /// [`USER_FLAG_RECOVERY_ANCHOR`] and [`USER_FLAG_RECOVERY_POINT`]. `decoder_keyframe` is an optional
    /// belt from decoders that flag IDRs themselves (libavcodec's `AV_FRAME_FLAG_KEY` on Linux/Windows);
    /// pass `false` where the decoder doesn't (Android MediaCodec, Apple VideoToolbox) and rely on the
    /// wire `FLAG_SOF`.
    ///
    /// A decoded frame always clears the no-output streak. When frozen, a live mark stream pushes the
    /// backstop out ([`RECOVERY_MARK_PATIENCE`]) so a healing wave isn't pre-empted by a mid-heal IDR.
    ///
    /// This is the whole-hearsay entry point: it believes an anchor on sight. A client whose decoder
    /// parses the bitstream should call
    /// [`on_decoded_corroborated`](Self::on_decoded_corroborated) instead and let its own parser
    /// check the host's claim.
    ///
    /// [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR
    /// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
    pub fn on_decoded(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        now: Instant,
    ) -> GateVerdict {
        self.on_decoded_corroborated(
            wire_flags,
            decoder_keyframe,
            AnchorEvidence::Unavailable,
            now,
        )
    }

    /// [`on_decoded`](Self::on_decoded) for a client that can CHECK the host's re-anchor claim
    /// against its own decoder — the native-decode lanes, which parse every AU themselves and so
    /// know both which pictures this one predicts from and whether each of those decoded cleanly.
    ///
    /// `evidence` is consulted for exactly one thing: whether a
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) on THIS frame may
    /// lift the freeze. [`AnchorEvidence::ReferencesDamaged`] withholds that lift and nothing else,
    /// and the two exclusions are as deliberate as the rule itself:
    ///
    /// * **A real IDR still lifts.** It predicts from nothing, so no evidence about its references
    ///   can bear on it — and the IDR is precisely the escalation a refused anchor is trying to
    ///   provoke. Refusing it would turn the fix into the permanent freeze it exists to avoid.
    /// * **The two-mark [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT) rule
    ///   is untouched**, including its [`RECOVERY_MARK_PATIENCE`] deadline push. An intra-refresh
    ///   wave heals by overwriting stripes rather than by predicting from one named picture, so
    ///   "this frame's references were damaged" says nothing about whether the wave completed.
    ///
    /// A refused anchor also leaves the backstop deadline exactly where the arm put it. That is the
    /// point rather than an omission: the freeze becomes overdue on its ORIGINAL schedule, [`poll`](Self::poll)
    /// re-asks, and the client escalates to a real IDR — the recovery the host's anchor failed to
    /// deliver. Pushing the deadline out on a refusal would reward a host whose anchors do not work
    /// with a longer wait.
    pub fn on_decoded_corroborated(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        evidence: AnchorEvidence,
        now: Instant,
    ) -> GateVerdict {
        self.no_output_streak = 0;
        let is_keyframe = decoder_keyframe || (wire_flags & FLAG_SOF as u32 != 0);
        // An anchor the client's own parser refutes is not an anchor. Folded in HERE rather than
        // inside `reanchor_after_frame` so that function stays a pure statement of the wire rules.
        let has_anchor = wire_flags & USER_FLAG_RECOVERY_ANCHOR != 0 && evidence.honours_anchor();
        let has_mark = wire_flags & USER_FLAG_RECOVERY_POINT != 0;
        if has_mark && self.awaiting {
            self.deadline = Some(now + RECOVERY_MARK_PATIENCE);
        }
        let (lift, marks) = reanchor_after_frame(is_keyframe, has_anchor, has_mark, self.marks);
        self.marks = marks;
        if lift {
            self.awaiting = false;
            self.deadline = None;
        }
        if self.awaiting {
            GateVerdict::Hold
        } else {
            GateVerdict::Present
        }
    }

    /// A received AU produced no decoded frame (decode error, or the decoder swallowed a
    /// reference-missing delta). Returns `true` when the streak has tripped and the client should
    /// (throttled) request a keyframe — arming the freeze at the same time, since the stream is broken
    /// regardless of whether the throttle lets the request through this iteration.
    pub fn on_no_output(&mut self, now: Instant) -> bool {
        self.no_output_streak += 1;
        if self.no_output_streak >= NO_OUTPUT_KEYFRAME_STREAK {
            self.arm(now);
            self.no_output_streak = 0;
            true
        } else {
            false
        }
    }

    /// Periodic fold of the session's `frames_dropped` counter plus the overdue backstop. Returns
    /// `true` when the client should (throttled) request a keyframe: either the drop count climbed by
    /// more than the outstanding gap-arm credit (a fresh unrecoverable loss — arm the freeze) or the
    /// freeze has held a full [`REANCHOR_FREEZE_MAX`] window with no re-anchor (re-ask and keep
    /// holding — NEVER resume to the concealed picture; a genuinely dead stream is the QUIC
    /// idle-timeout watchdog's job, not the gate's).
    ///
    /// A climb covered by [`arm_expecting_drops`](Self::arm_expecting_drops)' credit is the
    /// reassembler's delayed bookkeeping of a loss this gate already armed for — it must neither
    /// re-arm (an LTR-RFI anchor may have healed the stream in the meantime; re-freezing it is the
    /// double-arm race) nor ask again (the gap already fired the precise RFI, and if THAT recovery
    /// was lost the overdue backstop still re-asks at the [`REANCHOR_FREEZE_MAX`] deadline the
    /// gap-arm set — which is also sooner than the deadline a re-arm here would push out to).
    pub fn poll(&mut self, frames_dropped: u64, now: Instant) -> bool {
        let mut want_keyframe = false;
        if frames_dropped > self.last_dropped {
            let climb = frames_dropped - self.last_dropped;
            self.last_dropped = frames_dropped;
            if self.drop_credit_expiry.is_some_and(|e| now >= e) {
                self.drop_credit = 0;
                self.drop_credit_expiry = None;
            }
            let credited = climb.min(self.drop_credit);
            self.drop_credit -= credited;
            if self.drop_credit == 0 {
                self.drop_credit_expiry = None;
            }
            if climb > credited {
                self.arm(now);
                want_keyframe = true;
            }
        }
        if self.awaiting && self.deadline.is_some_and(|d| now >= d) {
            self.deadline = Some(now + REANCHOR_FREEZE_MAX);
            want_keyframe = true;
        }
        want_keyframe
    }

    /// Whether the gate is currently withholding concealed frames (frozen on the last good picture).
    pub fn is_holding(&self) -> bool {
        self.awaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Simulate the gate's re-anchor state across a sequence of decoded frames: each `(is_keyframe,
    // has_mark)` pair is folded through `reanchor_after_frame`, returning the frame index (0-based) at
    // which the freeze first lifts, or `None` if it never does. A reset to 0 models a fresh loss
    // re-arming the freeze (the gate zeroes the count at every arm site).
    fn lift_at(frames: &[(bool, bool)]) -> Option<usize> {
        let mut marks = 0u32;
        for (i, &(is_kf, has_mark)) in frames.iter().enumerate() {
            // The intra-refresh-mark model never carries an LTR-RFI anchor (that path is exercised by
            // `an_rfi_anchor_lifts_immediately`), so `has_anchor` is always false here.
            let (lift, m) = reanchor_after_frame(is_kf, false, has_mark, marks);
            marks = m;
            if lift {
                return Some(i);
            }
        }
        None
    }

    #[test]
    fn a_single_recovery_mark_does_not_lift() {
        // The first wave boundary after a loss is only half-healed — one mark must hold the freeze.
        assert_eq!(REANCHOR_MARKS_TO_LIFT, 2);
        assert_eq!(lift_at(&[(false, true)]), None);
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false)]),
            None
        );
    }

    #[test]
    fn the_second_recovery_mark_lifts() {
        // Two marks = a full wave swept after the loss → clean re-anchor.
        assert_eq!(lift_at(&[(false, true), (false, true)]), Some(1));
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false), (false, true)]),
            Some(3)
        );
    }

    #[test]
    fn a_real_keyframe_lifts_immediately() {
        // An IDR is always a clean anchor — no marks needed.
        assert_eq!(lift_at(&[(true, false)]), Some(0));
        assert_eq!(lift_at(&[(false, true), (true, false)]), Some(1));
    }

    #[test]
    fn a_fresh_gap_resets_the_mark_count() {
        // The gate zeroes `marks` at each arm site, so one mark before a new gap plus one after must
        // NOT lift — the model resets the running count to imitate that.
        let mut marks = 0u32;
        let (_, m) = reanchor_after_frame(false, false, true, marks); // mark #1 (pre-gap)
        marks = m;
        assert_eq!(marks, 1);
        marks = 0; // a new gap re-arms the freeze → count reset
        let (lift, m) = reanchor_after_frame(false, false, true, marks); // first mark of the new wave
        assert!(!lift, "a single post-gap mark must not lift");
        assert_eq!(m, 1);
    }

    #[test]
    fn an_rfi_anchor_lifts_immediately() {
        // An LTR-RFI recovery anchor is a WHOLE re-anchor (a clean P-frame off a known-good reference),
        // so — like an IDR — it lifts on the FIRST occurrence, no two-mark wait.
        let (lift, marks) = reanchor_after_frame(false, true, false, 0);
        assert!(lift, "an RFI anchor must lift the freeze immediately");
        assert_eq!(marks, 0, "a lift resets the running mark count");
        // Even with zero prior marks and no keyframe, the anchor alone is sufficient.
        let (lift, _) = reanchor_after_frame(false, true, true, 1);
        assert!(lift, "an anchor lifts regardless of the pending mark count");
    }

    #[test]
    fn contiguous_indices_are_not_a_gap() {
        assert_eq!(index_gap(5, 5), None);
        assert_eq!(index_gap(0, 0), None);
    }

    #[test]
    fn a_forward_jump_reports_the_skip_count() {
        assert_eq!(index_gap(5, 6), Some(1)); // one frame missing
        assert_eq!(index_gap(5, 9), Some(4));
    }

    #[test]
    fn a_straggler_behind_us_is_not_a_gap() {
        // The reassembler emitted a newer frame first; the late one must not re-arm.
        assert_eq!(index_gap(9, 5), None);
        assert_eq!(index_gap(1, 0), None);
    }

    #[test]
    fn the_index_counter_wraps_cleanly() {
        // last frame = u32::MAX, so the next expected wraps to 0.
        assert_eq!(index_gap(0, 0), None);
        // waiting on u32::MAX, frame 0 arrived → MAX was skipped.
        assert_eq!(index_gap(u32::MAX, 0), Some(1));
        assert_eq!(index_gap(u32::MAX, 2), Some(3));
        // an old frame arriving just after the wrap is still a straggler.
        assert_eq!(index_gap(0, u32::MAX), None);
    }

    // ---- gate-level sequence tests (the whole behavioural contract) ----

    const SOF: u32 = FLAG_SOF as u32; // IDR wire flag
    const ANCHOR: u32 = USER_FLAG_RECOVERY_ANCHOR;
    const POINT: u32 = USER_FLAG_RECOVERY_POINT;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_clean_link_never_holds() {
        // Disarmed gate presents every frame, keyframe or not, and never asks for anything.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
        assert_eq!(g.on_decoded(SOF, true, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }

    #[test]
    fn a_gap_holds_until_the_wire_keyframe_lifts() {
        // Android/Apple path: no decoder keyframe flag, lift comes from the wire FLAG_SOF alone.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now); // frame-index gap
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold); // concealed delta withheld
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present); // IDR re-anchors
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present); // stays presenting
    }

    #[test]
    fn a_gap_lifts_on_the_first_rfi_anchor() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(ANCHOR, false, now), GateVerdict::Present);
        assert!(!g.is_holding());
    }

    #[test]
    fn a_gap_lifts_on_the_second_recovery_mark() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // first boundary: half-healed
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present); // second: clean
    }

    #[test]
    fn a_second_gap_mid_freeze_resets_the_marks() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // mark #1
        g.arm(now); // a fresh loss re-arms → mark count zeroed
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // this is mark #1 of the new wave
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present); // #2 lifts
    }

    #[test]
    fn the_dropped_climb_arms_and_asks() {
        let mut g = ReanchorGate::new(5);
        let now = t0();
        assert!(!g.poll(5, now), "no climb → no ask"); // baseline
        assert!(g.poll(6, now), "a climb asks for a keyframe");
        assert!(g.is_holding(), "and arms the freeze");
        assert!(
            !g.poll(6, now),
            "same value → no repeat ask from the drop path"
        );
    }

    #[test]
    fn an_rfi_anchor_is_not_refrozen_by_the_same_losss_drop_climb() {
        // The double-arm race (field: "H265 freezes on every loss, AV1 fine"): a loss arms via
        // the frame-index gap at T+10ms, the LTR-RFI anchor heals at T+60ms, and the reassembler
        // books the SAME loss into frames_dropped at ~T+130ms. The credited arm must keep that
        // stale climb from re-freezing the healed stream (and from re-asking — the host would
        // swallow the ask as an RFI echo and the picture would freeze until a forced IDR).
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t + Duration::from_millis(10), 1); // gap of one lost frame + RFI
        assert_eq!(
            g.on_decoded(ANCHOR, false, t + Duration::from_millis(60)),
            GateVerdict::Present,
            "the anchor lifts"
        );
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "the credited climb must not ask again"
        );
        assert!(!g.is_holding(), "and must not re-freeze the healed stream");
        assert_eq!(
            g.on_decoded(0, false, t + Duration::from_millis(141)),
            GateVerdict::Present,
            "healthy P-frames keep presenting"
        );
    }

    #[test]
    fn a_climb_beyond_the_credit_is_a_fresh_loss_and_arms() {
        // The credit covers exactly the gap's frames; a bigger climb means MORE loss than the gap
        // accounted for (an interleaved partial-frame loss) — that part must still arm and ask.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 2);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50)); // healed the credited loss
        assert!(
            g.poll(3, t + Duration::from_millis(130)),
            "one uncredited drop → ask"
        );
        assert!(g.is_holding(), "and re-arm for the uncredited part");
    }

    #[test]
    fn the_drop_credit_expires_so_a_late_climb_still_arms() {
        // A straggler can fill the gap late (no climb ever comes) — the leftover credit must not
        // linger and mask a genuinely NEW loss later. Past DROP_CREDIT_WINDOW the credit is void.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50));
        let late = t + DROP_CREDIT_WINDOW + Duration::from_millis(1);
        assert!(
            g.poll(1, late),
            "an expired credit no longer absorbs climbs"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_credited_climb_keeps_the_unhealed_freezes_original_deadline() {
        // When the recovery never arrives, consuming the climb must not silence the gate: the
        // overdue backstop still re-asks — at the deadline the GAP arm set, which is sooner than
        // the deadline a climb re-arm would have pushed out to.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1); // RFI fired here; assume its anchor is lost in transit
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "credited climb: no early re-ask"
        );
        assert!(g.is_holding(), "still frozen — nothing healed it");
        assert!(
            g.poll(1, t + REANCHOR_FREEZE_MAX + Duration::from_millis(1)),
            "the overdue backstop still re-asks on the gap-arm's own deadline"
        );
        assert!(g.is_holding(), "and keeps holding, never resuming to gray");
    }

    #[test]
    fn the_no_output_streak_trips_at_three() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now), "third no-output trips the streak");
        assert!(g.is_holding());
        // A decoded frame resets the streak.
        g.on_decoded(SOF, false, now); // lifts + resets streak
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now));
    }

    #[test]
    fn an_overdue_freeze_re_asks_but_keeps_holding() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // Before the deadline: holding, no re-ask.
        assert!(!g.poll(0, start));
        assert!(g.is_holding());
        // Past REANCHOR_FREEZE_MAX with no re-anchor: re-ask, still holding.
        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(0, later), "overdue freeze re-asks for a keyframe");
        assert!(g.is_holding(), "but never resumes to the concealed picture");
    }

    // ---- the local (recovery point SEI) path ----

    /// One decoded frame's local observation, spelled as the two facts it is.
    fn local(sei_here: bool, is_recovery_point: bool) -> LocalRecovery {
        LocalRecovery {
            sei_here,
            is_recovery_point,
        }
    }

    /// The headline: an intra-refresh session heals and the freeze lifts on the SEI's own recovery
    /// point — no wire flag, no IDR, and above all no waiting out REANCHOR_FREEZE_MAX. This is the
    /// half-second of frozen picture M4 exists to remove.
    #[test]
    fn a_local_recovery_point_lifts_the_freeze_without_the_backstop() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start); // a frame-index gap
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        // The wave starts (SEI) and sweeps; the client keeps holding meanwhile.
        assert!(!g.on_local_recovery(local(true, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);
        assert!(!g.on_local_recovery(local(false, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        // The recovery point: healed, and the very next frame is presented — while the backstop
        // deadline is still far away, which is the whole point.
        let mid = start + Duration::from_millis(120);
        assert!(g.on_local_recovery(local(false, true)), "the heal lifts it");
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, mid), GateVerdict::Present);
        assert!(
            !g.poll(0, mid),
            "and no keyframe is ever asked for — no IDR spike on a stream that healed itself"
        );
    }

    /// The pairing rule. A recovery point whose SEI arrived BEFORE the loss guarantees nothing: the
    /// stripes that wave already swept still reference the picture that was lost, so lifting there
    /// would flash a half-stale frame. It must be ignored and the freeze must hold.
    #[test]
    fn a_recovery_point_from_a_wave_that_predates_the_loss_is_ignored() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        // A wave is in flight when the loss lands.
        assert!(!g.on_local_recovery(local(true, false)));
        g.arm(now);
        // Its recovery point arrives — about a wave that started before the loss.
        assert!(
            !g.on_local_recovery(local(false, true)),
            "a pre-loss wave's recovery point must not lift"
        );
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        // The NEXT wave — started after the loss — does lift.
        assert!(!g.on_local_recovery(local(true, false)));
        assert!(g.on_local_recovery(local(false, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    /// A single recovery point is spent when it lifts: it must not also lift a freeze armed by a
    /// LATER loss, which is exactly what a sticky "an SEI was seen once" flag would do.
    #[test]
    fn a_spent_recovery_point_cannot_lift_the_next_loss() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        g.on_local_recovery(local(true, false));
        assert!(g.on_local_recovery(local(false, true)));
        // A fresh loss, and a stray recovery point with no new SEI behind it.
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the previous wave's credit is gone"
        );
        assert!(g.is_holding());
    }

    /// An SEI whose count is zero puts both facts on ONE frame ("start here, this picture is already
    /// exact"). It must still lift — the pairing is about ORDER, not about needing two frames.
    #[test]
    fn an_sei_that_is_its_own_recovery_point_lifts_on_that_frame() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.on_local_recovery(local(true, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    /// The whole path is inert for every client that has no local parser — Android MediaCodec, Apple
    /// VideoToolbox, and all four FFmpeg rungs (libavcodec exposes no SEI). They never call it, and
    /// even if they did with an empty observation nothing may change.
    #[test]
    fn a_client_without_a_local_parser_sees_no_behaviour_change() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        for _ in 0..8 {
            assert!(!g.on_local_recovery(LocalRecovery::NONE));
            assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        }
        assert!(
            g.is_holding(),
            "still frozen — only the wire can lift this one"
        );
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present);
    }

    /// A local recovery point on a gate that is NOT frozen changes nothing: there is no freeze to
    /// lift, and the observation must not become a stored credit that pre-lifts the next loss.
    #[test]
    fn a_recovery_point_on_an_unfrozen_gate_is_not_banked() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_local_recovery(local(true, true)));
        assert!(!g.is_holding());
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the pre-arm SEI was cleared by the arm"
        );
        assert!(g.is_holding());
    }

    /// `arms()` has to move at EVERY arm site — including the two that arm from inside the gate —
    /// because a client pairing local recovery by decode order re-stamps its watermark off exactly
    /// this counter. An arm site that did not move it would leave that client trusting the
    /// recovery marks of pictures decoded before the loss. The overdue backstop is the one thing
    /// that must NOT move it: it re-asks without re-arming, and re-stamping there would discard a
    /// heal that is legitimately in flight.
    #[test]
    fn every_arm_site_moves_the_arm_counter_and_the_backstop_does_not() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        assert_eq!(g.arms(), 0, "a fresh gate has never armed");

        g.arm(start);
        assert_eq!(g.arms(), 1);
        // Re-arming mid-freeze is a second loss — it counts.
        g.arm(start);
        assert_eq!(g.arms(), 2);

        // The drop-climb arm inside `poll`.
        assert!(g.poll(1, start));
        assert_eq!(g.arms(), 3);

        // The overdue backstop re-asks and keeps holding — but does not re-arm.
        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(1, later));
        assert_eq!(g.arms(), 3, "the backstop re-asks, it does not re-arm");

        // The no-output streak's arm.
        let mut g = ReanchorGate::new(0);
        assert!(!g.on_no_output(start));
        assert!(!g.on_no_output(start));
        assert_eq!(g.arms(), 0, "the streak has not tripped yet");
        assert!(g.on_no_output(start));
        assert_eq!(g.arms(), 1);
    }

    #[test]
    fn a_live_mark_stream_pushes_the_deadline_out() {
        // A healing wave (marks arriving) must not be pre-empted by the overdue IDR floor.
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // A mark past the original freeze deadline pushes it out by RECOVERY_MARK_PATIENCE.
        let t = start + REANCHOR_FREEZE_MAX + Duration::from_millis(10);
        // mark #1 pushes the deadline out; at a time that WOULD have been overdue on the ORIGINAL
        // deadline, poll does not re-ask.
        assert_eq!(g.on_decoded(POINT, false, t), GateVerdict::Hold);
        assert!(!g.poll(0, t + Duration::from_millis(1)));
        assert!(g.is_holding());
    }

    // ---- the corroborated-anchor path (AnchorEvidence) ----

    use AnchorEvidence::{ReferencesClean, ReferencesDamaged, Unavailable};

    /// The headline. The host says "this P-frame re-anchors you"; the client's own parser says the
    /// picture it predicts from is one IT had to conceal. Both cannot be true, and the client's
    /// statement is about its OWN decoder — so the anchor does not lift and the gray plate the
    /// anchor would have presented never reaches the screen.
    #[test]
    fn an_anchor_whose_references_the_decoder_concealed_does_not_lift() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "a refuted anchor is not a re-anchor"
        );
        assert!(g.is_holding(), "and the freeze stays up");
        // Repeating it changes nothing — a host that keeps sending anchors it cannot honour never
        // talks its way past the gate.
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert!(g.is_holding());
    }

    /// The escalation a refusal exists to provoke must still work. An IDR predicts from nothing, so
    /// no evidence about damaged references can bear on it — refusing it too would convert this fix
    /// into the permanent freeze it is meant to avoid.
    #[test]
    fn a_real_idr_lifts_even_while_the_evidence_refutes_anchors() {
        // The decoder's own keyframe flag...
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(0, true, ReferencesDamaged, now),
            GateVerdict::Present,
            "the IDR re-anchors regardless of what the anchor evidence says"
        );
        assert!(!g.is_holding());

        // ...and the wire's FLAG_SOF, for the lanes whose decoder does not flag IDRs.
        let mut g = ReanchorGate::new(0);
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(SOF, false, ReferencesDamaged, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    /// A corroborated anchor is still an anchor: the whole point is to refuse the ones the client
    /// can disprove, not to stop honouring the mechanism.
    #[test]
    fn a_corroborated_anchor_lifts_on_the_first_occurrence() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesClean, now),
            GateVerdict::Hold,
            "an ordinary frame is still withheld"
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    /// `Unavailable` is the promise made to every lane without a local parser: silence is not
    /// refutation. This walks the same sequences the wire-path tests above assert and requires the
    /// identical verdicts through the corroborated entry point.
    #[test]
    fn an_uncorroborated_lane_behaves_exactly_as_it_always_has() {
        // The anchor lift, byte for byte the `a_gap_lifts_on_the_first_rfi_anchor` contract.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, Unavailable, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, Unavailable, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());

        // And `on_decoded` — which every such lane actually calls — must agree with it exactly.
        let mut wire = ReanchorGate::new(0);
        let mut corroborated = ReanchorGate::new(0);
        wire.arm(now);
        corroborated.arm(now);
        for flags in [0, POINT, 0, ANCHOR, SOF, 0] {
            assert_eq!(
                wire.on_decoded(flags, false, now),
                corroborated.on_decoded_corroborated(flags, false, Unavailable, now),
                "flags {flags:#x} diverged between the two entry points"
            );
            assert_eq!(wire.is_holding(), corroborated.is_holding());
        }
    }

    /// A refused anchor must not buy the host time. The freeze becomes overdue on the deadline the
    /// ARM set — not one pushed out by the refusal — so the client escalates to the real IDR that
    /// the failed anchor did not deliver.
    #[test]
    fn a_refused_anchor_leaves_the_backstop_on_its_original_deadline() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // Anchors keep arriving and keep being refused, right up to the deadline.
        for ms in [10, 100, 300, 490] {
            assert_eq!(
                g.on_decoded_corroborated(
                    ANCHOR,
                    false,
                    ReferencesDamaged,
                    start + Duration::from_millis(ms)
                ),
                GateVerdict::Hold
            );
            assert!(!g.poll(0, start + Duration::from_millis(ms)), "not yet due");
        }
        let overdue = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(
            g.poll(0, overdue),
            "the backstop fires on the arm's own deadline — the refusals did not extend it"
        );
        assert!(
            g.is_holding(),
            "and it keeps holding, never resuming to gray"
        );
    }

    /// Refuting an anchor says nothing about an intra-refresh wave: a wave heals by overwriting
    /// stripes rather than by predicting from one named picture, so the two-mark rule and its
    /// patience deadline must be untouched by the evidence.
    #[test]
    fn refuted_anchors_do_not_disturb_the_two_mark_rule() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "mark #1 is still only half a re-anchor"
        );
        // An anchor in between is refused and must not consume or reset the mark count.
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "mark #2 lifts exactly as it does on the wire path"
        );
        assert!(!g.is_holding());
    }

    /// The evidence is consulted only while an anchor flag is actually present — a refutation on an
    /// ordinary frame must not become a second, sticky reason to hold.
    #[test]
    fn damaged_evidence_alone_neither_holds_nor_arms_an_unfrozen_gate() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "an unfrozen gate presents; the evidence is about anchors, not about frames"
        );
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }
}
