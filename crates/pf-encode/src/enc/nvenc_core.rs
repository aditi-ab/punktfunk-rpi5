//! Shared direct-SDK NVENC core — the platform-agnostic pieces of the two `nvEncodeAPI` backends,
//! Windows D3D11 (`encode/windows/nvenc.rs`) and Linux CUDA (`encode/linux/nvenc_cuda.rs`), so the
//! byte-identical glue lives once (plan §2.2, the direct-NVENC Tier-2). The per-platform parts —
//! the entry-table load (`nvEncodeAPI64.dll` via `LoadLibrary` vs `libnvidia-encode.so` via
//! `libloading`), the device binding (D3D11 vs CUDA), input-surface registration, and the
//! Windows-only async retrieve — stay in their backends. Sibling of [`super::nvenc_status`].

// UNSAFE-LINT EXEMPTION REMOVED — the old fence rationale ("raw nvEncodeAPI entry-table calls
// almost line for line") was false for this file: it makes ZERO FFI calls. Its unsafe surface is
// C-union writes whose soundness hangs entirely on which codec arm is active, and the 4:4:4 note
// below records the shipped bug (hevcConfig bytes stamped onto an AV1 config) that per-operation
// blocks make visible. So this file runs the strictest discipline in the crate: every union
// access sits in its own `unsafe {}` block naming the codec guard it relies on.
#![deny(clippy::multiple_unsafe_ops_per_block)]

use super::Codec;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// Local `NVENCSTATUS` → `Result` (replaces the sdk's `result_without_string`, which lives in the
/// crate's `safe` module — code these backends must not pull in). The raw status's Debug repr
/// (`NV_ENC_ERR_INVALID_PARAM`, …) is the error payload; callers fold it through
/// [`super::nvenc_status`] for an operator-actionable cause.
pub(super) trait NvStatusExt {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS>;
}
impl NvStatusExt for nv::NVENCSTATUS {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS> {
        match self {
            nv::NVENCSTATUS::NV_ENC_SUCCESS => Ok(()),
            err => Err(err),
        }
    }
}

/// The NVENC codec GUID for a session [`Codec`]. PyroWave never opens the direct-NVENC backend
/// (guarded by the `open_video` dispatch), so it is unreachable here.
pub(super) fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }
}

/// Resolved per-frame slice count for a session (latency plan §7 LN1, Phase 3): the
/// `PUNKTFUNK_NVENC_SLICES` env override wins (1..=32; **1 = the explicit single-slice
/// escape**, needed now that a backend can default higher), else the backend's
/// `default_slices` — on Linux direct-NVENC the Phase-3 default of 4 CLAMPED to the session's
/// negotiated client-decoder ceiling (`VIDEO_CAP_MULTI_SLICE` / GameStream's
/// `videoEncoderSlicesPerFrame` — a client that never asked stays single-slice: Amlogic TV
/// SoCs wedge on multi-slice AUs), 1 everywhere else (the Windows async path is deliberately
/// untouched). H.264/HEVC only (AV1 partitions via tiles). ONE parse shared by the config
/// author ([`apply_low_latency_config`] via [`LowLatencyConfig::slices`]) and the Linux
/// backend's chunked-poll arming, so the two can never disagree about whether a session is
/// multi-slice.
pub(super) fn resolve_slices(codec: Codec, default_slices: u32) -> u32 {
    if !matches!(codec, Codec::H264 | Codec::H265) {
        return 1;
    }
    std::env::var("PUNKTFUNK_NVENC_SLICES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| (1..=32).contains(n))
        .unwrap_or(default_slices)
}

/// Resolved sub-frame readback (`enableSubFrameWrite` + `reportSliceOffsets`; sync sessions
/// only, see [`build_init_params`]): `PUNKTFUNK_NVENC_SUBFRAME` tri-state — `0` = never (the
/// default-on escape), `1` = force (even where the caps probe says unsupported — an operator
/// explicitly testing), unset = the backend's `default_on` — which is the GPU's
/// `SUBFRAME_READBACK` caps-probe result on **both** backends now (Linux since Phase 3, Windows
/// since the 2026-07-31 `.173` A/B). This comment used to say "Windows passes `false`"; it had
/// been stale since that flip, which mattered because it made the AUTO-plus-sub-frame dead
/// combination look Linux-only when it is fleet-wide.
pub(super) fn resolve_subframe(default_on: bool) -> bool {
    match std::env::var("PUNKTFUNK_NVENC_SUBFRAME").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => default_on,
    }
}

/// Whether the operator EXPLICITLY forced sub-frame readback on (`PUNKTFUNK_NVENC_SUBFRAME=1`)
/// — the log-severity input to [`resolve_split_subframe`]: a forced knob being overridden
/// deserves a `warn`, a default being tuned an `info`. Callers LATCH this once next to their
/// resolved subframe state (an env re-read at reconfigure would violate the "open and
/// reconfigure present identical init params" invariant).
/// Both direct-SDK backends latch it now: Linux at the `nvenc_cuda` query_caps latch, Windows at
/// session init since sub-frame defaults on there too (it used to be env opt-in only, so
/// `subframe == forced` held by construction and the item was Linux-cfg'd to avoid being dead
/// code on the Windows leg — the recurring item-level `dead_code` trap).
#[cfg(any(target_os = "linux", windows))]
pub(super) fn subframe_env_forced() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_NVENC_SUBFRAME").as_deref(),
        Ok("1")
    )
}

