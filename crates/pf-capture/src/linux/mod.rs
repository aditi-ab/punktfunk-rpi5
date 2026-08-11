//! Live capture: xdg ScreenCast portal (`ashpd`) → PipeWire (`pipewire`), CPU-copy path.
//!
//! Two dedicated threads, because both stacks are tied to their thread:
//!   * **portal thread** drives the async ashpd handshake on a multi-thread tokio runtime
//!     (control plane — never the per-frame path), then parks on a oneshot receiver so the
//!     `proxy` + its zbus connection stay alive (the cast is torn down when that connection
//!     drops; ashpd's `Session` has no `Drop`);
//!   * **pipewire thread** owns the (`!Send`) MainLoop/Stream and pumps frames.
//!
//! The portal hands the PipeWire remote fd + node id to the pipewire thread; decoded BGRx
//! frames leave the pipewire thread over a bounded channel. The authoritative frame size
//! comes from the negotiated PipeWire format, not the portal's size hint.
//!
//! Cleanup: BOTH threads are stopped deterministically — [`PortalCapturer`]'s `Drop` sends a
//! pipewire `channel` quit and joins that thread (releasing its EGL importer / CUDA context
//! promptly instead of leaking it to process exit), and [`PortalSession`]'s `Drop` fires the
//! portal thread's quit oneshot and waits, bounded, for it to unwind — which drops the zbus
//! connection and so ENDS the compositor's ScreenCast session. Dropping a capturer (session end,
//! or a retried/failed pipeline build) therefore leaves nothing behind on either side.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{CapturedFrame, Capturer, DmabufFrame, FramePayload, PixelFormat, ZeroCopyPolicy};
use anyhow::{anyhow, Context, Result};

// gamescope cursor source (remote-desktop-sweep Phase C) — feeds `cursor_live` from XFixes when
// the PipeWire node carries no `SPA_META_Cursor` (gamescope's does not).
mod xfixes_cursor;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// The frame hand-off from the PipeWire thread to the consumer: a ONE-DEEP slot the producer
/// OVERWRITES, i.e. genuinely drop-**oldest**.
///
/// This was a `sync_channel(8)` fed by `try_send`, which is drop-**newest**: with the consumer
/// stalled the queue filled and every fresh frame was discarded, so `next_frame` then handed back
/// the OLDEST of eight stale frames (`try_latest` drained to the newest of them, which is no better
/// — it is still the frame from the moment the queue filled). Shrinking the bound would not fix
/// that; only overwriting does. `SyncSender` cannot pop, so the queue is replaced outright.
///
/// A one-deep slot is also the right resource shape: a queued [`CapturedFrame`] can own a dup'd
/// dmabuf fd or a CUDA buffer, so a backlog of eight pinned eight compositor buffers.
type FrameSlot = Arc<std::sync::Mutex<Option<CapturedFrame>>>;

/// The per-session capture options — the four `bool`s that used to ride as consecutive positional
/// parameters through `spawn_pipewire` and on into `pipewire_thread`.
///
/// Named fields because four adjacent same-typed arguments are a silent-transposition footgun: swap
/// `want_444` and `want_hdr` at a call site and it compiles, negotiates the wrong pod family, and
/// shows up as a black screen ten seconds later. (Sweep Phase 5.6.)
#[derive(Clone, Copy)]
struct CaptureOpts {
    /// Allow GPU zero-copy capture (dmabuf→CUDA/VA). `false` forces the CPU mmap path even when
    /// `PUNKTFUNK_ZEROCOPY` is set — the session plan passes `gpu = false` when 4:4:4 has no
    /// zero-copy convert available (see `SessionPlan::output_format`).
    allow_zerocopy: bool,
    /// 4:4:4 session: tiled dmabufs convert to planar YUV444 on the GPU (`ImportKind::Tiled444`)
    /// instead of NV12/RGB, so the session stays zero-copy at full chroma.
    want_444: bool,
    /// HDR session (GNOME 50+ monitor mirror): offer ONLY the 10-bit PQ/BT.2020 formats as LINEAR
    /// dmabufs. SHM cannot carry them — Mutter's SHM record path paints 8-bit ARGB32 regardless of
    /// the negotiated format, and the tiled EGL de-tile blit is 8-bit.
    want_hdr: bool,
    /// The producer's FIRST negotiation is for a sacrificial mode and a renegotiation to
    /// `preferred`'s dims is guaranteed to follow (KWin virtual outputs — see kwin.rs `create`):
    /// skip whole buffers until the negotiated size matches, so the pipeline never builds against
    /// the doomed birth mode. `false` everywhere else (Mutter SIZES the monitor from negotiation and
    /// gamescope fixates its own — gating those would starve legitimate first frames).
    expect_exact_dims: bool,
    /// The producer rewrites `SPA_META_Cursor` on EVERY buffer, so an `id == 0` meta is an
    /// authoritative "pointer hidden / off this output" the blend must honor (KWin). `false` for
    /// the stale-meta producers (Mutter recycles buffers without rewriting the region) — see
    /// [`pw_cursor::CursorState::id0_hides`](pw_cursor) for the full contract.
    cursor_id0_hides: bool,
}

