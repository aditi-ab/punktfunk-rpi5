//! Video decode: reassembled HEVC access units → frames for the presenter.
//!
//! Backends, picked at session start (auto is vendor-ordered on BOTH desktop OSes —
//! see [`VulkanDecodeDevice::prefer_vulkan_first`]; on H.264 AND HEVC sessions the
//! native pf-vkdecode decoder (`video_vk_native`, gated by [`native_vulkan_gate`])
//! slots in immediately ABOVE the FFmpeg-Vulkan rung wherever the ladder reaches it —
//! the program's goal is dropping FFmpeg from the client, and a native INIT failure
//! falls through to FFmpeg-Vulkan; a runtime error streak demotes past it, same as
//! FFmpeg-Vulkan's own streaks do — EXCEPT while the native rung has never delivered
//! a frame, which falls through to FFmpeg-Vulkan too, see `decode_frame`). Linux:
//! native → vulkan → vaapi → software on NVIDIA and ALL AMD (VanGogh included), vaapi →
//! native → vulkan → software on Intel/unknown. Windows: native → vulkan → d3d11va →
//! software on NVIDIA/AMD, d3d11va → native → vulkan → software on Intel/unknown.
//! Override: `PUNKTFUNK_DECODER=vulkan|vaapi|d3d11va|software|native-vulkan` —
//! `vulkan` names the FFmpeg-Vulkan backend specifically; `native-vulkan` pins the
//! pf-vkdecode decoder by name, skipping the vendor-ordered rungs ahead of it):
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
//! hardware pair there is Vulkan Video and **D3D11VA** (`crate::video_d3d11` — the
//! vendor-agnostic DXVA path every Windows video player exercises), ordered per vendor:
//! Intel's driver DOES advertise Vulkan Video (Arc drivers since 2023), but FFmpeg-Vulkan
//! on it strobes and burns the frame budget (B580 field report, 2026-07) where D3D11VA
//! streams clean — so Intel/unknown take D3D11VA first and NVIDIA/AMD keep Vulkan first.
//! Everything dmabuf-shaped is `cfg(target_os = "linux")`-gated inline.

// bindgen's C-enum repr is target-dependent (u32 on Linux/clang, i32 on MSVC), so the
// pf-ffvk Vulkan flag/enum casts below are required on one platform and no-ops on the
// other — the lint would fire on whichever platform the cast is a no-op for.
#![allow(clippy::unnecessary_cast)]

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg_next as ffmpeg;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub use crate::video_color::{csc_rows, ColorDesc};
use crate::video_software::SoftwareDecoder;
#[cfg(target_os = "linux")]
use crate::video_vaapi::VaapiDecoder;
use crate::video_vk_native::{NativeCodec, NativeVulkanDecoder};
use crate::video_vulkan::VulkanDecoder;

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
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(crate::video_pyrowave::PyroWavePlanarFrame),
    /// Native Vulkan Video output (pf-vkdecode — auto's H.264/HEVC rung immediately
    /// above FFmpeg-Vulkan, also pinnable via `PUNKTFUNK_DECODER=native-vulkan`): a
    /// decoded image + per-plane views already on the PRESENTER's device — same
    /// zero-copy contract as [`DecodedImage::VkFrame`], no FFmpeg involved. The
    /// picture format is the stream's, carried on the frame
    /// ([`NativeVkFrame::vk_format`] — NV12 for H.264 and HEVC Main, P010 for Main
    /// 10, the two-plane 4:4:4 formats for RExt), never assumed. The presenter waits
    /// the frame's timeline pair, transitions the layer for sampling and BACK to
    /// [`NativeVkFrame::layout`], and releases the decoder's slot by dropping the
    /// frame (its guard sends the release token).
    NativeVk(NativeVkFrame),
}

/// What the decode lane knows about this session's INTEGRITY — M4's telemetry
/// surface, and the answer to the question that started the whole native-decode
/// program: "was that stream actually clean, or could nothing here have told us?"
///
/// Only the native rung fills it in ([`Decoder::decode_health`] answers `None`
/// everywhere else), because only the native rung has the two detectors: a
/// bitstream planner that reports lost references, and a per-op `RESULT_STATUS`
/// query that reports what the DRIVER thought of the decode. FFmpeg's Vulkan
/// decoder creates no queries at all (`nb_queries = 0`), never sets
/// `AV_FRAME_FLAG_CORRUPT`, and reports trouble only as log lines — which is why
/// the Xbox Ally X corruption was undetectable rather than merely undetected.
///
/// Counters are session-cumulative and monotonic; the stats window diffs them the
/// way it already diffs `frames_dropped`. Nothing here allocates, and nothing here
/// is computed per frame beyond an add — the whole struct is read once per stats
/// window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeHealth {
    /// AUs whose plan needed CONCEALMENT: a reference the DPB no longer held, a
    /// `frame_num` gap, a NALU walk that stopped early. The picture would have
    /// been decoded from a substitute, so its output was released unshown.
    pub damaged: u64,
    /// Frames the DRIVER reported corrupt through their `RESULT_STATUS_ONLY`
    /// query. Distinct from [`Self::damaged`] on purpose: damaged means the
    /// bitstream arrived incomplete, failed means the hardware could not decode
    /// what did arrive. They have different causes and different fixes, and
    /// collapsing them is how "the stream is fine, it's your GPU" arguments start.
    ///
    /// **Structurally 0 where [`Self::status_queries`] is false**, and
    /// [`Self::note`] enforces that rather than trusting its callers: on such a
    /// device `poll_status` still answers `Failed` for a lost device or an
    /// unreadable timeline, and reporting THAT as a driver verdict would point a
    /// support engineer at a verdict the hardware cannot produce ("driver-failed 1
    /// · no driver status" on one line). Those frames still cost a picture, so
    /// they still extend [`Self::run`] — they are just not attributed to a driver
    /// that never spoke.
    pub failed: u64,
    /// AUs the decoder REFUSED outright: a plan error (a parse failure, an AU
    /// outside the punktfunk envelope, a slice against a parameter set never
    /// seen), or a Vulkan/session failure. The decoder produced no picture and
    /// said so with an error.
    ///
    /// Counted apart from [`Self::damaged`] because the two mean opposite things
    /// about the RUNG: concealment says the decoder coped with a damaged stream,
    /// refusal says the decoder could not run at all. A rung refusing every AU is
    /// the shape of a host renegotiating outside the envelope — a frozen screen —
    /// and without this counter its stats surface reads exactly like a clean
    /// session, which is the founding failure mode of this whole program.
    pub refused: u64,
    /// Consecutive AUs that produced no showable picture, ending at the latest one
    /// — 0 the moment a clean AU decodes.
    ///
    /// This is the field a support engineer reads first, because it separates the
    /// two failure shapes a raw count cannot: `damaged 40 · run 0` is a lossy link
    /// that keeps recovering, `damaged 40 · run 40` is a stream that went down and
    /// never came back. Both look identical as a total.
    pub run: u32,
    /// The longest [`Self::run`] of the session — the worst moment, which a
    /// once-per-second sample of `run` will usually miss entirely.
    pub worst_run: u32,
    /// This device answers per-op decode-status queries
    /// (`queryResultStatusSupport`). When FALSE — RADV, where recording a query
    /// anyway HANGS the VCN ring — [`Self::failed`] can only ever read 0, because
    /// there is no verdict to read: the status degrades to timeline completion,
    /// exactly what FFmpeg knows on every driver. A report that omits this cannot
    /// tell "clean" from "unmeasured", which is the precise shape of the failure
    /// this program exists to end.
    pub status_queries: bool,
}

impl DecodeHealth {
    /// Fold one AU's verdict. `damaged` = its plan needed concealment; `refused` =
    /// the decoder rejected the AU outright (an `Err` out of `decode`); `failed` =
    /// how many PRIOR frames just read a `Failed` decode status.
    ///
    /// All three extend the run: a support engineer asking "did it ever recover?"
    /// means the picture, and a refused AU or a driver-failed frame is as absent
    /// from the screen as a concealed one.
    ///
    /// The one asymmetry is deliberate and is the whole point of
    /// [`Self::status_queries`]: where the device answers no status queries, a
    /// `Failed` read is NOT a driver verdict — it is the degraded timeline path
    /// (the session generation is gone, the device is lost, the semaphore could
    /// not be read) — so it extends the run without ever being counted as
    /// [`Self::failed`]. Enforced here, at the one place every counter is written,
    /// rather than at each call site, because "clean" and "unmeasured" staying
    /// distinguishable is the invariant this struct exists for.
    pub(crate) fn note(&mut self, damaged: bool, refused: bool, failed: u32) {
        if self.status_queries {
            self.failed = self.failed.saturating_add(u64::from(failed));
        }
        if damaged {
            self.damaged = self.damaged.saturating_add(1);
        }
        if refused {
            self.refused = self.refused.saturating_add(1);
        }
        if damaged || refused || failed > 0 {
            self.run = self.run.saturating_add(1);
            self.worst_run = self.worst_run.max(self.run);
        } else {
            self.run = 0;
        }
    }
}