/// The split-encode × sub-frame arbitration (Phase 8; verified against `nvEncodeAPI.h`'s own
/// `splitEncodeMode` doc, not folklore):
/// - **H.264**: split "is not applicable" — hard-DISABLE the mode so the written config, the
///   ceiling-cache key, the split diagnostic log and the rejection-retry all stay truthful (the
///   retry used to re-open a byte-identical session after an H.264 "split rejection").
/// - **HEVC**: split is "not supported if … subframe mode" — when WE force split
///   (TWO/THREE/AUTO_FORCED, e.g. the 4K120 throughput requirement), sub-frame yields. Under
///   plain AUTO the driver arbitrates — the shipped fleet state (1080p–1440p240 all run
///   AUTO+subframe); keying on `!= DISABLE` here would have disarmed the Phase-3 chunked-poll
///   feature fleet-wide.
/// - **AV1**: split passes through untouched (it is constrained only by output-into-vidmem,
///   which we never use), but sub-frame readback is **always disarmed** — see below.
///
/// # Why AV1 must never arm sub-frame readback
///
/// The two halves of this feature are armed by different conditions, and for AV1 they can only
/// ever disagree:
///
/// * the WRITER (`enableSubFrameWrite` + `reportSliceOffsets`) is armed by
///   [`build_init_params`] from this `subframe` alone;
/// * the READER ([`Encoder::poll_chunk`]'s `subframe_chunks` latch) additionally requires
///   `slices >= 2`, and [`resolve_slices`] returns 1 for AV1 **unconditionally** — before the
///   `PUNKTFUNK_NVENC_SLICES` override is even read, because AV1 partitions via tiles rather
///   than slices.
///
/// So an AV1 session armed the driver to publish its output unit by unit and then read it with
/// a single blocking `lock_bitstream`, which returns only the FIRST completed unit. One tile
/// per frame reached the wire. Measured on `.21` (RTX 5070 Ti, 4K60, split AUTO): every frame
/// carried a frame header declaring two tile rows and a single Tile Group OBU with
/// `tg_start = tg_end = 0` — half the picture missing — and libdav1d rejected **835 of 836**
/// access units with "Error parsing frame header". NVIDIA's hardware decoder accepts the
/// truncated stream, which is why native Vulkan Video looked healthy while both conformant
/// software decoders (rav1d in-tree, libdav1d out-of-tree) refused every frame. With sub-frame
/// disarmed and split still AUTO, the same session decoded 654/654 frames clean.
///
/// This is a plain disarm, NOT a codec restriction: `split_mode` is returned untouched, so AV1
/// keeps every engine split encode gives it. Arming the reader for AV1 instead is not a
/// drop-in alternative — `poll_chunk` cuts at `bitstreamSizeInBytes` on the reasoning that
/// "slices are contiguous Annex-B", which AV1's OBUs are not.
///
/// # A tile-aware chunk reader was considered and is CLOSED, not deferred
///
/// The obvious follow-up is to teach the reader AV1's units — cut on OBU boundaries instead
/// of byte counts and arm from the driver's reported unit count — so AV1 gets the sub-frame
/// latency win HEVC gets (ship tile 1 while tile 2 encodes). Measured on `.21` (RTX 5070 Ti,
/// `av1_nvenc`, 2026-08-07) before writing any of it, and the measurement closes it:
///
/// * **4K carries two tiles, and they share ONE Tile Group OBU.** The frame header reads
///   `width_in_sbs_minus_1[0] = 59` (one tile column, the full 3840) and
///   `height_in_sbs_minus_1[] = {16, 16}` (two tile rows) — but
///   `tile_start_and_end_present_flag = 0`, which puts both tiles in a single Tile Group
///   OBU. There is no OBU boundary between them to cut on. Shipping tile 1 early would mean
///   the HOST re-authoring AV1 syntax per chunk — synthesising a fresh Tile Group OBU header
///   with `tile_start_and_end_present_flag = 1` and its own `tg_start`/`tg_end` — which is
///   bitstream surgery on the encode path, not a reader change.
/// * **1080p carries one tile** (`tile_cols_log2 = tile_rows_log2 = 0`), so there is nothing
///   to pipeline at the commonest streaming resolution regardless.
/// * **The prize is small even at 4K, because split encode already spent it.** The two tile
///   rows go to two split-encode engines that run CONCURRENTLY, so they finish at nearly the
///   same moment: the win is bounded by the skew between engines, not by half the frame.
///   Whole-frame encode measures 3.3–3.6 ms at 4K60 against a 16.7 ms p50 end-to-end, so
///   even the sequential-tiles fantasy caps out near 1.7 ms and the real figure is a
///   fraction of that. HEVC's sub-frame win is larger for a structural reason that does not
///   transfer: forced split and sub-frame are mutually unsupported (below), so HEVC's slices
///   really are produced one after another.
///
/// Reopen only if NVENC starts emitting one OBU per tile, or sets
/// `tile_start_and_end_present_flag = 1` — at that point the cut points exist and the reader
/// change becomes the small piece it was assumed to be.
///
/// Returns the `(split_mode, subframe)` to ACTUALLY configure. The caller must store BOTH back
/// (the chunked-poll latch and `CeilingKey` key on them) — a silent in-params drop would leave
/// `poll_chunk` busy-polling its full budget every AU (`numSlices` stays 0 without
/// `reportSliceOffsets`, so neither loop exit ever fires).
pub(super) fn resolve_split_subframe(
    codec: Codec,
    split_mode: u32,
    subframe: bool,
    subframe_forced: bool,
) -> (u32, bool) {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    if codec == Codec::H264 {
        return (M::NV_ENC_SPLIT_DISABLE_MODE as u32, subframe);
    }
    // AV1: disarm sub-frame, keep split. The reader can never arm here (`resolve_slices` gives
    // AV1 one slice by construction), so arming the writer only truncates every frame to its
    // first tile — see this function's docs for the measurement.
    if codec == Codec::Av1 && subframe {
        if subframe_forced {
            tracing::warn!(
                split_mode,
                "PUNKTFUNK_NVENC_SUBFRAME=1 cannot be honoured on AV1 — its sub-frame units are \
                 TILES and the chunked reader cuts on Annex-B slice boundaries, so arming the \
                 writer would ship only the first tile of every frame; sub-frame readback \
                 disabled for this session (split encode is unaffected)"
            );
        } else {
            tracing::debug!(
                split_mode,
                "NVENC: sub-frame readback disarmed on AV1 (tiles, not slices — nothing consumes \
                 the chunks); split encode is unaffected"
            );
        }
        return (split_mode, false);
    }
    let split_forced = split_mode == M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        || split_mode == M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        || split_mode == M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32;
    if codec == Codec::H265 && split_forced && subframe {
        if subframe_forced {
            tracing::warn!(
                split_mode,
                "HEVC forced split-encode and PUNKTFUNK_NVENC_SUBFRAME=1 are mutually \
                 unsupported (nvEncodeAPI.h) — sub-frame readback disabled for this session; \
                 set PUNKTFUNK_SPLIT_ENCODE=0 to choose sub-frame instead"
            );
        } else {
            tracing::info!(
                split_mode,
                "HEVC forced split-encode supersedes default-on sub-frame readback (mutually \
                 unsupported per nvEncodeAPI.h; split is the 4K120 throughput lever) — set \
                 PUNKTFUNK_SPLIT_ENCODE=0 to choose sub-frame instead"
            );
        }
        return (split_mode, false);
    }
    // The silently-inert combination, made visible. HEVC + plain AUTO + sub-frame: the driver
    // cannot split (mutually unsupported) so it resolves AUTO to no-split — MEASURED on `.21` at
    // 4K, AUTO+sub-frame 5023/5157 µs vs DISABLE's 4979/5000, while the same AUTO with sub-frame
    // OFF splits at 2401/2352 vs TWO_FORCED's 2319/2378. This is the fleet's default shape, so
    // "split_mode=AUTO" in a log has meant "no split" for every default session and nothing said
    // so. Deliberately NOT rewritten to DISABLE: the mode we pass is what the driver was actually
    // given, and the ceiling-cache key must keep describing that.
    if codec == Codec::H265 && subframe && split_mode == M::NV_ENC_SPLIT_AUTO_MODE as u32 {
        tracing::debug!(
            "NVENC: split-encode AUTO with sub-frame readback on — the driver cannot split HEVC \
             in this combination, so this session runs SINGLE-ENGINE (measured). Set \
             PUNKTFUNK_NVENC_SUBFRAME=0 to trade sub-frame for a real split."
        );
    }
    (split_mode, subframe)
}

#[cfg(test)]
mod split_subframe_tests {
    use super::{resolve_slices, resolve_split_subframe, Codec};
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    const AUTO: u32 = M::NV_ENC_SPLIT_AUTO_MODE as u32;
    const TWO: u32 = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;
    const AUTO_F: u32 = M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32;
    const DISABLE: u32 = M::NV_ENC_SPLIT_DISABLE_MODE as u32;

