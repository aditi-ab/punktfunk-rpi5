//! Linux xdg-ScreenCast/PipeWire and Windows IDD direct-push capturers, plus
//! synthetic test sources and the [`Capturer`] trait.
//!
//! Speaks [`pf_frame`] and the display leaves. Encode-backend facts arrive
//! pre-resolved in [`ZeroCopyPolicy`]; Windows sealed-channel delivery arrives
//! as a [`FrameChannelSender`] closure. Never `pf-encode` or the host
//! orchestrator.
//!
//! Evidence: `design/idd-push-security.md`, `packaging/gamescope`.

use anyhow::Result;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// A FATAL frame-transport fault: the delivery ring's current generation is dead, and retrying
/// `try_latest` cannot help — the caller must rebuild the capture attachment or fail the session.
/// Carried inside the `anyhow::Error` a capture call returns (downcast to route on it), so a
/// fatal ring result can never again collapse into an ordinary `Ok(None)` repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingFault {
    /// The producer died (or a producer thread crashed) holding a slot's keyed mutex
    /// (`WAIT_ABANDONED`): the surface's consistency is unknown, the generation is poisoned.
    Abandoned,
    /// A fatal synchronization HRESULT (`hr`); `removed` is `GetDeviceRemovedReason`'s code at
    /// the time (0 = the device still reports healthy).
    DeviceLost { hr: i32, removed: i32 },
    /// A slot `ReleaseSync` failed (`hr`; `removed` as above) — the slot may be wedged for the
    /// producer, so the generation cannot be trusted either.
    ReleaseFailed { hr: i32, removed: i32 },
    /// A known-ACTIVE display (input/cursor moving, or the driver still offering frames)
    /// delivered no new source frame through the stale floor and the staged recovery ladder
    /// (immunity plan WP13) — its terminal verdict; `secs` is the source gap at that point.
    SourceStalled { secs: u32 },
}

impl std::fmt::Display for RingFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Abandoned => write!(f, "slot keyed mutex abandoned (producer died holding it)"),
            Self::DeviceLost { hr, removed } => write!(
                f,
                "fatal slot synchronization result {hr:#x} (device-removed reason {removed:#x})"
            ),
            Self::ReleaseFailed { hr, removed } => write!(
                f,
                "slot release failed {hr:#x} (device-removed reason {removed:#x})"
            ),
            Self::SourceStalled { secs } => write!(
                f,
                "no source frame for {secs}s on a known-active display, through a rebuild"
            ),
        }
    }
}

impl std::error::Error for RingFault {}
// The Linux capturer reaches `DmabufFrame` through `super::`; `CursorOverlay` it names directly as
// `pf_frame::CursorOverlay`, so only `DmabufFrame` needs to sit in this crate root's scope.
#[cfg(target_os = "linux")]
use pf_frame::DmabufFrame;