/// A raw `VkFormat` code point, carried across the ash-free boundary.
///
/// A newtype rather than a bare `i32` because the two hardware frame types
/// ([`VkVideoFrame`], [`NativeVkFrame`]) carry OTHER `i32`s — `poc` foremost — and
/// the presenter's colour-math lookup takes exactly one number. Handed the wrong
/// one it compiles, warns once about an unmapped format, and renders every frame of
/// the session as 8-bit: decoded correctly, displayed wrong, silently. The wrapper
/// makes that a type error instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawVkFormat(pub i32);

/// Every picture format the NATIVE decode lane can deliver, as raw `VkFormat` code
/// points — pf-vkdecode's own [`pf_vkdecode::OUTPUT_FORMATS`] vocabulary, not a copy
/// of it.
///
/// It is public so the PRESENTER can pin its per-format colour-math table against the
/// real producer. pf-presenter has no pf-vkdecode dependency, so without this its only
/// available check is the FFmpeg lane's table against itself — which stays green if
/// pf-vkdecode grows a fifth output format (12-bit RExt) that the CSC pass has no
/// depth mapping for. This crate sees both, so the fact crosses here.
pub fn native_picture_formats() -> Vec<RawVkFormat> {
    pf_vkdecode::OUTPUT_FORMATS
        .iter()
        .map(|f| RawVkFormat(f.as_raw()))
        .collect()
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
    /// The frame pool's VkFormat (`AVVulkanFramesContext.format[0]`) — the
    /// multiplanar format the presenter builds its per-plane views against.
    pub vk_format: RawVkFormat,
    /// The frame's timeline semaphore (raw VkSemaphore; creation-constant) and the
    /// value FFmpeg's decode submission signals on completion — the pump waits this
    /// pair AFTER shipping the frame to measure true GPU decode time (zero pipeline
    /// cost: the presenter already waits the same pair on the GPU).
    pub timeline_sem: u64,
    pub decode_done_value: u64,
    pub width: u32,
    pub height: u32,
    /// The decode POOL's allocated extent (`AVHWFramesContext.width`/`.height`) — the
    /// CODED picture size (rounded up to the codec's macroblock alignment, then to the
    /// driver's Vulkan picture-access granularity), so it is `>=` `width`/`height`. At
    /// 1080p the pool is 1088 rows tall: 1080 is not a multiple of 16.
    ///
    /// The presenter samples this image with NORMALIZED coordinates, so it needs both
    /// numbers — `width`/`height` is what to display, `coded_*` is what the texture
    /// actually spans. Sampling `0..1` without the ratio stretches the alignment padding
    /// into view; because encoders fill those rows by replicating the picture's last
    /// line, that reads as the bottom row smeared over the final few rows of the image
    /// (field report 2026-07-31). Same class as the D3D11VA source-rect clamp in
    /// `crate::video_d3d11`, which shows as a green bar there only because DXVA padding
    /// is left uninitialized rather than replicated.
    pub coded_width: u32,
    pub coded_height: u32,
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

/// The layout a [`NativeVkFrame`]'s image layer is in when its semaphore signals —
/// pf-client-core's ash-free mirror of the two decode layouts, so the presenter can
/// transition for sampling and back without this crate naming `vk::ImageLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeVkLayout {
    /// `VIDEO_DECODE_DST_KHR` — distinct-mode output; the layer holds ONLY this
    /// picture and the next decode into the slot discards it (UNDEFINED-old-layout).
    DecodeDst,
    /// `VIDEO_DECODE_DPB_KHR` — coincide-mode output: the picture IS a DPB slot and
    /// may still be a live reference, so a consumer that transitions it for sampling
    /// MUST transition it back to this layout in the same submission.
    DecodeDpb,
}

/// The release token a presented/dropped [`NativeVkFrame`] hands back to the native
/// decode backend: `seq` names the shipped frame, `generation` the decoder session it
/// belongs to (a stale generation routes to the decoder's graveyard — retired pools
/// die on their last token), and `presented` reports whether the presenter SAMPLED
/// the image — i.e. whether its submission enqueued the frame's `value + 1` timeline
/// signal (the AVVkFrame write-back the decoder must wait before reusing the image).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeReleaseToken {
    pub seq: u64,
    pub generation: u64,
    /// The presenter's sampling submission (with its `value + 1` signal) was
    /// enqueued for this frame. `false` for frames dropped unpresented
    /// (newest-wins displacement, demotion drain, failed submit).
    pub presented: bool,
}

/// Sends the frame's [`NativeReleaseToken`] exactly once, on drop — the native path's
/// analog of the VAAPI/VkFrame `DrmFrameGuard`s. The presenter holds the frame (and so
/// this guard) until its sampling submission's fence has been waited, which makes
/// "guard dropped" equal "the GPU is done with the image"; a frame dropped UNPRESENTED
/// (newest-wins displacement, demotion drain) releases through the very same drop. A
/// dead channel (the backend was demoted/rebuilt) is ignored — the decoder that owned
/// the slot is gone.
pub struct NativeReleaseGuard {
    tx: std::sync::mpsc::Sender<NativeReleaseToken>,
    token: Option<NativeReleaseToken>,
}

impl NativeReleaseGuard {
    pub(crate) fn new(
        tx: std::sync::mpsc::Sender<NativeReleaseToken>,
        token: NativeReleaseToken,
    ) -> Self {
        Self {
            tx,
            token: Some(token),
        }
    }

    /// Record that the sampling submission — including the frame's `value + 1`
    /// timeline signal — was enqueued. The presenter calls this exactly when its
    /// submit succeeded; the token then tells the decoder to wait that write-back
    /// before the image's next use.
    pub fn mark_presented(&mut self) {
        if let Some(token) = &mut self.token {
            token.presented = true;
        }
    }
}

impl Drop for NativeReleaseGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let _ = self.tx.send(token);
        }
    }
}