    /// THE FLEET CASE: plain AUTO + default-on sub-frame must pass through untouched — the
    /// driver arbitrates. Keying the rule on `!= DISABLE` would disarm sub-frame on every
    /// default Linux HEVC session (AUTO == 0 is the resolver's fallthrough).
    #[test]
    fn hevc_auto_keeps_subframe() {
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, true, false),
            (AUTO, true)
        );
    }

    #[test]
    fn hevc_forced_split_drops_subframe() {
        assert_eq!(
            resolve_split_subframe(Codec::H265, TWO, true, false),
            (TWO, false)
        );
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO_F, true, true),
            (AUTO_F, false)
        );
        // No sub-frame to drop → nothing changes.
        assert_eq!(
            resolve_split_subframe(Codec::H265, TWO, false, false),
            (TWO, false)
        );
        // Explicitly disabled split → sub-frame kept (the documented escape).
        assert_eq!(
            resolve_split_subframe(Codec::H265, DISABLE, true, true),
            (DISABLE, true)
        );
    }

    /// H.264: split "is not applicable" (nvEncodeAPI.h) — hard-DISABLE regardless of the
    /// resolved mode; sub-frame (H.264 slices) is unaffected.
    #[test]
    fn h264_split_hard_disabled() {
        assert_eq!(
            resolve_split_subframe(Codec::H264, TWO, true, false),
            (DISABLE, true)
        );
        assert_eq!(
            resolve_split_subframe(Codec::H264, AUTO, false, false),
            (DISABLE, false)
        );
    }

    /// ⚠ DO NOT "SIMPLIFY" THE `AUTO` ARM AWAY. Measured on `.21` at 4K, plain `AUTO` is
    /// conditional, not dead:
    ///   sub-frame ON  → 5023/5157 µs ≈ DISABLE 4979/5000   (cannot split — mutually unsupported)
    ///   sub-frame OFF → 2401/2352 µs ≈ TWO_FORCED 2319/2378 (DOES split)
    /// An earlier read of the sub-frame-ON measurement alone concluded "AUTO never splits, retire
    /// it" — that would have silently cost every sub-frame-off session its second engine. This
    /// test pins the arbitration's half of the contract: AUTO must survive both ways.
    #[test]
    fn auto_survives_the_arbitration_in_both_subframe_states() {
        // Sub-frame on: kept as AUTO (inert, but that is the driver's call, and rewriting it to
        // DISABLE would lie to the ceiling-cache key about what the session was given).
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, true, false),
            (AUTO, true)
        );
        // Sub-frame off: still AUTO, and here it is a REAL split — the arm must not be demoted.
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, false, false),
            (AUTO, false)
        );
    }

    /// AV1 KEEPS ITS SPLIT AND LOSES ITS SUB-FRAME, and this test is the one that used to
    /// assert the bug.
    ///
    /// It read `av1_untouched` and pinned `(TWO, true)` on the reasoning that "both features
    /// are legal together (sub-frame is per-tile)". Legal for the DRIVER, yes — but the two
    /// halves of the feature are armed by different conditions in this crate, and on AV1 they
    /// cannot agree: `build_init_params` arms the WRITER from `subframe`, while the READER
    /// needs `slices >= 2` and [`resolve_slices`] returns 1 for AV1 before the env override is
    /// even read. So the session told the driver to publish tile by tile and then took only the
    /// first tile with one blocking lock. Every 4K AV1 frame shipped half a picture; libdav1d
    /// rejected 835/836 AUs, and only NVIDIA's lenient hardware decoder hid it.
    ///
    /// The `true` argument here is `subframe_forced` — even an operator's explicit
    /// `PUNKTFUNK_NVENC_SUBFRAME=1` cannot buy a working AV1 sub-frame session, so it is
    /// refused (loudly) rather than honoured into a truncated stream.
    #[test]
    fn av1_keeps_split_but_never_arms_subframe() {
        // Forced split + forced sub-frame: split survives, sub-frame does not.
        assert_eq!(
            resolve_split_subframe(Codec::Av1, TWO, true, true),
            (TWO, false),
            "AV1 must keep its split mode and drop sub-frame readback"
        );
        // The fleet shape (plain AUTO + default-on sub-frame) — the one that shipped broken.
        assert_eq!(
            resolve_split_subframe(Codec::Av1, AUTO, true, false),
            (AUTO, false)
        );
        // Widest split, still untouched: this fix costs AV1 no engines.
        assert_eq!(
            resolve_split_subframe(Codec::Av1, AUTO_F, true, false),
            (AUTO_F, false)
        );
        // Already off stays off, and split still passes through.
        assert_eq!(
            resolve_split_subframe(Codec::Av1, TWO, false, false),
            (TWO, false)
        );
    }

    /// The two halves of the sub-frame feature, checked against each other on AV1 — the
    /// comparison nothing made, which is why the truncation shipped.
    ///
    /// The reader's gate is `subframe_chunks = slices >= 2 && subframe_on && sync`. For AV1
    /// [`resolve_slices`] returns 1 *by construction* (it early-returns for non-H.26x before
    /// reading `PUNKTFUNK_NVENC_SLICES`, which is also what makes this test independent of the
    /// environment it runs in), so the reader can never arm — and therefore the writer must
    /// never arm either.
    ///
    /// Deliberately NOT generalised to a loop over all codecs: for H.264/HEVC `slices` is
    /// `sliceModeData`, so a single-slice session really does produce ONE output unit and a
    /// single blocking lock is complete. The hazard is specific to a codec whose unit count the
    /// driver decides (AV1's tiles, via split encode) rather than our slice config. If AV1 ever
    /// becomes genuinely multi-slice here, the first assert fires and sends whoever changed it
    /// back to the arming rule.
    #[test]
    fn av1_can_never_arm_the_chunked_reader_so_it_must_not_arm_the_writer() {
        assert_eq!(
            resolve_slices(Codec::Av1, 4),
            1,
            "AV1 is single-slice by construction — `subframe_chunks` (slices >= 2) cannot arm"
        );
        let (_, subframe) = resolve_split_subframe(Codec::Av1, AUTO, true, false);
        assert!(
            !subframe,
            "the sub-frame WRITER is armed on a session whose chunked READER cannot arm: every \
             frame would reach the wire truncated to its first tile"
        );
    }
}

// Split arbitration now runs on BOTH direct-SDK backends, so these are gated to the union of
// the two rather than to Linux. Kept gated at all because `nvenc_core` is also reachable from
// builds where neither backend is compiled, and an ungated item there is the item-level
// dead_code trap this file already carries three scars from (see `subframe_env_forced`).
#[cfg(any(target_os = "linux", windows))]
/// What the split arbiter wants the backend to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArbAction {
    /// Reconfigure the live session to this split mode (in place — S1 proved this is IDR-free).
    SwitchTo(u32),
    /// Arbitration finished; this mode won and the arbiter will ask for nothing further.
    Settled(u32),
}

#[cfg(any(target_os = "linux", windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArbState {
    MeasuringIncumbent,
    Settling,
    MeasuringChallenger,
    Done,
}

#[cfg(any(target_os = "linux", windows))]
/// Picks the faster of two NVENC split modes **on the live session**, by measuring both.
///
/// This exists because the alternative — predicting the right mode at open — cannot work: the
/// decision depends on bits/frame, and for an Automatic client the host does not know the
/// steady-state bitrate at open (ABR climbs in place afterwards). Spike S1 showed
/// `nvEncReconfigureEncoder` accepts a changed `splitEncodeMode` with `resetEncoder=0`, emits **no
/// IDR**, and genuinely takes effect — so the encoder can simply try both and keep the winner,
/// with nothing visible on the wire.
///
/// Deliberately measures rather than models: hard-coded per-architecture constants are exactly how
/// the rule this replaces went wrong (one 5120×1440@240 Ada datapoint generalised into a fleet-wide
/// 10-bit veto). A measurement tracks driver updates for free.
///
/// ⚠ **`SETTLE_FRAMES` is load-bearing, not padding.** Split-encode does not reach steady state on
/// the first frame — a *fresh* `TWO_FORCED` session measured early-half 3280 µs against late-half
/// 1996 on `.21`. Judging an arm immediately after switching to it reads the transient, and does so
/// **intermittently**, which is the worst failure mode: the verdict would be wrong only sometimes,
/// and then be cached.
pub(super) struct SplitArbiter {
    state: ArbState,
    incumbent: u32,
    challenger: u32,
    samples: Vec<u64>,
    incumbent_us: u64,
    settle_left: u32,
    /// Latency the challenger COSTS beyond its encode time, added to its measured result before
    /// the comparison. Non-zero only when winning the split means giving up sub-frame readback:
    /// sub-frame lets the send overlap the encode, so losing it pushes the AU's last byte out by
    /// roughly `send_spread × (slices−1)/slices`. Without this term the arbiter compares encode
    /// against encode, always prefers split on HEVC, and makes end-to-end latency worse while
    /// reporting a win.
    challenger_handicap_us: u64,
}

/// Frames discarded after a switch before the challenger is judged (measured — see the struct doc).
#[cfg(any(target_os = "linux", windows))]
const SETTLE_FRAMES: u32 = 16;
/// Frames measured per arm. Long enough to median out content variation, short enough that the
/// whole arbitration is over in well under a second at 60 fps.
#[cfg(any(target_os = "linux", windows))]
const SAMPLE_FRAMES: usize = 24;
/// The challenger must beat the incumbent by this much to win. Switching is not free (a
/// reconfigure, and for HEVC it costs sub-frame readback), so a coin-flip difference should leave
/// the session where it already is.
#[cfg(any(target_os = "linux", windows))]
const WIN_MARGIN_PCT: u64 = 10;

#[cfg(any(target_os = "linux", windows))]
impl SplitArbiter {
    /// `handicap_us` is what the challenger costs OUTSIDE the encode it is measured on — pass `0`
    /// when it gives up nothing. See [`Self::challenger_handicap_us`].
    pub(super) fn with_handicap(incumbent: u32, challenger: u32, handicap_us: u64) -> Self {
        Self {
            state: ArbState::MeasuringIncumbent,
            incumbent,
            challenger,
            samples: Vec::with_capacity(SAMPLE_FRAMES),
            incumbent_us: 0,
            settle_left: 0,
            challenger_handicap_us: handicap_us,
        }
    }

    /// Feed one frame's encode time. Returns an action when the arbiter wants the session changed.
    pub(super) fn on_frame(&mut self, us: u64) -> Option<ArbAction> {
        match self.state {
            ArbState::Done => None,
            ArbState::Settling => {
                self.settle_left = self.settle_left.saturating_sub(1);
                if self.settle_left == 0 {
                    self.state = ArbState::MeasuringChallenger;
                    self.samples.clear();
                }
                None
            }
            ArbState::MeasuringIncumbent => {
                self.samples.push(us);
                if self.samples.len() < SAMPLE_FRAMES {
                    return None;
                }
                self.incumbent_us = median(&mut self.samples);
                self.state = ArbState::Settling;
                self.settle_left = SETTLE_FRAMES;
                Some(ArbAction::SwitchTo(self.challenger))
            }
            ArbState::MeasuringChallenger => {
                self.samples.push(us);
                if self.samples.len() < SAMPLE_FRAMES {
                    return None;
                }
                // Compare TOTAL cost, not encode cost: whatever the challenger gives up outside
                // the encode (on HEVC, the sub-frame send overlap) is charged to it here.
                let challenger_us = median(&mut self.samples) + self.challenger_handicap_us;
                self.state = ArbState::Done;
                // Strictly better by the margin, or the incumbent keeps the session. Equal-ish is
                // deliberately a win for the incumbent: we are already there.
                let threshold = self
                    .incumbent_us
                    .saturating_sub(self.incumbent_us.saturating_mul(WIN_MARGIN_PCT) / 100);
                if challenger_us < threshold {
                    tracing::info!(
                        winner = self.challenger,
                        winner_us = challenger_us,
                        loser = self.incumbent,
                        loser_us = self.incumbent_us,
                        "NVENC split arbitration: challenger wins — keeping it"
                    );
                    Some(ArbAction::Settled(self.challenger))
                } else {
                    tracing::info!(
                        winner = self.incumbent,
                        winner_us = self.incumbent_us,
                        loser = self.challenger,
                        loser_us = challenger_us,
                        "NVENC split arbitration: incumbent held — switching back"
                    );
                    // The session is currently running the challenger, so returning to the
                    // incumbent is an actual reconfigure, not a no-op.
                    Some(ArbAction::SwitchTo(self.incumbent))
                }
            }
        }
    }

