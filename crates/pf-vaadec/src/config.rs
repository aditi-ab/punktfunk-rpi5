//! Decoder-creation decisions: which `VAProfile` a stream needs, which render-target
//! format its surfaces must carry, and how many of them to allocate.
//!
//! The same job `pf-dxvadec`'s `config` module does for DXVA, and split out for the same
//! reason: these are pure functions of the stream's shape, so they belong where the
//! ordinary gates run them rather than inside `cfg(target_os = "linux")` FFI that
//! only a box can compile.
//!
//! Constant values are the libva 2.23.0 enumerators.

/// `VAEntrypointVLD` — full bitstream decode, the only entry point this rung uses.
pub const VA_ENTRYPOINT_VLD: u32 = 1;

/// `VAProfile` enumerators (`va.h`).
pub const VA_PROFILE_H264_MAIN: i32 = 6;
pub const VA_PROFILE_H264_HIGH: i32 = 7;
pub const VA_PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
pub const VA_PROFILE_HEVC_MAIN: i32 = 17;
pub const VA_PROFILE_HEVC_MAIN10: i32 = 18;

/// `VA_RT_FORMAT_*` — the surface render-target format.
pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RT_FORMAT_YUV444: u32 = 0x0000_0004;
pub const VA_RT_FORMAT_YUV420_10: u32 = 0x0000_0100;

/// Which codec a session decodes. Mirrors `pf-dxvadec`'s `Codec` rather than
/// re-exporting it: this crate must not depend on the Windows-facing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// A profile choice, with the name the logs print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaProfile {
    pub value: i32,
    pub name: &'static str,
}

