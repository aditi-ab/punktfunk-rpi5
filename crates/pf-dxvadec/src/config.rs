//! The decoder-creation decisions, extracted from the Windows plumbing so they
//! are testable everywhere: which DXVA profile a stream needs, which
//! `D3D11_VIDEO_DECODER_CONFIG` to pick out of the ones a driver offers, how big
//! the decode surfaces must be, and how many of them there are.
//!
//! None of this touches D3D11. `video_d3d11_native.rs` enumerates and creates;
//! everything it *chooses* is decided here, against values it read from the
//! driver — the same split `pf-vkdecode`'s [`caps`](pf_vkdecode::caps) uses for
//! the Vulkan rung, and for the same reason: on a box we cannot run, a decision
//! table that has unit tests is the only part of decoder creation that can be
//! known-good before it ships.

/// A DXVA decode profile: the GUID `ID3D11VideoDevice::GetVideoDecoderProfile`
/// returns and `D3D11_VIDEO_DECODER_DESC::Guid` takes.
///
/// The GUID is a `u128` rather than a `windows_core::GUID` so this module builds
/// on every host; the Windows side converts with `GUID::from_u128`, exactly as
/// `video_d3d11.rs` already does for the same four constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaProfile {
    /// For logs and errors — a field report saying "no HEVC Main10 profile" is
    /// worth a great deal more than one saying "no 107af0e0-…".
    pub name: &'static str,
    pub guid: u128,
    /// The DXGI format the decode surfaces must carry for this profile:
    /// `DXGI_FORMAT_NV12` (103) for 8-bit, `DXGI_FORMAT_P010` (104) for 10-bit.
    /// The raw code point, for the same host-portability reason as the GUID —
    /// and `i32` rather than `u32` because that is exactly what windows-rs's
    /// `DXGI_FORMAT` is (a plain type alias, not a newtype), so the Windows side
    /// passes this value through with no cast and no chance of a sign surprise.
    pub dxgi_format: i32,
}

/// `DXGI_FORMAT_NV12`.
pub const DXGI_FORMAT_NV12: i32 = 103;
/// `DXGI_FORMAT_P010`.
pub const DXGI_FORMAT_P010: i32 = 104;

