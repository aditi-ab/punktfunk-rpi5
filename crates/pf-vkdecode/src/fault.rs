//! Deliberate decoder-input corruption — the fault injector (M4 of the
//! native-decode program).
//!
//! # Why a first-class tool
//!
//! The whole program started from a field corruption that was ARCHITECTURALLY
//! undetectable: FFmpeg's Vulkan decoder creates no status queries
//! (`nb_queries = 0`), never sets `AV_FRAME_FLAG_CORRUPT`, and reports trouble only
//! as `av_log` lines. Nobody could tell a healthy stream from a broken one without
//! looking at the screen. The native decoder now has the signals — plan warnings
//! from pf-bitstream and per-op `RESULT_STATUS` verdicts from the driver — but a
//! detector nobody can fire is exactly as trustworthy as no detector at all. This
//! is the trigger: a deterministic way to break decoder input on purpose, so
//! detection can be PROVEN rather than assumed, on a lab box or in CI.
//!
//! It is inert unless explicitly armed ([`AuFault::from_spec`] returns `None` for
//! an unset/unparsable spec) and it is pure — no I/O, no clock, no randomness — so
//! a reported fault is reproducible from the spec string alone.
//!
//! # The three modes, and which detector each one fires
//!
//! They are not variations on one idea. The native lane has TWO independent
//! detectors — pf-bitstream's planner, which reads syntax, and the driver's per-op
//! `RESULT_STATUS` query, which reads the decode itself — and the modes exist to
//! fire them separately, because a harness that can only trip one of them proves
//! only half the lane:
//!
//! * [`FaultMode::Drop`] — the AU never reaches the decoder. The NEXT AU then
//!   references a picture that was never decoded, so the planner reports
//!   `FrameNumGap`/`MissingReference`: **parser-visible** damage, caught before a
//!   single macroblock is decoded. The everyday network-loss shape, and the one
//!   mode whose detection is provable without a GPU.
//! * [`FaultMode::Truncate`] — the AU arrives short. Worth knowing, and initially
//!   surprising: this is **NOT** parser-visible. Annex-B carries no NALU length, so
//!   a slice cut at a byte boundary is simply a shorter slice — its header parses,
//!   the picture plans, every later reference resolves against a DPB entry that
//!   exists. (pf-bitstream's `TruncatedAu` warning is a narrower thing: a NALU
//!   whose HEADER is malformed with real data still behind it.) What the hardware
//!   gets is a slice whose bitstream ends mid-picture, which is a decode error it
//!   can report — so this is the mode that fires the DRIVER's detector
//!   deterministically.
//! * [`FaultMode::Flip`] — one byte deep inside the slice payload is altered. The
//!   bitstream still parses, every reference still resolves, the planner has
//!   nothing to say — and the picture decodes WRONG, possibly without the driver
//!   minding either (an entropy decoder happily decodes garbage into macroblocks).
//!   This is precisely the Xbox Ally X class: corruption that reaches the screen
//!   with nothing in the pipeline objecting. It is the mode that shows what
//!   `RESULT_STATUS` can and cannot promise.
//!
//! The consequence worth stating plainly, because it is the program's whole thesis:
//! two of the three modes are invisible to every FFmpeg rung by construction
//! (`nb_queries = 0`, no `AV_FRAME_FLAG_CORRUPT`), and invisible to the native lane
//! too on a driver without `queryResultStatusSupport` (RADV). A session that cannot
//! answer the status query is not a clean session; it is an unmeasured one, and the
//! telemetry says so rather than reporting zeros.
//!
//! # Invocation
//!
//! `PUNKTFUNK_AU_FAULT=<mode>[:<period>]` on any desktop client —
//! `drop`, `truncate`, `flip`, default period 60 (once a second at 60 fps):
//!
//! ```text
//! PUNKTFUNK_AU_FAULT=drop:120 PUNKTFUNK_DECODER=native-vulkan punktfunk-session --connect host
//! ```
//!
//! Every `period`-th AU is faulted, counting from the first one the decoder is
//! offered, so the parameter sets and opening IDR of a session ride through
//! untouched at any period above 1.
//!
//! # What the injector is NOT in the same lane as
//!
//! `PUNKTFUNK_AU_DUMP` (the client's `au_dump` fixture capture) writes the AU as
//! it arrives from the wire, and this injector runs LATER — at the native
//! backend's decode entry, the last point before pf-bitstream. So on a faulted
//! run the dumped fixture is the CLEAN bitstream, not the one the decoder saw;
//! replaying it will not reproduce the damage. That ordering is deliberate: the
//! dump is what the HOST sent (the artefact a host-side bug is diagnosed from),
//! and moving the injector above it would corrupt every backend's input rather
//! than only the lane whose detectors it exists to fire. To capture the damaged
//! bytes, reconstruct them from the spec — the injector is pure and deterministic,
//! which is precisely what makes that possible.