    pub(super) fn is_done(&self) -> bool {
        self.state == ArbState::Done
    }
}

#[cfg(any(target_os = "linux", windows))]
fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// One session config's identity for the process-lifetime bitrate-ceiling cache
/// ([`cached_ceiling`]/[`store_ceiling`]). Everything the driver's codec-level validation keys
/// off: the GPU (different NVENC generations have different level ceilings), dims/fps (the luma
/// rate selects the level), depth/chroma (they select the profile) and the split mode the
/// sessions ACTUALLY opened with (a split session budgets per engine).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct CeilingKey {
    /// GPU identity — Linux: the process-global shared `CUcontext` pointer; Windows: the render
    /// adapter LUID (0 when unresolved). Best effort: the cache is advisory (see
    /// [`cached_ceiling`]), so a colliding identity costs one failed open + re-search, never a
    /// wrong session.
    pub gpu: u64,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bit_depth: u8,
    pub chroma_444: bool,
    pub split_mode: u32,
}

fn ceilings() -> &'static std::sync::Mutex<std::collections::HashMap<CeilingKey, u64>> {
    static CEILINGS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CeilingKey, u64>>,
    > = std::sync::OnceLock::new();
    CEILINGS.get_or_init(Default::default)
}

/// The codec-level bitrate ceiling (bps) a previous clamp search discovered for `key` this
/// process lifetime, if any. ADVISORY: the consumer must treat a failed open at the cached value
/// as a stale entry (fall back to the full search, which rewrites it via [`store_ceiling`]) —
/// that self-healing is what lets the key's GPU identity be best-effort. What this buys: an ABR
/// overshoot on a config whose ceiling is already known opens (or in-place reconfigures) straight
/// AT the ceiling instead of re-running the ~6-open binary search and its ~half-second of session
/// churn per rebuild.
pub(super) fn cached_ceiling(key: &CeilingKey) -> Option<u64> {
    ceilings().lock().unwrap().get(key).copied()
}

/// Record the clamp search's discovered max accepted bitrate (bps) for `key`.
pub(super) fn store_ceiling(key: CeilingKey, bps: u64) {
    ceilings().lock().unwrap().insert(key, bps);
}

#[cfg(any(target_os = "linux", windows))]
/// A config's identity for the split-arbitration verdict cache — [`CeilingKey`] **minus
/// `split_mode`**, because the split mode is the thing being decided. Including it would key each
/// verdict under the arm that produced it and the cache could never answer "which arm should this
/// config use?".
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SplitKey {
    pub gpu: u64,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bit_depth: u8,
    pub chroma_444: bool,
}

#[cfg(any(target_os = "linux", windows))]
fn split_verdicts() -> &'static std::sync::Mutex<std::collections::HashMap<SplitKey, u32>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<SplitKey, u32>>> =
        std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

#[cfg(any(target_os = "linux", windows))]
/// The split mode a previous arbitration found fastest for `key` this process lifetime.
///
/// Process-lifetime and advisory, exactly like [`cached_ceiling`]: a session that reads a verdict
/// opens straight into the winning arm and skips the ~1 s exploration. It is NOT persisted — a
/// driver update can change the answer, and a stale verdict on disk would outlive its evidence
/// (persisting it needs the driver version in the key; see the plan's WP3).
pub(super) fn cached_split_verdict(key: &SplitKey) -> Option<u32> {
    split_verdicts().lock().unwrap().get(key).copied()
}

#[cfg(any(target_os = "linux", windows))]
/// Record an arbitration result for `key`.
pub(super) fn store_split_verdict(key: SplitKey, mode: u32) {
    split_verdicts().lock().unwrap().insert(key, mode);
}

