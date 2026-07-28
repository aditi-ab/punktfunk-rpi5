//! VAAPI encoder via `ffmpeg-next` — AMD (Mesa `radeonsi`) and Intel (`iHD`/`i965`) over one
//! libavcodec backend (`h264_vaapi`/`hevc_vaapi`/`av1_vaapi`). The kernel driver differs per
//! vendor; the libva userspace API is identical, so a single encoder covers both. This is the
//! sibling of [`super::linux`] (NVENC/CUDA) behind the shared [`Encoder`] trait — selected in
//! [`super::open_video`] (NVIDIA → NVENC, AMD/Intel → here).
//!
//! Two input paths, chosen lazily from the FIRST frame's payload (so `open_video`'s signature
//! is unchanged and the encoder self-configures for whatever the capturer produces):
//! * **CPU upload** ([`CpuInner`]): the portal hands packed RGB/BGR CPU frames; we swscale to
//!   BT.709-limited NV12 and `av_hwframe_transfer_data` it into a pooled VA surface. Works on any
//!   VAAPI GPU with no capture changes (the capturer falls back to CPU frames on non-NVIDIA).
//! * **Zero-copy dmabuf** ([`DmabufInner`], `PUNKTFUNK_ZEROCOPY=1`): the capturer hands a packed-RGB
//!   dmabuf. We wrap it as an `AV_PIX_FMT_DRM_PRIME` frame and push it through a tiny filter graph
//!   `buffer(drm_prime) → hwmap=derive_device=vaapi → scale_vaapi=format=nv12 → buffersink`, so
//!   the import AND the RGB→NV12 colour conversion run on the GPU's video engine — no host CSC, no
//!   upload. The encoder takes the NV12 surfaces straight from the filter sink.
//!
//! Raw FFI: `ffmpeg-next` has no hwcontext/filter wrappers for what we need, so the
//! hwdevice/hwframes/buffersrc/buffersink calls go through `ffmpeg::ffi` (= `ffmpeg_sys_next`),
//! as the CUDA encode path and the clients' decode paths already do. The encoder is opened
//! *without* a global header, so VPS/SPS/PPS are in-band on every IDR.
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary};
use ffmpeg_next as ffmpeg;
use pf_frame::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use super::libav::{
    apply_low_latency_rc, pixel_to_av, poll_encoder, AvBuffer, AvFilterGraph, PollOutcome,
    SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi; // = ffmpeg_sys_next

/// The render node a VAAPI/DRM device should open, from [`pf_gpu::linux_render_node`]: a
/// matched web-console GPU preference pins it, else `PUNKTFUNK_RENDER_NODE`, else the single-GPU
/// default.
fn render_node() -> CString {
    let p = pf_gpu::linux_render_node().to_string_lossy().into_owned();
    CString::new(p).unwrap_or_else(|_| CString::new("/dev/dri/renderD128").unwrap())
}

/// The swscale *source* pixel format for a captured CPU layout (packed RGB/BGR only).
fn vaapi_sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        // The GNOME 50+ HDR capture formats (PQ/BT.2020 packed 2:10:10:10) — the HDR CPU path's
        // swscale source for the X2RGB10→P010 conversion.
        PixelFormat::X2Rgb10 => Pixel::X2RGB10LE,
        PixelFormat::X2Bgr10 => Pixel::X2BGR10LE,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Yuv444 => {
            bail!("VAAPI CPU-input path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Which VAAPI entrypoint mode opened successfully: 1 = default (full-feature `EncSlice`),
/// 2 = low-power (`EncSliceLP`/VDEnc). Modern Intel (Gen12+/Arc) removed the full-feature encode
/// entrypoints, so the default open fails there and only `low_power=1` works; AMD (radeonsi) is the
/// reverse. Caching the resolved mode lets later sessions/probes skip the known-failing attempt
/// (and its libav error spew).
///
/// Keyed on **(render node, codec, bit depth)**, and every part of that key is load-bearing:
///
/// * The render node, because the entrypoint is a property of the DEVICE libva opens — and
///   `render_node()` follows the web-console GPU preference. This used to be a process-global array
///   keyed by codec alone, which made it a session-killer rather than a staleness bug: once a mode
///   is latched the open tries exactly ONE mode and the `[false, true]` fallback is gone. Latch
///   low-power on an Intel Arc, switch the preference to an AMD dGPU, and every VAAPI open there
///   passes `low_power=1` — which radeonsi rejects — with no full-feature retry, for the process
///   lifetime: the probe reports all-false AND the session's own encoder open fails.
///   Keyed on the node rather than `pf_gpu::selection_key()` on purpose — the node is literally what
///   `render_node()` hands libva, so key and device cannot describe different GPUs.
/// * The bit depth, because Main10 and 8-bit can resolve to different entrypoints on the same
///   device; one shared slot let an 8-bit answer pin the 10-bit open (HDR under-advertisement).
static LP_MODE: OnceLock<Mutex<HashMap<LpKey, u8>>> = OnceLock::new();

/// (render-node path, codec label, 10-bit) — see [`LP_MODE`].
type LpKey = (String, &'static str, bool);

/// The [`LP_MODE`] key for this device/codec/depth. `render_node()` is re-read rather than cached
/// so a GPU-preference change is picked up on the next open.
fn lp_key(codec: Codec, ten_bit: bool) -> LpKey {
    lp_key_for(&render_node().to_string_lossy(), codec, ten_bit)
}

/// [`lp_key`] with the render node explicit (device-free for the unit tests). Every part of the
/// key is load-bearing — see [`LP_MODE`] for the two field-motivated bugs (node: cross-GPU
/// session-killer; depth: HDR under-advertisement).
fn lp_key_for(node: &str, codec: Codec, ten_bit: bool) -> LpKey {
    (node.to_owned(), codec.label(), ten_bit)
}

/// `PUNKTFUNK_VAAPI_LOW_POWER` pins the entrypoint mode (`1` = low-power only, `0` = full-feature
/// only); unset → try full-feature first, fall back to low-power.
fn low_power_override() -> Option<bool> {
    parse_low_power(&std::env::var("PUNKTFUNK_VAAPI_LOW_POWER").ok()?)
}

/// [`low_power_override`]'s value grammar (device-free for the unit tests). Anything outside the
/// two literal sets means "no pin" — the `[full-feature, low-power]` ladder runs.
fn parse_low_power(raw: &str) -> Option<bool> {
    match raw.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The entrypoint modes to attempt, in order (`false` = full-feature `EncSlice`, `true` =
/// low-power VDEnc): an operator pin tries exactly that one; a cached resolution ([`LP_MODE`]:
/// 1 = full-feature, 2 = low-power) skips the known-failing attempt; anything else runs the full
/// ladder — full-feature first so AMD's first-try open stays byte-for-byte unchanged. Never
/// empty: [`open_vaapi_encoder`] relies on at least one attempt running.
fn entrypoint_ladder(pin: Option<bool>, cached: u8) -> &'static [bool] {
    match pin {
        Some(true) => &[true],
        Some(false) => &[false],
        None => match cached {
            1 => &[false],
            2 => &[true],
            _ => &[false, true],
        },
    }
}

/// The [`LP_MODE`] value a successful open latches (1 = full-feature, 2 = low-power). Paired with
/// [`entrypoint_ladder`], which maps it back to the single-mode retry list.
fn latched_mode(low_power: bool) -> u8 {
    if low_power {
        2
    } else {
        1
    }
}

/// The VUI colour metadata the encoder signals for the session's depth.
struct Vui {
    colorspace: ffi::AVColorSpace,
    range: ffi::AVColorRange,
    primaries: ffi::AVColorPrimaries,
    trc: ffi::AVColorTransferCharacteristic,
}

/// 10-bit HDR: BT.2020 primaries + SMPTE-2084 (PQ) transfer, limited range — matches the P010 the
/// CSC produces (swscale BT.2020 on the CPU path; `scale_vaapi` pinned to bt2020 on the zero-copy
/// path); the client decoder auto-detects PQ from the VUI. SDR: we hand the encoder BT.709
/// *limited* NV12 (swscale CSC on the CPU path; `scale_vaapi` pinned to
/// `out_color_matrix=bt709:out_range=limited` on the zero-copy path, with the full-range RGB input
/// tagged), so signal that VUI — else the client decoder washes the picture out.
fn vui_for(ten_bit: bool) -> Vui {
    if ten_bit {
        Vui {
            colorspace: ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL,
            range: ffi::AVColorRange::AVCOL_RANGE_MPEG,
            primaries: ffi::AVColorPrimaries::AVCOL_PRI_BT2020,
            trc: ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084,
        }
    } else {
        Vui {
            colorspace: ffi::AVColorSpace::AVCOL_SPC_BT709,
            range: ffi::AVColorRange::AVCOL_RANGE_MPEG,
            primaries: ffi::AVColorPrimaries::AVCOL_PRI_BT709,
            trc: ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709,
        }
    }
}

/// The explicit `profile` option for this codec/depth, or `None` to let the encoder derive it.
/// HEVC Main10 is pinned explicitly so the depth is never silently dropped (`hevc_vaapi` would
/// derive it from the P010 surfaces, but a derivation can regress quietly); 10-bit AV1 is
/// input-driven — no profile knob — and every 8-bit open keeps the encoder default.
fn explicit_profile(codec: Codec, ten_bit: bool) -> Option<&'static str> {
    (ten_bit && codec == Codec::H265).then_some("main10")
}

/// `PUNKTFUNK_VAAPI_ASYNC_DEPTH` grammar: 1..=8 accepted verbatim; unset, junk, and out-of-range
/// values all resolve to depth 1 — the lowest-latency structure (see the `async_depth` note at the
/// call site for why 1 is the default and what depth ≥ 2 trades).
fn async_depth(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.parse::<u32>().ok())
        .filter(|d| (1..=8).contains(d))
        .unwrap_or(1)
}

/// The `scale_vaapi` filter args for the session's depth. The VPP's output colour is pinned to
/// exactly what the encoder's VUI signals ([`vui_for`]) — without the explicit options the
/// conversion matrix is whatever the driver defaults to for an unspecified output (Mesa: BT.601),
/// a hue shift against the signaled VUI. (The PQ transfer is per-channel and rides through the
/// matrix untouched.)
fn scale_vaapi_args(ten_bit: bool) -> &'static CStr {
    if ten_bit {
        c"format=p010:out_color_matrix=bt2020:out_range=limited"
    } else {
        c"format=nv12:out_color_matrix=bt709:out_range=limited"
    }
}

/// What a (captured format, negotiated bit depth) pair resolves to at open. 10-bit rides on the
/// captured PIXELS, not the negotiated depth — see [`crate::ten_bit_input`] for the failure the
/// reverse shape produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DepthResolution {
    /// PQ-graded 10-bit frames on a non-10-bit session: refuse the open, so PQ content is never
    /// mislabeled BT.709.
    RefuseMislabeledPq,
    /// 10-bit negotiated but the capture stayed SDR: honestly encode 8-bit (with a warning).
    SdrDowngrade,
    /// Format and negotiated depth agree.
    Agreed,
}

/// See [`DepthResolution`].
fn resolve_depth(format: PixelFormat, bit_depth: u8) -> DepthResolution {
    if format.is_hdr_rgb10() && bit_depth != 10 {
        DepthResolution::RefuseMislabeledPq
    } else if bit_depth == 10 && !format.is_hdr_rgb10() {
        DepthResolution::SdrDowngrade
    } else {
        DepthResolution::Agreed
    }
}

/// Whether a codec is even eligible for the 10-bit probe: PyroWave answers 10-bit support on its
/// own path (no VAAPI involved), and a codec without a 10-bit profile can never pass.
fn ten_bit_probe_eligible(codec: Codec) -> bool {
    codec.supports_10bit() && codec != Codec::PyroWave
}

/// Open the VAAPI encoder, resolving the entrypoint mode: try the full-feature entrypoint first
/// and, if the driver rejects it, retry with `low_power=1` — modern Intel (Gen12+/Arc) exposes
/// ONLY the low-power VDEnc entrypoint (ffmpeg's `vaapi_encode` defaults `low_power=0` and errors
/// "no usable encoding entrypoint" there; LP additionally needs the HuC firmware, loaded by
/// default on those kernels). AMD keeps its first-try full-feature open byte-for-byte unchanged.
/// The resolved mode is cached per codec; `PUNKTFUNK_VAAPI_LOW_POWER` pins it.
/// Safety contract is [`open_vaapi_encoder_mode`]'s (borrowed `device_ref`/`frames_ref`).
#[allow(clippy::too_many_arguments)]
unsafe fn open_vaapi_encoder(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    ten_bit: bool,
) -> Result<encoder::video::Encoder> {
    let key = lp_key(codec, ten_bit);
    let cached = LP_MODE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map(|m| m.get(&key).copied().unwrap_or(0))
        .unwrap_or(0);
    let modes: &[bool] = entrypoint_ladder(low_power_override(), cached);
    let mut first_err = None;
    for &lp in modes {
        match open_vaapi_encoder_mode(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            device_ref,
            frames_ref,
            ten_bit,
            lp,
        ) {
            Ok(enc) => {
                if let Ok(mut m) = LP_MODE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
                    m.insert(key.clone(), latched_mode(lp));
                }
                if lp {
                    tracing::info!(
                        encoder = codec.vaapi_name(),
                        "VAAPI using the low-power (VDEnc) entrypoint"
                    );
                }
                return Ok(enc);
            }
            Err(e) => {
                tracing::debug!(
                    encoder = codec.vaapi_name(),
                    low_power = lp,
                    "VAAPI encoder open failed: {e:#}"
                );
                first_err.get_or_insert(e);
            }
        }
    }
    // `modes` is never empty, so at least one attempt ran and recorded its error. The first
    // (full-feature) error is the informative one — "no VA display" etc.
    Err(first_err.unwrap())
}

