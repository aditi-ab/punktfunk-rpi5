//! Video decode: reassembled access units → frames for the presenter.
//!
//! Ladder: Vulkan Video, D3D11VA/VAAPI, then openh264 or rav1d. [`Decoder::new`]
//! orders rungs by platform and GPU vendor. [`native_evidence`] records which
//! pairs have hardware validation; [`native_rung_admitted`] yields an unverified
//! rung only to a verified rung usable on the same device. `PUNKTFUNK_DECODER`
//! pins skip that rule and still fall through after init failure.
//! [`migrate_decoder_pref`] rewrites stored libavcodec names onto native pins.
//!
//! Hosts emit zero-reorder streams: one AU in, one picture out. The CPU rung
//! has no HEVC decoder; [`last_rung_verdict`] reconnects with a codec this
//! build can finish. Evidence: tests in this file.

// Windows-only: the D3D11VA pin bails when win32 external-memory import is missing.
#[cfg(windows)]
use anyhow::bail;
use anyhow::Result;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub use crate::video_color::{csc_rows, ColorDesc};
/// Re-export so SESSION (and its tests) can name the refusal by type.
/// The module stays private, like every other backend.
pub use crate::video_software::NoSoftwareRung;
use crate::video_software::SoftwareDecoder;
use crate::video_vk_native::{NativeCodec, NativeVulkanDecoder};

/// One decoded frame. `pts_ns` is the host capture timestamp for
/// capture→displayed latency at present time.
pub struct DecodedFrame {
    /// Host-clock capture pts (ns). Compare to local wall + `clock_offset_ns`
    /// at paintable-set.
    pub pts_ns: u64,
    /// Local wall (ns) when the decoder emitted this image (`decoded` stage).
    /// The presenter subtracts it from its paintable-set stamp for `display`.
    pub decoded_ns: u64,
    pub image: DecodedImage,
}

/// Re-export so the presenter names every frame type through `video::`.
#[cfg(windows)]
pub use crate::video_d3d11::D3d11Frame;

pub enum DecodedImage {
    /// Tightly-packed 8-bit I420 for the presenter's planar CSC upload.
    Cpu(CpuPlanarFrame),
    /// Native VAAPI DRM-PRIME export (`pf-vaadec`). The variant name is the
    /// `stats:` decode-path tag; renaming it would rename that.
    #[cfg(target_os = "linux")]
    NativeDmabuf(DmabufFrame),
    /// Raspberry Pi HEVC V4L2 Request output after NEON SAND detiling.
    #[cfg(target_os = "linux")]
    V4l2Planar(CpuPlanarFrame),
    /// D3D11VA shareable NT-handle texture the presenter imports
    /// (`VK_KHR_external_memory_win32`) on GPUs without Vulkan Video.
    #[cfg(windows)]
    D3d11(crate::video_d3d11::D3d11Frame),
    /// Three R8 plane views on the presenter's device, fence-complete, GENERAL.
    /// Planar CSC samples them as BT.709 limited (the codec's colour contract).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(crate::video_pyrowave::PyroWavePlanarFrame),
    /// pf-vkdecode image + plane views already on the presenter's device.
    /// Format is [`NativeVkFrame::vk_format`], never assumed. The presenter waits
    /// the timeline, samples, restores [`NativeVkFrame::layout`], and drops the
    /// frame to release the slot.
    NativeVk(NativeVkFrame),
}

/// Session-cumulative decode integrity. Only native rungs fill it
/// ([`Decoder::decode_health`] is `None` elsewhere). Counters are monotonic;
/// the stats window diffs them like `frames_dropped`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeHealth {
    /// AUs whose plan needed concealment (missing DPB ref, `frame_num` gap,
    /// truncated NALU walk). Output was released unshown.
    pub damaged: u64,
    /// Frames the driver reported corrupt (`RESULT_STATUS_ONLY`). Distinct from
    /// [`Self::damaged`]: damaged is an incomplete bitstream; failed is hardware
    /// that could not decode what arrived. Structurally 0 where
    /// [`Self::status_queries`] is false — [`Self::note`] still extends [`Self::run`].
    pub failed: u64,
    /// AUs the decoder refused (plan error or session failure). No picture.
    /// Distinct from [`Self::damaged`]: concealment coped; refusal could not run.
    pub refused: u64,
    /// Consecutive AUs with no showable picture; 0 on the next clean AU.
    /// Separates a recovering lossy link (`run 0`) from a stream that went down.
    pub run: u32,
    /// Longest [`Self::run`] of the session. A 1 Hz sample of `run` misses the peak.
    pub worst_run: u32,
    /// Correctly decoded frames discarded because the deliverable queue overflowed.
    /// Not damaged/refused/failed: the stream was fine and a picture showed, so
    /// this must not extend [`Self::run`]. Structurally 0 without a deliverable queue.
    pub dropped: u64,
    /// Per-op decode-status queries (`queryResultStatusSupport`). False on RADV
    /// (a query hangs the VCN ring); [`Self::failed`] then stays 0. Distinguishes
    /// clean from unmeasured.
    pub status_queries: bool,
}

impl DecodeHealth {
    /// Fold one AU's verdict. Damaged, refused, and driver-failed all extend the
    /// run. Where [`Self::status_queries`] is false, a `Failed` read still extends
    /// the run but does not count as [`Self::failed`].
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

    /// One correctly-decoded frame discarded unshown. Separate from [`Self::note`]:
    /// several frames can drop inside one AU that still shipped a picture. Never touches [`Self::run`].
    pub(crate) fn note_dropped(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

/// Raw `VkFormat` code point, carried across the ash-free boundary.
///
/// Newtype, not a bare `i32`: [`NativeVkFrame`] also carries `poc` as `i32`,
/// and passing the wrong one compiles, warns once, and renders as 8-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawVkFormat(pub i32);

/// Picture formats the native decode lane can deliver — pf-vkdecode's
/// [`pf_vkdecode::OUTPUT_FORMATS`], not a copy. Public so the presenter can
/// pin its colour-math table without depending on pf-vkdecode.
pub fn native_picture_formats() -> Vec<RawVkFormat> {
    pf_vkdecode::OUTPUT_FORMATS
        .iter()
        .map(|f| RawVkFormat(f.as_raw()))
        .collect()
}

/// Layout a [`NativeVkFrame`] layer is in when its semaphore signals.
/// Ash-free, so the presenter can transition for sampling without naming `vk::ImageLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeVkLayout {
    /// `VIDEO_DECODE_DST_KHR` — distinct-mode. The next decode into this slot
    /// discards the layer (UNDEFINED old-layout).
    DecodeDst,
    /// `VIDEO_DECODE_DPB_KHR` — coincide-mode DPB slot, possibly still a live
    /// reference. A consumer that samples it must transition it back in the same submit.
    DecodeDpb,
}

/// Token a presented or dropped [`NativeVkFrame`] hands back. `seq` names the
/// frame; a stale `generation` routes to the graveyard. `presented` means the
/// sampling submit enqueued the frame's `value + 1` timeline signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeReleaseToken {
    pub seq: u64,
    pub generation: u64,
    /// Sampling submit (with its `value + 1` signal) was enqueued. `false` when
    /// dropped unpresented (newest-wins, demotion drain, failed submit).
    pub presented: bool,
}

/// Sends [`NativeReleaseToken`] once on drop. The presenter holds the frame
/// until its sampling fence is waited, so drop means the GPU is done. A dead
/// channel (backend demoted or rebuilt) is ignored.
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

    /// Sampling submit, including the frame's `value + 1` timeline signal, was enqueued.
    /// The decoder then waits that write-back.
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

/// One natively decoded frame (pf-vkdecode). Raw `u64` handles; this crate
/// stays ash-free. Valid until the guard drops AND `generation` is current.
/// The backend keeps the decoder alive until every shipped token returns.
pub struct NativeVkFrame {
    /// Raw `VkImage`; the picture occupies array layer [`Self::layer`].
    pub image: u64,
    /// Picture `VkFormat` — what the image was created with and what
    /// [`Self::plane_views`] alias. Never infer from the codec: H.265 format is
    /// the stream's and can change mid-stream. Assuming 8-bit over P010 displays wrong.
    pub vk_format: RawVkFormat,
    /// Per-plane `VkImageView`s for [`Self::vk_format`] — the presenter's planar CSC contract.
    pub plane_views: [u64; 2],
    pub layer: u32,
    /// Layout when the semaphore signals. The presenter must restore it after sampling.
    pub layout: NativeVkLayout,
    /// Timeline pair (raw `VkSemaphore` + value). The presenter waits it on the GPU, never the host.
    pub semaphore: u64,
    pub semaphore_value: u64,
    /// Decoder session generation the handles belong to (rides the release token).
    pub generation: u64,
    /// Display size (conformance-window crop). What [`DecodedImage::dimensions`] reports.
    pub width: u32,
    pub height: u32,
    /// Allocated/coded extent (`>=` display). Scale UVs by display/coded or
    /// alignment padding smears: 1080p pools 1088 rows (multiple of 16).
    pub coded_width: u32,
    pub coded_height: u32,
    /// Crop origin in the coded picture. Hosts emit (0,0); the UV-scale path
    /// assumes that. Carried so a nonzero origin is checkable, not silent.
    pub crop_x: u32,
    pub crop_y: u32,
    /// SPS colour for this picture (VUI → H.273; unspecified is BT.709 limited).
    /// Per frame: the host switches HDR in-band.
    pub color: ColorDesc,
    /// IDR re-anchor. H.265 CRA/BLA does not set this (NALU type). Hosts emit
    /// IDR-only re-entry; a CRA's leading pictures may be undecodable.
    pub keyframe: bool,
    pub poc: i32,
    /// Intra-refresh recovery-point SEI. [`Self::keyframe`] is never set for a
    /// wave, so without this the pump holds the last good picture until its 500 ms
    /// backstop. Fed to [`punktfunk_core::reanchor::ReanchorGate::on_local_recovery`].
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
    /// Every predicted-from picture decoded from a fully-available reference chain.
    /// Corroborates `USER_FLAG_RECOVERY_ANCHOR`: host tracks receipt, this tracks
    /// decode. When they disagree the freeze lifts onto a concealed picture.
    pub references_clean: bool,
    /// Decode-order ordinal (strictly increasing per session). After a failed AU
    /// H.265 flushes its DPB and may deliver pictures decoded before the loss.
    /// The pump stamps this at arm and ignores older [`Self::recovery`].
    pub decode_order: u64,
    /// Sends the release token on drop — see [`NativeReleaseGuard`].
    pub guard: NativeReleaseGuard,
}

impl DecodedImage {
    /// Intra keyframe (IDR) — the pump's post-loss re-anchor. Every rung answers
    /// from the bitstream. Not an intra-refresh recovery point; that is [`Self::local_recovery`].
    pub fn is_keyframe(&self) -> bool {
        match self {
            DecodedImage::Cpu(f) => f.keyframe,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.keyframe,
            #[cfg(target_os = "linux")]
            DecodedImage::V4l2Planar(f) => f.keyframe,
            #[cfg(windows)]
            DecodedImage::D3d11(f) => f.keyframe,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => f.keyframe,
            DecodedImage::NativeVk(f) => f.keyframe,
        }
    }

    /// Intra-refresh recovery-point SEI. Only rungs with their own parser answer
    /// (native Vulkan, CPU H.264). Everyone else reports
    /// [`punktfunk_core::reanchor::LocalRecovery::NONE`]. The CPU rung has no
    /// [`Self::decode_order`]; openh264 is one-AU-in, one-picture-out.
    pub fn local_recovery(&self) -> punktfunk_core::reanchor::LocalRecovery {
        match self {
            DecodedImage::NativeVk(f) => f.recovery,
            DecodedImage::Cpu(f) => f.recovery,
            _ => punktfunk_core::reanchor::LocalRecovery::NONE,
        }
    }

