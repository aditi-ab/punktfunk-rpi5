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

use super::{CapturedFrame, Capturer, PixelFormat};
use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

/// Live monitor capturer backed by the portal + PipeWire threads.
pub struct PortalCapturer {
    frames: Receiver<CapturedFrame>,
}

impl PortalCapturer {
    pub fn open() -> Result<PortalCapturer> {
        // Portal handshake (async) on its own thread; hands back the PW fd + node id.
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        thread::Builder::new()
            .name("lumen-portal".into())
            .spawn(move || portal_thread(setup_tx))
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
        thread::Builder::new()
            .name("lumen-pipewire".into())
            .spawn(move || {
                if let Err(e) = pipewire::pipewire_thread(fd, node_id, frame_tx) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
                }
            })
            .context("spawn pipewire thread")?;

        Ok(PortalCapturer { frames: frame_rx })
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
                        .set_cursor_mode(CursorMode::Hidden)
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

mod pipewire {
    //! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).

    use super::{CapturedFrame, PixelFormat};
    use anyhow::{Context, Result};
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::os::fd::OwnedFd;
    use std::sync::mpsc::SyncSender;
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
        tx: SyncSender<CapturedFrame>,
    }

    pub fn pipewire_thread(fd: OwnedFd, node_id: u32, tx: SyncSender<CapturedFrame>) -> Result<()> {
        crate::pwinit::ensure_init();

        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
        let core = context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?;

        let data = UserData {
            info: VideoInfoRaw::default(),
            format: None,
            tx,
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
                    tracing::info!(
                        width = sz.width,
                        height = sz.height,
                        spa_format = ?ud.info.format(),
                        mapped = ?ud.format,
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
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let sz = ud.info.size();
                let (w, h) = (sz.width as usize, sz.height as usize);
                if w == 0 || h == 0 {
                    return; // format not negotiated yet
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
                    cpu_bytes: tight,
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

        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("serialize format pod")?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).context("pod from bytes")?];

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