/// What to do to a faulted access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    /// Swallow the AU entirely — the decoder never sees it (network loss).
    Drop,
    /// Deliver a prefix of the AU: a picture whose slice data stops mid-frame.
    Truncate,
    /// Deliver the whole AU with one payload byte altered (in-picture corruption
    /// no parser can see).
    Flip,
}

/// What the caller must do with the access unit it was about to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultAction {
    /// Untouched — feed the original bytes. The answer for every AU but one in
    /// `period`, and for every AU of a session with no fault armed.
    Pass,
    /// Do not feed this AU at all.
    Drop,
    /// Feed these bytes instead. Owned because corruption is by definition not a
    /// borrow of the input; allocated only on the faulted AU, never on the
    /// streaming path.
    Corrupt(Vec<u8>),
}

/// The default fault period: one AU per second at 60 fps — frequent enough to see
/// within a few seconds of streaming, rare enough that recovery completes between
/// faults instead of the stream never leaving its post-loss freeze.
pub const DEFAULT_FAULT_PERIOD: u32 = 60;

/// Where in a faulted AU the damage lands, as a fraction of its length. Deep
/// enough to be past the parameter sets and the first slice header (so `Flip`
/// really is invisible to the parser and `Truncate` really does cut mid-picture
/// rather than refusing the AU at byte 0), and expressed as a fraction so it holds
/// for a 700-byte P-frame and a 4 MB IDR alike.
const FAULT_POINT_NUMERATOR: usize = 3;
const FAULT_POINT_DENOMINATOR: usize = 4;

/// The armed injector: a mode, a period, and the count of AUs offered so far.
#[derive(Debug, Clone, Copy)]
pub struct AuFault {
    mode: FaultMode,
    period: u32,
    seen: u32,
}

impl AuFault {
    /// Parse a `PUNKTFUNK_AU_FAULT` spec: `<mode>[:<period>]`. `None` for anything
    /// unrecognized, which is what keeps the injector inert — a typo must leave a
    /// user's stream alone rather than half-arm it.
    pub fn from_spec(spec: &str) -> Option<AuFault> {
        let spec = spec.trim();
        let (mode, period) = match spec.split_once(':') {
            Some((m, p)) => (m.trim(), p.trim().parse::<u32>().ok()?),
            None => (spec, DEFAULT_FAULT_PERIOD),
        };
        // A zero period would fault EVERY AU including the opening IDR, which
        // never produces a stream to damage in the first place.
        if period == 0 {
            return None;
        }
        let mode = match mode {
            "drop" => FaultMode::Drop,
            "truncate" => FaultMode::Truncate,
            "flip" => FaultMode::Flip,
            _ => return None,
        };
        Some(AuFault {
            mode,
            period,
            seen: 0,
        })
    }

    /// Build one directly (tests and callers that resolve the spec themselves).
    pub fn new(mode: FaultMode, period: u32) -> AuFault {
        AuFault {
            mode,
            period: period.max(1),
            seen: 0,
        }
    }

    pub fn mode(&self) -> FaultMode {
        self.mode
    }

    pub fn period(&self) -> u32 {
        self.period
    }

