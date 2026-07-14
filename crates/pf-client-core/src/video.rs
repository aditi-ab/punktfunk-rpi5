//! Video decode: reassembled HEVC access units → frames for the presenter.
//!
//! Three backends, picked at session start (auto on Linux: vaapi → vulkan → software on
//! desktop Mesa, vulkan first on NVIDIA/VanGogh — see
//! [`VulkanDecodeDevice::prefer_vulkan_over_vaapi`];
//! override: `PUNKTFUNK_DECODER=vulkan|vaapi|software`):
//!
//! * **Vulkan Video**: FFmpeg's Vulkan decoder running on the PRESENTER's own VkDevice
//!   (its handles arrive via [`VulkanDecodeDevice`]) — the decoded VkImage feeds the
//!   presenter's CSC pass directly, zero copy, every vendor with the video extensions
//!   (NVIDIA's only hardware path; measured 4K@144 with 0.1 ms decode).
//! * **VAAPI** (Intel/AMD fallback): libavcodec hwaccel; each frame is mapped to a
//!   DRM-PRIME dmabuf (`av_hwframe_map`, zero copy) and handed over as fds + plane
//!   layout for the presenter's Vulkan import. NVIDIA has no usable VAAPI
//!   (nvidia-vaapi-driver is broken for this — Moonlight blacklists it); device
//!   creation fails there. A mid-session error falls back — the host's IDR/RFI
//!   recovery resynchronizes.
//! * **Software**: libavcodec on the CPU + swscale to RGBA (staging upload).
//!   Slice threading only — frame threading would add a frame of latency per thread.
//!
//! Both run `AV_CODEC_FLAG_LOW_DELAY`; the host encodes zero-reorder streams (no
//! B-frames, in-band parameter sets on every IDR), so decode is strictly one-in/one-out.
//!
//! On Windows the VAAPI/dmabuf backend does not exist (DRM-PRIME is a Linux concept); the
//! chain there is Vulkan → **D3D11VA** (`crate::video_d3d11` — the vendor-agnostic DXVA
//! path, which is how Intel's Windows driver gets hardware decode without Vulkan Video)
//! → software. Everything dmabuf-shaped is `cfg(target_os = "linux")`-gated inline.

// bindgen's C-enum repr is target-dependent (u32 on Linux/clang, i32 on MSVC), so the
// pf-ffvk Vulkan flag/enum casts below are required on one platform and no-ops on the
// other — the lint would fire on whichever platform the cast is a no-op for.
#![allow(clippy::unnecessary_cast)]

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
use std::ptr;

/// One decoded frame headed for the presenter, carrying the host capture timestamp so the
/// UI can measure capture→displayed latency at the moment it presents.
pub struct DecodedFrame {
    /// Host-clock capture pts (ns) of the AU this image decoded from — compare against
    /// the local wall clock + `clock_offset_ns` at paintable-set time.
    pub pts_ns: u64,
    /// Local wall clock (ns) when the decoder emitted this image — the `decoded`
    /// measurement point (design/stats-unification.md); the presenter subtracts it from
    /// its paintable-set stamp for the client-local `display` stage.
    pub decoded_ns: u64,
    pub image: DecodedImage,
}

/// Re-exported so consumers (the presenter) name every frame type through `video::`.
#[cfg(windows)]
pub use crate::video_d3d11::D3d11Frame;

pub enum DecodedImage {
    Cpu(CpuFrame),
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
    /// FFmpeg Vulkan Video output: a VkImage already on the PRESENTER's device.
    VkFrame(VkVideoFrame),
    /// D3D11VA output copied into a shareable NT-handle texture the presenter imports
    /// (`VK_KHR_external_memory_win32`) — the DXVA path for GPUs without Vulkan Video
    /// (Intel's Windows driver foremost). See `crate::video_d3d11`.
    #[cfg(windows)]
    D3d11(crate::video_d3d11::D3d11Frame),
    /// PyroWave planar output: three R8 plane views on the presenter's own device,
    /// decode already fence-complete, GENERAL layout — the presenter's planar CSC
    /// samples them directly (BT.709 limited, the codec's fixed colour contract).
    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    PyroWave(crate::video_pyrowave::PyroWavePlanarFrame),
}

/// One Vulkan-decoded frame. The image lives on the presenter's own VkDevice (the
/// decoder was built over its handles), so presenting is: plane views → CSC pass — no
/// import, no copy. The live synchronization state (layout / timeline value / owning
/// queue family) is deliberately NOT snapshotted here: FFmpeg updates it per submission,
/// so the presenter reads it through `vkframe` under the frames-context lock at ITS
/// submit time (the `AVVulkanFramesContext.lock_frame` contract).
pub struct VkVideoFrame {
    /// `AVVkFrame*` — img[0] is the (multiplanar) image; sem/sem_value/layout/
    /// queue_family are the live sync state. Valid while `guard` lives.
    pub vkframe: usize,
    /// `AVHWFramesContext*` (FFmpeg's) — the first argument to the lock functions.
    /// Valid while `guard` lives.
    pub frames_ctx: usize,
    /// `AVVulkanFramesContext.lock_frame` / `.unlock_frame` (filled in by FFmpeg's
    /// init): the presenter MUST hold the lock while reading the live sync state and
    /// writing back the incremented semaphore value around its submission.
    pub lock_frame: usize,
    pub unlock_frame: usize,
    /// The frame pool's VkFormat (`AVVulkanFramesContext.format[0]`, raw i32) — the
    /// multiplanar format the presenter builds its per-plane views against.
    pub vk_format: i32,
    /// The frame's timeline semaphore (raw VkSemaphore; creation-constant) and the
    /// value FFmpeg's decode submission signals on completion — the pump waits this
    /// pair AFTER shipping the frame to measure true GPU decode time (zero pipeline
    /// cost: the presenter already waits the same pair on the GPU).
    pub timeline_sem: u64,
    pub decode_done_value: u64,
    pub width: u32,
    pub height: u32,
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I): the stream's re-anchor point. The pump resumes display on
    /// one after suppressing the concealed frames a reference loss leaves in its wake (on
    /// RADV a lost reference decodes to a gray plate with the new motion painted on top).
    pub keyframe: bool,
    /// Keeps the cloned AVFrame (and through it the VkImage + frames context) alive
    /// until the presenter's fence proves the GPU reads done — same mechanism as the
    /// VAAPI path's DRM guard.
    pub guard: DrmFrameGuard,
}

/// The stream's colour signaling, read PER-FRAME from the decoder (HEVC VUI → the
/// `AVFrame` CICP fields). The Windows host switches an HDR desktop to Main10 BT.2020 PQ
/// **in-band** (the Welcome still says SDR — clients are expected to follow the VUI, as
/// the Windows/Apple/Android clients do), so rendering must follow the frames, not the
/// handshake — else PQ content drawn as BT.709 comes out washed out and desaturated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorDesc {
    /// H.273 code points as signaled (2 = unspecified → the renderer picks the SDR default).
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorDesc {
    /// Read the CICP fields off a raw decoded frame. Public: the Windows client's raw-FFI
    /// D3D11VA/software decoders build their per-frame `ColorDesc` with it too (same
    /// `ffmpeg-next` major, so the `AVFrame` type unifies across the workspace).
    ///
    /// # Safety
    /// `frame` must point to a valid `AVFrame` (alive for the duration of the call).
    pub unsafe fn from_raw(frame: *const ffmpeg::ffi::AVFrame) -> ColorDesc {
        // SAFETY: caller guarantees a live AVFrame; these are plain enum field reads.
        unsafe {
            ColorDesc {
                primaries: (*frame).color_primaries as u32 as u8,
                transfer: (*frame).color_trc as u32 as u8,
                matrix: (*frame).colorspace as u32 as u8,
                full_range: (*frame).color_range == ffmpeg::ffi::AVColorRange::AVCOL_RANGE_JPEG,
            }
        }
    }

    /// PQ (SMPTE ST.2084) transfer — the HDR10 signal.
    pub fn is_pq(&self) -> bool {
        self.transfer == 16
    }
}

/// True if the decoder tagged this frame as a full IDR keyframe — a guaranteed clean re-anchor
/// after which the picture is loss-free, so the pump can lift a post-loss display freeze here.
///
/// Keys off `AV_FRAME_FLAG_KEY` (with `pict_type == I` as a belt for decoders that fill pict_type
/// but not the flag). NOTE: FFmpeg's H.264/HEVC decode layer sets this flag **only for true IDR
/// frames**, never for an *intra-refresh recovery point*. H.264 flags key only when a picture's
/// `recovery_frame_cnt == 0` (a moving band uses `> 0`); HEVC clears the flag on every non-IRAP
/// frame regardless of the recovery-point SEI. So an intra-refresh host (NVENC/AMF/QSV) heals the
/// picture over N P-frames with no decoded frame ever flagged key — this function cannot detect
/// that clean point, and the pump would freeze until the `REANCHOR_FREEZE_MAX` backstop (in
/// `session.rs`) forces a real IDR. Detecting an intra-refresh re-anchor requires an out-of-band
/// host wire signal on the AU that completes the wave; that is not yet plumbed.
///
/// # Safety
/// `frame` must point to a valid `AVFrame` alive for the duration of the call.
pub unsafe fn frame_is_keyframe(frame: *const ffmpeg::ffi::AVFrame) -> bool {
    // SAFETY: caller guarantees a live AVFrame; plain field reads.
    unsafe {
        ((*frame).flags & ffmpeg::ffi::AV_FRAME_FLAG_KEY) != 0
            || (*frame).pict_type == ffmpeg::ffi::AVPictureType::AV_PICTURE_TYPE_I
    }
}

