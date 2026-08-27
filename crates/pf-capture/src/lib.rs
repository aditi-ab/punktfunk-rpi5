//! Frame capture (plan §7 / §W6): the capturers themselves — the Linux xdg-ScreenCast/PipeWire
//! portal capturer and the Windows IDD direct-push capturer — plus the synthetic test sources and
//! the `Capturer` trait, extracted into a subsystem crate. Speaks the shared frame vocabulary
//! (`pf-frame`) + the zero-copy plumbing (`pf-zerocopy`) and the display leaves (`pf-win-display`),
//! and NEVER `pf-encode` — the capture→encode edge is one-way (the encode-backend facts arrive
//! pre-resolved in a [`ZeroCopyPolicy`], and the Windows sealed-channel delivery arrives as a
//! [`FrameChannelSender`] closure, so this crate reaches neither the encoder nor the host
//! orchestrator).

use anyhow::Result;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
// The Linux capturer reaches `DmabufFrame` through `super::`; `CursorOverlay` it names directly as
// `pf_frame::CursorOverlay`, so only `DmabufFrame` needs to sit in this crate root's scope.
#[cfg(target_os = "linux")]
use pf_frame::DmabufFrame;

/// Produces frames from a captured output. Lives on its own thread, handing frames over without
/// ever blocking the compositor — the Linux portal publishes into a one-deep OVERWRITING slot
/// (drop-oldest), so a stalled consumer costs the intermediate frames and is still handed the
/// freshest one.
pub trait Capturer: Send {
    // ---- Frames -----------------------------------------------------------------------------
    // `next_frame` blocks for one; `try_latest` is the steady-state non-blocking read;
    // `wait_arrival` + `supports_arrival_wait` are the frame-driven trigger that replaces a
    // free-running tick.

    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// [`next_frame`](Self::next_frame) with a caller-chosen first-frame budget instead of the
    /// backend's default. The pipeline retry loop shortens its FIRST attempt's wait: a PipeWire
    /// stream connected while gamescope re-inits its headless takeover can negotiate a format,
    /// reach `Streaming`, and still never receive a buffer — a fresh connect then delivers within
    /// ~0.5 s, so waiting out the full default budget on a doomed stream just delays the retry
    /// that fixes it. Backends without an internal wait budget ignore it (the default delegates).
    fn next_frame_within(&mut self, _budget: std::time::Duration) -> Result<CapturedFrame> {
        self.next_frame()
    }

    /// [`next_frame_within`](Self::next_frame_within), but the caller declares the budget
    /// PROVISIONAL: its expiry is the retry schedule firing (the deliberately truncated first
    /// attempt), not a verdict on anything this capture offered. The portal backend must NOT
    /// latch its sticky process-wide downgrades (HDR capture, either dmabuf-only offer) from a
    /// provisional expiry — a gamescope cold start routinely outlives the short window while it
    /// would have accepted every offer, and one latched race used to pin the whole host process
    /// to SDR/CPU capture. The full-length attempt that follows delivers the honest verdict.
    /// Backends that latch nothing from a timeout just delegate.
    fn next_frame_within_provisional(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<CapturedFrame> {
        self.next_frame_within(budget)
    }

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
    /// sample-and-hold (~half a frame interval on average). Must NOT consume the frame — the
    /// loop's `try_latest` call does that — so a backend implements this by waiting on a wakeup
    /// and then PEEKING its hand-off slot. Only called when
    /// [`supports_arrival_wait`](Self::supports_arrival_wait) is `true`; errors surface at the
    /// following `try_latest`.
    fn wait_arrival(&mut self, _deadline: std::time::Instant) {}

    // ---- Lifecycle --------------------------------------------------------------------------
    // Whether the capturer is being used right now, and whether it can still be used at all.

    /// Gate expensive per-frame work so the capturer can be kept alive (reused) between
    /// streams without burning CPU. The portal capturer skips the de-pad copy while inactive and
    /// flushes its frame mailbox on `false`; the default is a no-op (synthetic sources are produced
    /// on demand). Set `true` for the duration of a stream, `false` when it ends.
    ///
    /// `&mut self`: it mutates capturer state, and every caller owns the capturer. It took `&self`
    /// only because the flag happened to be an `Arc<AtomicBool>` — an implementation detail leaking
    /// into the contract, and one the mailbox flush this now also does would not have shared.
    fn set_active(&mut self, _active: bool) {}

    /// Whether this capturer can still produce frames — the gate a caller that POOLS capturers
    /// across streams must consult before reusing one.
    ///
    /// Some backends have TERMINAL states that are only observable by trying to consume a frame:
    /// the Linux portal capturer's zero-copy poison flag, a dead PipeWire thread, and a source that
    /// never returns to `Streaming` are all sticky, and each makes every subsequent
    /// [`next_frame`](Self::next_frame) / [`try_latest`](Self::try_latest) fail — for that backend
    /// an `Err` from either is terminal, never transient. A pool that re-admits such a capturer
    /// wedges the next session permanently (it re-fails at the same point, every reconnect), which
    /// is why this predicate exists rather than leaving callers to infer liveness from an error they
    /// have often already discarded.
    ///
    /// `true` (the default) for backends with no such state: the synthetic sources, and the Windows
    /// IDD-push capturer, whose failures already end the session through its own rebuild path.
    fn is_alive(&self) -> bool {
        true
    }

    // ---- Cursor -----------------------------------------------------------------------------
    // The out-of-band pointer: where it is, who draws it, and (Linux/gamescope) where to read it.

    /// The capture source's LIVE cursor state, when it arrives out-of-band from the frames
    /// (the Windows IddCx hardware-cursor channel). Polled by the encode loop every tick and
    /// preferred over `CapturedFrame::cursor` — with a hardware cursor, pointer-only moves
    /// produce NO new frame, so the frame-attached overlay would go stale on a static desktop.
    /// Default `None`: the Linux portal path attaches its cursor to frames instead.
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        None
    }