    /// Offer one access unit. Returns what the caller should feed the decoder.
    ///
    /// The counter advances on EVERY call, faulted or not, so the cadence is a
    /// property of the stream rather than of the damage: `period` AUs of clean
    /// stream, one fault, repeat.
    pub fn apply(&mut self, au: &[u8]) -> FaultAction {
        self.seen = self.seen.wrapping_add(1);
        if self.seen % self.period != 0 {
            return FaultAction::Pass;
        }
        // Too short to damage meaningfully — a handful of bytes is a parameter-set
        // AU or a fragment, and cutting/flipping inside one tests the parser's
        // error handling rather than the decoder's integrity signals. Pass it and
        // let the next multiple carry the fault.
        if au.len() < 16 {
            return FaultAction::Pass;
        }
        let point = au.len() * FAULT_POINT_NUMERATOR / FAULT_POINT_DENOMINATOR;
        match self.mode {
            FaultMode::Drop => FaultAction::Drop,
            FaultMode::Truncate => FaultAction::Corrupt(au[..point].to_vec()),
            FaultMode::Flip => {
                let mut bytes = au.to_vec();
                // XOR with a single high-ish bit rather than inverting the byte:
                // it moves the sample values the entropy coder decodes without
                // being especially likely to manufacture a `00 00 01` start code
                // out of the surrounding bytes (which would turn a payload
                // corruption into a framing corruption and quietly change which
                // detector the mode is testing).
                bytes[point] ^= 0x40;
                FaultAction::Corrupt(bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The knob is a support tool: it has to be exactly as inert as it looks when
    /// unset or mistyped, because the alternative is a user's stream quietly
    /// breaking on a typo'd environment variable.
    #[test]
    fn only_a_well_formed_spec_arms_the_injector() {
        assert_eq!(
            AuFault::from_spec("drop").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Drop, DEFAULT_FAULT_PERIOD))
        );
        assert_eq!(
            AuFault::from_spec("truncate:5").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Truncate, 5))
        );
        assert_eq!(
            AuFault::from_spec(" flip : 30 ").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Flip, 30))
        );
        for bad in [
            "", "1", "off", "drop:", "drop:0", "drop:x", "corrupt", "flip:-1", ":30",
        ] {
            assert!(AuFault::from_spec(bad).is_none(), "{bad:?} must not arm");
        }
    }

    /// The cadence: `period - 1` clean AUs, then one faulted, forever. The count
    /// starts at the first AU offered, so a period above 1 never touches the
    /// session's opening parameter sets and IDR.
    #[test]
    fn every_nth_au_is_faulted_and_the_rest_pass_through_untouched() {
        let au = vec![0xA5u8; 64];
        let mut f = AuFault::new(FaultMode::Drop, 3);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Drop);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Drop);
    }

    /// Truncation delivers a real prefix — the shape a lost tail shard has, not a
    /// zero-length AU (which is a different, uninteresting failure).
    #[test]
    fn truncation_delivers_a_prefix_of_the_original() {
        let au: Vec<u8> = (0..100u8).collect();
        let mut f = AuFault::new(FaultMode::Truncate, 1);
        let FaultAction::Corrupt(short) = f.apply(&au) else {
            panic!("truncate must corrupt");
        };
        assert_eq!(short.len(), 75, "three quarters of the AU survive");
        assert_eq!(short[..], au[..75], "and they are the ORIGINAL bytes");
    }

    /// A flip alters exactly one byte, deep in the payload, deterministically —
    /// the corruption a parser cannot see. Determinism is what makes a field
    /// report reproducible from the spec string alone.
    #[test]
    fn a_flip_changes_exactly_one_deep_payload_byte_and_is_reproducible() {
        let au: Vec<u8> = (0..=255u8).collect();
        let run = || {
            let mut f = AuFault::new(FaultMode::Flip, 1);
            match f.apply(&au) {
                FaultAction::Corrupt(bytes) => bytes,
                other => panic!("flip must corrupt, got {other:?}"),
            }
        };
        let bytes = run();
        assert_eq!(
            bytes.len(),
            au.len(),
            "length is untouched — this is not a cut"
        );
        let differing: Vec<usize> = (0..au.len()).filter(|&i| bytes[i] != au[i]).collect();
        assert_eq!(differing.len(), 1, "exactly one byte moves");
        let at = differing[0];
        assert_eq!(at, 192, "three quarters in — past the headers");
        assert_eq!(bytes[at], au[at] ^ 0x40);
        assert_eq!(run(), bytes, "the same spec produces the same damage");
    }

    /// Tiny AUs (a lone parameter-set NALU, a fragment) are passed through: the
    /// modes are about damaging a PICTURE, and cutting a 6-byte AU only tests the
    /// parser's own bounds checks.
    #[test]
    fn an_au_too_short_to_damage_meaningfully_is_left_alone() {
        let mut f = AuFault::new(FaultMode::Truncate, 1);
        assert_eq!(f.apply(&[0u8; 8]), FaultAction::Pass);
        // …and the counter still advanced, so the cadence does not stall waiting
        // for a big enough AU.
        assert!(matches!(f.apply(&[0u8; 64]), FaultAction::Corrupt(_)));
    }
}