/// The shared state the PipeWire thread PUBLISHES and the capturer READS — one struct instead of
/// seven separately-plumbed `Arc`s.
///
/// Those seven were created in `spawn_pipewire`, `_cb`-cloned one by one, passed as seven of
/// `pipewire_thread`'s parameters (most of its arity, and most of the reason for three
/// `too_many_arguments` allows), and then re-declared as fields on `PwHandles`, `PortalCapturer` AND
/// `UserData` — the same list written out four times, each with its own drifting copy of the doc
/// comment. Collapsed in sweep Phase 5.6: `Clone` is a refcount bump per field, and the producer and
/// consumer now demonstrably share ONE set.
#[derive(Clone)]
struct CaptureSignals {
    /// A stream is active: the PipeWire thread does the (expensive at 5K) per-frame de-pad only
    /// while this is set, so a pooled capturer costs almost nothing between streams.
    active: Arc<AtomicBool>,
    /// Set once the stream agrees a video format. Read in `next_frame`'s timeout branch to tell
    /// "format never negotiated" (modifier/format mismatch) apart from "negotiated but no buffers
    /// arrived" (compositor idle/unmapped) — the two black-screen root causes.
    negotiated: Arc<AtomicBool>,
    /// True only while the stream is `Streaming`. `try_latest` reads it to tell a static desktop
    /// (alive, no new buffers) from a dead source, and `is_alive` gates re-pooling on it.
    streaming: Arc<AtomicBool>,
    /// Poison flag: the zero-copy GPU import is irrecoverably gone for this stream (the import
    /// worker died — e.g. absorbing the driver fault of a crashing compositor — or tiled imports
    /// failed repeatedly, where the CPU fallback would de-pad scrambled tiled bytes). Both
    /// `next_frame` and `try_latest` surface it as an error, so the session's capture-loss rebuild
    /// runs instead of freezing or corrupting. Never cleared.
    broken: Arc<AtomicBool>,
    /// Set once the stream negotiated one of the 10-bit PQ formats, i.e. frames really are
    /// PQ/BT.2020 — drives `hdr_meta`.
    hdr_negotiated: Arc<AtomicBool>,
    /// Set by the PipeWire thread once it ACTUALLY advertised the EGL→CUDA dmabuf-only offer
    /// (importer constructed, importable modifiers found, not the raw passthrough, not HDR).
    /// `plan.build_importer` alone cannot answer this — the importer may fail to construct (no
    /// CUDA on this box), in which case no dmabuf was offered and a negotiation timeout must NOT
    /// latch the GPU offer off. Read by `next_frame`'s timeout diagnosis, the EGL→CUDA twin of
    /// the raw-passthrough arm.
    gpu_dmabuf_offer: Arc<AtomicBool>,
    /// The LIVE cursor overlay, published from every buffer's `SPA_META_Cursor` — including the
    /// cursor-only "corrupted" buffers that never become frames — so the encode loop's forwarder
    /// tracks pointer-only motion on a static desktop (the frame-attached overlay alone goes stale
    /// between damage frames). The gamescope XFixes source publishes into this SAME slot.
    cursor_live: Arc<std::sync::Mutex<Option<pf_frame::CursorOverlay>>>,
    /// The NEGOTIATED frame size, packed `(w << 32) | h`; `0` until `param_changed` runs. Read by
    /// the gamescope cursor source, which must map root-space pointer coordinates into FRAME space
    /// (gamescope's `-w/-h` and `-W/-H` are independent knobs).
    frame_size: Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureSignals {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            negotiated: Arc::new(AtomicBool::new(false)),
            streaming: Arc::new(AtomicBool::new(false)),
            broken: Arc::new(AtomicBool::new(false)),
            hdr_negotiated: Arc::new(AtomicBool::new(false)),
            gpu_dmabuf_offer: Arc::new(AtomicBool::new(false)),
            cursor_live: Arc::new(std::sync::Mutex::new(None)),
            frame_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

/// Live monitor capturer backed by the portal + PipeWire threads. Kept alive (reused) across
/// streams — [`set_active`](Capturer::set_active) gates the per-frame de-pad copy so it costs
/// almost nothing between streams while the screencast session stays up (instant reconnect,
/// and no second session to conflict with).
pub struct PortalCapturer {
    /// THE frame the PipeWire thread produced most recently and the consumer has not taken yet.
    /// See [`FrameSlot`] for why this is a one-deep overwriting slot rather than a queue.
    slot: FrameSlot,
    /// Wakeup edges from the producer (one `()` per publish, best-effort). The SLOT is the truth —
    /// this only un-blocks a waiter, so a coalesced edge loses nothing. Also the producer-liveness
    /// signal: its sender dies with the PipeWire thread, so `Disconnected` here means the thread
    /// ended.
    wake: Receiver<()>,
    signals: CaptureSignals,
    /// When the stream first dropped out of `Streaming` with no new frame; used to grace a transient
    /// renegotiation before declaring the source lost. Cleared whenever a frame arrives or the stream
    /// is `Streaming`.
    stall_since: Option<std::time::Instant>,
    /// True when this capture runs the raw-dmabuf passthrough offer (VAAPI backend, or ANY vendor on
    /// a PyroWave session). Taken verbatim from the ONE
    /// [`NegotiationPlan`](pipewire::NegotiationPlan) the thread also consumes — never re-derived
    /// here, which is what let this bool drift from the thread's real decision. If the offer never
    /// negotiates, [`next_frame`](Capturer::next_frame)'s timeout branch latches the scoped
    /// downgrade ([`pf_zerocopy::note_raw_dmabuf_negotiation_failed`]) so the pipeline rebuild
    /// retries on the CPU offer instead of failing identically forever.
    vaapi_dmabuf: bool,
    /// PW3: this capture's dmabuf offer has been confirmed to negotiate (a frame arrived), so the
    /// negotiation retry budget has already been credited back. One-shot — the credit is per
    /// capture, not per frame.
    negotiation_confirmed: bool,
    /// This capture ran the HDR (10-bit PQ/BT.2020 dmabuf) offer — see [`Self::open`]'s
    /// `want_hdr`. Read by the negotiation-timeout diagnosis (a failed HDR offer latches the
    /// process-wide SDR downgrade) and by [`hdr_meta`](Capturer::hdr_meta).
    hdr_offer: bool,
    /// Which HDR source this capturer is — the latch a failed [`hdr_offer`](Self::hdr_offer)
    /// belongs to. See [`super::HdrSource`] for why the latch is not one process-wide flag.
    hdr_source: super::HdrSource,
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
    /// The portal control-plane thread's teardown handles ([`PortalSession`]). `Some` only on the
    /// [`open`](Self::open) path; `None` for a virtual-output capturer (no portal thread). Held
    /// purely for its `Drop` (like `_keepalive`) — that is what ends the compositor's screencast.
    _portal: Option<PortalSession>,
    /// The gamescope XFixes cursor reader (remote-desktop-sweep Phase C), when this capturer
    /// serves a gamescope node. `Some` after
    /// [`attach_gamescope_cursor`](Capturer::attach_gamescope_cursor); its `Drop` stops the reader
    /// thread, so it lives exactly as long as the capturer. `None` on the portal path (its cursor
    /// comes from `SPA_META_Cursor`).
    _gs_cursor: Option<xfixes_cursor::XFixesCursorSource>,
}

/// Teardown handles for the portal control-plane thread. Firing `quit` un-parks
/// `portal_thread`(`_remote_desktop`), so `rt.block_on` returns, the tokio runtime and the zbus
/// connection drop — and **dropping that connection is what ends the compositor's ScreenCast
/// session** (ashpd's `Session` has no `Drop`). Without this the thread parked forever on
/// `future::pending()`, so every dropped portal capturer permanently leaked a thread, a 2-worker
/// runtime, a zbus connection AND a live screencast: a GameStream HDR↔SDR reconnect drops the
/// pooled capturer and opens a fresh session, so the sessions accumulated — the exact "second
/// conflicting screencast" the capturer pooling exists to prevent.
struct PortalSession {
    /// Un-parks the portal thread. `Option` so `Drop` can take (and thereby fire) it; dropping the
    /// sender without a send resolves the receiver with `Err`, which the thread treats identically.
    quit: Option<tokio::sync::oneshot::Sender<()>>,
    /// Signalled by the spawned closure after the runtime has been dropped — lets `Drop` bound its
    /// wait instead of blocking on `join()` behind a wedged D-Bus round-trip.
    done: Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Un-park the thread, then wait — BOUNDED — for it to unwind. The bound matters because the
        // thread may be inside a D-Bus round-trip against a wedged portal, and teardown runs on the
        // session path: an unbounded `join()` there would hang the host, not just leak. On timeout
        // we detach (the thread owns only its own runtime + connection and exits on its own once the
        // call returns), having at least fired the quit.
        drop(self.quit.take()); // send-or-drop: both resolve the receiver
        let joinable = match self.done.recv_timeout(Duration::from_millis(750)) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!(
                    "portal thread did not unwind within 750ms — detaching it (the compositor's \
                     ScreenCast session may linger until the host exits)"
                );
                false
            }
        };
        if let Some(join) = self.join.take() {
            if joinable {
                let _ = join.join(); // returns immediately: `done` already fired
            }
        }
    }
}