#[cfg(any(target_os = "linux", windows))]
/// Drop every cached verdict. Test-only: the cache is process-global, so an on-hardware test that
/// runs an arbitration would otherwise leak its verdict into every later test that opens the same
/// config with `PUNKTFUNK_SPLIT_ENCODE` unset — which is exactly the shape the D5 legs use.
// Linux-only: its sole caller is `nvenc_cuda`'s arbitration on-hw test. Ungated it is dead
// code on Windows — the same item-level trap, now four times over.
#[cfg(all(test, target_os = "linux"))]
pub(super) fn clear_split_verdicts() {
    split_verdicts().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clamp_to_engines, max_forced_split_mode, resolve_split_mode};
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;

    // These assume PUNKTFUNK_SPLIT_ENCODE is unset (CI); an operator override deliberately wins.

    /// `encodeCodecConfig` is a C union, so the HEVC 4:4:4 arm must be codec-gated or it stamps
    /// `hevcConfig` bytes onto another codec's config. Before the gate this branch was reached on
    /// ANY codec with `chroma_444 && full_chroma_input` and stayed non-UB only because `lib.rs`
    /// degrades 4:4:4 for non-HEVC — a two-file invariant with nothing asserting it.
    ///
    /// It also had to stop swallowing the per-codec bit-depth arm: this is an `if`/`else if`, so a
    /// non-HEVC 4:4:4 session used to take the HEVC branch and get NEITHER 4:4:4 nor its own 10-bit
    /// setup. AV1 asserts the depth it actually needs.
    fn low_latency_cfg(codec: Codec, chroma_444: bool, bit_depth: u8) -> LowLatencyConfig {
        LowLatencyConfig {
            codec,
            bitrate: 20_000_000,
            fps: 60,
            custom_vbv: false,
            chroma_444,
            full_chroma_input: true,
            bit_depth,
            av1_input_depth_minus8: 0,
            hdr: false,
            rfi_supported: false,
            slices: 0,
        }
    }

    #[test]
    fn hevc_444_still_takes_the_frext_path() {
        // `NV_ENC_CONFIG` must NOT be `mem::zeroed` — `frameFieldMode`/`mvPrecision` are C enums
        // whose discriminants start at 1, so all-zero is not a valid value and Rust's own
        // zero-init check aborts the process. Production seeds it the same way, from `Default`
        // (then overwrites from the driver's preset).
        // SAFETY: `apply_low_latency_config` only writes into the caller's config (union writes
        // included) and makes no driver calls, so this is pure in-memory work.
        let cfg = unsafe {
            let mut cfg = nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            apply_low_latency_config(&mut cfg, low_latency_cfg(Codec::H265, true, 10));
            cfg
        };
        assert_eq!(cfg.profileGUID, nv::NV_ENC_HEVC_PROFILE_FREXT_GUID);
        // SAFETY: an HEVC session's union arm is `hevcConfig` — the one this path wrote.
        unsafe { assert_eq!(cfg.encodeCodecConfig.hevcConfig.chromaFormatIDC(), 3) };
        // SAFETY: same HEVC arm as above.
        unsafe { assert_eq!(cfg.encodeCodecConfig.hevcConfig.pixelBitDepthMinus8(), 2) };
    }

    #[test]
    fn av1_never_takes_the_hevc_444_union_write() {
        // SAFETY: as above — pure in-memory config authoring, no driver involvement.
        let cfg = unsafe {
            let mut cfg = nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            apply_low_latency_config(&mut cfg, low_latency_cfg(Codec::Av1, true, 10));
            cfg
        };
        // The HEVC FREXT profile GUID on an AV1 session is an INVALID_PARAM at open.
        assert_ne!(
            cfg.profileGUID,
            nv::NV_ENC_HEVC_PROFILE_FREXT_GUID,
            "4:4:4 on AV1 must not stamp the HEVC FREXT profile"
        );
        // ...and the AV1 arm must still have run, which the old if/else-if skipped entirely.
        // SAFETY: an AV1 session's union arm is `av1Config`.
        unsafe {
            assert_eq!(
                cfg.encodeCodecConfig.av1Config.pixelBitDepthMinus8(),
                2,
                "AV1 10-bit setup was swallowed by the HEVC 4:4:4 branch"
            );
        }
    }

    #[test]
    fn split_forces_two_way_at_4k120() {
        // The regression this threshold constant exists for: 3840×2160×120 = 995,328,000 sat
        // 0.47% under the old `> 1_000_000_000` gate and stayed AUTO — pinned ~107 fps on a
        // 4090 because AUTO never engages at 2160 px height.
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
    }

    #[test]
    fn split_leaves_1440p240_auto() {
        // 884.7 Mpix/s is comfortably single-engine — the threshold move must not drag it in.
        let qhd_240 = 2560u64 * 1440 * 240;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, qhd_240, 2),
            M::NV_ENC_SPLIT_AUTO_MODE as u32
        );
    }

    #[test]
    fn split_rules_for_10bit_after_dropping_the_short_circuit() {
        let five_k_240 = 5120u64 * 1440 * 240; // 1.77 Gpix/s — over the bar
        let four_k_120 = 3840u64 * 2160 * 120; // 995.3 Mpix/s — over the bar
        let hd_60 = 1920u64 * 1080 * 60; // 124 Mpix/s — well under

        // ⚠ BEHAVIOUR FLIP, deliberate: the config the Main10 veto was measured on (7.6 ms
        // forced-2 vs 2.8 ms single-engine on Ada) now clears the pixel-rate bar and SPLITS. The
        // datapoint is one sample at low bits/frame; re-measuring it on Ada is the first on-glass
        // item, and PUNKTFUNK_SPLIT_ENCODE=0 is the escape if it regresses.
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, five_k_240, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        // D1: 10-bit 4K120 used to be vetoed by the depth rule BEFORE reaching the pixel-rate arm
        // written for exactly it. It splits now.
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        // Under the bar, HEVC Main10 keeps the conservative single-engine default — a second
        // engine buys nothing there, so being wrong costs ~nil.
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, hd_60, 2),
            M::NV_ENC_SPLIT_DISABLE_MODE as u32
        );
    }

    /// D2: the Main10 rule was measured on HEVC and used to be codec-blind, so it vetoed **AV1
    /// 10-bit** — which has neither the sub-frame conflict nor any measurement against it.
    #[test]
    fn av1_10bit_is_no_longer_vetoed_by_an_hevc_measurement() {
        let hd_60 = 1920u64 * 1080 * 60;
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::Av1, 10, hd_60, 2),
            M::NV_ENC_SPLIT_AUTO_MODE as u32,
            "AV1 10-bit must follow the ordinary path, not inherit an HEVC veto"
        );
        assert_eq!(
            resolve_split_mode(Codec::Av1, 10, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
    }

    /// THE ENGINE-COUNT FIX: a high-pixel-rate session must use every engine the GPU has, not a
    /// hard-coded two. A 3-NVENC part (GB202 / AD102 workstation) left at 2-way wastes a third of
    /// its encode silicon, and the driver never complains because it accepts an over- OR
    /// under-wide request without comment.
    #[test]
    fn split_uses_every_engine_the_gpu_has() {
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 3),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            "a 3-engine GPU must split three ways"
        );
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 1),
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            "a 1-engine GPU must not pretend to split — today this costs a wasted session open"
        );
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 0),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
            "unprobed engine count keeps the historical assumption; the rejection fallback corrects"
        );
    }

    /// `NV_ENC_SPLIT_ENCODE_MODE` cannot NAME more than three (SDK 0.4.0 / NVENCAPI 12.1), so a
    /// hypothetical wider part falls back to AUTO_FORCED = "split, driver picks how many" — which
    /// is measurably a real split (2.01× vs disabled on `.21`), not a no-op.
    #[test]
    fn split_beyond_three_engines_delegates_to_the_driver() {
        assert_eq!(
            max_forced_split_mode(4),
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
        assert_eq!(
            max_forced_split_mode(8),
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
    }

    /// An operator over-ask must be clamped, because the DRIVER WON'T: measured on `.21` (2 NVENC),
    /// `THREE_FORCED` was honoured and ran identically to `TWO_FORCED` (2303 vs 2308 µs/frame) —
    /// a log claiming a 3-way split over a 2-way encode. Clamping keeps the log honest.
    #[test]
    fn operator_override_is_clamped_to_real_engine_count() {
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(2),
                2
            ),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
            "asking for 3 on a 2-engine card must clamp to 2"
        );
        // Within budget → untouched.
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
                max_forced_split_mode(3),
                3
            ),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        // Unknown engine count must not clamp — we have nothing to clamp against.
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(0),
                0
            ),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        );
        // ⚠ The ordering trap: on a >3-engine part `hw_max` is AUTO_FORCED (1), which is NOT
        // "narrower than" TWO_FORCED (2) despite comparing smaller. A naive `min` would clamp a
        // legitimate 3-way request down to AUTO on the widest hardware we support.
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(4),
                4
            ),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            "a 4-engine GPU must honour an explicit 3-way request, not collapse it to AUTO"
        );
    }

    #[test]
    fn ceiling_cache_round_trips_and_keys_precisely() {
        let key = CeilingKey {
            gpu: 0xB0B0,
            codec: Codec::H265,
            width: 3840,
            height: 2160,
            fps: 120,
            bit_depth: 8,
            chroma_444: false,
            split_mode: M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        };
        assert_eq!(cached_ceiling(&key), None);
        store_ceiling(key, 794_000_000);
        assert_eq!(cached_ceiling(&key), Some(794_000_000));
        // Any config-identity change is a different ceiling — a miss, never a wrong clamp.
        assert_eq!(cached_ceiling(&CeilingKey { fps: 60, ..key }), None);
        assert_eq!(
            cached_ceiling(&CeilingKey {
                split_mode: M::NV_ENC_SPLIT_DISABLE_MODE as u32,
                ..key
            }),
            None
        );
        // A re-search overwrites (the advisory-cache stale-entry path).
        store_ceiling(key, 620_000_000);
        assert_eq!(cached_ceiling(&key), Some(620_000_000));
    }
}

/// Reference-frame DPB depth when RFI is supported (Apollo uses 5). A deeper DPB lets an invalidated
/// reference fall back to an older still-valid frame instead of a full IDR; `numRefL0 = 1` keeps each
/// P-frame single-reference for low latency. Also the window [`plan_range_recovery`] checks against
/// (`next_ts - RFI_DPB` = the oldest frame still in the DPB).
pub(super) const RFI_DPB: u32 = 5;

/// One loss event's recovery decision for the timestamp-range RFI both direct-NVENC backends run
/// (the range half of WP7.2's policy extraction; the slot half — AMF/QSV/Vulkan — is
/// `crate::rfi`). The mechanism (the per-timestamp `nvEncInvalidateRefFrames` loop, the
/// `last_rfi_range`/`pending_anchor` stores, the null-handle/`rfi_supported` gate) stays in each
/// backend.
pub(super) enum RangePlan {
    /// The last successful invalidation already covers this range — no new driver calls, no IDR.
    /// The caller must still RE-ARM its recovery anchor: the client re-asking means the previous
    /// anchor AU may itself have been lost, and the next frame is just as clean a re-anchor.
    Covered,
    /// Invalidate `first..=last` (the CLAMPED range — this is also what the caller must record in
    /// `last_rfi_range` on success, exactly as the inline code stored the post-clamp values).
    Invalidate { first: i64, last: i64 },
    /// Recovery without an IDR is impossible (nonsense range, loss older than the DPB, or a range
    /// entirely in the future) — the caller returns `false` and its (coalesced) keyframe path
    /// recovers. Deliberately NOT paired with any state clearing: neither twin touches
    /// `pending_anchor` on decline (matching Vulkan's decline, opposite of AMF/QSV's
    /// `pending_force` clear — see `crate::rfi`'s module doc before "harmonizing").
    Decline,
}

/// The range-RFI policy, extracted verbatim from the two backends' `invalidate_ref_frames` (they
/// were hand-copied twins). Step order is load-bearing and pinned by tests:
///
/// 1. nonsense range (`first < 0 || first > last`) → [`RangePlan::Decline`];
/// 2. covering-range dedup — checked with the UNCLAMPED `last`, BEFORE the DPB window, so a
///    covered re-ask never touches the driver even when the range has since left the DPB;
/// 3. DPB window: `first < next_ts - RFI_DPB` → Decline (a lost frame older than the DPB cannot
///    be invalidated; the only correct recovery is an IDR);
/// 4. clamp `last` to `next_ts - 1` (never invalidate a timestamp never assigned); an inverted
///    range after the clamp (loss entirely in the future — a prediction desync) → Decline.
///
/// `next_ts` is the backend's `frame_idx`: the NEXT timestamp to assign, which `submit_indexed`
/// pins to the wire frame index — so the client's lost-frame range maps 1:1 onto the timestamps
/// the driver invalidates, across every rebuild/reset. Note `teardown()` clears `last_rfi_range`
/// but NOT `frame_idx`, so a post-reset call legitimately sees a stale-high `next_ts` with a
/// `None` range — the same view the inline code had.
pub(super) fn plan_range_recovery(
    first: i64,
    last: i64,
    next_ts: i64,
    last_rfi_range: Option<(i64, i64)>,
) -> RangePlan {
    if first < 0 || first > last {
        return RangePlan::Decline;
    }
    if let Some((pf, pl)) = last_rfi_range {
        if first >= pf && last <= pl {
            return RangePlan::Covered;
        }
    }
    let oldest_in_dpb = next_ts - RFI_DPB as i64;
    if first < oldest_in_dpb {
        return RangePlan::Decline;
    }
    let last = last.min(next_ts - 1);
    if first > last {
        return RangePlan::Decline;
    }
    RangePlan::Invalidate { first, last }
}

