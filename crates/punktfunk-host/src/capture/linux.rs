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

use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
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
        Ok(spawn_pipewire(Some(fd), node_id, None)?.into_capturer(node_id, None))
    }

    /// Build a capturer from an already-created virtual output ([`crate::vdisplay::VirtualOutput`]):
    /// connect PipeWire to its node (`remote_fd` selects portal-remote vs. default-daemon) and
    /// take ownership of its keepalive so the output lives exactly as long as this capturer. This
    /// is how the client's requested resolution becomes the captured resolution without scaling.
    pub fn from_virtual_output(vout: crate::vdisplay::VirtualOutput) -> Result<PortalCapturer> {
        tracing::info!(
            node_id = vout.node_id,
            "connecting PipeWire to virtual output"
        );
        let node_id = vout.node_id;
        Ok(
            spawn_pipewire(vout.remote_fd, node_id, vout.preferred_mode)?
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
) -> Result<PwHandles> {
    // Frames flow from the pipewire thread over a small bounded channel.
    let (frame_tx, frame_rx) = sync_channel::<CapturedFrame>(8);
    let active = Arc::new(AtomicBool::new(false));
    let active_cb = active.clone();
    let negotiated = Arc::new(AtomicBool::new(false));
    let negotiated_cb = negotiated.clone();
    // pipewire's own cross-thread channel: the receiver attaches to the loop and quits it; the
    // sender lives on the capturer and fires in its `Drop`. Absolute `::pipewire` path — the
    // inner `mod pipewire` shadows the crate name at this scope.
    let (quit_tx, quit_rx) = ::pipewire::channel::channel::<()>();
    let zerocopy = crate::zerocopy::enabled();
    let join = thread::Builder::new()
        .name("punktfunk-pipewire".into())
        .spawn(move || {
            if let Err(e) = pipewire::pipewire_thread(
                fd,
                node_id,
                frame_tx,
                active_cb,
                negotiated_cb,
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
        quit: quit_tx,
        join,
    })
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        // First frame can lag behind format negotiation; later frames arrive at ~fps.
        match self.frames.recv_timeout(Duration::from_secs(10)) {
            Ok(frame) => Ok(frame),
            Err(RecvTimeoutError::Timeout) => {
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
                } else {
                    Err(anyhow!(
                        "no PipeWire frame within 10s (node {}): format negotiation never \
                         completed — the compositor offered no format this consumer accepts \
                         (pixel-format/modifier mismatch) or the node never emitted a Format param",
                        self.node_id
                    ))
                }
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!(
                "PipeWire capture thread ended before a frame (node {})",
                self.node_id
            )),
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
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
        Ok(latest)
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
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

    use super::{CapturedFrame, FramePayload, PixelFormat};
    use anyhow::{Context, Result};
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::os::fd::OwnedFd;
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
        /// Present when zero-copy is enabled: imports a dmabuf → CUDA device buffer.
        importer: Option<crate::zerocopy::EglImporter>,
        /// GNOME stale-frame filter state. Mutter re-delivers complete OLD pool buffers on the SHM
        /// download path (no GPU sync prevents it on NVIDIA); a captured frame whose content equals
        /// an EARLIER distinct frame (not the immediately-previous one — that's a normal duplicate) is
        /// such a stale re-delivery and gets DROPPED so the encoder never emits the flash. `stale_seen`
        /// counts drops; `keep_stale` (PUNKTFUNK_KEEP_STALE=1) turns the filter off for A/B testing.
        stale_recent: std::collections::VecDeque<u64>,
        stale_last: u64,
        stale_seen: u64,
        keep_stale: bool,
    }

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

    #[allow(clippy::too_many_arguments)]
    pub fn pipewire_thread(
        fd: Option<OwnedFd>,
        node_id: u32,
        tx: SyncSender<CapturedFrame>,
        active: Arc<AtomicBool>,
        negotiated: Arc<AtomicBool>,
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

        // Build the EGL→CUDA importer up front; if it fails, log and fall back to the CPU path
        // (we simply won't request dmabuf below).
        let importer = if zerocopy {
            match crate::zerocopy::EglImporter::new() {
                Ok(i) => Some(i),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                    None
                }
            }
        } else {
            None
        };
        // Modifiers our import stack handles for BGRx: the EGL-importable (tiled) set, plus
        // LINEAR (0) — NVIDIA's EGL won't list it, but LINEAR dmabufs (gamescope's only offer)
        // import via CUDA external memory instead. Tiled stays first so allocators that can do
        // both (KWin) prefer it. If none, we can't negotiate dmabuf → shm path.
        let mut modifiers = importer
            .as_ref()
            .map(|i| i.supported_modifiers(crate::zerocopy::drm_fourcc(PixelFormat::Bgrx).unwrap()))
            .unwrap_or_default();
        if importer.is_some() && !modifiers.contains(&0) {
            modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
        }
        // PUNKTFUNK_FORCE_SHM=1 forces the race-free download path (SHM, no dmabuf) — required on
        // Mutter+NVIDIA where dmabuf capture has no working sync and shows stale frames. KWin/
        // gamescope don't need it (they blit into the buffer, so no read-before-render race).
        let force_shm = std::env::var("PUNKTFUNK_FORCE_SHM").as_deref() == Ok("1");
        let want_dmabuf = importer.is_some() && !modifiers.is_empty() && !force_shm;
        if force_shm {
            tracing::info!(
                "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
            );
        } else if zerocopy && !want_dmabuf {
            tracing::warn!("zero-copy: no EGL-importable dmabuf modifiers — using CPU path");
        } else if want_dmabuf {
            tracing::info!(
                count = modifiers.len(),
                sample = ?&modifiers[..modifiers.len().min(6)],
                "zero-copy: advertising EGL-importable dmabuf modifiers"
            );
        }

        let data = UserData {
            info: VideoInfoRaw::default(),
            format: None,
            modifier: 0,
            tx,
            active,
            negotiated,
            importer,
            stale_recent: std::collections::VecDeque::new(),
            stale_last: 0,
            stale_seen: 0,
            keep_stale: std::env::var_os("PUNKTFUNK_KEEP_STALE").is_some(),
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
            .state_changed(|_stream, _ud, old, new| {
                tracing::info!(?old, ?new, "pipewire stream state");
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
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                // No active stream: release the buffer without the (expensive at 5K) de-pad.
                if !ud.active.load(Ordering::Relaxed) {
                    return;
                }
                let datas = buffer.datas_mut();
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
                            let imported = if modifier.is_some() {
                                importer.import(&plane, w as u32, h as u32, fourcc, modifier)
                            } else {
                                importer.import_linear(&plane, w as u32, h as u32)
                            };
                            match imported {
                                Ok(devbuf) => {
                                    static ONCE: std::sync::atomic::AtomicBool =
                                        std::sync::atomic::AtomicBool::new(true);
                                    if ONCE.swap(false, Ordering::Relaxed) {
                                        tracing::info!(w, h, modifier = ud.modifier,
                                            "zero-copy: dmabuf imported to CUDA (no CPU copy)");
                                    }
                                    let pts_ns = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .map(|d| d.as_nanos() as u64)
                                        .unwrap_or(0);
                                    let _ = ud.tx.try_send(CapturedFrame {
                                        width: w as u32,
                                        height: h as u32,
                                        pts_ns,
                                        format: fmt,
                                        payload: FramePayload::Cuda(devbuf),
                                    });
                                    return;
                                }
                                Err(e) => {
                                    // GPU import unavailable for this buffer kind (e.g. the
                                    // driver rejects LINEAR external-memory import). Disable
                                    // the importer and fall through to the CPU mmap path —
                                    // degraded, not dead.
                                    tracing::warn!(error = %format!("{e:#}"),
                                        "dmabuf GPU import failed — falling back to the CPU copy path");
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
                let dmabuf_fd =
                    (d.type_() == pw::spa::buffer::DataType::DmaBuf).then(|| d.fd());
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
                // MAP_BUFFERS only maps buffers flagged mappable; Vulkan-exported dmabufs
                // (gamescope) usually aren't, so mmap the fd ourselves for the de-pad read.
                let _mapping; // keeps a manual mmap alive for the copy below
                let buf: &[u8] = if let Some(data) = d.data() {
                    data
                } else if let Some(fd) = dmabuf_fd.filter(|&fd| fd > 0) {
                    match DmabufMap::new(fd, offset + needed) {
                        Some(m) => {
                            _mapping = m;
                            unsafe {
                                std::slice::from_raw_parts(_mapping.ptr as *const u8, _mapping.len)
                            }
                        }
                        None => {
                            warn_once("mmap(dmabuf) failed — frames dropped");
                            return;
                        }
                    }
                } else {
                    warn_once("buffer has no mappable data — frames dropped");
                    return;
                };
                // Need stride*(h-1)+row valid bytes within [offset, offset+size).
                if offset > buf.len() {
                    return;
                }
                let avail = buf.len() - offset;
                if needed > avail || needed > size {
                    warn_once("buffer smaller than frame span — frames dropped");
                    return;
                }
                let region = &buf[offset..offset + size.min(avail)];
                // De-pad into a tightly-packed buffer (chunk stride may exceed w*bpp).
                let mut tight = vec![0u8; row * h];
                for y in 0..h {
                    tight[y * row..y * row + row]
                        .copy_from_slice(&region[y * stride..y * stride + row]);
                }
                // GNOME stale-frame filter: Mutter re-delivers complete OLD pool buffers on the SHM
                // download path (no NVIDIA fence prevents it). A frame whose sampled content equals an
                // EARLIER distinct frame is a stale re-delivery — DROP it so the encoder never emits
                // the flash (try_latest then re-sends the last good frame). Hashes a spatial sample;
                // a real return-to-prior-state is rare and dropping it is harmless. KEEP_STALE=1 = off.
                {
                    let s: &[u8] = &tight;
                    let step = (s.len() / 1024).max(1);
                    let mut hh: u64 = 0xcbf2_9ce4_8422_2325;
                    let mut i = 0;
                    while i < s.len() {
                        hh = (hh ^ s[i] as u64).wrapping_mul(0x0100_0000_01b3);
                        i += step;
                    }
                    // `stale_last` = the current (newest) frame's hash. A frame equal to it is a normal
                    // duplicate (deliver). A frame equal to an OLDER distinct frame in `stale_recent`
                    // is a stale re-delivery (drop). Anything else is new forward content (deliver).
                    if hh == ud.stale_last {
                        // duplicate of the current frame — deliver as usual
                    } else if ud.stale_recent.contains(&hh) {
                        ud.stale_seen += 1;
                        if ud.stale_seen.is_power_of_two() {
                            tracing::warn!(
                                dropped = ud.stale_seen,
                                "GNOME stale-frame dropped (capture reverted to an earlier frame)"
                            );
                        }
                        if !ud.keep_stale {
                            return; // drop the stale re-delivery — don't advance `stale_last`
                        }
                    } else {
                        ud.stale_recent.push_back(hh);
                        if ud.stale_recent.len() > 24 {
                            ud.stale_recent.pop_front();
                        }
                        ud.stale_last = hh;
                    }
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