impl DecodedImage {
    /// Whether the frame is an intra keyframe — see [`frame_is_keyframe`]. The pump uses
    /// this as the stream's re-anchor signal after a loss.
    pub fn is_keyframe(&self) -> bool {
        match self {
            DecodedImage::Cpu(f) => f.keyframe,
            #[cfg(target_os = "linux")]
            DecodedImage::Dmabuf(f) => f.keyframe,
            DecodedImage::VkFrame(f) => f.keyframe,
            #[cfg(windows)]
            DecodedImage::D3d11(f) => f.keyframe,
            #[cfg(all(target_os = "linux", feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => f.keyframe,
        }
    }

    /// The decoded image's pixel dimensions. The presenter's resize indicator uses these
    /// as the mid-stream-resize END signal: a frame arriving at the target size means the
    /// new-mode picture is on glass (the ack alone lands before the host's rebuild does).
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DecodedImage::Cpu(f) => (f.width, f.height),
            #[cfg(target_os = "linux")]
            DecodedImage::Dmabuf(f) => (f.width, f.height),
            DecodedImage::VkFrame(f) => (f.width, f.height),
            #[cfg(windows)]
            DecodedImage::D3d11(f) => (f.width, f.height),
            #[cfg(all(target_os = "linux", feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
        }
    }
}

/// The Y′CbCr→RGB conversion as three vec4 rows for a shader constant buffer / push-constant
/// block: `rgb[i] = dot(r[i].xyz, yuv) + r[i].w` — bit-depth exact. The ONE coefficient
/// implementation every presenter derives its CSC from (Vulkan push constants, the Windows
/// client's D3D11 constant buffer), so a stream's signaled matrix/range is honored identically
/// everywhere; the Apple client ports this function (and its tests) to Swift.
///
/// `depth` picks the limited-range code points (8-bit: 16/235/240 over 255; 10-bit:
/// 64/940/960 over 1023 — NOT the same normalized values, the difference is ~half a
/// code). `msb_packed` folds in the P010/X6 packing factor: 10 significant bits live in
/// the MSBs of 16, so a UNORM16 sample reads `code·64/65535` — multiplying by
/// `65535/65472` recovers exact `code/1023`.
pub fn csc_rows(desc: ColorDesc, depth: u8, msb_packed: bool) -> [[f32; 4]; 3] {
    // BT.601 (5/6), BT.2020 (9/10); everything else — incl. unspecified — is the host's
    // BT.709 SDR default (mirrors the software path's swscale coefficient choice).
    let (kr, kb) = match desc.matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let max = f64::from((1u32 << depth) - 1); // 255 / 1023
    let step = f64::from(1u32 << (depth - 8)); // code points per 8-bit step: 1 / 4
    let pack = if msb_packed { 65535.0 / 65472.0 } else { 1.0 };
    let (sy, oy, sc) = if desc.full_range {
        (pack, 0.0f64, pack)
    } else {
        (
            pack * max / (219.0 * step),
            -(16.0 * step) / max,
            pack * max / (224.0 * step),
        )
    };
    // rgb = M * (yuv + off) = M*yuv + M*off — rows of M with the offset dot folded into
    // w. `yuv` is the SAMPLED (packed) value, so the offsets divide by the packing
    // factor to land on the same scale.
    let off = [oy / pack, -0.5 / pack, -0.5 / pack];
    let m = [
        [sy, 0.0, 2.0 * (1.0 - kr) * sc],
        [
            sy,
            -2.0 * (1.0 - kb) * kb / kg * sc,
            -2.0 * (1.0 - kr) * kr / kg * sc,
        ],
        [sy, 2.0 * (1.0 - kb) * sc, 0.0],
    ];
    core::array::from_fn(|r| {
        let w: f64 = (0..3).map(|c| m[r][c] * off[c]).sum();
        [m[r][0] as f32, m[r][1] as f32, m[r][2] as f32, w as f32]
    })
}

/// RGBA pixels for `GdkMemoryTexture` (which takes a stride).
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub rgba: Vec<u8>,
    /// Signaling of the source frame. swscale already undid the YUV matrix + range (the
    /// pixels are full-range RGB), but a PQ/BT.2020 stream keeps its transfer + primaries
    /// baked in — the presenter tags the texture so GTK tone-maps it.
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor signal. See [`VkVideoFrame`].
    pub keyframe: bool,
}

/// A decoded frame still on the GPU: dmabuf fds + plane layout for
/// `GdkDmabufTextureBuilder`. The fds belong to `guard`'s mapped DRM frame — they stay
/// valid until the guard drops (the texture's release func).
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    /// Combined DRM fourcc of the whole surface (NV12 for 8-bit VAAPI output), derived
    /// from the decoder's software format — NOT the per-plane component formats.
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Signaling of the source frame — drives the `GdkDmabufTexture` color state (BT.709
    /// narrow for SDR, BT.2020 PQ for an HDR stream).
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor signal. See [`VkVideoFrame`].
    pub keyframe: bool,
    pub guard: DrmFrameGuard,
}

#[cfg(target_os = "linux")]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Owns the mapped DRM-PRIME `AVFrame` (which in turn references the VAAPI surface).
/// Dropping it releases the surface back to the decoder pool and closes the fds.
pub struct DrmFrameGuard(*mut ffmpeg::ffi::AVFrame);
// An AVFrame is plain refcounted data; freeing it from the GTK main thread is fine.
unsafe impl Send for DrmFrameGuard {}

impl Drop for DrmFrameGuard {
    fn drop(&mut self) {
        unsafe { ffmpeg::ffi::av_frame_free(&mut self.0) };
    }
}

enum Backend {
    Vulkan(VulkanDecoder),
    #[cfg(target_os = "linux")]
    Vaapi(VaapiDecoder),
    #[cfg(windows)]
    D3d11va(crate::video_d3d11::D3d11vaDecoder),
    /// PyroWave (wired-LAN wavelet codec): pyrowave compute on the presenter's device,
    /// no FFmpeg involvement. No demotion rung — there is no other decoder for it.
    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    PyroWave(crate::video_pyrowave::PyroWaveDecoder),
    Software(SoftwareDecoder),
}

pub struct Decoder {
    backend: Backend,
    /// The negotiated codec (from the host's Welcome), so a mid-session VAAPI→software demotion
    /// rebuilds the software decoder for the SAME codec.
    codec_id: ffmpeg::codec::Id,
    /// Consecutive hardware decode errors (Vulkan or VAAPI) — a single transient failure
    /// (e.g. a reference-missing frame after packet loss) shouldn't cost the whole
    /// session its hardware decoder.
    vaapi_fails: u32,
    /// Set when the decoder needs a fresh IDR to resynchronize (after an error or a demotion).
    /// The pump drains it and asks the host — under the infinite GOP there is no periodic
    /// keyframe, so a rebuilt/erroring decoder would otherwise stay gray/frozen forever.
    want_keyframe: bool,
}

/// Demote VAAPI→software only after this many consecutive hardware decode errors; a lone
/// transient error just re-requests an IDR and keeps the hardware decoder.
const VAAPI_DEMOTE_AFTER: u32 = 3;

/// Map a negotiated `quic` codec bit to the FFmpeg decoder id the client opens.
pub fn ffmpeg_codec_id(wire: u8) -> ffmpeg::codec::Id {
    match wire {
        punktfunk_core::quic::CODEC_H264 => ffmpeg::codec::Id::H264,
        punktfunk_core::quic::CODEC_AV1 => ffmpeg::codec::Id::AV1,
        _ => ffmpeg::codec::Id::HEVC,
    }
}

/// The `quic` codec bitfield this client can decode — whatever FFmpeg has a decoder for (HEVC/H.264
/// always; AV1 when built in). Advertised to the host so it never emits a codec we can't decode.
pub fn decodable_codecs() -> u8 {
    let _ = ffmpeg::init();
    let mut bits = 0u8;
    for (id, bit) in [
        (ffmpeg::codec::Id::HEVC, punktfunk_core::quic::CODEC_HEVC),
        (ffmpeg::codec::Id::H264, punktfunk_core::quic::CODEC_H264),
        (ffmpeg::codec::Id::AV1, punktfunk_core::quic::CODEC_AV1),
    ] {
        if ffmpeg::decoder::find(id).is_some() {
            bits |= bit;
        }
    }
    bits
}

/// [`decodable_codecs`] plus the PyroWave bit when the presenter's device passed the
/// compute-feature probe. Advertisement-only: `resolve_codec` never auto-picks PyroWave —
/// the session must also name it `preferred_codec` (plan §3), which the client does only
/// under its explicit opt-in.
pub fn decodable_codecs_for(vk: Option<&VulkanDecodeDevice>) -> u8 {
    let bits = decodable_codecs();
    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    if vk.map(|v| v.pyrowave_decode).unwrap_or(false) {
        return bits | punktfunk_core::quic::CODEC_PYROWAVE;
    }
    #[cfg(not(all(target_os = "linux", feature = "pyrowave")))]
    let _ = vk;
    bits
}

/// libavcodec logs reference-frame recovery to the process stderr very verbosely
/// (`First slice in a frame missing`, `Could not find ref with POC …`, `Error
/// constructing the frame RPS`) — normal chatter while the decoder waits for a keyframe
/// after loss, but a raw flood in the user's terminal (it bypasses our tracing). Default
/// it to fatal-only; `PUNKTFUNK_FFMPEG_LOG=<quiet|error|warning|info|debug>` restores it
/// for decode debugging. Process-global; set once per decoder build (idempotent).
fn quiet_ffmpeg_log() {
    use ffmpeg::util::log::Level;
    let level = match std::env::var("PUNKTFUNK_FFMPEG_LOG").ok().as_deref() {
        Some("quiet") => Level::Quiet,
        Some("error") => Level::Error,
        Some("warning") => Level::Warning,
        Some("info") => Level::Info,
        Some("debug" | "trace") => Level::Debug,
        _ => Level::Fatal,
    };
    ffmpeg::util::log::set_level(level);
}