/// Produces frames without blocking the compositor. The Linux portal publishes
/// into a one-deep overwriting slot (drop-oldest): a stalled consumer still
/// sees the freshest frame.
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// [`next_frame`](Self::next_frame) with a caller-chosen first-frame budget.
    /// A PipeWire stream can sit in `Streaming` with no buffer; retry shortens
    /// the first wait instead of blocking the default. Backends without an
    /// internal wait budget ignore it (the default delegates).
    fn next_frame_within(&mut self, _budget: std::time::Duration) -> Result<CapturedFrame> {
        self.next_frame()
    }

    /// [`next_frame_within`](Self::next_frame_within) whose expiry is retry, not
    /// a capture verdict. Must not latch process-wide HDR/dmabuf-only downgrades:
    /// a cold start can outlive the short window and still accept every offer.
    /// Backends that latch nothing from a timeout just delegate.
    fn next_frame_within_provisional(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<CapturedFrame> {
        self.next_frame_within(budget)
    }

    /// Non-blocking: the freshest frame since the last call, or `None` so the
    /// caller reuses its last frame and holds a steady output rate. Default
    /// produces a frame each call; the portal drains without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }

    /// Whether [`wait_arrival`](Self::wait_arrival) is usable. `false` (default)
    /// keeps the encode loop on its fixed-cadence tick.
    fn supports_arrival_wait(&self) -> bool {
        false
    }

    /// Block until a frame is ready for [`try_latest`](Self::try_latest) or
    /// `deadline` passes. Must not consume the frame. Only called when
    /// [`supports_arrival_wait`](Self::supports_arrival_wait) is `true`;
    /// errors surface at the following `try_latest`.
    fn wait_arrival(&mut self, _deadline: std::time::Instant) {}

    /// Gate expensive per-frame work so the capturer can stay alive between
    /// streams. The portal skips the de-pad copy while inactive and flushes
    /// its frame mailbox on `false`. `&mut self`: the mailbox flush cannot share.
    fn set_active(&mut self, _active: bool) {}

    /// Whether this capturer can still produce frames. A pool must consult this
    /// before reuse: zero-copy poison, a dead PipeWire thread, or a source that
    /// never returns to `Streaming` makes every later call fail. Default `true`.
    fn is_alive(&self) -> bool {
        true
    }

    /// Live cursor out-of-band from frames (Windows IddCx hardware-cursor
    /// channel). Preferred over `CapturedFrame::cursor`: pointer-only moves
    /// produce no frame, so a frame-attached overlay goes stale. Default `None`.
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        None
    }

    /// Cursor-render flip: `true` keeps the pointer out of the video (client
    /// draws it); `false` puts it back in. A declared IddCx hardware cursor is
    /// irrevocable — DWM cannot take the job back. Default no-op.
    fn set_cursor_forward(&mut self, _on: bool) {}

    /// Attach a gamescope cursor source. gamescope paints no `SPA_META_Cursor`,
    /// so [`cursor`](Self::cursor) stays empty unless the portal reads nested
    /// Xwaylands (one per `--xwayland-count`) over X11. Called once, after build.
    #[cfg(target_os = "linux")]
    fn attach_gamescope_cursor(&mut self, _targets: GamescopeCursorTargets) {}

    /// Static HDR mastering metadata (SMPTE ST.2086 + CLL) when the capturer can
    /// read it (Windows `IDXGIOutput6::GetDesc1`), or a generic HDR10 block once
    /// an HDR stream is negotiated (Linux exposes no real mastering volume).
    /// Forwarded to the encoder (SEI) and the client (`0xCE`). May change if regraded.
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        None
    }

    /// How many frames the encode loop may keep in flight before it blocks.
    /// `1` (default) is capture → submit → poll-blocks. `>1` overlaps convert
    /// of N+1 with encode of N when each frame has a fresh output texture.
    fn pipeline_depth(&self) -> usize {
        1
    }

    // `capture_target_id` and `resize_output` are one operation split in half:
    // `Some` from the id promises resize works; resize without the id cannot
    // check the reconfigured display is still this capturer's. Both defaults decline.

    /// OS display-target id this capturer is bound to (Windows IDD-push). Resize
    /// uses it to verify the reconfigured display is still this one. In-place
    /// resize keeps the target; a re-arrival fallback mints a new one. `None`
    /// = no such identity.
    fn capture_target_id(&self) -> Option<u32> {
        None
    }

    /// Host-initiated output resize after the session handler has committed the
    /// new mode. Resize the capture surface now: no descriptor-poll debounce,
    /// no teardown. `true` handled; `false` rebuilds.
    fn resize_output(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Recreate the delivery ring at the current mode and re-run the driver
    /// attach handshake. Exclusive-topology eviction rebuilds the swap-chain
    /// while this capturer waits on the old ring; the descriptor is unchanged,
    /// so the two-strike debounce never trips. `true` handled; `false` unrecoverable.
    fn recreate_ring_in_place(&mut self) -> bool {
        false
    }

    /// A staged-recovery episode closed on new source frames since the last
    /// call: the measured local outage, from the last source frame before the
    /// stall to the frame that proved recovery. The stream loop forces an IDR
    /// and announces the gap. `None` = nothing recovered.
    fn take_recovered_outage(&mut self) -> Option<std::time::Duration> {
        None
    }
}

