//! Frame capture (plan §7 / §W6): the capturers themselves — the Linux xdg-ScreenCast/PipeWire
//! portal capturer and the Windows IDD direct-push capturer — plus the synthetic test sources and
//! the `Capturer` trait, extracted into a subsystem crate. Speaks the shared frame vocabulary
//! (`pf-frame`) + the zero-copy plumbing (`pf-zerocopy`) and the display leaves (`pf-win-display`),
//! and NEVER `pf-encode` — the capture→encode edge is one-way (the encode-backend facts arrive
//! pre-resolved in a [`ZeroCopyPolicy`], and the Windows sealed-channel delivery arrives as a
//! [`FrameChannelSender`] closure, so this crate reaches neither the encoder nor the host
//! orchestrator).

// Scaffold: trait defaults + synthetic sources are defined ahead of the backends that use them.
#![allow(dead_code)]
// Every unsafe block in this crate carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
// The Linux capturer reaches `DmabufFrame` through `super::`; `CursorOverlay` it names directly as
// `pf_frame::CursorOverlay`, so only `DmabufFrame` needs to sit in this crate root's scope.
#[cfg(target_os = "linux")]
use pf_frame::DmabufFrame;

/// Produces frames from a captured output. Lives on its own thread, feeding the encoder
/// over a bounded drop-oldest channel (never block the compositor).
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Non-blocking: the freshest frame available since the last call, or `None` if none has
    /// arrived (the caller reuses its last frame to hold a steady output rate). The default
    /// just produces a frame each call — fine for instant synthetic sources; the portal
    /// overrides it to drain its channel without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }

    /// Whether this backend can block until a frame ARRIVES ([`wait_arrival`]
    /// (Self::wait_arrival)) — the frame-driven encode trigger (latency plan T1.1). `false`
    /// (the default) keeps the encode loop on its legacy fixed-cadence tick for this backend.
    fn supports_arrival_wait(&self) -> bool {
        false
    }

    /// Block until a FRESH frame is available via [`try_latest`](Self::try_latest) or
    /// `deadline` passes — the encode loop's frame-driven wait (latency plan T1.1): waking on
    /// the compositor's publish instead of sampling at a free-running tick deletes the
    /// sample-and-hold (~half a frame interval on average). Must NOT consume the frame (the
    /// loop's `try_latest` call does); backends buffer internally where the arrival channel
    /// can't be peeked. Only called when [`supports_arrival_wait`](Self::supports_arrival_wait)
    /// is `true`; errors surface at the following `try_latest`.
    fn wait_arrival(&mut self, _deadline: std::time::Instant) {}

    /// Gate expensive per-frame work so the capturer can be kept alive (reused) between
    /// streams without burning CPU. The portal capturer skips the de-pad copy while inactive;
    /// the default is a no-op (synthetic sources are produced on demand). Set `true` for the
    /// duration of a stream, `false` when it ends.
    fn set_active(&self, _active: bool) {}

    /// The source's static HDR mastering metadata (SMPTE ST.2086 + content light level), when the
    /// capturer can read it from the output (Windows `IDXGIOutput6::GetDesc1`). `None` = unknown /
    /// SDR / a backend that doesn't expose it (the default — Linux capture has no HDR path yet).
    /// The stream loop forwards this to the encoder (in-band SEI) and the client (`0xCE` datagram),
    /// so the two stay a single source of truth. May change mid-session if the source is regraded.
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        None
    }

    /// How many frames the encode loop may keep in flight (submitted but not yet polled) before it
    /// blocks. `1` (the default) is the synchronous loop: capture → submit → poll-blocks, so the
    /// per-frame wall time is `capture+convert + encode`. A capturer that hands a fresh output texture
    /// per frame (so the encode of N reads a different texture than the convert of N+1 writes) can return
    /// `>1` to PIPELINE: the loop submits N+1 before polling N, overlapping the convert/copy on the 3D
    /// engine with the NVENC-ASIC encode of the prior frame, dropping per-frame wall toward `max(...)`.
    fn pipeline_depth(&self) -> usize {
        1
    }

    /// The OS display-target id this capturer is bound to (Windows IDD-push), so the resize path
    /// can verify the display it just reconfigured is STILL the one this capturer serves (an
    /// in-place resize keeps the target; a re-arrival fallback mints a new one, which needs a
    /// fresh capturer). `None` = the backend has no such identity (every non-IDD backend).
    fn capture_target_id(&self) -> Option<u32> {
        None
    }

    /// HOST-INITIATED output resize (latency plan P2.3): the session's resize handler has ALREADY
    /// committed the display's new mode (the manager's in-place mode set), so a capable capturer
    /// re-sizes its capture surface NOW — no descriptor-poll debounce (that machinery stays, for
    /// EXTERNAL changes only) and no teardown: the capture pipeline and its send thread survive;
    /// only the encoder is swapped by the caller once the first new-size frame arrives. Returns
    /// `true` when handled; `false` (the default) routes the caller to the full-rebuild path.
    fn resize_output(&mut self, _width: u32, _height: u32) -> bool {
        false
    }
}