    /// Corroboration for `USER_FLAG_RECOVERY_ANCHOR`. Only a rung that planned
    /// the AU knows its references. Native Vulkan answers; everyone else reports
    /// [`punktfunk_core::reanchor::AnchorEvidence::Unavailable`] — silence is not
    /// refutation. The CPU H.264 frame carries no [`NativeVkFrame::references_clean`].
    pub fn anchor_evidence(&self) -> punktfunk_core::reanchor::AnchorEvidence {
        use punktfunk_core::reanchor::AnchorEvidence;
        match self {
            DecodedImage::NativeVk(f) => {
                if f.references_clean {
                    AnchorEvidence::ReferencesClean
                } else {
                    AnchorEvidence::ReferencesDamaged
                }
            }
            _ => AnchorEvidence::Unavailable,
        }
    }

    /// Decode-order ordinal where the lane knows one — see [`NativeVkFrame::decode_order`]. `None` elsewhere.
    pub fn decode_order(&self) -> Option<u64> {
        match self {
            DecodedImage::NativeVk(f) => Some(f.decode_order),
            _ => None,
        }
    }

    /// Display pixel size. A frame at the target size is the mid-stream-resize
    /// end signal: the new-mode picture is on glass before the host rebuilds.
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DecodedImage::Cpu(f) => (f.width, f.height),
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => (f.width, f.height),
            #[cfg(target_os = "linux")]
            DecodedImage::V4l2Planar(f) => (f.width, f.height),
            #[cfg(windows)]
            DecodedImage::D3d11(f) => (f.width, f.height),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
            DecodedImage::NativeVk(f) => (f.width, f.height),
        }
    }
}

/// Software-decoded 8-bit 4:2:0: Y, Cb, Cr packed back-to-back at each plane's width.
///
/// Tight packing is load-bearing: the presenter uploads with `bufferRowLength = 0`,
/// so a padded row shears the picture. Decoder strides are undone in [`Self::from_i420`].
/// Planes carry the stream's Y′CbCr; the presenter CSC uses [`csc_rows`].
pub struct CpuPlanarFrame {
    pub width: u32,
    pub height: u32,
    /// Y, then Cb, then Cr — see [`Self::plane`].
    data: Vec<u8>,
    /// Byte offset of each plane's first row in [`Self::data`].
    offsets: [usize; 3],
    /// Bitstream colour (not the decoder — see `video_software`). Drives CSC
    /// matrix/range and, for PQ, the presenter's tone-map mode.
    pub color: ColorDesc,
    /// Intra keyframe (IDR) — the pump's post-loss re-anchor. See [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// Intra-refresh recovery (`RecoveryWatch`). [`Self::keyframe`] cannot answer
    /// for a wave. H.264 only; AV1 reports [`punktfunk_core::reanchor::LocalRecovery::NONE`].
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
}

impl CpuPlanarFrame {
    /// Chroma plane size for 4:2:0, rounding up — an odd luma dimension still
    /// has a chroma sample covering its last row/column.
    pub fn chroma_dims(width: u32, height: u32) -> (u32, u32) {
        (width.div_ceil(2), height.div_ceil(2))
    }

    /// Plane `i` (0 = Y, 1 = Cb, 2 = Cr), tightly packed.
    pub fn plane(&self, i: usize) -> &[u8] {
        let (w, h) = self.plane_dims(i);
        let start = self.offsets[i];
        &self.data[start..start + (w * h) as usize]
    }

    /// Plane `i` size in samples: luma is `(width, height)`, chroma is 4:2:0 halves.
    pub fn plane_dims(&self, i: usize) -> (u32, u32) {
        if i == 0 {
            (self.width, self.height)
        } else {
            Self::chroma_dims(self.width, self.height)
        }
    }

    /// Copy strided I420 into one tightly-packed allocation.
    ///
    /// Refuses rather than truncates: a short plane is a geometry disagreement,
    /// and reading the rows that are there would paint uninitialized memory.
    pub(crate) fn from_i420(
        width: u32,
        height: u32,
        planes: [&[u8]; 3],
        strides: [usize; 3],
        color: ColorDesc,
        keyframe: bool,
        recovery: punktfunk_core::reanchor::LocalRecovery,
    ) -> Result<CpuPlanarFrame> {
        anyhow::ensure!(width > 0 && height > 0, "empty picture {width}x{height}");
        let (cw, ch) = Self::chroma_dims(width, height);
        let dims = [(width, height), (cw, ch), (cw, ch)];
        let total: usize = dims.iter().map(|(w, h)| *w as usize * *h as usize).sum();
        let mut data = vec![0u8; total];
        let mut offsets = [0usize; 3];
        let mut at = 0usize;
        for i in 0..3 {
            let (w, h) = (dims[i].0 as usize, dims[i].1 as usize);
            anyhow::ensure!(
                strides[i] >= w,
                "plane {i}: stride {} is narrower than {w} samples",
                strides[i]
            );
            anyhow::ensure!(
                planes[i].len() >= (h - 1) * strides[i] + w,
                "plane {i}: decoder reported {} bytes for {w}x{h} at stride {}",
                planes[i].len(),
                strides[i]
            );
            offsets[i] = at;
            for row in 0..h {
                let src = row * strides[i];
                data[at..at + w].copy_from_slice(&planes[i][src..src + w]);
                at += w;
            }
        }
        Ok(CpuPlanarFrame {
            width,
            height,
            data,
            offsets,
            color,
            keyframe,
            recovery,
        })
    }
}

/// GPU frame: dmabuf fds + plane layout for `GdkDmabufTextureBuilder`.
/// Fds belong to `guard`'s mapped DRM frame; valid until the guard drops.
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    /// Combined DRM fourcc of the whole surface (NV12 for 8-bit VAAPI), from the
    /// decoder's software format — not the per-plane component formats.
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Source colour — `GdkDmabufTexture` state (BT.709 narrow SDR, BT.2020 PQ HDR).
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor. See [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    pub guard: DrmFrameGuard,
}

#[cfg(target_os = "linux")]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Keeps the decoded surface alive until GPU reads finish. Drop returns it
/// to the pool and closes the fds. The presenter dups imported fds and holds
/// the guard until its fence is waited.
#[cfg(target_os = "linux")]
pub struct DrmFrameGuard(
    /// Unread: the type is its `Drop` (close PRIME fds, return the surface).
    /// Removing the field releases the surface at construction; an alias would
    /// leak a pf-vaadec name the presenter must treat as opaque.
    #[allow(dead_code)]
    pub(crate) crate::video_vaapi_native::VaFrameGuard,
);

enum Backend {
    /// pf-vkdecode on the presenter's device. Auto's top rung; pinnable as
    /// `native-vulkan`. Codec is chosen once at construction. Boxed: the decoder is large.
    NativeVulkan(Box<NativeVulkanDecoder>),
    /// Native VAAPI (`pf-vaadec`). Pinnable as `native-vaapi`; `auto` reaches it
    /// in vendor order. Unverified ([`native_evidence`]), so `auto` yields to
    /// proven Vulkan ([`native_rung_admitted`]). Boxed: two planners + pools.
    #[cfg(target_os = "linux")]
    V4l2Request(Box<crate::video_v4l2_request::V4l2RequestDecoder>),
    #[cfg(target_os = "linux")]
    NativeVaapi(Box<crate::video_vaapi_native::NativeVaapiDecoder>),
    /// Native D3D11VA (`pf-dxvadec`): plans into the shareable-RGBA hand-off ring.
    /// Pinnable as `native-d3d11va`; `auto` reaches it. Boxed: two planners + session.
    #[cfg(windows)]
    NativeD3d11va(Box<crate::video_d3d11_native::NativeD3d11Decoder>),
    /// PyroWave compute on the presenter's device. No demotion rung: nothing else
    /// decodes it. Boxed: pinned create-info + plane ring.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(Box<crate::video_pyrowave::PyroWaveDecoder>),
    /// CPU rung (openh264 / rav1d). Last in every ladder, so it never demotes.
    /// The only rung that can fail to exist for a codec: see [`last_rung_verdict`].
    Software(SoftwareDecoder),
}

/// Picture shape the host resolved in Welcome, before any AU arrives.
///
/// The in-band SPS stays authoritative. Available at construction so a
/// device-dependent shape refuses before the rung is chosen, instead of
/// demoting past it on the first decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// [`punktfunk_core::quic::CHROMA_IDC_420`] (1) or
    /// [`punktfunk_core::quic::CHROMA_IDC_444`] (3). A host that omits it reads as 4:2:0, never 0.
    pub chroma_format_idc: u8,
    /// Bits per component: 8, or 10 for Main10/HDR. A host that omits it reads 8.
    pub bit_depth: u8,
}

impl StreamFormat {
    /// 8-bit 4:2:0 — every H.264 session, and what an omitted Welcome shape decodes to.
    pub const SDR_420_8: StreamFormat = StreamFormat {
        chroma_format_idc: punktfunk_core::quic::CHROMA_IDC_420,
        bit_depth: 8,
    };

    /// `bit_depth` as H.265 `bit_depth_luma_minus8`. `None` outside 8/10 is a refusal, not a skipped probe.
    pub(crate) fn bit_depth_minus8(self) -> Option<u8> {
        self.bit_depth.checked_sub(8)
    }
}

pub struct Decoder {
    backend: Backend,
    /// Negotiated `quic::CODEC_*` bit. Every rung map and the software refusal
    /// key on it, so a demotion rebuilds for the same codec.
    wire_codec: u8,
    /// Consecutive hardware decode errors. One transient (a missing reference after loss) must not demote.
    vaapi_fails: u32,
    /// When the current error streak started. Count alone is not enough: a
    /// startup loss burst fails 3+ AUs in milliseconds, before the first-error
    /// IDR (~100–300 ms RTT) can rescue the hardware decoder.
    first_fail: Option<std::time::Instant>,
    /// Needs a fresh IDR (after an error or demotion). The pump drains it; the GOP has no periodic keyframe.
    want_keyframe: bool,
    /// This backend has delivered at least one frame. A never-delivered rung is
    /// one the session never had; its streak must not cost the rung below. Reset on swap.
    delivered: bool,
    /// Presenter's device, so demotion can build native Vulkan mid-stream.
    /// Cloned once per session; handles outlive every pump ([`VulkanDecodeDevice`]).
    vk: Option<VulkanDecodeDevice>,
    /// Negotiated picture shape for a mid-stream native rebuild ([`StreamFormat`]).
    stream: StreamFormat,
    /// Hardware rungs this session has run ([`RUNG_BIT_NATIVE_VULKAN`],
    /// [`RUNG_BIT_NATIVE_PLATFORM`]). The two native rungs sit in opposite vendor
    /// order; without this they could bounce the session. An entered rung is never re-entered.
    entered_rungs: u8,
    /// Presenter can import win32 external memory, so D3D11VA frames reach the screen. Kept for Vulkan→D3D11VA demotion.
    #[cfg(windows)]
    d3d11_import: bool,
    /// Presenter adapter LUID ([`VulkanDecodeDevice::adapter_luid`]) so demotion lands on the same GPU.
    #[cfg(windows)]
    adapter_luid: Option<[u8; 8]>,
    /// [`VulkanDecodeDevice::d3d11_hdr10`], for the same demotion rebuild.
    #[cfg(windows)]
    d3d11_hdr10: bool,
}

/// Native Vulkan ran this session — see [`Decoder::entered_rungs`].
const RUNG_BIT_NATIVE_VULKAN: u8 = 1 << 0;
/// Native platform rung (VAAPI on Linux, D3D11VA on Windows) ran this session.
const RUNG_BIT_NATIVE_PLATFORM: u8 = 1 << 1;

/// [`Decoder::entered_rungs`] bit this backend claims. 0 for rungs that cannot
/// be a demotion target twice (software is terminal; PyroWave never demotes).
fn rung_bit(backend: &Backend) -> u8 {
    match backend {
        Backend::NativeVulkan(_) => RUNG_BIT_NATIVE_VULKAN,
        #[cfg(target_os = "linux")]
        Backend::NativeVaapi(_) => RUNG_BIT_NATIVE_PLATFORM,
        #[cfg(windows)]
        Backend::NativeD3d11va(_) => RUNG_BIT_NATIVE_PLATFORM,
        _ => 0,
    }
}

/// Consecutive decode errors before hardware demotion. A lone transient re-requests an IDR and stays.
const VAAPI_DEMOTE_AFTER: u32 = 3;