    /// LIVE cursor-render flip for a cursor-forward session (design/remote-desktop-sweep.md §8):
    /// `on = true` — the client draws the pointer, keep it OUT of the video; `on = false` —
    /// the capture mouse model, the pointer must be IN the video again. The Windows IDD
    /// capturer implements the composite side ITSELF (slot-copy + alpha-blended quad from the
    /// GDI poller) — a declared IddCx hardware cursor is irrevocable, so DWM can never be
    /// handed the job back. Called every encode tick (implementations cache; steady state is
    /// one compare). Default no-op: the Linux portal never bakes the pointer into frames —
    /// the encode loop blends its overlay instead.
    fn set_cursor_forward(&mut self, _on: bool) {}

    /// Attach a gamescope cursor source (remote-desktop-sweep Phase C). gamescope paints no
    /// `SPA_META_Cursor`, so [`cursor`](Self::cursor)'s slot stays empty — this hands the Linux
    /// portal capturer a way to reach gamescope's nested Xwaylands (it may run several — one per
    /// `--xwayland-count`) so it reads the pointer shape/position over X11 (XFixes +
    /// QueryPointer), following whichever display is focused, and publishes it into that same slot.
    /// Called once, after the capturer is built, only for gamescope sessions. Default no-op: every
    /// non-gamescope capturer already has a cursor source.
    #[cfg(target_os = "linux")]
    fn attach_gamescope_cursor(&mut self, _targets: GamescopeCursorTargets) {}

    // ---- Stream properties ------------------------------------------------------------------

    /// The source's static HDR mastering metadata (SMPTE ST.2086 + content light level), when the
    /// capturer can read it from the output (Windows `IDXGIOutput6::GetDesc1`), or a generic HDR10
    /// block once an HDR stream is negotiated (Linux — neither the portal nor gamescope exposes a
    /// real mastering volume). `None` = unknown / SDR / a backend that doesn't expose it.
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

    // ---- Host-initiated resize --------------------------------------------------------------
    // These two are ONE operation split in half and must be implemented together: a backend that
    // returns `Some` from `capture_target_id` is promising `resize_output` works, and one that
    // implements `resize_output` without the identity leaves the caller no way to check that the
    // display it just reconfigured is still this capturer's. Both defaults decline.

    /// The OS display-target id this capturer is bound to (Windows IDD-push), so the resize path
    /// can verify the display it just reconfigured is STILL the one this capturer serves (an
    /// in-place resize keeps the target; a re-arrival fallback mints a new one, which needs a
    /// fresh capturer). `None` = the backend has no such identity (every non-IDD backend).
    ///
    /// PAIRED with [`resize_output`](Self::resize_output) — see the cluster note above.
    fn capture_target_id(&self) -> Option<u32> {
        None
    }

