//! Pure, deterministic decoder-input fault injection for integrity testing.
//!
//! [`AuFault::from_spec`] accepts `PUNKTFUNK_AU_FAULT=<mode>[:<period>]`; invalid
//! or zero-period specifications do not arm it, and the default period is 60.
//! Every period-th offered AU is selected, counting from the first; inputs shorter
//! than 16 bytes pass unchanged rather than corrupting parameter-only fragments.
//! [`FaultMode::Drop`] withholds the AU and normally exposes a later planner gap or
//! missing reference. `Truncate` keeps the first three quarters and targets driver
//! decode-status detection. `Flip` XORs one byte at the same point and may remain
//! valid syntax, so neither planner nor driver is guaranteed to report it.
//! Lack of `queryResultStatusSupport` therefore means unmeasured, not clean.
//! Unselected AUs are borrowed unchanged; corrupted AUs allocate owned bytes only
//! on the selected call. The injector runs after `PUNKTFUNK_AU_DUMP`, so dumps
//! remain clean wire input and reproduce faults only by reapplying the same spec.

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