/// Minimum streak age before demotion. Every error re-requests an IDR; a
/// successful decode resets the streak. 1 s lets that IDR arrive; a loss burst
/// of consecutive bad AUs must not strand the session on software first.
const HW_DEMOTE_MIN_STREAK: std::time::Duration = std::time::Duration::from_millis(1000);

/// May this `decode` answer clear the demotion streak?
///
/// Clearing it claims the decoder works. A delivered frame proves that, as
/// does a clean `Ok(None)` (buffered, or an H.265 RASL skip). Concealment is
/// not an `Err` but must not clear the streak: interleaved `Err`s would never
/// reach the threshold, and a forever-concealing rung would freeze with no escape.
fn clears_demotion_streak(delivered: bool, concealed: bool) -> bool {
    delivered || !concealed
}

/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` — raw flag in
/// [`VulkanDecodeDevice::decode_video_caps`] (this crate stays ash-free).
const VIDEO_CODEC_OP_DECODE_H264: u32 = 0x0000_0001;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR`.
const VIDEO_CODEC_OP_DECODE_H265: u32 = 0x0000_0002;

/// `VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR`. What [`av1_hardware_decodable`]
/// reads and the caps bit [`native_codec`] demands for an AV1 session.
const VIDEO_CODEC_OP_DECODE_AV1: u32 = 0x0000_0004;

/// Native decoder for a wire codec plus the `VkVideoCodecOperationFlagBitsKHR`
/// the decode family must advertise, or `None` if pf-vkdecode cannot decode it.
///
/// Returned together: splitting them admits HEVC on an H.264-only family
/// (`vkCreateVideoSessionKHR` for an unsupported op is UB, not an error).
/// Presence here is "pf-vkdecode has a decoder", not auto admission ([`native_vulkan_gate`]).
fn native_codec(wire: u8) -> Option<(NativeCodec, u32)> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some((NativeCodec::H264, VIDEO_CODEC_OP_DECODE_H264)),
        punktfunk_core::quic::CODEC_HEVC => Some((NativeCodec::H265, VIDEO_CODEC_OP_DECODE_H265)),
        punktfunk_core::quic::CODEC_AV1 => Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1)),
        _ => None,
    }
}

/// Native DXVA decoder for a wire codec, or `None`. No caps bit: DXVA
/// advertises a profile GUID, which [`crate::video_d3d11_native::NativeD3d11Decoder::new`] checks.
#[cfg(windows)]
fn native_d3d11_codec(wire: u8) -> Option<pf_dxvadec::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_dxvadec::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_dxvadec::Codec::H265),
        punktfunk_core::quic::CODEC_AV1 => Some(pf_dxvadec::Codec::Av1),
        _ => None,
    }
}

/// Native VAAPI decoder for a wire codec, or `None`. No caps bit: VAAPI
/// advertises a profile/entrypoint pair, which [`crate::video_vaapi_native::NativeVaapiDecoder::new`] queries.
#[cfg(target_os = "linux")]
fn native_vaapi_codec(wire: u8) -> Option<pf_vaadec::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_vaadec::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_vaadec::Codec::H265),
        punktfunk_core::quic::CODEC_AV1 => Some(pf_vaadec::Codec::Av1),
        _ => None,
    }
}

/// One decode rung, named so evidence and admission can talk without a
/// per-platform [`Backend`] variant. The CPU rung is in the table only ([`native_evidence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRung {
    /// pf-vkdecode on the presenter's device (`video_vk_native`).
    Vulkan,
    /// pf-dxvadec driving `ID3D11VideoDecoder` (`video_d3d11_native`, Windows).
    D3d11va,
    /// pf-vaadec driving a dlopen'd libva (`video_vaapi_native`, Linux).
    Vaapi,
    /// openh264 + rav1d (`video_software`).
    Software,
}

impl NativeRung {
    /// Log / `PUNKTFUNK_DECODER` name — the same strings as the `stats:` decode-path tag.
    pub fn name(self) -> &'static str {
        match self {
            NativeRung::Vulkan => "native-vulkan",
            NativeRung::D3d11va => "native-d3d11va",
            NativeRung::Vaapi => "native-vaapi",
            NativeRung::Software => "software",
        }
    }
}

/// Whether hardware has produced a checked picture through this rung/codec pair.
/// Yes/no for admission and the session log's `hardware_verified=` line, not a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RungEvidence {
    /// A real device decoded this pair and the result was checked (frame-hash parity or soak).
    pub verified: bool,
    /// What hardware, in one line — or, when unverified, why none. Goes verbatim into the session log.
    pub note: &'static str,
}

/// Evidence table, keyed by rung and wire codec. An unknown pair is
/// `verified: false` — a new codec leg must not inherit a neighbour's evidence.
pub fn native_evidence(rung: NativeRung, wire: u8) -> RungEvidence {
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
    let (verified, note) = match (rung, wire) {
        (NativeRung::Vulkan, CODEC_H264) => (
            true,
            "bit-exact vs libavcodec, 250/250 AUs on three drivers + 92-min soak (M2 WP-D)",
        ),
        (NativeRung::Vulkan, CODEC_HEVC) => (
            true,
            "bit-exact vs libavcodec incl. Main10/4:4:4, three drivers + HDR and Deck legs (M3)",
        ),
        (NativeRung::Vulkan, CODEC_AV1) => (
            true,
            "250/250 bit-identical to libavcodec on an RTX 5070 Ti (M7) - one vendor, no soak",
        ),
        (NativeRung::D3d11va, CODEC_H264 | CODEC_HEVC) => (
            true,
            "frame-hash parity on an RTX 4090 and an AMD iGPU + 30-min soak (M5)",
        ),
        // Decode-target must not alias a referenced surface (pf-dxvadec regression test).
        (NativeRung::D3d11va, CODEC_AV1) => (
            true,
            "250/250 delivered frames bit-identical to libavcodec on an RTX 3500 Ada AND an \
             Intel Arc (2026-08-07), after fixing a decode target that aliased a reference \
             surface on 268 of 274 frames - two vendors, no soak (M7)",
        ),
        // Keep `verified` false: flipping it would move `auto` off Vulkan Video on every Linux AMD/Intel client.
        (NativeRung::Vaapi, _) => (
            false,
            "7 legs bit-identical to libavcodec on RDNA3 (Mesa 26.0.3, 2026-08-08) - H.264, \
             H.265, HEVC Main 10 and AV1, on both the conformance vectors and our own host's \
             low-delay streams - but has NEVER run on a second vendor and has never been \
             soaked, and `verified` here would move `auto` off Vulkan Video on every Linux \
             AMD/Intel client (M6/M7)",
        ),
        // rav1d uses two frame contexts so a damaged reference returns an error instead of aborting.
        (NativeRung::Software, CODEC_H264 | CODEC_AV1) => (
            false,
            "openh264 has never run on glass; rav1d decodes 1080p and 4K60 AV1 there and \
             survives a mid-stream reference loss, but has no parity check or soak (M8)",
        ),
        _ => (false, "no hardware run recorded for this rung and codec"),
    };
    RungEvidence { verified, note }
}

/// Can this device run native Vulkan for this wire codec?
///
/// [`native_vulkan_gate`] without its `choice` half. Callers that ask what is
/// below them share this so "Vulkan is available here" cannot mean two things.
pub fn native_vulkan_usable(wire: u8, video_decode: bool, decode_video_caps: u32) -> bool {
    native_vulkan_gate("auto", wire, video_decode, decode_video_caps)
}

/// Whether `auto` may pick this native rung for this codec and device.
///
/// A verified rung is always admitted. An unverified rung yields only when
/// `below` is usable and verified for the same codec; yielding to software for
/// lack of evidence would discard hardware. Pins and demotion bypass this.
pub fn native_rung_admitted(rung: NativeRung, wire: u8, below: Option<NativeRung>) -> bool {
    native_evidence(rung, wire).verified
        || !below.is_some_and(|b| native_evidence(b, wire).verified)
}

/// Native Vulkan admission: `choice` is `native-vulkan` or the auto family
/// (`auto` / `` / `hardware`), the wire codec is one pf-vkdecode speaks
/// ([`native_codec`]), and the decode family advertises that codec op.
/// `video_decode` proves the extension stack, never the codec.
///
/// Other-backend pins refuse here. Init failure falls through, so admission
/// cannot cost the session its decoder. Stream shape is probed at
/// [`NativeVulkanDecoder::new`], not here.
fn native_vulkan_gate(choice: &str, wire: u8, video_decode: bool, decode_video_caps: u32) -> bool {
    let Some((_, codec_op)) = native_codec(wire) else {
        return false;
    };
    let chosen = matches!(choice, "native-vulkan" | "auto" | "" | "hardware");
    chosen && video_decode && decode_video_caps & codec_op != 0
}

/// Human name of a `quic::CODEC_*` bit. `?` for an unknown bit — it must not print as a known codec.
pub fn wire_codec_name(wire: u8) -> &'static str {
    match wire {
        punktfunk_core::quic::CODEC_H264 => "H.264",
        punktfunk_core::quic::CODEC_HEVC => "HEVC",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        punktfunk_core::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "?",
    }
}

/// `quic` codec bits this build can decode on the CPU — the last rung, so
/// the set a session is guaranteed to finish. Shared so the software map and
/// [`last_rung_verdict`] cannot drift.
pub fn software_decodable_codecs() -> u8 {
    punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_AV1
}

/// Reconnect action when the last rung has no decoder for this codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastRungVerdict {
    /// Reconnect advertising these caps. Non-empty and excluding the exhausted codec, so the host must pick something else.
    Retry { caps: u8 },
    /// Nothing left to advertise. Reconnecting would negotiate the same dead end.
    Dead,
}

/// Why the last rung had no answer. [`last_rung_verdict`] needs more than "a codec failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RungLoss {
    /// This codec has no CPU rung (HEVC). Hardware already failed; retry may only offer codecs that have a CPU rung.
    Codec,
    /// The codec has a CPU rung; this picture shape is outside it (10-bit, 4:4:4).
    /// Do not filter by [`software_decodable_codecs`]: that would drop HEVC a shape retry could finish.
    Shape,
}

/// Reconnect rule when the last rung has no decoder. The codec is fixed at
/// Welcome, so the lever is the next Hello. Drops `negotiated`; for
/// [`RungLoss::Codec`] also drops codecs with no CPU rung. `caps` is what
/// the retry advertises, so the pump's `exclude_codecs` cannot disagree.
pub fn last_rung_verdict(negotiated: u8, advertised: u8, loss: RungLoss) -> LastRungVerdict {
    let survivors = advertised & !negotiated;
    let caps = match loss {
        RungLoss::Codec => survivors & software_decodable_codecs(),
        RungLoss::Shape => survivors,
    };
    // A retry the host cannot pick is not a retry: PyroWave is opt-in and
    // stays out of `resolve_codec`. Survivors that are PyroWave alone resolve
    // to nothing. Judge liveness on the pickable set; carry the rest along.
    const PICKABLE: u8 = punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1;
    if caps & PICKABLE == 0 {
        LastRungVerdict::Dead
    } else {
        LastRungVerdict::Retry { caps }
    }
}

// Lives in portable `decoder_pref` (the Skia console reads the decoder row
// through it). Re-exported so desktop callers keep `video::migrate_decoder_pref`.
pub use crate::decoder_pref::migrate_decoder_pref;

/// Whether decode is pinned to the CPU rung (`PUNKTFUNK_DECODER` wins over Settings).
/// Same precedence as [`Decoder::new`]: a second reading of the same two inputs would drift.
pub fn decode_pinned_to_software(pref: &str) -> bool {
    resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref) == "software"
}

/// `PUNKTFUNK_DECODER` if it carries a value, else the stored setting. Pure
/// and shared so [`Decoder::new`] and [`decode_pinned_to_software`] cannot drift.
///
/// Trimmed: a trailing space matched no [`native_vulkan_gate`] arm and fell
/// through to `auto`. Whitespace-only is absent, not a pin to `""` (the auto family).
pub(crate) fn resolve_decoder_pref(env: Option<&str>, pref: &str) -> String {
    env.map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or_else(|| pref.to_string(), str::to_string)
}