/// Deterministic moving BGRx test pattern: a sweeping bar plus an animated
/// gradient so every pixel changes.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl SyntheticCapturer {
    const BPP: usize = 4; // BGRx

    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(width > 0 && height > 0 && fps > 0);
        let buf = vec![0u8; width as usize * height as usize * Self::BPP];
        SyntheticCapturer {
            width,
            height,
            fps,
            frame_idx: 0,
            buf,
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bpp = Self::BPP;
        let t = self.frame_idx;
        // Vertical bar sweeps left→right once every ~2 s (`fps * 2`).
        let bar_x = ((t * w as u64) / (self.fps as u64 * 2)) % w as u64;
        let phase = (t % 256) as usize;
        for y in 0..h {
            let row = y * w * bpp;
            for x in 0..w {
                let i = row + x * bpp;
                let on_bar = (x as u64).abs_diff(bar_x) < 8;
                // BGRx: [B, G, R, x]
                self.buf[i] = if on_bar {
                    255
                } else {
                    ((x + phase) & 0xff) as u8
                };
                self.buf[i + 1] = if on_bar {
                    255
                } else {
                    ((y + phase) & 0xff) as u8
                };
                self.buf[i + 2] = if on_bar { 255 } else { ((x + y) & 0xff) as u8 };
                self.buf[i + 3] = 0;
            }
        }
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps as u64;
        self.frame_idx += 1;
        Ok(CapturedFrame {
            provenance: Default::default(),
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// Cheap moving BGRx test pattern: whole-buffer `fill`s, real-time at 5K.
pub struct FastSyntheticCapturer {
    width: u32,
    height: u32,
    frame_idx: u64,
    buf: Vec<u8>,
    /// `PUNKTFUNK_SYNTH_NOISE`: high-entropy noise NVENC cannot compress, so
    /// the encoder hits its CBR target. Default flat/band compresses to ~nothing.
    noise: bool,
    rng: u64,
}

impl FastSyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0);
        FastSyntheticCapturer {
            width,
            height,
            frame_idx: 0,
            buf: vec![0u8; width as usize * height as usize * 4],
            noise: std::env::var_os("PUNKTFUNK_SYNTH_NOISE").is_some(),
            rng: 0x9e3779b97f4a7c15,
        }
    }
}

impl Capturer for FastSyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if self.noise {
            // Reseed from the frame index so consecutive frames share no
            // structure — large P-frames, not just the keyframe.
            let mut s = self
                .rng
                .wrapping_add(self.frame_idx.wrapping_mul(0x2545F491_4F6CDD1D))
                | 1;
            for c in self.buf.chunks_exact_mut(8) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                c.copy_from_slice(&s.to_le_bytes());
            }
            self.rng = s;
        } else {
            let (w, h) = (self.width as usize, self.height as usize);
            let row = w * 4;
            let shade = (self.frame_idx % 256) as u8;
            self.buf.fill(shade);
            let band_h = (h / 20).max(1);
            let band_y = (self.frame_idx as usize * 6) % h;
            for y in band_y..(band_y + band_h).min(h) {
                self.buf[y * row..(y + 1) * row].fill(0xff);
            }
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            provenance: Default::default(),
            width: self.width,
            height: self.height,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// Encode-backend facts the Linux zero-copy negotiation needs, resolved once
/// by the host facade and passed in so capture never calls back into encode.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct ZeroCopyPolicy {
    /// VAAPI (AMD/Intel): hand raw dmabufs through instead of the EGL→CUDA
    /// import (`encode::linux_zero_copy_is_vaapi`).
    pub backend_is_vaapi: bool,
    /// GPU-resident frames (everything but software). Phrases the CPU-fallback
    /// warning (`encode::resolved_backend_is_gpu`).
    pub backend_is_gpu: bool,
    /// This session encodes PyroWave: the wavelet encoder's Vulkan device
    /// imports raw dmabufs on any vendor, so take raw-dmabuf passthrough.
    /// Per-session, unlike `backend_is_vaapi`.
    pub pyrowave_session: bool,
    /// Encoder can ingest producer-native NV12 (Linux raw Vulkan Video on
    /// H265/AV1 — `pf_encode::linux_native_nv12_ok`). libav VAAPI (H264)
    /// misreads the two-plane buffer; H264/GameStream/PyroWave must never see NV12.
    pub native_nv12_session: bool,
    /// PyroWave Vulkan-importable dmabuf modifiers for packed-RGB. Advertised
    /// so Mutter+NVIDIA (tiled-only alloc) still negotiates zero-copy. Empty otherwise.
    pub pyrowave_modifiers: Vec<u64>,
    /// Encoder can ingest packed 10-bit PQ CUDA (`pf_encode::linux_hdr_cuda_ok`,
    /// direct-SDK NVENC only). libav's HDR route swscales into P010, so packed
    /// 2:10:10:10 CUDA lands as garbage unless this holds.
    pub hdr_cuda_ok: bool,
}

/// Discovers gamescope's nested Xwayland cursor targets — `(DISPLAY, XAUTHORITY)`,
/// one per `--xwayland-count` — for [`Capturer::attach_gamescope_cursor`].
///
/// A closure, re-run on a slow cadence: gamescope creates a second Xwayland for
/// the game but advertises only the first in any child's environ, so a one-shot
/// snapshot taken before launch never sees the game display.
///
/// Built by the host facade (`pf_vdisplay::gamescope_xwayland_cursor_targets`)
/// so the capture→host edge stays one-way — same shape as [`FrameChannelSender`].
#[cfg(target_os = "linux")]
pub type GamescopeCursorTargets =
    std::sync::Arc<dyn Fn() -> Vec<(String, Option<String>)> + Send + Sync>;