/// `D3D11_DECODER_PROFILE_H264_VLD_NOFGT` — H.264 VLD, no film grain. The one
/// H.264 profile every vendor exposes; the film-grain variants are for a tool no
/// punktfunk host encodes with.
pub const H264_VLD_NOFGT: DxvaProfile = DxvaProfile {
    name: "H.264 VLD NoFGT",
    guid: 0x1b81be68_a0c7_11d3_b984_00c04f2e73c5,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN` — HEVC Main (8-bit 4:2:0).
pub const HEVC_VLD_MAIN: DxvaProfile = DxvaProfile {
    name: "HEVC Main",
    guid: 0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10` — HEVC Main 10 (10-bit 4:2:0), the
/// HDR profile.
pub const HEVC_VLD_MAIN10: DxvaProfile = DxvaProfile {
    name: "HEVC Main10",
    guid: 0x107af0e0_ef1a_4d19_aba8_67a163073d13,
    dxgi_format: DXGI_FORMAT_P010,
};

/// `D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0` — AV1 Profile 0 (4:2:0, 8 **or** 10
/// bits). The same GUID `video_d3d11.rs` already hands the FFmpeg rung.
///
/// AV1 numbers its profiles by CHROMA SAMPLING, not by depth: Profile 0 is 4:2:0
/// at 8 and 10 bits both, so [`AV1_VLD_PROFILE0_10BIT`] below repeats this GUID
/// with the other surface format rather than naming a second profile. (Profile 1
/// is 4:4:4 and Profile 2 is 4:2:2/12-bit; neither has a rung here — see
/// [`profile_for`].)
pub const AV1_VLD_PROFILE0: DxvaProfile = DxvaProfile {
    name: "AV1 Profile 0",
    guid: 0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// AV1 Profile 0 decoding TEN-bit 4:2:0 into P010 — the same profile GUID as
/// [`AV1_VLD_PROFILE0`], a different surface format.
///
/// Two constants rather than one plus a format argument because the format is
/// what `CheckVideoDecoderFormat` is asked about and what the pool is allocated
/// with: a profile whose GUID is supported at NV12 and not at P010 is a real
/// answer a driver can give, and the caller must be able to ask the question.
pub const AV1_VLD_PROFILE0_10BIT: DxvaProfile = DxvaProfile {
    name: "AV1 Profile 0 (10-bit)",
    guid: 0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a,
    dxgi_format: DXGI_FORMAT_P010,
};

/// Which codec this decoder was built for. The negotiated codec picks it once, at
/// construction — the same shape as `video_vk_native`'s `NativeCodec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

/// The profile a stream of this codec, chroma format and bit depth needs, or
/// `None` when the combination is outside what this backend decodes.
///
/// Refusals here are the ladder's cheap exit: they happen before a decoder
/// exists, so the caller falls through to the FFmpeg rung with a clean stream
/// rather than burning the opening IDR. What is deliberately refused:
///
/// * anything but 4:2:0 — the DXVA 4:4:4 RExt profiles exist, but the hand-off
///   path this backend feeds is a fixed-function `VideoProcessorBlt` whose 4:4:4
///   input support is not a thing we have ever measured;
/// * H.264 above 8-bit — `High10` has no mainstream DXVA profile GUID, and no
///   punktfunk host emits it;
/// * HEVC above 10-bit;
/// * AV1 above 10-bit (Profile 2's 12-bit) and AV1 monochrome — an AV1 sequence
///   with `mono_chrome` set reads as `chroma_format_idc` 0 here and is refused
///   with every other non-4:2:0 shape.
pub fn profile_for(codec: Codec, chroma_format_idc: u8, bit_depth: u8) -> Option<DxvaProfile> {
    if chroma_format_idc != 1 {
        return None;
    }
    match (codec, bit_depth) {
        (Codec::H264, 8) => Some(H264_VLD_NOFGT),
        (Codec::H265, 8) => Some(HEVC_VLD_MAIN),
        (Codec::H265, 10) => Some(HEVC_VLD_MAIN10),
        (Codec::Av1, 8) => Some(AV1_VLD_PROFILE0),
        (Codec::Av1, 10) => Some(AV1_VLD_PROFILE0_10BIT),
        _ => None,
    }
}

/// The `ConfigBitstreamRaw` value that means "short-format slice control" for a
/// codec.
///
/// The two specs number this differently and it is the single most important
/// number in decoder-config selection, because it decides which slice-control
/// STRUCT the driver is going to read:
///
/// * H.264: `1` = long format (`DXVA_Slice_H264_Long`), `2` = short format
///   (`DXVA_Slice_H264_Short`);
/// * HEVC: `1` = short format (`DXVA_Slice_HEVC_Short`) — the only format the
///   HEVC spec defines;
/// * AV1: `1`, and there is no second value. The AV1 DXVA specification defines
///   one slice-control record (`DXVA_Tile_AV1`) and no long form, so `1` is not
///   "the short one" so much as "the only one".
///
/// This backend implements short format only, for every codec: the long format
/// additionally carries the derived reference lists and the prediction weight
/// tables per slice, which is a second derivation of everything the picture
/// parameters already say, with a second chance to get it wrong. A device that
/// offers no short-format config is refused and the ladder answers with the
/// FFmpeg rung, which implements both.
///
/// The AV1 value is not a guess: libavcodec's own
/// `dxva_get_decoder_configuration` (`dxva2.c`, n8.1) scores
/// `ConfigBitstreamRaw == 1` for EVERY codec and additionally accepts `2` only
/// `if (avctx->codec_id == AV_CODEC_ID_H264)`. Anything else it `continue`s past,
/// so a device offering AV1 at some other value is a device libavcodec's D3D11VA
/// hwaccel refuses too.
pub const fn short_slice_config(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 2,
        Codec::H265 | Codec::Av1 => 1,
    }
}

/// One driver-offered decoder config, reduced to the three facts selection needs.
/// The Windows side fills this from a `D3D11_VIDEO_DECODER_CONFIG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigFacts {
    /// `D3D11_VIDEO_DECODER_CONFIG::ConfigBitstreamRaw`.
    pub bitstream_raw: u32,
    /// Is `guidConfigBitstreamEncryption` the all-zero "no encryption" GUID?
    pub no_encryption: bool,
    /// `ConfigMinRenderTargetBuffCount` — the driver's own floor on how many
    /// decode surfaces must exist. Honoured by [`pool_size`].
    pub min_render_target_buffers: u16,
}