/// `quic` codecs this build can decode. Advertised so the host never emits
/// one we cannot. Constants, not probes: asked before a device exists.
/// Device facts are [`decodable_codecs_for`].
///
/// AV1 here is a decoder existing, not one that can keep up — gated in
/// [`decodable_codecs_for`]. HEVC is hardware-only and still advertised:
/// refusing it up front would cost every working box to protect the few
/// that later fail. Exhaustion is [`last_rung_verdict`].
pub fn decodable_codecs() -> u8 {
    // Native Vulkan's three codecs union the CPU rung's. Written as a union so
    // removing a rung's codec leg drops the advertisement rather than keeping it.
    punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1
        | software_decodable_codecs()
}

/// Can this machine decode AV1 in hardware? Device facts only, never a decoder existing:
/// Vulkan `DECODE_AV1` on the decode family, or (Windows) D3D11 import so DXVA
/// can run Profile 0. The CPU AV1 rung still exists; the wire promise is made once.
///
/// VAAPI is not consulted: opening a display is too early and too often; the
/// Vulkan bit covers the Mesa devices where VAAPI AV1 exists.
pub fn av1_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    if vk.is_some_and(|v| v.video_decode && v.decode_video_caps & VIDEO_CODEC_OP_DECODE_AV1 != 0) {
        return true;
    }
    // Per-platform second answer, bound to a name: a cfg'd `return` is `needless_return` on Windows (`-D warnings`).
    #[cfg(windows)]
    let d3d11 = vk.is_some_and(|v| v.d3d11_import);
    #[cfg(not(windows))]
    let d3d11 = false;
    d3d11
}

/// Can this client decode 4:4:4 HEVC — the promise `VIDEO_CAP_444` makes.
///
/// Vulkan only: VAAPI/DXVA/CPU are 4:2:0. Advertising 4:4:4 without a Vulkan
/// 4:4:4 profile costs the whole codec (host grants 4:4:4 only on HEVC; no CPU HEVC).
/// Both 8- and 10-bit profiles are required: HDR may resolve 4:4:4 10-bit.
/// Not used for `VIDEO_CAP_10BIT`: all three hardware rungs decode 10-bit 4:2:0.
pub fn hevc_444_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    #[cfg(any(target_os = "linux", windows))]
    {
        vk.is_some_and(|v| {
            crate::video_vk_native::hevc_shape_supported(v, CHROMA_444, 0)
                && crate::video_vk_native::hevc_shape_supported(v, CHROMA_444, 2)
        })
    }
    // No native Vulkan rung off the two desktop OSes, so nothing here can decode 4:4:4.
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = vk;
        false
    }
}

/// `chroma_format_idc` for 4:4:4 (H.265 7.4.3.2). Spelled once so the two depth probes cannot disagree.
const CHROMA_444: u8 = 3;