impl PortalCapturer {
    /// `anchored` drives ScreenCast off a RemoteDesktop session (KWin/GNOME) so it inherits the
    /// RemoteDesktop grant and never raises a separate ScreenCast dialog; `false` uses a plain
    /// ScreenCast session (wlroots, which has no RemoteDesktop portal). `want_hdr` offers the
    /// GNOME 50+ HDR formats (10-bit PQ/BT.2020, dmabuf-only) instead of the SDR set.
    /// `want_metadata_cursor` picks the cursor mode — `true` asks for `SPA_META_Cursor` (this
    /// session's encode path composites it), `false` asks the compositor to EMBED the pointer; see
    /// `portal::choose_cursor_mode`.
    pub fn open(
        anchored: bool,
        want_hdr: bool,
        want_metadata_cursor: bool,
        policy: ZeroCopyPolicy,
    ) -> Result<PortalCapturer> {
        // Portal handshake (async) on its own thread; hands back the PW fd + node id.
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        // Teardown plumbing (see `PortalSession`): `quit_rx` parks the thread once the handshake is
        // done, `done_tx` reports that it has fully unwound.
        let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = sync_channel::<()>(1);
        let join = thread::Builder::new()
            .name("punktfunk-portal".into())
            .spawn(move || {
                if anchored {
                    portal_thread_remote_desktop(setup_tx, quit_rx, want_metadata_cursor)
                } else {
                    portal_thread(setup_tx, quit_rx, want_metadata_cursor)
                }
                // After the runtime has been dropped inside the fn above, so a successful
                // `recv_timeout` in `Drop` means the zbus connection is really gone. Covers the
                // early-return paths too (a runtime that failed to build).
                let _ = done_tx.send(());
            })
            .context("spawn portal thread")?;
        let portal = PortalSession {
            quit: Some(quit_tx),
            done: done_rx,
            join: Some(join),
        };

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
        // NOTE the `?`: on a PipeWire-spawn failure `portal` drops here, which fires the quit sender
        // and tears the just-created screencast session down instead of stranding it.
        Ok(spawn_pipewire(
            Some(fd),
            node_id,
            None,
            CaptureOpts {
                allow_zerocopy: true,
                want_444: false,
                want_hdr,
                expect_exact_dims: false,
                // The portal-monitor path today is Mutter (the GNOME HDR mirror) — the stale-meta
                // id-0 contract. A KDE portal capture would rewrite per buffer, but nothing routes
                // one through here yet; the virtual-output path below carries the real flag.
                cursor_id0_hides: false,
            },
            policy,
        )?
        .into_capturer(node_id, None, Some(portal), super::HdrSource::PortalMonitor))
    }

