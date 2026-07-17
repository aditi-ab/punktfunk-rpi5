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

/// Reference-frame DPB depth when RFI is supported (Apollo uses 5). A deeper DPB lets an invalidated
/// reference fall back to an older still-valid frame instead of a full IDR; `numRefL0 = 1` keeps each
/// P-frame single-reference for low latency. Also the window the backends' `invalidate_ref_frames`
/// paths check against (`frame_idx - RFI_DPB` = the oldest frame still in the DPB).
pub(super) const RFI_DPB: u32 = 5;

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

    // Chroma + bit depth. Full-chroma 4:4:4 (HEVC Range Extensions, chromaFormatIDC=3 under the FREXT
    // profile) takes precedence and composes with 10-bit (Main 4:4:4 10); it needs a full-chroma-
    // capable input. Otherwise 10-bit selects Main10 (HEVC) or the AV1 output depth — stamping the
    // HEVC Main10 GUID onto an AV1 session is an INVALID_PARAM, so bit depth is set PER CODEC.
    if c.chroma_444 && c.full_chroma_input {
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