#[cfg(test)]
mod range_policy_tests {
    use super::{plan_range_recovery, RangePlan, RFI_DPB};

    /// Convenience: the plan with no prior invalidation recorded.
    fn plan(first: i64, last: i64, next_ts: i64) -> RangePlan {
        plan_range_recovery(first, last, next_ts, None)
    }

    #[test]
    fn nonsense_ranges_decline() {
        assert!(matches!(plan(-1, 5, 100), RangePlan::Decline));
        assert!(matches!(plan(7, 5, 100), RangePlan::Decline));
    }

    /// `RFI_DPB` is the one host-side knob that sets how many DPB slots a client must
    /// find, so it may never grow past what a mainstream client can allocate.
    ///
    /// The chain, measured on `.21` (RTX 5070 Ti, 2026-08-07) by reading the SPS the
    /// host actually emitted: `maxNumRefFramesInDPB = RFI_DPB` makes NVENC write
    /// `sps_max_dec_pic_buffering_minus1 = 5`, i.e. `RFI_DPB + 1 = 6` pictures — five
    /// references plus the current one. Decoder backends then allocate one slot per
    /// DPB picture plus one for the picture in flight, so the hardware demand is
    /// `RFI_DPB + 2 = 7`. NVIDIA's Vulkan Video reports `maxDpbSlots = 16` (RADV 17),
    /// and a stream over that is refused outright — which on a client with no software
    /// HEVC decoder means losing the codec, not merely a slower path.
    ///
    /// Raising `RFI_DPB` is a legitimate thing to want (a deeper DPB recovers from loss
    /// with a clean P-frame instead of a 20-40x IDR spike), so this does not forbid it —
    /// it forbids raising it past the point where clients stop being able to decode us
    /// at all. There are nine slots of headroom; spend them knowingly.
    #[test]
    fn rfi_dpb_fits_a_mainstream_vulkan_decoder() {
        /// `VkVideoCapabilitiesKHR::maxDpbSlots` on NVIDIA — the lowest cap among the
        /// decoders punktfunk targets.
        const VULKAN_MAX_DPB_SLOTS: u32 = 16;
        // RFI_DPB references + the current picture = what the SPS declares; + 1 again
        // for the picture being decoded = what the backend's slot pool must hold.
        let slots_needed = RFI_DPB + 2;
        assert!(
            slots_needed <= VULKAN_MAX_DPB_SLOTS,
            "RFI_DPB = {RFI_DPB} makes the host emit a stream needing {slots_needed} DPB \
             slots, and mainstream Vulkan Video decode caps at {VULKAN_MAX_DPB_SLOTS} — \
             every access unit would be refused and the client would drop the codec"
        );
    }

    #[test]
    fn covering_range_dedups_partial_overlap_does_not() {
        let prior = Some((90i64, 95i64));
        // Exact cover and sub-range → Covered. This pins EXISTING behavior, including that a
        // covered range survives a forced IDR with zero driver calls (nothing clears
        // `last_rfi_range` on a keyframe) — a recorded fact, not an endorsement.
        assert!(matches!(
            plan_range_recovery(90, 95, 100, prior),
            RangePlan::Covered
        ));
        assert!(matches!(
            plan_range_recovery(92, 94, 100, prior),
            RangePlan::Covered
        ));
        // Partial overlap re-invalidates the FULL new range (next_ts = 98 keeps the window open:
        // oldest_in_dpb = 93; at next_ts = 100 the same range would age out and Decline instead).
        assert!(matches!(
            plan_range_recovery(93, 97, 98, prior),
            RangePlan::Invalidate {
                first: 93,
                last: 97
            }
        ));
    }

    /// The covering check runs BEFORE the DPB window: a covered re-ask stays Covered (no driver
    /// calls needed) even when the range has since aged out of the DPB.
    #[test]
    fn covered_is_checked_before_the_dpb_window() {
        let prior = Some((10i64, 12i64));
        assert!(matches!(
            plan_range_recovery(10, 12, 100, prior),
            RangePlan::Covered
        ));
        // ...whereas the same range with no prior invalidation is outside the window → Decline.
        assert!(matches!(plan(10, 12, 100), RangePlan::Decline));
    }

    #[test]
    fn dpb_window_boundary() {
        let next_ts = 100i64;
        let oldest = next_ts - RFI_DPB as i64; // 95: the oldest timestamp still in the DPB
        assert!(matches!(
            plan(oldest, oldest, next_ts),
            RangePlan::Invalidate { .. }
        ));
        assert!(matches!(
            plan(oldest - 1, oldest, next_ts),
            RangePlan::Decline
        ));
    }

    /// `last` clamps to `next_ts - 1` (the newest encoded frame); the Invalidate carries the
    /// CLAMPED value — which is also what the caller records in `last_rfi_range`.
    #[test]
    fn clamps_to_newest_encoded() {
        assert!(matches!(
            plan(98, 150, 100),
            RangePlan::Invalidate {
                first: 98,
                last: 99
            }
        ));
        // A range entirely in the future inverts under the clamp → Decline (prediction desync).
        assert!(matches!(plan(100, 150, 100), RangePlan::Decline));
        // Fresh session (`frame_idx == 0`): window passes (oldest = -5) but the clamp gives
        // last = -1 < first → Decline. The inline code behaved identically.
        assert!(matches!(plan(0, 3, 0), RangePlan::Decline));
    }
}

/// The per-session knobs both direct-NVENC backends feed [`apply_low_latency_config`]. `Copy` so the
/// backend fills it from `self` at the call. The two input-format fields bridge the only real
/// divergence between the CUDA and D3D11 paths (which surface formats can carry full chroma / 10-bit
/// input); everything else in the config is identical across platforms.
#[derive(Clone, Copy)]
pub(super) struct LowLatencyConfig {
    pub codec: Codec,
    /// Target bitrate (bps); CBR average == max.
    pub bitrate: u64,
    pub fps: u32,
    /// This GPU advertises custom VBV — else leave the preset default (per the caps probe).
    pub custom_vbv: bool,
    /// A 4:4:4 session was negotiated (HEVC Range Extensions).
    pub chroma_444: bool,
    /// The input surface can carry full chroma — Linux feeds a YUV444 surface, Windows a packed-RGB
    /// surface NVENC CSCs internally. 4:4:4 engages only when this AND [`chroma_444`](Self::chroma_444).
    pub full_chroma_input: bool,
    /// Output bit depth (8 or 10).
    pub bit_depth: u8,
    /// AV1 `inputPixelBitDepthMinus8` — the encoder's view of the INPUT depth (Linux is 8-bit in
    /// today, so 0; Windows derives it from the surface format). `u32` to match the SDK setter.
    pub av1_input_depth_minus8: u32,
    pub hdr: bool,
    /// This GPU supports reference-frame invalidation (a deeper DPB for graceful loss recovery).
    pub rfi_supported: bool,
    /// Resolved per-frame slice count ([`resolve_slices`] — env override, else the backend
    /// default). ≤ 1 leaves the preset's single slice untouched.
    pub slices: u32,
}