impl Decoder {
    /// `codec_id` is the codec the host resolved in the Welcome (never assume HEVC).
    /// `pref` is the Settings "Video decoder" value (`auto`/`vulkan`/`vaapi`/`d3d11va`/
    /// `software`; `hardware` — the WinUI shell's stored value — reads as auto).
    /// `vk` is the presenter's shared Vulkan device when its stack can run FFmpeg's
    /// Vulkan Video decoder — decode lands as VkImages the presenter samples directly.
    /// Precedence: the `PUNKTFUNK_DECODER` env override wins (support/debug escape
    /// hatch, and the documented knob), then the setting; both default to auto.
    /// Auto's hardware order on Linux depends on the device
    /// ([`VulkanDecodeDevice::prefer_vulkan_over_vaapi`]): VAAPI → Vulkan → software on
    /// desktop Mesa (AMD/Intel), Vulkan → VAAPI → software on NVIDIA and the Deck's
    /// VanGogh. Windows is Vulkan → D3D11VA → software (no VAAPI there).
    pub fn new(
        codec_id: ffmpeg::codec::Id,
        pref: &str,
        vk: Option<&VulkanDecodeDevice>,
    ) -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        quiet_ffmpeg_log();
        let choice = std::env::var("PUNKTFUNK_DECODER")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| pref.to_string());
        let done = |backend| {
            Ok(Decoder {
                backend,
                codec_id,
                vaapi_fails: 0,
                want_keyframe: false,
            })
        };
        // Linux `auto`: try VAAPI FIRST unless this device is one where Vulkan Video is
        // the established right answer (NVIDIA — no usable VAAPI; VanGogh — VAAPI
        // chroma-fringes). Mesa now exposes decode queues by default (and the session
        // binary opts RADV in for the Deck's sake), which silently moved every desktop
        // AMD/Intel box onto FFmpeg-Vulkan-on-Mesa — user-reported to judder/error-streak
        // (then demote to software) where explicit VAAPI streams perfectly.
        #[cfg(target_os = "linux")]
        let mut vaapi_tried = false;
        #[cfg(target_os = "linux")]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_over_vaapi())
        {
            vaapi_tried = true;
            match VaapiDecoder::new(codec_id) {
                Ok(v) => {
                    tracing::info!(?codec_id, "VAAPI hardware decode active (zero-copy dmabuf)");
                    return done(Backend::Vaapi(v));
                }
                Err(e) => {
                    tracing::info!(reason = %e, "VAAPI unavailable — trying Vulkan Video");
                }
            }
        }
        if matches!(choice.as_str(), "auto" | "" | "vulkan" | "hardware") {
            // `video_decode` gates the Vulkan Video attempt: the presenter now exports its
            // handle bundle even when the device has no decode queue (Windows D3D11 interop
            // rides the same struct), so presence alone no longer implies a usable decoder.
            match vk.filter(|v| v.video_decode) {
                Some(vk) => match VulkanDecoder::new(codec_id, vk) {
                    Ok(v) => {
                        tracing::info!(
                            ?codec_id,
                            "Vulkan Video hardware decode active (presenter-shared device)"
                        );
                        return done(Backend::Vulkan(v));
                    }
                    Err(e) => {
                        if choice == "vulkan" {
                            return Err(e.context("PUNKTFUNK_DECODER=vulkan but it failed"));
                        }
                        tracing::info!(reason = %format!("{e:#}"),
                            "Vulkan Video unavailable — falling back");
                    }
                },
                None if choice == "vulkan" => {
                    bail!(
                        "PUNKTFUNK_DECODER=vulkan but the presenter's device can't (missing \
                           video extensions/queue) — see the presenter log"
                    )
                }
                None => {}
            }
        }
        // Deck/NVIDIA note: `auto` reaches VAAPI here when Vulkan Video isn't available
        // (on desktop Mesa it was already tried above — `vaapi_tried` skips the repeat).
        // A presenter that can't display the dmabufs demotes this decoder to software
        // mid-session via [`Decoder::force_software`]. Windows has no VAAPI — auto falls
        // straight through to software there.
        #[cfg(target_os = "linux")]
        if choice != "software" && choice != "vulkan" && !vaapi_tried {
            match VaapiDecoder::new(codec_id) {
                Ok(v) => {
                    tracing::info!(?codec_id, "VAAPI hardware decode active (zero-copy dmabuf)");
                    return done(Backend::Vaapi(v));
                }
                Err(e) => {
                    if choice == "vaapi" {
                        return Err(e.context("PUNKTFUNK_DECODER=vaapi but VAAPI failed"));
                    }
                    tracing::info!(reason = %e, "VAAPI unavailable — software decode");
                }
            }
        }
        // Windows: D3D11VA is the vendor-agnostic DXVA fallback when Vulkan Video isn't
        // available (Intel's Windows driver foremost) — gated on the presenter having the
        // win32 external-memory import path, else its frames could never reach the screen.
        #[cfg(windows)]
        if choice != "software" && choice != "vulkan" {
            match vk.filter(|v| v.d3d11_import) {
                Some(v) => {
                    match crate::video_d3d11::D3d11vaDecoder::new(codec_id, v.adapter_luid) {
                        Ok(d) => {
                            tracing::info!(
                                ?codec_id,
                                "D3D11VA hardware decode active (shared-texture hand-off)"
                            );
                            return done(Backend::D3d11va(d));
                        }
                        Err(e) => {
                            if choice == "d3d11va" {
                                return Err(e.context("PUNKTFUNK_DECODER=d3d11va but it failed"));
                            }
                            tracing::info!(reason = %format!("{e:#}"),
                                "D3D11VA unavailable — software decode");
                        }
                    }
                }
                None if choice == "d3d11va" => bail!(
                    "PUNKTFUNK_DECODER=d3d11va but the presenter's device lacks the win32 \
                     external-memory import extensions — see the presenter log"
                ),
                None => {}
            }
        }
        if choice == "software" {
            // Say WHY hardware wasn't even attempted — a stored "software" preference
            // (or the env override) silently skipping vulkan/vaapi has burned real
            // debugging time on boxes that could do better.
            tracing::info!(
                "software decode by preference (Settings decoder / PUNKTFUNK_DECODER) — \
                 hardware decode not attempted"
            );
        }
        done(Backend::Software(SoftwareDecoder::new(codec_id)?))
    }

    /// Wait for a Vulkan-Video frame's GPU decode to complete (timeline semaphore) —
    /// the pump's decode-stat measurement. `false` = not the Vulkan backend, or timeout.
    pub fn wait_hw_decoded(&self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        match &self.backend {
            Backend::Vulkan(v) => v.wait_timeline(timeline_sem, value, timeout_ns),
            _ => false,
        }
    }

    /// Drain the "please ask the host for an IDR" flag — the pump calls this each iteration
    /// (throttled) so a demoted/erroring decoder can resynchronize under the infinite GOP.
    /// Open a PyroWave decoder for a `CODEC_PYROWAVE` session (plan §4.5): pyrowave
    /// compute on the presenter's device, no FFmpeg. `codec_id` is irrelevant (kept as
    /// HEVC so an — impossible — demotion path stays well-formed).
    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    pub fn new_pyrowave(vk: &VulkanDecodeDevice, width: u32, height: u32) -> Result<Decoder> {
        Ok(Decoder {
            backend: Backend::PyroWave(crate::video_pyrowave::PyroWaveDecoder::new(
                vk, width, height,
            )?),
            codec_id: ffmpeg::codec::Id::HEVC,
            vaapi_fails: 0,
            want_keyframe: false,
        })
    }

    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Demote to software decode on the PRESENTER's verdict (dmabuf presentation impossible:
    /// GL converter init failed, texture import rejected). Decode itself succeeds in that
    /// state, so the error-streak demotion never fires — without this the stream would stay
    /// black forever. No-op when already software.
    pub fn force_software(&mut self) -> Result<()> {
        if matches!(self.backend, Backend::Software(_)) {
            return Ok(());
        }
        tracing::warn!("presenter can't display hardware frames — demoting to software decode");
        self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
        self.vaapi_fails = 0;
        self.want_keyframe = true;
        Ok(())
    }

    /// Feed one access unit; returns the decoded frame (the host's streams are
    /// one-in/one-out). A software decode error after packet loss is survivable — log
    /// upstream and keep feeding. A VAAPI error re-requests an IDR and retries the hardware
    /// decoder; only a persistent streak of failures (a genuinely broken driver, e.g.
    /// nvidia-vaapi-driver) demotes to software. Either way `want_keyframe` is set so the
    /// pump asks the host for a fresh IDR — under the infinite GOP nothing else resyncs a
    /// rebuilt/erroring decoder, so skipping this leaves the picture gray/frozen for good.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        let result = match &mut self.backend {
            Backend::Vulkan(v) => v.decode(au).map(|f| f.map(DecodedImage::VkFrame)),
            #[cfg(target_os = "linux")]
            Backend::Vaapi(v) => v.decode(au).map(|f| f.map(DecodedImage::Dmabuf)),
            #[cfg(windows)]
            Backend::D3d11va(d) => d.decode(au).map(|f| f.map(DecodedImage::D3d11)),
            // No demote ladder below PyroWave (nothing else decodes it): propagate the
            // error; the pump surfaces it and the session falls back to HEVC by
            // renegotiation (plan §4.6), not by decoder swap.
            #[cfg(all(target_os = "linux", feature = "pyrowave"))]
            Backend::PyroWave(p) => return Ok(p.decode(au)?.map(DecodedImage::PyroWave)),
            Backend::Software(s) => return Ok(s.decode(au)?.map(DecodedImage::Cpu)),
        };
        match result {
            Ok(f) => {
                self.vaapi_fails = 0;
                Ok(f)
            }
            Err(e) => {
                let which = match self.backend {
                    Backend::Vulkan(_) => "Vulkan Video",
                    #[cfg(windows)]
                    Backend::D3d11va(_) => "D3D11VA",
                    _ => "VAAPI",
                };
                self.vaapi_fails += 1;
                self.want_keyframe = true;
                if self.vaapi_fails >= VAAPI_DEMOTE_AFTER {
                    // A failing Vulkan backend still has a hardware rung below it on
                    // Linux — demote to VAAPI first (user-reported: FFmpeg-Vulkan-on-Mesa
                    // error-streaking where VAAPI streams perfectly); only when that
                    // can't be built either does the session land on software.
                    #[cfg(target_os = "linux")]
                    if matches!(self.backend, Backend::Vulkan(_)) {
                        match VaapiDecoder::new(self.codec_id) {
                            Ok(v) => {
                                tracing::warn!(error = %e, fails = self.vaapi_fails,
                                    "Vulkan Video decode failing repeatedly — demoting to VAAPI");
                                self.backend = Backend::Vaapi(v);
                                self.vaapi_fails = 0;
                                return Ok(None);
                            }
                            Err(va) => tracing::info!(reason = %va,
                                "VAAPI unavailable for demotion — software decode"),
                        }
                    }
                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                        "{which} decode failing repeatedly — demoting to software");
                    self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
                    self.vaapi_fails = 0;
                } else {
                    tracing::warn!(error = %e,
                        "{which} decode error — requesting keyframe, keeping hardware decode");
                }
                Ok(None)
            }
        }
    }
}