/// Pick a decoder config: the first short-format one, preferring an unencrypted
/// bitstream.
///
/// Returns the INDEX into `configs` so the caller can hand back the driver's own
/// `D3D11_VIDEO_DECODER_CONFIG` untouched — re-synthesising a config from these
/// three fields would drop the dozen `Config*` members a driver may care about.
///
/// `None` means the device offers no short-format config for this profile, which
/// is a refusal, not a fallback: submitting short-format slice records against a
/// long-format config is exactly the kind of mismatch that decodes to garbage
/// instead of failing.
pub fn pick_config(codec: Codec, configs: &[ConfigFacts]) -> Option<usize> {
    let want = short_slice_config(codec);
    let mut best: Option<(usize, u8)> = None;
    for (index, cfg) in configs.iter().enumerate() {
        if cfg.bitstream_raw != want {
            continue;
        }
        // Unencrypted beats encrypted; among equals the first wins, so the
        // driver's own preference order is preserved.
        let score = u8::from(cfg.no_encryption);
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((index, score));
        }
    }
    best.map(|(index, _)| index)
}

/// The macroblock/CTB alignment a codec's decode surfaces need.
///
/// H.264 is macroblock-aligned (16). HEVC asks for 128: the DXVA HEVC spec
/// requires surfaces aligned to 128 luma samples so every coding feature has room
/// to work in, and libavcodec's `ff_dxva2_common_frame_params` applies exactly
/// this. Getting it wrong is not a validation failure — it is the class of bug
/// that shows up as smeared bottom rows, which this codebase has already paid for
/// once on the CSC side.
///
/// **AV1 is 128 too**, and that is the same function's answer rather than an
/// analogy: `ff_dxva2_common_frame_params` tests
/// `avctx->codec_id == AV_CODEC_ID_HEVC || avctx->codec_id == AV_CODEC_ID_AV1` in
/// ONE condition. (AV1's own superblock is 64 or 128 samples, so 128 also covers
/// the largest of them, but the reason it is written here is the measured one.)
pub const fn surface_alignment(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 16,
        Codec::H265 | Codec::Av1 => 128,
    }
}

/// Round a coded dimension up to the codec's surface alignment.
pub const fn align_surface(value: u32, codec: Codec) -> u32 {
    let align = surface_alignment(codec);
    value.div_ceil(align) * align
}