/// One natively decoded frame (pf-vkdecode). Everything is raw `u64`/plain data — this
/// crate stays ash-free, exactly like [`VulkanDecodeDevice`]. The handles BORROW the
/// decoder's pools: valid until the frame is released (the guard's drop) AND the
/// decoder generation they carry is current — the backend keeps the decoder alive
/// until every shipped frame's token has come back (bounded), so the presenter never
/// has to validate liveness itself.
pub struct NativeVkFrame {
    /// The decode image (raw `VkImage`); the picture occupies array layer [`Self::layer`].
    pub image: u64,
    /// The picture's own `VkFormat` (same shape as [`VkVideoFrame::vk_format`]):
    /// what the image was created with and what [`Self::plane_views`] alias.
    ///
    /// Read it, never infer it from the codec. H.264 in this program is the 8-bit
    /// 4:2:0 envelope, so its frames are always NV12 — but an H.265 session's format
    /// is the STREAM's (Main → NV12, Main 10 → P010, RExt 4:4:4 → the two-plane 4:4:4
    /// formats) and can change mid-stream when the host renegotiates. The presenter
    /// derives the CSC pass's bit depth and MSB-packing factor from this; an assumed
    /// 8 bits over a P010 surface decodes correctly and displays wrong, which is the
    /// failure class this program exists to refuse.
    pub vk_format: RawVkFormat,
    /// Per-plane views (raw `VkImageView`s) in the formats pf-vkdecode resolves for
    /// [`Self::vk_format`] — `R8`/`R8G8` for the 8-bit families, `R10X6`/`R10X6G10X6`
    /// for the 10-bit ones — the presenter's planar CSC sampling contract, same shape
    /// as the FFmpeg path's derived plane views.
    pub plane_views: [u64; 2],
    pub layer: u32,
    /// The layout the layer is in when the semaphore signals; the presenter must
    /// return it there after sampling (see [`NativeVkLayout`]).
    pub layout: NativeVkLayout,
    /// Timeline pair (raw `VkSemaphore` + value): pixels are ready when the semaphore
    /// reaches the value — the presenter waits it on the GPU (submit wait list, like
    /// the AVVkFrame path), never on the host.
    pub semaphore: u64,
    pub semaphore_value: u64,
    /// The decoder session generation the handles belong to (rides the release token).
    pub generation: u64,
    /// Display size (the conformance-window crop) — what [`DecodedImage::dimensions`]
    /// reports and what the presenter shows.
    pub width: u32,
    pub height: u32,
    /// The image's allocated/coded extent (`>=` display) — the presenter scales its
    /// sampling UVs by display/coded per axis or the alignment padding smears into
    /// view (the 1088-row lesson; same contract as [`VkVideoFrame::coded_width`]).
    pub coded_width: u32,
    pub coded_height: u32,
    /// Crop origin within the coded picture. Punktfunk hosts emit origin crops only;
    /// the presenter's UV-scale path assumes (0,0) and a nonzero origin would show the
    /// wrong window — carried so that assumption is checkable, not silent.
    pub crop_x: u32,
    pub crop_y: u32,
    /// Colour signalling, read from the SPS active for THIS picture (the H.264/H.265
    /// VUI → H.273 code points, with E.2.1's "unspecified" inference where the VUI is
    /// silent) — per frame, like the FFmpeg rungs' AVFrame CICP, because the host
    /// switches HDR in-band; "unspecified" resolves to the BT.709-limited SDR
    /// default (`csc_rows`' documented fallback).
    pub color: ColorDesc,
    /// IDR — the stream's re-anchor point (the pump's post-loss resume signal). Truly
    /// IDR: on H.265 a CRA/BLA does NOT set this (pf-bitstream keys it off the NALU
    /// type), which costs nothing against punktfunk hosts — they emit IDR-only
    /// re-entry points — and is the conservative direction anyway, since a CRA's
    /// leading pictures may be undecodable.
    pub keyframe: bool,
    pub poc: i32,
    /// What this frame's AU said about intra-refresh RECOVERY, read out of the
    /// bitstream's own recovery point SEI (pf-vkdecode's `RecoveryWatch`).
    ///
    /// [`Self::keyframe`] cannot answer for an intra-refresh session — the wave
    /// never emits an IDR — so without this the pump has no clean point to lift a
    /// post-loss freeze on and holds the last good picture until its 500 ms
    /// backstop forces the very IDR the wave exists to avoid. The wire's
    /// `USER_FLAG_RECOVERY_POINT` says the same thing when the host sets it, which
    /// only one of the three wave-running encoder backends does (Linux
    /// libav-NVENC); this is the same fact taken from the stream instead of from
    /// the host, and it cannot be lost separately from the picture. Fed to
    /// [`ReanchorGate::on_local_recovery`](punktfunk_core::reanchor::ReanchorGate::on_local_recovery).
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
    /// This picture's position in DECODE order (pf-vkdecode's strictly increasing
    /// per-session ordinal). Delivery order is not decode order: after a failed AU
    /// the H.265 decoder flushes its DPB, handing back every buffered picture at
    /// once — pictures decoded BEFORE the loss, carrying the recovery marks of the
    /// wave they were decoded in. Arriving after the pump armed its freeze, those
    /// marks would lift it on a heal that completed before the loss. The pump
    /// stamps this ordinal at every arm and ignores [`Self::recovery`] from
    /// anything older.
    pub decode_order: u64,
    /// Sends the release token on drop — see [`NativeReleaseGuard`].
    pub guard: NativeReleaseGuard,
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
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => f.keyframe,
            DecodedImage::NativeVk(f) => f.keyframe,
        }
    }

    /// What the decoder's OWN bitstream parser saw about an intra-refresh heal on
    /// this frame's AU — the recovery point SEI, which no platform decoder exposes.
    ///
    /// Only the native rung can answer: libavcodec parses the SEI internally and
    /// surfaces nothing of it (its `AV_FRAME_FLAG_KEY` is IDR-only), MediaCodec and
    /// VideoToolbox likewise. Everyone else reports
    /// [`LocalRecovery::NONE`](punktfunk_core::reanchor::LocalRecovery::NONE) and
    /// the pump's re-anchor behaviour on those lanes is byte-for-byte what it was.
    pub fn local_recovery(&self) -> punktfunk_core::reanchor::LocalRecovery {
        match self {
            DecodedImage::NativeVk(f) => f.recovery,
            _ => punktfunk_core::reanchor::LocalRecovery::NONE,
        }
    }

    /// This frame's position in DECODE order, where the lane knows one — see
    /// [`NativeVkFrame::decode_order`]. `None` everywhere else, which is what the
    /// pump reads as "this lane reports no local recovery either, so there is
    /// nothing to date-stamp".
    pub fn decode_order(&self) -> Option<u64> {
        match self {
            DecodedImage::NativeVk(f) => Some(f.decode_order),
            _ => None,
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
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
            DecodedImage::NativeVk(f) => (f.width, f.height),
        }
    }
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
pub struct DrmFrameGuard(pub(crate) *mut ffmpeg::ffi::AVFrame);
// SAFETY: the guard owns one `AVFrame` and frees it exactly once in `Drop`. libav's buffer
// refcounts are atomic and its hwframe pool is internally locked, so releasing the frame — and with
// it the VAAPI surface, back to the decoder's pool — from a different thread than the one that
// mapped it is sound. That is the whole point here: the guard is handed to GTK and dropped on the
// main thread while the pump thread keeps decoding. Moved, never shared; deliberately NOT `Sync`.
unsafe impl Send for DrmFrameGuard {}

impl Drop for DrmFrameGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the one `AVFrame` this guard owns; `av_frame_free` releases it
        // exactly once (this `Drop` runs once) and nulls the pointer through the `&mut`.
        unsafe { ffmpeg::ffi::av_frame_free(&mut self.0) };
    }
}

enum Backend {
    Vulkan(VulkanDecoder),
    /// Native Vulkan Video H.264/HEVC (pf-vkdecode) on the presenter's device —
    /// auto's rung immediately above FFmpeg-Vulkan since the 2026-08-05 ladder
    /// decision (WP-D closed bit-exact; the program's goal is dropping FFmpeg from
    /// the client), also pinnable by name (`PUNKTFUNK_DECODER=native-vulkan`) — see
    /// [`native_vulkan_gate`]. The negotiated codec picks the decoder once, at
    /// construction; everything else about this backend is codec-agnostic. Errors
    /// ride the SAME streak/demotion machinery as the FFmpeg-Vulkan rung.
    /// Boxed: the decoder (planner + shipped-frame ledger) dwarfs the other variants,
    /// same as PyroWave below.
    NativeVulkan(Box<NativeVulkanDecoder>),
    #[cfg(target_os = "linux")]
    Vaapi(VaapiDecoder),
    #[cfg(windows)]
    D3d11va(crate::video_d3d11::D3d11vaDecoder),
    /// PyroWave (wired-LAN wavelet codec): pyrowave compute on the presenter's device,
    /// no FFmpeg involvement (Linux + Windows — same Vulkan presenter on both). No demotion
    /// rung — there is no other decoder for it.
    /// Boxed: the decoder (pinned create-info hold + plane ring) dwarfs the other variants.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(Box<crate::video_pyrowave::PyroWaveDecoder>),
    Software(SoftwareDecoder),
}

/// The picture shape the host resolved in its Welcome, before a single AU arrives.
///
/// The in-band SPS stays authoritative — this is the NEGOTIATED answer, which is what
/// makes it available at decoder-construction time. It exists so a backend whose
/// support for a shape is device-dependent can refuse BEFORE it is chosen, where the
/// ladder's fall-through to the next rung is a plain construction failure, instead of
/// discovering it at the first decode where the only exit is an error-streak demotion
/// PAST that rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// `chroma_format_idc` — [`punktfunk_core::quic::CHROMA_IDC_420`] (1) or
    /// [`punktfunk_core::quic::CHROMA_IDC_444`] (3). An older host that omitted it
    /// reads as 4:2:0, never 0.
    pub chroma_format_idc: u8,
    /// Bits per component: 8, or 10 for a Main10/HDR session (an older host reads 8).
    pub bit_depth: u8,
}

impl StreamFormat {
    /// The 8-bit 4:2:0 envelope — what every H.264 session is, and what an older
    /// host's Welcome decodes to.
    pub const SDR_420_8: StreamFormat = StreamFormat {
        chroma_format_idc: punktfunk_core::quic::CHROMA_IDC_420,
        bit_depth: 8,
    };

    /// `bit_depth` as the `bit_depth_luma_minus8` the H.265 SPS (and pf-vkdecode's
    /// profile key) speaks, or `None` for a depth outside the 8/10 envelope — which
    /// is itself a refusal, not a "probe skipped".
    pub(crate) fn bit_depth_minus8(self) -> Option<u8> {
        self.bit_depth.checked_sub(8)
    }
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
    /// When the current error streak started. Demotion needs the streak to be OLD as well
    /// as long: one startup loss burst produces 3+ consecutive failing AUs within
    /// milliseconds — demoting on count alone (live-hit: Intel iGPU, 2026-07-19, three
    /// errors in 20 ms → software forever) never gives the IDR requested on the FIRST
    /// error (~100–300 ms round trip) a chance to rescue the hardware decoder.
    first_fail: Option<std::time::Instant>,
    /// Set when the decoder needs a fresh IDR to resynchronize (after an error or a demotion).
    /// The pump drains it and asks the host — under the infinite GOP there is no periodic
    /// keyframe, so a rebuilt/erroring decoder would otherwise stay gray/frozen forever.
    want_keyframe: bool,
    /// The CURRENT backend has delivered at least one frame. A backend that never did
    /// is one the session never actually had, so its error streak must not cost the
    /// session the rung BELOW it — see the native→FFmpeg-Vulkan arm in
    /// [`Decoder::decode_frame`]. Reset on every backend swap.
    delivered: bool,
    /// The presenter's device, kept so that same arm can build the FFmpeg-Vulkan
    /// decoder mid-stream. Cloned once per session; its handles outlive every pump
    /// (see [`VulkanDecodeDevice`]).
    vk: Option<VulkanDecodeDevice>,
    /// The presenter has the win32 external-memory import path, so D3D11VA frames can reach
    /// the screen — kept for the mid-session Vulkan→D3D11VA demotion rung (the Windows
    /// analog of Linux's Vulkan→VAAPI rung).
    #[cfg(windows)]
    d3d11_import: bool,
    /// The presenter adapter's LUID (see [`VulkanDecodeDevice::adapter_luid`]) so a demotion
    /// rebuild lands on the SAME GPU.
    #[cfg(windows)]
    adapter_luid: Option<[u8; 8]>,
    /// [`VulkanDecodeDevice::d3d11_hdr10`], for the same demotion rebuild.
    #[cfg(windows)]
    d3d11_hdr10: bool,
}