// --- software backend ---------------------------------------------------------------

struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size — or the colour signaling (a mid-stream
    /// SDR↔HDR flip) — changes.
    sws: Option<(scaling::Context, Pixel, u32, u32, ColorDesc)>,
}

impl SoftwareDecoder {
    fn new(codec_id: ffmpeg::codec::Id) -> Result<SoftwareDecoder> {
        let codec = ffmpeg::decoder::find(codec_id)
            .ok_or_else(|| anyhow!("no {codec_id:?} decoder in libavcodec"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open video decoder")?;
        Ok(SoftwareDecoder { decoder, sws: None })
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<CpuFrame>> {
        let packet = ffmpeg::Packet::copy(au);
        self.decoder
            .send_packet(&packet)
            .map_err(|e| anyhow!("send_packet: {e}"))?;
        let mut frame = AvFrame::empty();
        let mut out = None;
        while self.decoder.receive_frame(&mut frame).is_ok() {
            out = Some(self.convert_rgba(&frame)?);
        }
        Ok(out)
    }

    fn convert_rgba(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        // SAFETY: `frame.as_ptr()` is the decoder-owned live AVFrame for this call.
        let color = unsafe { ColorDesc::from_raw(frame.as_ptr()) };
        let rebuild = !matches!(&self.sws,
            Some((_, f, sw, sh, c)) if *f == fmt && *sw == w && *sh == h && *c == color);
        if rebuild {
            let mut ctx =
                scaling::Context::get(fmt, w, h, Pixel::RGBA, w, h, scaling::Flags::POINT)
                    .context("swscale context")?;
            // swscale defaults to BT.601 coefficients — set them from the FRAME's signaling
            // (unspecified → BT.709 limited, the host's SDR default; a Windows HDR desktop
            // streams BT.2020 in-band). Without this, YUV→RGB decodes with the wrong matrix
            // and colours shift. Destination = full-range RGB; the transfer function stays
            // baked in (the presenter tags PQ textures so GTK applies the EOTF).
            const SWS_CS_ITU709: i32 = 1;
            const SWS_CS_ITU601: i32 = 5;
            const SWS_CS_BT2020: i32 = 9;
            let cs = match color.matrix {
                9 | 10 => SWS_CS_BT2020,
                5 | 6 => SWS_CS_ITU601,
                _ => SWS_CS_ITU709,
            };
            unsafe {
                let coeffs = ffmpeg::ffi::sws_getCoefficients(cs);
                ffmpeg::ffi::sws_setColorspaceDetails(
                    ctx.as_mut_ptr(),
                    coeffs, // inv_table: source (YUV) coefficients per the VUI
                    color.full_range as i32, // srcRange: 0 = limited/studio (MPEG)
                    coeffs, // table: destination coefficients (ignored for RGB output)
                    1,      // dstRange: 1 = full-range RGB
                    0,
                    1 << 16,
                    1 << 16, // brightness, contrast, saturation (defaults)
                );
            }
            self.sws = Some((ctx, fmt, w, h, color));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        // Single-pass conversion: swscale writes straight into the Vec the texture will
        // wrap. (The old path scaled into a scratch AVFrame and then copied `data(0)` out
        // — a second full-frame pass per frame.) 64-byte row alignment keeps swscale on
        // aligned SIMD stores; `GdkMemoryTexture` takes the resulting stride explicitly.
        const ALIGN: i32 = 64;
        use ffmpeg::ffi;
        let dst_fmt = ffi::AVPixelFormat::AV_PIX_FMT_RGBA;
        // SAFETY: pure size computation from format/dimensions; no pointers involved.
        let size = unsafe { ffi::av_image_get_buffer_size(dst_fmt, w as i32, h as i32, ALIGN) };
        if size < 0 {
            return Err(averr("av_image_get_buffer_size", size));
        }
        let rgba = vec![0u8; size as usize];
        let mut dst_data: [*mut u8; 4] = [ptr::null_mut(); 4];
        let mut dst_linesize: [i32; 4] = [0; 4];
        // SAFETY: fill_arrays only derives plane pointers/strides into `rgba` (sized by
        // av_image_get_buffer_size above, same format/align) — no allocation, no
        // ownership transfer; `rgba` outlives the scale below.
        let r = unsafe {
            ffi::av_image_fill_arrays(
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
                rgba.as_ptr(),
                dst_fmt,
                w as i32,
                h as i32,
                ALIGN,
            )
        };
        if r < 0 {
            return Err(averr("av_image_fill_arrays", r));
        }
        // SAFETY: src pointers/strides belong to the decoder-owned `frame` (alive for the
        // call); dst pointers were just filled over `rgba`, and sws_scale writes rows
        // [0, h) only — exactly the buffer fill_arrays sized.
        let r = unsafe {
            ffi::sws_scale(
                sws.as_mut_ptr(),
                (*frame.as_ptr()).data.as_ptr() as *const *const u8,
                (*frame.as_ptr()).linesize.as_ptr(),
                0,
                h as i32,
                dst_data.as_ptr(),
                dst_linesize.as_ptr(),
            )
        };
        if r < 0 {
            return Err(averr("sws_scale", r));
        }
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: dst_linesize[0] as usize,
            rgba,
            color,
            // `is_key()` reads the same intra flag `frame_is_keyframe` derives from pict_type
            // for the hardware paths; ffmpeg-next handles the FFmpeg-version binding split.
            keyframe: frame.is_key(),
        })
    }
}

// --- VAAPI backend --------------------------------------------------------------------
//
// Raw FFI: ffmpeg-next has no hwaccel wrappers. All pointers are owned here and freed in
// Drop; decoded surfaces transfer out through DrmFrameGuard.

// -EAGAIN. FFmpeg uses POSIX errno values on both our targets (MinGW's EAGAIN is 11 too).
const AVERROR_EAGAIN: i32 = -11;

fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
}

/// libavcodec offers the formats it can decode into; pick the VAAPI hw surface. Falling
/// back to the first (software) entry would silently decode on the CPU *and* break our
/// dmabuf mapping — return NONE instead so the error surfaces and the session demotes
/// to the software backend explicitly.
#[cfg(target_os = "linux")]
unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    unsafe {
        while *list != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            list = list.add(1);
        }
    }
    ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

#[cfg(target_os = "linux")]
struct VaapiDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    hw_device: *mut ffmpeg::ffi::AVBufferRef,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
}

// Single-owner pointers, only touched from the session pump thread.
#[cfg(target_os = "linux")]
unsafe impl Send for VaapiDecoder {}

#[cfg(target_os = "linux")]
impl VaapiDecoder {
    fn new(codec_id: ffmpeg::codec::Id) -> Result<VaapiDecoder> {
        use ffmpeg::ffi;
        unsafe {
            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!("no VAAPI device ({})", ffmpeg::Error::from(r));
            }
            // The negotiated codec's decoder id (av_codec_id maps 1:1 from ffmpeg::codec::Id).
            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(pick_vaapi);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // hwaccel: threads only add latency

            // The presenter holds mapped surfaces PAST receive_frame (the paintable's
            // current texture + the newest frame in flight each pin one until GDK's
            // release func) — surfaces libavcodec doesn't know are missing from its
            // fixed-size VAAPI pool. Without headroom the decoder can recycle a surface
            // the renderer is still sampling (intermittent block corruption) or fail
            // allocation under scheduling jitter.
            (*ctx).extra_hw_frames = 4;
            let r = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                ffi::avcodec_free_context(&mut ctx);
                let mut hw_device = hw_device;
                ffi::av_buffer_unref(&mut hw_device);
                bail!("avcodec_open2: {}", ffmpeg::Error::from(r));
            }
            Ok(VaapiDecoder {
                ctx,
                hw_device,
                packet: ffi::av_packet_alloc(),
                frame: ffi::av_frame_alloc(),
            })
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        use ffmpeg::ffi;
        unsafe {
            let r = ffi::av_new_packet(self.packet, au.len() as i32);
            if r < 0 {
                return Err(averr("av_new_packet", r));
            }
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
            let r = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if r < 0 {
                return Err(averr("send_packet", r));
            }
            let mut out = None;
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    return Err(averr("receive_frame", r));
                }
                out = Some(self.map_dmabuf()?); // newest wins; older guards drop here
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// Map the VAAPI surface to DRM PRIME (zero copy) and lift the descriptor into a
    /// `DmabufFrame`. The mapped frame keeps the surface alive via its buffer refs.
    ///
    /// FFmpeg's VAAPI export uses `VA_EXPORT_SURFACE_SEPARATE_LAYERS`, so an NV12 surface
    /// comes back as TWO layers (`R8` luma + `GR88` chroma), each one plane — NOT a single
    /// `NV12` layer. The previous code took `layers[0]` only: GTK then saw an `R8`
    /// single-plane texture with the chroma dropped, painting the screen green. The fix:
    /// derive the COMBINED fourcc from the decoder's software pixel format (NV12 →
    /// `DRM_FORMAT_NV12`) and flatten every plane across every layer in order (Y then UV).
    unsafe fn map_dmabuf(&mut self) -> Result<DmabufFrame> {
        use ffmpeg::ffi;
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                bail!("decoder returned a software frame (no VAAPI surface)");
            }
            // The real pixel layout lives on the hardware frames context, not the
            // DRM-PRIME layer formats (those are the per-plane R8/GR88 component formats).
            let sw_format = {
                let hwfc = (*self.frame).hw_frames_ctx;
                if hwfc.is_null() {
                    bail!("VAAPI frame without a hardware frames context");
                }
                (*((*hwfc).data as *const ffi::AVHWFramesContext)).sw_format
            };
            let fourcc = drm_fourcc_for(sw_format)
                .ok_or_else(|| anyhow!("unsupported VAAPI output format {sw_format:?}"))?;