    /// Build a capturer from an already-created virtual output's PipeWire node. The host facade
    /// explodes its `vdisplay::VirtualOutput` into these primitives so this crate never depends on
    /// the vdisplay type: `remote_fd` selects portal-remote vs. default-daemon, `node_id` is the
    /// output's screencast node, `preferred_mode` seeds format negotiation, and `keepalive` owns the
    /// output (dropping the capturer releases it). `allow_zerocopy` mirrors
    /// [`OutputFormat::gpu`](pf_frame::OutputFormat): `false` forces the CPU mmap path, `true` keeps
    /// the GPU zero-copy path subject to `PUNKTFUNK_ZEROCOPY`. `want_444` (a 4:4:4 session) makes the
    /// zero-copy worker convert tiled dmabufs to planar YUV444 on the GPU instead of NV12/RGB.
    /// `want_hdr` runs the 10-bit PQ/BT.2020 offer instead of the SDR set — see
    /// [`crate::open_virtual_output`] for who is allowed to pass it. `cursor_id0_hides` declares
    /// the producer's cursor-meta contract ([`CaptureOpts::cursor_id0_hides`]).
    #[allow(clippy::too_many_arguments)]
    pub fn from_virtual_output(
        remote_fd: Option<OwnedFd>,
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
        allow_zerocopy: bool,
        want_444: bool,
        want_hdr: bool,
        policy: ZeroCopyPolicy,
        expect_exact_dims: bool,
        cursor_id0_hides: bool,
    ) -> Result<PortalCapturer> {
        tracing::info!(
            node_id,
            allow_zerocopy,
            want_444,
            want_hdr,
            expect_exact_dims,
            cursor_id0_hides,
            "connecting PipeWire to virtual output"
        );
        // Most virtual outputs are SDR-only upstream (Mutter's RecordVirtual streams advertise
        // 8-bit BGRx/BGRA exclusively, GNOME 50 and 51-dev alike; KWin/wlroots the same), so
        // `want_hdr` reaches here ONLY for a gamescope node off our `pipewire-hdr` build — the
        // host resolves that before the Welcome (`capture::capturer_supports_hdr_for`).
        Ok(spawn_pipewire(
            remote_fd,
            node_id,
            preferred_mode,
            CaptureOpts {
                allow_zerocopy,
                want_444,
                want_hdr,
                expect_exact_dims,
                cursor_id0_hides,
            },
            policy,
        )?
        // No portal thread on this path: the node belongs to a virtual output the caller created,
        // and `keepalive` is what releases it.
        .into_capturer(
            node_id,
            Some(keepalive),
            None,
            super::HdrSource::VirtualOutput,
        ))
    }
}

/// Live PipeWire-thread handles returned by [`spawn_pipewire`]: the frame channel, the
/// activation flag the per-frame copy gates on, a "format negotiated" flag (timeout diagnostics),
/// a quit sender that stops the loop, and the thread's join handle (synchronous teardown).
struct PwHandles {
    /// See [`FrameSlot`] / [`PortalCapturer::slot`].
    slot: FrameSlot,
    /// See [`PortalCapturer::wake`].
    wake: Receiver<()>,
    signals: CaptureSignals,
    /// This capture will offer LINEAR-dmabuf-only for the VAAPI passthrough (see
    /// [`PortalCapturer::vaapi_dmabuf`]).
    vaapi_dmabuf: bool,
    /// This capture ran the HDR offer (see [`PortalCapturer::hdr_offer`]).
    hdr_offer: bool,
    quit: ::pipewire::channel::Sender<()>,
    join: thread::JoinHandle<()>,
}