/// Why a stream cannot be decoded by this rung at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// A (chroma, depth) pair with no profile — 4:4:4, or a depth outside 8/10.
    UnsupportedShape { chroma_format_idc: u8, depth: u8 },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnsupportedShape {
                chroma_format_idc,
                depth,
            } => write!(
                f,
                "no VAAPI decode profile for chroma_format_idc {chroma_format_idc} at {depth} bits"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The profile a stream of this shape decodes under.
///
/// H.264 resolves to **High** for every 8-bit 4:2:0 stream rather than reading the
/// SPS's `profile_idc`. That is deliberate and is what VAAPI clients do: High is a
/// superset of Main and Constrained Baseline for the tools our hosts emit, every
/// driver advertising H.264 decode advertises High, and picking Main for a stream
/// that turns out to use 8x8 transforms is a mid-stream failure where picking High
/// is not. The narrower enumerators are exported for the capability probe, which
/// reports what the DEVICE offers.
///
/// 4:4:4 is refused rather than mapped: `VAProfileH264High444` exists in the header
/// but no driver in this fleet advertises it, and the Vulkan rung is where this
/// program's 4:4:4 support actually lives.
pub fn profile_for(
    codec: Codec,
    chroma_format_idc: u8,
    depth: u8,
) -> Result<VaProfile, ConfigError> {
    match (codec, chroma_format_idc, depth) {
        (Codec::H264, 1, 8) => Ok(VaProfile {
            value: VA_PROFILE_H264_HIGH,
            name: "H.264 High",
        }),
        (Codec::H265, 1, 8) => Ok(VaProfile {
            value: VA_PROFILE_HEVC_MAIN,
            name: "HEVC Main",
        }),
        (Codec::H265, 1, 10) => Ok(VaProfile {
            value: VA_PROFILE_HEVC_MAIN10,
            name: "HEVC Main 10",
        }),
        _ => Err(ConfigError::UnsupportedShape {
            chroma_format_idc,
            depth,
        }),
    }
}

/// The surface render-target format for a stream shape.
pub fn rt_format(chroma_format_idc: u8, depth: u8) -> Result<u32, ConfigError> {
    match (chroma_format_idc, depth) {
        (1, 8) => Ok(VA_RT_FORMAT_YUV420),
        (1, 10) => Ok(VA_RT_FORMAT_YUV420_10),
        (3, 8) => Ok(VA_RT_FORMAT_YUV444),
        _ => Err(ConfigError::UnsupportedShape {
            chroma_format_idc,
            depth,
        }),
    }
}

/// Headroom over the DPB for pictures the CONSUMER still holds.
///
/// A surface handed to the presenter is not free to decode into — that is what
/// zero-copy costs — and a pool sized exactly to the DPB stalls the decoder behind
/// the display.
///
/// **8, matching `pf_vkdecode::images::HOLD_HEADROOM`, and for its measurement**:
/// the real client pipeline holds roughly four to seven frames at steady state (two
/// bounded(2) channels, the frame store's 1..=3 preroll, the in-flight present and
/// the retired-frame slot), so eight leaves a frame of slack and a consumer holding
/// more than that has earned an honest "pool exhausted" rather than a silent stall.
/// The number was 4 when this module was written against no consumer; the native
/// Vulkan rung had already measured the pipeline by then, and 4 would have run the
/// pool dry on an ordinary stream.
///
/// (The FFmpeg VAAPI rung asks libavcodec for `extra_hw_frames = 4` and survives on
/// it, but its pool is not this pool: `av_hwframe_get_buffer` BLOCKS until a surface
/// frees, so its headroom buys latency where ours buys correctness.)
pub const PRESENTER_HEADROOM: usize = 8;

/// How many decode surfaces a session allocates: the DPB, plus the picture being
/// decoded, plus [`PRESENTER_HEADROOM`].
///
/// VAAPI reports no driver minimum to honour (DXVA's
/// `ConfigMinRenderTargetBuffCount` has no counterpart), so this is the whole rule.
pub fn surface_count(max_dpb_frames: usize) -> usize {
    max_dpb_frames + 1 + PRESENTER_HEADROOM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_8bit_420_is_high() {
        let p = profile_for(Codec::H264, 1, 8).expect("the envelope's only H.264 shape");
        assert_eq!(p.value, VA_PROFILE_H264_HIGH);
    }

    #[test]
    fn hevc_picks_main_or_main10_by_depth() {
        assert_eq!(
            profile_for(Codec::H265, 1, 8).unwrap().value,
            VA_PROFILE_HEVC_MAIN
        );
        assert_eq!(
            profile_for(Codec::H265, 1, 10).unwrap().value,
            VA_PROFILE_HEVC_MAIN10
        );
    }

    #[test]
    fn shapes_outside_the_envelope_are_refused_not_guessed() {
        // 10-bit H.264 (High10) and 4:4:4 both have header enumerators; neither is
        // in this rung's envelope, and silently narrowing to an 8-bit profile is the
        // class of bug that decodes to garbage instead of failing.
        assert!(profile_for(Codec::H264, 1, 10).is_err());
        assert!(profile_for(Codec::H264, 3, 8).is_err());
        assert!(profile_for(Codec::H265, 3, 10).is_err());
        assert!(rt_format(1, 12).is_err());
    }

    #[test]
    fn rt_format_tracks_depth() {
        assert_eq!(rt_format(1, 8).unwrap(), VA_RT_FORMAT_YUV420);
        assert_eq!(rt_format(1, 10).unwrap(), VA_RT_FORMAT_YUV420_10);
    }

    #[test]
    fn the_surface_pool_covers_dpb_plus_current_plus_headroom() {
        assert_eq!(surface_count(4), 4 + 1 + PRESENTER_HEADROOM);
        assert_eq!(surface_count(16), 16 + 1 + PRESENTER_HEADROOM);
    }

    /// The headroom must cover what the client pipeline actually holds, which the
    /// native Vulkan rung measured before this crate existed. Pinning it to that
    /// crate's constant means a future re-measurement moves both rungs together
    /// instead of leaving this one quietly short.
    #[test]
    fn the_headroom_matches_the_pipeline_depth_the_vulkan_rung_measured() {
        assert_eq!(
            PRESENTER_HEADROOM,
            pf_vkdecode::images::HOLD_HEADROOM as usize
        );
    }
}
