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

use super::{CapturedFrame, Capturer, DmabufFrame, FramePayload, PixelFormat};
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
    /// the process-wide downgrade ([`crate::zerocopy::note_vaapi_dmabuf_failed`]) so the pipeline
    /// rebuild retries on the CPU offer instead of failing identically forever.
    vaapi_dmabuf: bool,
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
    /// ScreenCast session (wlroots, which has no RemoteDesktop portal).
    pub fn open(anchored: bool) -> Result<PortalCapturer> {
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
            "ScreenCast portal session started; connecting PipeWire"
        );
        // This portal path (GameStream / monitor capture) is always 4:2:0, so allow zero-copy as before.
        Ok(spawn_pipewire(Some(fd), node_id, None, true)?.into_capturer(node_id, None))
    }

    /// Build a capturer from an already-created virtual output ([`crate::vdisplay::VirtualOutput`]):
    /// connect PipeWire to its node (`remote_fd` selects portal-remote vs. default-daemon) and
    /// take ownership of its keepalive so the output lives exactly as long as this capturer. This
    /// is how the client's requested resolution becomes the captured resolution without scaling.
    /// `allow_zerocopy` mirrors [`OutputFormat::gpu`](crate::capture::OutputFormat): `false` forces the
    /// CPU mmap path (a 4:4:4 NVENC session needs CPU-resident RGB), `true` keeps the GPU zero-copy
    /// path subject to `PUNKTFUNK_ZEROCOPY`.
    pub fn from_virtual_output(
        vout: crate::vdisplay::VirtualOutput,
        allow_zerocopy: bool,
    ) -> Result<PortalCapturer> {
        tracing::info!(
            node_id = vout.node_id,
            allow_zerocopy,
            "connecting PipeWire to virtual output"
        );
        let node_id = vout.node_id;
        Ok(
            spawn_pipewire(vout.remote_fd, node_id, vout.preferred_mode, allow_zerocopy)?
                .into_capturer(node_id, Some(vout.keepalive)),
        )
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
    quit: ::pipewire::channel::Sender<()>,
    join: thread::JoinHandle<()>,
}

impl PwHandles {
    /// Assemble a [`PortalCapturer`] around these handles. `node_id` is carried for diagnostics;
    /// `keepalive` owns the virtual output (drops after the PipeWire thread is joined).
    fn into_capturer(self, node_id: u32, keepalive: Option<Box<dyn Send>>) -> PortalCapturer {
        PortalCapturer {
            frames: self.frames,
            active: self.active,
            negotiated: self.negotiated,
            streaming: self.streaming,
            broken: self.broken,
            stall_since: None,
            vaapi_dmabuf: self.vaapi_dmabuf,
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
    // `PUNKTFUNK_ZEROCOPY` is set — a 4:4:4 NVENC session needs CPU-resident RGB (the encoder
    // swscales RGB→YUV444P; `hevc_nvenc` can't 4:4:4 from a CUDA RGB surface), so the session plan
    // passes `gpu = false` for it. Without this, a 4:4:4 session under `PUNKTFUNK_ZEROCOPY=1` would
    // get CUDA frames and the encoder would bail (`want_444 && cuda`).
    allow_zerocopy: bool,
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
    // pipewire's own cross-thread channel: the receiver attaches to the loop and quits it; the
    // sender lives on the capturer and fires in its `Drop`. Absolute `::pipewire` path — the
    // inner `mod pipewire` shadows the crate name at this scope.
    let (quit_tx, quit_rx) = ::pipewire::channel::channel::<()>();
    let zerocopy = allow_zerocopy && crate::zerocopy::enabled();
    // Mirror of the thread's `vaapi_passthrough` decision (deterministic from here: on a VAAPI
    // backend the EGL→CUDA importer is never built) — kept on the capturer so `next_frame`'s
    // negotiation-timeout branch knows a failed negotiation was the LINEAR-dmabuf offer.
    let vaapi_dmabuf = zerocopy
        && std::env::var("PUNKTFUNK_FORCE_SHM").as_deref() != Ok("1")
        && crate::encode::linux_zero_copy_is_vaapi();
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
                zerocopy,
                preferred,
                quit_rx,
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
        quit: quit_tx,
        join,
    })
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        // First frame can lag behind format negotiation; later frames arrive at ~fps. Wait in
        // short slices so a GPU-import poison (worker death) fails the capture within ~0.5 s
        // instead of sitting out the full first-frame budget.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.broken.load(Ordering::Relaxed) {
                return Err(anyhow!(
                    "zero-copy GPU import lost (node {}): the import worker died or tiled imports \
                     failed repeatedly — rebuilding capture",
                    self.node_id
                ));
            }
            let slice = Duration::from_millis(500)
                .min(deadline.saturating_duration_since(std::time::Instant::now()));
            match self.frames.recv_timeout(slice) {
                Ok(frame) => return Ok(frame),
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => continue,
                Err(e) => return self.next_frame_timed_out(e),
            }
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
        // hasn't produced a new frame since last call (static/idle desktop).
        let mut latest = None;
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
}