/// Can this client present a PQ stream — the promise `VIDEO_CAP_HDR` makes.
/// Decode is not the question (every hardware rung does 10-bit 4:2:0).
///
/// On Windows, D3D11VA is in every ladder and shows PQ as HDR10 pass-through
/// or the video processor's PQ→sRGB tonemap. That tonemap is unvalidated: the
/// Blt succeeds and paints garbage where it is missing. Elsewhere the CSC shader tonemaps PQ.
pub fn hdr_presentable(vk: Option<&VulkanDecodeDevice>) -> bool {
    #[cfg(windows)]
    {
        vk.is_none_or(|v| {
            !v.d3d11_import
                || v.d3d11_hdr10
                || crate::video_d3d11::pq_tonemap_supported(v.adapter_luid)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        true
    }
}

/// Desktop `video_caps` from the user switches, testable without a GPU.
/// Callers AND `want_444` with [`hevc_444_hardware_decodable`] and `hdr_enabled`
/// with [`hdr_presentable`]. `MULTI_SLICE` is unconditional here; Amlogic
/// MediaCodec wedges on multi-slice AUs. `ten_bit_sdr` asks for Main10 under SDR.
pub fn video_caps_for(hdr_enabled: bool, ten_bit_sdr: bool, want_444: bool) -> u8 {
    let mut caps = punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE;
    if hdr_enabled {
        caps |= punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR;
    }
    if ten_bit_sdr {
        caps |= punktfunk_core::quic::VIDEO_CAP_10BIT;
    }
    if want_444 {
        caps |= punktfunk_core::quic::VIDEO_CAP_444;
    }
    caps
}

/// [`decodable_codecs`] plus PyroWave when the compute probe passed, minus
/// codecs `decoder_pref` makes unreachable. Advertisement only: `resolve_codec`
/// never auto-picks PyroWave.
pub fn decodable_codecs_for(vk: Option<&VulkanDecodeDevice>, decoder_pref: &str) -> u8 {
    let mut bits = decodable_codecs();
    // AV1 is hardware-gated. Without this the CPU rung's existence advertises
    // AV1 to a machine that would decode it in software, and negotiation has no fallback.
    if bits & punktfunk_core::quic::CODEC_AV1 != 0 && !av1_hardware_decodable(vk) {
        tracing::info!(
            "AV1 not advertised: no hardware AV1 decode on this device (a software \
             decoder exists, but a 4K AV1 stream is not survivable on it)"
        );
        bits &= !punktfunk_core::quic::CODEC_AV1;
    }
    // Software pin has no HEVC. Guarded on something remaining: a Hello with
    // zero codecs reads as HEVC-only (`resolve_codec`'s pre-negotiation default).
    if bits & punktfunk_core::quic::CODEC_HEVC != 0
        && bits & !punktfunk_core::quic::CODEC_HEVC != 0
        && decode_pinned_to_software(decoder_pref)
    {
        tracing::info!(
            "HEVC not advertised: decode is pinned to software and there is no software \
             HEVC decoder in this build"
        );
        bits &= !punktfunk_core::quic::CODEC_HEVC;
    }
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    if vk.map(|v| v.pyrowave_decode).unwrap_or(false) {
        return bits | punktfunk_core::quic::CODEC_PYROWAVE;
    }
    #[cfg(not(all(any(target_os = "linux", windows), feature = "pyrowave")))]
    let _ = vk;
    bits
}

/// Log what `PUNKTFUNK_AU_FAULT` will do, including when the answer is nothing.
/// The injector lives on the native Vulkan decode entry; silence on any other
/// rung is indistinguishable from "injected and nothing detected it".
fn report_au_fault_env(native_rung: bool) {
    let Ok(spec) = std::env::var("PUNKTFUNK_AU_FAULT") else {
        return;
    };
    if spec.is_empty() {
        return;
    }
    match pf_vkdecode::AuFault::from_spec(&spec) {
        // The native backend logs the arming itself (mode + period).
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

/// Name the landed rung and whether hardware has decoded this codec through it.
/// `info` if verified, `warn` if not, with [`native_evidence`] verbatim.
/// The `stats:` decode-path tag names the rung but not its provenance.
fn log_rung(backend: &Backend, wire: u8) {
    let (rung, evidence) = match backend {
        Backend::NativeVulkan(_) => (
            NativeRung::Vulkan.name(),
            Some(native_evidence(NativeRung::Vulkan, wire)),
        ),
        #[cfg(windows)]
        Backend::NativeD3d11va(_) => (
            NativeRung::D3d11va.name(),
            Some(native_evidence(NativeRung::D3d11va, wire)),
        ),
        #[cfg(target_os = "linux")]
        Backend::V4l2Request(_) => ("v4l2-request", None),
        #[cfg(target_os = "linux")]
        Backend::NativeVaapi(_) => (
            NativeRung::Vaapi.name(),
            Some(native_evidence(NativeRung::Vaapi, wire)),
        ),
        Backend::Software(_) => (
            NativeRung::Software.name(),
            Some(native_evidence(NativeRung::Software, wire)),
        ),
        // PyroWave is not in the evidence table: its own codec and decoder, nothing above or below it.
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        Backend::PyroWave(_) => ("pyrowave", None),
    };
    let codec = wire_codec_name(wire);
    match evidence {
        Some(e) if e.verified => tracing::info!(
            rung,
            codec,
            hardware_verified = true,
            evidence = e.note,
            "decode rung active"
        ),
        Some(e) => tracing::warn!(
            rung,
            codec,
            hardware_verified = false,
            evidence = e.note,
            "decode rung active — NO hardware has ever decoded a frame through this \
             rung/codec pair (evidence table, video.rs)"
        ),
        None => tracing::info!(rung, codec, "decode rung active"),
    }
}

impl Decoder {
    /// Build the decode ladder. `wire` is the Welcome `quic::CODEC_*`; `pref` is
    /// Settings (`hardware` reads as auto); `vk` is the presenter's device.
    /// `PUNKTFUNK_DECODER` wins, then the setting; both default to auto.
    ///
    /// Auto order is [`VulkanDecodeDevice::prefer_vulkan_first`], then
    /// [`native_rung_admitted`]: an unproven rung does not go first when the rung
    /// below it is proven and usable. `stream` is the construction-time shape probe.
    pub fn new(
        wire: u8,
        pref: &str,
        vk: Option<&VulkanDecodeDevice>,
        stream: StreamFormat,
    ) -> Result<Decoder> {
        let stored = resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref);
        let choice = migrate_decoder_pref(&stored);
        if choice != stored {
            // Once per session, at `warn`: the stored name no longer exists, and
            // the rung below is not the one the settings file names.
            tracing::warn!(
                stored,
                using = choice,
                "the decoder preference named libavcodec's rung, which no longer exists \
                 (M10 removed FFmpeg from the client) — using the native rung for the \
                 same hardware path"
            );
        }
        #[cfg(windows)]
        let (d3d11_import, adapter_luid, d3d11_hdr10) = (
            vk.is_some_and(|v| v.d3d11_import),
            vk.and_then(|v| v.adapter_luid),
            vk.is_some_and(|v| v.d3d11_hdr10),
        );
        let done = |backend: Backend| {
            // One exit every backend leaves through, so a session that never
            // reaches the native constructor still reports `PUNKTFUNK_AU_FAULT`.
            report_au_fault_env(matches!(backend, Backend::NativeVulkan(_)));
            log_rung(&backend, wire);
            Ok(Decoder {
                entered_rungs: rung_bit(&backend),
                backend,
                wire_codec: wire,
                vaapi_fails: 0,
                first_fail: None,
                want_keyframe: false,
                delivered: false,
                vk: vk.cloned(),
                stream,
                #[cfg(windows)]
                d3d11_import,
                #[cfg(windows)]
                adapter_luid,
                #[cfg(windows)]
                d3d11_hdr10,
            })
        };
        let codec_name = wire_codec_name(wire);
        // Pins first: a pin skips vendor order. Refusal or init failure logs and
        // continues as `auto` — a pin's failure must not be quieter than auto's.
        let mut choice = choice;
        #[cfg(windows)]
        if choice == crate::video_d3d11_native::DECODER_PIN {
            match (native_d3d11_codec(wire), vk.filter(|v| v.d3d11_import)) {
                (Some(codec), Some(v)) => {
                    match crate::video_d3d11_native::NativeD3d11Decoder::new(
                        codec,
                        stream,
                        v.adapter_luid,
                        v.d3d11_hdr10,
                    ) {
                        Ok(d) => {
                            tracing::info!(
                                codec = codec_name,
                                decoder = d.name(),
                                "native D3D11VA hardware decode active \
                                 (pf-dxvadec, shared-texture hand-off)"
                            );
                            return done(Backend::NativeD3d11va(Box::new(d)));
                        }
                        Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                            "native D3D11VA init failed — demoting to the standard ladder"),
                    }
                }
                (None, _) => tracing::warn!(
                    codec = codec_name,
                    "PUNKTFUNK_DECODER=native-d3d11va refused (needs an H.264, HEVC or \
                     AV1 session) — standard ladder"
                ),
                (_, None) => tracing::warn!(
                    "PUNKTFUNK_DECODER=native-d3d11va refused (the presenter's device lacks \
                     the win32 external-memory import extensions) — standard ladder"
                ),
            }
            choice = "auto".to_string();
        }
        #[cfg(target_os = "linux")]
        if choice == crate::video_v4l2_request::DECODER_PIN {
            match crate::video_v4l2_request::V4l2RequestDecoder::new(wire) {
                Ok(d) => {
                    tracing::info!(codec = codec_name, decoder = d.name(),
                        "Raspberry Pi V4L2 Request HEVC hardware decode active (NEON SAND transfer)");
                    return done(Backend::V4l2Request(Box::new(d)));
                }
                Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                    "V4L2 Request init failed — demoting to the standard ladder"),
            }
            choice = "auto".to_string();
        }
        // Native VAAPI pin. Unverified; the pin is how a lab run reaches it when Vulkan is first.
        #[cfg(target_os = "linux")]
        if choice == crate::video_vaapi_native::DECODER_PIN {
            match native_vaapi_codec(wire) {
                Some(codec) => {
                    match crate::video_vaapi_native::NativeVaapiDecoder::new(codec, stream) {
                        Ok(d) => {
                            tracing::info!(
                                codec = codec_name,
                                decoder = d.name(),
                                "native VAAPI hardware decode active (pf-vaadec, zero-copy dmabuf)"
                            );
                            return done(Backend::NativeVaapi(Box::new(d)));
                        }
                        Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                            "native VAAPI init failed — demoting to the standard ladder"),
                    }
                }
                None => tracing::warn!(
                    codec = codec_name,
                    "PUNKTFUNK_DECODER=native-vaapi refused (needs an H.264, HEVC or \
                     AV1 session) — standard ladder"
                ),
            }
            choice = "auto".to_string();
        }
        let mut native_tried = false;
        if choice == "native-vulkan" {
            if native_vulkan_gate(
                &choice,
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            ) {
                native_tried = true;
                let vk = vk.expect("gate demands video_decode, so vk is Some");
                let (codec, _) = native_codec(wire).expect("the gate admitted this codec");
                match NativeVulkanDecoder::new(vk, codec, stream) {
                    Ok(n) => {
                        tracing::info!(
                            codec = codec_name,
                            "native Vulkan Video hardware decode active \
                             (pf-vkdecode, presenter-shared device)"
                        );
                        return done(Backend::NativeVulkan(Box::new(n)));
                    }
                    Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                        "native Vulkan decode init failed — demoting to the standard ladder"),
                }
            } else {
                // The gate is an AND of three; name all three. `video_decode=true`
                // beside "refused" is unreadable without the family's advertised ops.
                tracing::warn!(
                    codec = codec_name,
                    video_decode = vk.is_some_and(|v| v.video_decode),
                    decode_video_caps =
                        format_args!("0x{:X}", vk.map_or(0, |v| v.decode_video_caps)),
                    codec_op_needed =
                        format_args!("0x{:X}", native_codec(wire).map_or(0, |(_, op)| op)),
                    device = vk.map_or("", |v| v.device_name.as_str()),
                    "PUNKTFUNK_DECODER=native-vulkan refused (needs an H.264, HEVC or AV1 \
                     session and a presenter device whose decode family advertises that \
                     codec) — standard ladder"
                );
            }
            choice = "auto".to_string();
        }
        // Linux VAAPI rung, once: Intel/unknown take it before Vulkan, everyone else after.
        #[cfg(target_os = "linux")]
        let vaapi_rung = |choice: &str| -> Result<Option<Backend>> {
            if let Some(codec) = native_vaapi_codec(wire) {
                match crate::video_vaapi_native::NativeVaapiDecoder::new(codec, stream) {
                    Ok(d) => {
                        tracing::info!(
                            codec = codec_name,
                            decoder = d.name(),
                            "native VAAPI hardware decode active (pf-vaadec, zero-copy dmabuf)"
                        );
                        return Ok(Some(Backend::NativeVaapi(Box::new(d))));
                    }
                    Err(e) => tracing::info!(reason = %format!("{e:#}"),
                        "native VAAPI unavailable — continuing down the ladder"),
                }
            }
            // `choice` is unread: a native pin is handled above, so by here it is the auto family.
            let _ = choice;
            Ok(None)
        };
        // Linux `auto`: VAAPI first unless Vulkan Video is the established
        // answer (NVIDIA: no usable VAAPI; VanGogh: VAAPI chroma-fringes).
        // Mesa exposes decode queues by default, which would move every AMD/Intel box onto Vulkan-on-Mesa.
        #[cfg(target_os = "linux")]
        let mut vaapi_tried = false;
        #[cfg(target_os = "linux")]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
        {
            // Intel/unknown order: the rung below VAAPI is native Vulkan. If this
            // device can run that proven rung, VAAPI does not go first ([`native_rung_admitted`]).
            let below = native_vulkan_usable(
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            )
            .then_some(NativeRung::Vulkan);
            if native_rung_admitted(NativeRung::Vaapi, wire, below) {
                vaapi_tried = true;
                if let Some(b) = vaapi_rung(&choice)? {
                    return done(b);
                }
            } else {
                tracing::info!(
                    codec = codec_name,
                    evidence = native_evidence(NativeRung::Vaapi, wire).note,
                    "native VAAPI is this device's first hardware rung, but it has decoded \
                     nothing on any hardware and the rung below it — native Vulkan Video — \
                     has, for this codec, on this device: taking Vulkan first \
                     (PUNKTFUNK_DECODER=native-vaapi runs it anyway)"
                );
            }
        }
        // Windows `auto`: D3D11VA first unless Vulkan Video is the established
        // answer (NVIDIA/AMD). Intel advertises Vulkan Video, so the cap gate
        // alone does not keep it off that rung; DXVA is the path Windows players exercise.
        #[cfg(windows)]
        let d3d11_rung = |choice: &str| -> Result<Option<Backend>> {
            let Some(v) = vk.filter(|v| v.d3d11_import) else {
                // A pin that cannot work must log: a DXVA frame reaches the screen
                // only through the presenter's win32 import.
                if choice == crate::video_d3d11_native::DECODER_PIN {
                    bail!(
                        "PUNKTFUNK_DECODER=native-d3d11va but the presenter's device lacks the \
                         win32 external-memory import extensions — see the presenter log"
                    );
                }
                return Ok(None);
            };
            if let Some(codec) = native_d3d11_codec(wire) {
                match crate::video_d3d11_native::NativeD3d11Decoder::new(
                    codec,
                    stream,
                    v.adapter_luid,
                    v.d3d11_hdr10,
                ) {
                    Ok(d) => {
                        tracing::info!(
                            codec = codec_name,
                            decoder = d.name(),
                            "native D3D11VA hardware decode active \
                             (pf-dxvadec, shared-texture hand-off)"
                        );
                        return Ok(Some(Backend::NativeD3d11va(Box::new(d))));
                    }
                    Err(e) => tracing::info!(reason = %format!("{e:#}"),
                        "native D3D11VA unavailable — continuing down the ladder"),
                }
            }
            Ok(None)
        };
        #[cfg(windows)]
        let mut d3d11_tried = false;
        #[cfg(windows)]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
            // Intel/unknown: `below` is `None` on purpose. Under DXVA AV1 the
            // next rung is CPU, not proven Vulkan. H.264/H.265 are verified, so they admit.
            && native_rung_admitted(NativeRung::D3d11va, wire, None)
        {
            d3d11_tried = true;
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        // Vulkan rung. `auto` reaches it from one place; [`native_vulkan_gate`]
        // is the whole admission. `native_tried` skips a repeat of the pin above.
        if !native_tried
            && native_vulkan_gate(
                &choice,
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            )
        {
            let vk = vk.expect("gate demands video_decode, so vk is Some");
            let (codec, _) = native_codec(wire).expect("the gate admitted this codec");
            match NativeVulkanDecoder::new(vk, codec, stream) {
                Ok(n) => {
                    tracing::info!(
                        codec = codec_name,
                        "native Vulkan Video hardware decode active \
                         (pf-vkdecode auto rung, presenter-shared device)"
                    );
                    return done(Backend::NativeVulkan(Box::new(n)));
                }
                Err(e) => tracing::info!(reason = %format!("{e:#}"),
                    "native Vulkan decode unavailable — continuing down the ladder"),
            }
        }
        // VAAPI after Vulkan when that rung was not already tried. A presenter
        // that cannot display the dmabufs demotes via [`Decoder::force_software`].
        #[cfg(target_os = "linux")]
        if choice != "software" && !vaapi_tried {
            if let Some(b) = vaapi_rung(&choice)? {
                return done(b);
            }
        }
        // D3D11VA fallback when Vulkan is missing or failed. `d3d11_tried` skips the Intel/unknown first try.
        #[cfg(windows)]
        if choice != "software" && !d3d11_tried {
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        if choice == "software" {
            // Log why hardware was not attempted: a stored "software" pref otherwise silently skips it.
            tracing::info!(
                "software decode by preference (Settings decoder / PUNKTFUNK_DECODER) — \
                 hardware decode not attempted"
            );
        }
        // `?` can carry `NoSoftwareRung` (HEVC with no hardware). Stays typed to the pump ([`last_rung_verdict`]).
        done(Backend::Software(SoftwareDecoder::new(wire)?))
    }

    /// Wait for a Vulkan-Video GPU decode (timeline). `false` declines the
    /// sample: not this backend, timeout, missing ledger pair, or stale generation.
    pub fn wait_hw_decoded(&self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        match &self.backend {
            Backend::NativeVulkan(d) => d.wait_timeline(timeline_sem, value, timeout_ns),
            _ => false,
        }
    }

    /// Decode-integrity counters, or `None` where the backend cannot answer
    /// (CPU, PyroWave). `None` is "cannot see corruption"; `Some(default)` is "looked and saw none".
    pub fn decode_health(&self) -> Option<DecodeHealth> {
        match &self.backend {
            Backend::NativeVulkan(d) => Some(d.health()),
            // DXVA sees concealment and refusals via the planner. No per-picture
            // status query, so `status_queries` is false and `failed` stays 0.
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => Some(d.health()),
            // Same as DXVA: libva has no per-picture decode-status query.
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(d) => Some(d.health()),
            _ => None,
        }
    }

    /// Newest planned decode-order ordinal — the freeze watermark
    /// ([`NativeVkFrame::decode_order`]). 0 on lanes with no bitstream parser (and no local recovery).
    pub fn decode_order(&self) -> u64 {
        match &self.backend {
            Backend::NativeVulkan(d) => d.decode_order(),
            _ => 0,
        }
    }

    /// Open a PyroWave decoder: compute on the presenter's device.
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
            wire_codec: punktfunk_core::quic::CODEC_PYROWAVE,
            vaapi_fails: 0,
            first_fail: None,
            want_keyframe: false,
            delivered: false,
            // PyroWave never demotes (failure renegotiates the codec). Demotion-rebuild
            // fields stay well-formed and unused.
            vk: None,
            stream: StreamFormat::SDR_420_8,
            entered_rungs: 0,
            #[cfg(windows)]
            d3d11_import: false,
            #[cfg(windows)]
            adapter_luid: None,
            #[cfg(windows)]
            d3d11_hdr10: false,
        })
    }

    /// Drain the IDR request. The pump calls this each iteration so a demoted
    /// or erroring decoder can resync under the infinite GOP.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Swap the backend in and reset the old one's health. One place so a call
    /// site cannot forget [`Self::entered_rungs`] and loop the ladder.
    fn install(&mut self, backend: Backend) {
        self.entered_rungs |= rung_bit(&backend);
        self.backend = backend;
        self.vaapi_fails = 0;
        self.first_fail = None;
        self.delivered = false;
    }

    /// Running rung is the platform native (VAAPI / D3D11VA). The demotion
    /// that goes sideways into native Vulkan fires only from here.
    fn is_native_platform_rung(&self) -> bool {
        #[cfg(target_os = "linux")]
        let it = matches!(self.backend, Backend::NativeVaapi(_));
        #[cfg(windows)]
        let it = matches!(self.backend, Backend::NativeD3d11va(_));
        #[cfg(not(any(target_os = "linux", windows)))]
        let it = false;
        it
    }

    /// Demote to software when the presenter cannot display hardware frames.
    /// Decode still succeeds in that state, so the error streak never fires
    /// and without this the stream stays black. No-op when already software.
    pub fn force_software(&mut self) -> Result<()> {
        if matches!(self.backend, Backend::Software(_)) {
            return Ok(());
        }
        tracing::warn!("presenter can't display hardware frames — demoting to software decode");
        // Same typed refusal as every software construction: HEVC has nothing below, so the pump reconnects.
        self.install(Backend::Software(SoftwareDecoder::new(self.wire_codec)?));
        self.want_keyframe = true;
        Ok(())
    }

    /// Feed one access unit (hosts are one-in/one-out). Hardware errors
    /// re-request an IDR; only a persistent streak demotes. `want_keyframe` is
    /// set either way — the infinite GOP has no other resync.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        self.decode_frame(au, 0, true)
    }

    /// [`decode`](Self::decode) with wire facts. `user_flags` carries chunk
    /// alignment; `complete` is false for a partial delivery (PyroWave only,
    /// as localized blur).
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
        // Concealment: native `Ok(None)` because the picture was damaged, not
        // buffering. Decides whether the `Ok` below may clear the demotion streak.
        let mut concealed = false;
        let result = match &mut self.backend {
            Backend::NativeVulkan(n) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = n.decode(au).map(|f| f.map(DecodedImage::NativeVk));
                // Stream damage is not a decoder fault. Concealment is `Ok(None)`
                // plus this flag. A driver `RESULT_STATUS` Failed stays an `Err`.
                if n.take_recovery_request() {
                    self.want_keyframe = true;
                    concealed = true;
                }
                r
            }
            #[cfg(target_os = "linux")]
            Backend::V4l2Request(v) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                v.decode(au).map(|f| f.map(DecodedImage::V4l2Planar))
            }
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(v) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = v.decode(au).map(|f| f.map(DecodedImage::NativeDmabuf));
                // Same concealment split as Vulkan.
                if v.take_recovery_request() {
                    self.want_keyframe = true;
                    concealed = true;
                }
                r
            }
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = d.decode(au).map(|f| f.map(DecodedImage::D3d11));
                // Same concealment split as Vulkan.
                if d.take_recovery_request() {
                    self.want_keyframe = true;
                    concealed = true;
                }
                r
            }
            // Nothing else decodes PyroWave: propagate the error; the pump renegotiates the codec.
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
                // Only an answer that proves the rung works may clear the streak.
                if clears_demotion_streak(f.is_some(), concealed) {
                    self.vaapi_fails = 0;
                    self.first_fail = None;
                }
                self.delivered |= f.is_some();
                Ok(f)
            }
            Err(e) => {
                let which = match self.backend {
                    Backend::NativeVulkan(_) => "native Vulkan Video",
                    #[cfg(windows)]
                    Backend::NativeD3d11va(_) => "native D3D11VA",
                    #[cfg(target_os = "linux")]
                    Backend::V4l2Request(_) => "V4L2 Request",
                    #[cfg(target_os = "linux")]
                    Backend::NativeVaapi(_) => "native VAAPI",
                    // PyroWave returns above and software never reaches here.
                    _ => "hardware",
                };
                self.vaapi_fails += 1;
                self.want_keyframe = true;
                let first = *self.first_fail.get_or_insert_with(std::time::Instant::now);
                if self.vaapi_fails >= VAAPI_DEMOTE_AFTER && first.elapsed() >= HW_DEMOTE_MIN_STREAK
                {
                    // A never-delivered native rung is a decoder the session never
                    // had; it must not cost the rung below. `entered_rungs` keeps
                    // the walk monotone (native rungs sit in opposite vendor order).
                    #[cfg(target_os = "linux")]
                    if self.entered_rungs & RUNG_BIT_NATIVE_PLATFORM == 0 {
                        if let Some(codec) = native_vaapi_codec(self.wire_codec) {
                            match crate::video_vaapi_native::NativeVaapiDecoder::new(
                                codec,
                                self.stream,
                            ) {
                                Ok(d) => {
                                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                                        from = which, decoder = d.name(),
                                        "hardware decode failing repeatedly — demoting to \
                                         native VAAPI");
                                    self.install(Backend::NativeVaapi(Box::new(d)));
                                    return Ok(None);
                                }
                                Err(va) => tracing::info!(reason = %format!("{va:#}"),
                                    "native VAAPI unavailable for demotion — continuing down \
                                     the ladder"),
                            }
                        }
                    }
                    #[cfg(windows)]
                    if self.entered_rungs & RUNG_BIT_NATIVE_PLATFORM == 0 && self.d3d11_import {
                        if let Some(codec) = native_d3d11_codec(self.wire_codec) {
                            match crate::video_d3d11_native::NativeD3d11Decoder::new(
                                codec,
                                self.stream,
                                self.adapter_luid,
                                self.d3d11_hdr10,
                            ) {
                                Ok(d) => {
                                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                                        from = which, decoder = d.name(),
                                        "hardware decode failing repeatedly — demoting to \
                                         native D3D11VA");
                                    self.install(Backend::NativeD3d11va(Box::new(d)));
                                    return Ok(None);
                                }
                                Err(dx) => tracing::info!(reason = %format!("{dx:#}"),
                                    "native D3D11VA unavailable for demotion — continuing down \
                                     the ladder"),
                            }
                        }
                    }
                    // Failing platform native on Intel/unknown has Vulkan below it.
                    // Only fires from a native platform rung.
                    if self.entered_rungs & RUNG_BIT_NATIVE_VULKAN == 0
                        && self.is_native_platform_rung()
                    {
                        if let Some(v) = self.vk.clone().filter(|v| v.video_decode) {
                            if native_vulkan_gate(
                                "auto",
                                self.wire_codec,
                                true,
                                v.decode_video_caps,
                            ) {
                                let (codec, _) =
                                    native_codec(self.wire_codec).expect("the gate admitted it");
                                match NativeVulkanDecoder::new(&v, codec, self.stream) {
                                    Ok(n) => {
                                        tracing::warn!(error = %e, fails = self.vaapi_fails,
                                            from = which,
                                            "hardware decode failing repeatedly — demoting to \
                                             native Vulkan Video");
                                        self.install(Backend::NativeVulkan(Box::new(n)));
                                        return Ok(None);
                                    }
                                    Err(nv) => tracing::info!(reason = %format!("{nv:#}"),
                                        "native Vulkan Video unavailable for demotion — \
                                         software decode"),
                                }
                            }
                        }
                    }
                    tracing::warn!(error = %e, fails = self.vaapi_fails,
                        "{which} decode failing repeatedly — demoting to software");
                    // Ladder bottom. H.264/AV1 always builds; HEVC `?` carries
                    // `NoSoftwareRung` to the pump, which reconnects without HEVC.
                    self.install(Backend::Software(SoftwareDecoder::new(self.wire_codec)?));
                } else {
                    tracing::debug!(backend = which, error = %e,
                        "decode error — requesting keyframe, keeping hardware decode");
                }
                Ok(None)
            }
        }
    }
}