            let drm = ffi::av_frame_alloc();
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = ffi::av_hwframe_map(drm, self.frame, ffi::AV_HWFRAME_MAP_READ as i32);
            if r < 0 {
                let mut drm = drm;
                ffi::av_frame_free(&mut drm);
                return Err(averr("av_hwframe_map", r));
            }
            let desc = (*drm).data[0] as *const ffi::AVDRMFrameDescriptor;
            let guard = DrmFrameGuard(drm);
            let d = &*desc;
            if d.nb_layers < 1 || d.nb_objects < 1 {
                bail!("DRM descriptor without layers/objects");
            }

            // Flatten planes across ALL layers, in declared order — the combined fourcc's
            // plane order (Y, then UV for NV12) matches the layer order FFmpeg emits.
            let mut planes = Vec::new();
            for layer in &d.layers[..d.nb_layers as usize] {
                for p in &layer.planes[..layer.nb_planes as usize] {
                    let obj = &d.objects[p.object_index as usize];
                    planes.push(DmabufPlane {
                        fd: obj.fd,
                        offset: p.offset as u32,
                        stride: p.pitch as u32,
                    });
                }
            }

            // The whole surface shares one tiling modifier (one BO on radeonsi); GTK takes
            // a single modifier for the texture.
            let modifier = d.objects[0].format_modifier;

            log_descriptor_once(d, sw_format, fourcc, modifier);

            Ok(DmabufFrame {
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                fourcc,
                modifier,
                planes,
                // SAFETY: `self.frame` is the live decoded AVFrame (unref'd only after
                // this returns); plain CICP field reads.
                color: ColorDesc::from_raw(self.frame),
                keyframe: frame_is_keyframe(self.frame),
                guard,
            })
        }
    }
}

/// Guard-less mutex serializing every `vkQueueSubmit`/`vkQueuePresentKHR`/
/// `vkQueueWaitIdle` on the device the presenter shares with FFmpeg.
///
/// Why it exists: the presenter created the device with ONE graphics-family queue and
/// told FFmpeg's `AVVulkanDeviceContext` to use that same family (`nb_graphics_queues
/// = 1` ⇒ queue index 0) for its transfer/compute prep work — so the presenter thread
/// and the session pump thread were submitting to the SAME `VkQueue` with no shared
/// lock. `vkQueueSubmit` requires external synchronization on the queue; the race
/// surfaced as intermittent `VK_ERROR_DEVICE_LOST` at exactly the moments FFmpeg puts
/// work on the graphics queue (decoder open / frames-context rebuild — i.e. stream
/// start and every adaptive-bitrate encoder rebuild; live-diagnosed 2026-07-09).
///
/// FFmpeg's hook for this is the `lock_queue`/`unlock_queue` callback pair on
/// `AVVulkanDeviceContext` — a raw lock/unlock shape with no RAII scope, hence this
/// guard-less primitive (`std::sync::Mutex`'s guard can't cross the C callbacks).
/// Contention is a handful of µs-scale critical sections per frame; a plain
/// Mutex+Condvar is more than enough.
pub struct QueueLock {
    locked: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl QueueLock {
    #[allow(clippy::new_without_default)]
    pub fn new() -> QueueLock {
        QueueLock {
            locked: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Block until the queue is free, then take it. Pair with [`QueueLock::unlock`]
    /// (FFmpeg's callbacks), or use [`QueueLock::guard`] from Rust callers.
    pub fn lock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *g {
            g = self
                .cv
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *g = true;
    }

    pub fn unlock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = false;
        drop(g);
        self.cv.notify_one();
    }

    /// RAII form for Rust call sites (presenter submits/presents, Skia flushes).
    pub fn guard(&self) -> QueueLockGuard<'_> {
        self.lock();
        QueueLockGuard(self)
    }
}

/// Releases the [`QueueLock`] on drop.
pub struct QueueLockGuard<'a>(&'a QueueLock);

impl Drop for QueueLockGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}

/// The presenter's Vulkan device handles, exported so FFmpeg's Vulkan Video decoder
/// runs on the SAME device the presenter samples from — the whole point: the decoded
/// VkImage is composited directly, no interop, no copy (plan: Vulkan Video phase).
///
/// Plain integers/strings on purpose: pf-client-core has no ash dependency; pf-ffvk
/// casts these into vulkan.h handle types when filling `AVVulkanDeviceContext`. All
/// handles stay valid for the presenter's lifetime, which outlives every session pump
/// (the run loop tears the pump down before the presenter).
#[derive(Clone)]
pub struct VulkanDecodeDevice {
    /// `PFN_vkGetInstanceProcAddr` from the loader — FFmpeg resolves everything else.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    /// PCI vendor of the presenter's physical device (0x10DE NVIDIA, 0x1002 AMD,
    /// 0x8086 Intel) — drives [`Self::prefer_vulkan_over_vaapi`].
    pub vendor_id: u32,
    /// The driver's device-name string (e.g. "AMD RADV VANGOGH") — the VanGogh/Deck
    /// detection for [`Self::prefer_vulkan_over_vaapi`].
    pub device_name: String,
    /// The presenter's graphics+present family (FFmpeg's "required" tx/comp family too).
    pub graphics_qf: u32,
    /// Raw `VkQueueFlags` of that family (the qf[] entry wants the real capabilities).
    pub graphics_queue_flags: u32,
    /// The video-decode family (may equal `graphics_qf` on some hardware).
    pub decode_qf: u32,
    /// Raw `VkVideoCodecOperationFlagsKHR` the decode family advertises.
    pub decode_video_caps: u32,
    /// Everything enabled at instance/device creation — FFmpeg keys code paths off the
    /// extension STRINGS, so the lists must match reality exactly.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<std::ffi::CString>,
    /// Features enabled at device creation (reported via `device_features`).
    pub f_sampler_ycbcr: bool,
    pub f_timeline_semaphore: bool,
    pub f_synchronization2: bool,
    /// Vulkan Video decode is actually usable on this device (decode queue + extensions +
    /// features). The bundle now exists even without it — Windows D3D11 interop rides the
    /// same struct — so consumers gate the FFmpeg-Vulkan decoder on THIS, not on `Some`.
    pub video_decode: bool,
    /// PyroWave decode (the wired-LAN wavelet codec) is usable: Vulkan 1.3 + the compute
    /// features its kernels need were present AND enabled at device creation
    /// (`shaderInt16`, `storageBuffer8BitAccess`, subgroup size control). Gates the
    /// `CODEC_PYROWAVE` advertisement and the pyrowave decoder backend.
    pub pyrowave_decode: bool,
    /// The feature facts + creation shape the pyrowave decoder's pinned create-info
    /// reconstruction mirrors (pyrowave 0.4.0 requires the instance/device create infos —
    /// content-accurate, kept alive — to share our VkDevice).
    pub f_shader_int16: bool,
    pub f_storage_buffer8: bool,
    pub f_subgroup_size_control: bool,
    pub f_compute_full_subgroups: bool,
    pub f_shader_float16: bool,
    /// `VkPhysicalDeviceProperties::apiVersion` of the presenter's device.
    pub api_version: u32,
    /// The queue families the device was created with (one `VkDeviceQueueCreateInfo` each,
    /// one queue per family, priority 1.0) — mirrored by the reconstruction.
    pub queue_families: Vec<u32>,
    /// The presenter enabled `VK_KHR_external_memory_win32` + `VK_KHR_win32_keyed_mutex`:
    /// D3D11 shared-texture frames can reach the screen. Always `false` off Windows.
    pub d3d11_import: bool,
    /// `VkPhysicalDeviceIDProperties::deviceLUID` when the driver reports one — the D3D11VA
    /// backend creates its decode device on the SAME adapter so shared textures never cross
    /// GPUs. `None` when not reported (or off Windows, where it's unused).
    pub adapter_luid: Option<[u8; 8]>,
    /// The device's shared queue lock (see [`QueueLock`]). The presenter holds it around
    /// its own submits/presents; the decoder wires it into FFmpeg's
    /// `lock_queue`/`unlock_queue` callbacks so both sides serialize on the same queues.
    pub queue_lock: std::sync::Arc<QueueLock>,
}

impl VulkanDecodeDevice {
    /// Should `auto` try Vulkan Video BEFORE VAAPI on this device?
    ///  * **NVIDIA** — Vulkan is its only hardware path (no usable VAAPI; the
    ///    nvidia-vaapi-driver is broken for this, Moonlight blacklists it).
    ///  * **AMD (RADV, VanGogh included)** — Vulkan decode outperforms VAAPI on RADV
    ///    (on-glass verdict), and on VanGogh VAAPI's separate-plane dmabuf import
    ///    additionally shows chroma fringing; the session binary opts RADV into
    ///    `video_decode` precisely to get the Vulkan path. Vulkan-first is safe here
    ///    because a mid-session Vulkan failure streak demotes to VAAPI (not software),
    ///    so a broken Mesa Vulkan path still lands on the working driver.
    ///
    /// Intel (ANV) and unknown vendors keep the battle-tested zero-copy VAAPI first —
    /// ANV's Vulkan Video is the least-proven Mesa path and VAAPI is what every other
    /// Linux client uses there.
    pub fn prefer_vulkan_over_vaapi(&self) -> bool {
        const VENDOR_NVIDIA: u32 = 0x10DE;
        const VENDOR_AMD: u32 = 0x1002;
        self.vendor_id == VENDOR_NVIDIA || self.vendor_id == VENDOR_AMD
    }
}