/// Build the FFmpeg encoder context (shared by both inner paths): name, mode, low-latency RC,
/// infinite GOP, the VUI (BT.709 limited SDR, or BT.2020 PQ limited for `ten_bit` HDR),
/// `pix_fmt=VAAPI`, and the given hw device + frames contexts. Returns the opened encoder.
/// `device_ref`/`frames_ref` are borrowed (ref'd into the context).
#[allow(clippy::too_many_arguments)]
unsafe fn open_vaapi_encoder_mode(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    ten_bit: bool,
    low_power: bool,
) -> Result<encoder::video::Encoder> {
    let name = codec.vaapi_name();
    let av_codec = encoder::find_by_name(name).ok_or_else(|| {
        anyhow!("{name} not built into libavcodec (no VAAPI encoder for {codec:?})")
    })?;
    let mut video = codec::context::Context::new_with_codec(av_codec)
        .encoder()
        .video()
        .context("alloc video encoder")?;
    video.set_width(width);
    video.set_height(height);
    // sw view (pix_fmt overridden to VAAPI below): NV12, or P010 for the 10-bit HDR session.
    video.set_format(if ten_bit { Pixel::P010LE } else { Pixel::NV12 });
    // Fixed rate, CBR, no B-frames, ~1-frame VBV — the shared low-latency RC contract.
    apply_low_latency_rc(&mut video, fps, bitrate_bps);
    let raw = video.as_mut_ptr();
    (*raw).gop_size = i32::MAX; // no periodic IDR (forced-IDR via pict_type=I on RFI)
    let vui = vui_for(ten_bit);
    (*raw).colorspace = vui.colorspace;
    (*raw).color_range = vui.range;
    (*raw).color_primaries = vui.primaries;
    (*raw).color_trc = vui.trc;
    (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
    (*raw).hw_device_ctx = ffi::av_buffer_ref(device_ref);
    (*raw).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);

    let mut opts = Dictionary::new();
    if let Some(profile) = explicit_profile(codec, ten_bit) {
        opts.set("profile", profile);
    }
    // async_depth=1: `send_frame` blocks until THIS frame's ASIC encode completes — the lowest
    // latency structure libavcodec's vaapi_encode offers. Measured on the 780M at 1440p60: depth 1
    // = 8.3 ms end-to-end p50 vs depth 2 = 18 ms, because with depth ≥ 2 frame N's packet only
    // materializes once frame N+1 is queued (a structural +1-frame delay no poll can beat). The
    // knob exists for pixel rates beyond the ASIC's serial budget (e.g. 1440p120+ on an iGPU),
    // where depth 2 restores throughput at that one-frame cost. NOTE: the per-frame block tracks
    // GPU CLOCKS — a paced 60 fps trickle lets the VCN downclock (~8 ms/frame vs ~4.4 ms hot);
    // see `gpuclocks` for the session clock pin that removes the ramp tax.
    let depth = async_depth(std::env::var("PUNKTFUNK_VAAPI_ASYNC_DEPTH").ok().as_deref());
    opts.set("async_depth", &depth.to_string());
    if low_power {
        opts.set("low_power", "1"); // VDEnc — the only encode entrypoint on modern Intel
    }
    video.open_with(opts).with_context(|| {
        format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps, low_power={low_power})")
    })
}

/// Probe whether THIS GPU can VAAPI-encode `codec`, by opening a tiny encoder: the driver rejects
/// codecs its video engine can't do (e.g. AV1 on pre-RDNA3 AMD / pre-Arc Intel). Used to build the
/// GameStream codec advertisement so a client never negotiates a codec the GPU can't encode. The
/// device + encoder are torn down immediately (RAII).
pub fn probe_can_encode(codec: Codec) -> bool {
    if ffmpeg::init().is_err() {
        return false;
    }
    // A missing VA device (non-VAAPI host, GPU-less CI) is an expected probe outcome — quiet
    // ffmpeg's "No VA display found" error for the probe. Held until the function returns, so the
    // level is restored after the open either way. Shares one lock with the NVENC probes, which
    // race this one on the same libav global (see [`crate::linux::QuietLibavLog`]).
    let _quiet = crate::linux::QuietLibavLog::new();
    // SAFETY: `ffmpeg::init()` returned Ok above, so libav is initialized. `VaapiHw::new` (an
    // `unsafe fn`) builds a VAAPI device + NV12 frames pool from the literal NV12/640x480/pool=2
    // args and hands back a RAII handle that unrefs both `AVBufferRef`s on drop.
    // `open_vaapi_encoder` (an `unsafe fn`) borrows `hw.device_ref`/`hw.frames_ref` — the two
    // non-null refs `VaapiHw::new` just created — and `av_buffer_ref`s them into the encoder; `hw`
    // is a live local for the whole match arm, so the borrows outlive the synchronous call, and
    // both `hw` and the probe encoder are dropped (RAII) when the arm ends.
    unsafe {
        match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, 640, 480, 2) {
            Ok(hw) => open_vaapi_encoder(
                codec,
                640,
                480,
                30,
                2_000_000,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                false,
            )
            .is_ok(),
            Err(_) => false,
        }
    }
}

