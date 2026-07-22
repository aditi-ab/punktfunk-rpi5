//! Live capture: xdg ScreenCast portal (`ashpd`) → PipeWire (`pipewire`), CPU-copy path.
//!
//! Two dedicated threads, because both stacks are tied to their thread:
//!   * **portal thread** drives the async ashpd handshake on a multi-thread tokio runtime
//!     (control plane — never the per-frame path), then parks on a pending future so the
//!     `proxy` + its zbus connection stay alive (the cast is torn down when that connection
//!     drops; ashpd's `Session` has no `Drop`);
//!   * **pipewire thread** owns the (`!Send`) MainLoop/Stream and pumps frames.
//!
//! The portal hands the PipeWire remote fd + node id to the pipewire thread; decoded BGRx
//! frames leave the pipewire thread over a bounded channel. The authoritative frame size
//! comes from the negotiated PipeWire format, not the portal's size hint.
//!
//! Cleanup: the pipewire thread is stopped deterministically — [`PortalCapturer`]'s `Drop`
//! sends a pipewire `channel` quit and joins the thread, so dropping a capturer (session end,
//! or a retried/failed pipeline build) releases its EGL importer / CUDA context promptly
//! instead of leaking it to process exit. The portal thread (when used) still parks on its zbus
//! connection until process exit.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{CapturedFrame, Capturer, DmabufFrame, FramePayload, PixelFormat, ZeroCopyPolicy};
use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Live monitor capturer backed by the portal + PipeWire threads. Kept alive (reused) across
/// streams — [`set_active`](Capturer::set_active) gates the per-frame de-pad copy so it costs
/// almost nothing between streams while the screencast session stays up (instant reconnect,
/// and no second session to conflict with).
pub struct PortalCapturer {
    frames: Receiver<CapturedFrame>,
    /// A frame [`wait_arrival`](Capturer::wait_arrival) received while blocking (the channel
    /// can't be peeked, so the wait must consume) — always the FIRST candidate the next
    /// `try_latest`/`next_frame` considers, before draining anything newer off the channel.
    pending: Option<CapturedFrame>,
    active: Arc<AtomicBool>,
    /// Set true once the PipeWire stream agrees a video format. Read in [`next_frame`]'s timeout
    /// branch to tell "format never negotiated" (modifier/format mismatch) apart from "negotiated
    /// but no buffers arrived" (compositor idle/unmapped) — the two black-screen root causes.
    negotiated: Arc<AtomicBool>,
    /// True only while the PipeWire stream is `Streaming`. [`try_latest`](Self::try_latest) reads it
    /// to distinguish a static desktop (alive, no new buffers) from a dead source (left `Streaming`).
    streaming: Arc<AtomicBool>,
    /// Poison flag: the zero-copy GPU import is irrecoverably gone for this stream (the import
    /// worker died — e.g. it absorbed the driver fault of a crashing compositor — or tiled imports
    /// failed repeatedly, where the CPU fallback would de-pad scrambled tiled bytes). Both
    /// [`next_frame`](Capturer::next_frame) and [`try_latest`](Self::try_latest) surface it as an
    /// error so the session's capture-loss rebuild runs instead of freezing/corrupting.
    broken: Arc<AtomicBool>,
    /// When the stream first dropped out of `Streaming` with no new frame; used to grace a transient
    /// renegotiation before declaring the source lost. Cleared whenever a frame arrives or the stream
    /// is `Streaming`.
    stall_since: Option<std::time::Instant>,
    /// True when this capture runs the VAAPI dmabuf passthrough (a LINEAR-dmabuf-only offer). If
    /// that offer never negotiates, [`next_frame`](Capturer::next_frame)'s timeout branch latches
    /// the process-wide downgrade ([`pf_zerocopy::note_vaapi_dmabuf_failed`]) so the pipeline
    /// rebuild retries on the CPU offer instead of failing identically forever.
    vaapi_dmabuf: bool,
    /// This capture ran the HDR (10-bit PQ/BT.2020 dmabuf) offer — see [`Self::open`]'s
    /// `want_hdr`. Read by the negotiation-timeout diagnosis (a failed HDR offer latches the
    /// process-wide SDR downgrade) and by [`hdr_meta`](Capturer::hdr_meta).
    hdr_offer: bool,
    /// Set once the stream negotiated one of the 10-bit PQ formats (`param_changed`), i.e. frames
    /// really are PQ/BT.2020 — drives [`hdr_meta`](Capturer::hdr_meta).
    hdr_negotiated: Arc<AtomicBool>,
    /// The PipeWire node this capturer consumes — surfaced in error messages for diagnosis.
    node_id: u32,
    /// Stops the PipeWire loop on teardown (sent in `Drop`). Without it a dropped or failed
    /// capturer leaks its PipeWire thread — and its EGL importer / CUDA context — because
    /// `mainloop.run()` otherwise blocks until process exit. `Option` so `Drop` can take it.
    quit: Option<::pipewire::channel::Sender<()>>,
    /// Joined in `Drop` (after `quit`) so teardown is synchronous: the importer/CUDA context is
    /// released before the next pipeline builds, not left racing it.
    join: Option<thread::JoinHandle<()>>,
    /// Owns the virtual output (if this capturer was built from one) — dropped when the capturer
    /// is, releasing the compositor-side output via the keepalive's own `Drop`. `None` for the
    /// portal source (its session ends with the portal thread's zbus connection).
    _keepalive: Option<Box<dyn Send>>,
}

impl PortalCapturer {
    /// `anchored` drives ScreenCast off a RemoteDesktop session (KWin/GNOME) so it inherits the
    /// RemoteDesktop grant and never raises a separate ScreenCast dialog; `false` uses a plain
    /// ScreenCast session (wlroots, which has no RemoteDesktop portal). `want_hdr` offers the
    /// GNOME 50+ HDR formats (10-bit PQ/BT.2020, dmabuf-only) instead of the SDR set.
    pub fn open(anchored: bool, want_hdr: bool, policy: ZeroCopyPolicy) -> Result<PortalCapturer> {
        // Portal handshake (async) on its own thread; hands back the PW fd + node id.
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        thread::Builder::new()
            .name("punktfunk-portal".into())
            .spawn(move || {
                if anchored {
                    portal_thread_remote_desktop(setup_tx)
                } else {
                    portal_thread(setup_tx)
                }
            })
            .context("spawn portal thread")?;

        let (fd, node_id) = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(anyhow!("ScreenCast portal setup failed: {e}")),
            Err(_) => return Err(anyhow!("timed out waiting for the ScreenCast portal")),
        };
        tracing::info!(
            node_id,
            want_hdr,
            "ScreenCast portal session started; connecting PipeWire"
        );
        // This portal path (GameStream / monitor capture) is always 4:2:0, so allow zero-copy as before.
        Ok(
            spawn_pipewire(Some(fd), node_id, None, true, false, want_hdr, policy)?
                .into_capturer(node_id, None),
        )
    }

    /// Build a capturer from an already-created virtual output's PipeWire node. The host facade
    /// explodes its `vdisplay::VirtualOutput` into these primitives so this crate never depends on
    /// the vdisplay type: `remote_fd` selects portal-remote vs. default-daemon, `node_id` is the
    /// output's screencast node, `preferred_mode` seeds format negotiation, and `keepalive` owns the
    /// output (dropping the capturer releases it). `allow_zerocopy` mirrors
    /// [`OutputFormat::gpu`](pf_frame::OutputFormat): `false` forces the CPU mmap path, `true` keeps
    /// the GPU zero-copy path subject to `PUNKTFUNK_ZEROCOPY`. `want_444` (a 4:4:4 session) makes the
    /// zero-copy worker convert tiled dmabufs to planar YUV444 on the GPU instead of NV12/RGB.
    #[allow(clippy::too_many_arguments)]
    pub fn from_virtual_output(
        remote_fd: Option<OwnedFd>,
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
        allow_zerocopy: bool,
        want_444: bool,
        policy: ZeroCopyPolicy,
    ) -> Result<PortalCapturer> {
        tracing::info!(
            node_id,
            allow_zerocopy,
            want_444,
            "connecting PipeWire to virtual output"
        );
        // Virtual outputs are SDR-only upstream (Mutter's RecordVirtual streams advertise 8-bit
        // BGRx/BGRA exclusively, GNOME 50 and 51-dev alike) — never run the HDR offer here.
        Ok(spawn_pipewire(
            remote_fd,
            node_id,
            preferred_mode,
            allow_zerocopy,
            want_444,
            false,
            policy,
        )?
        .into_capturer(node_id, Some(keepalive)))
    }
}

/// Live PipeWire-thread handles returned by [`spawn_pipewire`]: the frame channel, the
/// activation flag the per-frame copy gates on, a "format negotiated" flag (timeout diagnostics),
/// a quit sender that stops the loop, and the thread's join handle (synchronous teardown).
struct PwHandles {
    frames: Receiver<CapturedFrame>,
    active: Arc<AtomicBool>,
    negotiated: Arc<AtomicBool>,
    streaming: Arc<AtomicBool>,
    /// See [`PortalCapturer::broken`].
    broken: Arc<AtomicBool>,
    /// This capture will offer LINEAR-dmabuf-only for the VAAPI passthrough (see
    /// [`PortalCapturer::vaapi_dmabuf`]).
    vaapi_dmabuf: bool,
    /// This capture ran the HDR offer (see [`PortalCapturer::hdr_offer`]).
    hdr_offer: bool,
    /// See [`PortalCapturer::hdr_negotiated`].
    hdr_negotiated: Arc<AtomicBool>,
    quit: ::pipewire::channel::Sender<()>,
    join: thread::JoinHandle<()>,
}

impl PwHandles {
    /// Assemble a [`PortalCapturer`] around these handles. `node_id` is carried for diagnostics;
    /// `keepalive` owns the virtual output (drops after the PipeWire thread is joined).
    fn into_capturer(self, node_id: u32, keepalive: Option<Box<dyn Send>>) -> PortalCapturer {
        PortalCapturer {
            frames: self.frames,
            pending: None,
            active: self.active,
            negotiated: self.negotiated,
            streaming: self.streaming,
            broken: self.broken,
            stall_since: None,
            vaapi_dmabuf: self.vaapi_dmabuf,
            hdr_offer: self.hdr_offer,
            hdr_negotiated: self.hdr_negotiated,
            node_id,
            quit: Some(self.quit),
            join: Some(self.join),
            _keepalive: keepalive,
        }
    }
}