#[cfg(target_os = "linux")]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    true
}

/// Whether a native-plane capturer (compositor virtual output) can deliver HDR
/// (10-bit PQ/BT.2020) on this platform alone — the platform half of the
/// handshake's 10-bit gate, without knowing which compositor will be driven.
///
/// Linux is `false`: Mutter `RecordVirtual` and KWin/wlroots advertise 8-bit
/// BGRx/BGRA. gamescope can be 10-bit with the carried `pipewire-hdr` patch;
/// the host resolves that in `capture::capturer_supports_hdr_for`. The GNOME
/// portal monitor mirror (`open_portal_monitor` + `want_hdr`) is a separate gate.
#[cfg(target_os = "linux")]
pub fn capturer_supports_hdr() -> bool {
    false
}
/// Windows: IDD-push enables advanced colour and delivers P010/Rgb10a2.
#[cfg(target_os = "windows")]
pub fn capturer_supports_hdr() -> bool {
    true
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_hdr() -> bool {
    false
}

/// Which HDR capture source a `want_hdr` negotiation failure belongs to.
/// The latch is per source so a portal-monitor failure cannot disable the
/// virtual-output path, and vice versa, until host restart.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrSource {
    /// GNOME 50+ portal monitor mirror (`open_portal_monitor` with `want_hdr`).
    PortalMonitor,
    /// Compositor virtual output (`open_virtual_output` with `want_hdr`) —
    /// gamescope's PipeWire node with the carried `pipewire-hdr` patch.
    VirtualOutput,
}

/// Per-source latch: `want_hdr` failed to negotiate the 10-bit PQ offer.
/// Later sessions fall back to SDR instead of re-running the 10 s timeout.
/// Sticky until host restart.
#[cfg(target_os = "linux")]
static HDR_CAPTURE_FAILED: [std::sync::atomic::AtomicBool; 2] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

#[cfg(target_os = "linux")]
impl HdrSource {
    fn slot(self) -> usize {
        match self {
            HdrSource::PortalMonitor => 0,
            HdrSource::VirtualOutput => 1,
        }
    }
}

#[cfg(target_os = "linux")]
pub fn hdr_capture_failed(source: HdrSource) -> bool {
    HDR_CAPTURE_FAILED[source.slot()].load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
pub(crate) fn note_hdr_capture_failed(source: HdrSource) {
    if !HDR_CAPTURE_FAILED[source.slot()].swap(true, std::sync::atomic::Ordering::Relaxed) {
        match source {
            HdrSource::PortalMonitor => tracing::warn!(
                "HDR capture negotiation failed on the monitor mirror — this host will offer SDR \
                 for that source for the rest of the process lifetime (restart the host after \
                 fixing the monitor's HDR mode to retry)"
            ),
            HdrSource::VirtualOutput => tracing::warn!(
                "HDR capture negotiation failed on the virtual output — this host will offer SDR \
                 for that source for the rest of the process lifetime (is the spawned gamescope \
                 the punktfunk build? see packaging/gamescope)"
            ),
        }
    }
}
#[cfg(target_os = "windows")]
pub fn capturer_supports_444(encoder_ingests_rgb_444: bool) -> bool {
    // IDD-push is full-chroma RGB (BGRA SDR, Rgb10a2 HDR). Only a backend that
    // CSCs RGB to 4:4:4 itself can use that (direct-NVENC). Both depths are
    // full-chroma, so Welcome chroma is real regardless of HDR.
    encoder_ingests_rgb_444
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    false
}

/// Host-registered HID compose-kick: `(target_rect, desktop_bounds) -> accepted`,
/// both `(x, y, w, h)` in desktop coordinates (CCD). Device-level input wakes a
/// powered-off display regardless of session; `SendInput` does not. `false` →
/// `SendInput` fallback. This crate never reaches the host inject module.
#[cfg(target_os = "windows")]
pub static HID_COMPOSE_KICK: std::sync::OnceLock<HidKickFn> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
pub type HidKickFn = fn((i32, i32, i32, i32), (i32, i32, i32, i32)) -> bool;

/// Delivers a monitor's sealed frame channel to the pf-vdisplay driver
/// (`IOCTL_SET_FRAME_CHANNEL`). Host-built so this crate never reaches the
/// orchestrator. Called once per ring generation, never per-frame. On IOCTL
/// success the driver owns the handles duplicated into WUDFHost.
#[cfg(target_os = "windows")]
pub type FrameChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetFrameChannelRequestV2) -> Result<()> + Send + Sync,
>;

