//! NVENC hardware encoder (Windows, D3D11 input) — zero-copy capture→encode on the GPU.
//!
//! Drives the raw NVENC API via `nvidia_video_codec_sdk::{sys, ENCODE_API}` (the safe `Encoder`
//! wrapper is CUDA-only). Opens an encode session bound to the **same** `ID3D11Device` as the DXGI
//! capturer (the device is carried on `FramePayload::D3d11`), and **encodes the capturer's texture in
//! place** — it registers each input texture with NVENC once (cached by pointer) and `encode_picture`s
//! it directly, with NO per-frame `CopyResource`. (That's safe because the host encode loop is
//! synchronous — capture → submit → poll, where `poll`/`lock_bitstream` blocks until the encode
//! finishes — so the capturer never overwrites the texture mid-encode; if that loop ever becomes
//! pipelined, the capturer must hand a ring of textures.) Mirrors the Linux NVENC config: CBR +
//! ultra-low-latency, infinite GOP, P-frames only, forced-IDR for RFI, in-band SPS/PPS each keyframe.
//!
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise) — compiles GPU-less, but
//! `open`/`submit` only succeed on a GPU box. The software encoder (`super::sw`) is the fallback.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use nvidia_video_codec_sdk::ENCODE_API as API;

// Output bitstream buffers = max in-flight encodes. The helper deep-pipelines (submits several frames
// before locking the oldest) so per-frame GPU-scheduling waits OVERLAP instead of serializing under a
// GPU-saturating game; this must be ≥ the helper's `PUNKTFUNK_ENCODE_DEPTH` (default 4, clamped ≤ 6).
const POOL: usize = 8;

fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
    }
}

pub struct NvencD3d11Encoder {
    encoder: *mut c_void,
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    /// Encoded bit depth (8 or 10). 10 → HEVC Main10 (NVENC upconverts the 8-bit ARGB input).
    bit_depth: u8,
    /// HDR: the capturer is delivering BT.2020 PQ 10-bit (`PixelFormat::Rgb10a2`) frames. Sets the
    /// `ABGR10` input format + the BT.2020/PQ colour VUI. Derived per-frame from the capture format
    /// (HDR can toggle mid-session); a change re-inits the session.
    hdr: bool,
    /// Registrations of the capturer's input textures, cached by texture raw pointer — NVENC encodes
    /// them in place (no per-frame copy). The cloned `ID3D11Texture2D` keeps each alive until we
    /// unregister it (the capturer may drop its copy on a device recreate before our teardown runs).
    regs: HashMap<isize, (nv::NV_ENC_REGISTERED_PTR, ID3D11Texture2D)>,
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// (bitstream, mapped input resource to unmap after retrieval, pts_ns) per in-flight encode.
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64)>,
    frame_idx: i64,
    force_kf: bool,
    inited: bool,
    /// Raw ptr of the D3D11 device this session was initialized with. The capturer recreates the
    /// device on a desktop switch (normal ↔ Winlogon secure); when a frame carries a new device we
    /// tear down and re-init NVENC against it.
    init_device: *mut c_void,
}

// Raw NVENC handle + COM ptrs; confined to the single encode thread (like the Linux encoder).
unsafe impl Send for NvencD3d11Encoder {}

impl NvencD3d11Encoder {
    pub fn open(
        codec: Codec,
        _format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
    ) -> Result<Self> {
        Ok(Self {
            encoder: ptr::null_mut(),
            codec_guid: codec_guid(codec),
            width,
            height,
            fps,
            bitrate_bps,
            buffer_fmt: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bit_depth,
            hdr: false,
            regs: HashMap::new(),
            next: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            inited: false,
            init_device: ptr::null_mut(),
        })
    }

