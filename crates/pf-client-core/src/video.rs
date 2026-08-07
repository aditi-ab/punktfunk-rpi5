//! Video decode: reassembled access units → frames for the presenter.
//!
//! # The ladder (M10: native only)
//!
//! Every rung is a NATIVE rung — pf-vkdecode, pf-dxvadec, pf-vaadec, openh264/rav1d — for
//! every codec and on both desktop OSes. **There is no libavcodec in this crate at all**:
//! M10 deleted the three FFmpeg-backed rungs (`video_vulkan`, `video_vaapi`, the
//! libavcodec half of `video_d3d11`), the `video_libav` helpers they shared, `pf-ffvk` and
//! the `ffmpeg-next` dependency itself. Vendor order is unchanged
//! ([`VulkanDecodeDevice::prefer_vulkan_first`]):
//!
//! | Linux, NVIDIA/AMD | Linux, Intel/unknown | Windows, NVIDIA/AMD | Windows, Intel/unknown |
//! |---|---|---|---|
//! | native-vk → native-vaapi → sw | native-vaapi → native-vk → sw | native-vk → native-d3d11va → sw | native-d3d11va → native-vk → sw |
//!
//! M9's evidence FILTER survives, narrowed to the one thing it can still protect
//! ([`native_rung_admitted`]). The filter kept a rung that had never decoded on real
//! hardware out of `auto` while its proven libavcodec twin was one step below. With the
//! twins deleted that is usually no longer the situation: below native-d3d11va's AV1 leg,
//! and below native-vaapi on NVIDIA/AMD, there is nothing proven left to fall onto, so
//! barring the rung would not move a session one rung DOWN — it would take hardware decode
//! away from it entirely, which is the worse answer.
//!
//! **One column of that table is different, and it is the one the filter still guards.**
//! On Linux, Intel and every unknown vendor id run `native-vaapi → native-vk → sw`
//! ([`VulkanDecodeDevice::prefer_vulkan_first`] is true for NVIDIA and AMD only), so the
//! rung directly below the never-run pf-vaadec is native Vulkan Video — H.264 and H.265 on
//! three drivers plus a 92-minute soak, AV1 250/250. There, barring the unproven rung moves
//! the session exactly one rung down, onto proven code, so it is barred: an unproven rung
//! yields to a rung that is BOTH verified for this codec and usable on THIS device, and to
//! nothing else. A pin still reaches it — that is how the missing evidence gets generated.
//!
//! Where a rung is admitted unproven, every session SAYS so: the "decode rung active" line
//! carries `hardware_verified` and the evidence note verbatim, and it is a **warning** when
//! no hardware has ever decoded through the rung/codec pair the session just chose
//! ([`log_rung`], [`native_evidence`]). That table below is what a field report about M10
//! has to be read against.
//!
//! # Evidence — which rungs have actually decoded on hardware
//!
//! Recorded here because it is the fact a support engineer needs when a session log names
//! a rung. [`native_evidence`] is the same table in code; it is what every session logs at
//! decoder construction (`hardware_verified` / `evidence` on the "decode rung active"
//! line).
//!
//! | rung | module | codecs | hardware that has decoded on it |
//! |---|---|---|---|
//! | native Vulkan Video | [`crate::video_vk_native`] | H.264 | **yes** — bit-exact vs libavcodec, 250/250 AUs on three drivers + a 92-minute soak (M2 WP-D) |
//! | native Vulkan Video | | H.265 (Main / Main10 / 4:4:4) | **yes** — same parity run + HDR chain and Deck/VanGogh legs (M3) |
//! | native Vulkan Video | | AV1 | **yes** — 250/250 bit-identical to libavcodec on an RTX 5070 Ti (M7); ONE vendor, no soak |
//! | native D3D11VA | [`crate::video_d3d11_native`] | H.264, H.265 | **yes** — frame-hash parity on an RTX 4090 and an AMD iGPU + a 30-minute soak (M5) |
//! | native D3D11VA | | AV1 | **NO** — has never decoded a frame anywhere (M7 wired it; the box was unavailable) |
//! | native VAAPI | [`crate::video_vaapi_native`] | H.264, H.265, AV1 | **NO** — has never decoded a frame anywhere (M6/M7; no VAAPI hardware was reachable) |
//! | software | `video_software` | H.264, AV1 | **NO on glass** — openh264 + rav1d, CPU unit tests only (M8) |
//!
//! The software rung's evidence is recorded for the same reason but does not gate
//! anything: it is the LAST rung, so there is nothing below it to protect.
//!
//! # The rungs
//!
//! * **native Vulkan Video** (`video_vk_native`): pf-vkdecode's H.264/H.265/AV1 decoders
//!   on the PRESENTER's own VkDevice — the decoded VkImage feeds its CSC pass directly,
//!   zero copy. Admission is [`native_vulkan_gate`].
//! * **native D3D11VA** (`video_d3d11_native`, Windows): pf-dxvadec plans driven into
//!   `ID3D11VideoDecoder`, filling the field-proven shareable-texture hand-off ring in
//!   `crate::video_d3d11`.
//! * **native VAAPI** (`video_vaapi_native`, Linux): pf-vaadec plans driven into a
//!   dlopen'd libva, exporting DRM-PRIME dmabufs. NVIDIA has no usable VAAPI at all
//!   (nvidia-vaapi-driver is broken for this — Moonlight blacklists it), so device
//!   creation simply fails there and the ladder walks on.
//! * **Software**: the CPU rung, FFmpeg-free since M8 — openh264 for H.264, rav1d
//!   (dav1d) for AV1, planes uploaded straight to the presenter's planar CSC pass. It is
//!   the LAST rung, so it never demotes further; and it has no HEVC decoder at all (none
//!   exists under a permissive licence), which is a REFUSAL that reconnects the session
//!   onto a codec this client can decode — see [`last_rung_verdict`] and
//!   [`NoSoftwareRung`].
//!
//! The host encodes zero-reorder streams (no B-frames, in-band parameter sets on every
//! IDR), so decode is strictly one-in/one-out on every rung.
//!
//! Windows has no VAAPI (DRM-PRIME is a Linux concept) and Linux no DXVA; the vendor
//! order differs for one reason worth keeping in view: Intel's Windows driver DOES
//! advertise Vulkan Video (Arc drivers since 2023), but Vulkan decode on it strobed and
//! burned the frame budget (B580 field report, 2026-07 — measured on the FFmpeg-Vulkan
//! rung of the day) where DXVA streamed clean, so Intel/unknown take DXVA first and
//! NVIDIA/AMD keep Vulkan first. Everything dmabuf-shaped is
//! `cfg(target_os = "linux")`-gated inline.
//!
//! # Overrides
//!
//! `PUNKTFUNK_DECODER=native-vulkan|native-d3d11va|native-vaapi|software`. A pin skips
//! the vendor order, which is how a lab run reaches a rung `auto` would not pick on this
//! device; an init failure still logs and falls through to the standard ladder, so a pin
//! can never cost a session its decoder.
//!
//! The pre-M10 spellings `vulkan`/`vaapi`/`d3d11va` named libavcodec's rungs specifically.
//! They MIGRATE onto the native rung for the same hardware family, loudly
//! ([`migrate_decoder_pref`]) — they are not developer-only strings, every desktop
//! Settings UI offered them, and refusing them would end a session over a dropdown the
//! user picked long ago.