/// `fourcc(a,b,c,d)` — the DRM FourCC packing (little-endian, `a | b<<8 | c<<16 | d<<24`).
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// The combined DRM FourCC for a decoder software pixel format. The host streams 8-bit
/// 4:2:0 (NV12); P010 is here for the eventual 10-bit/HDR path.
// Only the (Linux-gated) VAAPI path calls this outside tests; the constants are worth
// locking on every platform, so it stays compiled rather than cfg-gated with its caller.
#[cfg_attr(windows, allow(dead_code))]
fn drm_fourcc_for(sw: ffmpeg_next::ffi::AVPixelFormat) -> Option<u32> {
    use ffmpeg_next::ffi::AVPixelFormat::*;
    Some(match sw {
        AV_PIX_FMT_NV12 => fourcc(b'N', b'V', b'1', b'2'),
        AV_PIX_FMT_P010LE => fourcc(b'P', b'0', b'1', b'0'),
        _ => return None,
    })
}

/// One-time dump of the DRM descriptor layout (objects, layers, planes, modifier) — so a
/// new client/driver combination's real layout is visible in the logs without a debugger.
#[cfg(target_os = "linux")]
fn log_descriptor_once(
    d: &ffmpeg_next::ffi::AVDRMFrameDescriptor,
    sw: ffmpeg_next::ffi::AVPixelFormat,
    fourcc: u32,
    modifier: u64,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if !ONCE.swap(false, Ordering::Relaxed) {
        return;
    }
    let layers: Vec<(u32, i32)> = d.layers[..d.nb_layers.max(0) as usize]
        .iter()
        .map(|l| (l.format, l.nb_planes))
        .collect();
    tracing::info!(
        sw_format = ?sw,
        chosen_fourcc = format_args!("{:#010x}", fourcc),
        nb_objects = d.nb_objects,
        nb_layers = d.nb_layers,
        ?layers,
        modifier = format_args!("{:#018x}", modifier),
        "VAAPI dmabuf descriptor layout (first frame)"
    );
}

#[cfg(target_os = "linux")]
impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

// --- Vulkan Video backend -------------------------------------------------------------

/// FFmpeg's Vulkan Video decoder over the PRESENTER's device: the hwdevice context is
/// built from [`VulkanDecodeDevice`]'s handles (not `av_hwdevice_ctx_create`, which
/// would make FFmpeg create its own device the presenter can't sample from). Output
/// frames are `AVVkFrame`s whose VkImage the presenter feeds straight to its CSC pass.
struct VulkanDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    hw_device: *mut ffmpeg::ffi::AVBufferRef,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
    /// `vkWaitSemaphores` on the shared device — the decode-complete measurement
    /// (resolved through the same get_proc_addr chain FFmpeg uses).
    wait_semaphores: pf_ffvk::PFN_vkWaitSemaphores,
    vk_device: pf_ffvk::VkDevice,
    /// Storage `AVVulkanDeviceContext` points into (extension string arrays + the
    /// feature chain) — FFmpeg reads the extension lists past init (frames-context
    /// setup keys code paths off them), so this lives exactly as long as `hw_device`.
    _ctx_storage: Box<VkCtxStorage>,
}

// Single-owner pointers, only touched from the session pump thread.
unsafe impl Send for VulkanDecoder {}

struct VkCtxStorage {
    _inst: Vec<std::ffi::CString>,
    inst_ptrs: Vec<*const std::os::raw::c_char>,
    _dev: Vec<std::ffi::CString>,
    dev_ptrs: Vec<*const std::os::raw::c_char>,
    f11: pf_ffvk::VkPhysicalDeviceVulkan11Features,
    f12: pf_ffvk::VkPhysicalDeviceVulkan12Features,
    f13: pf_ffvk::VkPhysicalDeviceVulkan13Features,
    /// Keeps the shared queue lock alive for `AVHWDeviceContext.user_opaque` — the
    /// `lock_queue`/`unlock_queue` trampolines below dereference it for as long as the
    /// hw device context can fire them.
    _queue_lock: std::sync::Arc<QueueLock>,
}

/// FFmpeg `AVVulkanDeviceContext.lock_queue` trampoline: take the device's shared
/// [`QueueLock`] (stashed in `AVHWDeviceContext.user_opaque`; owned by
/// [`VkCtxStorage`], which outlives the context). Replaces FFmpeg's internal default,
/// which only serializes FFmpeg against itself — the presenter submits to the same
/// graphics queue from another thread and holds this same lock around its calls.
unsafe extern "C" fn ffvk_lock_queue(
    ctx: *mut pf_ffvk::AVHWDeviceContext,
    _queue_family: u32,
    _index: u32,
) {
    let dev = ctx as *mut ffmpeg::ffi::AVHWDeviceContext;
    let lock = (*dev).user_opaque as *const QueueLock;
    (*lock).lock();
}

/// The matching `unlock_queue` trampoline — see [`ffvk_lock_queue`].
unsafe extern "C" fn ffvk_unlock_queue(
    ctx: *mut pf_ffvk::AVHWDeviceContext,
    _queue_family: u32,
    _index: u32,
) {
    let dev = ctx as *mut ffmpeg::ffi::AVHWDeviceContext;
    let lock = (*dev).user_opaque as *const QueueLock;
    (*lock).unlock();
}