/// Demote a hardware backend (Vulkan→VAAPI/D3D11VA, VAAPI/D3D11VA→software) only after
/// this many consecutive decode errors; a lone transient error just re-requests an IDR
/// and keeps the hardware decoder.
const VAAPI_DEMOTE_AFTER: u32 = 3;

/// ...AND only when the streak has lasted this long. Every error re-requests an IDR, and
/// one arriving + decoding resets the streak — so a genuinely broken driver (errors keep
/// flowing through multiple IDR cycles) still demotes ~a second in, while a burst of
/// consecutive bad AUs from a single loss event no longer strands the session on
/// software before the first requested IDR could even arrive.
const HW_DEMOTE_MIN_STREAK: std::time::Duration = std::time::Duration::from_millis(1000);

/// May a successful `decode` answer CLEAR the demotion error streak?
///
/// The streak is the hardware rungs' only escape hatch, and clearing it is a
/// claim: *this decoder is working*. A delivered frame proves that outright. So
/// does a clean `Ok(None)` — the decoder ran and had nothing to object to (it
/// buffered, or skipped an H.265 RASL picture after an open-GOP join).
///
/// What proves nothing is the third `Ok(None)`: the native rung's CONCEALMENT
/// answer, where the plan needed a substitute for something lost and the picture
/// was released unshown. That is deliberately not an `Err` — stream damage is not
/// a decoder fault, and three of them in a second must not demote the rung on
/// exactly the lossy links it exists to diagnose — but "not an error" was silently
/// read as "a success", and clearing on it is the dangerous half of that:
///
/// * a driver failing every OTHER AU on a lossy link has its `Err`s zeroed by the
///   concealment between them and never reaches [`VAAPI_DEMOTE_AFTER`];
/// * and a rung answering concealment FOREVER — a host framing regression putting
///   two pictures in one AU makes every AU conceal, and unlike a reference gap it
///   does not self-heal at an IDR — holds a frozen last-good frame with no escape
///   at all, where before this milestone the same stream demoted to a rung that
///   ignores AU boundaries and showed a picture.
///
/// Leaving the streak untouched costs nothing on a healthy link: one damaged AU
/// between good frames is cleared by the next good frame.
fn clears_demotion_streak(delivered: bool, concealed: bool) -> bool {
    delivered || !concealed
}