/// A deterministic moving test pattern (BGRx). Lets the spike exercise the encode → file →
/// `punktfunk_core` path with no live capture session, and produces obviously non-static
/// content (a sweeping bar + animated gradient) so the encoded output is verifiable.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl SyntheticCapturer {
    const BPP: usize = 4; // emits BGRx

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
        // A vertical bar sweeps left→right once every ~2s; the background is a gradient
        // whose phase advances each frame, so every pixel changes frame-to-frame.
        let bar_x = ((t * w as u64) / (self.fps as u64 * 2)) % w as u64;
        let phase = (t % 256) as usize;
        for y in 0..h {
            let row = y * w * bpp;
            for x in 0..w {
                let i = row + x * bpp;
                let on_bar = (x as u64).abs_diff(bar_x) < 8;
                // BGRx byte order: [B, G, R, x]
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
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// A cheap moving test pattern (BGRx) for the streaming path: a pulsing field + a white band
/// sweeping down, generated with whole-buffer `fill`s so it stays real-time even at 5K.
pub struct FastSyntheticCapturer {
    width: u32,
    height: u32,
    frame_idx: u64,
    buf: Vec<u8>,
    /// PUNKTFUNK_SYNTH_NOISE: every frame is fresh high-entropy noise NVENC can't compress or
    /// predict, so the encoder hits its (CBR) bitrate target — a throughput test of the real
    /// encode→FEC→send→recv path. The default flat/band content compresses to ~nothing, so it
    /// can't generate real Mbps (the encoder is content-driven). xorshift over u64 chunks.
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
            // Fresh, every-frame-decorrelated noise: reseed from the frame index so consecutive
            // frames share no structure (forces large P-frames too, not just the keyframe).
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
            width: self.width,
            height: self.height,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// The encode-backend facts the Linux zero-copy negotiation needs, resolved **once** here (the host
/// facade, which may reach the host `encode`) and passed **into** the capturer — so the capturer never
/// calls back into `encode`, keeping the capture→encode dependency one-way (plan §2.4 / §W6). The
/// three facts were formerly re-derived inside the PipeWire thread via
/// `encode::{linux_zero_copy_is_vaapi, resolved_backend_is_gpu, pyrowave_capture_modifiers}`.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct ZeroCopyPolicy {
    /// The GPU encode backend resolves to VAAPI (AMD/Intel) — the capturer hands raw dmabufs
    /// straight through instead of the EGL→CUDA import (the host `encode::linux_zero_copy_is_vaapi`).
    pub backend_is_vaapi: bool,
    /// The resolved backend produces GPU-resident frames (everything but the software encoder) —
    /// used only to phrase the CPU-fallback warning (the host `encode::resolved_backend_is_gpu`).
    pub backend_is_gpu: bool,
    /// The PyroWave encoder's Vulkan-importable dmabuf modifiers for the capture's packed-RGB fourcc,
    /// resolved when the encoder pref is `pyrowave` (the passthrough advertises them so Mutter+NVIDIA,
    /// which allocates tiled-only, still negotiates zero-copy). Empty otherwise.
    pub pyrowave_modifiers: Vec<u64>,
}

#[cfg(target_os = "linux")]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    true
}
#[cfg(target_os = "windows")]
pub fn capturer_supports_444(encoder_ingests_rgb_444: bool) -> bool {
    // IDD-push delivers full-chroma BGRA for an SDR 4:4:4 session (skipping the NV12 VideoConverter),
    // but only a backend that ingests RGB and CSCs it to 4:4:4 itself can use it — today just
    // direct-NVENC (AMF can't 4:4:4 at all; the QSV/ffmpeg path has no RGB-input 4:4:4 wiring). An HDR
    // display can't be known here (the virtual display's mode settles after the Welcome); that
    // combination downgrades at capture time — the capturer emits P010 and the encoder's caps
    // cross-check reports the 4:2:0 truth (the in-band SPS keeps the client correct either way).
    encoder_ingests_rgb_444
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    false
}

/// Host-registered HID compose-kick hook: `(target_rect, desktop_bounds) -> accepted`, both
/// `(x, y, w, h)` in desktop coordinates (from CCD). The host facade registers it once at startup
/// when the resident virtual HID mouse exists (`inject::mouse_windows::hid_kick`); the IDD-push
/// capturer's compose kick then prefers it over `SendInput`, because device-level input is
/// delivered regardless of this process's session or the active desktop and wakes a powered-off
/// display — the lid-closed first-frame fix. Same one-way-edge philosophy as
/// [`FrameChannelSender`]: this crate never reaches back into the host's inject module. `false`
/// from the hook = mouse not available right now → the caller falls back to `SendInput`.
#[cfg(target_os = "windows")]
pub static HID_COMPOSE_KICK: std::sync::OnceLock<HidKickFn> = std::sync::OnceLock::new();

/// The [`HID_COMPOSE_KICK`] hook's shape: `(target_rect, desktop_bounds) -> accepted`, both
/// `(x, y, w, h)` in desktop coordinates.
#[cfg(target_os = "windows")]
pub type HidKickFn = fn((i32, i32, i32, i32), (i32, i32, i32, i32)) -> bool;

/// Delivers a monitor's sealed frame channel to the pf-vdisplay driver (`IOCTL_SET_FRAME_CHANNEL`) —
/// the ONE reach the IDD-push capturer would otherwise make into the host's `vdisplay` module. The
/// host facade builds this closure (capturing the pf-vdisplay control device handle + the
/// `send_frame_channel` IOCTL wrapper) and hands it in, so this crate delivers the channel without a
/// path back to the orchestrator. Called once per ring generation (at attach), never per-frame —
/// guardrail-compliant. The handle values in `req` were just duplicated into the driver's WUDFHost
/// by the capturer's [`windows::idd_push`] broker; on IOCTL success the DRIVER owns them.
#[cfg(target_os = "windows")]
pub type FrameChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetFrameChannelRequest) -> Result<()> + Send + Sync,
>;

