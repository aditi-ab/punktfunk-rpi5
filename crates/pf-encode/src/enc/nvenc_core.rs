//! Shared direct-SDK NVENC core — the platform-agnostic pieces of the two `nvEncodeAPI` backends,
//! Windows D3D11 (`encode/windows/nvenc.rs`) and Linux CUDA (`encode/linux/nvenc_cuda.rs`), so the
//! byte-identical glue lives once (plan §2.2, the direct-NVENC Tier-2). The per-platform parts —
//! the entry-table load (`nvEncodeAPI64.dll` via `LoadLibrary` vs `libnvidia-encode.so` via
//! `libloading`), the device binding (D3D11 vs CUDA), input-surface registration, and the
//! Windows-only async retrieve — stay in their backends. Sibling of [`super::nvenc_status`].

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
/// `default_slices` — 4 on Linux direct-NVENC since the Phase-3 default-on, 1 everywhere else
/// (the Windows async path is deliberately untouched). H.264/HEVC only (AV1 partitions via
/// tiles). ONE parse shared by the config author ([`apply_low_latency_config`] via
/// [`LowLatencyConfig::slices`]) and the Linux backend's chunked-poll arming, so the two can
/// never disagree about whether a session is multi-slice.
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
/// explicitly testing), unset = the backend's `default_on` (Linux direct-NVENC passes its
/// SUBFRAME_READBACK caps-probe result since Phase 3; Windows passes `false`).
pub(super) fn resolve_subframe(default_on: bool) -> bool {
    match std::env::var("PUNKTFUNK_NVENC_SUBFRAME").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => default_on,
    }
}

/// Resolved NVENC split-frame encode mode for a session — ONE selector shared by the Windows and
/// Linux direct-SDK backends (they had drifted into byte-identical duplicates, one of which
/// logged and one didn't). Precedence:
/// 1. `PUNKTFUNK_SPLIT_ENCODE` = `0`/`disable` | `1`/`auto` (AUTO_FORCED) | `2` | `3` — operator
///    override, always wins.
/// 2. 10-bit → DISABLE: 2-way split is measurably SLOWER on Ada for Main10 — at 5120×1440@240
///    forced-2 took 7.6 ms/frame (~131 fps) vs 2.8 ms (~357 fps) single-engine (the split/merge
///    overhead dominates), and a single engine handles 5K@240 Main10 well under budget. This was
///    the "broken animations in HDR" cap at ~131 fps.
/// 3. Pixel rate ≥ [`super::SPLIT_FORCE_PIXEL_RATE`] → force 2-way (AUTO never engages below
///    ~2112 px height, so 4K120 must be forced onto the second engine).
/// 4. Else AUTO (the ~2% BD-rate split cost isn't worth it at low pixel rates).
///
/// The caller still owns the rejection fallback (retry split-disabled) — a codec/config that
/// rejects the chosen mode downgrades at open, not here.
pub(super) fn resolve_split_mode(bit_depth: u8, pixel_rate: u64) -> u32 {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    let mode = match std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok().as_deref() {
        Some("0") | Some("disable") => M::NV_ENC_SPLIT_DISABLE_MODE as u32,
        Some("1") | Some("auto") => M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32,
        Some("3") => M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
        Some("2") => M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        _ if bit_depth >= 10 => M::NV_ENC_SPLIT_DISABLE_MODE as u32,
        _ if pixel_rate >= super::SPLIT_FORCE_PIXEL_RATE => M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        _ => M::NV_ENC_SPLIT_AUTO_MODE as u32,
    };
    tracing::debug!(
        split_mode = mode,
        bit_depth,
        pixel_rate,
        "NVENC split-encode mode selected"
    );
    mode
}