    /// HOST-INITIATED output resize (latency plan P2.3): the session's resize handler has ALREADY
    /// committed the display's new mode (the manager's in-place mode set), so a capable capturer
    /// re-sizes its capture surface NOW — no descriptor-poll debounce (that machinery stays, for
    /// EXTERNAL changes only) and no teardown: the capture pipeline and its send thread survive;
    /// only the encoder is swapped by the caller once the first new-size frame arrives. Returns
    /// `true` when handled; `false` (the default) routes the caller to the full-rebuild path.
    ///
    /// PAIRED with [`capture_target_id`](Self::capture_target_id) — see the cluster note above.
    fn resize_output(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Recreate the delivery ring at the CURRENT mode and re-run the driver attach handshake —
    /// the recovery half of a swap-chain bounce the descriptor poller cannot see: an
    /// exclusive-topology eviction (the vdisplay re-assert watchdog) is a real topology change,
    /// so the OS drives COMMIT_MODES on the live virtual display too and the driver's swap-chain
    /// is recreated while this capturer keeps waiting on the old ring attachment — frames stop
    /// with an unchanged descriptor (same mode, same HDR), so the two-strike debounce never
    /// trips. Arms the same recover-or-drop window as a real resize, so a driver that cannot
    /// re-attach still fails the session cleanly. Returns `true` when handled; `false` (the
    /// default) means the backend has no in-place ring recovery and the caller should treat the
    /// pipeline as unrecoverable in place.
    fn recreate_ring_in_place(&mut self) -> bool {
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
    /// THIS session encodes PyroWave: the frames' consumer is the wavelet encoder's own Vulkan
    /// device, which imports raw dmabufs on ANY vendor — so the capturer takes the raw-dmabuf
    /// passthrough (like the VAAPI backend) instead of the EGL→CUDA import whose payloads only
    /// NVENC can consume. Per-session (the codec is negotiated), unlike `backend_is_vaapi`.
    pub pyrowave_session: bool,
    /// THIS session's encoder can ingest a producer-native NV12 capture (the Linux raw Vulkan
    /// Video backend on an H265/AV1 session — resolved by the host facade via
    /// `pf_encode::linux_native_nv12_ok`). Gates whether the negotiation PREFERS gamescope's
    /// producer-side NV12 pod: libav VAAPI (H264's backend) would misread the two-plane buffer,
    /// so H264/GameStream/PyroWave sessions must never see NV12 frames.
    pub native_nv12_session: bool,
    /// The PyroWave encoder's Vulkan-importable dmabuf modifiers for the capture's packed-RGB fourcc,
    /// resolved when the session encodes PyroWave (the passthrough advertises them so Mutter+NVIDIA,
    /// which allocates tiled-only, still negotiates zero-copy). Empty otherwise.
    pub pyrowave_modifiers: Vec<u64>,
    /// The resolved encoder can ingest a packed 10-bit PQ CUDA payload (`pf_encode::linux_hdr_cuda_ok`
    /// — direct-SDK NVENC only). An HDR capture builds the GPU importer ONLY when this holds:
    /// libav's HDR route wants a P010 hardware frame it swscales into, so a packed-2:10:10:10 CUDA
    /// buffer would land in a P010 surface as garbage. `false` ⇒ HDR takes the CPU path, exactly as
    /// it did before the direct backend learned 10-bit.
    pub hdr_cuda_ok: bool,
}

/// Discovers gamescope's nested Xwayland cursor targets — `(DISPLAY, XAUTHORITY)`, one per
/// `--xwayland-count` — for [`Capturer::attach_gamescope_cursor`].
///
/// A CLOSURE, not the `Vec` it used to be, and re-run on a slow cadence by the cursor worker. The
/// snapshot was taken once, before the game launched: gamescope creates a second Xwayland for the
/// game but only advertises the FIRST in any child's environ, so the game's display was invisible to
/// discovery — and when the connected (Big Picture) display then reported "gamescope is not drawing
/// the pointer here", the source blanked the cursor for the whole game session, which is the exact
/// regression the module doc says it fixed. A provider also lets the worker retry a display that
/// died, and lets a stream that starts BEFORE the game converge instead of staying cursorless.
///
/// Built by the host facade (it wraps `pf_vdisplay::gamescope_xwayland_cursor_targets`), exactly
/// like [`FrameChannelSender`] — so the capture→host edge stays one-way.
#[cfg(target_os = "linux")]
pub type GamescopeCursorTargets =
    std::sync::Arc<dyn Fn() -> Vec<(String, Option<String>)> + Send + Sync>;

#[cfg(target_os = "linux")]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    true
}

/// Whether the **native-plane** capturer (a compositor virtual output) can deliver an HDR (10-bit
/// PQ/BT.2020) source **on this platform alone**, without knowing which compositor will be
/// driven — the platform half of the gate the punktfunk/1 handshake consults before negotiating
/// 10-bit (mirroring [`capturer_supports_444`]).
///
/// Linux: `false`, and this is NOT the whole Linux answer any more. It says only that no Linux
/// virtual output is HDR-capable *by platform*: Mutter's `RecordVirtual` virtual-monitor streams
/// advertise 8-bit BGRx/BGRA exclusively (still true on the GNOME 51 dev branch) and report no
/// BT2020/PQ colour capabilities, and KWin/wlroots virtual outputs are the same. The one Linux
/// virtual output that CAN be 10-bit — gamescope's PipeWire node, with our carried
/// `pipewire-hdr` patch (`packaging/gamescope`) — depends on the resolved compositor **and** the
/// resolved gamescope binary, neither of which this crate knows. The host resolves it in
/// `capture::capturer_supports_hdr_for(compositor)`, which consults this for the platform floor;
/// the other Linux HDR path (the GNOME 50+ portal **monitor mirror**, `open_portal_monitor` with
/// `want_hdr`) is gated separately by the GameStream plane (`host_hdr_capable` + the live monitor
/// colour-mode probe).
#[cfg(target_os = "linux")]
pub fn capturer_supports_hdr() -> bool {
    false
}
/// Windows: the IDD-push capturer proactively enables advanced colour and delivers P010/Rgb10a2.
#[cfg(target_os = "windows")]
pub fn capturer_supports_hdr() -> bool {
    true
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_hdr() -> bool {
    false
}

/// Which HDR capture source a `want_hdr` negotiation failure belongs to.
///
/// The failure latch below is **per source**, because the two Linux HDR sources fail for
/// completely unrelated reasons and share nothing but the word "HDR": the portal monitor mirror
/// fails when the mirrored monitor leaves HDR mode (a live, box-state fact), a gamescope virtual
/// output fails when the spawned binary has no 10-bit formats (a static, binary-identity fact).
/// A single process-wide latch let either one disable the other until the host restarted.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrSource {
    /// The GNOME 50+ portal **monitor mirror** (`open_portal_monitor` with `want_hdr`) — the
    /// GameStream plane's HDR path.
    PortalMonitor,
    /// A compositor **virtual output** (`open_virtual_output` with `want_hdr`) — today only
    /// gamescope's PipeWire node, with the carried `pipewire-hdr` patch.
    VirtualOutput,
}

/// Per-source latch: a `want_hdr` capture failed to negotiate the HDR (10-bit PQ) offer — the
/// producer never accepted it (monitor left HDR mode between the probe and the negotiation,
/// NVIDIA EGL not listing LINEAR for XR30, an unpatched gamescope…). Later sessions **on that
/// same source** consult [`hdr_capture_failed`] and fall back to the SDR offer instead of
/// re-running the same doomed 10-second negotiation timeout on every reconnect. Sticky until host
/// restart (matching the zero-copy downgrade latches); the log line at latch time says so.
/// Indexed by [`HdrSource`] — see its doc for why one shared latch was wrong.
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
    // IDD-push delivers full-chroma RGB for a 4:4:4 session — BGRA on an SDR display, packed 10-bit
    // BT.2020 PQ (`Rgb10a2`) on an HDR one — skipping the subsampling converters entirely. Only a
    // backend that ingests RGB and CSCs it to 4:4:4 itself can use that: today just direct-NVENC
    // (AMF can't 4:4:4 at all; the QSV/ffmpeg path has no RGB-input 4:4:4 wiring).
    //
    // The display's HDR state is deliberately NOT part of this answer, and no longer needs to be:
    // both depths have a full-chroma source now, so the chroma resolved here — before the Welcome —
    // is the chroma the stream really carries. (It used to be a lie whenever the display was HDR:
    // this returned true, the Welcome promised 4:4:4, and the capturer then quietly emitted P010.)
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

/// Delivery closure for the v5 hardware-cursor channel (`IOCTL_SET_CURSOR_CHANNEL`) — same
/// facade contract as [`FrameChannelSender`]. `Some` also OPTS THE SESSION IN: the capturer
/// creates + delivers the cursor section only when the host hands it a sender (the negotiated
/// cursor-forward sessions), and the driver only declares the hardware cursor once that
/// delivery lands — so a plain session keeps DWM's composited pointer untouched.
#[cfg(target_os = "windows")]
pub type CursorChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetCursorChannelRequest) -> Result<()> + Send + Sync,
>;