// One-time PipeWire library init, shared by the video (portal) and audio capture threads.
#[cfg(target_os = "linux")]
pub mod pwinit;

// The Windows backend lives under `windows/`, the Linux one under `linux/`. Windows capture is IDD
// direct-push only (DXGI Desktop Duplication + the WGC relay were removed).
#[cfg(target_os = "windows")]
#[path = "windows/dxgi.rs"]
pub mod dxgi;
#[cfg(target_os = "windows")]
#[path = "windows/idd_push.rs"]
mod idd_push;
// The WUDFHost-identity check the IDD-push broker uses is reused by the host's gamepad-channel
// bootstrap (`inject::windows::gamepad_raii`); re-export it so that reach stays a leaf dependency.
#[cfg(target_os = "windows")]
pub use idd_push::verify_is_wudfhost;
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
#[cfg(target_os = "windows")]
#[path = "windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;

/// Open the Linux xdg-ScreenCast portal capturer for a client-sized monitor. `anchored` drives
/// ScreenCast off a RemoteDesktop session (KWin/GNOME) so it inherits that grant headlessly. The
/// [`ZeroCopyPolicy`] carries the pre-resolved encode-backend facts (the one-way edge).
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(anchored: bool, policy: ZeroCopyPolicy) -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::open(anchored, policy).map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Open the Linux portal capturer bound to an already-created virtual output's PipeWire node. The
/// caller (host facade) explodes its `VirtualOutput` into these primitives + owns nothing after —
/// the capturer takes `keepalive`, so dropping it releases the output. `allow_zerocopy` mirrors
/// `OutputFormat::gpu`; `want_444` selects the planar-YUV444 GPU convert.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub fn open_virtual_output(
    remote_fd: Option<std::os::fd::OwnedFd>,
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    keepalive: Box<dyn Send>,
    allow_zerocopy: bool,
    want_444: bool,
    policy: ZeroCopyPolicy,
) -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::from_virtual_output(
        remote_fd,
        node_id,
        preferred_mode,
        keepalive,
        allow_zerocopy,
        want_444,
        policy,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Open the Windows IDD direct-push capturer on a pf-vdisplay target. `sender` delivers the sealed
/// frame channel to the driver (the host facade builds it from the vdisplay control device). On
/// failure the `keepalive` is handed back so the caller can retire the display.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn open_idd_push(
    target: pf_frame::dxgi::WinCaptureTarget,
    preferred: Option<(u32, u32, u32)>,
    client_10bit: bool,
    want_444: bool,
    pyrowave: bool,
    keepalive: Box<dyn Send>,
    sender: FrameChannelSender,
) -> std::result::Result<Box<dyn Capturer>, (anyhow::Error, Box<dyn Send>)> {
    idd_push::IddPushCapturer::open(
        target,
        preferred,
        client_10bit,
        want_444,
        pyrowave,
        keepalive,
        sender,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}