    /// Tear down the encode session + pooled resources. Reused on a capture-device change (desktop
    /// switch) and at Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Unmap any in-flight inputs, then unregister every cached texture and destroy the bitstreams.
        for (_, map, _) in &self.pending {
            if !map.is_null() {
                let _ = (API.unmap_input_resource)(self.encoder, *map);
            }
        }
        for (reg, _tex) in self.regs.values() {
            let _ = (API.unregister_resource)(self.encoder, *reg);
        }
        for &bs in &self.bitstreams {
            let _ = (API.destroy_bitstream_buffer)(self.encoder, bs);
        }
        let _ = (API.destroy_encoder)(self.encoder);
        self.regs.clear(); // drops the texture clones, releasing our refs
        self.bitstreams.clear();
        self.pending.clear();
        self.encoder = ptr::null_mut();
        self.inited = false;
        self.next = 0;
    }

    /// Open + configure + initialize ONE NVENC session at `bitrate` (bps) and `split_mode`. Returns
    /// the session handle, or destroys it and returns the error. NVENC has no re-init after a failed
    /// `initialize_encoder`, so the bitrate-clamp search in `init_session` calls this once per probe.
    unsafe fn try_open_session(
        &self,
        device: &ID3D11Device,
        bitrate: u64,
        split_mode: u32,
    ) -> Result<*mut c_void> {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        (API.open_encode_session_ex)(&mut params, &mut enc)
            .result_without_string()
            .map_err(|e| anyhow!("NVENC open_encode_session_ex: {e:?} (no NVIDIA GPU?)"))?;

        // Seed the P1 + ultra-low-latency preset config.
        let mut preset = nv::NV_ENC_PRESET_CONFIG {
            version: nv::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        if let Err(e) = (API.get_encode_preset_config_ex)(
            enc,
            self.codec_guid,
            nv::NV_ENC_PRESET_P1_GUID,
            nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset,
        )
        .result_without_string()
        {
            let _ = (API.destroy_encoder)(enc);
            return Err(anyhow!("get_encode_preset_config_ex: {e:?}"));
        }
        let mut cfg = preset.presetCfg;

        // Mirror the Linux RC config: CBR, infinite GOP, P-only, ~1-frame VBV.
        cfg.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
        cfg.frameIntervalP = 1;
        cfg.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        let bps = bitrate.min(u32::MAX as u64) as u32;
        cfg.rcParams.averageBitRate = bps;
        cfg.rcParams.maxBitRate = bps;
        // Shrink the VBV with the bitrate — NVENC validates it against the same level ceiling.
        let vbv = (bitrate as f64 / self.fps.max(1) as f64) as u32;
        cfg.rcParams.vbvBufferSize = vbv;
        cfg.rcParams.vbvInitialDelay = vbv;

        // HIGH tier + autoselect level. The codec's PER-LEVEL bitrate ceiling is otherwise the
        // MAIN-tier cap — for HEVC at 5K that's Level 6.2 Main ≈ 240 Mbps. HIGH tier lifts the HEVC
        // ceiling to ≈800 Mbps (AV1 higher still); autoselect lets NVENC pick the level for the
        // tier+bitrate. `tier`/`level` are u32 (HIGH=1, AUTOSELECT=0); HEVC/AV1 share the union offset.
        cfg.encodeCodecConfig.hevcConfig.tier = 1;
        cfg.encodeCodecConfig.hevcConfig.level = 0;

        // 10-bit HEVC Main10 (HDR foundation): NVENC upconverts the 8-bit input; 8-bit leaves the
        // preset default (Main) untouched.
        if self.bit_depth == 10 {
            cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID;
            cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2); // 10 - 8
        }

        // HDR colour signaling: BT.2020 primaries + SMPTE ST 2084 (PQ) in the HEVC VUI.
        if self.hdr {
            let vui = &mut cfg.encodeCodecConfig.hevcConfig.hevcVUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFullRangeFlag = 0; // limited (studio) range — NVENC RGB→YUV default
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            vui.transferCharacteristics =
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
            vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        }

        let mut init = nv::NV_ENC_INITIALIZE_PARAMS {
            version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: self.codec_guid,
            presetGUID: nv::NV_ENC_PRESET_P1_GUID,
            tuningInfo: nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            encodeWidth: self.width,
            encodeHeight: self.height,
            darWidth: self.width,
            darHeight: self.height,
            frameRateNum: self.fps,
            frameRateDen: 1,
            enablePTD: 1,
            encodeConfig: &mut cfg,
            ..Default::default()
        };
        // splitEncodeMode is a C bitfield — set via the generated accessor, not a struct field.
        init.set_splitEncodeMode(split_mode);

        match (API.initialize_encoder)(enc, &mut init).result_without_string() {
            Ok(()) => Ok(enc),
            Err(e) => {
                let _ = (API.destroy_encoder)(enc);
                Err(anyhow!("initialize_encoder: {e:?}"))
            }
        }
    }

    /// Lazily create the session on the first frame's D3D11 device (so capture + encode share it).
    fn init_session(&mut self, device: &ID3D11Device) -> Result<()> {
        unsafe {
            // Bitrate clamp (see the search below): NVENC rejects `initialize_encoder` when the bitrate
            // exceeds the GPU's max codec level. We try the requested rate, then binary-search down to
            // the MAX the level accepts and clamp to it — so an over-asking client (e.g. 1 Gbps on HEVC)
            // gets the highest the GPU can actually do, not a coarse fraction of it.
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // 2-way NVENC split-frame encoding (Ada dual-NVENC) — the high-pixel-rate throughput lever
            // the Linux host enables via libavcodec `split_encode_mode`. A single Ada NVENC session tops
            // out ~0.8 Gpix/s, so at high motion a 5K@240 (1.77 Gpix/s) frame takes ~8 ms to encode and
            // the rate caps ~125 fps; splitting across both engines roughly halves that. Force 2-way
            // above ~1 Gpix/s (matching encode/linux.rs), AUTO below (the ~2% BD-rate cost isn't worth
            // it at low pixel rates). Env override PUNKTFUNK_SPLIT_ENCODE = 0/disable | 1/auto | 2 | 3.
            // HEVC/AV1 only; the init-failure fallback below disables it if a codec/config rejects it.
            let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
            let mut split_mode: u32 = match std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok().as_deref()
            {
                Some("0") | Some("disable") => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32
                }
                Some("1") | Some("auto") => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
                }
                Some("3") => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                Some("2") => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
                // Main10 (10-bit / HDR): 2-way split is measurably SLOWER on Ada — at 5120x1440@240
                // Main10, forced-2 took 7.6 ms/frame (~131 fps) vs 2.8 ms (~357 fps) single-engine
                // (the split/merge overhead dominates for 10-bit). A single Ada NVENC engine already
                // handles 5K@240 Main10 well under the 4.17 ms budget, so DON'T split — splitting was
                // the "broken animations in HDR" (the stream capped at ~131 fps). Env still overrides.
                _ if self.bit_depth >= 10 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32
                }
                _ if pixel_rate > 1_000_000_000 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
                }
                _ => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_MODE as u32,
            };
            tracing::info!(
                split_mode,
                bit_depth = self.bit_depth,
                pixel_rate,
                "NVENC split-encode mode (0=disable 1=auto-forced 2=two 3=three 4=auto)"
            );
            // Find the highest bitrate the GPU's codec LEVEL accepts and CLAMP to it. NVENC rejects
            // `initialize_encoder` (InvalidParam) when the bitrate exceeds the level ceiling (e.g. a
            // 1 Gbps request on HEVC). Strategy: try the requested rate; if the only problem is a forced
            // split-encode mode the codec doesn't support, disable split and retry; if the bitrate
            // itself is too high, binary-search [FLOOR, requested] for the MAX accepted rate and clamp
            // to THAT (don't undershoot — the old ×¾ step-down landed well below the real ceiling).
            const CLAMP_TOL_BPS: u64 = 20_000_000; // stop bisecting within ~20 Mbps of the ceiling

            let mut probe = self.try_open_session(device, requested_bps, split_mode);
            // Disambiguate a forced-split rejection from a bitrate-cap rejection: retry once at the
            // requested rate with split disabled — if THAT succeeds, split was the problem, not bitrate.
            let split_forced = split_mode
                != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_MODE as u32
                && split_mode != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
            if probe.is_err() && split_forced {
                let no_split = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                if let Ok(e) = self.try_open_session(device, requested_bps, no_split) {
                    tracing::warn!("NVENC: split-encode rejected by codec/config — disabled");
                    split_mode = no_split;
                    probe = Ok(e);
                }
            }

            let enc = match probe {
                Ok(enc) => {
                    self.bitrate_bps = requested_bps;
                    enc
                }
                Err(_) => {
                    // Requested bitrate exceeds the codec-level ceiling — binary-search the max accepted.
                    // `lo` is the highest known-good rate (FLOOR is assumed to fit), `hi` the lowest
                    // rejected; `best` holds the live session at `lo` so we end up with the clamped one.
                    let mut lo = FLOOR_BPS;
                    let mut hi = requested_bps;
                    let mut best: *mut c_void = ptr::null_mut();
                    let mut best_bps = 0u64;
                    while hi > lo + CLAMP_TOL_BPS {
                        let mid = lo + (hi - lo) / 2;
                        match self.try_open_session(device, mid, split_mode) {
                            Ok(e) => {
                                if !best.is_null() {
                                    let _ = (API.destroy_encoder)(best);
                                }
                                best = e;
                                best_bps = mid;
                                lo = mid;
                            }
                            Err(_) => hi = mid,
                        }
                    }
                    if best.is_null() {
                        // Nothing in (FLOOR, requested] accepted — fall back to the floor itself, also
                        // trying split-disabled in case a forced split (not the bitrate) is the blocker.
                        let no_split =
                            nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                        best = self
                            .try_open_session(device, FLOOR_BPS, split_mode)
                            .or_else(|_| self.try_open_session(device, FLOOR_BPS, no_split))
                            .context(
                                "NVENC initialize_encoder rejected even at the floor bitrate",
                            )?;
                        best_bps = FLOOR_BPS;
                    }
                    tracing::warn!(
                        requested_mbps = requested_bps / 1_000_000,
                        clamped_mbps = best_bps / 1_000_000,
                        "NVENC: requested bitrate above the GPU codec-level ceiling — clamped to the max accepted"
                    );
                    self.bitrate_bps = best_bps;
                    best
                }
            };
            self.encoder = enc;
            if self.bitrate_bps < requested_bps {
                tracing::info!(
                    requested_mbps = requested_bps / 1_000_000,
                    applied_mbps = self.bitrate_bps / 1_000_000,
                    "NVENC bitrate capped to this GPU's max for the codec"
                );
            }

            // 5. one output bitstream per in-flight slot. There is NO encoder-owned input pool: the
            // capturer's textures are registered on demand in `submit` and encoded in place.
            for _ in 0..POOL {
                let mut cb = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                (API.create_bitstream_buffer)(enc, &mut cb)
                    .result_without_string()
                    .map_err(|e| anyhow!("create_bitstream_buffer: {e:?}"))?;
                self.bitstreams.push(cb.bitstreamBuffer);
            }
            self.inited = true;
            tracing::info!(
                "NVENC D3D11 session: {}x{}@{} {}-bit{} {} Mbps {:?}",
                self.width,
                self.height,
                self.fps,
                self.bit_depth,
                if self.hdr { " HDR(BT.2020 PQ)" } else { "" },
                self.bitrate_bps / 1_000_000,
                self.codec_guid
            );
            Ok(())
        }
    }
}