impl PwHandles {
    /// Assemble a [`PortalCapturer`] around these handles. `node_id` is carried for diagnostics;
    /// `keepalive` owns the virtual output (drops after the PipeWire thread is joined); `portal`
    /// carries the control-plane thread's teardown handles on the [`PortalCapturer::open`] path.
    fn into_capturer(
        self,
        node_id: u32,
        keepalive: Option<Box<dyn Send>>,
        portal: Option<PortalSession>,
        hdr_source: super::HdrSource,
    ) -> PortalCapturer {
        PortalCapturer {
            slot: self.slot,
            wake: self.wake,
            signals: self.signals,
            stall_since: None,
            vaapi_dmabuf: self.vaapi_dmabuf,
            negotiation_confirmed: false,
            hdr_offer: self.hdr_offer,
            hdr_source,
            node_id,
            quit: Some(self.quit),
            join: Some(self.join),
            _keepalive: keepalive,
            _portal: portal,
            _gs_cursor: None,
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
    opts: CaptureOpts,
    // Encode-backend facts resolved by the facade (never re-derived here) — the one-way
    // capture→encode edge (plan §W6).
    policy: ZeroCopyPolicy,
) -> Result<PwHandles> {
    // `expect_exact_dims` is forwarded to the thread inside `opts`, not read here.
    let CaptureOpts {
        allow_zerocopy,
        want_444,
        want_hdr,
        ..
    } = opts;
    // Frames hand over through the one-deep overwriting mailbox (see `FrameSlot`); the channel
    // carries only wakeup EDGES, so depth 1 is exactly right — a coalesced edge loses nothing
    // because the slot, not the channel, holds the frame.
    let slot: FrameSlot = Arc::new(std::sync::Mutex::new(None));
    let slot_cb = slot.clone();
    let (wake_tx, wake_rx) = sync_channel::<()>(1);
    let signals = CaptureSignals::new();
    let signals_cb = signals.clone();
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
    // PW3: tell the raw-dmabuf latch which capture this is BEFORE reading its verdict below. A
    // different node id is a different question — a fresh virtual output, a compositor restart,
    // the Bazzite Gaming↔Desktop switch — and inheriting "dmabuf does not work here" from an
    // unrelated capture is how one transient timeout used to cost a host CPU capture until it was
    // restarted. The portal bit is in the key because a portal-fd capture and a virtual-output
    // capture with the same node number are genuinely different sources.
    pf_zerocopy::note_raw_dmabuf_capture(u64::from(node_id) | (u64::from(fd.is_some()) << 32));
    // THE negotiation decision, resolved once here and handed to the thread — no mirror (L3/F1).
    // Every environment/latch read the decision depends on happens at this single point.
    let plan = pipewire::negotiation_plan(pipewire::NegotiationInputs {
        zerocopy,
        force_shm,
        want_hdr,
        want_444,
        backend_is_vaapi: policy.backend_is_vaapi,
        pyrowave_session: policy.pyrowave_session,
        native_nv12_session: policy.native_nv12_session,
        raw_dmabuf_import_disabled: pf_zerocopy::raw_dmabuf_import_disabled(),
        gpu_import_disabled: pf_zerocopy::gpu_import_disabled(),
        gpu_dmabuf_negotiation_failed: pf_zerocopy::gpu_dmabuf_negotiation_disabled(),
        // Default ON; `PUNKTFUNK_PIPEWIRE_NV12=0` (or any falsy spelling — the shared parser, not a
        // bare `!= "0"` string compare) restores the packed-RGB negotiation.
        native_nv12_env_on: pf_host_config::env_on("PUNKTFUNK_PIPEWIRE_NV12").unwrap_or(true),
        hdr_cuda_ok: policy.hdr_cuda_ok,
    });
    // The capturer's timeout diagnosis reads the SAME resolved bool the thread acts on.
    let vaapi_dmabuf = plan.vaapi_passthrough;
    let join = thread::Builder::new()
        .name("punktfunk-pipewire".into())
        .spawn(move || {
            if let Err(e) = pipewire::pipewire_thread(
                fd,
                node_id,
                slot_cb,
                wake_tx,
                signals_cb,
                plan,
                // `allow_zerocopy` is already folded into `plan`; the rest still matter downstream.
                CaptureOpts { want_hdr, ..opts },
                preferred,
                quit_rx,
                policy,
            ) {
                tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
            }
        })
        .context("spawn pipewire thread")?;
    Ok(PwHandles {
        slot,
        wake: wake_rx,
        signals,
        vaapi_dmabuf,
        hdr_offer: want_hdr,
        quit: quit_tx,
        join,
    })
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.frame_within(Duration::from_secs(10), TimeoutVerdict::Conclusive)
    }

    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        // The PipeWire thread's live cursor slot (fed by every buffer's meta, frames or not) —
        // lets the forwarder track pointer-only motion on a static desktop. See `cursor_live`.
        // On a gamescope node the meta never arrives; the XFixes source (attached below) fills
        // the same slot instead.
        self.signals
            .cursor_live
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    fn attach_gamescope_cursor(&mut self, targets: crate::GamescopeCursorTargets) {
        // gamescope paints no `SPA_META_Cursor`, so `cursor_live` would stay empty. Spawn the
        // XFixes reader to publish gamescope's pointer into that SAME slot — `cursor()` above then
        // serves it and the encode loop composites it, exactly like the portal path. It connects to
        // every nested Xwayland the provider reports, RE-RUNS the provider so a game's Xwayland
        // that appears later is adopted, and follows whichever one gamescope draws the pointer on.
        // `frame_size` lets it map root-space coordinates into frame space.
        self._gs_cursor = xfixes_cursor::XFixesCursorSource::spawn(
            targets,
            Arc::clone(&self.signals.cursor_live),
            Arc::clone(&self.signals.frame_size),
        );
    }

    fn next_frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        self.frame_within(budget, TimeoutVerdict::Conclusive)
    }

