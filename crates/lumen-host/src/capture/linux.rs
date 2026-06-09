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
//! Cleanup note (M0): the two threads are detached and torn down at process exit. A
//! graceful stop (pipewire `channel` quit + Session close) belongs with the M2 session
//! lifecycle.

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
}

impl PortalCapturer {
    /// `anchored` drives ScreenCast off a RemoteDesktop session (KWin/GNOME) so it inherits the
    /// RemoteDesktop grant and never raises a separate ScreenCast dialog; `false` uses a plain
    /// ScreenCast session (wlroots, which has no RemoteDesktop portal).
    pub fn open(anchored: bool) -> Result<PortalCapturer> {
        // Portal handshake (async) on its own thread; hands back the PW fd + node id.
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        thread::Builder::new()
            .name("lumen-portal".into())
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

        // Frames flow from the pipewire thread over a small bounded channel.
        let (frame_tx, frame_rx) = sync_channel::<CapturedFrame>(8);
        let active = Arc::new(AtomicBool::new(false));
        let active_cb = active.clone();
        let zerocopy = crate::zerocopy::enabled();
        thread::Builder::new()
            .name("lumen-pipewire".into())
            .spawn(move || {
                if let Err(e) =
                    pipewire::pipewire_thread(fd, node_id, frame_tx, active_cb, zerocopy)
                {
                    tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
                }
            })
            .context("spawn pipewire thread")?;

        Ok(PortalCapturer {
            frames: frame_rx,
            active,
        })
    }
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        // First frame can lag behind format negotiation; later frames arrive at ~fps.
        match self.frames.recv_timeout(Duration::from_secs(10)) {
            Ok(frame) => Ok(frame),
            Err(RecvTimeoutError::Timeout) => Err(anyhow!("no PipeWire frame within 10s")),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("PipeWire capture thread ended")),
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
        /// Present when zero-copy is enabled: imports a dmabuf → CUDA device buffer.
        importer: Option<crate::zerocopy::EglImporter>,
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
    fn build_dmabuf_format(modifiers: &[u64]) -> Result<Vec<u8>> {
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
                    width: 1920,
                    height: 1080
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
                pw::spa::utils::Fraction { num: 60, denom: 1 },
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

    pub fn pipewire_thread(
        fd: OwnedFd,
        node_id: u32,
        tx: SyncSender<CapturedFrame>,
        active: Arc<AtomicBool>,
        zerocopy: bool,
    ) -> Result<()> {
        crate::pwinit::ensure_init();

        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
        let core = context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?;

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
        // Modifiers our EGL stack can import for BGRx (the layout KWin gives); if none, we can't
        // negotiate dmabuf and fall back to the shm path.
        let modifiers = importer
            .as_ref()
            .map(|i| i.supported_modifiers(crate::zerocopy::drm_fourcc(PixelFormat::Bgrx).unwrap()))
            .unwrap_or_default();
        let want_dmabuf = importer.is_some() && !modifiers.is_empty();
        if zerocopy && !want_dmabuf {
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
            importer,
        };

        let stream = pw::stream::StreamBox::new(
            &core,
            "lumen-screencast",
            properties! {
                *pw::keys::MEDIA_TYPE     => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE     => "Screen",
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

                // Zero-copy path: if the buffer is a dmabuf and we have an importer, import it
                // into a CUDA device buffer (no CPU touch) and deliver that. Otherwise fall
                // through to the shm de-pad copy below.
                if let (Some(importer), Some(fmt)) = (ud.importer.as_ref(), ud.format) {
                    if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
                        let plane = crate::zerocopy::DmabufPlane {
                            fd: datas[0].fd(),
                            offset: datas[0].chunk().offset(),
                            stride: datas[0].chunk().stride().max(0) as u32,
                        };
                        // 0 (unset/LINEAR) → import with the implicit modifier; a real tiled
                        // modifier (if the producer reported one) → import it explicitly.
                        let modifier = (ud.modifier != 0).then_some(ud.modifier);
                        if let Some(fourcc) = crate::zerocopy::drm_fourcc(fmt) {
                            match importer.import(&plane, w as u32, h as u32, fourcc, modifier) {
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
                                }
                                Err(e) => {
                                    static ONCE: std::sync::atomic::AtomicBool =
                                        std::sync::atomic::AtomicBool::new(true);
                                    if ONCE.swap(false, Ordering::Relaxed) {
                                        tracing::warn!(error = %format!("{e:#}"),
                                            "dmabuf import failed — frames dropped (consider unsetting LUMEN_ZEROCOPY)");
                                    }
                                }
                            }
                        }
                        return;
                    }
                }

                let d = &mut datas[0];
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
                let Some(buf) = d.data() else { return };
                // Need stride*(h-1)+row valid bytes within [offset, offset+size).
                if stride < row || offset > buf.len() {
                    return;
                }
                let avail = buf.len() - offset;
                let needed = stride * (h - 1) + row;
                if needed > avail || needed > size {
                    return;
                }
                let region = &buf[offset..offset + size.min(avail)];
                // De-pad into a tightly-packed buffer (chunk stride may exceed w*bpp).
                let mut tight = vec![0u8; row * h];
                for y in 0..h {
                    tight[y * row..y * row + row]
                        .copy_from_slice(&region[y * stride..y * stride + row]);
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

        // Request raw video in any encoder-mappable layout, any size/framerate.
        let obj = pw::spa::pod::object!(
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
                    width: 1920,
                    height: 1080
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
                pw::spa::utils::Fraction { num: 60, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 240, denom: 1 }
            ),
        );

        // When zero-copy is on, offer ONLY a BGRx dmabuf format with our EGL-importable modifiers
        // (offering shm too makes the compositor pick shm). The modifier list is advertised with
        // DONT_FIXATE so the compositor's allocator chooses one; we re-emit the fixated format in
        // `param_changed` (the two-step DMA-BUF handshake). Otherwise offer the multi-format shm
        // pod and let MAP_BUFFERS map it.
        let shm_values = serialize_pod(obj)?;
        let (dmabuf_values, buffers_values) = if want_dmabuf {
            (
                Some(build_dmabuf_format(&modifiers)?),
                Some(build_dmabuf_buffers()?),
            )
        } else {
            (None, None)
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