/// The mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`, proto v6) as a host-facade
/// closure — same contract as [`CursorChannelSender`]. `bool` = declare the IddCx hardware
/// cursor (`true`) or stand it down (`false`; the host facade additionally forces the same-mode
/// re-commit that actualises the OS's software-cursor default). The capturer drives this from
/// its secure-desktop watch: UAC/Winlogon render only through the software-cursor path, so a
/// path pinned to the hardware cursor never presents them (the 0.18.0 secure-desktop
/// regression).
#[cfg(target_os = "windows")]
pub type CursorForwardSender = std::sync::Arc<dyn Fn(bool) -> Result<()> + Send + Sync>;

// One-time PipeWire library init, shared by the video (portal) and audio capture threads.
#[cfg(target_os = "linux")]
pub mod pwinit;

// Which clock the wire's `pts_ns` comes from, and how clean each candidate is. Only the Linux
// capturer consumes it, but the arithmetic is pure and platform-independent, so its tests run
// everywhere rather than only where the backend does.
#[cfg(any(target_os = "linux", test))]
mod pts_provenance;

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
// The GNOME BT.2100 colour-mode probe — the host's capture-side gate for offering HDR on the
// portal monitor path (see `open_portal_monitor`'s `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
#[cfg(target_os = "windows")]
#[path = "windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;