// `bail!` has exactly one site left and it is Windows-only (the D3D11VA rung's win32
// external-memory refusal), so the import is gated with it rather than allowed dead.
#[cfg(windows)]
use anyhow::bail;
use anyhow::Result;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub use crate::video_color::{csc_rows, ColorDesc};
/// Re-exported so the SESSION layer (and its tests) can name the refusal by type — the
/// module itself stays private, like every other backend's.
pub use crate::video_software::NoSoftwareRung;
use crate::video_software::SoftwareDecoder;
use crate::video_vk_native::{NativeCodec, NativeVulkanDecoder};

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
    /// The SOFTWARE rung's output (M8): tightly-packed 8-bit I420 planes for the
    /// presenter to upload and run its planar CSC pass over.
    ///
    /// It REPLACES the old `Cpu(CpuFrame)` RGBA variant rather than joining it — there is
    /// exactly one CPU rung, and the swscale conversion it used to carry (and its BT.601
    /// default) is what M8 deleted. Adding a second CPU variant would have
    /// bought the [`DecodedImage::NativeDmabuf`] property below for a distinction that
    /// does not exist: no `stats:` tag, no presenter path and no consumer would ever have
    /// been able to reach the old one.
    Cpu(CpuPlanarFrame),
    /// The NATIVE VAAPI rung's output (`pf-vaadec` + `video_vaapi_native`, M6): dmabuf
    /// fds plus a plane layout, exported DRM-PRIME from libva.
    ///
    /// It shared this payload type with a `Dmabuf` variant — libavcodec's VAAPI hwaccel,
    /// deleted at M10 — and was kept SEPARATE from it deliberately, which is what the
    /// name still records. That was not fastidiousness: the two D3D11VA rungs shared one
    /// variant (they shared the hand-off ring on purpose) and the consequence had to be
    /// fixed in `1573a987` — the `stats:` decode-path tag is derived from the variant, so
    /// a "native" soak could silently have been an FFmpeg soak with nothing in the log to
    /// tell. The name is load-bearing for the same reason today: `native-vaapi` is the
    /// tag downstream tooling reads, and renaming the variant would rename that.
    #[cfg(target_os = "linux")]
    NativeDmabuf(DmabufFrame),
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
    /// Native Vulkan Video output (pf-vkdecode — auto's top rung for H.264, HEVC and
    /// AV1; pinnable via `PUNKTFUNK_DECODER=native-vulkan`): a decoded image +
    /// per-plane views already on the PRESENTER's device, zero copy — no import, no
    /// staging, no interop. The picture format is the
    /// stream's, carried on the frame ([`NativeVkFrame::vk_format`] — NV12 for H.264,
    /// HEVC Main and AV1 Main 8-bit, P010 for Main 10, the two-plane 4:4:4 formats for
    /// RExt and AV1 High), never assumed. The presenter waits the frame's timeline
    /// pair, transitions the layer for sampling and BACK to
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
    /// Frames that decoded CORRECTLY and were then discarded without ever being
    /// shown, because the backend's deliverable queue overflowed
    /// (`video_vk_native::MAX_DELIVERABLE` — a decoder making more pictures
    /// display-ready per access unit than the pump can take one at a time).
    ///
    /// Deliberately its own number and not folded into any of the three above:
    /// nothing was damaged, nothing was refused and no driver failed, so counting
    /// it as any of those would put a damage report on a healthy stream — and the
    /// AU it happened on still showed a picture, so it must not extend
    /// [`Self::run`] either. But it cannot be nothing at all: a session quietly
    /// discarding a frame per AU is one running at half the frame rate it thinks
    /// it is, and before this counter existed it read as perfectly clean.
    ///
    /// Structurally 0 on every rung but native Vulkan — it is the only one with a
    /// deliverable queue — and not on the session stats line today; the
    /// rate-limited `warn` at the drop site is the field signal, and this is the
    /// number a stats field would read.
    pub dropped: u64,
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

    /// Note one correctly-decoded frame discarded unshown — see [`Self::dropped`].
    ///
    /// Separate from [`Self::note`] because it is not an AU verdict: several frames
    /// can be dropped within one access unit, and the access unit itself may well
    /// have shipped a picture. It touches nothing but its own counter, and in
    /// particular never [`Self::run`], which answers "did the picture come back"
    /// and here it did.
    pub(crate) fn note_dropped(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

/// A raw `VkFormat` code point, carried across the ash-free boundary.
///
/// A newtype rather than a bare `i32` because the hardware frame type
/// ([`NativeVkFrame`]) carries OTHER `i32`s — `poc` foremost — and the presenter's
/// colour-math lookup takes exactly one number. Handed the wrong
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
/// available check would be its own table against itself — which stays green if
/// pf-vkdecode grows a fifth output format (12-bit RExt) that the CSC pass has no
/// depth mapping for. This crate sees both, so the fact crosses here.
pub fn native_picture_formats() -> Vec<RawVkFormat> {
    pf_vkdecode::OUTPUT_FORMATS
        .iter()
        .map(|f| RawVkFormat(f.as_raw()))
        .collect()
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
/// signal (the write-back the decoder must wait before reusing the image).
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
/// analog of the VAAPI rung's [`DrmFrameGuard`]. The presenter holds the frame (and so
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
    /// The picture's own `VkFormat`: what the image was created with and what
    /// [`Self::plane_views`] alias.
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
    /// for the 10-bit ones — the presenter's planar CSC sampling contract.
    pub plane_views: [u64; 2],
    pub layer: u32,
    /// The layout the layer is in when the semaphore signals; the presenter must
    /// return it there after sampling (see [`NativeVkLayout`]).
    pub layout: NativeVkLayout,
    /// Timeline pair (raw `VkSemaphore` + value): pixels are ready when the semaphore
    /// reaches the value — the presenter waits it on the GPU (submit wait list), never
    /// on the host.
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
    /// view. That is the 1088-row lesson (field report 2026-07-31): at 1080p the pool
    /// is 1088 rows tall because 1080 is not a multiple of 16, encoders fill the extra
    /// rows by replicating the picture's last line, and sampling `0..1` without the
    /// ratio smears that line over the bottom of the image. Same class as the D3D11VA
    /// source-rect clamp in `crate::video_d3d11`, which shows as a green bar there only
    /// because DXVA padding is left uninitialized rather than replicated.
    pub coded_width: u32,
    pub coded_height: u32,
    /// Crop origin within the coded picture. Punktfunk hosts emit origin crops only;
    /// the presenter's UV-scale path assumes (0,0) and a nonzero origin would show the
    /// wrong window — carried so that assumption is checkable, not silent.
    pub crop_x: u32,
    pub crop_y: u32,
    /// Colour signalling, read from the SPS active for THIS picture (the H.264/H.265
    /// VUI → H.273 code points, with E.2.1's "unspecified" inference where the VUI is
    /// silent) — per frame, because the host switches HDR in-band; "unspecified"
    /// resolves to the BT.709-limited SDR default (`csc_rows`' documented fallback).
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

impl DecodedImage {
    /// Whether the frame is an intra keyframe (IDR) — the pump's re-anchor signal after
    /// a loss.
    ///
    /// Every rung answers this from the BITSTREAM now (pf-bitstream's NALU/OBU walk),
    /// which is both earlier than a decoder's own flag and — unlike libavcodec's
    /// `AV_FRAME_FLAG_KEY`, which this used to be read from — not the whole story:
    /// libavcodec flags key only for a true IDR, never for an *intra-refresh recovery
    /// point* (H.264 needs `recovery_frame_cnt == 0`; HEVC clears the flag on every
    /// non-IRAP frame regardless of the recovery-point SEI). An intra-refresh host
    /// (NVENC/AMF/QSV) heals the picture over N P-frames and flags nothing, so this
    /// alone would freeze the pump until `session.rs`'s `REANCHOR_FREEZE_MAX` backstop
    /// forced a real IDR — the very IDR the wave exists to avoid. That is what
    /// [`Self::local_recovery`] answers, from the SEI, and it is one of the things the
    /// native rungs bought.
    pub fn is_keyframe(&self) -> bool {
        match self {
            DecodedImage::Cpu(f) => f.keyframe,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.keyframe,
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
    /// Only the rungs with their OWN parser can answer: a platform decoder parses the
    /// SEI internally and surfaces nothing of it (MediaCodec, VideoToolbox — and
    /// libavcodec, whose `AV_FRAME_FLAG_KEY` is IDR-only, back when it was a rung here).
    /// That is the native Vulkan rung and — since
    /// M8 — the CPU rung's H.264 leg, which plans every AU with the same `H264Planner`
    /// and folds the SEI with the same `RecoveryWatch`. Everyone else reports
    /// [`LocalRecovery::NONE`](punktfunk_core::reanchor::LocalRecovery::NONE) and
    /// the pump's re-anchor behaviour on those lanes is byte-for-byte what it was.
    ///
    /// ⚠ The CPU rung reports no [`Self::decode_order`], so its mark cannot be dated
    /// against the pump's arm the way the native rung's is. It does not need to be:
    /// openh264 is one-AU-in, at-most-one-picture-out with no DPB flush that replays
    /// pictures decoded before a loss, which is the only thing that ordinal defends
    /// against.
    pub fn local_recovery(&self) -> punktfunk_core::reanchor::LocalRecovery {
        match self {
            DecodedImage::NativeVk(f) => f.recovery,
            DecodedImage::Cpu(f) => f.recovery,
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
            DecodedImage::NativeDmabuf(f) => (f.width, f.height),
            #[cfg(windows)]
            DecodedImage::D3d11(f) => (f.width, f.height),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
            DecodedImage::NativeVk(f) => (f.width, f.height),
        }
    }
}

/// One software-decoded picture as 8-bit 4:2:0 PLANES (M8) — Y, Cb and Cr back to back
/// in one allocation, every plane tightly packed at its own width.
///
/// "Tightly packed" is a load-bearing invariant, not a convenience: the presenter uploads
/// the buffer with a single `copy_nonoverlapping` and three `vkCmdCopyBufferToImage`
/// regions with `bufferRowLength = 0`, so a padded row here would shear the picture. The
/// decoders' own strides (openh264 pads for SIMD, dav1d aligns) are undone once, in
/// [`Self::from_i420`], which is also the only copy this rung makes per frame — where the
/// old RGBA path made a full swscale conversion pass and then handed over 4 bytes per
/// pixel instead of 1.5.
///
/// Colour is NOT applied here. The planes carry the stream's own Y′CbCr and `color`
/// carries what the bitstream said about it; the presenter's planar CSC shader converts
/// with [`csc_rows`], the same coefficients every hardware rung's frames go through. That
/// is the whole point of the milestone: there is no second CSC implementation on this
/// lane to get the matrix or the range wrong.
pub struct CpuPlanarFrame {
    pub width: u32,
    pub height: u32,
    /// Y, then Cb, then Cr — see [`Self::plane`].
    data: Vec<u8>,
    /// Byte offset of each plane's first row in [`Self::data`].
    offsets: [usize; 3],
    /// Signalling of the source frame, read from the bitstream (not from the decoder —
    /// see `video_software`'s module docs). Drives the CSC matrix/range AND, for a PQ
    /// stream, the presenter's tone-map mode.
    pub color: ColorDesc,
    /// Intra keyframe (IDR) — the pump's post-loss re-anchor signal. See
    /// [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// What this frame's AU said about intra-refresh RECOVERY — the same
    /// `pf-vkdecode` [`RecoveryWatch`](pf_vkdecode::RecoveryWatch) fold the native rung
    /// runs, over the same `AuPlan`. [`Self::keyframe`] cannot answer for an
    /// intra-refresh session (the wave emits no IDR), so without this the pump freezes
    /// until its 500 ms backstop forces the very IDR the wave exists to avoid.
    ///
    /// H.264 only: AV1 carries no equivalent SEI (see `video_software`'s AV1 leg), so
    /// that half reports [`LocalRecovery::NONE`](punktfunk_core::reanchor::LocalRecovery)
    /// and behaves exactly as it did.
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
}

impl CpuPlanarFrame {
    /// Chroma plane size for 4:2:0, rounding UP — an odd luma dimension still has a
    /// chroma sample covering its last row/column, and rounding down would drop it.
    pub fn chroma_dims(width: u32, height: u32) -> (u32, u32) {
        (width.div_ceil(2), height.div_ceil(2))
    }

    /// Plane `i` (0 = Y, 1 = Cb, 2 = Cr), tightly packed.
    pub fn plane(&self, i: usize) -> &[u8] {
        let (w, h) = self.plane_dims(i);
        let start = self.offsets[i];
        &self.data[start..start + (w * h) as usize]
    }

    /// Plane `i`'s size in samples — `(width, height)` for luma, the 4:2:0 halves for
    /// chroma. The presenter sizes its plane images from this.
    pub fn plane_dims(&self, i: usize) -> (u32, u32) {
        if i == 0 {
            (self.width, self.height)
        } else {
            Self::chroma_dims(self.width, self.height)
        }
    }

    /// Copy a decoder's strided I420 output into one tightly-packed allocation.
    ///
    /// Refuses rather than truncates: a plane the decoder reported shorter than its own
    /// geometry means the decoder and we disagree about the picture, and reading the rows
    /// that ARE there would produce a plausible-looking picture over uninitialized
    /// memory.
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
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor signal. See
    /// [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    pub guard: DrmFrameGuard,
}

#[cfg(target_os = "linux")]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Keeps a decoded surface alive until the consumer's GPU reads are done: dropping
/// it releases the surface back to its decoder's pool and closes the fds.
///
/// The consumer treats this as opaque — the presenter dups every dmabuf fd it
/// imports and simply holds the guard until its fence has been waited.
///
/// It was an ENUM until M10, because libavcodec's VAAPI hwaccel handed over a mapped
/// `AVFrame` while the native rung (`video_vaapi_native`, M6) owns a `VASurface` from its
/// own pool — widening this seam is what let the native rung exist alongside it. With the
/// FFmpeg rung deleted there is one owner left, so the enum collapses to the newtype it
/// started as, and the `unsafe impl Send` the `AVFrame` pointer needed goes with it:
/// [`VaFrameGuard`](crate::video_vaapi_native::VaFrameGuard) is owned fds plus an
/// `mpsc::Sender`, i.e. `Send` on its own terms.
#[cfg(target_os = "linux")]
pub struct DrmFrameGuard(
    /// Nothing ever READS this — the whole type is its `Drop`, which closes the exported
    /// PRIME fds and returns the surface to the decoder's pool. `dead_code` is answered
    /// here rather than by removing the field (that would release the surface at
    /// construction) or by making the type an alias (that would hand the presenter a
    /// pf-vaadec-shaped name for something it must treat as opaque).
    #[allow(dead_code)]
    pub(crate) crate::video_vaapi_native::VaFrameGuard,
);

enum Backend {
    /// Native Vulkan Video H.264/HEVC/AV1 (pf-vkdecode) on the presenter's device —
    /// auto's TOP rung on both desktop OSes, for all three codecs — every leg
    /// has hardware parity against libavcodec (see this module's evidence table) — also
    /// pinnable by name (`PUNKTFUNK_DECODER=native-vulkan`); see [`native_vulkan_gate`].
    /// The negotiated codec picks the decoder once, at construction; everything else
    /// about this backend is codec-agnostic.
    /// Boxed: the decoder (planner + shipped-frame ledger) dwarfs the other variants,
    /// same as PyroWave below.
    NativeVulkan(Box<NativeVulkanDecoder>),
    /// Native VAAPI (`pf-vaadec` + `video_vaapi_native`) — M6's replacement for
    /// libavcodec's VAAPI hwaccel, and since M10 the only VAAPI rung: libva driven
    /// straight from pf-bitstream plans, dlopen'd, exporting the same DRM-PRIME dmabufs.
    /// Reachable by pin (`PUNKTFUNK_DECODER=native-vaapi`) and by `auto` in the vendor
    /// order. ⚠ This rung has decoded NOTHING on hardware ([`native_evidence`]) — `auto`
    /// runs it where the alternative below it is the CPU, and yields to native Vulkan
    /// Video where that rung is proven for the codec and usable on the device
    /// ([`native_rung_admitted`], which is the Intel/unknown arm). Every session that
    /// lands on it says so at `warn`. Errors ride the SAME streak/demotion machinery as
    /// every other hardware rung.
    /// Boxed: the decoder (two planners, a display and a surface pool) dwarfs the
    /// other variants.
    #[cfg(target_os = "linux")]
    NativeVaapi(Box<crate::video_vaapi_native::NativeVaapiDecoder>),
    /// Native D3D11VA (`pf-dxvadec` + `video_d3d11_native`) — M5's replacement for
    /// libavcodec's D3D11VA hwaccel, and since M10 the only DXVA rung:
    /// `ID3D11VideoDecoder` driven from pf-bitstream plans, filling the shareable-RGBA
    /// hand-off ring in `crate::video_d3d11`.
    /// Reachable by pin (`PUNKTFUNK_DECODER=native-d3d11va`) and by `auto` in the vendor
    /// order: its H.264/H.265 legs have hardware parity + a soak (M5); its AV1 leg has
    /// decoded nothing anywhere and runs with the warning [`log_rung`] emits. Errors
    /// ride the SAME streak/demotion machinery as every other hardware rung.
    /// Boxed: the decoder (two planners plus a session) dwarfs the other variants.
    #[cfg(windows)]
    NativeD3d11va(Box<crate::video_d3d11_native::NativeD3d11Decoder>),
    /// PyroWave (wired-LAN wavelet codec): pyrowave compute on the presenter's device
    /// (Linux + Windows — same Vulkan presenter on both). No demotion
    /// rung — there is no other decoder for it.
    /// Boxed: the decoder (pinned create-info hold + plane ring) dwarfs the other variants.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(Box<crate::video_pyrowave::PyroWaveDecoder>),
    /// The CPU rung (M8: openh264 / rav1d). Last in every ladder, so it
    /// never demotes — and the only rung that can fail to EXIST for a codec, which is a
    /// different answer from failing to decode: see [`last_rung_verdict`].
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
    /// The negotiated codec as the WIRE states it ([`punktfunk_core::quic`]'s `CODEC_*`
    /// bit, from the host's Welcome) — the ONE codec vocabulary this crate speaks since
    /// M10 deleted `ffmpeg::codec::Id` along with the rungs that needed it. Every rung
    /// map ([`native_codec`], [`native_d3d11_codec`], [`native_vaapi_codec`]), the
    /// evidence table and the software rung's refusal are keyed on it, so a mid-session
    /// demotion rebuilds for the SAME codec by construction rather than by translation.
    wire_codec: u8,
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
    /// session the rung BELOW it. Reset on every backend swap.
    ///
    /// ⚠ Read by nothing but [`Decoder::decode_frame`]'s streak accounting since M10:
    /// the arm that consumed it — "a native Vulkan rung that never delivered demotes to
    /// FFmpeg-Vulkan rather than past it" — is gone with FFmpeg-Vulkan itself, and the
    /// property it protected now holds structurally, because the rung directly below
    /// native Vulkan IS the next candidate the walk tries.
    delivered: bool,
    /// The presenter's device, kept so the demotion walk can build a native Vulkan
    /// decoder mid-stream. Cloned once per session; its handles outlive every pump
    /// (see [`VulkanDecodeDevice`]).
    vk: Option<VulkanDecodeDevice>,
    /// The negotiated picture shape, kept for the same reason `vk` is: the
    /// demotion walk can build a NATIVE rung mid-stream, and every native constructor
    /// takes it as its device probe ([`StreamFormat`]).
    stream: StreamFormat,
    /// Which hardware rungs this session has actually RUN — [`RUNG_BIT_NATIVE_VULKAN`]
    /// and [`RUNG_BIT_NATIVE_PLATFORM`], set the moment a rung is installed.
    ///
    /// It is what makes the demotion walk TERMINATE now that two native rungs can
    /// demote into each other (the ladder is `native → other native → software`, and
    /// the two orders are opposite per vendor — so
    /// without this a Vulkan⇄platform pair could hand the session back and forth
    /// forever, one error streak at a time, and never reach the CPU rung that would at
    /// least show a picture). A rung already entered is never re-entered.
    entered_rungs: u8,
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

/// The native VULKAN rung ran this session — see [`Decoder::entered_rungs`].
const RUNG_BIT_NATIVE_VULKAN: u8 = 1 << 0;
/// The native PLATFORM rung (VAAPI on Linux, D3D11VA on Windows) ran this session.
const RUNG_BIT_NATIVE_PLATFORM: u8 = 1 << 1;

/// Which [`Decoder::entered_rungs`] bit a backend claims — 0 for the rungs that cannot
/// be a demotion TARGET twice (software is terminal; PyroWave never demotes at all), so
/// they need no bookkeeping.
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
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR` — its H.265 sibling.
const VIDEO_CODEC_OP_DECODE_H265: u32 = 0x0000_0002;

/// `VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR`. The Deck's VanGogh advertises
/// it alongside H.264/H.265/VP9; it is what [`av1_hardware_decodable`] reads and,
/// since M7, the caps bit [`native_codec`] demands for an AV1 session.
const VIDEO_CODEC_OP_DECODE_AV1: u32 = 0x0000_0004;

/// The native decoder for a negotiated wire codec, plus the
/// `VkVideoCodecOperationFlagBitsKHR` the presenter's decode family must advertise
/// for it — or `None` for a codec pf-vkdecode cannot decode natively.
///
/// The two are returned together on purpose: "which decoder" and "which caps bit"
/// are one fact, and splitting them is how a gate ends up admitting HEVC on an
/// H.264-only decode family (`vkCreateVideoSessionKHR` for a codec operation the
/// family cannot run is undefined behaviour, not an error).
///
/// ⚠ Being here is "pf-vkdecode has a decoder", NOT "the automatic ladder may pick
/// it": [`native_vulkan_gate`] holds that decision, and it reads this map for the
/// codec/caps pair only.
///
/// The key is the WIRE bit ([`punktfunk_core::quic`]'s `CODEC_*`), which since M10 is
/// the only codec vocabulary in this crate — the `ffmpeg::codec::Id` it used to be
/// died with the last libavcodec rung.
fn native_codec(wire: u8) -> Option<(NativeCodec, u32)> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some((NativeCodec::H264, VIDEO_CODEC_OP_DECODE_H264)),
        punktfunk_core::quic::CODEC_HEVC => Some((NativeCodec::H265, VIDEO_CODEC_OP_DECODE_H265)),
        punktfunk_core::quic::CODEC_AV1 => Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1)),
        _ => None,
    }
}

/// The native DXVA decoder for a negotiated wire codec, or `None` for one pf-dxvadec
/// cannot decode. No caps bit accompanies it (unlike [`native_codec`]): DXVA advertises
/// support as a profile GUID on the adapter, which
/// [`crate::video_d3d11_native::NativeD3d11Decoder::new`] checks directly against the
/// device it is about to build on — there is no device-level "which codecs" flag to
/// consult first.
#[cfg(windows)]
fn native_d3d11_codec(wire: u8) -> Option<pf_dxvadec::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_dxvadec::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_dxvadec::Codec::H265),
        // AV1 (M7). It was not a widening of what this client can decode — the FFmpeg
        // D3D11VA rung of the day already decoded AV1 Profile 0 through the same profile
        // GUID — but the native rung had to cover it, or dropping FFmpeg (M10, this
        // milestone) would have dropped a codec.
        punktfunk_core::quic::CODEC_AV1 => Some(pf_dxvadec::Codec::Av1),
        _ => None,
    }
}

/// The native VAAPI decoder for a negotiated wire codec, or `None` for one pf-vaadec
/// cannot decode. Like its DXVA twin there is no caps bit to consult first: VAAPI
/// advertises support as a profile/entrypoint pair on the DISPLAY, which
/// [`crate::video_vaapi_native::NativeVaapiDecoder::new`] queries on the device it is
/// about to build on.
#[cfg(target_os = "linux")]
fn native_vaapi_codec(wire: u8) -> Option<pf_vaadec::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_vaadec::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_vaadec::Codec::H265),
        // AV1 (M7) — same reasoning as the DXVA map above.
        punktfunk_core::quic::CODEC_AV1 => Some(pf_vaadec::Codec::Av1),
        _ => None,
    }
}

/// One NATIVE decode rung, named so the evidence table and the admission rule can talk
/// about rungs without naming a [`Backend`] variant (whose set is per-platform).
///
/// The CPU rung is here for completeness of the table only — see [`native_evidence`].
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
    /// The name this rung goes by in logs and in `PUNKTFUNK_DECODER` — the same strings
    /// the `stats:` decode-path tag uses, so a log line and a stats line name one thing.
    pub fn name(self) -> &'static str {
        match self {
            NativeRung::Vulkan => "native-vulkan",
            NativeRung::D3d11va => "native-d3d11va",
            NativeRung::Vaapi => "native-vaapi",
            NativeRung::Software => "software",
        }
    }
}

/// What HARDWARE has actually decoded a frame on a given rung/codec pair — the fact M9's
/// default flip turns on, written down where it cannot rot.
///
/// This is deliberately not a confidence score or a tier. It answers exactly one
/// question — *has a real GPU ever produced a picture through this code path* — because
/// that is the question the admission rule needs and the one a support engineer reading
/// `hardware_verified=false` on a session's "decode rung active" line needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RungEvidence {
    /// A real device has decoded frames through this rung/codec pair and the result was
    /// checked (frame-hash parity, a soak, or both).
    pub verified: bool,
    /// WHAT hardware, in one line — or, when `verified` is false, why there is none. Goes
    /// verbatim into the session log so a report carries its own provenance.
    pub note: &'static str,
}

/// The evidence table (this module's docs hold the readable copy), keyed by rung and WIRE
/// codec.
///
/// Wire bits rather than `ffmpeg::codec::Id` — deliberately, back when there still was such
/// a vocabulary here: this is a fact about punktfunk's own decode lanes, and keying it on
/// FFmpeg's ids would have meant re-keying it at M10. It did not have to be re-keyed.
///
/// An unknown codec for a rung answers `verified: false` — the safe direction: a rung
/// grows a codec leg before anyone runs it on hardware, and the default must be "no
/// evidence", not "inherits its neighbour's".
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
        (NativeRung::D3d11va, CODEC_AV1) => (
            false,
            "NEVER decoded a frame on any hardware - wired in M7, the box was unavailable",
        ),
        (NativeRung::Vaapi, _) => (
            false,
            "NEVER decoded a frame on any hardware - no VAAPI device was reachable (M6/M7)",
        ),
        (NativeRung::Software, CODEC_H264 | CODEC_AV1) => (
            false,
            "never run on glass - openh264/rav1d have CPU unit tests only (M8)",
        ),
        _ => (false, "no hardware run recorded for this rung and codec"),
    };
    RungEvidence { verified, note }
}

/// Can THIS device run the native Vulkan rung for this wire codec at all?
///
/// [`native_vulkan_gate`] without its `choice` half — the same device facts, asked by the
/// callers that need to know what is BELOW them rather than what they are about to build:
/// [`native_rung_admitted`]'s callers, and (spelled inline, with `"auto"`) the mid-session
/// demotion walk. One function so that "the Vulkan rung is available on this box" cannot
/// come to mean two different things inside one file.
pub fn native_vulkan_usable(wire: u8, video_decode: bool, decode_video_caps: u32) -> bool {
    native_vulkan_gate("auto", wire, video_decode, decode_video_caps)
}

/// May `auto` pick this native rung for this wire codec, given what sits directly BELOW it
/// on THIS device?
///
/// One sentence: **an unproven rung yields to a proven one, and to nothing else.**
///
/// * a rung/codec pair WITH hardware evidence is admitted, always — that is what the
///   evidence was collected for;
/// * a pair without it is admitted unless the rung the ladder would fall onto instead is
///   itself verified for this codec AND usable on this device.
///
/// `below` is that rung, or `None` when nothing usable is left below it (the CPU). It is
/// the CALLER's to compute, because "what is below me" is the vendor order's answer, not
/// this function's: it differs per platform and, on Linux, per vendor id.
///
/// Where the rule bites, and where it deliberately does not:
///
/// * **Linux, Intel and every unknown vendor id.** The order is `native-vaapi →
///   native-vk → sw`, so the rung under the never-run pf-vaadec is native Vulkan Video,
///   proven for all three codecs. Barring VAAPI there moves the session ONE rung down onto
///   proven code, so it is barred — and it stays reachable below Vulkan (the same ladder
///   reaches it again if Vulkan can't be built) and by pin.
/// * **Everything else.** Below the unproven rung is the CPU. Trading hardware decode for
///   software decode to avoid an unproven decoder is the worse answer, so those rungs run,
///   with the warning [`log_rung`] emits. That includes Windows Intel/unknown, where the
///   rung below native-d3d11va IS native Vulkan Video on paper: that vendor family is the
///   one thing in this program with a MEASURED wrong-pixel report against Vulkan decode
///   (the B580, see [`Decoder::new`]), and "has never run" is not a reason to move a
///   session onto "known to strobe here". Callers say so where they pass `None`.
///
/// ⚠ This governs `auto` ONLY. An explicit `PUNKTFUNK_DECODER=` pin bypasses it exactly as
/// it bypasses the vendor order — a pin is how a lab run reaches a rung `auto` will not
/// pick, and taking that away would leave no way to GENERATE the missing evidence. The
/// mid-session demotion walk bypasses it too: there the rung above has already failed
/// repeatedly, so unproven hardware is the only hardware left to try.
pub fn native_rung_admitted(rung: NativeRung, wire: u8, below: Option<NativeRung>) -> bool {
    native_evidence(rung, wire).verified
        || !below.is_some_and(|b| native_evidence(b, wire).verified)
}

/// The native Vulkan Video admission gate (WP-C of the native-decode program, widened by
/// the 2026-08-05 ladder decision, by M3 WP-2's HEVC wiring and by M7's AV1 wiring): the
/// pf-vkdecode backend engages when `choice` asks for it, by name
/// (`PUNKTFUNK_DECODER=native-vulkan` — `choice` is env-first, so that's what carries it)
/// or as the auto family (`auto`/``/`hardware`), where native is auto's TOP rung for
/// every codec it speaks.
///
/// A native INIT failure falls through to the platform's own native rung, so admission
/// can never cost a session its decoder at start. A runtime error streak demotes like
/// every hardware rung's streaks do. Every explicit OTHER-backend pin refuses here; the
/// pre-M10 `vulkan` spelling reaches this gate already rewritten to `native-vulkan`
/// ([`migrate_decoder_pref`]), so it admits, which is the point of the migration.
///
/// Beyond the choice: the negotiated wire codec must be one pf-vkdecode speaks —
/// H.264, H.265 or AV1 ([`native_codec`]) — and the presenter's decode family must
/// advertise THAT codec's decode operation. `video_decode` alone proves the extension
/// stack, never the codec: an AV1-only decode family exists on real hardware, and
/// H.264-only ones are the common case on older silicon.
///
/// What the gate deliberately does NOT check is the stream's picture SHAPE — that is
/// [`NativeVulkanDecoder::new`]'s construction-time probe, which has the negotiated
/// chroma format and bit depth and can ask the device directly. Keeping it there keeps
/// this decision pure (and CPU-testable) while still refusing before a decoder exists.
fn native_vulkan_gate(choice: &str, wire: u8, video_decode: bool, decode_video_caps: u32) -> bool {
    let Some((_, codec_op)) = native_codec(wire) else {
        return false;
    };
    let chosen = matches!(choice, "native-vulkan" | "auto" | "" | "hardware");
    chosen && video_decode && decode_video_caps & codec_op != 0
}

/// The `quic::CODEC_*` bit's human name — for logs, errors and the user-visible
/// reconnect toast. `?` for a bit this build does not know, which is honest: an unknown
/// codec must not print as one of the known ones.
pub fn wire_codec_name(wire: u8) -> &'static str {
    match wire {
        punktfunk_core::quic::CODEC_H264 => "H.264",
        punktfunk_core::quic::CODEC_HEVC => "HEVC",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        punktfunk_core::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "?",
    }
}

/// The `quic` codec bits this build can decode ON THE CPU — the ladder's last rung, and
/// therefore the set a session is guaranteed to survive to the end of.
///
/// One function so the answer cannot drift between the rung that refuses (the software
/// backend's own codec map) and the rule that decides what to reconnect as
/// ([`last_rung_verdict`]).
pub fn software_decodable_codecs() -> u8 {
    punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_AV1
}

/// What to do when the last rung has no decoder for the session's codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastRungVerdict {
    /// Reconnect advertising these caps instead — they are non-empty, and they exclude
    /// the codec that just ran out of rungs, so the host must pick something else.
    Retry { caps: u8 },
    /// Nothing is left to advertise: every codec this client offered has now exhausted
    /// its rungs. Reconnecting would negotiate the same dead end, so the session ends
    /// and says why.
    Dead,
}

/// WHY the last rung had no answer — the two diagnoses behind a [`NoSoftwareRung`], and
/// the reason [`last_rung_verdict`] needs more than "a codec failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RungLoss {
    /// The CODEC has no CPU rung in this build at all (HEVC). Every hardware rung for it
    /// has already failed, so the retry may only offer codecs that DO have a CPU rung —
    /// anything else is the same bet that just lost, one session later.
    Codec,
    /// The codec has a CPU rung; this stream's picture SHAPE is outside it (10-bit,
    /// 4:4:4). The hardware rungs are not implicated at all — nothing failed, the CPU
    /// decoder simply is not built for this picture — so every other advertised codec is
    /// a genuine candidate and filtering by [`software_decodable_codecs`] here would end
    /// sessions a plain HEVC retry would have finished.
    Shape,
}

/// The reconnect rule, in one pure function: an HEVC session whose hardware rungs are
/// exhausted must come back as a session this client can finish.
///
/// The codec is fixed at Welcome and the control stream renegotiates shard payload only,
/// so there is no in-session move available — the only lever is what the NEXT Hello
/// advertises. `advertised` is what this session offered; the answer removes `negotiated`
/// from it, plus — for a [`RungLoss::Codec`] — any other codec that would land in the
/// same hole (one whose only remaining rung is a software one that does not exist). Two
/// sessions of the same failure is the shape this rules out.
///
/// `caps` is what the retry ACTUALLY advertises: the pump derives its `exclude_codecs`
/// from this set rather than from the failed codec alone, so the wire and this verdict
/// cannot disagree (they did until the M8 review — the wire re-offered PyroWave the rule
/// had removed).
///
/// Pure and total on purpose — this is the piece that gets tested as a first-class path,
/// because the on-glass version of it costs a real host with a real GPU failure.
pub fn last_rung_verdict(negotiated: u8, advertised: u8, loss: RungLoss) -> LastRungVerdict {
    let survivors = advertised & !negotiated;
    let caps = match loss {
        // Everything still on the table that ALSO has a CPU rung underneath it.
        RungLoss::Codec => survivors & software_decodable_codecs(),
        RungLoss::Shape => survivors,
    };
    // A retry the host's precedence ladder cannot PICK is not a retry: `resolve_codec`
    // deliberately keeps PyroWave out of that ladder (it is opt-in only), so a Hello
    // whose survivors are PyroWave alone resolves to nothing and the host refuses the
    // session. Judge liveness on the pickable ones and carry the rest along.
    const PICKABLE: u8 = punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1;
    if caps & PICKABLE == 0 {
        LastRungVerdict::Dead
    } else {
        LastRungVerdict::Retry { caps }
    }
}

/// The pre-M10 decoder-preference spellings, mapped onto the rung that replaced them.
///
/// `vulkan`, `vaapi` and `d3d11va` named **libavcodec's** rungs specifically — the whole
/// point of the `native-*` names was that they were the OTHER ones — and M10 deleted them.
/// But those three are not developer-only env values: all three desktop Settings UIs
/// offered them (`clients/linux`'s "VAAPI", the WinUI shell's "Hardware (Direct3D 11 /
/// DXVA)", the console UI's list), so they sit in shipped users' settings files right now.
/// Refusing them would turn an upgrade into a dead session for anyone who ever touched
/// that dropdown, and the message would tell them to edit something they never edited.
///
/// So they MIGRATE rather than refuse, and the mapping is exact in the sense that
/// matters: the user asked for a hardware FAMILY (Vulkan Video / VAAPI / DXVA) and gets
/// that family, on the implementation that still exists. The UI labels stay true
/// word-for-word — native Vulkan Video is still Vulkan Video. What does change is the
/// failure mode: a libavcodec pin that failed to open was a hard session error, while
/// every `native-*` pin logs and falls through to the standard ladder. For a value read
/// out of a settings file that is the right direction; a pin that cannot open must not be
/// the reason a user's client stops working.
///
/// ⚠ It is `pub` and PURE — no log line — for the Settings dialogs, not for the pump.
/// Their decoder combos look the stored string up in their own preset list, and a value
/// that matches nothing shows as "Automatic"; save the dialog without touching that row
/// and the user's Vulkan/VAAPI preference is silently rewritten to `auto`. So they
/// migrate on the WAY IN too, and a function that logged would then warn once per dialog
/// open. [`Decoder::new`] does the logging, where "a session started on a migrated
/// preference" is the fact worth recording.
///
/// Nothing rewrites the STORE. The value is migrated on every read, so a user who
/// downgrades to an older client still finds the preference they set.
pub fn migrate_decoder_pref(pref: &str) -> String {
    match pref {
        "vulkan" => "native-vulkan".to_string(),
        "vaapi" => "native-vaapi".to_string(),
        "d3d11va" => "native-d3d11va".to_string(),
        _ => pref.to_string(),
    }
}

/// Is video decode PINNED to the CPU rung — the Settings "Video decoder" value, or the
/// `PUNKTFUNK_DECODER` override that wins over it?
///
/// Same precedence as [`Decoder::new`] resolves (env first, then the setting), because a
/// second reading of the same two inputs is a second place for them to drift.
pub fn decode_pinned_to_software(pref: &str) -> bool {
    resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref) == "software"
}

/// Resolve the decoder preference: the `PUNKTFUNK_DECODER` override if it carries a
/// value, else the stored setting. Pure, so the rule is testable without touching the
/// process environment — and shared, because [`Decoder::new`] and
/// [`decode_pinned_to_software`] read the same two inputs and a second reading is a
/// second place for them to drift.
///
/// **Trimmed**, which is the part that had to be fixed rather than merely factored out.
/// `PUNKTFUNK_VK_ADAPTER` already trimmed; this did not, so `"native-vulkan "` — one
/// trailing space, which a Windows `.cmd` produces for free because `echo x>> file`
/// keeps the space before the redirect — matched no arm of [`native_vulkan_gate`] and
/// fell through to `auto` SILENTLY. An operator's pin was ignored and nothing said so,
/// which is the exact failure the rest of this module's logging exists to prevent. It
/// cost a full on-glass session to find.
///
/// Whitespace-only is treated as absent, not as a pin to `""`: someone who exported the
/// variable empty means "no override", and `""` is a value `native_vulkan_gate` happens
/// to accept.
pub(crate) fn resolve_decoder_pref(env: Option<&str>, pref: &str) -> String {
    env.map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or_else(|| pref.to_string(), str::to_string)
}

/// The `quic` codec bitfield this client can decode — the union of the codecs the RUNGS
/// THIS BUILD COMPILED speak. Advertised to the host so it never emits a codec we can't
/// decode.
///
/// It used to be a libavcodec registry walk (`ffmpeg::decoder::find` per id), and M9 is
/// where that stopped being an answer to the question asked: the registry described
/// decoders that were not in the ladder. It is now what §3.6 of the plan asked for — a
/// statement about our own rungs — and it is the reason M10 could delete every FFmpeg rung
/// without renegotiating a single field session: the answer did not move, because the
/// FFmpeg rungs never covered a codec the native ones don't:
///
/// * native Vulkan Video (`video_vk_native`, both desktop OSes) decodes H.264, H.265 and
///   AV1, and it is compiled unconditionally;
/// * the platform native rungs (`video_d3d11_native` / `video_vaapi_native`) cover the
///   same three;
/// * the CPU rung ([`software_decodable_codecs`]) covers H.264 and AV1.
///
/// The three flags are constants rather than probes for the same reason they always were:
/// this is asked before a device exists (it feeds the very first Hello), so it can only
/// speak about what was BUILT. Everything device-shaped is [`decodable_codecs_for`].
///
/// ⚠ **AV1 here is a decoder EXISTING, not a decoder that can keep up.** Use
/// [`decodable_codecs_for`], which gates it on hardware — see [`av1_hardware_decodable`].
///
/// ⚠ **HEVC here is a HARDWARE decoder existing**, and since M8 that is the only kind
/// there is: the CPU rung has no HEVC ([`software_decodable_codecs`]). Advertising it
/// anyway is deliberate and is the plan's — hardware HEVC is the path most hosts and most
/// clients actually take, and refusing it up front would cost every one of them the codec
/// to protect the few whose hardware later fails. The exhaustion case is handled where it
/// happens, by [`last_rung_verdict`], and it is the ONE codec whose advertisement is a
/// promise this client cannot keep unconditionally. Where the client can KNOW in advance
/// that it cannot keep it — decode pinned to software — [`decodable_codecs_for`] drops
/// the bit before the first Hello instead.
pub fn decodable_codecs() -> u8 {
    // The native Vulkan rung's three codecs (`native_codec`'s map is the same set), plus
    // the CPU rung's — written as a union so that removing a rung's codec leg shows up
    // here rather than silently keeping the advertisement alive.
    punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1
        | software_decodable_codecs()
}

/// Can this machine decode AV1 in HARDWARE?
///
/// The question exists because "a decoder for AV1 exists" was never the same claim: back
/// when this crate linked libavcodec, `ffmpeg::decoder::find(AV1)` answered yes on every
/// build that carried libdav1d — a SOFTWARE decoder — and advertising off that answer told
/// the host "send me AV1" on machines that would then decode a 4K stream on the CPU. The
/// dependency is gone and the trap is not: this crate still HAS a CPU AV1 rung (rav1d), so
/// `decodable_codecs` still says AV1, and the wire's codec negotiation is still a promise
/// about capability made once, with nothing to fall back to once the session runs.
///
/// Answered from device facts only, never from a decoder existing:
///
/// * the presenter's Vulkan device advertises `DECODE_AV1` in its decode queue
///   family's codec operations, or
/// * (Windows) the presenter can import D3D11 textures — the native DXVA rung then decodes
///   AV1 Profile 0 through the adapter's profile GUID, and `auto` reaches it. ⚠ That leg
///   has decoded nothing on hardware ([`native_evidence`]); the session says so at `warn`.
///   Before M10 this arm was conditional, because the leg was kept out of `auto` while
///   libavcodec's DXVA rung was still below it — with that rung deleted there is no
///   condition left to write.
///
/// ⚠ Deliberately NOT consulted: VAAPI. Asking libva costs opening a display, which
/// this function is called too early and too often to do; the Vulkan bit covers the
/// Mesa devices where VAAPI AV1 exists in practice, and a machine with VAAPI AV1 but
/// no Vulkan AV1 loses only the ADVERTISEMENT, not a working path.
pub fn av1_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    if vk.is_some_and(|v| v.video_decode && v.decode_video_caps & VIDEO_CODEC_OP_DECODE_AV1 != 0) {
        return true;
    }
    // The second answer is per-platform, so it is bound to a name rather than
    // written as a cfg'd `return`: on Windows clippy calls that `needless_return`
    // and fails `-D warnings`, which NO ci leg would have caught (nothing runs
    // clippy on Windows — this surfaced only from a manual check on a box).
    #[cfg(windows)]
    let d3d11 = vk.is_some_and(|v| v.d3d11_import);
    #[cfg(not(windows))]
    let d3d11 = false;
    d3d11
}