impl PortalCapturer {
    /// The [`Capturer::next_frame`] budget expired (or the thread ended) — turn it into the
    /// diagnosis-bearing error. Split out of the slicing loop above; behavior unchanged.
    fn next_frame_timed_out(&self, err: RecvTimeoutError) -> Result<CapturedFrame> {
        match err {
            RecvTimeoutError::Timeout => {
                // Split the two black-screen root causes apart so the operator gets a cause, not
                // just a symptom: did the format negotiate (compositor produced no buffers) or
                // not (no acceptable format / node never emitted a param)?
                if self.negotiated.load(Ordering::Relaxed) {
                    Err(anyhow!(
                        "no PipeWire frame within 10s (node {}): format negotiated but no buffers \
                         arrived — the compositor produced no frames (virtual output idle/unmapped, \
                         or capture never started)",
                        self.node_id
                    ))
                } else if self.vaapi_dmabuf && !crate::zerocopy::vaapi_dmabuf_forced() {
                    // The LINEAR-dmabuf-only offer (VAAPI passthrough default) was never accepted.
                    // Latch the process-wide downgrade so the encode loop's pipeline rebuild
                    // retries on the CPU offer instead of failing this same negotiation forever.
                    crate::zerocopy::note_vaapi_dmabuf_failed();
                    Err(anyhow!(
                        "no PipeWire frame within 10s (node {}): the compositor never accepted \
                         the LINEAR-dmabuf offer (VAAPI zero-copy) — downgrading this host to the \
                         CPU capture path; the pipeline rebuild will renegotiate without dmabuf",
                        self.node_id
                    ))
                } else {
                    Err(anyhow!(
                        "no PipeWire frame within 10s (node {}): format negotiation never \
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

/// The portal handshake: connect ScreenCast, select a single monitor, start, open the
/// PipeWire remote, hand the fd + node id back, then keep the session alive.
fn portal_thread(setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>) {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
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
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
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
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
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
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
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

    use super::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
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
            _ => return None,
        })
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
        /// Consecutive tiled-import failures (reset on success); see [`IMPORT_FAIL_POISON`].
        import_fail_streak: u32,
        /// Present when zero-copy is enabled on NVIDIA: imports a dmabuf → CUDA device buffer,
        /// normally via the isolated worker process (`crate::zerocopy::Importer::Remote`).
        importer: Option<crate::zerocopy::Importer>,
        /// VAAPI zero-copy: hand the raw dmabuf to the encoder (which imports + GPU-CSCs it) instead
        /// of a CUDA import. Set when zero-copy is on, the EGL→CUDA importer is unavailable, and the
        /// encoder backend is VAAPI (AMD/Intel).
        vaapi_passthrough: bool,
        /// `PUNKTFUNK_NV12`: on the tiled EGL/GL zero-copy path, convert to NV12 on the GPU and feed
        /// NVENC native YUV (Tier 2A). Off ⇒ the BGRx path is unchanged.
        nv12: bool,
        /// Rate-limit counter for the latest-frame-only diagnostic log (see `.process`).
        dbg_log_n: u64,
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

    /// Build a BGRx dmabuf `EnumFormat` pod advertising the EGL-importable `modifiers` as a
    /// mandatory enum Choice; the compositor fixates to one of them that it can allocate, which
    /// we read back in `param_changed`.
    fn build_dmabuf_format(
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
            pw::spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
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
            match crate::dmabuf_fence::wait_read_ready(datas[0].fd(), 100) {
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
                            error = %format!("{e}"),
                            "dmabuf EXPORT_SYNC_FILE failed — no implicit-fence sync; NVIDIA \
                             zero-copy may show stale frames (no producer explicit sync)"
                        );
                    }
                }
            }
        }

        // VAAPI zero-copy passthrough: hand the raw dmabuf straight to the encoder, which imports
        // it into a VA surface and does RGB→NV12 on the GPU video engine. No CUDA importer here.
        if ud.vaapi_passthrough {
            if let Some(fmt) = ud.format {
                if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
                    if let Some(fourcc) = crate::zerocopy::drm_fourcc(fmt) {
                        let chunk = datas[0].chunk();
                        let offset = chunk.offset();
                        let stride = chunk.stride().max(0) as u32;
                        // dup the fd so it survives the SPA buffer recycle — the encode thread
                        // imports it. (Content stability across the brief map+CSC window relies on
                        // the compositor's buffer-pool depth, like any zero-copy capture.)
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
                                }),
                            });
                            static ONCE: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(true);
                            if ONCE.swap(false, Ordering::Relaxed) {
                                tracing::info!(
                                    w,
                                    h,
                                    modifier = ud.modifier,
                                    fourcc = format_args!("{:#010x}", fourcc),
                                    "zero-copy: handing dmabuf to VAAPI (GPU import + CSC)"
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
            if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
                let plane = crate::zerocopy::DmabufPlane {
                    fd: datas[0].fd(),
                    offset: datas[0].chunk().offset(),
                    stride: datas[0].chunk().stride().max(0) as u32,
                };
                // Tiled modifier → EGL/GL de-tile import; LINEAR (0/unset, e.g.
                // gamescope) → direct CUDA external-memory import (NVIDIA EGL can't
                // sample LINEAR).
                let modifier = (ud.modifier != 0).then_some(ud.modifier);
                if let Some(fourcc) = crate::zerocopy::drm_fourcc(fmt) {
                    // NV12 convert (Tier 2A) only on the tiled EGL/GL path (`modifier.is_some()`):
                    // produce native YUV so NVENC skips its internal RGB→YUV CSC. The LINEAR/Vulkan
                    // (gamescope) path stays RGB — its convert isn't wired here. When NV12 is
                    // produced the frame's format is reported as `Nv12` so the encoder opens native.
                    let nv12 = ud.nv12 && modifier.is_some();
                    let imported = if let Some(m) = modifier {
                        if nv12 {
                            importer.import_nv12(&plane, w as u32, h as u32, fourcc, Some(m))
                        } else {
                            importer.import(&plane, w as u32, h as u32, fourcc, Some(m))
                        }
                    } else {
                        importer.import_linear(&plane, w as u32, h as u32)
                    };
                    match imported {
                        Ok(devbuf) => {
                            ud.import_fail_streak = 0;
                            crate::zerocopy::note_gpu_import_ok();
                            static ONCE: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(true);
                            if ONCE.swap(false, Ordering::Relaxed) {
                                tracing::info!(
                                    w,
                                    h,
                                    modifier = ud.modifier,
                                    nv12,
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
                                format: if nv12 { PixelFormat::Nv12 } else { fmt },
                                payload: FramePayload::Cuda(devbuf),
                            });
                            return;
                        }
                        Err(e) => {
                            let dead = importer.dead();
                            if dead {
                                crate::zerocopy::note_gpu_import_death();
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
        zerocopy: bool,
        preferred: Option<(u32, u32, u32)>,
        quit_rx: pw::channel::Receiver<()>,
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
        // (we simply won't request dmabuf below). Skipped entirely when the encode backend is
        // VAAPI: those frames go to the raw-dmabuf passthrough, and building the importer there
        // would waste a CUDA probe — or worse, on an NVIDIA box forced to PUNKTFUNK_ENCODER=vaapi,
        // succeed and produce CUDA payloads the VAAPI encoder must reject. Also skipped once
        // repeated worker deaths latched the import off (a wedged GPU stack must not crash-loop).
        let backend_is_vaapi = crate::encode::linux_zero_copy_is_vaapi();
        let mut importer = if zerocopy && !backend_is_vaapi {
            if crate::zerocopy::gpu_import_disabled() {
                tracing::warn!(
                    "zero-copy GPU import disabled after repeated import-worker deaths — using CPU path"
                );
                None
            } else {
                match crate::zerocopy::Importer::new_for_capture() {
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
        // PUNKTFUNK_FORCE_SHM=1 forces the race-free download path (SHM, no dmabuf) — required on
        // Mutter+NVIDIA where dmabuf capture has no working sync and shows stale frames. KWin/
        // gamescope don't need it (they blit into the buffer, so no read-before-render race).
        let force_shm = std::env::var("PUNKTFUNK_FORCE_SHM").as_deref() == Ok("1");
        // VAAPI zero-copy passthrough: zero-copy on, no EGL→CUDA importer (any non-NVIDIA host), and
        // the encoder backend is VAAPI → hand the raw dmabuf to the encoder (it imports + GPU-CSCs).
        let vaapi_passthrough = zerocopy && !force_shm && importer.is_none() && backend_is_vaapi;
        // Modifiers our import stack handles for BGRx: the EGL-importable (tiled) set, plus LINEAR
        // (0) — NVIDIA's EGL won't list it, but LINEAR dmabufs (gamescope's only offer) import via
        // CUDA external memory instead. For the VAAPI passthrough path we advertise LINEAR only:
        // radeonsi/iHD import it and any compositor can allocate it.
        let mut modifiers = importer
            .as_mut()
            .map(|i| i.supported_modifiers(crate::zerocopy::drm_fourcc(PixelFormat::Bgrx).unwrap()))
            .unwrap_or_default();
        if (importer.is_some() || vaapi_passthrough) && !modifiers.contains(&0) {
            modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
        }
        let want_dmabuf =
            (importer.is_some() || vaapi_passthrough) && !modifiers.is_empty() && !force_shm;
        if force_shm {
            tracing::info!(
                "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
            );
        } else if zerocopy && !want_dmabuf {
            tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
        } else if vaapi_passthrough {
            tracing::info!(
                "zero-copy: advertising LINEAR dmabuf for direct VAAPI import (GPU CSC)"
            );
        } else if want_dmabuf {
            tracing::info!(
                count = modifiers.len(),
                sample = ?&modifiers[..modifiers.len().min(6)],
                "zero-copy: advertising EGL-importable dmabuf modifiers"
            );
        } else if backend_is_vaapi && crate::capture::gpu_encode() {
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
        if want_dmabuf && !vaapi_passthrough && crate::zerocopy::nv12_enabled() {
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
            import_fail_streak: 0,
            importer,
            vaapi_passthrough,
            nv12: crate::zerocopy::nv12_enabled(),
            dbg_log_n: 0,
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
                    tracing::info!(
                        width = sz.width,
                        height = sz.height,
                        spa_format = ?ud.info.format(),
                        mapped = ?ud.format,
                        modifier = ud.modifier,
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
                // PipeWire dispatches this from a C trampoline with no catch_unwind; a
                // panic crossing that FFI boundary would abort the whole host. Contain it.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Latest-frame-only (OBS pattern): Mutter delivers buffers in bursts and
                    // recycles its pool; an older queued buffer carries a STALE frame. Drain all
                    // queued buffers, requeue the older ones, keep only the newest.
                    // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on
                    // the loop thread, where `pw_stream_dequeue_buffer` is the documented call. It returns
                    // a `*mut pw_buffer` owned by the stream (or null when the queue is drained),
                    // null-checked before any use. The loop is single-threaded, so no concurrent access.
                    let mut newest = unsafe { stream.dequeue_raw_buffer() };
                    if newest.is_null() {
                        return;
                    }
                    let mut drained = 1u32;
                    loop {
                        // SAFETY: same stream/loop-thread contract as the dequeue above; each call returns
                        // the next stream-owned `*mut pw_buffer` or null (null-checked before use).
                        let next = unsafe { stream.dequeue_raw_buffer() };
                        if next.is_null() {
                            break;
                        }
                        // SAFETY: `newest` is a non-null `*mut pw_buffer` previously dequeued from this same
                        // stream and not yet requeued; `pw_stream_queue_buffer` hands ownership back to the
                        // stream. We immediately overwrite `newest = next`, so the requeued pointer is never
                        // touched again (no use-after-requeue). Loop thread, single-threaded.
                        unsafe { stream.queue_raw_buffer(newest) };
                        newest = next;
                        drained += 1;
                    }
                    // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                    // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                    // load through a valid pointer — no mutation or aliasing.
                    let spa_buf = unsafe { (*newest).buffer };

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
                        // SAFETY: `newest` is the non-null buffer we own (dequeued, never requeued on this
                        // skip path); hand it back to the stream exactly once and return without touching it
                        // again. Loop thread inside `.process`.
                        unsafe { stream.queue_raw_buffer(newest) };
                        return;
                    }

                    consume_frame(ud, spa_buf);
                    // SAFETY: `consume_frame` has finished reading `spa_buf` (and the `datas` borrows derived
                    // from `newest`), so requeuing the owned `newest` exactly once here is sound — no
                    // use-after-requeue. Loop thread inside `.process`.
                    unsafe { stream.queue_raw_buffer(newest) };
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire process callback — frame dropped");
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
            tracing::info!(fw, fh, "PW DEBUG: offering fixed BGRx pod");
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
        // pod and let MAP_BUFFERS map it.
        let shm_values = serialize_pod(obj)?;
        let (dmabuf_values, buffers_values) = if want_dmabuf {
            (
                Some(build_dmabuf_format(&modifiers, preferred)?),
                Some(build_dmabuf_buffers()?),
            )
        } else if force_shm {
            // True SHM: exclude DmaBuf so Mutter MUST download (glReadPixels orders against render).
            (None, Some(build_shm_only_buffers()?))
        } else {
            // CPU path still accepts mappable dmabufs (gamescope offers only those once its
            // modifier-bearing format pod wins the intersection).
            (None, Some(build_mappable_buffers()?))
        };

        let mut byte_slices: Vec<&[u8]> = Vec::new();
        match &dmabuf_values {
            Some(d) => byte_slices.push(d),
            None => byte_slices.push(&shm_values),
        }
        if let Some(b) = &buffers_values {
            byte_slices.push(b);
        }
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
}