/// Mutex serializing `vkQueueSubmit` / `vkQueuePresentKHR` / `vkQueueWaitIdle`
/// on the queue the presenter shares with the decode lane.
///
/// The presenter has one graphics-family queue; the pump submits decode/CSC
/// to it from another thread. Unsynchronized `vkQueueSubmit` is intermittent
/// `VK_ERROR_DEVICE_LOST`. Lock/unlock stay for callbacks; [`QueueLock::guard`] is RAII.
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

    /// Block until the queue is free, then take it. Pair with [`QueueLock::unlock`], or use [`QueueLock::guard`].
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

/// Presenter's Vulkan device, so decode runs on the same device the presenter
/// samples from — the VkImage is composited in place.
///
/// Plain integers: this crate has no ash. Handles stay valid for the
/// presenter's lifetime, which outlives every session pump.
#[derive(Clone)]
pub struct VulkanDecodeDevice {
    /// `PFN_vkGetInstanceProcAddr` from the loader. Decode lanes resolve everything else through it.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    /// PCI vendor of the presenter's physical device (0x10DE NVIDIA, 0x1002 AMD,
    /// 0x8086 Intel) — drives [`Self::prefer_vulkan_first`].
    pub vendor_id: u32,
    /// Driver device-name string (logged on admission refusal).
    pub device_name: String,
    /// The presenter's graphics+present family.
    pub graphics_qf: u32,
    /// Video-decode family. May equal `graphics_qf`; the native rung must detect that (`submit_queues_collide`).
    pub decode_qf: u32,
    /// Raw `VkVideoCodecOperationFlagsKHR` the decode family advertises.
    pub decode_video_caps: u32,
    /// Extensions enabled at instance/device creation. Pyrowave replays these
    /// verbatim into pinned create-info, so they must match reality.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<std::ffi::CString>,
    /// Features enabled at device creation (reported via `device_features`).
    pub f_sampler_ycbcr: bool,
    pub f_timeline_semaphore: bool,
    pub f_synchronization2: bool,
    /// Vulkan Video decode is usable (queue + extensions + features). The bundle
    /// exists without it (D3D11 interop); gate the Vulkan rung on this, not on `Some`.
    pub video_decode: bool,
    /// Real present timing (`VK_KHR_present_wait`). Gates `CLIENT_CAP_PHASE_LOCK`:
    /// without a latch stamp the desktop must not claim the cap.
    pub present_timing: bool,
    /// PyroWave decode is usable (Vulkan 1.3 + `shaderInt16` / 8-bit storage /
    /// subgroup size control). Gates the `CODEC_PYROWAVE` advertisement.
    pub pyrowave_decode: bool,
    /// Feature facts the pyrowave pinned create-info reconstruction mirrors
    /// so it can share this `VkDevice`.
    pub f_shader_int16: bool,
    pub f_storage_buffer8: bool,
    pub f_subgroup_size_control: bool,
    pub f_compute_full_subgroups: bool,
    pub f_shader_float16: bool,
    /// `VkPhysicalDeviceProperties::apiVersion` of the presenter's device.
    pub api_version: u32,
    /// Queue families the device was created with (one queue each, priority 1.0). Mirrored by reconstruction.
    pub queue_families: Vec<u32>,
    /// Presenter enabled win32 external-memory + keyed mutex. Always `false` off Windows.
    pub d3d11_import: bool,
    /// Presenter can import RGB10A2 and offers an HDR10 swapchain, so D3D11VA
    /// emits PQ pass-through instead of tonemapping to sRGB. Always `false` off Windows.
    pub d3d11_hdr10: bool,
    /// Adapter LUID when the driver reports one. D3D11VA builds on the same
    /// adapter so shared textures never cross GPUs. `None` off Windows or when unreported.
    pub adapter_luid: Option<[u8; 8]>,
    /// Shared queue lock. Presenter and decode lanes both take it around their submits.
    pub queue_lock: std::sync::Arc<QueueLock>,
}

impl VulkanDecodeDevice {
    /// Should `auto` try Vulkan Video before VAAPI / D3D11VA on this device?
    ///
    /// NVIDIA and AMD: yes. NVIDIA has no usable VAAPI; VanGogh VAAPI chroma-fringes.
    /// A Vulkan streak demotes to the platform rung, not software. Intel/unknown
    /// take VAAPI or D3D11VA first (ANV is the least-proven Mesa path).
    pub fn prefer_vulkan_first(&self) -> bool {
        const VENDOR_NVIDIA: u32 = 0x10DE;
        const VENDOR_AMD: u32 = 0x1002;
        self.vendor_id == VENDOR_NVIDIA || self.vendor_id == VENDOR_AMD
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};

    /// Advertising 4:4:4 on a device that cannot decode it costs HEVC: there is
    /// no CPU HEVC, and the host grants 4:4:4 on HEVC only.
    #[test]
    fn the_444_bit_needs_the_setting_and_a_device_that_can_decode_it() {
        const V444: u8 = punktfunk_core::quic::VIDEO_CAP_444;
        assert_eq!(
            video_caps_for(true, false, false) & V444,
            0,
            "a 4:4:4 promise this device cannot keep costs HEVC entirely"
        );
        assert_ne!(video_caps_for(true, false, true) & V444, 0);
        assert_eq!(video_caps_for(true, false, false) & V444, 0);
        assert_eq!(video_caps_for(false, false, false) & V444, 0);

        // 4:4:4 must not disturb 10-bit/HDR (those are not probe-gated).
        const HDR_BITS: u8 =
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR;
        for want_444 in [false, true] {
            assert_eq!(video_caps_for(true, false, want_444) & HDR_BITS, HDR_BITS);
            assert_eq!(video_caps_for(false, false, want_444) & HDR_BITS, 0);
            assert_ne!(
                video_caps_for(false, false, want_444)
                    & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE,
                0,
                "MULTI_SLICE is unconditional for this embedder"
            );
        }
    }