/// Spawn the PipeWire consumer thread for `node_id` (fd `Some` = portal remote, `None` =
/// default daemon) and return its [`PwHandles`]. `preferred` seeds the format negotiation's
/// default size/framerate — for Mutter virtual monitors this is what actually sizes the monitor.
fn spawn_pipewire(
    fd: Option<OwnedFd>,
    node_id: u32,
    preferred: Option<(u32, u32, u32)>,
    // Allow GPU zero-copy capture (dmabuf→CUDA/VA). `false` forces the CPU mmap path even when
    // `PUNKTFUNK_ZEROCOPY` is set (the session plan passes `gpu = false` when 4:4:4 has no
    // zero-copy convert available — see `SessionPlan::output_format`).
    allow_zerocopy: bool,
    // 4:4:4 session: tiled dmabufs convert to planar YUV444 on the GPU (`ImportKind::Tiled444`)
    // instead of NV12/RGB, so the session stays zero-copy at full chroma.
    want_444: bool,
    // HDR session (GNOME 50+ monitor mirror): offer ONLY the 10-bit PQ/BT.2020 formats as
    // LINEAR dmabufs (SHM can't carry them — Mutter's SHM record path paints 8-bit ARGB32
    // regardless of the negotiated format, and the tiled EGL de-tile blit is 8-bit).
    want_hdr: bool,
    // Encode-backend facts resolved by the facade (never re-derived here) — the one-way
    // capture→encode edge (plan §W6).
    policy: ZeroCopyPolicy,
) -> Result<PwHandles> {
    // Frames flow from the pipewire thread over a small bounded channel.
    let (frame_tx, frame_rx) = sync_channel::<CapturedFrame>(8);
    let active = Arc::new(AtomicBool::new(false));
    let active_cb = active.clone();
    let negotiated = Arc::new(AtomicBool::new(false));
    let negotiated_cb = negotiated.clone();
    let streaming = Arc::new(AtomicBool::new(false));
    let streaming_cb = streaming.clone();
    let broken = Arc::new(AtomicBool::new(false));
    let broken_cb = broken.clone();
    let hdr_negotiated = Arc::new(AtomicBool::new(false));
    let hdr_negotiated_cb = hdr_negotiated.clone();
    // pipewire's own cross-thread channel: the receiver attaches to the loop and quits it; the
    // sender lives on the capturer and fires in its `Drop`. Absolute `::pipewire` path — the
    // inner `mod pipewire` shadows the crate name at this scope.
    let (quit_tx, quit_rx) = ::pipewire::channel::channel::<()>();
    let zerocopy = allow_zerocopy && pf_zerocopy::enabled();
    // HDR cannot ride the SHM path (see `want_hdr` above): under PUNKTFUNK_FORCE_SHM the HDR
    // offer is dropped — SDR capture, loudly.
    let force_shm = std::env::var("PUNKTFUNK_FORCE_SHM").as_deref() == Ok("1");
    let want_hdr = if want_hdr && force_shm {
        tracing::warn!(
            "HDR capture requested but PUNKTFUNK_FORCE_SHM=1 — the SHM path is 8-bit only; \
             offering SDR"
        );
        false
    } else {
        want_hdr
    };
    // Mirror of the thread's `vaapi_passthrough` decision (deterministic from here: on a VAAPI
    // backend or a PyroWave session the EGL→CUDA importer is never built) — kept on the capturer
    // so `next_frame`'s negotiation-timeout branch knows a failed negotiation was the raw-dmabuf
    // passthrough offer.
    let vaapi_dmabuf =
        zerocopy && !force_shm && (policy.backend_is_vaapi || policy.pyrowave_session);
    let join = thread::Builder::new()
        .name("punktfunk-pipewire".into())
        .spawn(move || {
            if let Err(e) = pipewire::pipewire_thread(
                fd,
                node_id,
                frame_tx,
                active_cb,
                negotiated_cb,
                streaming_cb,
                broken_cb,
                hdr_negotiated_cb,
                zerocopy,
                want_444,
                want_hdr,
                preferred,
                quit_rx,
                policy,
            ) {
                tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
            }
        })
        .context("spawn pipewire thread")?;
    Ok(PwHandles {
        frames: frame_rx,
        active,
        negotiated,
        streaming,
        broken,
        vaapi_dmabuf,
        hdr_offer: want_hdr,
        hdr_negotiated,
        quit: quit_tx,
        join,
    })
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.frame_within(Duration::from_secs(10))
    }

    fn next_frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        self.frame_within(budget)
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        // Frame-driven trigger (latency plan T1.1): block on the PipeWire channel until the
        // compositor delivers a frame or the deadline passes. The channel can't be peeked, so
        // the received frame is stashed in `pending` — `try_latest` starts from it and still
        // drains anything newer. A broken/ended stream just returns; the following
        // `try_latest` surfaces the error through its existing paths.
        if self.pending.is_some() || self.broken.load(Ordering::Relaxed) {
            return;
        }
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return;
        };
        if let Ok(f) = self.frames.recv_timeout(left) {
            self.pending = Some(f);
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "zero-copy GPU import lost (node {}): the import worker died or tiled imports \
                 failed repeatedly — rebuilding capture",
                self.node_id
            ));
        }
        // Drain to the newest queued frame without blocking; `None` means the compositor
        // hasn't produced a new frame since last call (static/idle desktop). A frame
        // `wait_arrival` stashed is the oldest candidate — anything on the channel is newer.
        let mut latest = self.pending.take();
        loop {
            match self.frames.try_recv() {
                Ok(frame) => latest = Some(frame),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("PipeWire capture thread ended"))
                }
            }
        }
        if latest.is_some() || self.streaming.load(Ordering::Relaxed) {
            // A frame arrived, or the source is alive but idle (static desktop) — normal. Clear any
            // stall and repeat the last frame on `None`, exactly as before.
            self.stall_since = None;
            return Ok(latest);
        }
        // No new frame AND the stream has left `Streaming` (Paused/Unconnected/Error). The source
        // went away — a compositor torn down on a Gaming↔Desktop switch, a removed virtual output.
        // Grace a brief window (a transient mid-stream renegotiation can blip out of Streaming and
        // back) before declaring it lost so the encode loop rebuilds in place rather than freezing
        // on the last frame forever.
        const STALL_GRACE: Duration = Duration::from_millis(1500);
        let since = *self.stall_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= STALL_GRACE {
            self.stall_since = None;
            return Err(anyhow!(
                "PipeWire source stalled (node {}): stream left Streaming for >{}ms with no frames \
                 — the compositor/virtual output went away (session switch?)",
                self.node_id,
                STALL_GRACE.as_millis()
            ));
        }
        Ok(latest)
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    /// Generic HDR10 mastering metadata once the stream negotiated a 10-bit PQ format. Mutter
    /// exposes no per-monitor mastering volume through the screencast, so this is the standard
    /// HDR10 default block (BT.2020 primaries, D65 white, 1000 / 0.005 cd/m², CLL unknown) — the
    /// same fallback Windows uses when a display reports nothing. The native stream loop prefers
    /// the client display's own volume when the client sent one (`Hello::display_hdr`).
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        if !self.hdr_negotiated.load(Ordering::Relaxed) {
            return None;
        }
        Some(punktfunk_core::quic::HdrMeta {
            // ST.2086 order G, B, R; (x, y) chromaticity in 1/50000 units.
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],                 // D65
            max_display_mastering_luminance: 10_000_000, // 1000 cd/m² (0.0001 units)
            min_display_mastering_luminance: 50,         // 0.005 cd/m²
            max_cll: 0,
            max_fall: 0,
        })
    }
}

impl PortalCapturer {
    /// The blocking first-frame wait behind [`Capturer::next_frame`] /
    /// [`Capturer::next_frame_within`]. First frame can lag behind format negotiation; later
    /// frames arrive at ~fps. Wait in short slices so a GPU-import poison (worker death) fails
    /// the capture within ~0.5 s instead of sitting out the full first-frame budget.
    fn frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if self.broken.load(Ordering::Relaxed) {
                return Err(anyhow!(
                    "zero-copy GPU import lost (node {}): the import worker died or tiled imports \
                     failed repeatedly — rebuilding capture",
                    self.node_id
                ));
            }
            if let Some(f) = self.pending.take() {
                return Ok(f); // a wait_arrival stash outranks the channel (it's older)
            }
            let slice = Duration::from_millis(500)
                .min(deadline.saturating_duration_since(std::time::Instant::now()));
            match self.frames.recv_timeout(slice) {
                Ok(frame) => return Ok(frame),
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => continue,
                Err(e) => return self.next_frame_timed_out(e, budget),
            }
        }
    }

    /// The [`frame_within`](Self::frame_within) budget expired (or the thread ended) — turn it
    /// into the diagnosis-bearing error. Split out of the slicing loop above; behavior unchanged.
    fn next_frame_timed_out(
        &self,
        err: RecvTimeoutError,
        budget: Duration,
    ) -> Result<CapturedFrame> {
        let within = budget.as_secs_f32();
        match err {
            RecvTimeoutError::Timeout => {
                // Split the two black-screen root causes apart so the operator gets a cause, not
                // just a symptom: did the format negotiate (compositor produced no buffers) or
                // not (no acceptable format / node never emitted a param)?
                if self.negotiated.load(Ordering::Relaxed) {
                    Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiated but no \
                         buffers arrived — the compositor produced no frames (virtual output \
                         idle/unmapped, capture never started, or a stream bound during a \
                         compositor (re)start that will never deliver — a reconnect fixes that)",
                        self.node_id
                    ))
                } else if self.hdr_offer {
                    // The HDR (10-bit PQ dmabuf) offer was never accepted — the monitor left HDR
                    // mode between the probe and the negotiation, the compositor pre-dates the
                    // GNOME 50 HDR formats, or its allocator can't do LINEAR for XR30/XB30.
                    // Latch the process-wide SDR downgrade so the next session (Moonlight
                    // auto-reconnects) negotiates SDR instead of re-running this same timeout.
                    super::note_hdr_capture_failed();
                    Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): the compositor never \
                         accepted the HDR (10-bit PQ/BT.2020 dmabuf) offer — is the mirrored \
                         monitor in HDR mode on GNOME 50+? Downgrading this host to SDR capture; \
                         reconnect to stream SDR",
                        self.node_id
                    ))
                } else if self.vaapi_dmabuf && !pf_zerocopy::vaapi_dmabuf_forced() {
                    // The LINEAR-dmabuf-only offer (VAAPI passthrough default) was never accepted.
                    // Latch the process-wide downgrade so the encode loop's pipeline rebuild
                    // retries on the CPU offer instead of failing this same negotiation forever.
                    pf_zerocopy::note_vaapi_dmabuf_failed();
                    Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): the compositor never \
                         accepted the LINEAR-dmabuf offer (VAAPI zero-copy) — downgrading this \
                         host to the CPU capture path; the pipeline rebuild will renegotiate \
                         without dmabuf",
                        self.node_id
                    ))
                } else {
                    Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiation never \
                         completed — the compositor offered no format this consumer accepts \
                         (pixel-format/modifier mismatch) or the node never emitted a Format param",
                        self.node_id
                    ))
                }
            }
            RecvTimeoutError::Disconnected => Err(anyhow!(
                "PipeWire capture thread ended before a frame (node {})",
                self.node_id
            )),
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        // Stop the PipeWire loop and wait for the thread to unwind BEFORE the keepalive (virtual
        // output) drops: quit → join releases the EGL importer / CUDA context, then field-drop
        // order releases the output. Without this, `mainloop.run()` blocks until process exit, so
        // every dropped/failed capturer (e.g. a retried first-frame attempt) leaks a thread + GPU
        // context. `send` errors only if the thread already exited — then `join` returns at once.
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Whether any monitor of the live GNOME session is currently in BT.2100 (HDR) colour mode — the
/// precondition for Mutter's monitor screencast advertising the 10-bit PQ formats (GNOME 50+;
/// Mutter only appends the HDR formats while the mirrored monitor's colour state is BT.2020+PQ).
/// Queried over the session bus: `DisplayConfig.GetCurrentState`, monitor property
/// `"color-mode" == 1` (`META_COLOR_MODE_BT2100`). `false` on any error — not GNOME, a pre-48
/// Mutter without colour modes, no monitors — so callers fall back to the honest SDR offer.
/// Blocking (one D-Bus round-trip on a fresh connection); call from control-plane threads only.
pub fn gnome_hdr_monitor_active() -> bool {
    use ashpd::zbus;
    // GetCurrentState reply: (serial, monitors, logical_monitors, properties); each monitor is
    // (spec(ssss), modes a(siiddada{sv}), properties a{sv}) — "color-mode" lives in the monitor
    // properties.
    type Mode = (
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type Monitor = (
        (String, String, String, String),
        Vec<Mode>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type LogicalMonitor = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<(String, String, String, String)>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type State = (
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    let probe = || -> Result<bool> {
        // zbus is built async-only here (ashpd's tokio integration) — run the one round-trip on
        // a throwaway current-thread runtime; this is a control-plane call, never per-frame.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        rt.block_on(async {
            let conn = zbus::Connection::session().await.context("session bus")?;
            let reply = conn
                .call_method(
                    Some("org.gnome.Mutter.DisplayConfig"),
                    "/org/gnome/Mutter/DisplayConfig",
                    Some("org.gnome.Mutter.DisplayConfig"),
                    "GetCurrentState",
                    &(),
                )
                .await
                .context("DisplayConfig.GetCurrentState")?;
            let (_serial, monitors, _logical, _props): State = reply
                .body()
                .deserialize()
                .context("parse GetCurrentState")?;
            Ok(monitors.iter().any(|(_spec, _modes, props)| {
                props
                    .get("color-mode")
                    .and_then(|v| u32::try_from(v).ok())
                    .is_some_and(|mode| mode == 1) // META_COLOR_MODE_BT2100
            }))
        })
    };
    match probe() {
        Ok(hdr) => hdr,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "GNOME HDR colour-mode probe failed — SDR");
            false
        }
    }
}

/// Pick the ScreenCast cursor mode from what the backend advertises (`AvailableCursorModes`),
/// preferring **cursor-as-metadata**: the compositor keeps its cheap hardware cursor plane and
/// ships the pointer as PipeWire `SPA_META_Cursor` metadata (position + an occasional bitmap),
/// which the consumer composites itself. That avoids forcing the producer to burn the cursor into
/// every frame — the `Embedded` mode — which on gamescope would defeat its HW cursor plane. Falls
/// back to `Embedded`, then `Hidden`, and (if the property query fails, e.g. an older portal)
/// keeps the prior `Embedded` behavior so the cursor is never silently lost.
async fn choose_cursor_mode(
    proxy: &ashpd::desktop::screencast::Screencast,
) -> ashpd::desktop::screencast::CursorMode {
    use ashpd::desktop::screencast::CursorMode;
    match proxy.available_cursor_modes().await {
        Ok(avail) if avail.contains(CursorMode::Metadata) => {
            tracing::info!(
                ?avail,
                "ScreenCast: requesting cursor-as-metadata (SPA_META_Cursor)"
            );
            CursorMode::Metadata
        }
        Ok(avail) if avail.contains(CursorMode::Embedded) => {
            tracing::info!(
                ?avail,
                "ScreenCast: cursor metadata unavailable — requesting Embedded cursor"
            );
            CursorMode::Embedded
        }
        Ok(avail) => {
            tracing::warn!(
                ?avail,
                "ScreenCast: neither Metadata nor Embedded cursor advertised — cursor will be hidden"
            );
            CursorMode::Hidden
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ScreenCast: AvailableCursorModes query failed — defaulting to Embedded cursor"
            );
            CursorMode::Embedded
        }
    }
}