/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` — the raw flag bit within
/// [`VulkanDecodeDevice::decode_video_caps`] (this crate stays ash-free).
const VIDEO_CODEC_OP_DECODE_H264: u32 = 0x0000_0001;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR` — its H.265 sibling. (AV1 is 0x4
/// and deliberately has no constant here: pf-vkdecode has no AV1 decoder, so the bit
/// would only invite a gate that admits a session nothing can decode.)
const VIDEO_CODEC_OP_DECODE_H265: u32 = 0x0000_0002;

/// The native decoder for a negotiated wire codec, plus the
/// `VkVideoCodecOperationFlagBitsKHR` the presenter's decode family must advertise
/// for it — or `None` for a codec pf-vkdecode cannot decode natively.
///
/// The two are returned together on purpose: "which decoder" and "which caps bit"
/// are one fact, and splitting them is how a gate ends up admitting HEVC on an
/// H.264-only decode family (`vkCreateVideoSessionKHR` for a codec operation the
/// family cannot run is undefined behaviour, not an error). AV1 has a Vulkan decode
/// op and real hardware advertises it — but there is no AV1 decoder in pf-vkdecode,
/// so those sessions must keep falling through to the FFmpeg rungs.
fn native_codec(codec_id: ffmpeg::codec::Id) -> Option<(NativeCodec, u32)> {
    match codec_id {
        ffmpeg::codec::Id::H264 => Some((NativeCodec::H264, VIDEO_CODEC_OP_DECODE_H264)),
        ffmpeg::codec::Id::HEVC => Some((NativeCodec::H265, VIDEO_CODEC_OP_DECODE_H265)),
        _ => None,
    }
}

/// The native Vulkan Video admission gate (WP-C of the native-decode program, widened
/// by the 2026-08-05 ladder decision and again by M3 WP-2's HEVC wiring): the
/// pf-vkdecode backend engages when `choice` asks for it — by name
/// (`PUNKTFUNK_DECODER=native-vulkan` — `choice` is env-first, so that's what carries
/// it) or as the auto family (`auto`/``/`hardware`), where native is the rung
/// immediately ABOVE FFmpeg-Vulkan: WP-D closed with bit-exact parity against
/// libavcodec (250/250 AUs on three drivers, clean 92-minute soak), and the program's
/// goal is dropping FFmpeg from the client, so native goes first wherever the ladder
/// would reach FFmpeg-Vulkan — a native INIT failure falls through to that rung, so
/// admission can't cost a session its decoder at start. A runtime error streak demotes
/// past FFmpeg-Vulkan to VAAPI/D3D11VA/software like every hardware rung's streaks do,
/// with ONE exception: a native backend that never delivered a single frame demotes to
/// FFmpeg-Vulkan first, because a rung the session never actually had must not cost it
/// the rung below (see [`Decoder::decode_frame`]). The explicit `vulkan` pin still
/// names the FFmpeg-Vulkan backend specifically; it — and every other explicit backend
/// pin — refuses.
///
/// Beyond the choice: the negotiated wire codec must be one pf-vkdecode speaks —
/// H.264 or H.265 ([`native_codec`]) — and the presenter's decode family must
/// advertise THAT codec's decode operation. `video_decode` alone proves the extension
/// stack, never the codec: an AV1-only decode family exists on real hardware, and
/// H.264-only ones are the common case on older silicon. AV1 sessions refuse outright.
///
/// What the gate deliberately does NOT check is the stream's picture SHAPE — that is
/// [`NativeVulkanDecoder::new`]'s construction-time probe, which has the negotiated
/// chroma format and bit depth and can ask the device directly. Keeping it there keeps
/// this decision pure (and CPU-testable) while still refusing before a decoder exists.
fn native_vulkan_gate(
    choice: &str,
    codec_id: ffmpeg::codec::Id,
    video_decode: bool,
    decode_video_caps: u32,
) -> bool {
    let Some((_, codec_op)) = native_codec(codec_id) else {
        return false;
    };
    matches!(choice, "native-vulkan" | "auto" | "" | "hardware")
        && video_decode
        && decode_video_caps & codec_op != 0
}

/// Map a negotiated `quic` codec bit to the FFmpeg decoder id the client opens.
pub fn ffmpeg_codec_id(wire: u8) -> ffmpeg::codec::Id {
    match wire {
        punktfunk_core::quic::CODEC_H264 => ffmpeg::codec::Id::H264,
        punktfunk_core::quic::CODEC_AV1 => ffmpeg::codec::Id::AV1,
        _ => ffmpeg::codec::Id::HEVC,
    }
}

/// Select a decoder for `codec_id` that can actually drive `hw_pix_fmt` through
/// `hw_device_ctx` — the open-time capability check every hardware backend needs.
///
/// `avcodec_find_decoder(id)` is NOT that: it returns the registry's FIRST decoder for
/// the id, and upstream orders the native `av1` decoder LAST on purpose ("hwaccel hooks
/// only, so prefer external decoders" — allcodecs.c), behind libdav1d/libaom. The ID
/// lookup therefore hands every AV1 session a pure software decoder that silently
/// ignores `hw_device_ctx` and never calls `get_format`; each frame then fails the
/// backend's hw-format guard and the session burns the demotion ladder MID-STREAM
/// (~1 s per rung — field-logged as 68 Vulkan fails → D3D11VA → 102 fails → software,
/// ~3 s of black) instead of failing here at open in milliseconds. H.264/HEVC never hit
/// this only because their native decoders happen to be registered first.
///
/// The walk mirrors what `avcodec_find_decoder` would do, restricted to decoders whose
/// `avcodec_get_hw_config` advertises the wanted surface via
/// `AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX` — registry order still wins among those,
/// so H.264/HEVC keep selecting exactly the decoder they always did. The error names
/// the decoders that WERE found, so a log reader can tell "this build has no AV1
/// hwaccel at all" from "no AV1 decoder exists, period".
pub(crate) fn find_hw_decoder(
    codec_id: ffmpeg::codec::Id,
    hw_pix_fmt: ffmpeg::ffi::AVPixelFormat,
) -> Result<*const ffmpeg::ffi::AVCodec> {
    use ffmpeg::ffi;
    let want: ffi::AVCodecID = codec_id.into();
    let mut found: Vec<String> = Vec::new();
    // SAFETY: `av_codec_iterate` walks libav's static codec registry (`opaque` is its
    // cursor) and returns static `AVCodec`s; `avcodec_get_hw_config` only reads the
    // codec's own static hw-config table, NULL-terminated by returning null past the end.
    unsafe {
        let mut opaque = std::ptr::null_mut();
        loop {
            let codec = ffi::av_codec_iterate(&mut opaque);
            if codec.is_null() {
                break;
            }
            if (*codec).id != want || ffi::av_codec_is_decoder(codec) == 0 {
                continue;
            }
            for i in 0.. {
                let cfg = ffi::avcodec_get_hw_config(codec, i);
                if cfg.is_null() {
                    break;
                }
                if (*cfg).methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0
                    && (*cfg).pix_fmt == hw_pix_fmt
                {
                    return Ok(codec);
                }
            }
            found.push(
                std::ffi::CStr::from_ptr((*codec).name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    if found.is_empty() {
        bail!("no {codec_id:?} decoder in this FFmpeg build");
    }
    bail!(
        "no {codec_id:?} decoder in this FFmpeg build can drive {hw_pix_fmt:?} via \
         hw_device_ctx (found: {})",
        found.join(", ")
    );
}

/// The name of a registry `AVCodec` (`(*codec).name`), owned — the field every decode
/// log carries so `decoder="av1"` vs `decoder="libdav1d"` is one glance, not a debugger.
///
/// # Safety
/// `codec` must point to a registered `AVCodec` (their `name` is a static NUL-terminated
/// string, valid for the process).
pub(crate) unsafe fn codec_name(codec: *const ffmpeg::ffi::AVCodec) -> String {
    // SAFETY: caller guarantees a registered AVCodec; `name` is its static C string.
    unsafe {
        std::ffi::CStr::from_ptr((*codec).name)
            .to_string_lossy()
            .into_owned()
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
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    if vk.map(|v| v.pyrowave_decode).unwrap_or(false) {
        return bits | punktfunk_core::quic::CODEC_PYROWAVE;
    }
    #[cfg(not(all(any(target_os = "linux", windows), feature = "pyrowave")))]
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

/// Say what `PUNKTFUNK_AU_FAULT` will do to THIS session, once, at decoder
/// construction — including the two cases where the answer is "nothing".
///
/// The knob only bites on the native rung (its injector sits at that backend's
/// decode entry), so a lab run that armed it and landed anywhere else — an FFmpeg
/// rung, a shape the native rung refused, a session that demoted — must be told
/// so. Silence there is indistinguishable from "the fault was injected and
/// nothing detected it", which is precisely the conclusion a fault run exists to
/// make trustworthy. Unset is the normal state and says nothing at all.
fn report_au_fault_env(native_rung: bool) {
    let Ok(spec) = std::env::var("PUNKTFUNK_AU_FAULT") else {
        return;
    };
    if spec.is_empty() {
        return;
    }
    match pf_vkdecode::AuFault::from_spec(&spec) {
        // The native backend logs the arming itself (mode + period), with the
        // decoder it is about to corrupt in hand — no need to say it twice.
        Some(_) if native_rung => {}
        Some(_) => tracing::warn!(
            value = %spec,
            "PUNKTFUNK_AU_FAULT is armed, but this session is NOT on the native \
             Vulkan rung — no AU will be corrupted and no detector will fire"
        ),
        None => tracing::warn!(
            value = %spec,
            "PUNKTFUNK_AU_FAULT not understood (want drop|truncate|flip[:period]) \
             — ignored"
        ),
    }
}

impl Decoder {
    /// `codec_id` is the codec the host resolved in the Welcome (never assume HEVC).
    /// `pref` is the Settings "Video decoder" value (`auto`/`vulkan`/`vaapi`/`d3d11va`/
    /// `software`; `hardware` — the WinUI shell's stored value — reads as auto).
    /// `vk` is the presenter's shared Vulkan device when its stack can run FFmpeg's
    /// Vulkan Video decoder — decode lands as VkImages the presenter samples directly.
    /// Precedence: the `PUNKTFUNK_DECODER` env override wins (support/debug escape
    /// hatch, and the documented knob), then the setting; both default to auto.
    /// Auto's hardware order depends on the device on BOTH desktop OSes
    /// ([`VulkanDecodeDevice::prefer_vulkan_first`]); on H.264 and HEVC sessions the
    /// native pf-vkdecode rung sits immediately above FFmpeg-Vulkan wherever the
    /// ladder reaches it ([`native_vulkan_gate`] — the program is dropping FFmpeg, and
    /// a native INIT failure falls through to FFmpeg-Vulkan). Linux: native → Vulkan →
    /// VAAPI → software on NVIDIA and ALL AMD (`prefer_vulkan_first` is vendor-wide —
    /// desktop RADV included, on-glass verdict — not just the Deck's VanGogh);
    /// VAAPI → native → Vulkan → software on Intel/unknown. Windows (no VAAPI
    /// there): native → Vulkan → D3D11VA → software on NVIDIA/AMD, D3D11VA →
    /// native → Vulkan → software on Intel/unknown (Intel's driver advertises Vulkan
    /// Video, but FFmpeg-Vulkan on it strobes/overruns the budget — B580 field
    /// report).
    ///
    /// `stream` is the picture shape the host resolved ([`StreamFormat`]). Only the
    /// native rung reads it — as its construction-time device probe — because it is
    /// the one backend whose support for a shape is a per-device fact the ladder must
    /// learn BEFORE it commits (FFmpeg's rungs open a codec and discover the pool
    /// format themselves).
    pub fn new(
        codec_id: ffmpeg::codec::Id,
        pref: &str,
        vk: Option<&VulkanDecodeDevice>,
        stream: StreamFormat,
    ) -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        quiet_ffmpeg_log();
        let choice = std::env::var("PUNKTFUNK_DECODER")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| pref.to_string());
        #[cfg(windows)]
        let (d3d11_import, adapter_luid, d3d11_hdr10) = (
            vk.is_some_and(|v| v.d3d11_import),
            vk.and_then(|v| v.adapter_luid),
            vk.is_some_and(|v| v.d3d11_hdr10),
        );
        let done = |backend: Backend| {
            // Whatever rung this session landed on, say what `PUNKTFUNK_AU_FAULT`
            // is going to do about it — see [`report_au_fault_env`]. Here, at the
            // one exit every backend leaves through, rather than in the native
            // backend's constructor: a lab run whose session never REACHES that
            // constructor (an FFmpeg rung, a refused shape, a demotion) would
            // otherwise sit silently un-faulted and read as a fault run that
            // detected nothing.
            report_au_fault_env(matches!(backend, Backend::NativeVulkan(_)));
            Ok(Decoder {
                backend,
                codec_id,
                vaapi_fails: 0,
                first_fail: None,
                want_keyframe: false,
                delivered: false,
                vk: vk.cloned(),
                #[cfg(windows)]
                d3d11_import,
                #[cfg(windows)]
                adapter_luid,
                #[cfg(windows)]
                d3d11_hdr10,
            })
        };
        // Native Vulkan Video (pf-vkdecode), pinned by name (`PUNKTFUNK_DECODER=
        // native-vulkan`). Since the 2026-08-05 ladder decision native is ALSO an
        // auto rung (below, immediately above FFmpeg-Vulkan); the pin stays as the
        // support/debug escape hatch that skips the vendor-ordered rungs ahead of
        // it. Any refusal or init failure logs and DEMOTES to the standard ladder
        // below exactly as if the native rung errored (choice reads as `auto` from
        // here on) — a native failure must never be quieter, or land somewhere
        // other, than the FFmpeg rungs' failures do.
        let mut choice = choice;
        let mut native_tried = false;
        if choice == "native-vulkan" {
            if native_vulkan_gate(
                &choice,
                codec_id,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            ) {
                native_tried = true;
                let vk = vk.expect("gate demands video_decode, so vk is Some");
                let (codec, _) = native_codec(codec_id).expect("the gate admitted this codec");
                match NativeVulkanDecoder::new(vk, codec, stream) {
                    Ok(n) => {
                        tracing::info!(
                            ?codec_id,
                            "native Vulkan Video hardware decode active \
                             (pf-vkdecode, presenter-shared device)"
                        );
                        return done(Backend::NativeVulkan(Box::new(n)));
                    }
                    Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                        "native Vulkan decode init failed — demoting to the standard ladder"),
                }
            } else {
                tracing::warn!(
                    ?codec_id,
                    video_decode = vk.is_some_and(|v| v.video_decode),
                    "PUNKTFUNK_DECODER=native-vulkan refused (needs an H.264 or HEVC session \
                     and a presenter device whose decode family advertises that codec) — \
                     standard ladder"
                );
            }
            choice = "auto".to_string();
        }
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
                .is_some_and(|v| v.prefer_vulkan_first())
        {
            vaapi_tried = true;
            match VaapiDecoder::new(codec_id) {
                Ok(v) => {
                    tracing::info!(
                        ?codec_id,
                        decoder = v.name(),
                        "VAAPI hardware decode active (zero-copy dmabuf)"
                    );
                    return done(Backend::Vaapi(v));
                }
                Err(e) => {
                    tracing::info!(reason = %e, "VAAPI unavailable — trying Vulkan Video");
                }
            }
        }
        // Windows `auto`: D3D11VA FIRST unless this device is one where Vulkan Video is
        // the established right answer (NVIDIA/AMD). Intel's Windows driver advertises
        // Vulkan Video (Arc drivers since 2023) so the capability gate alone no longer
        // keeps Intel off FFmpeg-Vulkan — and that combination is field-broken (B580,
        // 2026-07: strobing between clean anchors and corrupt inter frames that never
        // trips the error-streak demotion, 7 ms p50 decodes blowing the 120 Hz budget)
        // where D3D11VA — the DXVA path every Windows video player exercises, and what
        // this backend was built for — streams clean. Vulkan stays reachable below by
        // explicit preference and as auto's fallback when D3D11VA can't be built.
        #[cfg(windows)]
        let mut d3d11_tried = false;
        #[cfg(windows)]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
        {
            if let Some(v) = vk.filter(|v| v.d3d11_import) {
                d3d11_tried = true;
                match crate::video_d3d11::D3d11vaDecoder::new(
                    codec_id,
                    v.adapter_luid,
                    v.d3d11_hdr10,
                ) {
                    Ok(d) => {
                        tracing::info!(
                            ?codec_id,
                            decoder = d.name(),
                            "D3D11VA hardware decode active (shared-texture hand-off)"
                        );
                        return done(Backend::D3d11va(d));
                    }
                    Err(e) => {
                        tracing::info!(reason = %format!("{e:#}"),
                            "D3D11VA unavailable — trying Vulkan Video");
                    }
                }
            }
        }
        // Native Vulkan Video (pf-vkdecode) — auto's rung immediately ABOVE
        // FFmpeg-Vulkan (2026-08-05 ladder decision: WP-D closed with bit-exact parity
        // against libavcodec on three drivers and a clean soak, and the program's goal
        // is dropping FFmpeg from the client entirely — so wherever auto would reach
        // FFmpeg-Vulkan, native goes first). [`native_vulkan_gate`] carries the whole
        // decision, including the choice: the explicit `vulkan` pin is NOT this rung —
        // it names the FFmpeg-Vulkan backend specifically and keeps meaning exactly
        // that. An init failure logs and falls through to FFmpeg-Vulkan below, so
        // admission can never cost a session hardware decode it had before.
        // (`native_tried` skips the repeat when the pin above already attempted — and
        // failed — the same construction.)
        if !native_tried
            && native_vulkan_gate(
                &choice,
                codec_id,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            )
        {
            let vk = vk.expect("gate demands video_decode, so vk is Some");
            let (codec, _) = native_codec(codec_id).expect("the gate admitted this codec");
            match NativeVulkanDecoder::new(vk, codec, stream) {
                Ok(n) => {
                    tracing::info!(
                        ?codec_id,
                        "native Vulkan Video hardware decode active \
                         (pf-vkdecode auto rung, presenter-shared device)"
                    );
                    return done(Backend::NativeVulkan(Box::new(n)));
                }
                Err(e) => tracing::info!(reason = %format!("{e:#}"),
                    "native Vulkan decode unavailable — trying FFmpeg Vulkan Video"),
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
                            decoder = v.name(),
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
                    tracing::info!(
                        ?codec_id,
                        decoder = v.name(),
                        "VAAPI hardware decode active (zero-copy dmabuf)"
                    );
                    return done(Backend::Vaapi(v));
                }
                Err(e) => {
                    if choice == "vaapi" {
                        return Err(e.context("PUNKTFUNK_DECODER=vaapi but VAAPI failed"));
                    }
                    tracing::warn!(error = %e, "VAAPI unavailable — falling back to software decode");
                }
            }
        }
        // Windows: D3D11VA as the fallback rung for NVIDIA/AMD auto (Vulkan Video missing
        // or failed to open) and the explicit `d3d11va` preference — gated on the presenter
        // having the win32 external-memory import path, else its frames could never reach
        // the screen. (On Intel/unknown auto it was already tried above — `d3d11_tried`
        // skips the repeat.)
        #[cfg(windows)]
        if choice != "software" && choice != "vulkan" && !d3d11_tried {
            match vk.filter(|v| v.d3d11_import) {
                Some(v) => {
                    match crate::video_d3d11::D3d11vaDecoder::new(
                        codec_id,
                        v.adapter_luid,
                        v.d3d11_hdr10,
                    ) {
                        Ok(d) => {
                            tracing::info!(
                                ?codec_id,
                                decoder = d.name(),
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
    /// the pump's decode-stat measurement. `false` = not a Vulkan backend, timeout, or
    /// (native rung) a pair no longer in the shipped ledger / a stale session
    /// generation — every false just declines the sample.
    pub fn wait_hw_decoded(&self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        match &self.backend {
            Backend::Vulkan(v) => v.wait_timeline(timeline_sem, value, timeout_ns),
            Backend::NativeVulkan(d) => d.wait_timeline(timeline_sem, value, timeout_ns),
            _ => false,
        }
    }

    /// This session's decode-integrity counters, or `None` on a backend that has
    /// no way to answer (every FFmpeg rung and PyroWave — see [`DecodeHealth`]).
    ///
    /// `None` and `Some(DecodeHealth::default())` are deliberately different
    /// answers, and the stats surface must keep them different: the first is "this
    /// decoder cannot see corruption", the second is "this decoder looked and saw
    /// none". Reporting the first as the second is exactly the mistake that let a
    /// field corruption run undetected for a release.
    pub fn decode_health(&self) -> Option<DecodeHealth> {
        match &self.backend {
            Backend::NativeVulkan(d) => Some(d.health()),
            _ => None,
        }
    }

    /// The DECODE-order ordinal of the newest picture this lane has planned — the
    /// watermark a caller stamps when it arms a post-loss freeze, so it can tell a
    /// frame decoded before the loss from one decoded after it (see
    /// [`NativeVkFrame::decode_order`]). 0 on every lane that has no bitstream
    /// parser of its own, which is also every lane that reports no local recovery.
    pub fn decode_order(&self) -> u64 {
        match &self.backend {
            Backend::NativeVulkan(d) => d.decode_order(),
            _ => 0,
        }
    }

    /// Drain the "please ask the host for an IDR" flag — the pump calls this each iteration
    /// (throttled) so a demoted/erroring decoder can resynchronize under the infinite GOP.
    /// Open a PyroWave decoder for a `CODEC_PYROWAVE` session (plan §4.5): pyrowave
    /// compute on the presenter's device, no FFmpeg. `codec_id` is irrelevant (kept as
    /// HEVC so an — impossible — demotion path stays well-formed).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pub fn new_pyrowave(
        vk: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<Decoder> {
        // Never the native rung — see [`report_au_fault_env`].
        report_au_fault_env(false);
        Ok(Decoder {
            backend: Backend::PyroWave(Box::new(crate::video_pyrowave::PyroWaveDecoder::new(
                vk,
                width,
                height,
                shard_payload,
                chroma444,
                color,
                hdr16,
            )?)),
            codec_id: ffmpeg::codec::Id::HEVC,
            vaapi_fails: 0,
            first_fail: None,
            want_keyframe: false,
            delivered: false,
            // A PyroWave session never demotes (nothing else decodes it — a failure
            // renegotiates the codec instead), so the demotion-rebuild facts (the
            // device here, the D3D11VA ones below) are unused; keep them well-formed
            // rather than plumbing them in for nothing.
            vk: None,
            #[cfg(windows)]
            d3d11_import: false,
            #[cfg(windows)]
            adapter_luid: None,
            #[cfg(windows)]
            d3d11_hdr10: false,
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
        self.first_fail = None;
        self.delivered = false;
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
        self.decode_frame(au, 0, true)
    }

    /// [`decode`](Self::decode) with the AU's wire facts: `user_flags` (chunk-aligned AUs
    /// are parsed in shard windows — [`punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED`])
    /// and completeness (`false` = a partial delivery; only the PyroWave backend decodes
    /// those — as one frame of localized blur, plan §4.4).
    pub fn decode_frame(
        &mut self,
        au: &[u8],
        // Only the PyroWave backend reads the flags; without that feature the param is unused.
        #[cfg_attr(
            not(all(any(target_os = "linux", windows), feature = "pyrowave")),
            allow(unused_variables)
        )]
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        // Did THIS AU come back as a concealment — an `Ok(None)` the native rung
        // produced because the picture was damaged, not because the decoder was
        // buffering? Only the native rung can answer, and the answer decides
        // whether the `Ok` below is allowed to clear the demotion streak.
        let mut concealed = false;
        let result = match &mut self.backend {
            Backend::Vulkan(v) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                v.decode(au).map(|f| f.map(DecodedImage::VkFrame))
            }
            Backend::NativeVulkan(n) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = n.decode(au).map(|f| f.map(DecodedImage::NativeVk));
                // STREAM damage is not a decoder fault, and must not ride the
                // demotion streak.
                //
                // This distinction only exists on the native rung, because it is
                // the only one that can SEE damage — and that is precisely what
                // makes it dangerous. An FFmpeg rung conceals a lost reference
                // silently and keeps its job; if the native rung turned the same
                // event into an error, three of them over a second would demote
                // the program's own headline decoder exactly on the lossy links it
                // was built to diagnose. So concealment comes back as `Ok(None)`
                // plus this flag: the pump still asks for a re-anchor at the same
                // moment and through the same throttle it always did, and the
                // hardware rung survives the loss that caused it.
                //
                // A driver `RESULT_STATUS` verdict of Failed is NOT routed here —
                // it stays an `Err` below. That one really is a statement about
                // the decoder ("I could not decode what I was given"), and a
                // driver making it repeatedly is the exact case demotion exists
                // for; it is also the Xbox Ally X shape.
                if n.take_recovery_request() {
                    self.want_keyframe = true;
                    concealed = true;
                }
                r
            }
            #[cfg(target_os = "linux")]
            Backend::Vaapi(v) => v.decode(au).map(|f| f.map(DecodedImage::Dmabuf)),
            #[cfg(windows)]
            Backend::D3d11va(d) => d.decode(au).map(|f| f.map(DecodedImage::D3d11)),
            // No demote ladder below PyroWave (nothing else decodes it): propagate the
            // error; the pump surfaces it and the session falls back to HEVC by
            // renegotiation (plan §4.6), not by decoder swap.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Backend::PyroWave(p) => {
                let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
                return Ok(p
                    .decode_frame(au, aligned, complete)?
                    .map(DecodedImage::PyroWave));
            }
            Backend::Software(s) => return Ok(s.decode(au)?.map(DecodedImage::Cpu)),
        };
        match result {
            Ok(f) => {
                // Only an answer that PROVES the rung works may clear the streak —
                // see [`clears_demotion_streak`] for the whole argument.
                if clears_demotion_streak(f.is_some(), concealed) {
                    self.vaapi_fails = 0;
                    self.first_fail = None;
                }
                self.delivered |= f.is_some();
                Ok(f)
            }
            Err(e) => {
                let which = match self.backend {
                    Backend::Vulkan(_) => "Vulkan Video",
                    Backend::NativeVulkan(_) => "native Vulkan Video",
                    #[cfg(windows)]
                    Backend::D3d11va(_) => "D3D11VA",
                    _ => "VAAPI",
                };
                self.vaapi_fails += 1;
                self.want_keyframe = true;
                let first = *self.first_fail.get_or_insert_with(std::time::Instant::now);
                if self.vaapi_fails >= VAAPI_DEMOTE_AFTER && first.elapsed() >= HW_DEMOTE_MIN_STREAK
                {
                    // A NATIVE rung that never delivered a single frame is not a
                    // failing decoder — it is a decoder the session never had, and
                    // the cause is almost always a stream shape THIS DEVICE cannot
                    // host (`NativeVulkanDecoder::new`'s probe catches the ones the
                    // negotiation can see; a level above the device's `maxLevelIdc`,
                    // or an SPS that disagrees with the Welcome, only surface here).
                    // Demoting past FFmpeg-Vulkan for that would cost the session the
                    // rung it would have run on before this backend existed — on
                    // NVIDIA/Linux, where VAAPI is unusable, that means a 4K HEVC
                    // session on SOFTWARE. So the first streak in this state falls
                    // through to FFmpeg-Vulkan, exactly where a construction failure
                    // would have landed. Once a frame HAS been delivered the rung is
                    // proven and its streaks demote like every other Vulkan rung's.
                    if !self.delivered && matches!(self.backend, Backend::NativeVulkan(_)) {
                        // `take`: this arm is one-shot by construction (the native
                        // backend is gone after it), and taking is also what lets the
                        // rebuild borrow the device while `self.backend` is assigned.
                        if let Some(v) = self.vk.take().filter(|v| v.video_decode) {
                            match VulkanDecoder::new(self.codec_id, &v) {
                                Ok(fallback) => {
                                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                                        decoder = fallback.name(),
                                        "native Vulkan Video never delivered a frame — \
                                         demoting to FFmpeg Vulkan Video");
                                    self.backend = Backend::Vulkan(fallback);
                                    self.vaapi_fails = 0;
                                    self.first_fail = None;
                                    self.delivered = false;
                                    return Ok(None);
                                }
                                Err(fe) => tracing::info!(reason = %format!("{fe:#}"),
                                    "FFmpeg Vulkan Video unavailable for demotion — \
                                     continuing down the ladder"),
                            }
                        }
                    }
                    // A failing Vulkan backend (FFmpeg or native — the native rung
                    // demotes exactly like the FFmpeg one) still has a hardware rung
                    // below it on Linux — demote to VAAPI first (user-reported:
                    // FFmpeg-Vulkan-on-Mesa error-streaking where VAAPI streams
                    // perfectly); only when that can't be built either does the
                    // session land on software.
                    #[cfg(target_os = "linux")]
                    if matches!(self.backend, Backend::Vulkan(_) | Backend::NativeVulkan(_)) {
                        match VaapiDecoder::new(self.codec_id) {
                            Ok(v) => {
                                tracing::warn!(error = %e, fails = self.vaapi_fails,
                                    decoder = v.name(),
                                    "Vulkan Video decode failing repeatedly — demoting to VAAPI");
                                self.backend = Backend::Vaapi(v);
                                self.vaapi_fails = 0;
                                self.first_fail = None;
                                self.delivered = false;
                                return Ok(None);
                            }
                            Err(va) => tracing::info!(reason = %va,
                                "VAAPI unavailable for demotion — software decode"),
                        }
                    }
                    // Windows' hardware rung below Vulkan (FFmpeg or native) is D3D11VA
                    // (a 4K120 stream is not survivable on software) — same-GPU rebuild
                    // via the stashed LUID.
                    #[cfg(windows)]
                    if matches!(self.backend, Backend::Vulkan(_) | Backend::NativeVulkan(_))
                        && self.d3d11_import
                    {
                        match crate::video_d3d11::D3d11vaDecoder::new(
                            self.codec_id,
                            self.adapter_luid,
                            self.d3d11_hdr10,
                        ) {
                            Ok(d) => {
                                tracing::warn!(error = %e, fails = self.vaapi_fails,
                                    decoder = d.name(),
                                    "Vulkan Video decode failing repeatedly — demoting to D3D11VA");
                                self.backend = Backend::D3d11va(d);
                                self.vaapi_fails = 0;
                                self.first_fail = None;
                                self.delivered = false;
                                return Ok(None);
                            }
                            Err(dx) => tracing::info!(reason = %dx,
                                "D3D11VA unavailable for demotion — software decode"),
                        }
                    }
                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                        "{which} decode failing repeatedly — demoting to software");
                    self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
                    self.vaapi_fails = 0;
                    self.first_fail = None;
                    self.delivered = false;
                } else {
                    tracing::debug!(backend = which, error = %e,
                        "decode error — requesting keyframe, keeping hardware decode");
                }
                Ok(None)
            }
        }
    }
}

// -EAGAIN. FFmpeg uses POSIX errno values on both our targets (MinGW's EAGAIN is 11 too).
pub(crate) const AVERROR_EAGAIN: i32 = -11;

pub(crate) fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
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
    /// 0x8086 Intel) — drives [`Self::prefer_vulkan_first`].
    pub vendor_id: u32,
    /// The driver's device-name string (e.g. "AMD RADV VANGOGH") — the VanGogh/Deck
    /// detection for [`Self::prefer_vulkan_first`].
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
    /// The presenter has REAL on-glass present timing (`VK_KHR_present_wait` — its
    /// `PresentTimer` runs). Gates the `CLIENT_CAP_PHASE_LOCK` advertisement: without a
    /// true latch stamp the desktop has no latch grid and must not claim the cap.
    pub present_timing: bool,
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
    /// The presenter can also import the RGB10A2 hand-off texture AND offers an HDR10
    /// swapchain — the D3D11VA backend emits its HDR (RGB10 PQ pass-through) ring flavor
    /// for PQ streams instead of tone-mapping to sRGB. Always `false` off Windows.
    pub d3d11_hdr10: bool,
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
    /// Should `auto` try Vulkan Video BEFORE the platform's other hardware path (VAAPI on
    /// Linux, D3D11VA on Windows) on this device?
    ///  * **NVIDIA** — Vulkan Video is the proven path (on Linux the only one: no usable
    ///    VAAPI — the nvidia-vaapi-driver is broken for this, Moonlight blacklists it;
    ///    on Windows it's the validated zero-copy default, 4K@144 with 0.1 ms decode).
    ///  * **AMD (RADV, VanGogh included)** — Vulkan decode outperforms VAAPI on RADV
    ///    (on-glass verdict), and on VanGogh VAAPI's separate-plane dmabuf import
    ///    additionally shows chroma fringing; the session binary opts RADV into
    ///    `video_decode` precisely to get the Vulkan path. Vulkan-first is safe here
    ///    because a mid-session Vulkan failure streak demotes to VAAPI (not software),
    ///    so a broken Mesa Vulkan path still lands on the working driver.
    ///
    /// Intel and unknown vendors take the battle-tested path first: VAAPI on Linux (ANV's
    /// Vulkan Video is the least-proven Mesa path), D3D11VA on Windows — Intel's Windows
    /// driver advertises Vulkan Video (Arc drivers since 2023), but FFmpeg-Vulkan on it is
    /// field-broken (B580, 2026-07: strobing + ~7 ms decodes) where DXVA streams clean.
    pub fn prefer_vulkan_first(&self) -> bool {
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
pub(crate) fn drm_fourcc_for(sw: ffmpeg_next::ffi::AVPixelFormat) -> Option<u32> {
    use ffmpeg_next::ffi::AVPixelFormat::*;
    Some(match sw {
        AV_PIX_FMT_NV12 => fourcc(b'N', b'V', b'1', b'2'),
        AV_PIX_FMT_P010LE => fourcc(b'P', b'0', b'1', b'0'),
        // Full-chroma 4:4:4 semi-planar (HEVC RExt decode on drivers that export it as
        // two planes) — the presenter imports the full-size chroma plane like any other.
        AV_PIX_FMT_NV24 => fourcc(b'N', b'V', b'2', b'4'),
        _ => return None,
    })
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
            present_timing: false,
            d3d11_import: false,
            d3d11_hdr10: false,
            adapter_luid: None,
            queue_lock: std::sync::Arc::new(QueueLock::new()),
        }
    }

    /// The demotion streak's escape hatch, stated as the invariant it is: an `Ok`
    /// clears the streak only when it PROVES the rung works.
    ///
    /// Concealment (`Ok(None)` with a recovery request) proves nothing — it is the
    /// STREAM that was damaged — and before M4's review it cleared the streak
    /// anyway, because the `Ok(_)` arm matched `Ok(None)` too. Two shapes followed
    /// from that, and this test pins both away:
    ///
    /// * a driver failing every other AU on a lossy link: `Err` / concealment /
    ///   `Err` / concealment … the concealment zeroed the count and
    ///   [`VAAPI_DEMOTE_AFTER`] was never reached;
    /// * and a rung that conceals forever and ships nothing: a frozen picture with
    ///   no path down the ladder at all.
    #[test]
    fn only_an_answer_that_proves_the_rung_works_clears_the_demotion_streak() {
        // A shipped frame is proof, concealed or not (the AU carried damage AND a
        // picture — the decoder is plainly alive).
        assert!(clears_demotion_streak(true, false));
        assert!(clears_demotion_streak(true, true));
        // A CLEAN no-output AU is proof too: the decoder ran and objected to
        // nothing (it buffered, or skipped an H.265 RASL picture after an open-GOP
        // join). Treating that as suspicious would demote healthy sessions.
        assert!(clears_demotion_streak(false, false));
        // Concealment with no picture is the one that proves nothing.
        assert!(!clears_demotion_streak(false, true));

        // The streak arithmetic that follows, spelled out on the milder and
        // likelier shape: a broken driver alternating with concealment must still
        // reach the demotion threshold.
        let mut fails = 0u32;
        for concealed_ok in [false, true, false, true, false] {
            if concealed_ok {
                if clears_demotion_streak(false, true) {
                    fails = 0;
                }
            } else {
                fails += 1; // an Err from the driver's own verdict
            }
        }
        assert!(
            fails >= VAAPI_DEMOTE_AFTER,
            "three driver errors interleaved with concealment must still reach the \
             demotion threshold — they got to {fails}"
        );
    }

    /// Auto's hardware order (both OSes): Vulkan-first on NVIDIA (on Linux: no usable
    /// VAAPI) and ALL AMD (Vulkan decode outperforms VAAPI on RADV — on-glass verdict;
    /// VanGogh additionally chroma-fringes over VAAPI); Intel/unknown take the proven
    /// path first — VAAPI on Linux (ANV's Vulkan Video is the least-proven Mesa path),
    /// D3D11VA on Windows (Intel's driver advertises Vulkan Video since 2023, but
    /// FFmpeg-Vulkan on it strobes — B580 field report). A Vulkan failure streak still
    /// demotes to hardware (VAAPI/D3D11VA), so Vulkan-first can never strand a box on
    /// software decode.
    #[test]
    fn vulkan_first_on_nvidia_and_amd_only() {
        assert!(decode_device(0x10DE, "NVIDIA GeForce RTX 5070 Ti").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD RADV VANGOGH").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Custom GPU 0405 (RADV VANGOGH)").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Radeon RX 7800 XT (RADV NAVI32)").prefer_vulkan_first());
        assert!(
            !decode_device(0x8086, "Intel(R) Arc(tm) A770 Graphics (DG2)").prefer_vulkan_first()
        );
        // The Windows-side motivation: discrete Arc advertises Vulkan Video and must
        // still land on D3D11VA in auto.
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) B580 Graphics").prefer_vulkan_first());
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) Pro Graphics").prefer_vulkan_first());
    }

    /// The native-Vulkan admission gate (WP-C, widened by the 2026-08-05 ladder
    /// decision and again by M3 WP-2's HEVC wiring): the pin AND the auto family
    /// admit on a capable H.264 or HEVC session — native sits immediately above
    /// FFmpeg-Vulkan because the program is dropping FFmpeg — while every explicit
    /// backend pin refuses (`vulkan` names the FFmpeg-Vulkan backend specifically and
    /// must keep meaning exactly that), and the codec/device legs still refuse for
    /// every choice. The codec's OWN caps bit is the device leg: admitting HEVC on an
    /// H.264-only decode family would create a video session for an operation the
    /// family cannot run, which is undefined behaviour rather than an error.
    #[test]
    fn native_vulkan_gate_admits_pin_and_auto_family_per_codec_on_a_capable_family() {
        use ffmpeg::codec::Id;
        // Pin the raw spec values, not the implementation constants — a typo'd bit
        // would refuse every real driver's caps and native would silently never
        // engage (the program's own nb_queries=0 lesson: silent non-engagement is
        // the failure mode nothing flags).
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H264, 0x1,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR"
        );
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H265, 0x2,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR"
        );
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const H265_OP: u32 = VIDEO_CODEC_OP_DECODE_H265;
        // `VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR` — a real bit on real
        // hardware, and never enough on its own (no AV1 decoder exists here).
        const AV1_OP: u32 = 0x4;
        for choice in ["native-vulkan", "auto", "", "hardware"] {
            // The pin and the whole auto family admit both codecs pf-vkdecode
            // speaks, on a family that advertises the matching op…
            assert!(
                native_vulkan_gate(choice, Id::H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, Id::HEVC, true, H265_OP),
                "{choice:?}"
            );
            // …including the ordinary case of a family that runs both.
            assert!(
                native_vulkan_gate(choice, Id::H264, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, Id::HEVC, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            // Each codec needs ITS OWN bit: an H.264-only family (the common case on
            // older silicon) must not take an HEVC session, and vice versa.
            assert!(
                !native_vulkan_gate(choice, Id::HEVC, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, Id::H264, true, H265_OP),
                "{choice:?}"
            );
            // AV1 refuses whatever the family advertises — pf-vkdecode has no AV1
            // decoder, so the session must fall through to the FFmpeg rungs.
            assert!(
                !native_vulkan_gate(choice, Id::AV1, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, Id::AV1, true, H264_OP | H265_OP | AV1_OP),
                "{choice:?}"
            );
            // No Vulkan-Video-capable presenter device.
            assert!(
                !native_vulkan_gate(choice, Id::H264, false, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, Id::HEVC, false, H265_OP),
                "{choice:?}"
            );
            // A decode family advertising NO codec op, or only a foreign one,
            // refuses even with the extension stack present — the caps BIT is the
            // codec gate, not `video_decode`.
            assert!(!native_vulkan_gate(choice, Id::H264, true, 0), "{choice:?}");
            assert!(!native_vulkan_gate(choice, Id::HEVC, true, 0), "{choice:?}");
            assert!(
                !native_vulkan_gate(choice, Id::H264, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, Id::HEVC, true, AV1_OP),
                "{choice:?}"
            );
        }
        // Never for an explicit OTHER-backend pin, capable device or not.
        for choice in ["vulkan", "vaapi", "d3d11va", "software"] {
            assert!(
                !native_vulkan_gate(choice, Id::H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, Id::HEVC, true, H265_OP),
                "{choice:?}"
            );
        }
        // The decoder the gate implies — the construction sites `expect()` this
        // exact agreement, so a codec admitted with no decoder behind it would be a
        // panic rather than a demotion.
        assert_eq!(
            native_codec(Id::H264).map(|(c, _)| c),
            Some(NativeCodec::H264)
        );
        assert_eq!(
            native_codec(Id::HEVC).map(|(c, _)| c),
            Some(NativeCodec::H265)
        );
        assert!(native_codec(Id::AV1).is_none());
        assert!(native_codec(Id::VP9).is_none());
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
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV24),
            Some(0x3432_564e)
        );
        assert_eq!(
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_RGBA),
            None
        );
    }
}