    /// 10-bit SDR advertises the depth bit alone — never HDR — and is subsumed by HDR.
    #[test]
    fn ten_bit_sdr_advertises_depth_without_hdr() {
        const TEN: u8 = punktfunk_core::quic::VIDEO_CAP_10BIT;
        const HDR: u8 = punktfunk_core::quic::VIDEO_CAP_HDR;
        assert_eq!(video_caps_for(false, true, false) & (TEN | HDR), TEN);
        assert_eq!(video_caps_for(false, false, false) & (TEN | HDR), 0);
        assert_eq!(video_caps_for(true, true, false) & (TEN | HDR), TEN | HDR);
        assert_eq!(
            video_caps_for(false, true, false) & punktfunk_core::quic::VIDEO_CAP_444,
            0
        );
        assert_ne!(
            video_caps_for(false, true, false) & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE,
            0
        );
    }

    /// No presenter Vulkan device ⇒ no 4:4:4. The `Some` arm needs a GPU.
    #[test]
    fn no_vulkan_device_means_no_444_promise() {
        assert!(!hevc_444_hardware_decodable(None));
    }

    /// An exhausted codec reconnects onto one with a CPU rung, never onto itself.
    #[test]
    fn an_exhausted_codec_reconnects_only_onto_one_with_a_cpu_rung() {
        let sw = software_decodable_codecs();
        assert_eq!(sw, CODEC_H264 | CODEC_AV1, "M8's CPU rung set");
        assert_eq!(sw & CODEC_HEVC, 0, "software HEVC is what M8 dropped");

        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Retry { caps: CODEC_H264 }
        );
        // Survivors stay on the table — the host picks; we only ever remove.
        assert_eq!(
            last_rung_verdict(
                CODEC_HEVC,
                CODEC_H264 | CODEC_HEVC | CODEC_AV1,
                RungLoss::Codec
            ),
            LastRungVerdict::Retry {
                caps: CODEC_H264 | CODEC_AV1
            }
        );
        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
                if let LastRungVerdict::Retry { caps } =
                    last_rung_verdict(negotiated, advertised, RungLoss::Codec)
                {
                    assert_eq!(caps & negotiated, 0, "{negotiated:#x} re-offered");
                    // A codec with no CPU rung must not be offered again.
                    assert_eq!(caps & !software_decodable_codecs(), 0);
                    assert_ne!(caps, 0, "Retry must carry something to advertise");
                }
            }
        }
        // PyroWave never demotes; if it reached this rule the answer must be Dead.
        assert_eq!(
            last_rung_verdict(CODEC_PYROWAVE, CODEC_PYROWAVE, RungLoss::Codec),
            LastRungVerdict::Dead
        );
    }

    /// A shape the CPU rung cannot decode is not "this codec has no CPU rung":
    /// a 4:4:4 H.264 session must retry onto HEVC.
    #[test]
    fn a_shape_refusal_may_retry_onto_a_codec_with_no_cpu_rung() {
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Shape),
            LastRungVerdict::Retry { caps: CODEC_HEVC }
        );
        // Same inputs as `Codec`: hardware H.264 exhausted and no CPU H.264 — HEVC is the same losing bet.
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        // PyroWave opt-in survives a shape refusal, but never alone: `resolve_codec` would pick nothing.
        assert_eq!(
            last_rung_verdict(
                CODEC_H264,
                CODEC_H264 | CODEC_HEVC | CODEC_PYROWAVE,
                RungLoss::Shape
            ),
            LastRungVerdict::Retry {
                caps: CODEC_HEVC | CODEC_PYROWAVE
            }
        );
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_PYROWAVE, RungLoss::Shape),
            LastRungVerdict::Dead
        );
        // A shape refusal still never re-offers the codec that raised it.
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
                if let LastRungVerdict::Retry { caps } =
                    last_rung_verdict(negotiated, advertised, RungLoss::Shape)
                {
                    assert_eq!(caps & negotiated, 0, "{negotiated:#x} re-offered");
                    assert_ne!(caps, 0, "Retry must carry something to advertise");
                }
            }
        }
    }

    /// A software pin has no HEVC rung, so HEVC must leave the advertisement before Hello.
    #[test]
    fn a_software_pin_takes_hevc_off_the_advertisement() {
        // Same precedence as `Decoder::new` (env first). Skip if the override is set.
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        assert!(decode_pinned_to_software("software"));
        assert!(!decode_pinned_to_software("auto"));
        assert!(!decode_pinned_to_software("vulkan"));
        assert!(!decode_pinned_to_software(""));
    }

    /// Stored `vulkan` / `vaapi` / `d3d11va` map onto native pins. An unknown
    /// named rung is a hard error; the `native-*` names must not move.
    #[test]
    fn a_pre_m10_decoder_preference_migrates_onto_its_native_rung() {
        for (stored, want) in [
            ("vulkan", "native-vulkan"),
            ("vaapi", "native-vaapi"),
            ("d3d11va", "native-d3d11va"),
        ] {
            assert_eq!(migrate_decoder_pref(stored), want, "stored {stored:?}");
        }
        // Everything else passes through, including an unknown value — it must
        // reach the ladder unchanged rather than become a silent hardware pin.
        for pass in [
            "auto",
            "",
            "hardware",
            "software",
            "native-vulkan",
            "native-vaapi",
            "native-d3d11va",
            "something-else",
        ] {
            assert_eq!(migrate_decoder_pref(pass), pass, "passthrough {pass:?}");
        }
        // Migrated names are the pin constants the ladder compares against.
        assert_eq!(migrate_decoder_pref("vulkan"), "native-vulkan");
        #[cfg(target_os = "linux")]
        assert_eq!(
            migrate_decoder_pref("vaapi"),
            crate::video_vaapi_native::DECODER_PIN
        );
        #[cfg(windows)]
        assert_eq!(
            migrate_decoder_pref("d3d11va"),
            crate::video_d3d11_native::DECODER_PIN
        );
        // Migrated `vulkan` is a name `native_vulkan_gate` admits.
        assert!(native_vulkan_gate(
            &migrate_decoder_pref("vulkan"),
            CODEC_H264,
            true,
            VIDEO_CODEC_OP_DECODE_H264
        ));
    }

    /// Presenter uploads with no stride: the copy undoes decoder padding, and a
    /// short plane is refused rather than read past.
    #[test]
    fn planar_frames_are_tightly_packed_and_short_planes_are_refused() {
        let color = ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        // 4×2 luma, 2×1 chroma, all planes padded by 3 bytes per row.
        let y: Vec<u8> = vec![1, 2, 3, 4, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9];
        let u: Vec<u8> = vec![10, 11, 9, 9, 9];
        let v: Vec<u8> = vec![20, 21, 9, 9, 9];
        let none = punktfunk_core::reanchor::LocalRecovery::NONE;
        let f =
            CpuPlanarFrame::from_i420(4, 2, [&y, &u, &v], [7, 5, 5], color, true, none).unwrap();
        assert_eq!(f.plane(0), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f.plane(1), &[10, 11]);
        assert_eq!(f.plane(2), &[20, 21]);
        assert_eq!(f.plane_dims(0), (4, 2));
        assert_eq!(f.plane_dims(1), (2, 1));
        // Odd dimensions round chroma up; dropping the last sample would read past the plane.
        assert_eq!(CpuPlanarFrame::chroma_dims(5, 3), (3, 2));
        // A short plane is a geometry disagreement, not something to truncate.
        let short: Vec<u8> = vec![1, 2, 3];
        assert!(
            CpuPlanarFrame::from_i420(4, 2, [&short, &u, &v], [7, 5, 5], color, true, none)
                .is_err()
        );
        // A stride narrower than the picture is the same disagreement.
        assert!(
            CpuPlanarFrame::from_i420(4, 2, [&y, &u, &v], [2, 5, 5], color, true, none).is_err()
        );
    }

    fn decode_device(vendor_id: u32, device_name: &str) -> VulkanDecodeDevice {
        VulkanDecodeDevice {
            get_instance_proc_addr: 0,
            instance: 0,
            physical_device: 0,
            device: 0,
            vendor_id,
            device_name: device_name.into(),
            graphics_qf: 0,
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

    /// An `Ok` clears the demotion streak only when it proves the rung works.
    /// Concealment with no picture proves nothing: interleaved `Err`s must still
    /// reach the threshold, and a forever-concealing rung must still have an escape.
    #[test]
    fn only_an_answer_that_proves_the_rung_works_clears_the_demotion_streak() {
        assert!(clears_demotion_streak(true, false));
        assert!(clears_demotion_streak(true, true));
        // Clean `Ok(None)` is proof (buffered, or an H.265 RASL skip).
        assert!(clears_demotion_streak(false, false));
        assert!(!clears_demotion_streak(false, true));

        // Alternating driver errors with concealment must still reach the threshold.
        let mut fails = 0u32;
        for concealed_ok in [false, true, false, true, false] {
            if concealed_ok {
                if clears_demotion_streak(false, true) {
                    fails = 0;
                }
            } else {
                fails += 1; // driver verdict Err
            }
        }
        assert!(
            fails >= VAAPI_DEMOTE_AFTER,
            "three driver errors interleaved with concealment must still reach the \
             demotion threshold — they got to {fails}"
        );

        // A re-anchor wait produces no picture; every AU of it is an error, so
        // a rung that never recovers reaches the threshold.
        let mut fails = 0u32;
        for errored in [true; 5] {
            // failing AU, then four skipped ones
            if errored {
                fails += 1;
            } else if clears_demotion_streak(false, false) {
                fails = 0;
            }
        }
        assert!(fails >= VAAPI_DEMOTE_AFTER);

        // Counterfactual: answering skipped AUs as clean `Ok(None)` zeroes the
        // streak and `VAAPI_DEMOTE_AFTER` is unreachable. That is the AV1
        // film-grain-without-profile path in `NativeVulkanDecoder::new`.
        let mut fails = 0u32;
        for errored in [true, false, false, true, false, false, true, false, false] {
            if errored {
                fails += 1;
            } else if clears_demotion_streak(false, false) {
                fails = 0;
            }
        }
        assert!(
            fails < VAAPI_DEMOTE_AFTER,
            "a recovery wait answered as a CLEAN AU zeroes the streak once per frame \
             — which is why it must not be answered that way; it got to {fails}"
        );
    }

    /// Auto is Vulkan-first on NVIDIA and AMD; Intel/unknown take VAAPI or D3D11VA.
    /// A Vulkan streak still demotes to hardware, so this cannot strand a box on software.
    #[test]
    fn vulkan_first_on_nvidia_and_amd_only() {
        assert!(decode_device(0x10DE, "NVIDIA GeForce RTX 5070 Ti").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD RADV VANGOGH").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Custom GPU 0405 (RADV VANGOGH)").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Radeon RX 7800 XT (RADV NAVI32)").prefer_vulkan_first());
        assert!(
            !decode_device(0x8086, "Intel(R) Arc(tm) A770 Graphics (DG2)").prefer_vulkan_first()
        );
        // Discrete Arc advertises Vulkan Video and must still land on D3D11VA in auto.
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) B580 Graphics").prefer_vulkan_first());
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) Pro Graphics").prefer_vulkan_first());
    }

    /// AV1 is advertised on a hardware fact, never on a decoder existing.
    /// Negotiation happens once; there is no falling back afterwards.
    #[test]
    fn av1_is_advertised_only_where_hardware_can_decode_it() {
        assert!(!av1_hardware_decodable(None));

        // `video_decode` alone is not AV1: plenty of devices decode H.264/H.265 only.
        let mut dev = decode_device(0x10de, "no-av1");
        dev.decode_video_caps = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
        #[cfg(not(windows))]
        assert!(
            !av1_hardware_decodable(Some(&dev)),
            "H.264+H.265 decode support says nothing about AV1"
        );

        // The AV1 operation bit is the yes.
        let mut dev = decode_device(0x1002, "vangogh-ish");
        dev.decode_video_caps =
            VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265 | VIDEO_CODEC_OP_DECODE_AV1;
        assert!(av1_hardware_decodable(Some(&dev)));

        // No decode queue: caps bits are not a claim.
        let mut dev = decode_device(0x1002, "no-decode-queue");
        dev.decode_video_caps = VIDEO_CODEC_OP_DECODE_AV1;
        dev.video_decode = false;
        #[cfg(not(windows))]
        assert!(!av1_hardware_decodable(Some(&dev)));
    }

    /// A pin with stray whitespace is still a pin. `"native-vulkan "` matched
    /// no gate arm and fell through to `auto`. Same two inputs as `decode_pinned_to_software`.
    #[test]
    fn a_decoder_pin_survives_the_whitespace_a_shell_script_adds() {
        assert_eq!(
            resolve_decoder_pref(Some("native-vulkan "), "auto"),
            "native-vulkan",
            "a trailing space must not turn a pin into an unrecognised value"
        );
        assert_eq!(
            resolve_decoder_pref(Some("  software\t"), "auto"),
            "software"
        );
        // Trimmed to nothing is absent, not a pin to `""` (the auto family).
        assert_eq!(
            resolve_decoder_pref(Some("   "), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(
            resolve_decoder_pref(Some(""), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(resolve_decoder_pref(None, "native-vaapi"), "native-vaapi");
        // The trimmed value is what the gate admits.
        assert!(
            native_vulkan_gate(
                &resolve_decoder_pref(Some("native-vulkan "), "auto"),
                punktfunk_core::quic::CODEC_HEVC,
                true,
                VIDEO_CODEC_OP_DECODE_H265,
            ),
            "the whole point: the trimmed pin reaches the gate and is admitted"
        );
    }

    #[test]
    fn native_vulkan_gate_admits_pin_and_auto_family_per_codec_on_a_capable_family() {
        // Pin the spec values, not the implementation constants: a typo'd bit
        // would refuse every real driver and native would never engage.
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H264, 0x1,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR"
        );
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H265, 0x2,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR"
        );
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_AV1, 0x4,
            "VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR"
        );
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const H265_OP: u32 = VIDEO_CODEC_OP_DECODE_H265;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        for choice in ["native-vulkan", "auto", "", "hardware"] {
            // Pin and auto family admit both codecs pf-vkdecode speaks.
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H265_OP),
                "{choice:?}"
            );
            // A family that runs both still admits each codec.
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            // Each codec needs its own bit.
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, H265_OP),
                "{choice:?}"
            );
            // AV1 is in the auto family. The pin is not a licence to skip the device leg.
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, H264_OP | H265_OP | AV1_OP),
                "{choice:?}"
            );
            // An AV1 session on a family without the AV1 op would create a session the family cannot run.
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, false, AV1_OP),
                "{choice:?}"
            );
            // No Vulkan-Video-capable presenter device.
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, false, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, false, H265_OP),
                "{choice:?}"
            );
            // Caps bit is the codec gate, not `video_decode`.
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, 0),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, 0),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, AV1_OP),
                "{choice:?}"
            );
        }
        // Never for an explicit other-backend pin. Legacy spellings never reach this gate.
        for choice in ["native-vaapi", "native-d3d11va", "software"] {
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, H265_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, true, AV1_OP),
                "{choice:?}"
            );
        }
        // Construction sites `expect()` this map: a codec admitted with no decoder would panic.
        assert_eq!(
            native_codec(CODEC_H264).map(|(c, _)| c),
            Some(NativeCodec::H264)
        );
        assert_eq!(
            native_codec(CODEC_HEVC).map(|(c, _)| c),
            Some(NativeCodec::H265)
        );
        // AV1 has a decoder here. Whether `auto` may pick it is the gate, not this map.
        assert_eq!(
            native_codec(CODEC_AV1),
            Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1))
        );
        assert!(native_codec(CODEC_PYROWAVE).is_none());
        assert!(native_codec(0).is_none());
    }

    /// Which rung/codec pairs have decoded on hardware. A table nobody checks
    /// drifts into a table that says everything is fine.
    #[test]
    fn the_evidence_table_says_exactly_which_rungs_have_run_on_hardware() {
        for (rung, codec, what) in [
            (
                NativeRung::Vulkan,
                CODEC_H264,
                "native Vulkan H.264 (M2 WP-D)",
            ),
            (NativeRung::Vulkan, CODEC_HEVC, "native Vulkan H.265 (M3)"),
            (
                NativeRung::Vulkan,
                CODEC_AV1,
                "native Vulkan AV1 (M7, RTX 5070 Ti)",
            ),
            (NativeRung::D3d11va, CODEC_H264, "native D3D11VA H.264 (M5)"),
            (NativeRung::D3d11va, CODEC_HEVC, "native D3D11VA H.265 (M5)"),
            (
                NativeRung::D3d11va,
                CODEC_AV1,
                "native D3D11VA AV1 (M7, RTX 3500 Ada + Intel Arc, 2026-08-07)",
            ),
        ] {
            assert!(
                native_evidence(rung, codec).verified,
                "{what} has hardware parity recorded"
            );
        }
        for (rung, codec, why) in [
            (
                NativeRung::Vaapi,
                CODEC_H264,
                "no VAAPI device has run this leg",
            ),
            (
                NativeRung::Vaapi,
                CODEC_HEVC,
                "no VAAPI device has run this leg",
            ),
            (
                NativeRung::Vaapi,
                CODEC_AV1,
                "VAAPI decoded AV1 on RDNA3 but has no parity check",
            ),
            (
                NativeRung::Software,
                CODEC_H264,
                "openh264 never ran on glass",
            ),
            (
                NativeRung::Software,
                CODEC_AV1,
                "rav1d has decoded on glass but has no parity check and no soak",
            ),
        ] {
            assert!(
                !native_evidence(rung, codec).verified,
                "{why} — claiming otherwise is the dishonesty this program must not ship"
            );
        }
        // An unknown codec leg is unverified, never a neighbour's evidence.
        assert!(!native_evidence(NativeRung::Vulkan, CODEC_PYROWAVE).verified);
        assert!(!native_evidence(NativeRung::Software, CODEC_HEVC).verified);
        assert!(!native_evidence(NativeRung::D3d11va, 0).verified);
        // Every answer explains itself in the session log.
        for rung in [
            NativeRung::Vulkan,
            NativeRung::D3d11va,
            NativeRung::Vaapi,
            NativeRung::Software,
        ] {
            for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1, 0] {
                assert!(
                    !native_evidence(rung, codec).note.is_empty(),
                    "{} / {codec} must carry a note",
                    rung.name()
                );
            }
        }
    }

    /// An unproven rung still runs where only the CPU is below it, and it is named.
    /// Which of them `auto` may pick first is [`native_rung_admitted`].
    #[test]
    fn every_rung_runs_and_the_unproven_ones_are_named() {
        let unproven = [
            (NativeRung::Vaapi, CODEC_H264),
            (NativeRung::Vaapi, CODEC_HEVC),
            (NativeRung::Vaapi, CODEC_AV1),
        ];
        for (rung, codec) in unproven {
            let e = native_evidence(rung, codec);
            assert!(
                !e.verified,
                "{} / {codec:#x} is claimed proven — if a hardware run really happened, \
                 move it into the verified half of the table on purpose",
                rung.name()
            );
            assert!(
                e.note.contains("NEVER") || e.note.contains("never"),
                "{} / {codec:#x}: the note is what the session log prints at warn — it \
                 must name plainly what this pair has NEVER had, whether that is a \
                 hardware run at all (VAAPI H.264/H.265) or the parity check that would \
                 promote it (VAAPI AV1, which HAS decoded), got {:?}",
                rung.name(),
                e.note
            );
        }
        // Proven pairs stay proven. Deleting the filter must not relabel them.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
            (NativeRung::D3d11va, CODEC_AV1),
        ] {
            assert!(native_evidence(rung, codec).verified, "{}", rung.name());
        }
    }

    /// `auto` yields an unproven rung only to proven code. Wrong pixels leave
    /// only through the error streak, so the choice is made before the session runs.
    #[test]
    fn an_unproven_rung_yields_to_a_proven_one_and_to_nothing_else() {
        // Linux Intel/unknown: VAAPI first, proven Vulkan under it — `auto` takes none of them.
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(
                !native_rung_admitted(NativeRung::Vaapi, codec, Some(NativeRung::Vulkan)),
                "codec {codec:#x}: a never-run VAAPI rung must not go first when the \
                 device can run the proven Vulkan rung for it"
            );
            // Admitted when nothing proven is below it (after Vulkan, or when Vulkan cannot run this codec).
            assert!(
                native_rung_admitted(NativeRung::Vaapi, codec, None),
                "codec {codec:#x}: with only the CPU below, the unproven rung runs"
            );
            // Yields to proven code, not to the unverified CPU rung.
            assert!(native_rung_admitted(
                NativeRung::Vaapi,
                codec,
                Some(NativeRung::Software)
            ));
        }
        // A proven rung is admitted whatever is below it.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
            (NativeRung::D3d11va, CODEC_AV1),
        ] {
            for below in [
                None,
                Some(NativeRung::Vulkan),
                Some(NativeRung::Vaapi),
                Some(NativeRung::Software),
            ] {
                assert!(
                    native_rung_admitted(rung, codec, below),
                    "{} / {codec:#x} is proven and must run",
                    rung.name()
                );
            }
        }
        // CPU is last everywhere, so it always runs. A codec it cannot decode is [`last_rung_verdict`].
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(native_rung_admitted(NativeRung::Software, codec, None));
        }
    }

    /// "Vulkan is below me" is a claim about this GPU. Without it, Linux Intel
    /// would bar VAAPI on a box whose Vulkan device cannot decode this codec.
    #[test]
    fn the_rung_below_must_be_one_this_device_can_actually_run() {
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        // Mesa/Intel: a decode family that advertises this codec.
        assert!(native_vulkan_usable(CODEC_H264, true, H264_OP));
        // No Vulkan Video, or a family that runs some other codec: neither is a rung to fall onto.
        assert!(!native_vulkan_usable(CODEC_H264, false, H264_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, AV1_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, 0));
        // A codec no native rung speaks is not a Vulkan rung.
        assert!(!native_vulkan_usable(CODEC_PYROWAVE, true, u32::MAX));
        // Linux Intel, both ways: H.264 on an H.264-capable device takes Vulkan;
        // AV1 on that device has no proven rung below VAAPI, so VAAPI runs.
        let below =
            |wire, caps| native_vulkan_usable(wire, true, caps).then_some(NativeRung::Vulkan);
        assert!(!native_rung_admitted(
            NativeRung::Vaapi,
            CODEC_H264,
            below(CODEC_H264, H264_OP)
        ));
        assert!(native_rung_admitted(
            NativeRung::Vaapi,
            CODEC_AV1,
            below(CODEC_AV1, H264_OP)
        ));
    }

    /// Advertised codecs are our rungs, not a decoder-library registry. The
    /// Hello is a promise: moving the set would renegotiate every session.
    #[test]
    fn advertised_codecs_describe_our_rungs_and_not_libavcodecs_registry() {
        let bits = decodable_codecs();
        assert_eq!(
            bits,
            CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            "the three codecs the native rungs speak"
        );
        assert_eq!(
            bits & CODEC_PYROWAVE,
            0,
            "pyrowave rides decodable_codecs_for"
        );
        // CPU codecs are a subset, except HEVC: the one advertised codec with no CPU rung.
        assert_eq!(
            software_decodable_codecs() & !bits,
            0,
            "a codec with a CPU rung but no advertisement would be unreachable"
        );
        assert_eq!(
            bits & !software_decodable_codecs(),
            CODEC_HEVC,
            "HEVC is the ONE advertised codec with no CPU rung (last_rung_verdict owns it)"
        );
    }
}