impl Encoder for NvencD3d11Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!("NVENC D3D11 encoder needs a GPU texture frame (use the software encoder for CPU frames)")
            }
        };
        // The capturer recreates its D3D11 device on a desktop switch (secure/Winlogon) and may come
        // back at a different resolution (user session applies its own mode on login). Re-init when the
        // frame arrives on a different device OR at a different size than our session was built on.
        // HDR (BT.2020 PQ 10-bit) when the capturer hands us a 10-bit R10G10B10A2 frame. This can flip
        // mid-session when the user toggles HDR (which arrives as a capture device recreate anyway).
        // HDR (BT.2020 PQ) when the capturer hands a 10-bit frame — either R10G10B10A2 (the legacy
        // shader path) or P010 (the video-processor path). 8-bit NV12/ARGB → SDR.
        let hdr = matches!(captured.format, PixelFormat::Rgb10a2 | PixelFormat::P010);
        let dev_raw = frame.device.as_raw();
        let size_changed =
            self.inited && (self.width != captured.width || self.height != captured.height);
        let hdr_changed = self.inited && self.hdr != hdr;
        if self.inited && (self.init_device != dev_raw || size_changed || hdr_changed) {
            tracing::info!(
                device_changed = self.init_device != dev_raw,
                size_changed,
                hdr_changed,
                hdr,
                new = format!("{}x{}", captured.width, captured.height),
                "NVENC: capture device/size/HDR changed — re-initializing session"
            );
            unsafe { self.teardown() };
        }
        if !self.inited {
            // Adopt the current frame size + colour so the encoder always matches the capturer output.
            self.width = captured.width;
            self.height = captured.height;
            self.hdr = hdr;
            // Pick the NVENC input format from the captured pixel format. YUV (NV12/P010) is the
            // video-processor path — NVENC encodes it natively (no internal RGB→YUV, which is a hidden
            // 3D/compute step that would fight a GPU-saturating game). RGB (ARGB/ABGR10) is the legacy
            // shader path. 10-bit (P010/ABGR10) forces HEVC Main10 + the BT.2020 PQ VUI.
            self.buffer_fmt = match captured.format {
                PixelFormat::P010 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
                }
                PixelFormat::Rgb10a2 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                }
                PixelFormat::Nv12 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
                _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            };
            let device = frame.device.clone();
            self.init_session(&device)?;
            self.init_device = dev_raw;
        }
        let slot = self.next % POOL;
        self.next += 1;
        unsafe {
            // Register the capturer's texture with NVENC once (cached by raw pointer), then encode it
            // IN PLACE — no `CopyResource` into an encoder-owned pool. This is the zero-copy win: the
            // capturer already produced a stable GPU texture; we just register + map + encode it.
            let key = frame.texture.as_raw() as isize;
            if !self.regs.contains_key(&key) {
                let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType:
                        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                    width: self.width,
                    height: self.height,
                    pitch: 0,
                    resourceToRegister: frame.texture.as_raw(),
                    bufferFormat: self.buffer_fmt,
                    bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                (API.register_resource)(self.encoder, &mut rr)
                    .result_without_string()
                    .map_err(|e| anyhow!("register_resource: {e:?}"))?;
                self.regs
                    .insert(key, (rr.registeredResource, frame.texture.clone()));
            }
            let reg = self.regs[&key].0;

            let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
                version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: reg,
                ..Default::default()
            };
            (API.map_input_resource)(self.encoder, &mut mp)
                .result_without_string()
                .map_err(|e| anyhow!("map_input_resource: {e:?}"))?;

            let pts = self.frame_idx as u64;
            self.frame_idx += 1;
            let flags = if std::mem::take(&mut self.force_kf) {
                nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            };
            let mut pic = nv::NV_ENC_PIC_PARAMS {
                version: nv::NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: 0,
                inputBuffer: mp.mappedResource,
                bufferFmt: mp.mappedBufferFmt,
                outputBitstream: self.bitstreams[slot],
                pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: pts,
                encodePicFlags: flags as u32,
                ..Default::default()
            };
            (API.encode_picture)(self.encoder, &mut pic)
                .result_without_string()
                .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
            self.pending
                .push_back((self.bitstreams[slot], mp.mappedResource, captured.pts_ns));
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some((bs, map, pts_ns)) = self.pending.pop_front() else {
            return Ok(None);
        };
        unsafe {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: bs,
                ..Default::default()
            };
            (API.lock_bitstream)(self.encoder, &mut lock)
                .result_without_string()
                .map_err(|e| anyhow!("lock_bitstream: {e:?}"))?;
            let data = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let keyframe = matches!(
                lock.pictureType,
                nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            (API.unlock_bitstream)(self.encoder, bs)
                .result_without_string()
                .map_err(|e| anyhow!("unlock_bitstream: {e:?}"))?;
            if !map.is_null() {
                let _ = (API.unmap_input_resource)(self.encoder, map);
            }
            Ok(Some(EncodedFrame {
                data,
                pts_ns,
                keyframe,
            }))
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + frameIntervalP=1: each submit yields its AU; no internal queue to drain.
    }
}

impl Drop for NvencD3d11Encoder {
    fn drop(&mut self) {
        unsafe { self.teardown() };
    }
}