/// Whether the operator EXPLICITLY forced sub-frame readback on (`PUNKTFUNK_NVENC_SUBFRAME=1`)
/// — the log-severity input to [`resolve_split_subframe`]: a forced knob being overridden
/// deserves a `warn`, a default being tuned an `info`. Callers LATCH this once next to their
/// resolved subframe state (an env re-read at reconfigure would violate the "open and
/// reconfigure present identical init params" invariant).
/// Linux-cfg'd like its ONLY caller (the `nvenc_cuda` query_caps latch) — Windows sessions have
/// `subframe == forced` by construction (env opt-in only) and never consult this; without the
/// cfg it is dead code on every Windows leg (item-level dead_code, the recurring trap).
#[cfg(target_os = "linux")]
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
/// - **AV1**: both legal (sub-frame is per-tile; split is constrained only by
///   output-into-vidmem, which we never use) — untouched.
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
    (split_mode, subframe)
}

#[cfg(test)]
mod split_subframe_tests {
    use super::{resolve_split_subframe, Codec};
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

    /// AV1: both features are legal together (per-tile sub-frame; split constrained only by
    /// output-into-vidmem) — the arbitration must not touch it.
    #[test]
    fn av1_untouched() {
        assert_eq!(
            resolve_split_subframe(Codec::Av1, TWO, true, true),
            (TWO, true)
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
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
        unsafe {
            assert_eq!(cfg.encodeCodecConfig.hevcConfig.chromaFormatIDC(), 3);
            assert_eq!(cfg.encodeCodecConfig.hevcConfig.pixelBitDepthMinus8(), 2);
        }
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
            resolve_split_mode(8, four_k_120),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
    }

    #[test]
    fn split_leaves_1440p240_auto() {
        // 884.7 Mpix/s is comfortably single-engine — the threshold move must not drag it in.
        let qhd_240 = 2560u64 * 1440 * 240;
        assert_eq!(
            resolve_split_mode(8, qhd_240),
            M::NV_ENC_SPLIT_AUTO_MODE as u32
        );
    }

    #[test]
    fn split_disabled_for_10bit_even_at_high_pixel_rate() {
        // The measured Main10 rule: split/merge overhead dominates 10-bit on Ada (7.6 ms forced-2
        // vs 2.8 ms single-engine at 5K240) — 10-bit precedes the pixel-rate arm.
        let five_k_240 = 5120u64 * 1440 * 240;
        assert_eq!(
            resolve_split_mode(10, five_k_240),
            M::NV_ENC_SPLIT_DISABLE_MODE as u32
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
            cfg.encodeCodecConfig.hevcConfig.tier = 1;
            cfg.encodeCodecConfig.hevcConfig.level = 0;
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
                cfg.encodeCodecConfig.h264Config.sliceMode = 3;
                cfg.encodeCodecConfig.h264Config.sliceModeData = n;
            }
            Codec::H265 => {
                cfg.encodeCodecConfig.hevcConfig.sliceMode = 3;
                cfg.encodeCodecConfig.hevcConfig.sliceModeData = n;
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
        cfg.encodeCodecConfig.hevcConfig.set_chromaFormatIDC(3);
        if c.bit_depth == 10 {
            cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2); // Main 4:4:4 10
        }
    } else if c.bit_depth == 10 {
        match c.codec {
            Codec::H265 => {
                cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID;
                cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2);
            }
            Codec::Av1 => {
                cfg.encodeCodecConfig.av1Config.set_pixelBitDepthMinus8(2);
                cfg.encodeCodecConfig
                    .av1Config
                    .set_inputPixelBitDepthMinus8(c.av1_input_depth_minus8);
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
                let vui = &mut cfg.encodeCodecConfig.hevcConfig.hevcVUIParameters;
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = 0;
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::H264 => {
                let vui = &mut cfg.encodeCodecConfig.h264Config.h264VUIParameters;
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = 0;
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::Av1 => {
                let av1 = &mut cfg.encodeCodecConfig.av1Config;
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
                cfg.encodeCodecConfig.h264Config.maxNumRefFrames = RFI_DPB;
                cfg.encodeCodecConfig.h264Config.numRefL0 = one;
            }
            Codec::H265 => {
                cfg.encodeCodecConfig.hevcConfig.maxNumRefFramesInDPB = RFI_DPB;
                cfg.encodeCodecConfig.hevcConfig.numRefL0 = one;
            }
            Codec::Av1 => {
                cfg.encodeCodecConfig.av1Config.maxNumRefFramesInDPB = RFI_DPB;
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }
}