/// Probe whether the active VAAPI GPU can encode **10-bit** (HEVC Main10 / 10-bit AV1) from P010
/// surfaces — the exact shape a live HDR session opens (P010 pool + Main10 profile + PQ VUI). The
/// driver rejects what the video engine can't do; the result is cached by the caller
/// ([`crate::can_encode_10bit`]), so a non-Main10 GPU resolves every session to 8-bit SDR before
/// the Welcome (honest downgrade).
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !ten_bit_probe_eligible(codec) {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // A missing VA device / no Main10 entrypoint is an expected probe outcome — quiet ffmpeg's
    // error for the probe. Held until the function returns, so the level is restored after the open
    // either way, and shared with the other probes (see [`crate::linux::QuietLibavLog`]).
    let _quiet = crate::linux::QuietLibavLog::new();
    // SAFETY: `ffmpeg::init()` returned Ok above, so libav is initialized. `VaapiHw::new` (an
    // `unsafe fn`) builds a VAAPI device + P010 frames pool from the literal args and hands back a
    // RAII handle; `open_vaapi_encoder` (an `unsafe fn`) borrows `hw.device_ref`/`hw.frames_ref` —
    // the two non-null refs `VaapiHw::new` just created, live locals for the whole match arm — and
    // `av_buffer_ref`s them into the probe encoder. Both `hw` and the encoder drop (RAII) at arm end.
    unsafe {
        match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_P010LE, 640, 480, 2) {
            Ok(hw) => open_vaapi_encoder(
                codec,
                640,
                480,
                30,
                2_000_000,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                true,
            )
            .is_ok(),
            Err(_) => false,
        }
    }
}

/// Whether the active VAAPI GPU can encode HEVC **4:4:4** (Range Extensions). **Deferred in v1 —
/// always `false`.** VAAPI HEVC 4:4:4 encode is narrow and vendor-specific (the lab's AMD Phoenix1 /
/// RDNA3 exposes only `VAProfileHEVCMain`/`Main10` `EncSlice`, no `Main444`), and there is no
/// validated hardware to build + verify the 4:4:4 surface/profile path against. Returning `false`
/// keeps the negotiation honest: a VAAPI host resolves every session to 4:2:0 before the Welcome, so
/// the client never builds a 4:4:4 decoder it would only get 4:2:0 frames for. (Follow-up: implement
/// and validate on an Intel Arc / RDNA4-class box that advertises a HEVC 4:4:4 encode entrypoint.)
pub fn probe_can_encode_444(_codec: Codec) -> bool {
    tracing::info!("VAAPI HEVC 4:4:4 encode is not implemented yet — declining (encoding 4:2:0)");
    false
}

// ---------------------------------------------------------------------------------------------
// CPU upload path (Phase 1): swscale RGB→NV12 → upload into a pooled VA surface → encode.
// ---------------------------------------------------------------------------------------------

/// VAAPI device + NV12 frames pool (the encoder's input surfaces for the CPU path).
struct VaapiHw {
    // Declared frames-BEFORE-device on purpose: these drop in declaration order, which reproduces
    // exactly what the hand-written `Drop` this replaced did (the frames ctx holds its own
    // reference on the device). Do not reorder these two fields.
    frames_ref: AvBuffer,
    device_ref: AvBuffer,
}

impl VaapiHw {
    unsafe fn new(sw_format: ffi::AVPixelFormat, w: u32, h: u32, pool: c_int) -> Result<Self> {
        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        let node = render_node();
        let r = ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if r < 0 {
            bail!("no VAAPI device ({:?}): {}", node, ffmpeg::Error::from(r));
        }
        // `av_hwdevice_ctx_create` wrote an owned ref into `device_ref`; take ownership of it here so
        // every `bail!` below drops it (and the frames ref, once built) without cleanup of its own.
        let device_ref = AvBuffer::from_raw(device_ref)
            .context("av_hwdevice_ctx_create(VAAPI) gave no device")?;
        let frames_ref = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(device_ref.as_ptr()))
            .context("av_hwframe_ctx_alloc(VAAPI) failed")?;
        let fc = (*frames_ref.as_ptr()).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = sw_format;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = pool;
        let r = ffi::av_hwframe_ctx_init(frames_ref.as_ptr());
        if r < 0 {
            bail!("av_hwframe_ctx_init(VAAPI) failed ({r})");
        }
        Ok(VaapiHw {
            frames_ref,
            device_ref,
        })
    }
}

// No `Drop` for `VaapiHw`: each `AvBuffer` field unrefs itself, in declaration order (frames, then
// device — see the field comment). The hand-written unref pair this replaced had to stay in sync
// with every failure branch in `new`; now there is one unref path and it cannot be skipped.

struct CpuInner {
    enc: encoder::video::Encoder,
    hw: VaapiHw,
    sws: *mut ffi::SwsContext,
    nv12: *mut ffi::AVFrame, // reusable software NV12 staging frame (swscale dst → upload src)
    src_format: PixelFormat,
    width: u32,
    height: u32,
}