    fn next_frame_within_provisional(&mut self, budget: Duration) -> Result<CapturedFrame> {
        // The retry loop's truncated first attempt: its expiry re-runs the schedule, it does not
        // convict an offer — see `TimeoutVerdict` and the latch arms in `next_frame_timed_out`.
        self.frame_within(budget, TimeoutVerdict::Provisional)
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        // Frame-driven trigger (latency plan T1.1): block until the compositor delivers a frame or
        // the deadline passes. Now that the hand-off is a peekable slot this honours the trait's
        // "must NOT consume" contract directly — it observes the slot and leaves the frame for
        // `try_latest`, so the `pending` stash the old un-peekable channel forced no longer exists.
        // A broken/ended stream just returns; the following `try_latest` surfaces the error.
        if self.signals.broken.load(Ordering::Relaxed) {
            return;
        }
        loop {
            if self.slot.lock().is_ok_and(|s| s.is_some()) {
                return;
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return;
            };
            // Timeout OR a dead producer: either way stop waiting and let `try_latest` classify.
            if self.wake.recv_timeout(left).is_err() {
                return;
            }
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.signals.broken.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "zero-copy GPU import lost (node {}): the import worker died or tiled imports \
                 failed repeatedly — rebuilding capture",
                self.node_id
            ));
        }
        // Take the newest frame, if any; `None` means the compositor hasn't produced one since the
        // last call (static/idle desktop). Wakeup edges are drained first — they are pure edges, so
        // stale ones must not make the next `wait_arrival` return early — and `Disconnected` there
        // is how a dead PipeWire thread surfaces (a frame it left behind is still served first).
        let mut producer_gone = false;
        loop {
            match self.wake.try_recv() {
                Ok(()) => continue,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    producer_gone = true;
                    break;
                }
            }
        }
        let latest = self.take_frame();
        if producer_gone && latest.is_none() {
            return Err(anyhow!("PipeWire capture thread ended"));
        }
        if latest.is_some() || self.signals.streaming.load(Ordering::Relaxed) {
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

    fn set_active(&mut self, active: bool) {
        self.signals.active.store(active, Ordering::Relaxed);
        if !active {
            // Flush the mailbox on stream end (L10): a reused capturer used to open the NEXT stream
            // by handing over the PREVIOUS session's last frame — wrong content, and stamped with a
            // `pts_ns` from before the new stream's clock. `active == false` also stops the producer
            // publishing, so nothing refills it until the next `set_active(true)`.
            if let Ok(mut slot) = self.slot.lock() {
                *slot = None;
            }
        }
    }

    /// All three of this backend's terminal states, checked without consuming a frame — every one
    /// of them is sticky, so an `Err` from `next_frame`/`try_latest` here is permanent:
    ///   * `broken` — the zero-copy GPU import is irrecoverably gone for this stream (worker death,
    ///     or a tiled-import failure streak). Never cleared.
    ///   * the PipeWire thread has exited (its `Drop`-less death is otherwise indistinguishable from
    ///     an idle desktop: the frame channel just goes quiet, and `streaming` keeps its last value).
    ///   * the stream left `Streaming` for good — the compositor or virtual output went away.
    ///
    /// A static desktop stays `Streaming` (it simply sends no buffers), so an idle-but-healthy
    /// capture is never reported dead.
    fn is_alive(&self) -> bool {
        !self.signals.broken.load(Ordering::Relaxed)
            && self.signals.streaming.load(Ordering::Relaxed)
            && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Generic HDR10 mastering metadata once the stream negotiated a 10-bit PQ format — for
    /// BOTH Linux HDR sources (the portal monitor mirror and a gamescope virtual output).
    /// Neither producer exposes a mastering volume through the screencast: Mutter has no
    /// per-monitor one, and gamescope's PipeWire node carries no per-frame HDR metadata (the
    /// game's `VK_EXT_hdr_metadata` blob stops at the compositor — forwarding it needs the
    /// patch's v2 custom SPA meta). So this is the standard HDR10 default block (BT.2020
    /// primaries, D65 white, 1000 / 0.005 cd/m², CLL unknown) — the same fallback Windows uses
    /// when a display reports nothing. The native stream loop prefers the client display's own
    /// volume when the client sent one (`Hello::display_hdr`).
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        if !self.signals.hdr_negotiated.load(Ordering::Relaxed) {
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

/// Whether an expired first-frame budget is allowed to CONVICT an offer. The retry loop's
/// deliberately truncated first attempt passes `Provisional`: its expiry means the schedule
/// moved on, not that the compositor refused anything — a gamescope cold start regularly needs
/// longer than that window to accept every offer it would have accepted. Latching from it pinned
/// the whole host process to SDR + CPU capture off a race the attempt lost by design; only a
/// full-length wait carries a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutVerdict {
    Conclusive,
    Provisional,
}

/// Which offer a first-frame timeout implicates — the diagnosis behind
/// [`PortalCapturer::next_frame_timed_out`], split out pure so the latch policy is testable.
/// Mirrors the negotiation state exactly: a negotiated format clears every offer (the compositor
/// accepted, it just produced nothing), and a forced `PUNKTFUNK_ZEROCOPY=1` keeps both dmabuf
/// arms erroring loudly instead of implicating them (the operator asked for exactly that path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutOffer {
    /// Format negotiated; no offer implicated — the compositor produced no buffers.
    NoBuffers,
    /// The 10-bit PQ/BT.2020 (HDR) dmabuf offer was never accepted.
    Hdr,
    /// The dmabuf-only raw-passthrough offer was never accepted.
    RawDmabuf,
    /// The dmabuf-only EGL→CUDA offer was never accepted.
    GpuDmabuf,
    /// Nothing negotiated and no offer implicated — format/modifier mismatch.
    NoFormat,
}

fn classify_first_frame_timeout(
    negotiated: bool,
    hdr_offer: bool,
    vaapi_dmabuf: bool,
    gpu_dmabuf_offer: bool,
    zerocopy_forced: bool,
) -> TimeoutOffer {
    if negotiated {
        TimeoutOffer::NoBuffers
    } else if hdr_offer {
        TimeoutOffer::Hdr
    } else if vaapi_dmabuf && !zerocopy_forced {
        TimeoutOffer::RawDmabuf
    } else if gpu_dmabuf_offer && !zerocopy_forced {
        TimeoutOffer::GpuDmabuf
    } else {
        TimeoutOffer::NoFormat
    }
}

/// The latch policy: only a conclusive expiry of an offer-implicating timeout fires the offer's
/// sticky process-wide downgrade.
fn timeout_convicts(offer: TimeoutOffer, verdict: TimeoutVerdict) -> bool {
    verdict == TimeoutVerdict::Conclusive
        && matches!(
            offer,
            TimeoutOffer::Hdr | TimeoutOffer::RawDmabuf | TimeoutOffer::GpuDmabuf
        )
}

impl PortalCapturer {
    /// The blocking first-frame wait behind [`Capturer::next_frame`] /
    /// [`Capturer::next_frame_within`]. First frame can lag behind format negotiation; later
    /// frames arrive at ~fps. Wait in short slices so a GPU-import poison (worker death) fails
    /// the capture within ~0.5 s instead of sitting out the full first-frame budget.
    fn frame_within(&mut self, budget: Duration, verdict: TimeoutVerdict) -> Result<CapturedFrame> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if self.signals.broken.load(Ordering::Relaxed) {
                return Err(anyhow!(
                    "zero-copy GPU import lost (node {}): the import worker died or tiled imports \
                     failed repeatedly — rebuilding capture",
                    self.node_id
                ));
            }
            // The slot before the wakeup: a publish that coalesced its edge (or landed while we were
            // not waiting) is still visible here.
            if let Some(f) = self.take_frame() {
                self.note_negotiation_confirmed();
                return Ok(f);
            }
            let slice = Duration::from_millis(500)
                .min(deadline.saturating_duration_since(std::time::Instant::now()));
            match self.wake.recv_timeout(slice) {
                Ok(()) => continue, // a publish happened — the loop re-reads the slot
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => continue,
                Err(e) => {
                    // A last frame can be sitting in the slot even as the producer exits.
                    if let Some(f) = self.take_frame() {
                        return Ok(f);
                    }
                    return self.next_frame_timed_out(e, budget, verdict);
                }
            }
        }
    }

    /// Take the mailbox's frame, if the producer has published one since the last take.
    fn take_frame(&self) -> Option<CapturedFrame> {
        self.slot.lock().ok().and_then(|mut s| s.take())
    }

    /// PW3: a frame arrived, so this capture's dmabuf-only offer DID negotiate — credit the
    /// negotiation retry budget back. Only meaningful for a capture that actually made that offer,
    /// and only once per capture (the budget counts consecutive failed BUILDS, not frames).
    fn note_negotiation_confirmed(&mut self) {
        if self.vaapi_dmabuf && !self.negotiation_confirmed {
            self.negotiation_confirmed = true;
            pf_zerocopy::note_raw_dmabuf_negotiation_ok();
        }
    }

    /// The [`frame_within`](Self::frame_within) budget expired (or the thread ended) — turn it
    /// into the diagnosis-bearing error, and fire the offer's sticky downgrade latch when — and
    /// only when — the expiry convicts the offer (see [`timeout_convicts`]).
    fn next_frame_timed_out(
        &self,
        err: RecvTimeoutError,
        budget: Duration,
        verdict: TimeoutVerdict,
    ) -> Result<CapturedFrame> {
        let within = budget.as_secs_f32();
        match err {
            RecvTimeoutError::Timeout => {
                let offer = classify_first_frame_timeout(
                    self.signals.negotiated.load(Ordering::Relaxed),
                    self.hdr_offer,
                    self.vaapi_dmabuf,
                    self.signals.gpu_dmabuf_offer.load(Ordering::Relaxed),
                    pf_zerocopy::zerocopy_forced(),
                );
                let convicted = timeout_convicts(offer, verdict);
                // A provisional expiry names the same suspect but hands down no sentence — the
                // full-length retry that follows is the one whose timeout latches.
                let sentence = if convicted {
                    "" // each arm below states its own downgrade
                } else {
                    " (short first-attempt window — nothing is latched; the full-length retry \
                     decides)"
                };
                match offer {
                    TimeoutOffer::NoBuffers => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiated but no \
                         buffers arrived — the compositor produced no frames (virtual output \
                         idle/unmapped, capture never started, or a stream bound during a \
                         compositor (re)start that will never deliver — a reconnect fixes that)",
                        self.node_id
                    )),
                    TimeoutOffer::Hdr => {
                        // The HDR (10-bit PQ dmabuf) offer was never accepted — the monitor left HDR
                        // mode between the probe and the negotiation, the compositor pre-dates the
                        // GNOME 50 HDR formats, or its allocator can't do LINEAR for XR30/XB30.
                        // Latch the SDR downgrade for THIS source (`HdrSource`, not process-wide — one
                        // shared flag let either Linux HDR source disable the other) so the next session
                        // (Moonlight auto-reconnects) negotiates SDR instead of re-running this timeout.
                        if convicted {
                            super::note_hdr_capture_failed(self.hdr_source);
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the HDR (10-bit PQ/BT.2020 dmabuf) offer — is the mirrored \
                             monitor in HDR mode on GNOME 50+?{}",
                            self.node_id,
                            if convicted {
                                " Downgrading this host to SDR capture; reconnect to stream SDR"
                            } else {
                                sentence
                            }
                        ))
                    }
                    TimeoutOffer::RawDmabuf => {
                        // The dmabuf-only raw-passthrough offer was never accepted. Latch the
                        // downgrade so the encode loop's pipeline rebuild retries on the CPU offer
                        // instead of failing this same negotiation forever. The latch is SCOPED to the
                        // raw-passthrough decision: it used to be `note_vaapi_dmabuf_failed`, which fed
                        // `pf_zerocopy::enabled()` and therefore dropped every later session on this
                        // host — NVENC's EGL→CUDA path included — to CPU capture. Since this offer is
                        // also the PyroWave one (any vendor), a single PyroWave negotiation timeout was
                        // enough to do that.
                        if convicted {
                            pf_zerocopy::note_raw_dmabuf_negotiation_failed();
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the dmabuf-only offer (raw-dmabuf passthrough){}",
                            self.node_id,
                            if convicted {
                                " — downgrading THIS path to CPU capture for the rest of the \
                                 process; the pipeline rebuild will renegotiate without dmabuf"
                            } else {
                                sentence
                            }
                        ))
                    }
                    TimeoutOffer::GpuDmabuf => {
                        // The EGL→CUDA dmabuf-only offer was never accepted — the twin of the raw-
                        // passthrough arm above (the offer the thread ACTUALLY made, per the signal
                        // it set — see `CaptureSignals::gpu_dmabuf_offer`). One FULL-LENGTH timeout
                        // is conclusive: a compositor that allocates none of the importer's
                        // modifiers refuses them identically on every retry, so latch the offer off
                        // and let the pipeline rebuild renegotiate the CPU path instead of
                        // re-running this same 10 s timeout on every reconnect. A forced
                        // PUNKTFUNK_ZEROCOPY=1 keeps erroring loudly instead (same rule as the raw
                        // arm).
                        if convicted {
                            pf_zerocopy::note_gpu_dmabuf_negotiation_failed();
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the dmabuf-only offer (EGL→CUDA GPU import){}",
                            self.node_id,
                            if convicted {
                                " — downgrading THIS offer to the CPU path for the rest of the \
                                 process; the pipeline rebuild will renegotiate without dmabuf"
                            } else {
                                sentence
                            }
                        ))
                    }
                    TimeoutOffer::NoFormat => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiation never \
                         completed — the compositor offered no format this consumer accepts \
                         (pixel-format/modifier mismatch) or the node never emitted a Format param",
                        self.node_id
                    )),
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