/// Open the Linux xdg-ScreenCast portal capturer for a client-sized monitor. `anchored` drives
/// ScreenCast off a RemoteDesktop session (KWin/GNOME) so it inherits that grant headlessly.
/// `want_hdr` offers the GNOME 50+ HDR formats (10-bit PQ/BT.2020 dmabufs) instead of the SDR
/// set — pass it only when the mirrored monitor is actually in HDR mode (the host probes
/// DisplayConfig) or the negotiation runs into its 10 s timeout and latches the SDR downgrade.
/// `want_metadata_cursor` asks for cursor-as-metadata (`SPA_META_Cursor`) — pass it only when
/// the session's encode path composites `CapturedFrame::cursor` (the host consults
/// `pf-encode`'s `cursor_blend_capable`); otherwise the portal EMBEDS the pointer so it is
/// never silently lost. The [`ZeroCopyPolicy`] carries the pre-resolved encode-backend facts
/// (the one-way edge).
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

/// Open the Linux portal capturer bound to an already-created virtual output's PipeWire node. The
/// caller (host facade) explodes its `VirtualOutput` into these primitives + owns nothing after —
/// the capturer takes `keepalive`, so dropping it releases the output. `allow_zerocopy` mirrors
/// `OutputFormat::gpu`; `want_444` selects the planar-YUV444 GPU convert. `want_hdr` offers the
/// 10-bit PQ/BT.2020 formats instead of the SDR set — pass it only when the output was actually
/// brought up HDR (a gamescope spawned with `--hdr-enabled` off our `pipewire-hdr` build); the
/// host resolves that in `capture::capturer_supports_hdr_for` **before** the Welcome, because a
/// session that negotiated PQ cannot fall back to SDR afterwards. `cursor_id0_hides` declares the
/// producer's cursor-meta contract — pass it for outputs whose compositor rewrites
/// `SPA_META_Cursor` on every buffer (KWin), where an `id == 0` meta is an authoritative
/// "pointer hidden" the composited/forwarded cursor must honor.
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

/// Open the Windows IDD direct-push capturer on a pf-vdisplay target. `sender` delivers the sealed
/// frame channel to the driver (the host facade builds it from the vdisplay control device). On
/// failure the `keepalive` is handed back so the caller can retire the display.
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