/// [`decodable_codecs`] plus the PyroWave bit when the presenter's device passed the
/// compute-feature probe, minus the codecs `decoder_pref` makes unreachable.
/// Advertisement-only: `resolve_codec` never auto-picks PyroWave — the session must also
/// name it `preferred_codec` (plan §3), which the client does only under its explicit
/// opt-in.
pub fn decodable_codecs_for(vk: Option<&VulkanDecodeDevice>, decoder_pref: &str) -> u8 {
    let mut bits = decodable_codecs();
    // AV1 is hardware-gated (M7). Without this the bit rides on the CPU rung's mere
    // existence and the host is told to send AV1 to a machine that would decode it on
    // the CPU — and once the session is negotiated there is nothing to fall back to.
    if bits & punktfunk_core::quic::CODEC_AV1 != 0 && !av1_hardware_decodable(vk) {
        tracing::info!(
            "AV1 not advertised: no hardware AV1 decode on this device (a software \
             decoder exists, but a 4K AV1 stream is not survivable on it)"
        );
        bits &= !punktfunk_core::quic::CODEC_AV1;
    }
    // The one HEVC case the client can answer BEFORE the Hello (M8 review): decode is
    // pinned to the CPU rung, and the CPU rung has no HEVC — so the advertisement would
    // be a promise this build cannot keep for the whole session, exactly what
    // `av1_hardware_decodable` exists to stop for AV1. Every other HEVC failure is a
    // per-device fact only the session can learn, and `last_rung_verdict` answers it
    // there. Guarded on something remaining: a Hello advertising ZERO codecs reads as
    // "HEVC-only" to a host (`resolve_codec`'s pre-negotiation default), which would be
    // the precise opposite of this.
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

/// Say what `PUNKTFUNK_AU_FAULT` will do to THIS session, once, at decoder
/// construction — including the two cases where the answer is "nothing".
///
/// The knob only bites on the native VULKAN rung (its injector sits at that backend's
/// decode entry), so a lab run that armed it and landed anywhere else — a shape that
/// rung refused, a session that demoted, a PyroWave session — must be told
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

/// Name the rung a session just landed on, and say whether any hardware has ever decoded
/// a frame through it for this codec.
///
/// This is the program's honesty surface, and M10 is where it earns its keep: every rung
/// is now native, two of them have never decoded anything anywhere, and there is no
/// libavcodec twin left underneath to catch a session that lands wrong. A field report of
/// the form "M10 broke my stream" is only actionable if the log distinguishes *the rung
/// with three drivers and a 92-minute soak behind it* from *the rung nothing has ever
/// run*, and the `stats:` decode-path tag — which is a machine interface and stays
/// additive-only — names the rung but says nothing about its provenance.
///
/// So: `info` when the pair is hardware-verified, **`warn` when it is not**, with the
/// evidence string from [`native_evidence`] carried verbatim so the log explains itself
/// without a reader having to find this file.
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
        Backend::NativeVaapi(_) => (
            NativeRung::Vaapi.name(),
            Some(native_evidence(NativeRung::Vaapi, wire)),
        ),
        Backend::Software(_) => (
            NativeRung::Software.name(),
            Some(native_evidence(NativeRung::Software, wire)),
        ),
        // PyroWave is not in the evidence table and never will be: it is not a rung of
        // the H.264/H.265/AV1 ladder at all, it is its own codec with its own decoder and
        // no rung above or below it.
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
    /// `wire` is the codec the host resolved in the Welcome, as the WIRE states it
    /// ([`punktfunk_core::quic`]'s `CODEC_*` bit — never assume HEVC). It is the only
    /// codec vocabulary the ladder speaks since M10 dropped `ffmpeg::codec::Id` with the
    /// last libavcodec rung.
    /// `pref` is the Settings "Video decoder" value (`auto`/`native-vulkan`/
    /// `native-vaapi`/`native-d3d11va`/`software`; `hardware` — the WinUI shell's stored
    /// value — reads as auto).
    /// `vk` is the presenter's shared Vulkan device — decode lands as VkImages the
    /// presenter samples directly.
    /// Precedence: the `PUNKTFUNK_DECODER` env override wins (support/debug escape
    /// hatch, and the documented knob), then the setting; both default to auto.
    /// Auto's hardware order depends on the device on BOTH desktop OSes
    /// ([`VulkanDecodeDevice::prefer_vulkan_first`]). Linux: native-vk → native-vaapi →
    /// software on NVIDIA and ALL AMD (`prefer_vulkan_first` is vendor-wide —
    /// desktop RADV included, on-glass verdict — not just the Deck's VanGogh); the two
    /// swap on Intel/unknown. Windows (no VAAPI there): native-vk →
    /// native-d3d11va → software on NVIDIA/AMD, swapped on
    /// Intel/unknown (Intel's driver advertises Vulkan Video, but Vulkan decode on it
    /// strobed/overran the budget — B580 field report).
    ///
    /// On top of that order sits the evidence filter ([`native_rung_admitted`]): a rung
    /// that has never decoded a frame does not go FIRST when the rung directly below it is
    /// proven for this codec and usable on this device. That is the Linux Intel/unknown
    /// arm and only that arm — everywhere else what is below is the CPU.
    ///
    /// Whatever it lands on, the session logs `decode rung active` with the rung's name
    /// and its evidence state, and that line is a WARNING when no hardware has ever
    /// decoded a frame through the rung/codec pair the session just chose.
    ///
    /// `stream` is the picture shape the host resolved ([`StreamFormat`]). Every native
    /// rung reads it as its construction-time device probe, so a shape this GPU cannot
    /// decode refuses BEFORE the rung is chosen — where the fall-through to the next rung
    /// is a plain construction failure — instead of at the first AU, where the only exit
    /// is an error-streak demotion past it.
    pub fn new(
        wire: u8,
        pref: &str,
        vk: Option<&VulkanDecodeDevice>,
        stream: StreamFormat,
    ) -> Result<Decoder> {
        let stored = resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref);
        let choice = migrate_decoder_pref(&stored);
        if choice != stored {
            // Said once per session, at `warn`, because a developer who set the env var to
            // bisect libavcodec against native needs to know that distinction is gone —
            // and a support engineer reading a log needs to know the rung below was not
            // the one the settings file names. See [`migrate_decoder_pref`].
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
            // Whatever rung this session landed on, say what `PUNKTFUNK_AU_FAULT`
            // is going to do about it — see [`report_au_fault_env`]. Here, at the
            // one exit every backend leaves through, rather than in the native
            // Vulkan backend's constructor: a lab run whose session never REACHES that
            // constructor (a refused shape, a demotion, another rung entirely) would
            // otherwise sit silently un-faulted and read as a fault run that
            // detected nothing.
            report_au_fault_env(matches!(backend, Backend::NativeVulkan(_)));
            // ...and say WHICH rung it is and whether any hardware has ever decoded a
            // frame through it. Every rung is native now and two of them have no
            // hardware evidence at all — a session log that does not distinguish
            // those from the proven ones would make every field report about M10
            // unfalsifiable. See [`log_rung`].
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
        // The codec's human name, for every log line below — the ladder used to print
        // `?codec_id` (an `ffmpeg::codec::Id` Debug), and this is the same fact stated in
        // the vocabulary that survived M10.
        let codec_name = wire_codec_name(wire);
        // The PINS, ahead of everything, because a pin is a pin: it skips the vendor
        // order, which is how a lab run reaches a rung `auto` would not have picked on
        // this device. Any refusal or init failure logs and DEMOTES to the standard
        // ladder below exactly as if the rung had errored (choice reads as `auto` from
        // here on) — a pinned rung's failure must never be quieter, or land somewhere
        // other, than the same rung's failure inside `auto`.
        let mut choice = choice;
        // Native D3D11VA (M5, pf-dxvadec).
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
        // Native VAAPI (M6, pf-vaadec). The pin matters most here: this rung has decoded
        // nothing on any hardware, and the pin is what a lab run uses to GENERATE the
        // evidence that would change that — on a box whose vendor order puts Vulkan first
        // and where `auto` would therefore never reach it.
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
                // The gate is an AND of three, so name all three. `video_decode=true`
                // beside "refused" is otherwise unreadable: it says the device decodes
                // SOMETHING while refusing THIS codec, and the bit that would explain it
                // — the decode family's advertised operations — went unprinted. That is
                // the difference between "your GPU can't do this" and "we asked for the
                // wrong thing", and only the second is our bug.
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
        // Linux's VAAPI RUNG: native VAAPI (pf-vaadec). `auto` reaches it from two places
        // — Intel/unknown take it before Vulkan, everyone else after — so it lives here
        // once instead of twice.
        //
        // ⚠ It has decoded nothing on any hardware ([`native_evidence`]). Until M10 that
        // kept it out of `auto` while libavcodec's VAAPI hwaccel sat directly below it;
        // with that rung deleted this is the only VAAPI there is. It runs with the warning
        // `done` logs — except where the evidence filter sends the Intel/unknown arm to
        // native Vulkan Video first ([`native_rung_admitted`], at the call site below),
        // after which this closure is reached from the SECOND arm, genuinely below Vulkan.
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
            // ⚠ `choice` is unread here now, and that is the M10 change worth naming: the
            // pre-M10 `vaapi` pin meant libavcodec's rung SPECIFICALLY, so this closure
            // ended in a hard error for it. It is migrated to `native-vaapi` before the
            // ladder ever runs ([`migrate_decoder_pref`]), so by here it is either a
            // native pin (handled above, ahead of the vendor order) or the auto family.
            let _ = choice;
            Ok(None)
        };
        // Linux `auto`: try VAAPI FIRST unless this device is one where Vulkan Video
        // is the established right answer (NVIDIA — no usable VAAPI; VanGogh — VAAPI
        // chroma-fringes). Mesa now exposes decode queues by default (and the session
        // binary opts RADV in for the Deck's sake), which silently moved every desktop
        // AMD/Intel box onto Vulkan-on-Mesa — user-reported to judder/error-streak
        // (then demote to software) where explicit VAAPI streams perfectly.
        #[cfg(target_os = "linux")]
        let mut vaapi_tried = false;
        #[cfg(target_os = "linux")]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
        {
            // ⚠ The evidence filter, and the ONE arm of the whole ladder where it still
            // has somewhere to yield to ([`native_rung_admitted`]). This is the
            // Intel/unknown order, so the rung directly below VAAPI is native Vulkan
            // Video — proven for all three codecs, where pf-vaadec is proven for none. If
            // this device can actually run that rung for this codec, VAAPI does not go
            // first; the ladder falls through to Vulkan below and VAAPI keeps its place
            // UNDER it (`vaapi_tried` stays false, so the second arm still tries it when
            // Vulkan can't be built).
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
        // Windows `auto`: D3D11VA FIRST unless this device is one where Vulkan Video is
        // the established right answer (NVIDIA/AMD). Intel's Windows driver advertises
        // Vulkan Video (Arc drivers since 2023) so the capability gate alone does not
        // keep Intel off the Vulkan rung — and that combination is field-broken (B580,
        // 2026-07: strobing between clean anchors and corrupt inter frames that never
        // trips the error-streak demotion, 7 ms p50 decodes blowing the 120 Hz budget)
        // where D3D11VA — the DXVA path every Windows video player exercises, and what
        // this backend was built for — streams clean. Vulkan stays reachable below by
        // explicit preference and as auto's fallback when D3D11VA can't be built.
        //
        // ⚠ The B580 measurement was taken on the FFmpeg-Vulkan rung of the day, not on
        // pf-vkdecode, and no Intel box has run the native one. The vendor order is kept
        // as it was for exactly that reason: nothing has been measured that would justify
        // changing it, and "the old evidence no longer applies" is not evidence.
        //
        // Windows' D3D11VA RUNG: native D3D11VA (pf-dxvadec). Its H.264/H.265 legs HAVE
        // hardware evidence (parity on an RTX 4090 and an AMD iGPU plus a 30-minute soak,
        // M5); its AV1 leg has none and runs with the warning `done` logs — until M10 that
        // leg was skipped in `auto` in favour of libavcodec's DXVA rung, which no longer
        // exists. The rung needs the presenter's win32 import path or its frames could
        // never reach the screen — that check is first, once.
        #[cfg(windows)]
        let d3d11_rung = |choice: &str| -> Result<Option<Backend>> {
            let Some(v) = vk.filter(|v| v.d3d11_import) else {
                // A PIN that cannot possibly work is worth saying out loud — a DXVA frame
                // reaches the screen through the presenter's win32 import and nothing else,
                // so without it this rung would decode into a texture no one can display.
                // (`native-d3d11va` here covers the migrated `d3d11va` too — see
                // [`migrate_decoder_pref`].)
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
            // The evidence filter applies here too, and it admits — the `None` is the
            // load-bearing part, so it is stated rather than assumed. On paper the rung
            // below D3D11VA on this arm is native Vulkan Video; in fact this arm IS the
            // Intel/unknown vendor family, the one family in this program with a MEASURED
            // wrong-pixel report against Vulkan decode (the B580 note below). Falling from
            // "has never run" onto "known to strobe here" is not a fall onto proven code,
            // so what is really below the DXVA AV1 leg is the CPU — and its H.264/H.265
            // legs are verified anyway, which is what the first clause of
            // [`native_rung_admitted`] answers.
            && native_rung_admitted(NativeRung::D3d11va, wire, None)
        {
            d3d11_tried = true;
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        // The VULKAN RUNG: native Vulkan Video (pf-vkdecode). Unlike the two platform
        // rungs above it needs no closure — `auto` reaches it from exactly one place.
        // [`native_vulkan_gate`] carries the whole admission decision, including the
        // choice. Every codec leg has hardware parity against libavcodec (M2/M3 for
        // H.264/H.265, M7 for AV1 — this module's evidence table); an init failure logs
        // and falls through to the rung below.
        // (`native_tried` skips the repeat when the pin above already attempted — and
        // failed — the same construction.)
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
        // Deck/NVIDIA note: `auto` reaches the VAAPI rung here when Vulkan Video isn't
        // available (on desktop Mesa it was already tried above — `vaapi_tried` skips the
        // repeat). A presenter that can't display the dmabufs demotes this decoder to
        // software mid-session via [`Decoder::force_software`]. Windows has no VAAPI — auto
        // falls straight through to software there.
        #[cfg(target_os = "linux")]
        if choice != "software" && !vaapi_tried {
            if let Some(b) = vaapi_rung(&choice)? {
                return done(b);
            }
        }
        // Windows: the D3D11VA rung as the fallback for NVIDIA/AMD auto (Vulkan Video
        // missing or failed to open). (On Intel/unknown auto it was already tried above —
        // `d3d11_tried` skips the repeat.)
        #[cfg(windows)]
        if choice != "software" && !d3d11_tried {
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        if choice == "software" {
            // Say WHY hardware wasn't even attempted — a stored "software" preference
            // (or the env override) silently skipping the hardware rungs has burned real
            // debugging time on boxes that could do better.
            tracing::info!(
                "software decode by preference (Settings decoder / PUNKTFUNK_DECODER) — \
                 hardware decode not attempted"
            );
        }
        // `?` here can carry a `NoSoftwareRung` (an HEVC session that pinned software, or
        // one whose device offered no hardware rung at all). It stays typed all the way
        // to the pump, which turns it into the reconnect rather than a dead session —
        // see [`last_rung_verdict`].
        done(Backend::Software(SoftwareDecoder::new(wire)?))
    }

    /// Wait for a Vulkan-Video frame's GPU decode to complete (timeline semaphore) —
    /// the pump's decode-stat measurement. `false` = not a Vulkan backend, timeout, or
    /// a pair no longer in the shipped ledger / a stale session
    /// generation — every false just declines the sample.
    pub fn wait_hw_decoded(&self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        match &self.backend {
            Backend::NativeVulkan(d) => d.wait_timeline(timeline_sem, value, timeout_ns),
            _ => false,
        }
    }

    /// This session's decode-integrity counters, or `None` on a backend that has
    /// no way to answer (the CPU rung and PyroWave — see [`DecodeHealth`]).
    ///
    /// `None` and `Some(DecodeHealth::default())` are deliberately different
    /// answers, and the stats surface must keep them different: the first is "this
    /// decoder cannot see corruption", the second is "this decoder looked and saw
    /// none". Reporting the first as the second is exactly the mistake that let a
    /// field corruption run undetected for a release.
    pub fn decode_health(&self) -> Option<DecodeHealth> {
        match &self.backend {
            Backend::NativeVulkan(d) => Some(d.health()),
            // The native DXVA rung has the bitstream planner, so it sees concealment and
            // refusals — but D3D11VA exposes no per-picture status query at all, so its
            // `status_queries` is false and `failed` stays structurally 0. That is the
            // honest report: "this decoder looked at the STREAM and saw none" without
            // claiming a driver verdict nothing can produce.
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => Some(d.health()),
            // Same shape as the DXVA rung above, for the same reason: libva has no
            // per-picture decode-status query either.
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(d) => Some(d.health()),
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

    /// Open a PyroWave decoder for a `CODEC_PYROWAVE` session (plan §4.5): pyrowave
    /// compute on the presenter's device.
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
            // A PyroWave session never demotes (nothing else decodes it — a failure
            // renegotiates the codec instead), so the demotion-rebuild facts (the
            // device here, the D3D11VA ones below) are unused; keep them well-formed
            // rather than plumbing them in for nothing.
            vk: None,
            stream: StreamFormat::SDR_420_8,
            // A PyroWave session never demotes, so nothing ever reads this.
            entered_rungs: 0,
            #[cfg(windows)]
            d3d11_import: false,
            #[cfg(windows)]
            adapter_luid: None,
            #[cfg(windows)]
            d3d11_hdr10: false,
        })
    }

    /// Drain the "please ask the host for an IDR" flag — the pump calls this each iteration
    /// (throttled) so a demoted/erroring decoder can resynchronize under the infinite GOP.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Install a rung: swap the backend in and reset everything that describes the OLD
    /// one's health, in one place.
    ///
    /// It exists because M9 doubled the number of demotion targets, and every one of
    /// them has to clear the same four things — the error streak, its start stamp, the
    /// delivered flag, and (new) the [`Self::entered_rungs`] bookkeeping the walk's
    /// termination depends on. Four call sites each doing it by hand is how one of them
    /// eventually forgets the bit and the ladder starts looping.
    fn install(&mut self, backend: Backend) {
        self.entered_rungs |= rung_bit(&backend);
        self.backend = backend;
        self.vaapi_fails = 0;
        self.first_fail = None;
        self.delivered = false;
    }

    /// Is the running rung the platform's NATIVE hardware rung (native VAAPI on Linux,
    /// native D3D11VA on Windows)? The one demotion candidate that goes "sideways" —
    /// into native Vulkan — fires only from here.
    fn is_native_platform_rung(&self) -> bool {
        #[cfg(target_os = "linux")]
        let it = matches!(self.backend, Backend::NativeVaapi(_));
        #[cfg(windows)]
        let it = matches!(self.backend, Backend::NativeD3d11va(_));
        #[cfg(not(any(target_os = "linux", windows)))]
        let it = false;
        it
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
        // Same typed refusal as every other software-rung construction: on an HEVC
        // session there is nothing below this and the pump reconnects.
        self.install(Backend::Software(SoftwareDecoder::new(self.wire_codec)?));
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
            Backend::NativeVulkan(n) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = n.decode(au).map(|f| f.map(DecodedImage::NativeVk));
                // STREAM damage is not a decoder fault, and must not ride the
                // demotion streak.
                //
                // The distinction exists only because these rungs can SEE damage —
                // and that is precisely what makes it dangerous. libavcodec's rungs
                // concealed a lost reference silently and kept their job; if a
                // native rung turned the same event into an error, three of them
                // over a second would demote the program's own headline decoder
                // exactly on the lossy links it was built to diagnose. So
                // concealment comes back as `Ok(None)` plus this flag: the pump
                // still asks for a re-anchor at the same moment and through the
                // same throttle it always did, and the hardware rung survives the
                // loss that caused it.
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
            Backend::NativeVaapi(v) => {
                debug_assert!(complete, "partial AUs are pyrowave-only");
                let r = v.decode(au).map(|f| f.map(DecodedImage::NativeDmabuf));
                // Same concealment split as the Vulkan rung above, for the same reason.
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
                // Same concealment split as the Vulkan rung above, for the same reason.
                if d.take_recovery_request() {
                    self.want_keyframe = true;
                    concealed = true;
                }
                r
            }
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
                    Backend::NativeVulkan(_) => "native Vulkan Video",
                    #[cfg(windows)]
                    Backend::NativeD3d11va(_) => "native D3D11VA",
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
                    // ⚠ A native rung that never delivered a single frame is not a
                    // failing decoder — it is a decoder the session never had (usually a
                    // stream shape THIS DEVICE cannot host: `NativeVulkanDecoder::new`'s
                    // probe catches what the negotiation can see, but a level above the
                    // device's `maxLevelIdc`, or an SPS that disagrees with the Welcome,
                    // only surfaces here). Such a rung must not cost the session the rung
                    // BELOW it as well.
                    //
                    // Until M10 that needed an explicit arm: native Vulkan fell through
                    // to FFmpeg-Vulkan first, because demoting past it would have taken a
                    // 4K HEVC session on NVIDIA/Linux — where VAAPI is unusable — straight
                    // to the CPU. The arm is gone with FFmpeg-Vulkan, and the property now
                    // holds structurally: the rung directly below native Vulkan is the
                    // platform's own native rung, and that is exactly the next candidate
                    // this walk tries. `self.delivered` is kept for the streak accounting
                    // and for the record, not for a branch.
                    //
                    // The platform's NATIVE hardware rung, first, exactly as in
                    // `Decoder::new`'s ladder. `entered_rungs` is what keeps the walk
                    // monotone: the two native rungs sit in opposite orders per vendor, so
                    // without it a demotion could climb back into a rung that already failed.
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
                    // The last hardware candidate, and the one that only exists because
                    // M9 stacked two native rungs: a failing native PLATFORM rung on an
                    // Intel/unknown box has native Vulkan BELOW it (that vendor order
                    // puts the platform rung first), and there is nothing between them.
                    // Without this the rung with the weakest evidence in the whole
                    // program — native VAAPI, which has decoded nothing anywhere — would
                    // take a 4K session straight to the CPU rung the moment it
                    // error-streaked. Only fires FROM a native platform rung.
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
                    // The ladder's bottom. On H.264/AV1 this always builds; on HEVC it
                    // NEVER does, and the `?` carries the typed `NoSoftwareRung` up to
                    // the pump, which reconnects with HEVC-less caps instead of leaving
                    // the session on a rung that cannot decode a single AU. That
                    // substitution — a refusal where a silently useless decoder used to
                    // sit — is the whole reason the drop of software HEVC is safe.
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

/// Guard-less mutex serializing every `vkQueueSubmit`/`vkQueuePresentKHR`/
/// `vkQueueWaitIdle` on the device the presenter shares with the decode lane.
///
/// Why it exists: the presenter creates the device with ONE graphics-family queue, and
/// the session pump thread submits decode/CSC prep work to that SAME `VkQueue` from a
/// different thread. `vkQueueSubmit` requires external synchronization on the queue; the
/// race surfaced as intermittent `VK_ERROR_DEVICE_LOST` at exactly the moments the decode
/// lane put work on the graphics queue — decoder open and frames-context rebuild, i.e.
/// stream start and every adaptive-bitrate encoder rebuild (live-diagnosed 2026-07-09,
/// on the FFmpeg-Vulkan rung, whose `AVVulkanDeviceContext` was configured with
/// `nb_graphics_queues = 1` ⇒ queue index 0).
///
/// It is guard-less because FFmpeg's hook for this was a raw `lock_queue`/`unlock_queue`
/// callback PAIR with no RAII scope (`std::sync::Mutex`'s guard can't cross a C callback).
/// That consumer is gone; the shape stays because the presenter, the Skia overlay and the
/// native decode lane all still share the queue, and [`QueueLock::guard`] gives Rust
/// callers the RAII form. Contention is a handful of µs-scale critical sections per
/// frame; a plain Mutex+Condvar is more than enough.
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

/// The presenter's Vulkan device handles, exported so the DECODE lane runs on the SAME
/// device the presenter samples from — the whole point: the decoded VkImage is composited
/// directly, no interop, no copy (plan: Vulkan Video phase).
///
/// Plain integers/strings on purpose: pf-client-core has no ash dependency, so the
/// consumers (`video_vk_native` → pf-vkdecode, `video_pyrowave` → pyrowave-sys) cast
/// these back into handle types themselves. All handles stay valid for the presenter's
/// lifetime, which outlives every session pump (the run loop tears the pump down before
/// the presenter).
#[derive(Clone)]
pub struct VulkanDecodeDevice {
    /// `PFN_vkGetInstanceProcAddr` from the loader — the decode lanes resolve everything
    /// else through it.
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
    /// The presenter's graphics+present family.
    pub graphics_qf: u32,
    /// The video-decode family (may equal `graphics_qf` on some hardware — which is a
    /// case the native rung must detect; see `video_vk_native::submit_queues_collide`).
    pub decode_qf: u32,
    /// Raw `VkVideoCodecOperationFlagsKHR` the decode family advertises.
    pub decode_video_caps: u32,
    /// Everything enabled at instance/device creation. The pyrowave decoder replays these
    /// lists verbatim into its pinned create-info reconstruction, so they must match
    /// reality exactly.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<std::ffi::CString>,
    /// Features enabled at device creation (reported via `device_features`).
    pub f_sampler_ycbcr: bool,
    pub f_timeline_semaphore: bool,
    pub f_synchronization2: bool,
    /// Vulkan Video decode is actually usable on this device (decode queue + extensions +
    /// features). The bundle exists even without it — Windows D3D11 interop rides the
    /// same struct — so consumers gate the Vulkan decode rung on THIS, not on `Some`.
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
    /// its own submits/presents and every decode lane takes it around its own, so both
    /// sides serialize on the same queues.
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
    /// driver advertises Vulkan Video (Arc drivers since 2023), but Vulkan decode on it
    /// was field-broken (B580, 2026-07: strobing + ~7 ms decodes) where DXVA streamed
    /// clean. ⚠ That measurement was taken on the FFmpeg-Vulkan rung, which M10 deleted;
    /// no Intel box has run pf-vkdecode. The order stands until something is measured,
    /// because "the old evidence no longer applies" is not evidence.
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

    /// The reconnect rule, as the invariant it is: an exhausted codec must come back as
    /// one this client can decode ALL THE WAY DOWN, and must never come back as itself.
    ///
    /// This is the "first-class path" the risk register asks for, tested where it can be
    /// tested exhaustively — the on-glass half needs a host, a GPU and a decode failure
    /// nobody can schedule.
    #[test]
    fn an_exhausted_codec_reconnects_only_onto_one_with_a_cpu_rung() {
        let sw = software_decodable_codecs();
        assert_eq!(sw, CODEC_H264 | CODEC_AV1, "M8's CPU rung set");
        assert_eq!(sw & CODEC_HEVC, 0, "software HEVC is what M8 dropped");

        // The shipping case: a desktop advertises H.264+HEVC, HEVC runs out of rungs.
        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Retry { caps: CODEC_H264 }
        );
        // With hardware AV1 also advertised, both survivors stay on the table — the host
        // picks; we only ever REMOVE.
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
        // A client that offered HEVC alone has nowhere to go: reconnecting would
        // negotiate the same dead end, so say so instead of looping.
        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        // The retry NEVER re-offers the codec that just failed...
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
                if let LastRungVerdict::Retry { caps } =
                    last_rung_verdict(negotiated, advertised, RungLoss::Codec)
                {
                    assert_eq!(caps & negotiated, 0, "{negotiated:#x} re-offered");
                    // ...and, when the CODEC is what has no CPU rung, never offers one
                    // that would reach the same refusal a session later.
                    assert_eq!(caps & !software_decodable_codecs(), 0);
                    assert_ne!(caps, 0, "Retry must carry something to advertise");
                }
            }
        }
        // PyroWave is not in the software set and never reaches this rule (its sessions
        // renegotiate the codec on failure instead of demoting) — but if it ever did, the
        // answer must be Dead, not a retry that offers a codec with no CPU decoder.
        assert_eq!(
            last_rung_verdict(CODEC_PYROWAVE, CODEC_PYROWAVE, RungLoss::Codec),
            LastRungVerdict::Dead
        );
    }

    /// A picture SHAPE the CPU rung cannot decode is not "this codec has no CPU rung",
    /// and the review found the rule conflating them: a 4:4:4 H.264 session ended with
    /// "no other codec is available" while an HEVC retry — whose hardware rungs never
    /// even ran — would have worked.
    #[test]
    fn a_shape_refusal_may_retry_onto_a_codec_with_no_cpu_rung() {
        // The one that used to die. HEVC has no CPU rung, but nothing about HEVC failed:
        // this client asked for 4:4:4, the host resolved it, and only the CPU DECODER is
        // 4:2:0-only. A reconnect without H.264 re-resolves the shape too.
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Shape),
            LastRungVerdict::Retry { caps: CODEC_HEVC }
        );
        // Same inputs, the OTHER diagnosis: hardware H.264 exhausted and the CPU rung
        // has no H.264 at all (impossible in this build, but the rule must not depend on
        // that) — then HEVC really is the same losing bet and the session ends.
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        // The user's PyroWave opt-in survives a shape refusal — but never ALONE: the
        // host's `resolve_codec` keeps PyroWave out of its precedence ladder, so a Hello
        // offering nothing else resolves to no codec and the host refuses the session.
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
        // And a shape refusal still never re-offers the codec that raised it — the codec
        // is fixed at Welcome, so it is the only lever there is.
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

    /// The one HEVC promise the client can refuse to make BEFORE the Hello: decode
    /// pinned to software has no HEVC rung at any level, so advertising it guarantees
    /// the reconnect flow rather than risking it.
    #[test]
    fn a_software_pin_takes_hevc_off_the_advertisement() {
        // The pin is read the way `Decoder::new` reads it: env first, then the setting —
        // so a run with the override actually set has nothing here to assert about.
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        assert!(decode_pinned_to_software("software"));
        assert!(!decode_pinned_to_software("auto"));
        assert!(!decode_pinned_to_software("vulkan"));
        assert!(!decode_pinned_to_software(""));
    }

    /// A settings file written before M10 must still stream.
    ///
    /// This is the upgrade path, not a nicety: all three desktop Settings UIs offered
    /// `vulkan` / `vaapi` / `d3d11va` as decoder choices, so those strings are sitting in
    /// shipped users' stores right now. They named **libavcodec's** rungs, which M10
    /// deleted — and the pre-M10 code answered an unavailable named rung with a hard
    /// error. Left as-is, an upgrade would have ended every one of those sessions with a
    /// message about a decoder the user never chose by that name.
    ///
    /// So each maps onto the rung that replaced it, and the mapping is checked as a pair:
    /// the LEGACY name must move, and the `native-*` names must NOT (they were always the
    /// exact pins, and a migration that rewrote them would be a second bug).
    #[test]
    fn a_pre_m10_decoder_preference_migrates_onto_its_native_rung() {
        for (stored, want) in [
            ("vulkan", "native-vulkan"),
            ("vaapi", "native-vaapi"),
            ("d3d11va", "native-d3d11va"),
        ] {
            assert_eq!(migrate_decoder_pref(stored), want, "stored {stored:?}");
        }
        // Everything else is passed through verbatim — including `auto`/``/`hardware`
        // (the auto family `native_vulkan_gate` matches on), `software`, the three exact
        // pins, and a value this build has never heard of, which must reach the ladder
        // unchanged so it falls through to the CPU rung rather than becoming a silent
        // hardware pin.
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
        // The migrated names are exactly the pin constants the ladder compares against —
        // a typo here would read as "some unknown decoder" and land the session on auto.
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
        // …and the migrated Vulkan name is one `native_vulkan_gate` admits, on a device
        // that can run the codec. (`decode_pinned_to_software` above pins the other
        // direction: none of these is the software pin.)
        assert!(native_vulkan_gate(
            &migrate_decoder_pref("vulkan"),
            CODEC_H264,
            true,
            VIDEO_CODEC_OP_DECODE_H264
        ));
    }

    /// `CpuPlanarFrame` is what the presenter uploads with no stride: prove the copy
    /// really does undo the decoder's padding, and that a short plane is REFUSED rather
    /// than read past.
    #[test]
    fn planar_frames_are_tightly_packed_and_short_planes_are_refused() {
        let color = ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        // 4x2 luma, 2x1 chroma, all planes padded by 3 bytes per row.
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
        // Odd dimensions round the chroma plane UP — the last column/row still has a
        // chroma sample and dropping it would read past the plane on the next frame.
        assert_eq!(CpuPlanarFrame::chroma_dims(5, 3), (3, 2));
        // A plane shorter than its own geometry is a disagreement with the decoder, not
        // something to truncate into a plausible picture.
        let short: Vec<u8> = vec![1, 2, 3];
        assert!(
            CpuPlanarFrame::from_i420(4, 2, [&short, &u, &v], [7, 5, 5], color, true, none)
                .is_err()
        );
        // A stride narrower than the picture is the same class of disagreement.
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

        // ---- The AV1 shape (M7), and the reason its recovery wait is an `Err` ----
        //
        // A native rung waiting to re-anchor after a failure produces no picture for
        // every AU of the wait, and all three codecs say so with an ERROR: H.264 and
        // H.265 through their planners' `PlanError::AwaitingIdr`, AV1 through
        // `VkDecodeError::AwaitingKeyAv1`. So the streak ticks for the whole wait and
        // a rung that never recovers reaches the threshold.
        let mut fails = 0u32;
        for errored in [true; 5] {
            // the failing AU, then four skipped ones
            if errored {
                fails += 1;
            } else if clears_demotion_streak(false, false) {
                fails = 0;
            }
        }
        assert!(fails >= VAAPI_DEMOTE_AFTER);

        // The counterfactual is the whole point, and it is what the AV1 rung was
        // first wired as: answer the skipped AUs with a CLEAN `Ok(None)` instead —
        // no picture, no warnings, nothing to object to — and every one of them
        // clears the streak. The `Err` from each failure is then alone, and
        // `VAAPI_DEMOTE_AFTER` is unreachable no matter how long the session runs.
        //
        // The stream this strands is real and named in `NativeVulkanDecoder::new`:
        // an AV1 sequence with `film_grain_params_present = 1` on a device without
        // the grain decode profile fails at `ensure_state` — at EVERY key frame, and
        // only at a key frame. Key frame `Err`, inter frames "clean", next key frame
        // `Err`: a frozen screen for the whole session, `refused N · damaged 0 ·
        // run 0` on the stats line, and the `!delivered` fall-through to
        // FFmpeg-Vulkan below never reached.
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

    /// AV1 is advertised on a HARDWARE fact, never on a decoder existing.
    ///
    /// The standing open item M7 closes. `ffmpeg::decoder::find(AV1)` says yes
    /// wherever libdav1d is linked, so the old advertisement told the host "send me
    /// AV1" on machines that would then decode it on the CPU — and codec negotiation
    /// happens once, so there is no falling back afterwards.
    #[test]
    fn av1_is_advertised_only_where_hardware_can_decode_it() {
        // No device at all: no claim.
        assert!(!av1_hardware_decodable(None));

        // A decode-capable device that does NOT list AV1 among its codec
        // operations. `video_decode` alone is not the question — plenty of devices
        // decode H.264 and H.265 and no AV1.
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

        // A device whose decode queue is absent cannot be taken at its caps word.
        let mut dev = decode_device(0x1002, "no-decode-queue");
        dev.decode_video_caps = VIDEO_CODEC_OP_DECODE_AV1;
        dev.video_decode = false;
        #[cfg(not(windows))]
        assert!(!av1_hardware_decodable(Some(&dev)));
    }

    /// The native-Vulkan admission gate (WP-C, widened by the 2026-08-05 ladder
    /// decision, by M3 WP-2's HEVC wiring and by M7's AV1 wiring): the pin AND the auto
    /// family admit on a capable H.264, HEVC **or AV1** session, every explicit
    /// other-backend pin refuses (a `native-vaapi` pin must not land on Vulkan just
    /// because the device could), and the
    /// codec/device legs still refuse for every choice. The codec's OWN caps bit is the
    /// device leg: admitting HEVC on an H.264-only decode family would create a video
    /// session for an operation the family cannot run, which is undefined behaviour
    /// rather than an error.
    /// A pin with stray whitespace is still a pin, and the gate must accept it.
    ///
    /// This is a regression test with a field cost: `"native-vulkan "` (one trailing
    /// space, which a Windows `.cmd` adds for free) matched no arm of
    /// `native_vulkan_gate`, so the rung fell through to `auto` with nothing logged —
    /// on a box where `auto` picks a different rung, that reads exactly like the pin
    /// being refused for a hardware reason. The second half is what makes it a *shared*
    /// rule: `decode_pinned_to_software` reads the same variable, and its own docs say
    /// a second reading is a second place to drift.
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
        // Trimmed to nothing means ABSENT — fall back to the stored setting rather than
        // pinning to "", which the gate would otherwise accept as the auto family.
        assert_eq!(
            resolve_decoder_pref(Some("   "), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(
            resolve_decoder_pref(Some(""), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(resolve_decoder_pref(None, "native-vaapi"), "native-vaapi");
        // …and the trimmed value is what the gate actually admits.
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
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_AV1, 0x4,
            "VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR"
        );
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const H265_OP: u32 = VIDEO_CODEC_OP_DECODE_H265;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        for choice in ["native-vulkan", "auto", "", "hardware"] {
            // The pin and the whole auto family admit both codecs pf-vkdecode
            // speaks, on a family that advertises the matching op…
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H265_OP),
                "{choice:?}"
            );
            // …including the ordinary case of a family that runs both.
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            // Each codec needs ITS OWN bit: an H.264-only family (the common case on
            // older silicon) must not take an HEVC session, and vice versa.
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, H265_OP),
                "{choice:?}"
            );
            // AV1 joined the auto family at M9, on the evidence rule and not on a
            // date: `native_evidence(Vulkan, CODEC_AV1)` is verified (250/250
            // bit-identical to libavcodec on an RTX 5070 Ti, M7). Before M9 this pair
            // asserted `choice == "native-vulkan"`; the flip is what changed, and it
            // changed HERE.
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, H264_OP | H265_OP | AV1_OP),
                "{choice:?}"
            );
            // …and the pin is still not a licence to skip the device leg: an AV1
            // session on a family that does not advertise the AV1 op would create a
            // video session for an operation the family cannot run.
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
            // A decode family advertising NO codec op, or only a foreign one,
            // refuses even with the extension stack present — the caps BIT is the
            // codec gate, not `video_decode`.
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
        // Never for an explicit OTHER-backend pin, capable device or not.
        // NOTE: the pre-M10 `vulkan`/`vaapi`/`d3d11va` spellings never reach this gate —
        // `migrate_decoder_pref` rewrites them first — so what is asserted here is the
        // gate's own rule: a pin naming ANOTHER backend is not a licence to run this one.
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
        // The decoder the gate implies — the construction sites `expect()` this
        // exact agreement, so a codec admitted with no decoder behind it would be a
        // panic rather than a demotion.
        assert_eq!(
            native_codec(CODEC_H264).map(|(c, _)| c),
            Some(NativeCodec::H264)
        );
        assert_eq!(
            native_codec(CODEC_HEVC).map(|(c, _)| c),
            Some(NativeCodec::H265)
        );
        // AV1 has a decoder AND the caps bit here — being in this map is what the
        // pin construction path reads. Whether `auto` may pick it is the gate's
        // decision above, and deliberately not this one's.
        assert_eq!(
            native_codec(CODEC_AV1),
            Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1))
        );
        assert!(native_codec(CODEC_PYROWAVE).is_none());
        assert!(native_codec(0).is_none());
    }

    /// The evidence table, asserted as the FACT it is — which rung/codec pairs have
    /// actually decoded on hardware and which have not.
    ///
    /// This test is the reason the table can be trusted a milestone from now. M9 turned
    /// native rungs on by default and M10 deleted every libavcodec rung beneath them; the
    /// argument for doing that honestly rests entirely on the claim "these five pairs are
    /// proven and these six are not", and on the session log saying so. A
    /// table nobody checks drifts into a table that says everything is fine — which is
    /// the exact failure this whole program exists to end, one layer up.
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
        ] {
            assert!(
                native_evidence(rung, codec).verified,
                "{what} has hardware parity recorded"
            );
        }
        for (rung, codec, why) in [
            (
                NativeRung::D3d11va,
                CODEC_AV1,
                "the DXVA AV1 leg never ran (M7)",
            ),
            (
                NativeRung::Vaapi,
                CODEC_H264,
                "no VAAPI device was reachable",
            ),
            (
                NativeRung::Vaapi,
                CODEC_HEVC,
                "no VAAPI device was reachable",
            ),
            (
                NativeRung::Vaapi,
                CODEC_AV1,
                "no VAAPI device was reachable",
            ),
            (
                NativeRung::Software,
                CODEC_H264,
                "openh264 never ran on glass",
            ),
            (NativeRung::Software, CODEC_AV1, "rav1d never ran on glass"),
        ] {
            assert!(
                !native_evidence(rung, codec).verified,
                "{why} — claiming otherwise is the dishonesty this program must not ship"
            );
        }
        // A codec leg nobody wrote an arm for reads as UNVERIFIED, never as its
        // neighbour's evidence: the next codec this program grows must land in the
        // session log's WARNING branch by default, not by somebody remembering to add a
        // row.
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

    /// M10's admission rule, stated as the pair of facts it is: an unproven rung runs
    /// wherever the only thing below it is the CPU, and wherever it runs it is NAMED —
    /// because naming it is the only protection left there.
    ///
    /// The four pairs below are the unproven ones. `log_rung` turns exactly these into a
    /// `warn` line carrying their note, and that line is what a field report about M10 gets
    /// read against. Which of them `auto` may pick FIRST is
    /// [`native_rung_admitted`]'s decision, asserted in the test after this one.
    #[test]
    fn every_rung_runs_and_the_unproven_ones_are_named() {
        let unproven = [
            (NativeRung::Vaapi, CODEC_H264),
            (NativeRung::Vaapi, CODEC_HEVC),
            (NativeRung::Vaapi, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_AV1),
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
                 must say plainly that nothing has run it, got {:?}",
                rung.name(),
                e.note
            );
        }
        // ...and the pairs that ARE proven stay proven. Nothing about M10 changes what
        // hardware has run; deleting the filter must not quietly relabel the evidence.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
        ] {
            assert!(native_evidence(rung, codec).verified, "{}", rung.name());
        }
    }

    /// The evidence FILTER: which rung `auto` may pick first, given what is under it.
    ///
    /// This is the rule that keeps M10 from shipping a default no hardware has ever run.
    /// The case it exists for is a Linux Intel (or unknown-vendor) desktop: the vendor
    /// order puts native VAAPI first, pf-vaadec has decoded nothing anywhere, and directly
    /// below it sits native Vulkan Video with three drivers, a 92-minute soak and 250/250
    /// AV1 behind it. A rung that produces WRONG PIXELS leaves only through the error-streak
    /// demotion, and the field has already shown that streak failing to trip (the B580's
    /// strobing between clean anchors and corrupt inter frames) — so "it will demote if it
    /// misbehaves" is not a guarantee, and the choice has to be made BEFORE the session
    /// runs. Hence: yield to proven code when there is proven code to yield to, and only
    /// then.
    #[test]
    fn an_unproven_rung_yields_to_a_proven_one_and_to_nothing_else() {
        // The Linux Intel/unknown arm: VAAPI first, native Vulkan Video under it. Every
        // codec pf-vaadec speaks is unproven on it, and every one of them is proven on the
        // rung below — so `auto` takes none of them.
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(
                !native_rung_admitted(NativeRung::Vaapi, codec, Some(NativeRung::Vulkan)),
                "codec {codec:#x}: a never-run VAAPI rung must not go first when the \
                 device can run the proven Vulkan rung for it"
            );
            // …and the same rung IS admitted when there is nothing proven below it: on
            // NVIDIA/AMD it is reached after Vulkan, and on a box whose Vulkan device
            // cannot run this codec at all the fall would be to the CPU. Taking hardware
            // decode away to protect a session from an unproven decoder is the worse answer.
            assert!(
                native_rung_admitted(NativeRung::Vaapi, codec, None),
                "codec {codec:#x}: with only the CPU below, the unproven rung runs"
            );
            // It yields to PROVEN code, not to any code: the CPU rung has never run on
            // glass either, so it is not something to fall onto in preference.
            assert!(native_rung_admitted(
                NativeRung::Vaapi,
                codec,
                Some(NativeRung::Software)
            ));
        }
        // A rung with hardware behind it is admitted whatever is below it — that is what
        // the evidence was collected for, and the filter must never demote a proven rung.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
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
        // Windows, Intel/unknown auto: the DXVA AV1 leg has never run, and the ladder
        // passes `None` there on purpose — that vendor family is the one with a measured
        // wrong-pixel report against Vulkan decode, so what is really below it is the CPU.
        // This asserts the ARGUMENT the call site passes, which is where the judgement
        // lives; `Some(Vulkan)` would bar it, and that is deliberately not what it passes.
        assert!(native_rung_admitted(NativeRung::D3d11va, CODEC_AV1, None));
        assert!(!native_rung_admitted(
            NativeRung::D3d11va,
            CODEC_AV1,
            Some(NativeRung::Vulkan)
        ));
        // The CPU rung is last everywhere, so nothing is ever below it and it always runs
        // — including for a codec it has no decoder for, which is `last_rung_verdict`'s
        // problem and not the filter's.
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(native_rung_admitted(NativeRung::Software, codec, None));
        }
    }

    /// The device half of the filter: "native Vulkan Video is below me" is a claim about
    /// THIS GPU, not about the ladder's shape.
    ///
    /// Without it the Linux Intel arm would bar VAAPI on a box whose Vulkan device cannot
    /// decode the session's codec — no decode family, or a family without that codec's
    /// operation — and hand the session to the CPU to protect it from a rung it needed.
    #[test]
    fn the_rung_below_must_be_one_this_device_can_actually_run() {
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        // The ordinary Mesa/Intel case: a decode family that advertises this codec.
        assert!(native_vulkan_usable(CODEC_H264, true, H264_OP));
        // No Vulkan Video at all, and a family that runs some OTHER codec: neither is a
        // rung to fall onto.
        assert!(!native_vulkan_usable(CODEC_H264, false, H264_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, AV1_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, 0));
        // A codec no native rung speaks (PyroWave rides its own path) is not a Vulkan rung.
        assert!(!native_vulkan_usable(CODEC_PYROWAVE, true, u32::MAX));
        // Composed, this is the whole Linux Intel decision, both ways round: an H.264
        // session on an H.264-capable device takes Vulkan; an AV1 session on that same
        // device has no proven rung below VAAPI, so VAAPI runs (and warns).
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

    /// What this client advertises it can decode is a statement about OUR rungs, and it
    /// did not move when the FFmpeg rungs were deleted.
    ///
    /// That invariance is the point: the wire's codec negotiation is a promise, M10
    /// deleted the FFmpeg rungs, and a client whose Hello changed on that deletion would
    /// have renegotiated every session in the field for a refactor. It used to be a
    /// libavcodec registry walk, which would have answered differently — and, worse,
    /// would have answered about decoders the ladder never reaches.
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
        // The CPU rung's codecs are a subset — the ladder must never advertise a codec
        // whose LAST rung it does not have... except HEVC, which is the one deliberate
        // exception this module documents at length.
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