/// v5 hardware-cursor channel (`IOCTL_SET_CURSOR_CHANNEL`) — same facade
/// contract as [`FrameChannelSender`]. `Some` opts in: the capturer creates
/// the cursor section only when the host hands a sender; a plain session
/// keeps DWM's pointer.
#[cfg(target_os = "windows")]
pub type CursorChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetCursorChannelRequest) -> Result<()> + Send + Sync,
>;

/// Mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`). `true` declares
/// the IddCx hardware cursor; `false` stands it down (host also forces the
/// same-mode re-commit that actualises the OS software cursor). UAC/Winlogon
/// render only through software cursor.
#[cfg(target_os = "windows")]
pub type CursorForwardSender = std::sync::Arc<dyn Fn(bool) -> Result<()> + Send + Sync>;

// One-time PipeWire library init, shared by video (portal) and audio capture.
#[cfg(target_os = "linux")]
pub mod pwinit;

// Which clock the wire's `pts_ns` comes from. Linux-only consumer; arithmetic
// is platform-independent so tests run everywhere.
#[cfg(any(target_os = "linux", test))]
mod pts_provenance;

#[cfg(target_os = "windows")]
#[path = "windows/dxgi.rs"]
pub mod dxgi;
#[cfg(target_os = "windows")]
#[path = "windows/idd_push.rs"]
mod idd_push;
// WUDFHost-identity check reused by the host gamepad-channel bootstrap
// (`inject::windows::gamepad_raii`); re-export so that reach stays a leaf.
#[cfg(target_os = "windows")]
pub use idd_push::verify_is_wudfhost;
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
// GNOME BT.2100 colour-mode probe — host gate for offering HDR on the portal
// monitor path (`open_portal_monitor` `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
#[cfg(target_os = "windows")]
#[path = "windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;

/// Linux xdg-ScreenCast portal capturer for a client-sized monitor. `anchored`
/// inherits a RemoteDesktop grant headlessly. Pass `want_hdr` only when the
/// mirrored monitor is in HDR mode, or the 10 s negotiation latches SDR.
/// Pass `want_metadata_cursor` only when encode composites `CapturedFrame::cursor`;
/// otherwise the portal embeds the pointer so it is never silently lost.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    anchored: bool,
    want_hdr: bool,
    want_metadata_cursor: bool,
    policy: ZeroCopyPolicy,
) -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::open(
        anchored,
        want_hdr && !hdr_capture_failed(HdrSource::PortalMonitor),
        want_metadata_cursor,
        policy,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Linux portal capturer bound to an already-created virtual output's PipeWire
/// node. The capturer takes `keepalive`; dropping it releases the output. Pass
/// `want_hdr` only when the output was brought up HDR — a PQ session cannot
/// fall back to SDR. `cursor_id0_hides`: KWin rewrites `SPA_META_Cursor` on
/// every buffer and treats `id == 0` as "pointer hidden".
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub fn open_virtual_output(
    remote_fd: Option<std::os::fd::OwnedFd>,
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    keepalive: Box<dyn Send>,
    allow_zerocopy: bool,
    want_444: bool,
    want_hdr: bool,
    policy: ZeroCopyPolicy,
    expect_exact_dims: bool,
    cursor_id0_hides: bool,
) -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::from_virtual_output(
        remote_fd,
        node_id,
        preferred_mode,
        keepalive,
        allow_zerocopy,
        want_444,
        want_hdr && !hdr_capture_failed(HdrSource::VirtualOutput),
        policy,
        expect_exact_dims,
        cursor_id0_hides,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Windows IDD direct-push capturer on a pf-vdisplay target. `sender` delivers
/// the sealed frame channel. On failure `keepalive` is handed back so the
/// caller can retire the display.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn open_idd_push(
    target: pf_frame::dxgi::WinCaptureTarget,
    preferred: Option<(u32, u32, u32)>,
    want_hdr: bool,
    ten_bit_sdr: bool,
    want_444: bool,
    pyrowave: bool,
    keepalive: Box<dyn Send>,
    sender: FrameChannelSender,
    cursor_sender: Option<CursorChannelSender>,
    cursor_forward: Option<CursorForwardSender>,
) -> std::result::Result<Box<dyn Capturer>, (anyhow::Error, Box<dyn Send>)> {
    idd_push::IddPushCapturer::open(
        target,
        preferred,
        want_hdr,
        ten_bit_sdr,
        want_444,
        pyrowave,
        keepalive,
        sender,
        cursor_sender,
        cursor_forward,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}