// The portal CONTROL PLANE (the ScreenCast/RemoteDesktop handshake + GNOME's colour-mode probe),
// split out in sweep Phase 5.3 — async/tokio/zbus, none of it per-frame. `gnome_hdr_monitor_active`
// is the host's HDR gate, re-exported from `lib.rs`.
mod portal;
pub use portal::gnome_hdr_monitor_active;
use portal::{portal_thread, portal_thread_remote_desktop};

// The PipeWire consumer (the PW types are `!Send`, so it owns its own thread). Split out of this
// file in the pf-capture sweep Phase 5.1: it was 1,900 of these 2,778 lines. `mod.rs` is a
// directory module, so the plain `mod pipewire;` resolves to `linux/pipewire.rs` with no
// `#[path]` — and `super` inside it still means `linux`, so every `use super::…` in the moved
// code keeps its meaning (the trap recorded in 28f8fc71).
mod pipewire;
// Two leaf halves of that consumer, carved out in sweep Phase 5.2: the negotiation POD builders
// (the crate's wire surface — what a compositor intersects against) and the cursor-as-metadata
// parser + CPU composite blits (the producer-driven, bounds-critical half). Both are pure enough to
// unit-test without a compositor, which is the point.
mod pw_cursor;
mod pw_pods;

#[cfg(test)]
mod first_frame_timeout_tests {
    use super::{classify_first_frame_timeout, timeout_convicts, TimeoutOffer, TimeoutVerdict};