/// How many decode surfaces the pool holds: one per DPB slot, or the driver's own
/// minimum when that is larger.
///
/// `dpb_slots` is the [`crate::SlotMap`] capacity (`max_dpb_frames + 1`) — the
/// exact number of pictures that can be simultaneously live, because a DXVA
/// surface IS the picture: unlike the Vulkan rung, whose picture pool is
/// deliberately decoupled from its DPB slots, here the surface index in
/// `RefFrameList` is the slot index, so one surface per slot is not a choice but
/// the data model.
///
/// There is deliberately no spare on top. Surface indices come from the slot map
/// and the map's capacity IS `dpb_slots`, so a `dpb_slots + 1`th surface is one
/// no submission could ever name — a frame of VRAM (a hundred megabytes at 4K)
/// bought for a picture that cannot exist. Nor is one needed for the hand-off:
/// its `VideoProcessorBlt` is queued on the same immediate context as the decode,
/// so the ordering is the driver's to keep, not ours to buy with an extra
/// allocation.
///
/// `driver_min` is the config's `ConfigMinRenderTargetBuffCount`, honoured because
/// some drivers genuinely refuse to decode into a smaller pool — and it is the
/// one honest route to a pool larger than the slot map, because it is the driver
/// asking rather than us guessing.
pub fn pool_size(dpb_slots: usize, driver_min: u16) -> u32 {
    // A DXVA surface index is seven bits (`DXVA_PicEntry::Index7Bits`), so 127 is
    // the hard ceiling regardless of what a driver asks for.
    (dpb_slots.max(usize::from(driver_min)).min(127)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_table_covers_exactly_the_shapes_this_backend_decodes() {
        assert_eq!(profile_for(Codec::H264, 1, 8), Some(H264_VLD_NOFGT));
        assert_eq!(profile_for(Codec::H265, 1, 8), Some(HEVC_VLD_MAIN));
        assert_eq!(profile_for(Codec::H265, 1, 10), Some(HEVC_VLD_MAIN10));
        // A Main10 stream decodes into P010, not NV12 — the surface format is
        // part of the profile choice, not a separate decision the caller makes.
        assert_eq!(
            profile_for(Codec::H265, 1, 10).map(|p| p.dxgi_format),
            Some(DXGI_FORMAT_P010)
        );
        assert_eq!(
            profile_for(Codec::H265, 1, 8).map(|p| p.dxgi_format),
            Some(DXGI_FORMAT_NV12)
        );
        // AV1 Profile 0 covers 8 AND 10 bits under ONE GUID, so the pair differs
        // only in the surface format — the one thing that must NOT be shared,
        // since it is what the pool is allocated with.
        assert_eq!(profile_for(Codec::Av1, 1, 8), Some(AV1_VLD_PROFILE0));
        assert_eq!(profile_for(Codec::Av1, 1, 10), Some(AV1_VLD_PROFILE0_10BIT));
        assert_eq!(AV1_VLD_PROFILE0.guid, AV1_VLD_PROFILE0_10BIT.guid);
        assert_eq!(AV1_VLD_PROFILE0.dxgi_format, DXGI_FORMAT_NV12);
        assert_eq!(AV1_VLD_PROFILE0_10BIT.dxgi_format, DXGI_FORMAT_P010);
        // The GUID `video_d3d11.rs` hands the FFmpeg rung for AV1
        // (`PROFILE_AV1_VLD_PROFILE0`), transcribed here so a typo in one of the
        // two is a failing test rather than a rung that quietly never engages.
        assert_eq!(
            AV1_VLD_PROFILE0.guid,
            0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a
        );
    }

    #[test]
    fn shapes_outside_the_envelope_are_refused_rather_than_approximated() {
        // 4:4:4 and 4:2:2 have no rung here at all.
        assert_eq!(profile_for(Codec::H264, 3, 8), None);
        assert_eq!(profile_for(Codec::H265, 3, 10), None);
        assert_eq!(profile_for(Codec::H265, 2, 8), None);
        // H.264 High10 has no mainstream DXVA profile.
        assert_eq!(profile_for(Codec::H264, 1, 10), None);
        // 12-bit HEVC likewise.
        assert_eq!(profile_for(Codec::H265, 1, 12), None);
        // AV1: Profile 1 (4:4:4) and Profile 2 (4:2:2 / 12-bit) have no rung
        // here, and neither does monochrome — which the AV1 planner reports as
        // `chroma_format_idc` 0, i.e. it lands in the same refusal as 4:4:4
        // rather than being mistaken for 4:2:0.
        assert_eq!(profile_for(Codec::Av1, 3, 8), None);
        assert_eq!(profile_for(Codec::Av1, 3, 10), None);
        assert_eq!(profile_for(Codec::Av1, 1, 12), None);
        assert_eq!(profile_for(Codec::Av1, 0, 8), None);
    }

    #[test]
    fn short_slice_control_is_2_for_h264_and_1_for_hevc_and_av1() {
        // The one number whose two spellings would silently swap the slice
        // struct a driver reads. `2` is H.264's and H.264's alone — libavcodec's
        // own config scoring accepts it `if (codec_id == AV_CODEC_ID_H264)` and
        // takes `1` everywhere else.
        assert_eq!(short_slice_config(Codec::H264), 2);
        assert_eq!(short_slice_config(Codec::H265), 1);
        assert_eq!(short_slice_config(Codec::Av1), 1);
    }

    #[test]
    fn config_selection_takes_a_short_format_config_and_prefers_no_encryption() {
        let configs = [
            ConfigFacts {
                bitstream_raw: 1, // H.264 long format — unusable here
                no_encryption: true,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: false,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
        ];
        assert_eq!(pick_config(Codec::H264, &configs), Some(2));
        // For HEVC the same array reads the other way round: 1 IS short format
        // there, and 2 means nothing. AV1 reads it the HEVC way.
        assert_eq!(pick_config(Codec::H265, &configs), Some(0));
        assert_eq!(pick_config(Codec::Av1, &configs), Some(0));
    }

    #[test]
    fn a_device_with_no_short_format_config_is_refused_not_downgraded() {
        let long_only = [ConfigFacts {
            bitstream_raw: 1,
            no_encryption: true,
            min_render_target_buffers: 0,
        }];
        assert_eq!(pick_config(Codec::H264, &long_only), None);
        assert_eq!(pick_config(Codec::H264, &[]), None);
    }

    #[test]
    fn among_equal_configs_the_drivers_own_order_wins() {
        let configs = [
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
        ];
        assert_eq!(pick_config(Codec::H264, &configs), Some(0));
    }

    #[test]
    fn surfaces_are_macroblock_aligned_for_h264_and_128_aligned_for_hevc() {
        assert_eq!(align_surface(1920, Codec::H264), 1920);
        assert_eq!(align_surface(1080, Codec::H264), 1088);
        assert_eq!(align_surface(1920, Codec::H265), 1920);
        assert_eq!(align_surface(1080, Codec::H265), 1152);
        // The 3840x2400 shape that produced the green bar on Intel: 2400 is
        // already a multiple of 128, so nothing moves.
        assert_eq!(align_surface(3840, Codec::H265), 3840);
        assert_eq!(align_surface(2400, Codec::H265), 2432);
        assert_eq!(align_surface(2432, Codec::H265), 2432);
        // AV1 shares HEVC's granule (`ff_dxva2_common_frame_params` tests the two
        // codec ids in one condition), so the 320x240 conformance vector decodes
        // into a 384x256 surface and the chroma plane starts 256 rows down — the
        // geometry the parity readback has to use.
        assert_eq!(align_surface(320, Codec::Av1), 384);
        assert_eq!(align_surface(240, Codec::Av1), 256);
        assert_eq!(align_surface(1920, Codec::Av1), 1920);
        assert_eq!(align_surface(1080, Codec::Av1), 1152);
    }

    #[test]
    fn an_av1_pool_is_the_eight_reference_slots_plus_the_current_picture() {
        // AV1's DPB depth is a CONSTANT of the codec (`NUM_REF_FRAMES` = 8), not
        // an SPS field, so the pool is always nine surfaces — which is also
        // libavcodec's `num_surfaces = 1 + 8` for `AV_CODEC_ID_AV1`. A driver
        // asking for more still wins.
        assert_eq!(pool_size(9, 0), 9);
        assert_eq!(pool_size(9, 16), 16);
    }

    #[test]
    fn the_pool_holds_exactly_one_surface_per_dpb_slot_and_no_unaddressable_spare() {
        // A surface index comes from the slot map, whose capacity is this number:
        // one more would be a surface no submission could ever name.
        assert_eq!(pool_size(17, 0), 17);
        assert_eq!(pool_size(2, 0), 2);
    }

    #[test]
    fn the_pool_honours_a_drivers_own_minimum_and_the_seven_bit_index_ceiling() {
        assert_eq!(pool_size(3, 12), 12);
        assert_eq!(pool_size(3, 2), 3);
        // Seven-bit surface indices cap the pool no matter who asks.
        assert_eq!(pool_size(200, 0), 127);
        assert_eq!(pool_size(3, 4000), 127);
    }
}