impl CpuInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let src_pixel = vaapi_sws_src(format)?;
        // A 10-bit HDR capture (X2RGB10/X2BGR10, PQ/BT.2020) uploads P010 and encodes Main10; the
        // 8-bit paths keep NV12/BT.709 byte-for-byte unchanged.
        let ten_bit = format.is_hdr_rgb10();
        let staging_av = if ten_bit {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        const POOL: c_int = 16;
        // SAFETY: `VaapiHw::new` (an `unsafe fn`) requires libav initialized — guaranteed because the
        // only path here is `VaapiEncoder::open` → `ensure_inner` → `CpuInner::open`, and `open` ran
        // `ffmpeg::init()`. The args are valid: an NV12/P010 sw_format, the validated positive
        // `width`/`height`, pool=16. It returns a RAII `VaapiHw` that unrefs its two `AVBufferRef`s
        // on drop.
        let hw = unsafe { VaapiHw::new(staging_av, width, height, POOL)? };
        // SAFETY: `open_vaapi_encoder` (an `unsafe fn`) borrows `hw.device_ref`/`hw.frames_ref` — both
        // non-null (`VaapiHw::new` guarantees it) and from the `hw` just built above, which is a live
        // local that outlives this synchronous call. The fn `av_buffer_ref`s them into the encoder, so
        // the encoder holds its own references; `hw` is also moved into the returned `CpuInner` next to
        // `enc`, keeping the device/frames alive for the encoder's whole lifetime.
        let enc = unsafe {
            open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                ten_bit,
            )?
        };
        // swscale RGB→NV12 (BT.709 limited) or X2RGB10→P010 (BT.2020 limited, HDR) — matches the
        // VUI; no rescale.
        let src_av = pixel_to_av(src_pixel);
        // SAFETY: `sws_getContext` allocates a swscale context for the given src/dst dimensions and
        // pixel formats. All four dims are the encoder's positive `width`/`height` cast to `c_int`;
        // `src_av` is a valid `AVPixelFormat` (from `pixel_to_av` of the `vaapi_sws_src`-validated
        // `src_pixel`), the dst is NV12/P010. The three trailing pointers (srcFilter, dstFilter,
        // param) are explicitly null = "use defaults", which the API documents as accepted. No Rust
        // memory is borrowed — only by-value ints/enums — and the returned pointer is null-checked
        // just below.
        let sws = unsafe {
            ffi::sws_getContext(
                width as c_int,
                height as c_int,
                src_av,
                width as c_int,
                height as c_int,
                staging_av,
                SWS_POINT,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if sws.is_null() {
            bail!(
                "sws_getContext(RGB→{})",
                if ten_bit { "P010" } else { "NV12" }
            );
        }
        // SAFETY: `sws` is the non-null `SwsContext` from `sws_getContext` above (the `is_null()`
        // check immediately preceding returned false). The coefficient table from
        // `sws_getCoefficients` (ITU-709, or BT.2020 NCL for the HDR path — matching the VUI) is a
        // libswscale static const valid for the whole process, reused here for both the inverse
        // (src) and forward (dst) matrices. `sws_setColorspaceDetails` only reads those tables and
        // writes scalar CSC settings into `sws`; the table pointer outlives the synchronous call and
        // no Rust memory is passed.
        unsafe {
            let cs = ffi::sws_getCoefficients(if ten_bit {
                super::libav::SWS_CS_BT2020
            } else {
                SWS_CS_ITU709
            });
            ffi::sws_setColorspaceDetails(sws, cs, 1, cs, 0, 0, 1 << 16, 1 << 16);
        }
        // SAFETY: `av_frame_alloc` returns a fresh, uniquely-owned heap `AVFrame` (null-checked — on
        // null we free the already-built `sws` and bail). We then write the plain `format`/`width`/
        // `height` fields through the non-null, properly-aligned `f` (sole owner, not yet shared).
        // `av_frame_get_buffer(f, 0)` allocates backing storage for those dims/format; on failure we
        // free `f` and `sws` (unwinding the half-built state) and bail. On success `f` is a fully-owned
        // NV12/P010 frame stored in `CpuInner.nv12` and freed once in `CpuInner::drop`. `f` is a
        // unique fresh pointer, so none of these writes alias anything.
        let nv12 = unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                ffi::sws_freeContext(sws);
                bail!("av_frame_alloc(staging) failed");
            }
            (*f).format = staging_av as c_int;
            (*f).width = width as c_int;
            (*f).height = height as c_int;
            if ffi::av_frame_get_buffer(f, 0) < 0 {
                let mut f = f;
                ffi::av_frame_free(&mut f);
                ffi::sws_freeContext(sws);
                bail!("av_frame_get_buffer(staging) failed");
            }
            f
        };
        tracing::info!(
            encoder = codec.vaapi_name(),
            "VAAPI encode active ({width}x{height}@{fps}, CPU→{} upload path)",
            if ten_bit { "P010 (HDR10)" } else { "NV12" }
        );
        Ok(CpuInner {
            enc,
            hw,
            sws,
            nv12,
            src_format: format,
            width,
            height,
        })
    }

    fn submit(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {format:?} != encoder source {:?}",
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_row = w * self.src_format.bytes_per_pixel();
        anyhow::ensure!(bytes.len() >= src_row * h, "captured buffer too small");
        // SAFETY: The `ensure!`s above guarantee `format == self.src_format` and
        // `bytes.len() >= src_row * h`. `sws_scale` reads `h` rows of `src_row` bytes from
        // `src_data[0] = bytes.as_ptr()` (the other planes null/0 — packed RGB is single-plane), all
        // in bounds; `bytes`, `src_data`, `src_stride` are live locals for this synchronous call.
        // `self.sws` is the non-null context built in `open`; it writes into `self.nv12` (a non-null
        // owned frame whose `data`/`linesize` in-struct arrays were sized by `av_frame_get_buffer`).
        // `av_frame_alloc` (null-checked) yields a fresh `hwf`; `av_hwframe_get_buffer` pulls a pooled
        // VAAPI surface from the live non-null `self.hw.frames_ref`; `av_hwframe_transfer_data` uploads
        // the staged NV12 into it — both frames live, failures free `hwf` and bail. We then write
        // `pts`/`pict_type` through the non-null `hwf` and `avcodec_send_frame` it into the live
        // owned `self.enc` context (which takes its own ref), then free our `hwf` ref exactly once.
        // The encoder runs only on this thread (see `unsafe impl Send`), so no aliasing/data race.
        unsafe {
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.nv12).data.as_ptr(),
                (*self.nv12).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            let mut hwf = ffi::av_frame_alloc();
            if hwf.is_null() {
                bail!("av_frame_alloc(hw) failed");
            }
            if ffi::av_hwframe_get_buffer(self.hw.frames_ref.as_ptr(), hwf, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_get_buffer(VAAPI) failed");
            }
            if ffi::av_hwframe_transfer_data(hwf, self.nv12, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_transfer_data(→VAAPI) failed");
            }
            (*hwf).pts = pts;
            (*hwf).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), hwf);
            ffi::av_frame_free(&mut hwf);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        Ok(())
    }
}