/// The portal handshake: connect ScreenCast, select a single monitor, start, open the
/// PipeWire remote, hand the fd + node id back, then keep the session alive.
fn portal_thread(setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Multi-thread runtime: the zbus connection's background reader must be pumped
    // continuously across the create_session → select_sources → start handshake, or the
    // portal reports "Invalid session". (A current-thread runtime starves it.)
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    let err_tx = setup_tx.clone();

    rt.block_on(async move {
        let result: Result<()> = async {
            let proxy = Screencast::new()
                .await
                .context("connect ScreenCast portal")?;
            let session = proxy
                .create_session(Default::default())
                .await
                .context("create_session")?;
            let cursor_mode = choose_cursor_mode(&proxy).await;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(cursor_mode)
                        // Only MONITOR is offered by the wlroots backend
                        // (AvailableSourceTypes=1); requesting unsupported types
                        // invalidates the session.
                        .set_sources(BitFlags::from_flag(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .context("select_sources")?
                .response()
                .context("select_sources rejected (unsupported source type / cursor mode?)")?;
            let streams = proxy
                .start(&session, None, Default::default())
                .await
                .context("start cast")?
                .response()
                .context("start response (chooser cancelled? portal misconfigured?)")?;
            let stream = streams
                .streams()
                .first()
                .context("portal returned no streams")?
                .clone();
            let node_id = stream.pipe_wire_node_id();
            let fd = proxy
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .context("open_pipe_wire_remote")?;

            setup_tx
                .send(Ok((fd, node_id)))
                .map_err(|_| anyhow!("capturer dropped before setup completed"))?;

            // Keep `proxy` + `session` (and the underlying zbus connection) alive for the
            // capture; the cast is torn down when the connection drops (ashpd's `Session`
            // has no `Drop`), which here happens at process exit.
            let _keep_alive = (&proxy, &session);
            std::future::pending::<()>().await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
}

/// Combined RemoteDesktop+ScreenCast portal setup (KWin/GNOME). ScreenCast sources are selected
/// on a session created via RemoteDesktop, so a single RemoteDesktop `start` grant —
/// pre-authorized headlessly via the `kde-authorized` permission, exactly like the libei input
/// path — also covers screen capture, with no separate ScreenCast dialog (which has no such
/// bypass). Yields the same PipeWire fd + node id as the standalone path; the consumer is
/// identical.
fn portal_thread_remote_desktop(setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>) {
    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    let err_tx = setup_tx.clone();

    rt.block_on(async move {
        let result: Result<()> = async {
            let remote = RemoteDesktop::new()
                .await
                .context("connect RemoteDesktop portal")?;
            let screencast = Screencast::new()
                .await
                .context("connect ScreenCast portal")?;
            let session = remote
                .create_session(Default::default())
                .await
                .context("create RemoteDesktop session")?;
            // RemoteDesktop requires a device selection; we never connect_to_eis on this session
            // (input injection runs its own), but selecting devices is what makes `start` the
            // RemoteDesktop grant the kde-authorized bypass covers.
            remote
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .context("select_devices")?
                .response()
                .context("select_devices rejected")?;
            let cursor_mode = choose_cursor_mode(&screencast).await;
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(cursor_mode)
                        .set_sources(BitFlags::from_flag(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .context("select_sources")?
                .response()
                .context("select_sources rejected (unsupported source type?)")?;
            let streams = remote
                .start(&session, None, Default::default())
                .await
                .context("start RemoteDesktop+ScreenCast")?
                .response()
                .context("start response (grant not pre-authorized / headless dialog?)")?;
            let stream = streams
                .streams()
                .first()
                .context("portal returned no screencast streams")?
                .clone();
            let node_id = stream.pipe_wire_node_id();
            let fd = screencast
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .context("open_pipe_wire_remote")?;

            setup_tx
                .send(Ok((fd, node_id)))
                .map_err(|_| anyhow!("capturer dropped before setup completed"))?;

            // Keep the proxies + session (and their zbus connection) alive for the capture.
            let _keep_alive = (&remote, &screencast, &session);
            std::future::pending::<()>().await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
}

mod pipewire {
    //! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).

    use super::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat, ZeroCopyPolicy};
    use anyhow::{Context, Result};
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::SyncSender;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use spa::param::video::{VideoFormat, VideoInfoRaw};
    use spa::pod::Pod;

    /// Map a negotiated SPA video format to a layout the encoder can consume. Returns
    /// `None` for formats we don't handle (the frame is then skipped).
    fn map_format(f: VideoFormat) -> Option<PixelFormat> {
        Some(match f {
            VideoFormat::BGRx => PixelFormat::Bgrx,
            VideoFormat::RGBx => PixelFormat::Rgbx,
            VideoFormat::BGRA => PixelFormat::Bgra,
            VideoFormat::RGBA => PixelFormat::Rgba,
            VideoFormat::RGB => PixelFormat::Rgb,
            VideoFormat::BGR => PixelFormat::Bgr,
            VideoFormat::NV12 => PixelFormat::Nv12,
            // The GNOME 50+ HDR screencast formats (packed 2:10:10:10; only ever negotiated by
            // the `want_hdr` offer, whose MANDATORY colorimetry props pin them to PQ/BT.2020).
            VideoFormat::xRGB_210LE => PixelFormat::X2Rgb10,
            VideoFormat::xBGR_210LE => PixelFormat::X2Bgr10,
            _ => return None,
        })
    }

    /// Latest cursor state parsed from `SPA_META_Cursor` (cursor-as-metadata mode). Position is
    /// refreshed every buffer that carries the meta (including Mutter's cursor-only "corrupted"
    /// buffers we otherwise skip for their stale frame); the RGBA bitmap is cached and only
    /// replaced when the compositor sends a fresh one (`bitmap_offset != 0`).
    #[derive(Default)]
    struct CursorState {
        /// True when the compositor reports a visible pointer (`spa_meta_cursor.id != 0`).
        visible: bool,
        /// Top-left where the bitmap is drawn = reported position − hotspot.
        x: i32,
        y: i32,
        /// Cached straight-alpha RGBA pixels (`bw*bh*4`, bytes R,G,B,A). `Arc` so the overlay handed
        /// to each GPU frame is a refcount bump, not a copy. Empty until the first bitmap arrives.
        rgba: Arc<Vec<u8>>,
        bw: u32,
        bh: u32,
        /// Bumps whenever the bitmap (`rgba`/`bw`/`bh`) changes — stable across position-only moves,
        /// so the GPU encoder re-uploads its cursor texture only on change.
        serial: u64,
        /// The compositor-reported hotspot — carried on the overlay for the cursor-forward
        /// channel (the blend path uses the pre-adjusted `x`/`y` and never reads it).
        hot_x: i32,
        hot_y: i32,
    }

    impl CursorState {
        /// A shareable overlay for the encode/forward paths, or `None` before the first bitmap
        /// arrived. A HIDDEN pointer still yields `Some` (with `visible: false`): the
        /// cursor-forward channel needs "known but hidden" — an app grabbed the pointer, the
        /// client's relative-mode hint (M3) — which is a different fact from "no cursor yet".
        /// The encode loop strips invisible overlays before any blend path sees the frame.
        /// Cheap: clones an `Arc` + a few scalars.
        fn overlay(&self) -> Option<pf_frame::CursorOverlay> {
            if self.rgba.is_empty() {
                return None;
            }
            Some(pf_frame::CursorOverlay {
                x: self.x,
                y: self.y,
                w: self.bw,
                h: self.bh,
                rgba: self.rgba.clone(),
                serial: self.serial,
                hot_x: self.hot_x.max(0) as u32,
                hot_y: self.hot_y.max(0) as u32,
                visible: self.visible,
            })
        }
    }

    struct UserData {
        info: VideoInfoRaw,
        /// Negotiated layout (`None` until param_changed, or if unsupported).
        format: Option<PixelFormat>,
        /// Negotiated DRM format modifier (for dmabuf import); 0 = LINEAR.
        modifier: u64,
        tx: SyncSender<CapturedFrame>,
        /// When false (no active stream), skip the de-pad copy — the buffer is just released.
        active: Arc<AtomicBool>,
        /// Set once a video format is agreed (`param_changed`), so a first-frame timeout can tell
        /// "format never negotiated" apart from "negotiated but no buffers arrived".
        negotiated: Arc<AtomicBool>,
        /// True only while the PipeWire stream is in `Streaming` (the source is alive). Goes false on
        /// `Paused`/`Unconnected`/`Error` — the source vanished (compositor torn down on a session
        /// switch). Read by [`PortalCapturer::try_latest`] to surface a sustained drop as a loss.
        streaming: Arc<AtomicBool>,
        /// Poison flag (see [`PortalCapturer::broken`]): set here when the GPU import is
        /// irrecoverably gone for this stream — the import worker died, or tiled imports failed
        /// [`IMPORT_FAIL_POISON`] times in a row.
        broken: Arc<AtomicBool>,
        /// Set when the negotiated format is one of the 10-bit PQ formats (`param_changed`) —
        /// read by [`PortalCapturer::hdr_meta`](super::PortalCapturer).
        hdr_negotiated: Arc<AtomicBool>,
        /// Consecutive tiled-import failures (reset on success); see [`IMPORT_FAIL_POISON`].
        import_fail_streak: u32,
        /// Present when zero-copy is enabled on NVIDIA: imports a dmabuf → CUDA device buffer,
        /// normally via the isolated worker process (`pf_zerocopy::Importer::Remote`).
        importer: Option<pf_zerocopy::Importer>,
        /// VAAPI zero-copy: hand the raw dmabuf to the encoder (which imports + GPU-CSCs it) instead
        /// of a CUDA import. Set when zero-copy is on, the EGL→CUDA importer is unavailable, and the
        /// encoder backend is VAAPI (AMD/Intel).
        vaapi_passthrough: bool,
        /// `PUNKTFUNK_NV12`: on the tiled EGL/GL zero-copy path, convert to NV12 on the GPU and feed
        /// NVENC native YUV (Tier 2A). Off ⇒ the BGRx path is unchanged.
        nv12: bool,
        /// 4:4:4 session: on the tiled EGL/GL zero-copy path, convert to planar YUV444 on the GPU
        /// (`ImportKind::Tiled444`) and feed NVENC native full-chroma YUV — takes precedence over
        /// `nv12` (a 4:4:4 session must never subsample).
        yuv444: bool,
        /// The LINEAR (gamescope) NV12 compute CSC failed once — RGB for the rest of the stream
        /// (T2.5b's per-stream fallback latch; cleared by the next stream's fresh `Ud`).
        linear_nv12_failed: bool,
        /// Rate-limit counter for the latest-frame-only diagnostic log (see `.process`).
        dbg_log_n: u64,
        /// Cursor-as-metadata state, composited into the CPU de-pad path (see `consume_frame`).
        cursor: CursorState,
    }

    /// Consecutive tiled-import failures (worker alive, e.g. a per-buffer `EGL_BAD_MATCH`) before
    /// the stream is poisoned for rebuild. A tiled import failure must NEVER fall through to the
    /// CPU mmap path — de-padding tiled bytes as linear produces a scrambled image — so after a
    /// short streak of dropped frames the capturer fails loudly and the session renegotiates.
    const IMPORT_FAIL_POISON: u32 = 3;

    /// Log a frame-drop reason once per process (the process callback runs per frame; a stuck
    /// pipeline must say why without flooding).
    fn warn_once(msg: &'static str) {
        use std::sync::Mutex;
        static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap();
        if !seen.contains(&msg) {
            seen.push(msg);
            tracing::warn!("{msg}");
        }
    }

    /// A read-only mmap of a dmabuf fd, unmapped on drop. Used when MAP_BUFFERS didn't map the
    /// buffer (producers don't always flag dmabufs mappable, e.g. gamescope's Vulkan exports).
    struct DmabufMap {
        ptr: *mut std::ffi::c_void,
        len: usize,
    }

    impl DmabufMap {
        fn new(fd: i32, len: usize) -> Option<DmabufMap> {
            // SAFETY: a null `addr` lets the kernel choose the mapping address; `fd` is a caller-owned
            // dmabuf/MemFd fd, valid for the duration of this call, and `len` is the requested map length.
            // `mmap` reads no Rust memory — it installs a fresh PROT_READ/MAP_SHARED page mapping and
            // returns its base (or MAP_FAILED, checked below before `DmabufMap` adopts it). The returned
            // region is a brand-new VMA, so it aliases no live Rust object, and it keeps the underlying
            // object mapped independently of `fd` (which may be closed after this returns).
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            (ptr != libc::MAP_FAILED).then_some(DmabufMap { ptr, len })
        }
    }

    impl Drop for DmabufMap {
        fn drop(&mut self) {
            // SAFETY: `self.ptr`/`self.len` are exactly the base+length of a successful `mmap` in
            // `DmabufMap::new` (constructed only when `ptr != MAP_FAILED`). This `DmabufMap` uniquely owns
            // that mapping and `drop` runs once, so `munmap` releases a live mapping exactly once — no
            // double-unmap. Every `&[u8]` derived from the mapping is bounded by this `DmabufMap`'s
            // lifetime, so no borrow outlives the unmap.
            unsafe {
                libc::munmap(self.ptr, self.len);
            }
        }
    }

    fn serialize_pod(obj: pw::spa::pod::Object) -> Result<Vec<u8>> {
        Ok(pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("serialize pod")?
        .0
        .into_inner())
    }

    /// Build a LINEAR/modifier DMA-BUF `EnumFormat` pod. Packed BGRx is the existing import path;
    /// NV12 is gamescope's producer-side RGB→YUV path (opt-in during bring-up).
    fn build_dmabuf_format(
        format: VideoFormat,
        modifiers: &[u64],
        preferred: Option<(u32, u32, u32)>,
    ) -> Result<Vec<u8>> {
        let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
        use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        let mut obj = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
            pw::spa::pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: dw,
                    height: dh
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction { num: dhz, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 240, denom: 1 }
            ),
        );
        if format == VideoFormat::NV12 {
            obj.properties.push(pw::spa::pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_colorMatrix,
                flags: pw::spa::pod::PropertyFlags::MANDATORY,
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                    pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709,
                )),
            });
            obj.properties.push(pw::spa::pod::Property {
                key: pw::spa::sys::SPA_FORMAT_VIDEO_colorRange,
                flags: pw::spa::pod::PropertyFlags::MANDATORY,
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                    pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235,
                )),
            });
        }
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Long(
                pw::spa::utils::Choice(
                    pw::spa::utils::ChoiceFlags::empty(),
                    pw::spa::utils::ChoiceEnum::Enum {
                        default: modifiers[0] as i64,
                        alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                    },
                ),
            )),
        });
        serialize_pod(obj)
    }

    /// Build one GNOME 50+ HDR format pod: `format` (xRGB_210LE / xBGR_210LE) as a LINEAR-only
    /// dmabuf with **MANDATORY** BT.2020 primaries + SMPTE ST.2084 (PQ) transfer-function props —
    /// the exact colorimetry Mutter's monitor stream advertises while the mirrored monitor is in
    /// HDR mode (its HDR pods carry the same props MANDATORY, so both sides must speak them for
    /// the intersection to exist; an SDR or pre-50 producer can never match this pod).
    ///
    /// LINEAR-only because every 10-bit consumer we have reads the buffer without a de-tile pass:
    /// the CPU path mmaps it, and the VAAPI passthrough imports it into a VA surface. The tiled
    /// EGL de-tile blit renders into an 8-bit `GL_RGBA8` texture — it would silently crush the
    /// depth — so tiled modifiers are deliberately NOT advertised (a zero-copy 10-bit de-tile is
    /// the follow-up). SHM is excluded entirely: Mutter's SHM record path paints 8-bit ARGB32
    /// regardless of the negotiated format.
    /// `SPA_VIDEO_TRANSFER_SMPTE2084` (PQ) — spelled out rather than taken from `pw::spa::sys`
    /// because libspa only grew the constant with the BT2020_10/SMPTE2084/ARIB_STD_B67 block, and
    /// the distro builders (Ubuntu 24.04 noble for the .deb) ship headers predating it — bindgen
    /// then emits no such constant and the host fails to compile there, even though the code never
    /// runs on those systems (the HDR path needs GNOME 50+).
    ///
    /// 14 is the enum's position in `spa/param/video/color.h` and is wire ABI, not a private
    /// detail: SPA mirrors GStreamer's `GstVideoTransferFunction`, where that block was added
    /// together, so the value is identical on every libspa that has the symbol at all. On one that
    /// doesn't, PipeWire simply fails to intersect this format offer and the session negotiates
    /// SDR — the same outcome as not offering HDR.
    const SPA_VIDEO_TRANSFER_SMPTE2084: u32 = 14;

    fn build_hdr_dmabuf_format(
        format: VideoFormat,
        preferred: Option<(u32, u32, u32)>,
    ) -> Result<Vec<u8>> {
        let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
        use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        let mut obj = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
            pw::spa::pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: dw,
                    height: dh
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction { num: dhz, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 240, denom: 1 }
            ),
        );
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Long(0), // DRM_FORMAT_MOD_LINEAR
        });
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_transferFunction,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_TRANSFER_SMPTE2084)),
        });
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorPrimaries,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020,
            )),
        });
        serialize_pod(obj)
    }

    /// The default (shm/CPU-path) format offer: raw video in any encoder-mappable layout, any
    /// size, any framerate (0/1 = variable allowed — gamescope fixates exactly that).
    fn build_default_format_obj(preferred: Option<(u32, u32, u32)>) -> pw::spa::pod::Object {
        let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
        pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            // Offer the layouts the encoder can map to an NVENC input format. wlroots
            // commonly fixates packed RGB (3 bpp); other compositors offer 4 bpp. Only
            // these are requested, so negotiation fails loudly rather than handing us a
            // format we'd misinterpret.
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::RGB,
                VideoFormat::RGB,
                VideoFormat::BGR,
                VideoFormat::RGBx,
                VideoFormat::BGRx,
                VideoFormat::RGBA,
                VideoFormat::BGRA,
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: dw,
                    height: dh
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction { num: dhz, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 240, denom: 1 }
            ),
        )
    }

    /// Build a Buffers param for the CPU path accepting anything mappable: MemPtr, MemFd, and
    /// DmaBuf. The DmaBuf bit matters for producers like gamescope whose format intersection
    /// lands on their modifier-bearing (LINEAR) pod: they then offer *only* DmaBuf buffers, and
    /// without this bit the buffer-type intersection is empty and the link silently stalls in
    /// "negotiating". A LINEAR dmabuf is mmap-able by MAP_BUFFERS, so the CPU de-pad copy works.
    fn build_mappable_buffers() -> Result<Vec<u8>> {
        serialize_pod(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: pw::spa::param::ParamType::Buffers.as_raw(),
            properties: vec![pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(
                    (1i32 << pw::spa::sys::SPA_DATA_MemPtr)
                        | (1i32 << pw::spa::sys::SPA_DATA_MemFd)
                        | (1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
                ),
            }],
        })
    }

    /// Build a Buffers param for a TRUE SHM path: MemPtr + MemFd only, NO DmaBuf. Forces the
    /// producer to download into mappable memory (Mutter's `glReadPixels`), which orders against its
    /// render — so the frame is complete and current by construction. This is the only race-free
    /// capture of Mutter's virtual monitor on NVIDIA: the compositor renders straight into the buffer
    /// pool, NVIDIA attaches no implicit dmabuf fence (verified: `EXPORT_SYNC_FILE` waited=false) and
    /// can't produce an explicit sync_fd, so any dmabuf read (zero-copy OR mmap) races the render and
    /// flashes the buffer's previous frame. Excluding DmaBuf is what makes the difference vs.
    /// `build_mappable_buffers` (which still let Mutter hand dmabufs).
    fn build_shm_only_buffers() -> Result<Vec<u8>> {
        serialize_pod(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: pw::spa::param::ParamType::Buffers.as_raw(),
            properties: vec![pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(
                    (1i32 << pw::spa::sys::SPA_DATA_MemPtr)
                        | (1i32 << pw::spa::sys::SPA_DATA_MemFd),
                ),
            }],
        })
    }

    /// Build a Buffers param requesting dmabuf-only buffers.
    fn build_dmabuf_buffers() -> Result<Vec<u8>> {
        serialize_pod(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: pw::spa::param::ParamType::Buffers.as_raw(),
            properties: vec![pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
            }],
        })
    }

    /// Request the compositor attach `SPA_META_Cursor` to each buffer, so the pointer travels as
    /// metadata (position + an occasional bitmap) instead of being burned into the frame. Paired
    /// with the portal's `CursorMode::Metadata`; producers that don't support it simply don't
    /// attach it (harmless). Size is a range up to a 256×256 bitmap — bigger than any real cursor.
    fn build_cursor_meta_param() -> Result<Vec<u8>> {
        fn meta_size(w: u32, h: u32) -> i32 {
            (std::mem::size_of::<spa::sys::spa_meta_cursor>()
                + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
                + (w as usize * h as usize * 4)) as i32
        }
        serialize_pod(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
            id: pw::spa::param::ParamType::Meta.as_raw(),
            properties: vec![
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_PARAM_META_type,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_Cursor)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_PARAM_META_size,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                        pw::spa::utils::Choice(
                            pw::spa::utils::ChoiceFlags::empty(),
                            // The max must cover the producer's offer or the Meta param silently
                            // fails to negotiate and NO buffer ever carries the meta region:
                            // Mutter offers a FIXED `SPA_POD_Int(CURSOR_META_SIZE(384, 384))`
                            // (meta-screen-cast-stream-src.c, GNOME 50) — a 256² max made the
                            // intersection empty, which cost the whole Linux cursor channel
                            // on-glass. 1024² is headroom, not an allocation: the negotiated
                            // region follows the producer's value.
                            pw::spa::utils::ChoiceEnum::Range {
                                default: meta_size(64, 64),
                                min: meta_size(1, 1),
                                max: meta_size(1024, 1024),
                            },
                        ),
                    )),
                },
            ],
        })
    }

    /// Extract straight (R,G,B,A) from one 4-byte cursor-bitmap pixel, honoring the bitmap's SPA
    /// video format (portals emit RGBA or BGRA; ARGB/ABGR handled for completeness). Unknown
    /// 4-byte formats are read as RGBA.
    fn decode_bitmap_pixel(vfmt: u32, s: &[u8]) -> (u8, u8, u8, u8) {
        match vfmt {
            x if x == spa::sys::SPA_VIDEO_FORMAT_RGBA => (s[0], s[1], s[2], s[3]),
            x if x == spa::sys::SPA_VIDEO_FORMAT_BGRA => (s[2], s[1], s[0], s[3]),
            x if x == spa::sys::SPA_VIDEO_FORMAT_ARGB => (s[1], s[2], s[3], s[0]),
            x if x == spa::sys::SPA_VIDEO_FORMAT_ABGR => (s[3], s[2], s[1], s[0]),
            _ => (s[0], s[1], s[2], s[3]),
        }
    }

    /// Update `cursor` from the newest buffer's `SPA_META_Cursor` (no-op when the buffer carries no
    /// cursor meta — producer doesn't support it, or the portal isn't in Metadata cursor mode).
    /// Called for EVERY dequeued buffer, before the stale-frame skip, so pointer-only movements
    /// (which Mutter delivers as metadata-only "corrupted" buffers) still refresh the position.
    fn update_cursor_meta(cursor: &mut CursorState, spa_buf: *mut spa::sys::spa_buffer) {
        // SAFETY: `spa_buf` is the live buffer we still hold (dequeued, not yet requeued).
        // `spa_buffer_find_meta` returns the `spa_meta` (type + byte `size` + `data` pointer) for
        // `SPA_META_Cursor`, or null. We take `find_meta` rather than `find_meta_data` specifically
        // to obtain the region's real `size`: the bitmap offset, pixel offset and stride read below
        // are ALL producer-written, and without a bound against the actual region they drive
        // out-of-bounds pointer arithmetic and an oversized `slice::from_raw_parts` — an OOB read
        // that SIGSEGVs inside the PipeWire `.process` callback (a segfault `catch_unwind` cannot
        // catch). Every offset below is validated against `region_size` with checked arithmetic,
        // mirroring the fd-length guard the main frame path already applies to xdg-desktop-portal-wlr.
        // TEMP M2b on-glass probe (DROP BEFORE MERGE): the journal must show whether the
        // compositor delivers SPA_META_Cursor at all, what it reports, and whether a bitmap is
        // ever accepted — the cursor pipeline is otherwise blind end-to-end. Statics are fine for
        // the single bring-up stream; every log is rate-limited or edge-triggered.
        use std::sync::atomic::{AtomicU64, Ordering as ProbeOrd};
        static PROBE_NULL: AtomicU64 = AtomicU64::new(0);
        static PROBE_META: AtomicU64 = AtomicU64::new(0);
        let meta = unsafe { spa::sys::spa_buffer_find_meta(spa_buf, spa::sys::SPA_META_Cursor) };
        if meta.is_null() {
            let n = PROBE_NULL.fetch_add(1, ProbeOrd::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(n, "cursor meta probe: buffer carries NO SPA_META_Cursor");
            }
            return;
        }
        // SAFETY: `meta` is non-null and points into the held buffer's metadata array.
        let (region_size, data) = unsafe { ((*meta).size as usize, (*meta).data as *const u8) };
        if data.is_null() || region_size < std::mem::size_of::<spa::sys::spa_meta_cursor>() {
            return;
        }
        let cur = data as *const spa::sys::spa_meta_cursor;
        // SAFETY: `region_size >= size_of::<spa_meta_cursor>()` checked above, so every field is in bounds.
        let (id, pos_x, pos_y, hot_x, hot_y, bmp_off) = unsafe {
            (
                (*cur).id,
                (*cur).position.x,
                (*cur).position.y,
                (*cur).hotspot.x,
                (*cur).hotspot.y,
                (*cur).bitmap_offset,
            )
        };
        // TEMP M2b probe (see above): what the meta reports, every 64th + the first few.
        let n = PROBE_META.fetch_add(1, ProbeOrd::Relaxed) + 1;
        if n <= 4 || n % 64 == 1 {
            tracing::info!(
                n,
                id,
                pos_x,
                pos_y,
                hot_x,
                hot_y,
                bmp_off,
                region_size,
                "cursor meta probe: SPA_META_Cursor arrived"
            );
        }
        if id == 0 {
            // SPA contract: id 0 = "no cursor information", NOT "cursor hidden". Mutter only
            // REWRITES a buffer's meta region when the cursor changed, so recycled buffers
            // between damage frames carry a stale id-0 meta — treating that as hidden flickered
            // the cursor off between hovers (on-glass round 5). Keep the last-known state; a
            // pointer that really left/hid simply stops producing updates. (The M3 hidden hint
            // loses its Mutter signal — Windows has its own CURSOR_SUPPRESSED source.)
            return;
        }
        cursor.visible = true;
        cursor.x = pos_x - hot_x;
        cursor.y = pos_y - hot_y;
        cursor.hot_x = hot_x;
        cursor.hot_y = hot_y;
        if bmp_off == 0 {
            // Position-only update — keep the cached bitmap.
            return;
        }
        let bmp_off = bmp_off as usize;
        // The `spa_meta_bitmap` header must fit entirely inside the region before we read it —
        // `bitmap_offset` is producer-controlled and otherwise reads past the metadata.
        match bmp_off.checked_add(std::mem::size_of::<spa::sys::spa_meta_bitmap>()) {
            Some(end) if end <= region_size => {}
            _ => return,
        }
        // SAFETY: `bmp_off + size_of::<spa_meta_bitmap>() <= region_size` (checked directly above),
        // so the header is fully in bounds; the producer places it aligned as before.
        let bmp = unsafe { data.add(bmp_off) as *const spa::sys::spa_meta_bitmap };
        // SAFETY: `bmp` is the in-bounds `spa_meta_bitmap` header validated just above; reading its
        // scalar fields is sound.
        let (vfmt, bw, bh, stride, pix_off) = unsafe {
            (
                (*bmp).format,
                (*bmp).size.width,
                (*bmp).size.height,
                (*bmp).stride.max(0) as usize,
                (*bmp).offset as usize,
            )
        };
        // Ignore empty or implausibly large bitmaps (the meta-size request covers <= 1024×1024;
        // real cursors are ≤96px — the cursor channel downscales >120px for the wire anyway).
        if bw == 0 || bh == 0 || bw > 1024 || bh > 1024 {
            return;
        }
        let row = bw as usize * 4;
        let stride = if stride < row { row } else { stride };
        // `span` is the exact byte extent the strided loop reads: `stride·(bh-1) + row`. Compute it
        // with checked arithmetic (a producer stride near `i32::MAX` would otherwise overflow) and
        // require the whole pixel block `[bmp_off + pix_off, +span)` to lie inside the region before
        // fabricating the slice — this is the check whose absence made the read go out of bounds.
        let span = match stride
            .checked_mul(bh as usize - 1)
            .and_then(|v| v.checked_add(row))
        {
            Some(s) => s,
            None => return,
        };
        match bmp_off
            .checked_add(pix_off)
            .and_then(|v| v.checked_add(span))
        {
            Some(end) if end <= region_size => {}
            _ => return,
        }
        // SAFETY: `bmp_off + pix_off + span <= region_size` (checked directly above), so the slice
        // is fully within the producer's meta region; `span` is exactly the strided loop's extent.
        let src = unsafe { std::slice::from_raw_parts(data.add(bmp_off + pix_off), span) };
        let mut rgba = vec![0u8; bw as usize * bh as usize * 4];
        for y in 0..bh as usize {
            for x in 0..bw as usize {
                let so = y * stride + x * 4;
                let (r, g, b, a) = decode_bitmap_pixel(vfmt, &src[so..so + 4]);
                let d = (y * bw as usize + x) * 4;
                rgba[d] = r;
                rgba[d + 1] = g;
                rgba[d + 2] = b;
                rgba[d + 3] = a;
            }
        }
        cursor.rgba = Arc::new(rgba);
        cursor.bw = bw;
        cursor.bh = bh;
        cursor.serial = cursor.serial.wrapping_add(1);
        // TEMP M2b probe (see above): bitmap accepted — once per shape change by construction.
        tracing::info!(
            bw,
            bh,
            vfmt,
            serial = cursor.serial,
            "cursor meta probe: bitmap cached"
        );
    }

    /// Destination channel byte offsets (R,G,B) and bytes-per-pixel for a packed-RGB `PixelFormat`,
    /// or `None` for a layout the CPU cursor blit doesn't handle (YUV/10-bit — those never reach
    /// the CPU de-pad path anyway).
    fn dst_offsets(fmt: PixelFormat) -> Option<(usize, usize, usize, usize)> {
        Some(match fmt {
            PixelFormat::Bgrx | PixelFormat::Bgra => (2, 1, 0, 4),
            PixelFormat::Rgbx | PixelFormat::Rgba => (0, 1, 2, 4),
            PixelFormat::Rgb => (0, 1, 2, 3),
            PixelFormat::Bgr => (2, 1, 0, 3),
            _ => return None,
        })
    }

    /// Alpha-blend the cached cursor bitmap into a packed 10-bit (`X2Rgb10`/`X2Bgr10`) CPU frame:
    /// unpack each u32, blend the 8-bit cursor channels scaled to 10 bits (`v<<2 | v>>6`), repack.
    /// The frame samples are PQ-encoded, so like the 8-bit gamma-space blend this is a display-
    /// referred approximation — fine for a cursor. `r_shift` is the R channel's bit offset (20 for
    /// x:R:G:B, 0 for x:B:G:R); G is always at 10 and B mirrors R.
    fn composite_cursor_rgb10(
        tight: &mut [u8],
        w: usize,
        h: usize,
        r_shift: u32,
        cursor: &CursorState,
    ) {
        let b_shift = 20 - r_shift; // 0 or 20 — the opposite end from R
        let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
        for cy in 0..bh {
            let dy = cursor.y + cy;
            if dy < 0 || dy as usize >= h {
                continue;
            }
            for cx in 0..bw {
                let dx = cursor.x + cx;
                if dx < 0 || dx as usize >= w {
                    continue;
                }
                let s = ((cy * bw + cx) as usize) * 4;
                let a = cursor.rgba[s + 3] as u32;
                if a == 0 {
                    continue;
                }
                // 8-bit cursor channel → 10-bit (replicate the top bits into the bottom).
                let up10 = |v: u8| ((v as u32) << 2) | ((v as u32) >> 6);
                let (sr, sg, sb) = (
                    up10(cursor.rgba[s]),
                    up10(cursor.rgba[s + 1]),
                    up10(cursor.rgba[s + 2]),
                );
                let di = (dy as usize * w + dx as usize) * 4;
                let px = u32::from_le_bytes(tight[di..di + 4].try_into().unwrap());
                let blend = |dst: u32, src: u32| (src * a + dst * (255 - a)) / 255;
                let dr = blend((px >> r_shift) & 0x3ff, sr);
                let dg = blend((px >> 10) & 0x3ff, sg);
                let db = blend((px >> b_shift) & 0x3ff, sb);
                let out = (px & 0xc000_0000) | (dr << r_shift) | (dg << 10) | (db << b_shift);
                tight[di..di + 4].copy_from_slice(&out.to_le_bytes());
            }
        }
    }

    /// Alpha-blend the cached cursor bitmap into the tightly-packed CPU frame at its latched
    /// position. Cheap: a straight-alpha blit over at most ~256×256 pixels, clipped to the frame —
    /// the whole point of cursor-as-metadata (no forced full-frame composite on the producer).
    fn composite_cursor(
        tight: &mut [u8],
        w: usize,
        h: usize,
        fmt: PixelFormat,
        cursor: &CursorState,
    ) {
        if !cursor.visible || cursor.rgba.is_empty() {
            return;
        }
        // The packed 10-bit HDR layouts blend via bit unpack/repack, not byte offsets.
        match fmt {
            PixelFormat::X2Rgb10 => return composite_cursor_rgb10(tight, w, h, 20, cursor),
            PixelFormat::X2Bgr10 => return composite_cursor_rgb10(tight, w, h, 0, cursor),
            _ => {}
        }
        let Some((ri, gi, bi, bpp)) = dst_offsets(fmt) else {
            return;
        };
        let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
        for cy in 0..bh {
            let dy = cursor.y + cy;
            if dy < 0 || dy as usize >= h {
                continue;
            }
            for cx in 0..bw {
                let dx = cursor.x + cx;
                if dx < 0 || dx as usize >= w {
                    continue;
                }
                let s = ((cy * bw + cx) as usize) * 4;
                let a = cursor.rgba[s + 3] as u32;
                if a == 0 {
                    continue;
                }
                let (sr, sg, sb) = (
                    cursor.rgba[s] as u32,
                    cursor.rgba[s + 1] as u32,
                    cursor.rgba[s + 2] as u32,
                );
                let di = (dy as usize * w + dx as usize) * bpp;
                let blend = |dst: u8, src: u32| ((src * a + dst as u32 * (255 - a)) / 255) as u8;
                tight[di + ri] = blend(tight[di + ri], sr);
                tight[di + gi] = blend(tight[di + gi], sg);
                tight[di + bi] = blend(tight[di + bi], sb);
            }
        }
    }

    /// De-pad / import a single PipeWire buffer and push it to the encoder. Called from the
    /// `.process` callback with the NEWEST drained buffer (latest-frame-only). `datas` is sourced
    /// via the same transparent cast libspa's `Buffer::datas_mut` performs, so the safe `Data`
    /// accessors (`.type_()`, `.chunk()`, `.data()`, `.fd()`, `.as_raw()`) keep working.
    fn consume_frame(ud: &mut UserData, spa_buf: *mut spa::sys::spa_buffer) {
        // No active stream: release the buffer without the (expensive at 5K) de-pad.
        if !ud.active.load(Ordering::Relaxed) {
            return;
        }
        // Poisoned (GPU import lost): the capturer is already surfacing an error to the encode
        // loop; skip per-frame work until the rebuild tears this stream down.
        if ud.broken.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: `spa_buf` is the `*mut spa_buffer` of the PipeWire buffer we dequeued and still hold for
        // this `.process` callback (not requeued until after `consume_frame` returns), so it is live. The
        // block null-checks `spa_buf`, requires `n_datas != 0`, and null-checks the `datas` array pointer
        // before forming any slice. `(*spa_buf).datas` points to `n_datas` libspa `spa_data` structs, and
        // `pw::spa::buffer::Data` is `#[repr(transparent)]` over `spa_data` (the same cast
        // `Buffer::datas_mut` performs — see the function doc), so the pointer cast + length describe
        // exactly that array, in bounds. The PipeWire loop is single-threaded and owns the buffer here, so
        // this `&mut` slice is the only reference to it (no aliasing/data race).
        let datas: &mut [pw::spa::buffer::Data] = unsafe {
            if spa_buf.is_null() || (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
                &mut []
            } else {
                std::slice::from_raw_parts_mut(
                    (*spa_buf).datas as *mut pw::spa::buffer::Data,
                    (*spa_buf).n_datas as usize,
                )
            }
        };
        if datas.is_empty() {
            return;
        }
        let sz = ud.info.size();
        let (w, h) = (sz.width as usize, sz.height as usize);
        if w == 0 || h == 0 {
            return; // format not negotiated yet
        }

        // Implicit-fence wait: Mutter renders into the dmabuf and hands it over at
        // GPU-submit time; with no producer explicit sync (Mutter+NVIDIA can't) we snapshot
        // the buffer's implicit fence and wait the producer's render before sampling —
        // closing the stale/old-frame race on NVIDIA. No-op for shm buffers or drivers that
        // attach no fence. Covers both the GPU import and the CPU mmap read below.
        if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
            match pf_zerocopy::dmabuf_fence::wait_read_ready(datas[0].fd(), 100) {
                Ok(waited) => {
                    static F1: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if F1.swap(false, Ordering::Relaxed) {
                        tracing::info!(
                            waited,
                            "dmabuf implicit-fence sync active (waited=true → driver fences \
                             the render, race closed; false → no implicit fence, zero-copy \
                             may still show stale frames)"
                        );
                    }
                }
                Err(e) => {
                    static F2: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if F2.swap(false, Ordering::Relaxed) {
                        tracing::warn!(
                            error = %e,
                            "dmabuf EXPORT_SYNC_FILE failed — no implicit-fence sync; NVIDIA \
                             zero-copy may show stale frames (no producer explicit sync)"
                        );
                    }
                }
            }
        }

        // Raw DMA-BUF passthrough: packed RGB is imported for GPU CSC; producer-native NV12 can
        // be consumed by the Vulkan Video encoder without another color conversion.
        if ud.vaapi_passthrough {
            if let Some(fmt) = ud.format {
                if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
                    if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                        let chunk = datas[0].chunk();
                        let offset = chunk.offset();
                        let stride = chunk.stride().max(0) as u32;
                        // Native NV12 usually arrives as a two-plane SPA buffer over ONE buffer
                        // object; plane 1's chunk carries the REAL UV offset/stride (compositors
                        // may align the Y plane before UV). Pass it through instead of assuming
                        // contiguity. Each spa_data holds its own (dup'd) fd, so BO identity is
                        // by inode, not fd number; a genuinely two-BO frame cannot travel through
                        // the single-fd import — drop it with a diagnosis instead of streaming
                        // garbage chroma.
                        let plane1 =
                            if fmt == PixelFormat::Nv12 && datas.len() >= 2 && datas[1].fd() > 0 {
                                // SAFETY: zeroed `libc::stat` is a valid POD initializer; both fds are
                                // owned by the live PipeWire buffer for this callback, and `fstat`
                                // only writes the out-param structs, whose fields are read only after
                                // the `== 0` success checks.
                                let same_bo = unsafe {
                                    let mut s0: libc::stat = std::mem::zeroed();
                                    let mut s1: libc::stat = std::mem::zeroed();
                                    libc::fstat(datas[0].fd() as i32, &mut s0) == 0
                                        && libc::fstat(datas[1].fd() as i32, &mut s1) == 0
                                        && (s0.st_dev, s0.st_ino) == (s1.st_dev, s1.st_ino)
                                };
                                if !same_bo {
                                    warn_once(
                                        "NV12 planes live in different buffer objects — frames \
                                     dropped (single-fd import only)",
                                    );
                                    return;
                                }
                                let c1 = datas[1].chunk();
                                Some((c1.offset(), c1.stride().max(0) as u32))
                            } else {
                                None
                            };
                        // dup the fd so it survives the SPA buffer recycle — the encode thread
                        // imports it. Content stability across the brief import/encode window relies
                        // on the compositor's buffer-pool depth, like any zero-copy capture.
                        // SAFETY: `datas[0].fd()` is the dmabuf fd owned by the live PipeWire buffer (valid
                        // for this callback). `fcntl(fd, F_DUPFD_CLOEXEC, 0)` reads only the integer fd,
                        // touches no Rust memory, and returns a fresh independent CLOEXEC duplicate (or -1).
                        // The original stays owned by PipeWire; the dup is a new fd we own (checked >= 0).
                        let dup =
                            unsafe { libc::fcntl(datas[0].fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
                        if dup >= 0 {
                            let pts_ns = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_nanos() as u64)
                                .unwrap_or(0);
                            let _ = ud.tx.try_send(CapturedFrame {
                                width: w as u32,
                                height: h as u32,
                                pts_ns,
                                format: fmt,
                                payload: FramePayload::Dmabuf(DmabufFrame {
                                    // SAFETY: `dup` is the fresh fd `fcntl(F_DUPFD_CLOEXEC)` just returned
                                    // (checked `dup >= 0`); nothing else owns it, so `OwnedFd` takes sole
                                    // ownership and closes it exactly once on drop — no alias, no
                                    // double-close.
                                    fd: unsafe { OwnedFd::from_raw_fd(dup) },
                                    fourcc,
                                    modifier: ud.modifier,
                                    offset,
                                    stride,
                                    plane1,
                                }),
                                // Cursor-as-metadata is blended only by RGB→NV12 backends. Gamescope
                                // embeds its pointer in the produced pixels, so native NV12 has none.
                                cursor: ud.cursor.overlay(),
                            });
                            static ONCE: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(true);
                            if ONCE.swap(false, Ordering::Relaxed) {
                                tracing::info!(
                                    w,
                                    h,
                                    modifier = ud.modifier,
                                    fourcc = format_args!("{:#010x}", fourcc),
                                    source = if fmt == PixelFormat::Nv12 {
                                        "producer-native NV12"
                                    } else {
                                        "packed RGB (encoder GPU CSC)"
                                    },
                                    "zero-copy: handing the raw DMA-BUF to the encoder"
                                );
                            }
                            return;
                        }
                    }
                }
            }
            // Not a dmabuf (or unmappable format) — fall through to the CPU de-pad path.
        }

        // Zero-copy path: if the buffer is a dmabuf and we have an importer, import it
        // into a CUDA device buffer (no CPU touch) and deliver that. Otherwise fall
        // through to the shm de-pad copy below.
        let mut gpu_import_broken = false;
        if let (Some(importer), Some(fmt)) = (ud.importer.as_mut(), ud.format) {
            // Defense-in-depth: the 10-bit PQ formats must never enter the EGL→CUDA import (its
            // de-tile blit is 8-bit RGBA8 — silent depth loss). An HDR offer never builds the
            // importer, so this gate only matters if those invariants ever drift apart.
            if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf && !fmt.is_hdr_rgb10() {
                let plane = pf_zerocopy::DmabufPlane {
                    fd: datas[0].fd(),
                    offset: datas[0].chunk().offset(),
                    stride: datas[0].chunk().stride().max(0) as u32,
                };
                // Tiled modifier → EGL/GL de-tile import; LINEAR (0/unset, e.g.
                // gamescope) → direct CUDA external-memory import (NVIDIA EGL can't
                // sample LINEAR).
                let modifier = (ud.modifier != 0).then_some(ud.modifier);
                if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                    // GPU converts: a 4:4:4 session gets the planar-YUV444 convert on the tiled
                    // EGL/GL path (full chroma, takes precedence over NV12 — 4:4:4 must never
                    // subsample), otherwise `PUNKTFUNK_NV12` gets NV12 — tiled via the EGL/GL
                    // blit, LINEAR/gamescope via the Vulkan bridge's compute CSC (latency plan
                    // T2.5b) — so NVENC encodes native YUV and skips its internal RGB→YUV CSC on
                    // the contended SM. A 4:4:4 session on LINEAR frames has no convert and
                    // stays RGB, falling to the encoder's clear-error path (`want_444` with an
                    // RGB CUDA payload) rather than silently subsampling. A LINEAR NV12 convert
                    // failure latches RGB for the stream (mid-frame fallback, no drop).
                    let yuv444 = ud.yuv444 && modifier.is_some();
                    let mut nv12 = ud.nv12 && !ud.yuv444;
                    let imported = if let Some(m) = modifier {
                        if yuv444 {
                            importer.import_yuv444(&plane, w as u32, h as u32, fourcc, Some(m))
                        } else if nv12 {
                            importer.import_nv12(&plane, w as u32, h as u32, fourcc, Some(m))
                        } else {
                            importer.import(&plane, w as u32, h as u32, fourcc, Some(m))
                        }
                    } else if nv12 && !ud.linear_nv12_failed {
                        match importer.import_linear_nv12(&plane, w as u32, h as u32) {
                            Ok(buf) => Ok(buf),
                            Err(e) => {
                                ud.linear_nv12_failed = true;
                                nv12 = false;
                                tracing::warn!(error = %format!("{e:#}"),
                                    "LINEAR NV12 compute CSC failed — RGB for the rest of this \
                                     stream (NVENC does the CSC internally)");
                                importer.import_linear(&plane, w as u32, h as u32)
                            }
                        }
                    } else {
                        nv12 = false;
                        importer.import_linear(&plane, w as u32, h as u32)
                    };
                    match imported {
                        Ok(devbuf) => {
                            ud.import_fail_streak = 0;
                            pf_zerocopy::note_gpu_import_ok();
                            static ONCE: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(true);
                            if ONCE.swap(false, Ordering::Relaxed) {
                                tracing::info!(
                                    w,
                                    h,
                                    modifier = ud.modifier,
                                    nv12,
                                    yuv444,
                                    "zero-copy: dmabuf imported to CUDA (no CPU copy)"
                                );
                            }
                            let pts_ns = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_nanos() as u64)
                                .unwrap_or(0);
                            let _ = ud.tx.try_send(CapturedFrame {
                                width: w as u32,
                                height: h as u32,
                                pts_ns,
                                format: if yuv444 {
                                    PixelFormat::Yuv444
                                } else if nv12 {
                                    PixelFormat::Nv12
                                } else {
                                    fmt
                                },
                                payload: FramePayload::Cuda(devbuf),
                                // Cursor-as-metadata: blended by the CUDA encoder into its owned
                                // device surface. (RGB LINEAR-import case; YUV sessions blend planes.)
                                cursor: ud.cursor.overlay(),
                            });
                            return;
                        }
                        Err(e) => {
                            let dead = importer.dead();
                            if dead {
                                pf_zerocopy::note_gpu_import_death();
                            }
                            if modifier.is_some() {
                                // Tiled buffer: the CPU fallback below would mmap TILED bytes
                                // and de-pad them as linear — a scrambled image, worse than no
                                // frame. Drop the frame instead; on a dead worker (it absorbed a
                                // driver fault) or a short failure streak, poison the stream so
                                // the session's capture-loss rebuild renegotiates cleanly.
                                ud.import_fail_streak += 1;
                                if dead || ud.import_fail_streak >= IMPORT_FAIL_POISON {
                                    tracing::error!(error = %format!("{e:#}"), dead,
                                        "tiled GPU import lost — failing this capture for rebuild");
                                    ud.broken.store(true, Ordering::Relaxed);
                                } else {
                                    tracing::warn!(error = %format!("{e:#}"),
                                        streak = ud.import_fail_streak,
                                        "tiled dmabuf GPU import failed — frame dropped");
                                }
                                return;
                            }
                            // LINEAR dmabuf: CPU-mappable, so disable the importer and fall
                            // through to the CPU mmap path — degraded, not dead.
                            tracing::warn!(error = %format!("{e:#}"),
                                "LINEAR dmabuf GPU import failed — falling back to the CPU copy path");
                            gpu_import_broken = true;
                        }
                    }
                } else {
                    return; // format has no DRM fourcc mapping — skip the frame
                }
            }
        }
        if gpu_import_broken {
            ud.importer = None;
        }

        let d = &mut datas[0];
        // CPU path may also receive LINEAR dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins); capture the fd before `data()` borrows `d`.
        let data_type = d.type_();
        // fd-backed buffer (MemFd SHM, or DmaBuf)? Capture the fd before `data()` borrows `d`.
        let raw_fd = d.fd();
        let (size, offset, stride) = {
            let c = d.chunk();
            (
                c.size() as usize,
                c.offset() as usize,
                c.stride().max(0) as usize,
            )
        };
        let Some(fmt) = ud.format else { return }; // unsupported/not negotiated
        let bpp = fmt.bytes_per_pixel();
        let row = w * bpp;
        let stride = if stride == 0 { row } else { stride };
        if stride < row {
            warn_once("chunk stride < row — frames dropped");
            return;
        }
        let needed = stride * (h - 1) + row;
        // dmabuf chunks commonly report size 0; fall back to the computed span.
        let size = if size == 0 { needed } else { size };
        // For fd-backed buffers (MemFd SHM, DmaBuf) mmap the fd OURSELVES, sized to the fd's real
        // length (fstat), rather than trusting PipeWire's MAP_BUFFERS slice: xdg-desktop-portal-wlr
        // hands MemFd buffers whose reported `data.maxsize` exceeds the bytes actually mapped into
        // our process, so reading to maxsize segfaults (it also covers the original case — MAP_BUFFERS
        // not mapping Vulkan dmabufs, e.g. gamescope). The `needed > avail` guard below then drops
        // cleanly if the real buffer is genuinely too small. MemPtr buffers (no fd) are same-process —
        // trust `d.data()`.
        let fd_len = if raw_fd > 0 {
            // SAFETY: `libc::stat` is a C plain-old-data struct for which all-zero is a valid value, so
            // `mem::zeroed()` is a sound initializer. `raw_fd` is the buffer's fd (`> 0` checked here) and
            // valid for this callback; `fstat` writes metadata into `&mut st`, a live, aligned,
            // correctly-sized stack `stat` that outlives the synchronous call. `st.st_size` is read only
            // after the return value is confirmed `== 0`. `st` is a fresh local, so nothing aliases it.
            unsafe {
                let mut st: libc::stat = std::mem::zeroed();
                (libc::fstat(raw_fd as i32, &mut st) == 0 && st.st_size > 0)
                    .then_some(st.st_size as usize)
            }
        } else {
            None
        };
        let _mapping; // keeps a manual mmap alive for the copy below
                      // Prefer our own fstat-sized mmap of the fd; fall back to PipeWire's MAP_BUFFERS slice
                      // (and finally drop) so an fd PipeWire could map but we can't never silently over-reads.
        let self_mapped: Option<&[u8]> = if raw_fd > 0 {
            let map_len = fd_len.unwrap_or(offset + needed);
            match DmabufMap::new(raw_fd as i32, map_len) {
                Some(m) => {
                    _mapping = m;
                    // SAFETY: `_mapping` is the `DmabufMap` just stored; its `ptr`/`len` come from a
                    // successful `mmap` of `map_len` PROT_READ bytes, so `ptr` is non-null, page-aligned,
                    // and the VMA is one allocated object of `len` bytes valid for reads. In the common
                    // path `map_len == fd_len` (the fd's real size from `fstat`), so the mapping spans the
                    // whole object; the de-pad copy below is further bounded by the `offset <= buf.len()`
                    // and `needed > avail` guards. The `&[u8]` borrows `_mapping`, which lives to the end
                    // of `consume_frame`, so the slice never outlives the mapping, and the memory is only
                    // read here, so there is no aliasing/mutation.
                    Some(unsafe {
                        std::slice::from_raw_parts(_mapping.ptr as *const u8, _mapping.len)
                    })
                }
                None => None,
            }
        } else {
            None
        };
        let buf: &[u8] = if let Some(b) = self_mapped {
            b
        } else if let Some(data) = d.data() {
            data
        } else {
            warn_once("buffer has no mappable data — frames dropped");
            return;
        };
        // Need stride*(h-1)+row valid bytes within [offset, offset+size).
        if offset > buf.len() {
            return;
        }
        let avail = buf.len() - offset;
        {
            // One-time geometry dump — makes a new compositor/GPU's buffer layout visible in the
            // logs (the kind of mismatch that crashed xdpw MemFd capture before the self-mmap fix).
            use std::sync::atomic::{AtomicBool, Ordering};
            static ONCE: AtomicBool = AtomicBool::new(true);
            if ONCE.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    stride, size, offset, buf_len = buf.len(), needed,
                    data_type = ?data_type, fd_len = ?fd_len, self_mapped = self_mapped.is_some(),
                    "capture CPU de-pad geometry (first frame)"
                );
            }
        }
        if needed > avail || needed > size {
            warn_once("buffer smaller than frame span — frames dropped");
            return;
        }
        let region = &buf[offset..offset + size.min(avail)];
        // De-pad into a tightly-packed buffer (chunk stride may exceed w*bpp).
        let mut tight = vec![0u8; row * h];
        for y in 0..h {
            tight[y * row..y * row + row].copy_from_slice(&region[y * stride..y * stride + row]);
        }
        // Cursor-as-metadata: blit the latched pointer into the frame (no-op when hidden or when
        // the layout isn't packed RGB). This is the CPU path's counterpart to the producer's
        // hardware cursor plane, which stays out of the captured buffer.
        composite_cursor(&mut tight, w, h, fmt, &ud.cursor);
        let pts_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let frame = CapturedFrame {
            width: w as u32,
            height: h as u32,
            pts_ns,
            format: fmt,
            payload: FramePayload::Cpu(tight),
            // Already composited inline into `tight` above — nothing for the encoder to blend.
            cursor: None,
        };
        // Drop if the encoder is behind — never block the pipewire loop.
        let _ = ud.tx.try_send(frame);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pipewire_thread(
        fd: Option<OwnedFd>,
        node_id: u32,
        tx: SyncSender<CapturedFrame>,
        active: Arc<AtomicBool>,
        negotiated: Arc<AtomicBool>,
        streaming: Arc<AtomicBool>,
        broken: Arc<AtomicBool>,
        hdr_negotiated: Arc<AtomicBool>,
        zerocopy: bool,
        // 4:4:4 session: tiled dmabufs take the worker's planar-YUV444 GPU convert.
        want_444: bool,
        // HDR session: offer ONLY the 10-bit PQ/BT.2020 formats as LINEAR dmabufs (see
        // `build_hdr_dmabuf_format`); the SDR offers are not built at all.
        want_hdr: bool,
        preferred: Option<(u32, u32, u32)>,
        quit_rx: pw::channel::Receiver<()>,
        // Encode-backend facts resolved by the facade (never re-derived here) — the one-way
        // capture→encode edge (plan §W6).
        policy: ZeroCopyPolicy,
    ) -> Result<()> {
        crate::pwinit::ensure_init();

        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
        // A quit signal (capturer `Drop`) lands here on the loop thread and stops `run()` so the
        // thread unwinds instead of blocking to process exit. Hold the attachment for the loop's
        // life; the cloned loop handle is the one the callback quits.
        let quit_loop = mainloop.clone();
        let _quit_attach = quit_rx.attach(mainloop.loop_(), move |()| {
            tracing::debug!("pipewire: quit signal received — stopping capture loop");
            quit_loop.quit();
        });
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
        // A portal source hands us an fd to a (sandboxed) PipeWire remote; the KWin
        // virtual-output source has no fd — its node lives on the user's default daemon.
        let core = match fd {
            Some(fd) => context
                .connect_fd_rc(fd, None)
                .context("pw connect_fd (portal remote)")?,
            None => context
                .connect_rc(None)
                .context("pw connect (default daemon)")?,
        };

        // Build the GPU importer up front — normally the ISOLATED worker process
        // (design/zerocopy-worker-isolation.md), so a driver fault on a dying compositor's
        // dmabuf kills the worker, not this host. If it fails, log and fall back to the CPU path
        // (we simply won't request dmabuf below). Skipped entirely when the frames go to the
        // raw-dmabuf passthrough — the encode backend is VAAPI, or the SESSION encodes PyroWave
        // (its Vulkan device imports raw dmabufs on any vendor): building the importer there
        // would waste a CUDA probe — or worse, succeed and produce CUDA payloads only NVENC can
        // consume. Also skipped once repeated worker deaths latched the import off (a wedged GPU
        // stack must not crash-loop).
        let backend_is_vaapi = policy.backend_is_vaapi;
        let raw_passthrough = backend_is_vaapi || policy.pyrowave_session;
        // HDR never builds the EGL→CUDA importer: its de-tile blit renders into 8-bit RGBA8,
        // which would silently crush the 10-bit depth. The HDR consumers are the CPU mmap path
        // (LINEAR de-pad → X2Rgb10 CPU frames) and the VAAPI raw-dmabuf passthrough.
        let mut importer = if zerocopy && !raw_passthrough && !want_hdr {
            if pf_zerocopy::gpu_import_disabled() {
                tracing::warn!(
                    "zero-copy GPU import disabled after repeated import-worker deaths — using CPU path"
                );
                None
            } else {
                match pf_zerocopy::Importer::new_for_capture() {
                    Ok(i) => Some(i),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                        None
                    }
                }
            }
        } else {
            None
        };
        // PUNKTFUNK_FORCE_SHM=1 forces the race-free download path (SHM, no dmabuf) — a manual
        // escape hatch, mainly for Mutter+NVIDIA: that combo has no implicit dmabuf fence, so
        // zero-copy capture can in principle race the compositor's render and show stale frames.
        // Zero-copy is the Mutter+NVIDIA default (no unconditional override) since live retesting
        // found no visible staleness; set this if you do see flashing/stale content on such a
        // host. KWin/gamescope don't need it (they blit into the buffer, so no read-before-render
        // race).
        let force_shm = std::env::var("PUNKTFUNK_FORCE_SHM").as_deref() == Ok("1");
        // Raw-dmabuf zero-copy passthrough: zero-copy on, no EGL→CUDA importer, and the frames'
        // consumer imports raw dmabufs itself — the VAAPI backend (libva import + GPU CSC) or a
        // PyroWave session (the wavelet encoder's own Vulkan device, any vendor) → hand the raw
        // dmabuf straight to the encoder.
        let vaapi_passthrough = zerocopy && !force_shm && importer.is_none() && raw_passthrough;
        // Producer-side NV12 (default-on; PUNKTFUNK_PIPEWIRE_NV12=0 escapes): gamescope offers a
        // one-fd LINEAR NV12 image when the consumer asks — its compositor pass does the RGB→YUV,
        // and the Vulkan Video encoder imports the buffer as its encode source directly (no host
        // CSC at all). `native_nv12_session` restricts this to sessions whose encoder can ingest
        // it (Linux vulkan-encode H265/AV1 — never H264/libav-VAAPI, GameStream-resolve, or
        // PyroWave, whose Vulkan compute CSC ingests packed RGB only). Raw passthrough is
        // required because the CUDA importer expects packed RGB, and 4:4:4/HDR must not be
        // silently subsampled/downconverted. Non-NV12 compositors (KWin/GNOME) simply match the
        // packed-RGB fallback pod.
        let prefer_native_nv12 = std::env::var("PUNKTFUNK_PIPEWIRE_NV12").as_deref() != Ok("0")
            && policy.native_nv12_session
            && backend_is_vaapi
            && vaapi_passthrough
            && !policy.pyrowave_session
            && !want_444
            && !want_hdr;
        if prefer_native_nv12 {
            tracing::info!(
                "zero-copy: preferring gamescope producer-side NV12 LINEAR DMA-BUF (no host \
                 RGB CSC; PUNKTFUNK_PIPEWIRE_NV12=0 restores the packed-RGB negotiation)"
            );
        }
        // Modifiers our import stack handles for BGRx: the EGL-importable (tiled) set, plus LINEAR
        // (0) — NVIDIA's EGL won't list it, but LINEAR dmabufs (gamescope's only offer) import via
        // CUDA external memory instead. For the VAAPI passthrough path we advertise LINEAR only:
        // radeonsi/iHD import it and any compositor can allocate it.
        let mut modifiers = importer
            .as_mut()
            .map(|i| i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgrx).unwrap()))
            .unwrap_or_default();
        if (importer.is_some() || vaapi_passthrough) && !modifiers.contains(&0) {
            modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
        }
        // PyroWave passthrough: the encoder imports through Vulkan, not libva — extend the
        // advertisement with every modifier its device samples from, so compositors that
        // never allocate LINEAR (Mutter+NVIDIA) still negotiate zero-copy dmabufs. The modifiers
        // were resolved by the facade (`ZeroCopyPolicy::pyrowave_modifiers`) — non-empty only when
        // the host's `pyrowave` feature is on AND the session (or the global encoder pref) is
        // PyroWave — so capture never calls back into `encode` and needs no feature gate of its
        // own (the emptiness check gates it).
        if vaapi_passthrough && !policy.pyrowave_modifiers.is_empty() {
            for &m in &policy.pyrowave_modifiers {
                if !modifiers.contains(&m) {
                    modifiers.push(m);
                }
            }
            tracing::info!(
                count = modifiers.len(),
                "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
            );
        }
        let want_dmabuf =
            (importer.is_some() || vaapi_passthrough) && !modifiers.is_empty() && !force_shm;
        if force_shm {
            tracing::info!(
                "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
            );
        } else if zerocopy && !want_dmabuf {
            tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
        } else if vaapi_passthrough && policy.pyrowave_modifiers.is_empty() {
            tracing::info!(
                native_nv12_preferred = prefer_native_nv12,
                "zero-copy: advertising LINEAR DMA-BUF for encoder import (native NV12 first \
                 when enabled, packed RGB fallback)"
            );
        } else if want_dmabuf && !vaapi_passthrough {
            tracing::info!(
                count = modifiers.len(),
                sample = ?&modifiers[..modifiers.len().min(6)],
                "zero-copy: advertising EGL-importable dmabuf modifiers"
            );
        } else if backend_is_vaapi && policy.backend_is_gpu {
            // A VAAPI session on the CPU path pays three full-frame CPU touches (mmap de-pad +
            // swscale RGB→NV12 + surface upload) — make the silent fallback visible.
            tracing::warn!(
                "VAAPI encode with the CPU capture path (per-frame de-pad + swscale CSC + \
                 upload) — zero-copy was disabled ({}); clear PUNKTFUNK_ZEROCOPY to restore \
                 the dmabuf default",
                if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                    "PUNKTFUNK_ZEROCOPY is set falsy"
                } else {
                    "downgraded after a failed dmabuf negotiation"
                }
            );
        }
        if want_dmabuf && !vaapi_passthrough && want_444 {
            tracing::info!(
                "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
                 NVENC fed native full-chroma YUV, no CPU pixel path"
            );
        } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
            tracing::info!(
                "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709 limited) on the GPU — NVENC \
                 fed native YUV (no internal RGB→YUV CSC)"
            );
        }

        let data = UserData {
            info: VideoInfoRaw::default(),
            format: None,
            modifier: 0,
            tx,
            active,
            negotiated,
            streaming,
            broken,
            hdr_negotiated,
            import_fail_streak: 0,
            importer,
            vaapi_passthrough,
            nv12: pf_zerocopy::nv12_enabled(),
            yuv444: want_444,
            linear_nv12_failed: false,
            dbg_log_n: 0,
            cursor: CursorState::default(),
        };

        let stream = pw::stream::StreamBox::new(
            &core,
            "punktfunk-screencast",
            properties! {
                *pw::keys::MEDIA_TYPE     => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE     => "Screen",
                // Never let the session manager re-target this stream to a different node when
                // its target goes away: an orphaned stream auto-linked to a fresh Video/Source
                // wedges that node — and a stuck link head-blocks the PipeWire daemon's shared
                // work queue, stalling ALL new link negotiation system-wide.
                "node.dont-reconnect"     => "true",
            },
        )
        .context("pw Stream")?;

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_stream, ud, old, new| {
                tracing::info!(?old, ?new, "pipewire stream state");
                // Track whether the node is actively producing. A live source sits in `Streaming`
                // (a static desktop just sends no buffers); anything else — `Paused`/`Unconnected`/
                // `Error` — means the source went away (compositor died, virtual output removed on a
                // Gaming↔Desktop switch). `try_latest` turns a sustained non-Streaming state into a
                // capture-loss so the encode loop rebuilds instead of freezing on the last frame.
                ud.streaming.store(
                    matches!(new, pw::stream::StreamState::Streaming),
                    Ordering::Relaxed,
                );
            })
            .param_changed(|_stream, ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) =
                    pw::spa::param::format_utils::parse_format(param)
                else {
                    return;
                };
                if media_type != pw::spa::param::format::MediaType::Video
                    || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if ud.info.parse(param).is_ok() {
                    ud.negotiated.store(true, Ordering::Relaxed);
                    // A (re)negotiation replaces the buffer pool: every cached per-buffer import
                    // (stored fds in the worker, the Vulkan bridge's per-fd sources) keys on
                    // buffers that no longer exist — and a recycled fd number/inode must never
                    // resolve to a stale import. No-op on the first negotiation (empty caches).
                    if let Some(imp) = ud.importer.as_mut() {
                        imp.clear_cache();
                    }
                    let sz = ud.info.size();
                    ud.format = map_format(ud.info.format());
                    ud.modifier = ud.info.modifier();
                    // HDR: the 10-bit PQ formats are only ever offered with MANDATORY BT.2020/PQ
                    // colorimetry props, so a 10-bit negotiation IS an HDR negotiation — but log
                    // what the producer actually fixated for diagnosis.
                    let hdr = ud.format.is_some_and(|f| f.is_hdr_rgb10());
                    ud.hdr_negotiated.store(hdr, Ordering::Relaxed);
                    tracing::info!(
                        width = sz.width,
                        height = sz.height,
                        spa_format = ?ud.info.format(),
                        mapped = ?ud.format,
                        modifier = ud.modifier,
                        hdr,
                        transfer_function = ud.info.transfer_function(),
                        color_primaries = ud.info.color_primaries(),
                        "pipewire format negotiated"
                    );
                    if ud.format.is_none() {
                        tracing::error!(
                            spa_format = ?ud.info.format(),
                            "negotiated a pixel format the encoder cannot consume — frames will be skipped"
                        );
                    }
                }
            })
            .process(|stream, ud| {
                // Latest-frame-only (OBS pattern): Mutter delivers buffers in bursts and recycles its
                // pool; an older queued buffer carries a STALE frame. Drain all queued buffers, requeue
                // the older ones, keep only the newest. This dequeue/requeue runs OUTSIDE the
                // `catch_unwind` below — they are non-panicking C FFI pointer ops, and `newest` is
                // requeued exactly once AFTER the panic-containing region. Previously the whole thing was
                // inside the catch, so a caught panic (in `update_cursor_meta`/`consume_frame`) stranded
                // `newest` forever, permanently shrinking the stream's fixed pool until capture wedged.
                // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on the
                // loop thread; `dequeue_raw_buffer` returns a stream-owned `*mut pw_buffer` or null
                // (null-checked), single-threaded so no concurrent access.
                let mut newest = unsafe { stream.dequeue_raw_buffer() };
                if newest.is_null() {
                    return;
                }
                let mut drained = 1u32;
                loop {
                    // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
                    let next = unsafe { stream.dequeue_raw_buffer() };
                    if next.is_null() {
                        break;
                    }
                    // SAFETY: `newest` was dequeued from this stream and not yet requeued; we immediately
                    // overwrite it, so the requeued pointer is never touched again.
                    unsafe { stream.queue_raw_buffer(newest) };
                    newest = next;
                    drained += 1;
                }
                // PipeWire dispatches from a C trampoline with no catch_unwind; a panic crossing that FFI
                // boundary would abort the whole host. Contain the inspect/consume work — the only Rust
                // code here that can panic — and requeue `newest` unconditionally after it.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                    // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                    // load through a valid pointer — no mutation or aliasing.
                    let spa_buf = unsafe { (*newest).buffer };

                    // Refresh cursor-as-metadata BEFORE the stale-frame skip below: Mutter delivers
                    // pointer-only movements as metadata-only "corrupted" buffers we drop for their
                    // frame, but their cursor meta is fresh and must still move our overlay.
                    update_cursor_meta(&mut ud.cursor, spa_buf);

                    // Inspect the newest buffer's header + first chunk for the diagnostic and the
                    // CORRUPTED skip. SPA_META_Header is optional — `hdr` may be null.
                    // SAFETY: `spa_buf` is the `*mut spa_buffer` of the buffer we still hold.
                    // `spa_buffer_find_meta_data` scans that buffer's metadata array for a `SPA_META_Header`
                    // of at least `size_of::<spa_meta_header>()` bytes and returns a pointer into the held
                    // buffer's metadata (or null). The size argument matches the struct the result is cast
                    // to, and the pointer stays valid as long as the buffer is held (until requeue). Null is
                    // handled below.
                    let hdr = unsafe {
                        spa::sys::spa_buffer_find_meta_data(
                            spa_buf,
                            spa::sys::SPA_META_Header,
                            std::mem::size_of::<spa::sys::spa_meta_header>(),
                        ) as *const spa::sys::spa_meta_header
                    };
                    let hdr_flags = if hdr.is_null() {
                        0u32
                    } else {
                        // SAFETY: reached only when `hdr` is non-null; it points to a `spa_meta_header`
                        // inside the live buffer's metadata (returned for a size >=
                        // `size_of::<spa_meta_header>()`, so `.flags` is in bounds). A single field read
                        // while the buffer is still held.
                        unsafe { (*hdr).flags }
                    };
                    // First data chunk's size + flags (used for the diagnostic + CORRUPTED check)
                    // and its data type (a dmabuf legitimately reports chunk size 0, so the size-0
                    // stale skip only applies to mappable SHM buffers).
                    // SAFETY: every dereference is guarded in order before any field read — `spa_buf`
                    // non-null, `n_datas > 0`, the `datas` (`*mut spa_data`) array non-null, and the first
                    // element's `chunk` (`*mut spa_chunk`) non-null. `d0` is that first `spa_data` and `c`
                    // its chunk; reading `(*d0).type_`, `(*c).size`, `(*c).flags` are in-bounds field loads
                    // of libspa structs inside the buffer we still hold. Single-threaded loop, no mutation.
                    let (chunk_size, chunk_flags, is_dmabuf) = unsafe {
                        if !spa_buf.is_null()
                            && (*spa_buf).n_datas > 0
                            && !(*spa_buf).datas.is_null()
                            && !(*(*spa_buf).datas).chunk.is_null()
                        {
                            let d0 = (*spa_buf).datas;
                            let c = (*d0).chunk;
                            let is_dmabuf =
                                (*d0).type_ == spa::sys::SPA_DATA_DmaBuf;
                            ((*c).size, (*c).flags, is_dmabuf)
                        } else {
                            (0u32, 0i32, false)
                        }
                    };

                    let corrupted = (hdr_flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED) != 0
                        || (chunk_flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32) != 0;

                    // THE GNOME FLASH FIX: skip Mutter's CORRUPTED / size-0 cursor-update buffers.
                    // When the pointer moves (e.g. dragging a window) Mutter sends metadata-only
                    // buffers flagged CORRUPTED (chunk size 0) that still reference a RECYCLED old
                    // frame; consuming them encodes "the window at its old position" — the flash.
                    // Confirmed live on worker-3 (chunk_flags=CORRUPTED, size 0) for both the zero-copy
                    // and SHM paths. The size-0 half is SHM-only (a real dmabuf legitimately reports
                    // chunk size 0). `drained` is the latest-frame-only depth — a cheap extra defense
                    // against bursty delivery, though here Mutter sends one buffer per callback.
                    if corrupted || (chunk_size == 0 && !is_dmabuf) {
                        ud.dbg_log_n += 1;
                        if ud.dbg_log_n.is_power_of_two() {
                            tracing::debug!(
                                skipped = ud.dbg_log_n,
                                drained,
                                "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                            );
                        }
                        // Skip this stale/cursor buffer — `newest` is requeued unconditionally below.
                        return;
                    }

                    consume_frame(ud, spa_buf);
                }));
                // Hand `newest` back to the stream exactly once, on EVERY path — normal, corrupted-skip,
                // or a caught panic in the closure above. This single requeue is what keeps the fixed
                // buffer pool from draining.
                // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame) completed
                // inside the closure above; `newest` was dequeued from this stream and not yet requeued.
                unsafe { stream.queue_raw_buffer(newest) };
                if outcome.is_err() {
                    // In the per-frame `.process` callback: a deterministic panic (e.g. a bad
                    // format) would fire this every frame, so power-of-two throttle it — enough to
                    // surface the fault without evicting the whole log ring.
                    static PANICS: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        tracing::error!(count = n, "panic in pipewire process callback — frame dropped");
                    }
                }
            })
            .register()
            .context("register stream listener")?;

        // Debug knob: offer a single fixed format (PUNKTFUNK_PW_FIXED_POD="WxH") to bisect
        // negotiation failures against a producer's exact EnumFormat (e.g. gamescope).
        let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
            .ok()
            .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
            .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));

        // Request raw video in any encoder-mappable layout, any size/framerate.
        let obj = if let Some((fw, fh)) = fixed_pod {
            tracing::info!(
                fw,
                fh,
                "pipewire: offering a fixed BGRx format pod (PUNKTFUNK_PW_FIXED_POD)"
            );
            pw::spa::pod::object!(
                pw::spa::utils::SpaTypes::ObjectParamFormat,
                pw::spa::param::ParamType::EnumFormat,
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaType,
                    Id,
                    pw::spa::param::format::MediaType::Video
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaSubtype,
                    Id,
                    pw::spa::param::format::MediaSubtype::Raw
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFormat,
                    Id,
                    VideoFormat::BGRx
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoSize,
                    Rectangle,
                    pw::spa::utils::Rectangle {
                        width: fw,
                        height: fh
                    }
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFramerate,
                    Fraction,
                    pw::spa::utils::Fraction { num: 0, denom: 1 }
                ),
            )
        } else {
            build_default_format_obj(preferred)
        };

        // When zero-copy is on, offer ONLY a BGRx dmabuf format with our EGL-importable modifiers
        // (offering shm too makes the compositor pick shm). The modifier list is advertised with
        // DONT_FIXATE so the compositor's allocator chooses one; we re-emit the fixated format in
        // `param_changed` (the two-step DMA-BUF handshake). Otherwise offer the multi-format shm
        // pod and let MAP_BUFFERS map it. An HDR session replaces ALL of this with the two 10-bit
        // PQ pods (LINEAR dmabuf, MANDATORY colorimetry — see `build_hdr_dmabuf_format`): offering
        // SDR alongside would make the producer pick its earlier-listed SDR format, and the
        // negotiation-timeout path latches the process-wide SDR downgrade if nothing matches.
        let format_pods: Vec<Vec<u8>> = if want_hdr {
            tracing::info!(
                "HDR capture: offering xRGB_210LE/xBGR_210LE LINEAR dmabufs with MANDATORY \
                 BT.2020 + SMPTE-2084 (PQ) colorimetry (GNOME 50+ monitor stream)"
            );
            vec![
                build_hdr_dmabuf_format(VideoFormat::xRGB_210LE, preferred)?,
                build_hdr_dmabuf_format(VideoFormat::xBGR_210LE, preferred)?,
            ]
        } else if want_dmabuf {
            let mut pods = Vec::with_capacity(if prefer_native_nv12 { 2 } else { 1 });
            if prefer_native_nv12 {
                // First compatible consumer pod wins. Gamescope advertises NV12 and BGRx; pinning
                // BT.709 limited here selects its RGB→NV12 shader with our bitstream colorimetry.
                pods.push(build_dmabuf_format(VideoFormat::NV12, &[0], preferred)?);
            }
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &modifiers,
                preferred,
            )?);
            pods
        } else {
            vec![serialize_pod(obj)?]
        };
        let buffers_values = if want_hdr || want_dmabuf {
            // Dmabuf-only. For HDR this is load-bearing beyond zero-copy: Mutter's SHM record
            // path paints 8-bit ARGB32 regardless of the negotiated format, so a MemFd buffer
            // under a 10-bit format would carry mislabeled bytes.
            Some(build_dmabuf_buffers()?)
        } else if force_shm {
            // True SHM: exclude DmaBuf so Mutter MUST download (glReadPixels orders against render).
            Some(build_shm_only_buffers()?)
        } else {
            // CPU path still accepts mappable dmabufs (gamescope offers only those once its
            // modifier-bearing format pod wins the intersection).
            Some(build_mappable_buffers()?)
        };

        // Ask for cursor-as-metadata on every path (harmless if the producer can't supply it): the
        // pointer rides as SPA_META_Cursor rather than being burned into the frame, so the
        // compositor keeps its cheap hardware cursor plane (see `choose_cursor_mode`).
        let cursor_meta = build_cursor_meta_param()?;
        let mut byte_slices: Vec<&[u8]> = Vec::new();
        for pod in &format_pods {
            byte_slices.push(pod);
        }
        if let Some(b) = &buffers_values {
            byte_slices.push(b);
        }
        byte_slices.push(&cursor_meta);
        let mut params: Vec<&Pod> = byte_slices
            .iter()
            .map(|&b| Pod::from_bytes(b).context("pod from bytes"))
            .collect::<Result<_>>()?;

        stream
            .connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("pw stream connect")?;

        // Blocks this thread, pumping frame callbacks until process exit.
        mainloop.run();
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        /// Pin our hand-written PQ transfer id against the real libspa binding. We can't take the
        /// constant from `pw::spa::sys` directly (older distro headers don't export it — see
        /// [`super::SPA_VIDEO_TRANSFER_SMPTE2084`]), so assert the two agree wherever the symbol
        /// DOES exist. Any libspa that renumbers the enum fails this instead of silently tagging
        /// the HDR offer with the wrong transfer function.
        ///
        /// Only builds where tests are compiled — the .deb/.rpm builders run plain `cargo build`,
        /// so this never reintroduces the compile failure it exists to prevent.
        #[test]
        fn pq_transfer_id_matches_libspa() {
            assert_eq!(
                super::SPA_VIDEO_TRANSFER_SMPTE2084,
                super::pw::spa::sys::SPA_VIDEO_TRANSFER_SMPTE2084,
                "libspa renumbered spa_video_transfer_function — update the hardcoded PQ id"
            );
        }
    }
}