/// Author the shared `NV_ENC_INITIALIZE_PARAMS` (P1/ULL preset, PTD, the session dimensions/rate)
/// pointing at `cfg`. `enable_async` drives the Windows two-thread async retrieve — Linux is
/// sync-only and passes `false`, leaving `enableEncodeAsync` at 0 as before. The returned struct
/// borrows `cfg` as a raw pointer; the caller must keep `cfg` alive across the NVENC call it feeds
/// this into. Used at open and at in-place reconfigure, which must present the SAME init params.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_init_params(
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    cfg: &mut nv::NV_ENC_CONFIG,
    split_mode: u32,
    enable_async: bool,
    subframe: bool,
) -> nv::NV_ENC_INITIALIZE_PARAMS {
    let mut init = nv::NV_ENC_INITIALIZE_PARAMS {
        version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
        encodeGUID: codec_guid,
        presetGUID: nv::NV_ENC_PRESET_P1_GUID,
        tuningInfo: nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        encodeWidth: width,
        encodeHeight: height,
        darWidth: width,
        darHeight: height,
        frameRateNum: fps,
        frameRateDen: 1,
        enablePTD: 1,
        enableEncodeAsync: enable_async as u32,
        encodeConfig: cfg,
        ..Default::default()
    };
    // splitEncodeMode is a C bitfield — set via the generated accessor, not a struct field.
    init.set_splitEncodeMode(split_mode);
    // Sub-frame readback (latency plan §7 LN1; default-on for Linux direct-NVENC since Phase 3 —
    // the caller resolves `subframe` via [`resolve_subframe`] + its caps probe): the driver
    // writes each slice into the output buffer as it completes and reports per-slice offsets, so
    // a sync-mode consumer can read slices out while the frame is still encoding. Pair with
    // multi-slice (a single-slice frame yields nothing to read early). `reportSliceOffsets`
    // requires `enableEncodeAsync = 0`, so async (Windows) sessions never arm.
    if !enable_async && subframe {
        init.set_enableSubFrameWrite(1);
        init.set_reportSliceOffsets(1);
    }
    init
}