impl Drop for CpuInner {
    fn drop(&mut self) {
        // SAFETY: `self.nv12` (an owned `AVFrame`) and `self.sws` (an owned `SwsContext`) are each
        // freed exactly once here, guarded by `is_null()` so a never-set pointer is skipped (no double
        // free). `CpuInner` owns both exclusively and `Drop` runs once. `av_frame_free` takes `&mut`
        // and nulls the pointer. `self.enc`/`self.hw` are freed afterward by their own `Drop` impls;
        // the encoder holds its own `av_buffer_ref`'d device/frames copies, so field-drop order is
        // irrelevant to soundness.
        unsafe {
            if !self.nv12.is_null() {
                ffi::av_frame_free(&mut self.nv12);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Zero-copy dmabuf path: DRM-PRIME → hwmap(vaapi) → scale_vaapi(nv12) filter graph → encode.
// ---------------------------------------------------------------------------------------------

struct DmabufInner {
    // FIELD ORDER IS LOAD-BEARING. These drop in declaration order, and this order reproduces
    // exactly what the hand-written `Drop` this replaced did: graph, then frames, then the derived
    // VAAPI device, then DRM — and `enc` last of all, since the old `Drop` ran before any field and
    // so freed all four ahead of ffmpeg-next dropping the encoder. Every one of these holds its own
    // reference, so refcounting makes any order sound; the point is that a field reorder must not
    // silently change what ships. Do not move `enc`, and do not reorder the four below it.
    /// The filter graph. Owner-only: `src`/`sink` below are borrowed filter contexts the graph owns,
    /// and everything per-frame goes through those, so nothing reads this field again — it exists so
    /// the graph outlives them and is freed exactly once. (`dead_code` answered here rather than by
    /// removing the field, which would free the graph while `src`/`sink` still point into it.)
    #[allow(dead_code)]
    graph: AvFilterGraph,
    /// DRM-PRIME frames context for the imported dmabufs (buffersrc input). Read per frame by
    /// `submit`, which tags each imported `AVFrame` with a new ref of it.
    drm_frames: AvBuffer,
    /// VAAPI device driving `hwmap`/`scale_vaapi`/the encoder. Owner-only: the two filters and the
    /// encoder each took their own ref at open, so nothing reads it again — it keeps the device
    /// alive behind them.
    #[allow(dead_code)]
    vaapi_device: AvBuffer,
    /// DRM device the source dmabuf frames reference (the buffersrc's `hw_frames_ctx` device).
    /// Owner-only for the same reason: `drm_frames` holds its own ref on it.
    #[allow(dead_code)]
    drm_device: AvBuffer,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    width: u32,
    height: u32,
    fourcc: u32,
    /// Frames submitted — drives the sampled `PUNKTFUNK_PERF` breakdown of the synchronous
    /// submit (import+push vs CSC pull vs encoder send), the stage that dominates AMD/Intel
    /// host latency (7.9 ms p50 at 1440p on the 780M).
    frames: u64,
    /// Declared LAST on purpose — see the field-order note at the top of this struct. The encoder
    /// holds its own device ref and must drop after the graph and the three buffers, which is what
    /// the hand-written `Drop` used to guarantee by running ahead of every field.
    enc: encoder::video::Encoder,
}

impl DmabufInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let drm_fourcc = pf_frame::drm_fourcc(format)
            .ok_or_else(|| anyhow!("no DRM fourcc for {format:?} (VAAPI zero-copy)"))?;
        // A 10-bit HDR capture (X2RGB10/X2BGR10 dmabufs, PQ/BT.2020) maps + CSCs to P010 and
        // encodes Main10; the 8-bit paths keep the NV12/BT.709 graph byte-for-byte unchanged.
        let ten_bit = format.is_hdr_rgb10();
        let sw_format = match format {
            PixelFormat::X2Rgb10 => ffi::AVPixelFormat::AV_PIX_FMT_X2RGB10LE,
            PixelFormat::X2Bgr10 => ffi::AVPixelFormat::AV_PIX_FMT_X2BGR10LE,
            // The 8-bit capture formats are all XR24-shaped packed RGB (the historical BGR0 view).
            _ => ffi::AVPixelFormat::AV_PIX_FMT_BGR0,
        };
        let node = render_node();
        // SAFETY: libav is initialized (`VaapiEncoder::open` ran `ffmpeg::init()` before
        // `ensure_inner` → `DmabufInner::open`). Every raw pointer dereferenced below is either freshly
        // allocated by the immediately-preceding ffmpeg call and null-checked, or an in-struct field of
        // such an object:
        //  * `node` is a `CString` (from `render_node`) live for the whole block; its `.as_ptr()` is a
        //    NUL-terminated path read only during `av_hwdevice_ctx_create`.
        //  * `av_hwdevice_ctx_create(&mut drm_device, DRM, …)` / `…_create_derived(&mut vaapi_device,
        //    VAAPI, drm_device, …)`: on `r < 0` the out-param stays null and we bail (the derive path
        //    unrefs `drm_device` first); on success each is a non-null owned `AVBufferRef`.
        //  * `av_hwframe_ctx_alloc(drm_device)` → `drm_frames` (null-checked); `(*drm_frames).data` is
        //    its `AVHWFramesContext` payload, written before `av_hwframe_ctx_init`.
        //  * `avfilter_graph_alloc` → `graph` (null-checked); `avfilter_get_by_name` returns a static
        //    const `AVFilter` (process-lifetime) or null; `avfilter_graph_alloc_filter` allocates each
        //    filter ctx inside `graph`; the four are null-checked together. `inst`/arg strings are
        //    'static C literals.
        //  * `(*hwmap/scale).hw_device_ctx = av_buffer_ref(vaapi_device)` attaches a NEW ref owned by
        //    the filter (freed by `avfilter_graph_free`); our `vaapi_device` ref is untouched.
        //  * `av_buffersink_get_hw_frames_ctx(sink)` → `nv12_ctx` is a borrowed ref owned by the sink,
        //    valid while `graph` lives (and `graph` is moved into the returned `DmabufInner`).
        //  * `open_vaapi_encoder` borrows `vaapi_device` (our live owned ref) and `nv12_ctx` (sink's
        //    live ref) and `av_buffer_ref`s both into the encoder.
        // Every early-error path unref's the allocated buffers and frees the graph in the right order
        // before bailing; on success the four `AVBufferRef`s + `graph` + `src`/`sink` are moved into
        // `DmabufInner` and freed in its `Drop`. (Two non-UB leaks noted below: `av_buffersrc_*` and
        // the final `?`.)
        unsafe {
            // DRM device (source dmabuf frames) + a VAAPI device derived from it (same GPU) for
            // hwmap/scale_vaapi/the encoder.
            let mut drm_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut drm_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!(
                    "av_hwdevice_ctx_create(DRM {:?}): {}",
                    node,
                    ffmpeg::Error::from(r)
                );
            }
            // Own each handle the moment it exists: from here every `bail!` drops whatever has been
            // built so far, in reverse order, so not one failure branch below carries cleanup code.
            let drm_device = AvBuffer::from_raw(drm_device)
                .context("av_hwdevice_ctx_create(DRM) gave no device")?;
            let mut vaapi_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create_derived(
                &mut vaapi_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                drm_device.as_ptr(),
                0,
            );
            if r < 0 {
                bail!("derive VAAPI from DRM: {}", ffmpeg::Error::from(r));
            }
            let vaapi_device = AvBuffer::from_raw(vaapi_device)
                .context("av_hwdevice_ctx_create_derived(VAAPI) gave no device")?;

            // DRM-PRIME frames context for the imported dmabufs.
            let drm_frames = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(drm_device.as_ptr()))
                .context("av_hwframe_ctx_alloc(DRM) failed")?;
            let fc = (*drm_frames.as_ptr()).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            (*fc).sw_format = sw_format; // packed XR24 RGB plane, or XR30/XB30 for HDR
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            if ffi::av_hwframe_ctx_init(drm_frames.as_ptr()) < 0 {
                bail!("av_hwframe_ctx_init(DRM) failed");
            }

            // Filter graph: buffer(drm_prime) → hwmap=derive_device=vaapi:mode=read →
            // scale_vaapi=format=nv12 → buffersink.
            let graph = AvFilterGraph::alloc().context("avfilter_graph_alloc failed")?;

            let mk = |name: &CStr, inst: &CStr| -> *mut ffi::AVFilterContext {
                let f = ffi::avfilter_get_by_name(name.as_ptr());
                if f.is_null() {
                    return ptr::null_mut();
                }
                ffi::avfilter_graph_alloc_filter(graph.as_ptr(), f, inst.as_ptr())
            };
            let src = mk(c"buffer", c"in");
            let hwmap = mk(c"hwmap", c"map");
            let scale = mk(c"scale_vaapi", c"csc");
            let sink = mk(c"buffersink", c"out");
            if src.is_null() || hwmap.is_null() || scale.is_null() || sink.is_null() {
                bail!("a VAAPI filter (buffer/hwmap/scale_vaapi/buffersink) is missing");
            }
            // hwmap maps the DRM-PRIME input onto THIS vaapi device; scale_vaapi runs the CSC on
            // it. Giving both our device (rather than `hwmap=derive_device`) keeps every surface —
            // and the sink's output frames ctx the encoder adopts — on one VADisplay.
            (*hwmap).hw_device_ctx = ffi::av_buffer_ref(vaapi_device.as_ptr());
            (*scale).hw_device_ctx = ffi::av_buffer_ref(vaapi_device.as_ptr());

            // buffersrc params: DRM-PRIME frames, the drm_frames ctx.
            let par = ffi::av_buffersrc_parameters_alloc();
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*par).width = width as c_int;
            (*par).height = height as c_int;
            // Declare the link's colour up front (full-range RGB — the compositor's desktop) so
            // the per-frame tags in `submit` match the negotiated link instead of reading as a
            // mid-stream property change.
            (*par).color_space = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*par).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: fps as c_int,
            };
            // Assign `drm_frames` BORROWED (no extra ref): `av_buffersrc_parameters_set` takes its
            // own ref of `par->hw_frames_ctx` (via av_buffer_replace), and `av_free(par)` frees only
            // the struct, not the ref. Our single owned `drm_frames` ref is retained, lives in
            // `DmabufInner`, and is unref'd in `Drop`. Wrapping it in `av_buffer_ref` here would leak
            // that extra ref every session (the persistent listener would accumulate them).
            (*par).hw_frames_ctx = drm_frames.as_ptr();
            let r = ffi::av_buffersrc_parameters_set(src, par);
            ffi::av_free(par as *mut _);
            if r < 0 {
                bail!("av_buffersrc_parameters_set failed ({r})");
            }
            macro_rules! init {
                ($ctx:expr, $args:expr, $what:literal) => {{
                    let r = ffi::avfilter_init_str($ctx, $args);
                    if r < 0 {
                        bail!(concat!("init ", $what, " failed ({})"), r);
                    }
                }};
            }
            init!(src, ptr::null(), "buffer");
            init!(hwmap, c"mode=read".as_ptr(), "hwmap");
            init!(scale, scale_vaapi_args(ten_bit).as_ptr(), "scale_vaapi");
            init!(sink, ptr::null(), "buffersink");

            let link = |a: *mut ffi::AVFilterContext, b: *mut ffi::AVFilterContext| -> c_int {
                ffi::avfilter_link(a, 0, b, 0)
            };
            if link(src, hwmap) < 0 || link(hwmap, scale) < 0 || link(scale, sink) < 0 {
                bail!("avfilter_link failed");
            }
            let r = ffi::avfilter_graph_config(graph.as_ptr(), ptr::null_mut());
            if r < 0 {
                bail!("avfilter_graph_config failed ({r})");
            }

            // The encoder takes NV12 surfaces from the sink's output frames context.
            let nv12_ctx = ffi::av_buffersink_get_hw_frames_ctx(sink);
            if nv12_ctx.is_null() {
                bail!("filter sink has no VAAPI frames context");
            }
            // On encoder-open failure, free the graph + our owned buffer refs before bailing (matching
            // every error path above) so a failed session doesn't leak them. `nv12_ctx` is borrowed
            // from the sink (owned by `graph`), so `avfilter_graph_free` reclaims it — don't unref it
            // separately. On success the encoder takes its own ref of `vaapi_device`, and `drm_frames`/
            // `vaapi_device`/`drm_device`/`graph` move into `DmabufInner` (freed in `Drop`).
            let enc = match open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                vaapi_device.as_ptr(),
                nv12_ctx,
                ten_bit,
            ) {
                Ok(enc) => enc,
                Err(e) => {
                    return Err(e);
                }
            };

            tracing::info!(
                encoder = codec.vaapi_name(),
                "VAAPI encode active ({width}x{height}@{fps}, zero-copy dmabuf → GPU {})",
                if ten_bit { "P010 (HDR10)" } else { "NV12" }
            );
            Ok(DmabufInner {
                graph,
                drm_frames,
                vaapi_device,
                drm_device,
                src,
                sink,
                width,
                height,
                fourcc: drm_fourcc,
                frames: 0,
                enc,
            })
        }
    }

    fn submit(&mut self, dmabuf: &DmabufFrame, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            dmabuf.fourcc == self.fourcc,
            "dmabuf fourcc {:#x} != encoder {:#x}",
            dmabuf.fourcc,
            self.fourcc
        );
        // Sampled breakdown of this synchronous submit under PUNKTFUNK_PERF: push = descriptor
        // build + buffersrc (the per-frame DRM→VA import happens inside hwmap on the pull path),
        // pull = buffersink (VPP CSC + any sync), send = avcodec_send_frame. One line per ~2 s.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;
        let t0 = std::time::Instant::now();
        let t_push: std::time::Duration;
        let t_pull: std::time::Duration;
        // SAFETY: The `ensure!` above checked `dmabuf.fourcc == self.fourcc`.
        //  * `std::mem::zeroed::<AVDRMFrameDescriptor>()` is sound: it is a `#[repr(C)]` POD of ints and
        //    nested int-struct arrays (no `NonNull`/refs), for which all-zero is a valid bit pattern;
        //    `Box` puts it on the heap with a unique owner.
        //  * `dmabuf.fd.as_raw_fd()` is the fd of the caller's `&DmabufFrame`, which owns it for the
        //    whole synchronous `submit`; we describe one object/layer/plane from its
        //    fourcc/modifier/offset/stride and its `lseek`-queried size. `libc::lseek` on that live
        //    fd only reads the description's size and returns it (or -1); it touches no Rust memory.
        //  * `av_frame_alloc` → `drm` (null-checked); we set its scalar fields and
        //    `hw_frames_ctx = av_buffer_ref(self.drm_frames)` (new ref of the live owned ctx).
        //  * `data[0] = Box::into_raw(desc)` transfers the box into the frame; `buf[0] =
        //    av_buffer_create(.., free_desc, ..)` registers a destructor that reclaims it exactly once
        //    when the buffer's refcount hits zero — matched alloc/free, no leak/double-free.
        //  * `av_buffersrc_add_frame_flags(self.src, drm, KEEP_REF)` pushes a ref into the live
        //    buffersrc; KEEP_REF keeps our own `drm` ref, which we then `av_frame_free`. We pull the
        //    converted surface with `av_buffersink_get_frame(self.sink, nv12)` BEFORE returning, so the
        //    dmabuf (owned by the caller) is read while still valid. `nv12` is sent into the live owned
        //    `self.enc` (takes its own ref) and our ref freed once. Single-threaded encoder → no race.
        unsafe {
            // Build a DRM-PRIME AVFrame describing the dmabuf (one object/fd, one layer/plane).
            let mut desc: Box<ffi::AVDRMFrameDescriptor> = Box::new(std::mem::zeroed());
            desc.nb_objects = 1;
            desc.objects[0].fd = dmabuf.fd.as_raw_fd();
            // The object's REAL size, not 0. libav does not query it for us — both of its import
            // paths hand this value straight to libva, as `prime_desc.objects[i].size` on the
            // PRIME_2 path and `buffer_desc.data_size` on the legacy fallback — so a 0 told every
            // VA driver the backing object was empty and left it to work the real size out itself.
            // The drivers this has run on (radeonsi, modern Intel iHD) do; a Gen9 Intel host
            // answered `vaCreateSurfaces` with VA_STATUS_ERROR_ALLOCATION_FAILED on every single
            // frame. `lseek(SEEK_END)` is the standard dma-buf size query — the same one the
            // Vulkan bridge already uses on these fds (`pf_zerocopy::imp::vulkan`). If a kernel
            // refuses it, keep the old 0 rather than drop a frame we could still have encoded.
            let obj_size = libc::lseek(dmabuf.fd.as_raw_fd(), 0, libc::SEEK_END);
            desc.objects[0].size = if obj_size > 0 { obj_size as _ } else { 0 };
            desc.objects[0].format_modifier = dmabuf.modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = self.fourcc;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = dmabuf.offset as isize;
            desc.layers[0].planes[0].pitch = dmabuf.stride as isize;

            let mut drm = ffi::av_frame_alloc();
            if drm.is_null() {
                bail!("av_frame_alloc(drm) failed");
            }
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*drm).width = self.width as c_int;
            (*drm).height = self.height as c_int;
            // The dmabuf is the compositor's rendered desktop: full-range RGB. Tag the frame so
            // the VPP's colour negotiation sees the real input instead of "unspecified" (an
            // untagged input lets the driver pick its own default for the RGB→NV12 conversion —
            // Mesa's is BT.601, contradicting the BT.709-limited VUI the encoder signals).
            (*drm).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*drm).colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*drm).hw_frames_ctx = ffi::av_buffer_ref(self.drm_frames.as_ptr());
            (*drm).data[0] = Box::into_raw(desc) as *mut u8;
            // Own the descriptor so it frees with the frame (the fd is owned by the DmabufFrame,
            // which outlives this call — the graph reads the surface before submit returns).
            extern "C" fn free_desc(_opaque: *mut std::ffi::c_void, data: *mut u8) {
                // SAFETY: `data` is exactly the pointer produced by `Box::into_raw(desc)` and passed as
                // `av_buffer_create`'s first arg, which libav hands back verbatim to this callback. It
                // is a valid, uniquely-owned `Box<AVDRMFrameDescriptor>` raw pointer; libav invokes the
                // callback exactly once (when the last buffer ref drops), so `from_raw` + `drop`
                // reclaims it exactly once — no double-free. `_opaque` is unused (we passed null).
                unsafe { drop(Box::from_raw(data as *mut ffi::AVDRMFrameDescriptor)) };
            }
            (*drm).buf[0] = ffi::av_buffer_create(
                (*drm).data[0],
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_desc),
                ptr::null_mut(),
                0,
            );

            // Push through hwmap → scale_vaapi; pull the NV12 surface back out.
            let r = ffi::av_buffersrc_add_frame_flags(
                self.src,
                drm,
                ffi::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
            );
            ffi::av_frame_free(&mut drm);
            // These two stages ARE the import: the push hands libav our DRM-PRIME descriptor, and
            // the pull is where `hwmap` actually maps it into a VA surface (and `scale_vaapi` runs
            // the CSC). A failure here means this driver would not take this compositor's dmabuf —
            // which no encoder rebuild can fix — so tell the process-wide latch, and capture
            // negotiates CPU frames from the next session on. `avcodec_send_frame` below is
            // deliberately NOT counted: that one is the encoder stalling, which the in-place
            // rebuild above us exists to recover, and disabling zero-copy over it would be a
            // permanent penalty for a transient fault.
            if r < 0 {
                let e = format!("av_buffersrc_add_frame failed ({r})");
                pf_zerocopy::note_raw_dmabuf_import_failure(&e);
                bail!("{e}");
            }
            t_push = t0.elapsed();
            let mut nv12 = ffi::av_frame_alloc();
            if nv12.is_null() {
                bail!("av_frame_alloc(nv12) failed");
            }
            let r = ffi::av_buffersink_get_frame(self.sink, nv12);
            if r < 0 {
                ffi::av_frame_free(&mut nv12);
                let e = format!("av_buffersink_get_frame failed ({r})");
                pf_zerocopy::note_raw_dmabuf_import_failure(&e);
                bail!("{e}");
            }
            pf_zerocopy::note_raw_dmabuf_import_ok();
            t_pull = t0.elapsed() - t_push;
            (*nv12).pts = pts;
            (*nv12).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), nv12);
            ffi::av_frame_free(&mut nv12);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        if sample {
            let t_send = t0.elapsed() - t_push - t_pull;
            tracing::info!(
                push_us = t_push.as_micros() as u64,
                pull_us = t_pull.as_micros() as u64,
                send_us = t_send.as_micros() as u64,
                "VAAPI submit split (sampled): push=desc+buffersrc pull=hwmap-import+VPP-CSC \
                 send=avcodec_send_frame"
            );
        }
        Ok(())
    }
}