impl VulkanDecoder {
    fn new(codec_id: ffmpeg::codec::Id, vk: &VulkanDecodeDevice) -> Result<VulkanDecoder> {
        use ffmpeg::ffi;
        unsafe {
            let mut hw_device =
                ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN);
            if hw_device.is_null() {
                bail!("av_hwdevice_ctx_alloc(VULKAN) failed (FFmpeg built without Vulkan?)");
            }
            let devctx = (*hw_device).data as *mut ffi::AVHWDeviceContext;
            let hwctx = (*devctx).hwctx as *mut pf_ffvk::AVVulkanDeviceContext;

            // Pinned storage for everything the context points into.
            let mut store = Box::new(VkCtxStorage {
                _inst: vk.instance_extensions.clone(),
                inst_ptrs: Vec::new(),
                _dev: vk.device_extensions.clone(),
                dev_ptrs: Vec::new(),
                f11: std::mem::zeroed(),
                f12: std::mem::zeroed(),
                f13: std::mem::zeroed(),
                _queue_lock: vk.queue_lock.clone(),
            });
            store.inst_ptrs = store._inst.iter().map(|c| c.as_ptr()).collect();
            store.dev_ptrs = store._dev.iter().map(|c| c.as_ptr()).collect();
            // The features enabled at device creation, as the 1.1/1.2/1.3 chain FFmpeg
            // walks to learn what it may use (sType values are vulkan.h constants).
            store.f11.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES;
            store.f11.samplerYcbcrConversion = vk.f_sampler_ycbcr as u32;
            store.f12.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
            store.f12.timelineSemaphore = vk.f_timeline_semaphore as u32;
            store.f13.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES;
            store.f13.synchronization2 = vk.f_synchronization2 as u32;
            store.f11.pNext = &mut store.f12 as *mut _ as *mut std::ffi::c_void;
            store.f12.pNext = &mut store.f13 as *mut _ as *mut std::ffi::c_void;

            (*hwctx).get_proc_addr = std::mem::transmute::<usize, pf_ffvk::PFN_vkGetInstanceProcAddr>(
                vk.get_instance_proc_addr,
            );
            (*hwctx).inst = vk.instance as pf_ffvk::VkInstance;
            (*hwctx).phys_dev = vk.physical_device as pf_ffvk::VkPhysicalDevice;
            (*hwctx).act_dev = vk.device as pf_ffvk::VkDevice;
            (*hwctx).device_features.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
            (*hwctx).device_features.pNext = &mut store.f11 as *mut _ as *mut std::ffi::c_void;
            (*hwctx).enabled_inst_extensions = store.inst_ptrs.as_ptr();
            (*hwctx).nb_enabled_inst_extensions = store.inst_ptrs.len() as i32;
            (*hwctx).enabled_dev_extensions = store.dev_ptrs.as_ptr();
            (*hwctx).nb_enabled_dev_extensions = store.dev_ptrs.len() as i32;

            // Queue map: the deprecated per-role indices (tx/comp are "Required") plus
            // the qf[] list, which per the header must also carry every family named
            // above. One merged entry when decode shares the graphics family.
            let g = vk.graphics_qf as i32;
            let d = vk.decode_qf as i32;
            (*hwctx).queue_family_index = g;
            (*hwctx).nb_graphics_queues = 1;
            (*hwctx).queue_family_tx_index = g;
            (*hwctx).nb_tx_queues = 1;
            (*hwctx).queue_family_comp_index = g;
            (*hwctx).nb_comp_queues = 1;
            (*hwctx).queue_family_encode_index = -1;
            (*hwctx).nb_encode_queues = 0;
            (*hwctx).queue_family_decode_index = d;
            (*hwctx).nb_decode_queues = 1;
            const VIDEO_DECODE_BIT: u32 = 0x20; // VK_QUEUE_VIDEO_DECODE_BIT_KHR
                                                // `flags`/`video_caps` are bindgen enum types: i32 under MSVC, u32 under
                                                // Linux clang — the `as _` casts absorb the difference.
            if g == d {
                (*hwctx).qf[0] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: g,
                    num: 1,
                    flags: (vk.graphics_queue_flags | VIDEO_DECODE_BIT) as _,
                    video_caps: vk.decode_video_caps as _,
                };
                (*hwctx).nb_qf = 1;
            } else {
                (*hwctx).qf[0] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: g,
                    num: 1,
                    flags: vk.graphics_queue_flags as _,
                    video_caps: 0,
                };
                (*hwctx).qf[1] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: d,
                    num: 1,
                    flags: VIDEO_DECODE_BIT as _,
                    video_caps: vk.decode_video_caps as _,
                };
                (*hwctx).nb_qf = 2;
            }

            // Shared-queue external sync (see [`QueueLock`]): FFmpeg must take the
            // same lock the presenter holds around its own submits/presents — set
            // BEFORE init so FFmpeg never installs its internal defaults (which only
            // serialize FFmpeg against itself; the cross-thread race with the
            // presenter's queue was an intermittent VK_ERROR_DEVICE_LOST).
            (*devctx).user_opaque =
                std::sync::Arc::as_ptr(&store._queue_lock) as *mut std::ffi::c_void;
            (*hwctx).lock_queue = Some(ffvk_lock_queue);
            (*hwctx).unlock_queue = Some(ffvk_unlock_queue);

            let r = ffi::av_hwdevice_ctx_init(hw_device);
            if r < 0 {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(averr("av_hwdevice_ctx_init(VULKAN)", r));
            }

            // vkWaitSemaphores for the pump's decode-complete stat: loader →
            // vkGetDeviceProcAddr → device fn (core 1.2, guaranteed by our gate).
            let gipa = (*hwctx)
                .get_proc_addr
                .expect("get_proc_addr was just set above");
            let gdpa: pf_ffvk::PFN_vkGetDeviceProcAddr =
                std::mem::transmute(gipa((*hwctx).inst, c"vkGetDeviceProcAddr".as_ptr()));
            let wait_semaphores: pf_ffvk::PFN_vkWaitSemaphores = std::mem::transmute(gdpa
                .expect("vkGetDeviceProcAddr resolvable")(
                (*hwctx).act_dev,
                c"vkWaitSemaphores".as_ptr(),
            ));
            if wait_semaphores.is_none() {
                ffi::av_buffer_unref(&mut hw_device);
                bail!("vkWaitSemaphores unresolvable on this device");
            }
            let vk_device = (*hwctx).act_dev;

            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(pick_vulkan);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // hwaccel: threads only add latency
                                     // Same pool headroom rationale as VAAPI: the presenter pins the on-screen
                                     // frame + the newest in flight past receive_frame.
            (*ctx).extra_hw_frames = 4;
            let r = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                ffi::avcodec_free_context(&mut ctx);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(averr("avcodec_open2 (vulkan)", r));
            }
            Ok(VulkanDecoder {
                ctx,
                hw_device,
                packet: ffi::av_packet_alloc(),
                frame: ffi::av_frame_alloc(),
                wait_semaphores,
                vk_device,
                _ctx_storage: store,
            })
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<VkVideoFrame>> {
        use ffmpeg::ffi;
        unsafe {
            let r = ffi::av_new_packet(self.packet, au.len() as i32);
            if r < 0 {
                return Err(averr("av_new_packet", r));
            }
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
            let r = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if r < 0 {
                return Err(averr("send_packet", r));
            }
            let mut out = None;
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    return Err(averr("receive_frame", r));
                }
                out = Some(self.extract()?); // newest wins; older guards drop here
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// Block until the timeline semaphore reaches `value` (GPU decode complete) or the
    /// timeout passes. Pure measurement — the presenter's own GPU wait is what gates
    /// sampling, so a timeout here only degrades the stat, never the picture.
    fn wait_timeline(&self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        let sems = [sem as pf_ffvk::VkSemaphore];
        let values = [value];
        let info = pf_ffvk::VkSemaphoreWaitInfo {
            sType: pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
            pNext: std::ptr::null(),
            flags: 0,
            semaphoreCount: 1,
            pSemaphores: sems.as_ptr(),
            pValues: values.as_ptr(),
        };
        // SAFETY: resolved from this device at init; handles outlive the decoder.
        let r = unsafe {
            self.wait_semaphores.expect("checked at init")(self.vk_device, &info, timeout_ns)
        };
        r == 0 // VK_SUCCESS (VK_TIMEOUT = 2)
    }

    /// Lift the decoded `AVVkFrame` into a [`VkVideoFrame`]: clone the AVFrame (the
    /// guard — keeps the image + frames context alive through present) and ship the
    /// POINTERS; the presenter reads the live sync state under the frames-context lock
    /// at its own submit time.
    unsafe fn extract(&mut self) -> Result<VkVideoFrame> {
        use ffmpeg::ffi;
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32 {
                bail!("decoder returned a non-Vulkan frame");
            }
            let hwfc_ref = (*self.frame).hw_frames_ctx;
            if hwfc_ref.is_null() {
                bail!("Vulkan frame without a hardware frames context");
            }
            let fc = (*hwfc_ref).data as *mut ffi::AVHWFramesContext;
            let sw = (*fc).sw_format;
            if sw != ffi::AVPixelFormat::AV_PIX_FMT_NV12
                && sw != ffi::AVPixelFormat::AV_PIX_FMT_P010LE
            {
                bail!("Vulkan decode output {sw:?} unsupported (NV12/P010 only)");
            }
            let vkfc = (*fc).hwctx as *const pf_ffvk::AVVulkanFramesContext;
            let vk_format = (*vkfc).format[0] as i32;
            let lock_frame = (*vkfc).lock_frame.map_or(0, |f| f as usize);
            let unlock_frame = (*vkfc).unlock_frame.map_or(0, |f| f as usize);
            if lock_frame == 0 || unlock_frame == 0 {
                bail!("Vulkan frames context without lock functions");
            }

            let clone = ffi::av_frame_clone(self.frame);
            if clone.is_null() {
                bail!("av_frame_clone failed");
            }
            let vkf = (*clone).data[0] as *mut pf_ffvk::AVVkFrame;
            // v1 handles the (default) single multiplanar image; a disjoint/multi-image
            // pool would need per-plane images — bail so the session demotes cleanly.
            if !(*vkf).img[1].is_null() {
                let mut clone = clone;
                ffi::av_frame_free(&mut clone);
                bail!("multi-image Vulkan frames unsupported (disjoint pool)");
            }
            // Safe without the frames lock: the handle is creation-constant and
            // sem_value was last written by the decode submission on THIS thread.
            let timeline_sem = (*vkf).sem[0] as u64;
            let decode_done_value = (*vkf).sem_value[0];
            Ok(VkVideoFrame {
                vkframe: vkf as usize,
                frames_ctx: fc as usize,
                lock_frame,
                unlock_frame,
                vk_format,
                timeline_sem,
                decode_done_value,
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                color: ColorDesc::from_raw(self.frame),
                keyframe: frame_is_keyframe(self.frame),
                guard: DrmFrameGuard(clone),
            })
        }
    }
}

impl Drop for VulkanDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