/// Author the shared low-latency NVENC config onto a **preset-seeded** `cfg`: CBR + infinite GOP +
/// P-only + ~1-frame VBV, per-codec tier/level, chroma + bit depth, unconditional colour signaling,
/// and the RFI DPB. The caller seeds `cfg` from the P1/ULL preset first (that call needs the
/// per-platform entry table) and passes the input-format specifics via [`LowLatencyConfig`]; the
/// per-platform surface registration, device binding, and async retrieve stay in the backends.
///
/// # Safety
/// Writes NVENC codec-config union fields on `cfg`, which must be a valid, preset-seeded
/// `NV_ENC_CONFIG` whose active union arm matches [`LowLatencyConfig::codec`].
pub(super) unsafe fn apply_low_latency_config(cfg: &mut nv::NV_ENC_CONFIG, c: LowLatencyConfig) {
    // CBR, infinite GOP, P-only, ~1-frame VBV — mirrors the AMF/VAAPI/QSV libav RC config.
    cfg.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1;
    cfg.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    // Explicit zero reorder delay: with P-only + no lookahead there is no reordering to buffer,
    // but pin the bit so no preset/driver default can ever slip a frame of reorder delay in.
    cfg.rcParams.set_zeroReorderDelay(1);
    let bps = c.bitrate.min(u32::MAX as u64) as u32;
    cfg.rcParams.averageBitRate = bps;
    cfg.rcParams.maxBitRate = bps;
    // Shrink the VBV with the bitrate (NVENC validates it against the same level ceiling), but only
    // when the GPU advertises custom-VBV support — else keep the preset default.
    if c.custom_vbv {
        // ~1-frame VBV by default; PUNKTFUNK_VBV_FRAMES scales it (parity with AMF/VAAPI/QSV).
        let vbv = ((c.bitrate as f64 / c.fps.max(1) as f64) * crate::vbv_frames_env())
            .clamp(1.0, u32::MAX as f64) as u32;
        cfg.rcParams.vbvBufferSize = vbv;
        cfg.rcParams.vbvInitialDelay = vbv;
    }

    // Tier + autoselect level, PER CODEC (the union writes must match the negotiated codec). HEVC
    // keeps HIGH tier for its higher per-level bitrate ceiling (Main at 5K ≈ 240 Mbps, HIGH ≈ 800
    // Mbps); AV1 supports the Main tier ONLY — tier=1 fails the session open with INVALID_PARAM, and
    // its level enum's 0 is LEVEL 2.0 (NOT autoselect), so AV1 takes NO writes (the preset defaults
    // are the only accepted config). H.264 has no tier. Level 0 = autoselect for HEVC.
    match c.codec {
        Codec::H265 => {
            // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active arm.
            unsafe { cfg.encodeCodecConfig.hevcConfig.tier = 1 };
            // SAFETY: same HEVC arm, same match guard.
            unsafe { cfg.encodeCodecConfig.hevcConfig.level = 0 };
        }
        Codec::Av1 => {}
        Codec::H264 => {}
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }

    // Multi-slice frames (latency plan §7 LN1): `c.slices` splits every frame into N slices
    // (sliceMode 3 = "N slices per frame"), the unit sub-frame readback ships early and loss
    // concealment can discard independently. Costs ~1-2 % bitrate in slice headers. H.264/HEVC
    // only — AV1 partitions via tiles, not slices (the resolver already returns 1 there).
    // Default 4 on Linux direct-NVENC (Phase 3), env-only elsewhere; ≤ 1 keeps the preset's
    // single slice.
    if let Some(n) = Some(c.slices).filter(|n| *n >= 2) {
        match c.codec {
            Codec::H264 => {
                // SAFETY: H.264 session (matched on `c.codec`), so `h264Config` is the active arm.
                unsafe { cfg.encodeCodecConfig.h264Config.sliceMode = 3 };
                // SAFETY: same H.264 arm, same match guard.
                unsafe { cfg.encodeCodecConfig.h264Config.sliceModeData = n };
            }
            Codec::H265 => {
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active arm.
                unsafe { cfg.encodeCodecConfig.hevcConfig.sliceMode = 3 };
                // SAFETY: same HEVC arm, same match guard.
                unsafe { cfg.encodeCodecConfig.hevcConfig.sliceModeData = n };
            }
            Codec::Av1 | Codec::PyroWave => {}
        }
    }

    // Chroma + bit depth. Full-chroma 4:4:4 (HEVC Range Extensions, chromaFormatIDC=3 under the FREXT
    // profile) takes precedence and composes with 10-bit (Main 4:4:4 10); it needs a full-chroma-
    // capable input. Otherwise 10-bit selects Main10 (HEVC) or the AV1 output depth — stamping the
    // HEVC Main10 GUID onto an AV1 session is an INVALID_PARAM, so bit depth is set PER CODEC.
    // `encodeCodecConfig` is a C UNION, so the `hevcConfig` writes below are only meaningful on an
    // HEVC session — on an H.264 or AV1 one they reinterpret that codec's own config bytes. The
    // codec test is therefore load-bearing, not defensive: without it this branch was gated purely
    // on `chroma_444 && full_chroma_input` and stayed non-UB only because `lib.rs` degrades 4:4:4
    // for non-HEVC codecs. That was a two-file invariant with nothing asserting it, on the path
    // BOTH direct-NVENC backends take.
    //
    // Being a codec test also fixes a second, quieter bug in the same shape: this is an
    // `if`/`else if`, so a non-HEVC session that somehow arrived with `chroma_444` set took this
    // branch and skipped the per-codec bit-depth arm entirely — ending up with neither HEVC 4:4:4
    // (wrong for it) nor its own 10-bit configuration (simply missing). Non-HEVC now falls through
    // to the arm that knows what to do with it.
    let want_444 = c.chroma_444 && c.full_chroma_input;
    if want_444 && c.codec != Codec::H265 {
        tracing::warn!(
            codec = ?c.codec,
            "4:4:4 requested on a non-HEVC NVENC session — ignoring it (Range Extensions are \
             HEVC-only); the negotiator should have degraded this to 4:2:0 before the open"
        );
    }
    if want_444 && c.codec == Codec::H265 {
        cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_FREXT_GUID;
        // SAFETY: HEVC session (guarded by `c.codec == Codec::H265` on this branch), so
        // `hevcConfig` is the active arm.
        unsafe { cfg.encodeCodecConfig.hevcConfig.set_chromaFormatIDC(3) };
        if c.bit_depth == 10 {
            // SAFETY: same HEVC arm, same branch guard. (Main 4:4:4 10)
            unsafe { cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2) };
        }
    } else if c.bit_depth == 10 {
        match c.codec {
            Codec::H265 => {
                cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID;
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active arm.
                unsafe { cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2) };
            }
            Codec::Av1 => {
                // SAFETY: AV1 session (matched on `c.codec`), so `av1Config` is the active arm.
                unsafe { cfg.encodeCodecConfig.av1Config.set_pixelBitDepthMinus8(2) };
                // SAFETY: same AV1 arm, same match guard.
                unsafe {
                    cfg.encodeCodecConfig
                        .av1Config
                        .set_inputPixelBitDepthMinus8(c.av1_input_depth_minus8)
                };
            }
            Codec::H264 => {} // no 10-bit H.264 encode on NVENC — negotiation never asks
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }

    // Colour signaling, written UNCONDITIONALLY: the input is already CSC'd to a specific matrix
    // (BT.709 limited SDR, or BT.2020 PQ for HDR), so the stream must say so — a decoder whose
    // "unspecified" default is 601 (Moonlight/third-party/Android-vendor at sub-HD) otherwise
    // mis-renders. HEVC/H.264 carry it in the VUI; AV1 has no VUI, so the same CICP code points go in
    // the sequence-header colour config.
    {
        let (prim, trc, mat) = if c.hdr {
            (
                nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020,
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084,
                nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL,
            )
        } else {
            (
                nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709,
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709,
                nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709,
            )
        };
        match c.codec {
            Codec::H265 => {
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active
                // arm; the borrow is dropped before any other union access.
                let vui = unsafe { &mut cfg.encodeCodecConfig.hevcConfig.hevcVUIParameters };
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = 0;
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::H264 => {
                // SAFETY: H.264 session (matched on `c.codec`), so `h264Config` is the active
                // arm; the borrow is dropped before any other union access.
                let vui = unsafe { &mut cfg.encodeCodecConfig.h264Config.h264VUIParameters };
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = 0;
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::Av1 => {
                // SAFETY: AV1 session (matched on `c.codec`), so `av1Config` is the active arm;
                // the borrow is dropped before any other union access.
                let av1 = unsafe { &mut cfg.encodeCodecConfig.av1Config };
                av1.colorPrimaries = prim;
                av1.transferCharacteristics = trc;
                av1.matrixCoefficients = mat;
                av1.colorRange = 0; // studio/limited swing
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }

    // Reference-frame invalidation: a deeper DPB so an invalidated reference can fall back to an
    // older still-valid frame instead of a full IDR; `numRefL0 = 1` keeps each P-frame
    // single-reference for low latency. Only when this GPU supports RFI.
    if c.rfi_supported {
        let one = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
        match c.codec {
            Codec::H264 => {
                // SAFETY: H.264 session (matched on `c.codec`), so `h264Config` is the active arm.
                unsafe { cfg.encodeCodecConfig.h264Config.maxNumRefFrames = RFI_DPB };
                // SAFETY: same H.264 arm, same match guard.
                unsafe { cfg.encodeCodecConfig.h264Config.numRefL0 = one };
            }
            Codec::H265 => {
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active arm.
                unsafe { cfg.encodeCodecConfig.hevcConfig.maxNumRefFramesInDPB = RFI_DPB };
                // SAFETY: same HEVC arm, same match guard.
                unsafe { cfg.encodeCodecConfig.hevcConfig.numRefL0 = one };
            }
            Codec::Av1 => {
                // SAFETY: AV1 session (matched on `c.codec`), so `av1Config` is the active arm.
                unsafe { cfg.encodeCodecConfig.av1Config.maxNumRefFramesInDPB = RFI_DPB };
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod arbiter_tests {
    use super::{ArbAction, SplitArbiter, SETTLE_FRAMES};

    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    /// THE SUB-FRAME TRADE, which is the whole reason `set_send_spread_us` exists. Same encode
    /// numbers both times; only the handicap differs.
    ///
    /// A 4K HEVC session where split halves the encode (5000 → 2400 µs) but costs sub-frame
    /// readback. With a cheap send there is headroom and split wins. With an expensive send the
    /// lost overlap outweighs the encode saving, and the arbiter must REFUSE the arm that looks
    /// twice as fast — which is exactly the mistake an encode-only comparison makes.
    #[test]
    fn handicap_can_reverse_the_verdict() {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let run = |handicap: u64| {
            let mut arb = SplitArbiter::with_handicap(inc, chal, handicap);
            let mut live = inc;
            for _ in 0..500 {
                if arb.is_done() {
                    break;
                }
                let us = if live == inc { 5000 } else { 2400 };
                if let Some(a) = arb.on_frame(us) {
                    match a {
                        ArbAction::SwitchTo(m) | ArbAction::Settled(m) => live = m,
                    }
                }
            }
            live
        };
        // Cheap send: the 2600 µs encode saving is real, split wins.
        assert_eq!(run(500), chal, "with a cheap send, split should win");
        // Expensive send: 2400 + 3000 = 5400 against 5000 — the "twice as fast" arm is a LOSS
        // end to end, and an encode-only comparison would have taken it.
        assert_eq!(
            run(3000),
            inc,
            "when losing sub-frame costs more than split saves, the incumbent must hold — this is \
             the regression an encode-only arbiter would ship"
        );
    }

    /// Drive an arbiter with a fixed cost per arm and return every action it emitted.
    fn drive(incumbent_us: u64, challenger_us: u64) -> (Vec<ArbAction>, u32) {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let mut arb = SplitArbiter::with_handicap(inc, chal, 0);
        let mut actions = Vec::new();
        // Whatever the session is currently running; the harness follows the arbiter's switches
        // so the cost it reports matches the arm actually in effect.
        let mut live = inc;
        for _ in 0..500 {
            if arb.is_done() {
                break;
            }
            let us = if live == inc {
                incumbent_us
            } else {
                challenger_us
            };
            if let Some(a) = arb.on_frame(us) {
                actions.push(a);
                match a {
                    ArbAction::SwitchTo(m) => live = m,
                    ArbAction::Settled(m) => live = m,
                }
            }
        }
        (actions, live)
    }

    /// A clearly faster challenger is adopted, and the session ends up running it.
    #[test]
    fn arbiter_adopts_a_clearly_faster_challenger() {
        let (actions, live) = drive(5000, 2400);
        assert_eq!(
            actions[0],
            ArbAction::SwitchTo(M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32),
            "must try the challenger before judging it"
        );
        assert_eq!(
            actions.last(),
            Some(&ArbAction::Settled(M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32))
        );
        assert_eq!(live, M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32);
    }

    /// A slower challenger is rejected and the session is put BACK — the arbiter is mid-experiment
    /// when it decides, so "keep the incumbent" is a real reconfigure, not a no-op. Getting this
    /// wrong would strand every losing arbitration on the losing arm.
    #[test]
    fn arbiter_restores_the_incumbent_when_the_challenger_loses() {
        let (actions, live) = drive(2400, 5000);
        assert_eq!(
            actions.last(),
            Some(&ArbAction::SwitchTo(M::NV_ENC_SPLIT_DISABLE_MODE as u32)),
            "a losing experiment must be undone"
        );
        assert_eq!(live, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }

    /// Within the margin the incumbent holds: switching costs a reconfigure and, on HEVC, sub-frame
    /// readback, so a coin-flip difference must not move the session.
    #[test]
    fn arbiter_keeps_the_incumbent_inside_the_margin() {
        // 5 % better — under WIN_MARGIN_PCT.
        let (_, live) = drive(2400, 2280);
        assert_eq!(live, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }

    /// THE SETTLE CONTRACT: the challenger must not be judged on frames taken immediately after the
    /// switch. Feed it a transient — slow for the whole settle window, fast afterwards — and it
    /// must still see the fast steady state. Without the settle window this arbiter would read the
    /// transient, reject a genuinely better arm, and cache that verdict.
    #[test]
    fn arbiter_ignores_the_post_switch_transient() {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let mut arb = SplitArbiter::with_handicap(inc, chal, 0);
        let mut switched_at = None;
        let mut frame = 0usize;
        let mut outcome = None;
        while outcome.is_none() && frame < 500 {
            let us = match switched_at {
                None => 5000,
                // The transient: as slow as the incumbent for exactly the settle window.
                Some(s) if frame - s <= SETTLE_FRAMES as usize => 5000,
                Some(_) => 2000,
            };
            match arb.on_frame(us) {
                Some(ArbAction::SwitchTo(m)) if m == chal => switched_at = Some(frame),
                Some(a) => outcome = Some(a),
                None => {}
            }
            frame += 1;
        }
        assert_eq!(
            outcome,
            Some(ArbAction::Settled(chal)),
            "the settle window must hide the post-switch transient — otherwise a better arm is \
             rejected on its own warmup"
        );
    }
}

/// The hand-written split constants in `codec.rs` MUST equal the SDK enum they mirror. They are
/// duplicated there so the libav path — which builds without the `nvenc` feature, where the enum
/// does not exist — can share one policy instead of keeping the copy that had already drifted.
/// This is the only place both are visible at once.
#[cfg(test)]
mod split_constant_parity {
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    #[test]
    fn nvenc_split_constants_match_the_sdk() {
        assert_eq!(crate::SPLIT_AUTO, M::NV_ENC_SPLIT_AUTO_MODE as u32);
        assert_eq!(
            crate::SPLIT_AUTO_FORCED,
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
        assert_eq!(
            crate::SPLIT_TWO_FORCED,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        assert_eq!(
            crate::SPLIT_THREE_FORCED,
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        );
        assert_eq!(crate::SPLIT_DISABLE, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }
}