// No `Drop` for `DmabufInner`: `AvFilterGraph`/`AvBuffer` each free themselves, in field-declaration
// order — graph, frames, derived VAAPI device, DRM device, then `enc` — which is exactly the order
// the hand-written `Drop` this replaced used. That `Drop` had to be kept in step with eight separate
// failure branches in `open`, each repeating the same four-line unwind; there is now one release
// path per handle and no branch can skip it.

// ---------------------------------------------------------------------------------------------

enum Inner {
    Cpu(CpuInner),
    Dmabuf(DmabufInner),
}

pub struct VaapiEncoder {
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    /// Built lazily from the first frame's payload (CPU upload vs zero-copy dmabuf).
    inner: Option<Inner>,
    frame_idx: i64,
    force_kf: bool,
    /// Frames sent to the encoder but not yet returned as packets. Gates [`poll`](Encoder::poll)'s
    /// bounded wait: with `async_depth > 1` a submitted frame's AU lands ~ASIC-time later, so poll
    /// briefly waits for it (same-tick delivery) — but only when something is actually in flight.
    in_flight: u32,
}

// Raw FFI pointers; the encoder lives on a single thread (same contract as `NvencEncoder`).
// SAFETY: `VaapiEncoder`'s `Inner` holds raw FFI pointers (`SwsContext`, `AVFrame`, `AVBufferRef`,
// `AVFilterContext`, `AVCodecContext`) that are not `Send` by default. The encoder is owned and
// driven by exactly ONE thread — the host's per-session encode thread it is moved (transferred) to —
// and is only ever touched through `&mut self` methods, so it is never aliased or accessed
// concurrently from two threads. None of the underlying libav/libswscale objects have thread
// affinity (they are not thread-local), so transferring ownership across threads is sound. This
// asserts `Send` (transfer) only; `Sync` (shared `&`) is deliberately NOT implemented.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: super::ChromaFormat,
    ) -> Result<Self> {
        // 10-bit rides on the captured format: an HDR capture (X2RGB10/X2BGR10) opens the P010 /
        // Main10 / PQ-VUI variant of whichever inner path the first frame selects.
        match resolve_depth(format, bit_depth) {
            DepthResolution::RefuseMislabeledPq => bail!(
                "captured 10-bit HDR frames ({format:?}) on an {bit_depth}-bit VAAPI session — \
                 refusing to mislabel PQ content"
            ),
            DepthResolution::SdrDowngrade => tracing::warn!(
                bit_depth,
                ?format,
                "10-bit requested but the capture stayed SDR — encoding 8-bit"
            ),
            DepthResolution::Agreed => {}
        }
        // VAAPI 4:4:4 is deferred (see `probe_can_encode_444`): no validated AMD/Intel hardware in the
        // lab exposes a HEVC 4:4:4 encode entrypoint, and the probe returns false so the host never
        // negotiates 4:4:4 for a VAAPI session. If a request slips through, fall back to 4:2:0 rather
        // than emit an unverified stream — the host signalled 4:2:0 in the Welcome anyway.
        if chroma.is_444() {
            tracing::warn!("VAAPI 4:4:4 encode not implemented — encoding 4:2:0");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `av_log_set_level` sets libav's global integer log level; `48` (= AV_LOG_DEBUG)
            // is a valid level and there are no pointer args. libav was just initialized by the
            // `ffmpeg::init()` above, so the call is always sound.
            unsafe { ffi::av_log_set_level(48) };
        }
        // Validate the codec/format up front so a bad request fails at open, not on the first frame.
        let _ = vaapi_sws_src(format)?;
        Ok(VaapiEncoder {
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            inner: None,
            frame_idx: 0,
            force_kf: false,
            in_flight: 0,
        })
    }

    fn ensure_inner(&mut self, want_dmabuf: bool) -> Result<&mut Inner> {
        if self.inner.is_none() {
            let inner = if want_dmabuf {
                Inner::Dmabuf(DmabufInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            } else {
                Inner::Cpu(CpuInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            };
            self.inner = Some(inner);
        }
        Ok(self.inner.as_mut().unwrap())
    }
}

impl Encoder for VaapiEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let pts = self.frame_idx;
        self.frame_idx += 1;
        let idr = self.force_kf;
        self.force_kf = false;
        match &captured.payload {
            FramePayload::Cpu(bytes) => match self.ensure_inner(false)? {
                Inner::Cpu(c) => c.submit(bytes, captured.format, pts, idr),
                Inner::Dmabuf(_) => bail!("VAAPI encoder built for dmabuf got a CPU frame"),
            },
            FramePayload::Dmabuf(d) => match self.ensure_inner(true)? {
                Inner::Dmabuf(dm) => dm.submit(d, pts, idr),
                Inner::Cpu(_) => bail!("VAAPI encoder built for CPU got a dmabuf frame"),
            },
            FramePayload::Cuda(_) => bail!(
                "VAAPI encoder received a CUDA frame — that payload is NVENC-only; \
                 unset PUNKTFUNK_ZEROCOPY or don't force PUNKTFUNK_ENCODER=vaapi on an NVIDIA host"
            ),
        }?;
        self.in_flight += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Encode-stall recovery: drop the wedged libavcodec encoder (its `Drop` releases the VA
    /// surfaces/filter graph/devices) and let the next `submit` rebuild it lazily from the first
    /// frame's payload, exactly like first-frame bring-up — the same drop-and-reopen lever the
    /// Windows QSV path has. The owed AUs are forfeited (`in_flight` zeroed) and the rebuilt
    /// encoder's first frame is forced IDR so the client resyncs immediately. Without this the
    /// encode-stall watchdog had no lever on Linux AMD/Intel and a wedged driver ended the session.
    fn reset(&mut self) -> bool {
        self.inner = None;
        self.in_flight = 0;
        self.force_kf = true;
        true
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // With `async_depth > 1`, `submit` no longer waits for the ASIC — the AU for the frame we
        // just sent lands ~one hardware-encode-time later. Wait for it (bounded) so it still ships
        // this tick: the same blocking-retrieve model as NVENC's lock_bitstream, at the ASIC's
        // real per-frame latency instead of send_frame's synchronous ~2× wait. The budget is 3/4
        // of a frame interval (capped 12 ms); on expiry return None — the AU rides the next poll.
        let enc = match &mut self.inner {
            Some(Inner::Cpu(c)) => &mut c.enc,
            Some(Inner::Dmabuf(d)) => &mut d.enc,
            None => return Ok(None),
        };
        let budget = std::time::Duration::from_micros(750_000 / self.fps.max(1) as u64)
            .min(std::time::Duration::from_millis(12));
        let deadline = std::time::Instant::now() + budget;
        loop {
            match poll_encoder(enc, self.fps)? {
                PollOutcome::Packet(au) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    return Ok(Some(au));
                }
                // No AU yet (EAGAIN) or drained (EOF) both fall through to the budget check,
                // exactly as the previous `Option::None` did.
                PollOutcome::Again | PollOutcome::Eof => {}
            }
            // Nothing ready: only wait when a frame is actually in flight (a drained/EOF'd
            // encoder must not spin the budget), and give the ASIC ~250 µs between checks.
            if self.in_flight == 0 || std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
    }

    fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            Some(Inner::Cpu(c)) => c.enc.send_eof().context("send_eof")?,
            Some(Inner::Dmabuf(d)) => d.enc.send_eof().context("send_eof")?,
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct/drop `DmabufInner` repeatedly on real VAAPI silicon.
    ///
    /// This is the heaviest ownership change in the crate and the one with no other coverage.
    /// `open` builds four owned objects — a DRM device, a VAAPI device derived from it, a DRM-PRIME
    /// frames context, and a filter graph — and each of its eight failure branches used to unwind
    /// them by hand, the same four-line block copied eight times (once inside a macro) plus a ninth
    /// copy in `Drop`. All of that is now `AvBuffer`/`AvFilterGraph` field drops in declaration
    /// order, so *construct and drop* is the entire contract. Looping is what separates the
    /// outcomes: a double-free trips glibc, a missed one leaks a VAAPI device, a DRM device and a
    /// whole filter graph per iteration.
    ///
    /// `vaapi_cpu_encode_smoke` does NOT cover this — it drives the swscale/CPU-upload path, which
    /// uses `VaapiHw` and never builds a graph.
    ///
    /// `#[ignore]`d (needs a real VAAPI device):
    /// `cargo test -p pf-encode dmabuf_inner_alloc_drop_cycles -- --ignored --nocapture`
    /// ⚠️ Inside a distrobox on an immutable host, also set
    /// `LIBVA_DRIVERS_PATH=/run/host/usr/lib64/dri` — the container's own mesa is typically older
    /// than the host kernel's amdgpu and fails every encoder open with a bare ENOSYS.
    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn dmabuf_inner_alloc_drop_cycles() {
        for i in 0..8 {
            let inner = DmabufInner::open(Codec::H264, PixelFormat::Bgrx, 640, 480, 30, 8_000_000)
                .unwrap_or_else(|e| panic!("DmabufInner::open failed on iteration {i}: {e:#}"));
            assert!(!inner.graph.as_ptr().is_null(), "graph went null");
            assert!(!inner.drm_frames.as_ptr().is_null(), "drm_frames went null");
            assert!(
                !inner.vaapi_device.as_ptr().is_null(),
                "vaapi_device went null"
            );
            assert!(!inner.drm_device.as_ptr().is_null(), "drm_device went null");
        }
        eprintln!("8 DmabufInner alloc/drop cycles completed without abort");
    }

    /// The operator pin tries exactly one mode; a cached resolution ([`LP_MODE`]) tries only the
    /// mode that worked; anything else runs the full ladder, full-feature FIRST — AMD's first-try
    /// open must stay byte-for-byte unchanged.
    #[test]
    fn entrypoint_ladder_orders_and_pins() {
        assert_eq!(entrypoint_ladder(None, 0), &[false, true]);
        assert_eq!(entrypoint_ladder(None, 1), &[false]);
        assert_eq!(entrypoint_ladder(None, 2), &[true]);
        // A corrupt/unknown cache value degrades to the full ladder, never to a wrong pin.
        assert_eq!(entrypoint_ladder(None, 77), &[false, true]);
        // The pin beats the cache in BOTH directions — what makes PUNKTFUNK_VAAPI_LOW_POWER a
        // real escape hatch from a stale latch.
        for cached in [0u8, 1, 2, 77] {
            assert_eq!(entrypoint_ladder(Some(true), cached), &[true]);
            assert_eq!(entrypoint_ladder(Some(false), cached), &[false]);
        }
    }

    /// The latch round-trip: what a successful open stores makes the NEXT open attempt exactly
    /// the mode that worked (the "skip the known-failing attempt and its libav error spew"
    /// contract — and, per [`LP_MODE`], the shape that must never cross devices or depths).
    #[test]
    fn latch_round_trip_pins_the_resolved_mode() {
        for lp in [false, true] {
            assert_eq!(entrypoint_ladder(None, latched_mode(lp)), &[lp]);
        }
    }

    /// `PUNKTFUNK_VAAPI_LOW_POWER` grammar: the two lowercase literal sets pin; anything else —
    /// including uppercase — means "no pin" (the ladder runs). Case-sensitivity is pinned as
    /// shipped behavior.
    #[test]
    fn low_power_grammar() {
        for s in ["1", "true", "yes", "on", " on ", "yes\n"] {
            assert_eq!(parse_low_power(s), Some(true), "{s:?}");
        }
        for s in ["0", "false", "no", "off", " off "] {
            assert_eq!(parse_low_power(s), Some(false), "{s:?}");
        }
        for s in ["", "2", "TRUE", "On", "enabled", "low_power"] {
            assert_eq!(parse_low_power(s), None, "{s:?}");
        }
    }

    /// Every part of the LP_MODE key is load-bearing (see the [`LP_MODE`] field comments): the
    /// node (a GPU-preference switch must re-resolve — the old codec-only key was a
    /// session-killer), the codec, and the depth (an 8-bit answer must never pin the 10-bit open —
    /// the HDR under-advertisement).
    #[test]
    fn lp_key_separates_node_codec_and_depth() {
        let base = lp_key_for("/dev/dri/renderD128", Codec::H265, false);
        assert_ne!(base, lp_key_for("/dev/dri/renderD129", Codec::H265, false));
        assert_ne!(base, lp_key_for("/dev/dri/renderD128", Codec::H264, false));
        assert_ne!(base, lp_key_for("/dev/dri/renderD128", Codec::H265, true));
    }

    /// `PUNKTFUNK_VAAPI_ASYNC_DEPTH` grammar: 1..=8 verbatim; unset, junk, zero, and past-the-cap
    /// all resolve to the lowest-latency depth 1.
    #[test]
    fn async_depth_grammar() {
        assert_eq!(async_depth(None), 1);
        assert_eq!(async_depth(Some("1")), 1);
        assert_eq!(async_depth(Some("2")), 2);
        assert_eq!(async_depth(Some("8")), 8);
        assert_eq!(async_depth(Some("0")), 1);
        assert_eq!(async_depth(Some("9")), 1);
        assert_eq!(async_depth(Some("-1")), 1);
        assert_eq!(async_depth(Some("fast")), 1);
    }

    /// The swscale source map: packed RGB/BGR and the GNOME 50+ HDR 2:10:10:10 layouts convert;
    /// planar/video formats are refused (the CPU path is a packed-RGB fallback, not a general
    /// converter).
    #[test]
    fn sws_src_accepts_packed_rgb_only() {
        assert_eq!(vaapi_sws_src(PixelFormat::Bgrx).unwrap(), Pixel::BGRZ);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgbx).unwrap(), Pixel::RGBZ);
        assert_eq!(vaapi_sws_src(PixelFormat::Bgra).unwrap(), Pixel::BGRA);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgba).unwrap(), Pixel::RGBA);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgb).unwrap(), Pixel::RGB24);
        assert_eq!(vaapi_sws_src(PixelFormat::Bgr).unwrap(), Pixel::BGR24);
        assert_eq!(
            vaapi_sws_src(PixelFormat::X2Rgb10).unwrap(),
            Pixel::X2RGB10LE
        );
        assert_eq!(
            vaapi_sws_src(PixelFormat::X2Bgr10).unwrap(),
            Pixel::X2BGR10LE
        );
        for f in [
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Rgb10a2,
            PixelFormat::Yuv444,
        ] {
            assert!(vaapi_sws_src(f).is_err(), "{f:?} must be refused");
        }
    }

    /// VUI ↔ CSC agreement: the signaled VUI and the zero-copy VPP's pinned output colour must
    /// name the SAME matrix/range per depth — a mismatch is exactly the Mesa-BT.601 hue shift the
    /// pin exists to prevent.
    #[test]
    fn vui_and_scale_args_agree_per_depth() {
        let sdr = vui_for(false);
        assert!(matches!(sdr.colorspace, ffi::AVColorSpace::AVCOL_SPC_BT709));
        assert!(matches!(sdr.range, ffi::AVColorRange::AVCOL_RANGE_MPEG));
        assert!(matches!(
            sdr.primaries,
            ffi::AVColorPrimaries::AVCOL_PRI_BT709
        ));
        assert!(matches!(
            sdr.trc,
            ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709
        ));
        let args = scale_vaapi_args(false).to_str().unwrap();
        for needle in ["format=nv12", "out_color_matrix=bt709", "out_range=limited"] {
            assert!(
                args.contains(needle),
                "SDR scale args miss {needle}: {args}"
            );
        }

        let hdr = vui_for(true);
        assert!(matches!(
            hdr.colorspace,
            ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL
        ));
        assert!(matches!(hdr.range, ffi::AVColorRange::AVCOL_RANGE_MPEG));
        assert!(matches!(
            hdr.primaries,
            ffi::AVColorPrimaries::AVCOL_PRI_BT2020
        ));
        assert!(matches!(
            hdr.trc,
            ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
        ));
        let args = scale_vaapi_args(true).to_str().unwrap();
        for needle in [
            "format=p010",
            "out_color_matrix=bt2020",
            "out_range=limited",
        ] {
            assert!(
                args.contains(needle),
                "HDR scale args miss {needle}: {args}"
            );
        }
    }

    /// The honest-downgrade table at open: PQ pixels demand a 10-bit session (refused otherwise —
    /// PQ content must never be mislabeled BT.709); a 10-bit session over SDR pixels downgrades
    /// honestly to 8-bit; agreement passes both ways.
    #[test]
    fn depth_resolution_table() {
        use DepthResolution::*;
        assert_eq!(resolve_depth(PixelFormat::X2Rgb10, 8), RefuseMislabeledPq);
        assert_eq!(resolve_depth(PixelFormat::X2Bgr10, 8), RefuseMislabeledPq);
        assert_eq!(resolve_depth(PixelFormat::Bgrx, 10), SdrDowngrade);
        assert_eq!(resolve_depth(PixelFormat::Bgrx, 8), Agreed);
        assert_eq!(resolve_depth(PixelFormat::X2Rgb10, 10), Agreed);
        assert_eq!(resolve_depth(PixelFormat::X2Bgr10, 10), Agreed);
    }

    /// Main10 is pinned explicitly for 10-bit HEVC ONLY: 10-bit AV1 is input-driven (no profile
    /// knob), and every 8-bit open keeps the encoder's default profile.
    #[test]
    fn explicit_profile_is_hevc_main10_only() {
        assert_eq!(explicit_profile(Codec::H265, true), Some("main10"));
        assert_eq!(explicit_profile(Codec::H265, false), None);
        assert_eq!(explicit_profile(Codec::Av1, true), None);
        assert_eq!(explicit_profile(Codec::Av1, false), None);
        assert_eq!(explicit_profile(Codec::H264, true), None);
        assert_eq!(explicit_profile(Codec::H264, false), None);
    }

    /// The 10-bit probe gate: HEVC and AV1 are probe-eligible; H.264 (no 10-bit path here) and
    /// PyroWave (answers 10-bit on its own path, no VAAPI involved) are refused before any device
    /// is touched — this is what keeps the probe safe on GPU-less CI.
    #[test]
    fn ten_bit_probe_gate() {
        assert!(ten_bit_probe_eligible(Codec::H265));
        assert!(ten_bit_probe_eligible(Codec::Av1));
        assert!(!ten_bit_probe_eligible(Codec::H264));
        assert!(!ten_bit_probe_eligible(Codec::PyroWave));
    }

    /// Probe smoke on real silicon: H.264 VAAPI encode opens on every supported AMD/Intel GPU;
    /// the narrower codecs are printed, not asserted (device-dependent). Build anywhere, run on a
    /// VAAPI host:
    ///   cargo test -p pf-encode --no-run
    ///   <host> target/debug/deps/pf_encode-<hash> --ignored --nocapture vaapi_probe_smoke
    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn vaapi_probe_smoke() {
        assert!(
            probe_can_encode(Codec::H264),
            "H.264 VAAPI encode should open on any supported AMD/Intel GPU"
        );
        for codec in [Codec::H265, Codec::Av1] {
            eprintln!("probe_can_encode({codec:?}) = {}", probe_can_encode(codec));
            eprintln!(
                "probe_can_encode_10bit({codec:?}) = {}",
                probe_can_encode_10bit(codec)
            );
        }
    }

    /// CPU-path encode round-trip on real silicon: open → BGRX frames → poll AUs → the first AU
    /// is the IDR. Exercises the swscale CSC, the VA surface upload, and the entrypoint ladder
    /// end-to-end (same recipe as [`vaapi_probe_smoke`]).
    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn vaapi_cpu_encode_smoke() {
        let (w, h) = (256u32, 256u32);
        let mut enc = VaapiEncoder::open(
            Codec::H264,
            PixelFormat::Bgrx,
            w,
            h,
            30,
            2_000_000,
            8,
            crate::ChromaFormat::Yuv420,
        )
        .expect("open");
        let mut aus = Vec::new();
        for i in 0..30u32 {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&[(i * 8) as u8, 0x40, 0xC0, 0xFF]);
            }
            let frame = CapturedFrame {
                width: w,
                height: h,
                pts_ns: u64::from(i) * 33_333_333,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(buf),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert!(!aus.is_empty(), "no AUs out of 30 submitted frames");
        assert!(aus[0].keyframe, "the first AU must be the IDR");
    }
}