/// libavcodec offers the formats it can decode into; pick the Vulkan hw surface and
/// hand the decoder OUR frames context — the default one lacks
/// `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT`, without which the presenter can't create the
/// per-plane views its CSC pass samples. Returning NONE (over the software entry) keeps
/// failures loud: the session demotes explicitly instead of silently CPU-decoding.
unsafe extern "C" fn pick_vulkan(
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi;
    unsafe {
        let mut offered = false;
        while *list != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN {
                offered = true;
                break;
            }
            list = list.add(1);
        }
        if !offered {
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let mut fr: *mut ffi::AVBufferRef = ptr::null_mut();
        let r = ffi::avcodec_get_hw_frames_parameters(
            ctx,
            (*ctx).hw_device_ctx,
            ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
            &mut fr,
        );
        if r < 0 || fr.is_null() {
            tracing::warn!("avcodec_get_hw_frames_parameters(VULKAN) failed ({r})");
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let fc = (*fr).data as *mut ffi::AVHWFramesContext;
        let vkfc = (*fc).hwctx as *mut pf_ffvk::AVVulkanFramesContext;
        // MUTABLE_FORMAT: per-plane views (spec requirement); ALIAS is FFmpeg's default.
        // (`as _`: the FlagBits constants are i32 under MSVC, the img_flags field u32.)
        (*vkfc).img_flags = (pf_ffvk::VkImageCreateFlagBits_VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT
            | pf_ffvk::VkImageCreateFlagBits_VK_IMAGE_CREATE_ALIAS_BIT)
            as _;
        let r = ffi::av_hwframe_ctx_init(fr);
        if r < 0 {
            tracing::warn!("av_hwframe_ctx_init(VULKAN) failed ({r})");
            let mut fr = fr;
            ffi::av_buffer_unref(&mut fr);
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        if !(*ctx).hw_frames_ctx.is_null() {
            ffi::av_buffer_unref(&mut (*ctx).hw_frames_ctx);
        }
        (*ctx).hw_frames_ctx = fr; // the codec owns our ref now
        ffi::AVPixelFormat::AV_PIX_FMT_VULKAN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_device(vendor_id: u32, device_name: &str) -> VulkanDecodeDevice {
        VulkanDecodeDevice {
            get_instance_proc_addr: 0,
            instance: 0,
            physical_device: 0,
            device: 0,
            vendor_id,
            device_name: device_name.into(),
            graphics_qf: 0,
            graphics_queue_flags: 0,
            decode_qf: 0,
            decode_video_caps: 0,
            instance_extensions: Vec::new(),
            device_extensions: Vec::new(),
            f_sampler_ycbcr: true,
            f_timeline_semaphore: true,
            f_synchronization2: true,
            f_shader_int16: false,
            f_storage_buffer8: false,
            f_subgroup_size_control: false,
            f_compute_full_subgroups: false,
            f_shader_float16: false,
            api_version: 0,
            queue_families: Vec::new(),
            pyrowave_decode: false,
            video_decode: true,
            d3d11_import: false,
            adapter_luid: None,
            queue_lock: std::sync::Arc::new(QueueLock::new()),
        }
    }

    /// Auto's Linux hardware order: Vulkan-first on NVIDIA (no usable VAAPI) and ALL AMD
    /// (Vulkan decode outperforms VAAPI on RADV — on-glass verdict; VanGogh additionally
    /// chroma-fringes over VAAPI); Intel/unknown keep VAAPI first (ANV's Vulkan Video is
    /// the least-proven Mesa path). A Vulkan failure streak still demotes to VAAPI, so
    /// Vulkan-first can never strand a box on software decode.
    #[test]
    fn vulkan_over_vaapi_on_nvidia_and_amd() {
        assert!(decode_device(0x10DE, "NVIDIA GeForce RTX 5070 Ti").prefer_vulkan_over_vaapi());
        assert!(decode_device(0x1002, "AMD RADV VANGOGH").prefer_vulkan_over_vaapi());
        assert!(
            decode_device(0x1002, "AMD Custom GPU 0405 (RADV VANGOGH)").prefer_vulkan_over_vaapi()
        );
        assert!(
            decode_device(0x1002, "AMD Radeon RX 7800 XT (RADV NAVI32)").prefer_vulkan_over_vaapi()
        );
        assert!(
            !decode_device(0x8086, "Intel(R) Arc(tm) A770 Graphics (DG2)")
                .prefer_vulkan_over_vaapi()
        );
    }

    fn desc(matrix: u8, full_range: bool) -> ColorDesc {
        ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix,
            full_range,
        }
    }

    fn apply(rows: &[[f32; 4]; 3], yuv: [f32; 3]) -> [f32; 3] {
        core::array::from_fn(|r| {
            rows[r][0] * yuv[0] + rows[r][1] * yuv[1] + rows[r][2] * yuv[2] + rows[r][3]
        })
    }

    /// 10-bit limited MSB-packed (P010/X6): reference white Y=940, black Y=64, neutral
    /// chroma 512 — sampled as UNORM16 of `code << 6`.
    #[test]
    fn bt2020_10bit_limited_white_black() {
        let rows = csc_rows(desc(9, false), 10, true);
        let s = |code: u32| ((code << 6) as f32) / 65535.0;
        let white = apply(&rows, [s(940), s(512), s(512)]);
        let black = apply(&rows, [s(64), s(512), s(512)]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.002, "white {white:?}");
            assert!(b.abs() < 0.002, "black {black:?}");
        }
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0
    /// — the GL presenter's test, in row form.
    #[test]
    fn bt709_limited_white_black() {
        let rows = csc_rows(desc(1, false), 8, false);
        let white = apply(&rows, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let black = apply(&rows, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.005, "white {white:?}");
            assert!(b.abs() < 0.005, "black {black:?}");
        }
    }

    /// Full-range identity points + the 601-vs-709 red excursion (guards the
    /// matrix-code dispatch), same as the GL presenter's test.
    #[test]
    fn full_range_and_red_excursion() {
        let rows = csc_rows(desc(5, true), 8, false);
        let white = apply(&rows, [1.0, 0.5, 0.5]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let red = apply(&rows, [0.0, 0.5, 1.0]);
        assert!((red[0] - 2.0 * (1.0 - 0.299) * 0.5).abs() < 1e-4, "{red:?}");
        let rows709 = csc_rows(desc(1, true), 8, false);
        let red709 = apply(&rows709, [0.0, 0.5, 1.0]);
        assert!(
            (red709[0] - 2.0 * (1.0 - 0.2126) * 0.5).abs() < 1e-4,
            "{red709:?}"
        );
        assert!((red[0] - red709[0]).abs() > 0.05);
    }

    /// The row form must agree with the GL presenter's column-major `yuv_to_rgb` on a
    /// grid of inputs — same math, different packing.
    #[test]
    fn rows_match_the_gl_matrix_form() {
        for (matrix, full) in [(1u8, false), (1, true), (5, false), (9, false), (9, true)] {
            let d = desc(matrix, full);
            let rows = csc_rows(d, 8, false);
            // Reimplementation of video_gl::yuv_to_rgb's application for comparison.
            let (kr, kb) = match matrix {
                5 | 6 => (0.299f32, 0.114f32),
                9 | 10 => (0.2627, 0.0593),
                _ => (0.2126, 0.0722),
            };
            let kg = 1.0 - kr - kb;
            let (sy, oy, sc) = if full {
                (1.0f32, 0.0f32, 1.0f32)
            } else {
                (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
            };
            let mat = [
                sy,
                sy,
                sy,
                0.0,
                -2.0 * (1.0 - kb) * kb / kg * sc,
                2.0 * (1.0 - kb) * sc,
                2.0 * (1.0 - kr) * sc,
                -2.0 * (1.0 - kr) * kr / kg * sc,
                0.0,
            ];
            let off = [oy, -0.5, -0.5];
            for yuv in [
                [0.1f32, 0.3, 0.7],
                [0.9, 0.5, 0.5],
                [0.5, 0.2, 0.8],
                [16.0 / 255.0, 0.5, 0.5],
            ] {
                let v = [yuv[0] + off[0], yuv[1] + off[1], yuv[2] + off[2]];
                let gl: [f32; 3] =
                    core::array::from_fn(|r| (0..3).map(|c| mat[c * 3 + r] * v[c]).sum());
                let ours = apply(&rows, yuv);
                for (a, b) in gl.iter().zip(ours) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "{matrix}/{full}: gl {gl:?} rows {ours:?}"
                    );
                }
            }
        }
    }

    /// Lock the DRM FourCC magic numbers against typos — these are the exact values
    /// `<drm_fourcc.h>` defines, and a wrong one is what painted the Steam Deck green.
    #[test]
    fn drm_fourcc_constants() {
        assert_eq!(fourcc(b'N', b'V', b'1', b'2'), 0x3231_564e);
        assert_eq!(fourcc(b'P', b'0', b'1', b'0'), 0x3031_3050);
        assert_eq!(
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12),
            Some(0x3231_564e)
        );
        assert_eq!(
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_RGBA),
            None
        );
    }

    /// The wire → `ColorDesc` plumbing: an HDR10 stream's VUI (BT.2020 primaries, PQ
    /// transfer, BT.2020-NCL matrix, limited range) must arrive on the decoded frame —
    /// this is what the Windows host emits in-band for an HDR desktop, and mis-rendering
    /// it as BT.709 is the washed-out-colors bug. Fixture: one 64×64 Main10 IDR
    /// (`tests/pq-frame.h265`, x265 with explicit VUI).
    #[test]
    fn software_decode_carries_pq_signaling() {
        let au = include_bytes!("../tests/pq-frame.h265");
        let mut dec = SoftwareDecoder::new(ffmpeg::codec::Id::HEVC).expect("hevc decoder");
        let mut got = dec.decode(au).expect("decode");
        if got.is_none() {
            // Low-delay decoders may still hold the frame until a flush — send EOF.
            dec.decoder.send_eof().ok();
            let mut frame = AvFrame::empty();
            if dec.decoder.receive_frame(&mut frame).is_ok() {
                got = Some(dec.convert_rgba(&frame).expect("convert"));
            }
        }
        let f = got.expect("no frame decoded from the PQ fixture");
        assert_eq!(
            f.color,
            ColorDesc {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false
            }
        );
        assert!(f.color.is_pq());
        assert_eq!((f.width, f.height), (64, 64));
    }

    /// Golden colour fixtures: one 256×64 LOSSLESS x265 IDR of 8 fully-saturated colour bars per
    /// signaling variant (generated offline with ffmpeg/libx265; the RGB→YUV conversion matched
    /// to the VUI each fixture declares, so the original RGB is recoverable ±1 code). Decoding
    /// through the real CPU path (`SoftwareDecoder` → per-frame `ColorDesc` → swscale with the
    /// signaled matrix/range) must reproduce the bars — the end-to-end guard for the
    /// signaling-driven CSC across BT.601/709 × limited/full. A hardcoded-709 regression fails
    /// the 601 fixture by tens of code points; a range mix-up fails the full-range one.
    #[test]
    fn software_decode_reproduces_golden_bars() {
        const BARS: [(u8, u8, u8); 8] = [
            (255, 255, 255),
            (255, 255, 0),
            (0, 255, 255),
            (0, 255, 0),
            (255, 0, 255),
            (255, 0, 0),
            (0, 0, 255),
            (0, 0, 0),
        ];
        let fixtures: [(&str, &[u8], ColorDesc); 3] = [
            (
                "601-limited",
                include_bytes!("../tests/bars-601-limited.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 5, // BT.470BG — what a Linux host's RGB-input NVENC signals
                    full_range: false,
                },
            ),
            (
                "709-limited",
                include_bytes!("../tests/bars-709-limited.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: false,
                },
            ),
            (
                "709-full",
                include_bytes!("../tests/bars-709-full.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: true, // the PUNKTFUNK_444_FULLRANGE experiment's signaling
                },
            ),
        ];
        for (name, au, want_color) in fixtures {
            let mut dec = SoftwareDecoder::new(ffmpeg::codec::Id::HEVC).expect("hevc decoder");
            let mut got = dec.decode(au).expect("decode");
            if got.is_none() {
                dec.decoder.send_eof().ok();
                let mut frame = AvFrame::empty();
                if dec.decoder.receive_frame(&mut frame).is_ok() {
                    got = Some(dec.convert_rgba(&frame).expect("convert"));
                }
            }
            let f = got.unwrap_or_else(|| panic!("{name}: no frame decoded"));
            assert_eq!(f.color, want_color, "{name}: signaling");
            assert_eq!((f.width, f.height), (256, 64), "{name}: dims");
            for (i, (r, g, b)) in BARS.iter().enumerate() {
                let (cx, cy) = (i * 32 + 16, 32usize);
                let o = cy * f.stride + cx * 4;
                let px = &f.rgba[o..o + 3];
                for (got, want) in px.iter().zip([r, g, b]) {
                    assert!(
                        got.abs_diff(*want) <= 3,
                        "{name} bar {i}: got {px:?}, want ({r},{g},{b})"
                    );
                }
            }
        }
    }
}