    #[test]
    fn a_provisional_expiry_convicts_no_offer_whatever_was_on_the_table() {
        // The bug this pins down: the retry loop's truncated 2.5 s first attempt latched all
        // three sticky process-wide downgrades as if the compositor had refused the offers — a
        // gamescope HDR cold start then streamed SDR (and CPU-copied) for the process lifetime.
        for offer in [
            TimeoutOffer::NoBuffers,
            TimeoutOffer::Hdr,
            TimeoutOffer::RawDmabuf,
            TimeoutOffer::GpuDmabuf,
            TimeoutOffer::NoFormat,
        ] {
            assert!(
                !timeout_convicts(offer, TimeoutVerdict::Provisional),
                "provisional expiry must not latch {offer:?}"
            );
        }
    }

    #[test]
    fn a_conclusive_expiry_convicts_exactly_the_offer_bearing_diagnoses() {
        assert!(timeout_convicts(
            TimeoutOffer::Hdr,
            TimeoutVerdict::Conclusive
        ));
        assert!(timeout_convicts(
            TimeoutOffer::RawDmabuf,
            TimeoutVerdict::Conclusive
        ));
        assert!(timeout_convicts(
            TimeoutOffer::GpuDmabuf,
            TimeoutVerdict::Conclusive
        ));
        // A negotiated-but-idle stream and a plain format mismatch implicate no offer — nothing
        // to latch even on a full-length wait.
        assert!(!timeout_convicts(
            TimeoutOffer::NoBuffers,
            TimeoutVerdict::Conclusive
        ));
        assert!(!timeout_convicts(
            TimeoutOffer::NoFormat,
            TimeoutVerdict::Conclusive
        ));
    }

    #[test]
    fn classification_mirrors_the_negotiation_state_precedence() {
        // A negotiated format clears every offer, whatever else was on the table.
        assert_eq!(
            classify_first_frame_timeout(true, true, true, true, false),
            TimeoutOffer::NoBuffers
        );
        // The HDR offer outranks the dmabuf arms (it is the offer that failed to negotiate).
        assert_eq!(
            classify_first_frame_timeout(false, true, true, true, false),
            TimeoutOffer::Hdr
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, true, true, false),
            TimeoutOffer::RawDmabuf
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, false, true, false),
            TimeoutOffer::GpuDmabuf
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, false, false, false),
            TimeoutOffer::NoFormat
        );
    }

    #[test]
    fn a_forced_zerocopy_keeps_both_dmabuf_arms_erroring_loudly_instead_of_implicated() {
        // PUNKTFUNK_ZEROCOPY=1 is the operator insisting on the path — the timeout falls through
        // to the generic diagnosis (and so never latches), exactly as the old else-if chain did.
        assert_eq!(
            classify_first_frame_timeout(false, false, true, true, true),
            TimeoutOffer::NoFormat
        );
    }
}
