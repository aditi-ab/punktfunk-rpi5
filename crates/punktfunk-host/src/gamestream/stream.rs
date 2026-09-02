//! GameStream video data plane.
//!
//! On RTSP PLAY the client pings [`VIDEO_PORT`]; we learn that UDP endpoint and run
//! capture → encode → [`VideoPacketizer`] → UDP. Source is portal PipeWire
//! (`PUNKTFUNK_VIDEO_SOURCE=portal`), a compositor virtual output (`virtual`), or
//! a synthetic test pattern (default).
//!
//! Runs on its own native thread. Encode, FEC packetize, and paced send are
//! separate threads joined by depth-2 queues. Game lifetime:
//! `design/session-game-lifetime.md`.

use super::video::{FrameType, VideoPacketizer};
use super::VIDEO_PORT;
use crate::capture::{self, Capturer, FastSyntheticCapturer};
use crate::encode::{self, Codec};
use anyhow::{Context, Result};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Negotiated video parameters from the RTSP ANNOUNCE.
#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub packet_size: usize,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// Client's `x-nv-vqos[0].fec.minRequiredFecPackets` — parity floor per FEC block.
    pub min_fec: u8,
    /// Client asked for HDR (`dynamicRangeMode != 0`) and the host can deliver it.
    /// Encoder picks Main10 from the captured (P010) format. Always `false` on SDR hosts.
    pub hdr: bool,
    /// Client's `x-nv-video[0].videoEncoderSlicesPerFrame`. Hardware TV decoders wedge on
    /// multi-slice AUs they did not request. Absent ⇒ 1.
    pub slices: u32,
    /// Client echoed `SS_ENC_VIDEO`; shards are AES-128-GCM-sealed under the `/launch` rikey.
    /// Only true when the host advertised the bit (`gs_video_encryption_offered`).
    pub encrypt_video: bool,
}

/// Pooled capturer plus the three reuse keys: HDR-ness, metadata-cursor mode (both fixed at
/// PipeWire negotiation), and the `capture_monitor` pin (`None` = portal's pick). A mismatch
/// needs a fresh screencast session. The pin is live — without it a reconnect keeps the
/// previous screen (`design/per-monitor-portal-capture.md`).
pub type PooledCapturer = (Box<dyn Capturer>, bool, bool, Option<String>);

/// Slot for the persistent screen capturer, shared with the control plane and reused across
/// streams so a reconnect doesn't open a second (conflicting) screencast session.
pub type CapturerSlot = Arc<std::sync::Mutex<Option<PooledCapturer>>>;

/// A pending client reference-frame-invalidation range (lost `firstFrame..=lastFrame`), set by the
/// control plane and drained by the video thread (see [`AppState::rfi_range`](super::AppState)).
pub type RfiSlot = Arc<std::sync::Mutex<Option<(i64, i64)>>>;

/// Game-lifetime wiring spent by the stream thread (`design/session-game-lifetime.md`).
/// The control plane builds these from live `AppState` at RTSP PLAY; they only exist together.
pub struct GameLifetime {
    /// [`super::AppState::quit`]: a decision may end the game; a drop gets a reconnect window.
    pub quit: Arc<AtomicBool>,
    /// Paired client's cert fingerprint; only it can reclaim the launch. `None` if unread.
    pub fingerprint: Option<String>,
    /// Launching peer's source IP ([`super::LaunchSession::peer_ip`]). Bound when the video
    /// thread learns its UDP endpoint so an off-path LAN peer cannot win the race. `None` = open.
    pub owner_ip: Option<std::net::IpAddr>,
    /// Session A/V ping payload — the half of the endpoint guard a shared-address peer cannot forge.
    pub av_ping: [u8; super::AV_PING_LEN],
    pub on_game_exit: super::OnSessionLost,
}

/// Spawn the video stream thread (idempotent via `running`). Stops when `running` clears.
/// `force_idr` is set by the control stream on a client recovery request; `video_cap` holds
/// the persistent capturer the thread borrows for the stream's duration.
#[allow(clippy::too_many_arguments)]
pub fn start(
    cfg: StreamConfig,
    app: Option<super::apps::AppEntry>,
    running: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    rfi_range: RfiSlot,
    loss: Arc<super::GsLossStats>,
    // Session rikey, only when `cfg.encrypt_video`. Not a `StreamConfig` field: that
    // struct is `Debug`-logged at stream start.
    gcm_key: Option<[u8; 16]>,
    video_cap: CapturerSlot,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    on_lost: super::OnSessionLost,
    // Last act of this thread — `/resume` waits on this counter (`AppState::media_exited`).
    media_exited: Arc<std::sync::atomic::AtomicU64>,
    life: GameLifetime,
) {
    let _ = std::thread::Builder::new()
        .name("punktfunk-video".into())
        .spawn(move || {
            crate::native::boost_thread_priority(true);
            // Hold even for video-only viewers — plane parity with native `LiveSessionGuard`.
            let _sleep = crate::sleep_inhibit::hold();
            tracing::info!(?cfg, "video stream starting");
            // Before `run`: `run` launches the app, and the title's wrapper keys on this marker.
            // RTSP carries no device name, so `client` is empty; hooks key on `plane`.
            let stream_marker = crate::stream_marker::announce(crate::stream_marker::StreamInfo {
                width: cfg.width,
                height: cfg.height,
                refresh_hz: cfg.fps,
                hdr: cfg.hdr,
                client: String::new(),
                launch: app.as_ref().map(|a| a.title.clone()),
                plane: crate::events::Plane::Gamestream,
            });
            let event_client = crate::events::ClientRef {
                name: String::new(),
                fingerprint: None,
                plane: crate::events::Plane::Gamestream,
            };
            crate::events::emit(crate::events::EventKind::ClientConnected {
                client: event_client.clone(),
            });
            // Released when the closure exits so idle clocks are not pinned between sessions.
            #[cfg(target_os = "linux")]
            let _clock_pin = crate::gpuclocks::session_pin();
            let result = run(
                cfg,
                app.as_ref(),
                &running,
                &force_idr,
                &rfi_range,
                &loss,
                gcm_key,
                &video_cap,
                &stats,
                &on_lost,
                &life,
            );
            // Clean return is a stop; error is `error`. Compat has no typed close code.
            let reason = match &result {
                Ok(()) => crate::events::DisconnectReason::Quit,
                Err(_) => crate::events::DisconnectReason::Error,
            };
            if let Err(e) = result {
                tracing::error!(error = %format!("{e:#}"), "video stream failed");
            }
            running.store(false, Ordering::SeqCst);
            // Before `client.disconnected` — native loop event order.
            drop(stream_marker);
            crate::events::emit(crate::events::EventKind::ClientDisconnected {
                client: event_client,
                reason,
            });
            tracing::info!("video stream stopped");
            // After capturer re-pool, lease, marker, events — `/resume` may start successors.
            media_exited.fetch_add(1, Ordering::SeqCst);
        });
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    loss: &super::GsLossStats,
    gcm_key: Option<[u8; 16]>,
    video_cap: &std::sync::Mutex<Option<PooledCapturer>>,
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
    on_lost: &super::OnSessionLost,
    life: &GameLifetime,
) -> Result<()> {
    pf_frame::session_tuning::on_hot_thread();
    // Reject an out-of-range mode before allocating capture/encode buffers.
    encode::validate_dimensions(cfg.codec, cfg.width, cfg.height)
        .context("client-requested video mode")?;
    let sock = UdpSocket::bind(("0.0.0.0", VIDEO_PORT)).context("bind video UDP")?;
    // QoS is after `connect` — Windows qWAVE derives the flow from the connected 5-tuple.
    punktfunk_core::transport::grow_socket_buffers(&sock);
    // Client re-pings until video flows, so a missed early ping is fine.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    tracing::info!(
        port = VIDEO_PORT,
        "video: awaiting client ping to learn endpoint"
    );
    // Owner IP and this session's ping payload — both media planes share `learn_client_endpoint`.
    let client = super::learn_client_endpoint(&sock, "video", life.owner_ip, &life.av_ping)?;
    sock.connect(client)
        .context("connect client video endpoint")?;
    // Guard keeps the Windows qWAVE flow alive for this function's scope (the stream).
    let _qos_flow = punktfunk_core::transport::set_media_qos(
        &sock,
        punktfunk_core::transport::MediaClass::Video,
    );
    tracing::info!(%client, "video: client endpoint learned");
    let client_label = client.ip().to_string();

    // Not pooled: a reconnect at a different resolution needs a freshly-sized output.
    if pf_host_config::config().video_source.as_deref() == Some("virtual") {
        // Before prep, source, and launch — a later stamp would reject the process it is meant to find.
        let fresh_stamp = crate::gamelease::launch_clock();
        let target = resolve_gs_app(app);
        // Moonlight has no resume; relaunch must reprieve the leftover game before anything starts.
        if let Some(t) = target.as_ref() {
            let reprieved =
                crate::gamelease::readopt(life.fingerprint.as_deref(), t.game.id.as_deref());
            if !reprieved.is_empty() {
                tracing::info!(
                    reprieved = reprieved.len(),
                    title = %t.game.title,
                    "gamestream: this client came back for its game — keeping it"
                );
            }
        }
        // Do not start a second copy or mint a stamp the running game could never satisfy.
        // Anonymous / no library id is unrecordable.
        let launch_claim = target.as_ref().map(|t| {
            crate::launchreg::claim(
                life.fingerprint.as_deref(),
                t.game.id.as_deref(),
                fresh_stamp,
            )
        });
        let launch_stamp = launch_claim.as_ref().map_or(fresh_stamp, |c| c.stamp());
        let adopt_launch = launch_claim.as_ref().is_some_and(|c| !c.must_spawn());
        // Before the virtual output opens: HDR toggle / sink switch must land first.
        // Guard drop undoes in reverse, including panic-unwind.
        let mut prep_cmds = app.map(|a| a.prep.clone()).unwrap_or_default();
        if let Some(lib_id) = app.and_then(|a| a.library_id.as_deref()) {
            prep_cmds.extend(crate::library::prep_for(lib_id));
        }
        let mut prep_env = vec![(
            "PF_APP_TITLE".to_string(),
            app.map(|a| a.title.clone()).unwrap_or_default(),
        )];
        // Same `PF_STREAM_*` names as the native plane's prep env and the marker file.
        prep_env.extend(crate::hooks::prep_mode_env(
            cfg.width, cfg.height, cfg.fps, cfg.hdr,
        ));
        let _prep = (!prep_cmds.is_empty()).then(|| crate::hooks::run_prep(&prep_cmds, &prep_env));
        // Re-runnable: the encode loop calls it again on a mid-stream capture loss.
        let (mut capturer, compositor, gamescope_route) =
            open_gs_virtual_source(cfg, app, target.as_ref(), &life.quit)?;
        // Only Linux `launch_is_nested` reads it; gamescope does not exist on Windows.
        #[cfg(not(target_os = "linux"))]
        let _ = &gamescope_route;
        // GameStream holds a real display; without this, Windows `admit` budgets cannot see it.
        // `None` identity is the anonymous slot. Dropped at the end of `run`.
        let _admission_guard = crate::vdisplay::admission::register(
            None,
            (cfg.width, cfg.height, cfg.fps),
            life.quit.clone(),
            "gamestream".to_string(),
        );
        tracing::info!(
            ?compositor,
            app = ?app.map(|a| &a.title),
            w = cfg.width,
            h = cfg.height,
            "video source: virtual display (native client resolution)"
        );
        // Launch now that capture is live, for backends that do not nest via `set_launch_command`.
        // Library id wins over an operator-typed `cmd`. Skip spawn when `adopt_launch`.
        #[allow(unused_mut)]
        let mut spawned_now = false;
        // Windows pid for the lease. `None` when nothing spawned, or the spawn only forwards.
        #[allow(unused_mut)]
        let mut spawned_pid: Option<u32> = None;
        // `GameOnNewLaunch`: Moonlight is cert-paired, so the fingerprint keys the same records.
        if !adopt_launch {
            if let Some(t) = target.as_ref() {
                crate::gamelease::end_others_for_new_launch(
                    life.fingerprint.as_deref(),
                    t.game.id.as_deref(),
                );
            }
        }
        #[cfg(windows)]
        if let Some(t) = target.as_ref() {
            if adopt_launch {
                tracing::info!(
                    title = %t.game.title,
                    "gamestream: this client's copy of this title is already running — not starting \
                     a second one"
                );
            } else {
                let launched = match (t.game.id.as_deref(), t.command.as_deref()) {
                    (Some(id), _) => crate::library::launch_gamestream_library(id).map(Some),
                    (None, Some(cmd)) => crate::library::launch_gamestream_command(cmd).map(Some),
                    (None, None) => Ok(None),
                };
                match launched {
                    Ok(l) => {
                        spawned_pid = l.and_then(|l| l.tracked_pid());
                        spawned_now = true;
                    }
                    Err(e) => {
                        tracing::warn!(title = %t.game.title, error = %e, "gamestream: could not launch app")
                    }
                }
            }
        }
        // Keep the child: it is the liveness signal and the termination-ladder handle.
        // Gamescope bare-spawn already nested the command; launching again would start it twice.
        #[cfg(target_os = "linux")]
        let spawned_launch = match target.as_ref().and_then(|t| t.command.as_deref()) {
            Some(cmd) if adopt_launch => {
                tracing::info!(
                    command = %cmd,
                    "gamestream: this client's copy of this title is already running — not starting \
                     a second one"
                );
                None
            }
            Some(_) if crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref()) => {
                spawned_now = true;
                None
            }
            Some(cmd) => match crate::library::launch_session_command(compositor, cmd) {
                Ok(spawned) => {
                    spawned_now = true;
                    Some(spawned)
                }
                Err(e) => {
                    tracing::warn!(command = %cmd, error = %e, "gamestream: could not launch app");
                    None
                }
            },
            None => None,
        };
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let _ = adopt_launch;
        if let Some(c) = launch_claim.as_ref() {
            if spawned_now {
                c.launched();
            } else if c.must_spawn() {
                c.abandon();
            }
        }

        // Exit ends the session; session end can end the game only if asked, and a drop waits
        // the reconnect window (`design/session-game-lifetime.md`).
        let _game_life = target.as_ref().map(|t| {
            #[cfg(target_os = "linux")]
            let nested = crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref());
            #[cfg(not(target_os = "linux"))]
            let nested = false;
            #[cfg(target_os = "linux")]
            let child = spawned_launch.map(|s| (s.child, s.group_leader));
            #[cfg(not(target_os = "linux"))]
            let child = None;

            let on_exit: crate::gamelease::OnExit = {
                let on_game_exit = life.on_game_exit.clone();
                Box::new(move || {
                    // Read at fire time so a mid-session flip takes effect. The lease still runs.
                    if !crate::session_settings::get().session_on_game_exit {
                        tracing::info!(
                            "the launched game exited, but ending the session on game exit is off — \
                             leaving the stream up"
                        );
                        return;
                    }
                    tracing::info!("the launched game exited — ending the session");
                    // Skip keep-alive linger so the next `/launch` starts clean.
                    on_game_exit();
                })
            };
            let lease = crate::gamelease::open(
                crate::gamelease::LeaseRequest {
                    game: t.game.clone(),
                    // RTSP carries no device name; peer IP is the stats-capture label too.
                    client: client_label.clone(),
                    plane: crate::events::Plane::Gamestream,
                    spec: t.detect.clone(),
                    nested,
                    launcher: t.launcher,
                    child,
                    spawned: spawned_pid,
                    launch_stamp,
                    // Adopted launch keeps the original slot across the handover.
                    procs: launch_claim.as_ref().and_then(|c| c.procs()),
                },
                on_exit,
            );
            // Drops first so the console does not briefly show live and `grace` rows together.
            let published = crate::session_status::publish_gamestream_game(lease.shared());
            (
                published,
                crate::gamelease::SessionGuard::new(
                    lease,
                    life.quit.clone(),
                    life.fingerprint.clone(),
                    // Drop opens the reconnect window the next `/launch` of this title matches.
                    launch_claim,
                ),
            )
        });
        // Re-detect the live compositor so a Desktop↔Game switch is followed in place.
        // WxH is locked at ANNOUNCE — a resolution change cannot follow mid-stream.
        let rebuild =
            || open_gs_virtual_source(cfg, app, target.as_ref(), &life.quit).map(|(c, _, _)| c);
        return stream_body(
            &mut capturer,
            Some(&rebuild),
            // Gamescope capture carries no cursor either way; Mutter embed never paints.
            compositor != crate::vdisplay::Compositor::Gamescope
                && blend_capable_metadata_cursor(&cfg),
            &sock,
            cfg,
            running,
            force_idr,
            rfi_range,
            loss,
            gcm_key,
            stats,
            &client_label,
            on_lost,
        );
    }

    // Reuse gated on HDR + cursor mode + pin. Depth is a PipeWire-negotiation property;
    // mismatch needs a fresh session. Embed the pointer unless this backend composites it.
    let metadata_cursor = blend_capable_metadata_cursor(&cfg);
    // Host-wide pin: without this, Moonlight gets whichever head the portal hands back.
    #[cfg(target_os = "linux")]
    let pinned = crate::vdisplay::capture_monitor();
    #[cfg(not(target_os = "linux"))]
    let pinned: Option<String> = None;
    let pooled = match video_cap.lock().unwrap().take() {
        Some((c, was_hdr, was_meta, ref was_pin))
            if was_hdr == cfg.hdr && was_meta == metadata_cursor && *was_pin == pinned =>
        {
            Some(c)
        }
        Some((c, was_hdr, was_meta, was_pin)) => {
            tracing::info!(
                was_hdr,
                want_hdr = cfg.hdr,
                was_metadata_cursor = was_meta,
                want_metadata_cursor = metadata_cursor,
                was_monitor = was_pin.as_deref().unwrap_or("<portal's pick>"),
                want_monitor = pinned.as_deref().unwrap_or("<portal's pick>"),
                "video source: pooled capturer depth/cursor-mode/monitor mismatch — opening a \
                 fresh screencast session"
            );
            drop(c);
            None
        }
        None => None,
    };
    let mut capturer: Box<dyn Capturer> = match pooled {
        Some(c) => {
            tracing::info!("video source: reusing capturer");
            c
        }
        #[cfg(target_os = "linux")]
        None if pf_host_config::config().video_source.as_deref() == Some("portal")
            && pinned.is_some() =>
        {
            let connector = pinned.as_deref().expect("guarded by the match arm");
            tracing::info!(
                hdr = cfg.hdr,
                metadata_cursor,
                monitor = connector,
                "video source: mirroring the pinned monitor (portal source, host pin)"
            );
            open_gs_mirror_source(connector, cfg, metadata_cursor)
                .with_context(|| format!("mirror the pinned monitor {connector:?}"))?
        }
        None if pf_host_config::config().video_source.as_deref() == Some("portal") => {
            tracing::info!(
                hdr = cfg.hdr,
                metadata_cursor,
                "video source: portal desktop capture"
            );
            capture::open_portal_monitor(cfg.hdr, metadata_cursor)
                .context("open portal capturer")?
        }
        None => {
            tracing::info!("video source: synthetic test pattern");
            Box::new(FastSyntheticCapturer::new(cfg.width, cfg.height))
        }
    };
    capturer.set_active(true);
    // Portal/synthetic: no virtual output to re-detect.
    let result = stream_body(
        &mut capturer,
        None,
        metadata_cursor,
        &sock,
        cfg,
        running,
        force_idr,
        rfi_range,
        loss,
        gcm_key,
        stats,
        &client_label,
        on_lost,
    );
    capturer.set_active(false);
    // Portal terminal states are sticky and this path has no rebuild. Re-pooling a dead
    // capturer wedges the next connect; drop it and pay one fresh screencast session.
    if result.is_ok() && capturer.is_alive() {
        *video_cap.lock().unwrap() = Some((capturer, cfg.hdr, metadata_cursor, pinned));
    } else {
        tracing::info!(
            stream_failed = result.is_err(),
            capturer_alive = capturer.is_alive(),
            "video source: retiring the pooled capturer — the next stream opens a fresh screencast \
             session"
        );
    }
    result
}

/// Capturer on the pinned physical monitor for the portal source
/// (`design/per-monitor-portal-capture.md`). Not [`open_gs_virtual_source`]: a mirror launches
/// nothing and creates no virtual output. A missing monitor fails the stream rather than
/// falling back to another screen.
#[cfg(target_os = "linux")]
fn open_gs_mirror_source(
    connector: &str,
    cfg: StreamConfig,
    metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    // Enumerate against the compositor that is up now — Desktop↔Game may have switched.
    let active = crate::vdisplay::detect_active_session();
    crate::vdisplay::observe_session_instance(&active);
    crate::vdisplay::apply_session_env(&active);
    let compositor = crate::vdisplay::compositor_for_kind(active.kind)
        .map(Ok)
        .unwrap_or_else(crate::vdisplay::detect)
        .context("detect compositor")?;
    // Mirror streams an existing head: no gamescope sub-mode, no route.
    crate::inject::set_backend_id(crate::vdisplay::input_backend_id(compositor));
    let mut vd = crate::vdisplay::open_mirror(compositor, connector)?;
    vd.set_hw_cursor(metadata_cursor);
    // Panel runs at the owner's mode; the client scales. Pass the client's anyway.
    let vout = vd
        .create(punktfunk_core::Mode {
            width: cfg.width,
            height: cfg.height,
            refresh_hz: cfg.fps,
        })
        .context("start mirroring the pinned monitor")?;
    crate::capture::capture_virtual_output(
        vout,
        gs_session_plan(&cfg, metadata_cursor).output_format(),
        crate::session_plan::CaptureBackend::resolve(),
        compositor == crate::vdisplay::Compositor::Kwin,
    )
    .context("attach a capturer to the mirrored monitor")
}

/// Resolved launch: lease identity, detect signals, and the command to run.
struct GsApp {
    game: crate::gamelease::GameRef,
    /// Launcher tile, not a game — the lease stays untracked ([`crate::library::LaunchTarget`]).
    launcher: bool,
    detect: crate::library::DetectSpec,
    /// `Some` on Linux (host runs it). `None` for a Windows library title (launch by id).
    command: Option<String>,
}

/// Resolve a `/launch` catalog entry against the host's own library. The client sends only
/// an appid. `None` = nothing to launch (Desktop, or an unresolvable entry).
fn resolve_gs_app(app: Option<&super::apps::AppEntry>) -> Option<GsApp> {
    let app = app?;
    if let Some(id) = app.library_id.as_deref() {
        match crate::library::resolve_launch(id) {
            Some(t) => {
                return Some(GsApp {
                    game: t.game,
                    launcher: t.launcher,
                    detect: t.detect,
                    command: t.command,
                })
            }
            None => tracing::warn!(
                launch_id = id,
                "requested launch id not in this host's library (or no launch recipe) — ignoring"
            ),
        }
    }
    let cmd = app
        .cmd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())?;
    Some(GsApp {
        launcher: false,
        game: crate::gamelease::GameRef {
            id: None,
            store: None,
            title: if app.title.trim().is_empty() {
                cmd.to_string()
            } else {
                app.title.clone()
            },
        },
        detect: crate::library::spec_from_command(cmd),
        command: Some(cmd.to_string()),
    })
}

/// Cursor-as-metadata only where this session's encode backend composites `frame.cursor`.
/// Shared by the mirror and virtual-output sources so `set_hw_cursor` and `stream_body`'s
/// blend flag cannot drift. GameStream has no cursor channel.
fn blend_capable_metadata_cursor(cfg: &StreamConfig) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        crate::encode::cursor_blend_capable(cfg.codec, cuda_planned, cfg.hdr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        false
    }
}

/// Virtual-display source at the client's mode. Re-run on mid-stream capture loss to follow a
/// Desktop↔Game switch. Does not launch the app — a rebuild must not re-spawn it. The capturer
/// owns the output keepalive; the factory is dropped here.
fn open_gs_virtual_source(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
    // Resolved once by the caller so a rebuild cannot re-resolve to something different.
    launch: Option<&GsApp>,
    quit: &Arc<AtomicBool>,
) -> Result<(
    Box<dyn Capturer>,
    crate::vdisplay::Compositor,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    let (compositor, gamescope_route) = if let Some(c) = app.and_then(|a| a.compositor) {
        // Still resolve a route, or `create` falls through to a bare spawn on a managed box.
        let r = crate::vdisplay::resolve_gamescope_route(c, false);
        (c, r)
    } else {
        // `vdisplay::open` ignores the compositor on Windows; skip Linux `detect()` which bails.
        #[cfg(target_os = "windows")]
        {
            (crate::vdisplay::Compositor::Kwin, None)
        }
        #[cfg(not(target_os = "windows"))]
        {
            crate::vdisplay::cancel_pending_tv_restore();
            let active = crate::vdisplay::detect_active_session();
            // Fold an idle-time Game↔Desktop instance change into the epoch before acquire.
            crate::vdisplay::observe_session_instance(&active);
            crate::vdisplay::apply_session_env(&active);
            // Gate on a resolved command so an unresolvable entry falls back to auto routing.
            let has_launch = launch.and_then(|t| t.command.as_deref()).is_some();
            if crate::vdisplay::wants_dedicated_game_session(has_launch) {
                let c = crate::vdisplay::Compositor::Gamescope;
                crate::inject::set_backend_id(crate::vdisplay::input_backend_id(c));
                (c, crate::vdisplay::resolve_gamescope_route(c, true))
            } else {
                let c = crate::vdisplay::compositor_for_kind(active.kind)
                    .map(Ok)
                    .unwrap_or_else(crate::vdisplay::detect)
                    .context("detect compositor")?;
                crate::inject::set_backend_id(crate::vdisplay::input_backend_id(c));
                (c, crate::vdisplay::resolve_gamescope_route(c, false))
            }
        }
    };
    let mut vd = crate::vdisplay::open(compositor).context("open virtual display")?;
    // Mutter virtual never paints an embedded pointer. Gamescope stays off: no metadata
    // either way, and the request would cost the native-NV12 shape for nothing.
    vd.set_hw_cursor(
        compositor != crate::vdisplay::Compositor::Gamescope && blend_capable_metadata_cursor(&cfg),
    );
    // Per-session, not a process-global env: concurrent sessions must not stomp launch targets.
    vd.set_launch_command(launch.and_then(|t| t.command.clone()));
    // Same reason: a process env let either plane retarget the other's `create`.
    vd.set_gamescope_route(gamescope_route.clone());
    // Register an unread stop flag so a later session can preempt (3 s grace, then force).
    // Anonymous slot 0 — only another slot-0 connect preempts it.
    #[cfg(target_os = "windows")]
    let _idd_setup_guard = matches!(
        crate::session_plan::CaptureBackend::resolve(),
        crate::session_plan::CaptureBackend::IddPush
    )
    .then(|| {
        crate::vdisplay::manager::vdm().begin_idd_setup(
            0,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    });
    let vout = crate::vdisplay::registry::acquire(
        &mut vd,
        punktfunk_core::Mode {
            width: cfg.width,
            height: cfg.height,
            refresh_hz: cfg.fps,
        },
        // Quit App / management stop / game exit skip linger; only a real drop lingers.
        quit.clone(),
        None,
    )
    .context("create virtual output at client resolution")?;
    // Linux virtual-output capture is SDR-only (Mutter RecordVirtual); HDR is portal mirror.
    let mut capturer = capture::capture_virtual_output(
        vout,
        gs_session_plan(
            &cfg,
            compositor != crate::vdisplay::Compositor::Gamescope
                && blend_capable_metadata_cursor(&cfg),
        )
        .output_format(),
        crate::session_plan::CaptureBackend::resolve(),
        compositor == crate::vdisplay::Compositor::Kwin,
    )
    .context("capture virtual output")?;
    capturer.set_active(true);
    Ok((capturer, compositor, gamescope_route))
}

/// Shared [`SessionPlan`](crate::session_plan::SessionPlan) at this plane's shape: 4:2:0,
/// depth 10 only with HDR, no cursor-forward (GameStream has no client cursor channel).
fn gs_session_plan(cfg: &StreamConfig, cursor_blend: bool) -> crate::session_plan::SessionPlan {
    crate::session_plan::SessionPlan::resolve(
        if cfg.hdr { 10 } else { 8 },
        cfg.hdr,
        encode::ChromaFormat::Yuv420,
        cfg.codec,
        cursor_blend,
        false,
        cfg.slices > 1,
    )
}

/// Encoder `bit_depth` from the captured format. Backends key the real profile off `format`;
/// this keeps the argument honest (a hard-coded `8` mislabels a P010 stream).
fn gs_bit_depth(format: crate::capture::PixelFormat) -> u8 {
    use crate::capture::PixelFormat;
    match format {
        PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10 => {
            10
        }
        _ => 8,
    }
}

type PacketBatch = Vec<Vec<u8>>;

/// Send `pkts` with as few syscalls as possible (`sendmmsg`, up to 64 per call). The socket is
/// connected, so no per-message address. Returns an error on the first send failure.
#[cfg(target_os = "linux")]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const CHUNK: usize = 64;
    let fd = sock.as_raw_fd();
    for chunk in pkts.chunks(CHUNK) {
        let mut iovs: Vec<libc::iovec> = chunk
            .iter()
            .map(|p| libc::iovec {
                iov_base: p.as_ptr() as *mut libc::c_void,
                iov_len: p.len(),
            })
            .collect();
        let mut hdrs: Vec<libc::mmsghdr> = iovs
            .iter_mut()
            .map(|iov| {
                // SAFETY: `libc::mmsghdr` is a plain `#[repr(C)]` struct of integers and raw
                // pointers, for which an all-zero bit pattern is valid (null pointers / zero
                // lengths); the fields we rely on (`msg_iov`, `msg_iovlen`) are overwritten on the
                // next two lines before the struct is handed to the kernel.
                let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                h.msg_hdr.msg_iov = iov;
                h.msg_hdr.msg_iovlen = 1;
                h
            })
            .collect();
        let mut off = 0usize;
        while off < hdrs.len() {
            // SAFETY: `fd` is `sock`'s live raw fd (`sock` outlives the call). `hdrs[off..]
            // .as_mut_ptr()` is a live slice of `(hdrs.len() - off)` `mmsghdr`s — exactly the count
            // passed — into which the kernel writes each `msg_len`. Each header's `msg_iov` points
            // into `iovs` (a local that outlives this call, with `msg_iovlen == 1` matching its one
            // entry) and each `iovec.iov_base` points into the `chunk` packet buffers (the caller's
            // `pkts`, alive for the call); the kernel only reads those payloads. Flags 0; the return
            // is error-/progress-checked before advancing `off`.
            let n = unsafe {
                libc::sendmmsg(fd, hdrs[off..].as_mut_ptr(), (hdrs.len() - off) as u32, 0)
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            off += n as usize;
        }
    }
    Ok(())
}

/// Windows USO: equal-size packets of a paced burst go in one `WSASendMsg`. Remainder
/// (USO off, mixed sizes, or the frame's short last packet) is a per-packet `send`.
#[cfg(target_os = "windows")]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    let refs: Vec<&[u8]> = pkts.iter().map(|p| p.as_slice()).collect();
    let n = punktfunk_core::transport::send_uso_all(sock, &refs)?;
    for p in &pkts[n..] {
        sock.send(p)?;
    }
    Ok(())
}

/// One syscall per packet (non-Linux, non-Windows; GameStream hosting does not ship there).
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    for p in pkts {
        sock.send(p)?;
    }
    Ok(())
}

/// One encoded frame handed to the packetizer: AUs plus the 90 kHz RTP timestamp.
/// FEC runs on that thread so it never serializes behind encode.
struct RawFrame {
    /// `(bitstream, type, wire frameIndex)` per AU. The stream loop owns numbering (`au_seq`)
    /// so RFI stays 1:1 with Moonlight across mid-stream encoder rebuilds.
    aus: Vec<(Vec<u8>, FrameType, u32)>,
    ts: u32,
    /// Encode-loop tick. Packetizer stamps `now - cap_at` as wire `frame_processing_latency`
    /// (1/10 ms) — Moonlight's "Host processing latency".
    cap_at: Instant,
}

/// Packetize AUs into data + Reed–Solomon shards, then hand the batch to the paced sender.
/// The send hand-off blocks so backpressure fills the encode queue and the encode loop drops
/// the newest frame rather than stalling. Exits when either neighbor's channel closes.
fn spawn_packetizer(
    rx: std::sync::mpsc::Receiver<RawFrame>,
    tx: std::sync::mpsc::SyncSender<PacketBatch>,
    // Recycled batches. `try_recv` only — an empty return channel means allocate fresh.
    pool_rx: std::sync::mpsc::Receiver<PacketBatch>,
    mut pk: VideoPacketizer,
    // Applied between frames, never mid-AU (block geometry + wire percent stay in step).
    fec_pct_live: Arc<std::sync::atomic::AtomicU8>,
    goodput: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("punktfunk-pkt".into())
        .spawn(move || {
            crate::native::boost_thread_priority(false);
            let mut shells: Vec<PacketBatch> = Vec::new();
            let mut cur_pct = fec_pct_live.load(std::sync::atomic::Ordering::Relaxed);
            while let Ok(frame) = rx.recv() {
                let pct = fec_pct_live.load(std::sync::atomic::Ordering::Relaxed);
                if pct != cur_pct {
                    pk.set_fec_percent(pct);
                    cur_pct = pct;
                }
                while let Ok(mut spent) = pool_rx.try_recv() {
                    pk.recycle(&mut spent);
                    shells.push(spent);
                }
                let mut batch = shells.pop().unwrap_or_default();
                // Wire header, 1/10 ms, saturating — a stall reports max rather than wrapping.
                let processing_100us =
                    u16::try_from(frame.cap_at.elapsed().as_micros() / 100).unwrap_or(u16::MAX);
                for (au, ft, idx) in frame.aus {
                    pk.packetize_into(&mut batch, &au, ft, frame.ts, Some(idx), processing_100us);
                }
                if batch.is_empty() {
                    continue;
                }
                let bytes: u64 = batch.iter().map(|p| p.len() as u64).sum();
                if tx.send(batch).is_err() {
                    break;
                }
                goodput.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .context("spawn packetizer thread")?;
    Ok(())
}

/// Paced send thread. A normal-size frame leaves whole via
/// [`auto_burst_bytes`](crate::send_pacing::auto_burst_bytes); only IDR / scene-change overflow
/// spreads at ~3× stream rate, bounded to ~2 frame intervals. Chunking is ≤ 12 steps, chunk ≥
/// 16: every paced step ends in `thread::sleep` whose overshoot must stay bitrate-independent.
/// Send failure ends the whole session via `on_lost` — audio would otherwise keep streaming at
/// the dead endpoint.
#[allow(clippy::too_many_arguments)]
fn spawn_sender(
    sock: UdpSocket,
    rx: std::sync::mpsc::Receiver<PacketBatch>,
    frame_interval: Duration,
    // ~3× the derived encoder rate, live. `0` = `PUNKTFUNK_PACE_FACTOR=0` (legacy spread).
    pace_rate_bps: Arc<std::sync::atomic::AtomicU64>,
    pool_tx: std::sync::mpsc::SyncSender<PacketBatch>,
    spread_us: Arc<std::sync::Mutex<Vec<u32>>>,
    running: Arc<AtomicBool>,
    on_lost: super::OnSessionLost,
) -> Result<()> {
    std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn(move || {
            crate::native::boost_thread_priority(false);
            let mut sent: u64 = 0;
            let mut dropped: u64 = 0;
            while let Ok(mut batch) = rx.recv() {
                dropped += crate::send_pacing::inject_video_drop(&mut batch);
                if batch.is_empty() {
                    continue;
                }
                let wire_bytes: usize = batch.iter().map(|p| p.len()).sum();
                let pace_rate = pace_rate_bps.load(Ordering::Relaxed);
                let burst_bytes = crate::send_pacing::auto_burst_bytes(pace_rate, wire_bytes);
                let cfg = crate::send_pacing::PaceCfg {
                    burst_bytes: Some(burst_bytes),
                    chunk: crate::send_pacing::ChunkPolicy::Bounded {
                        min_chunk: 16,
                        max_steps: 12,
                    },
                    sleep_floor: Duration::from_micros(500),
                };
                let overflow_bytes = wire_bytes.saturating_sub(burst_bytes) as u64;
                let budget = crate::send_pacing::native_budget(
                    Instant::now() + frame_interval,
                    pace_rate,
                    overflow_bytes,
                    frame_interval * 2,
                );
                let r = crate::send_pacing::pace_frame(&batch, budget, &cfg, |chunk| {
                    sendmmsg_all(&sock, chunk)?;
                    sent += chunk.len() as u64;
                    Ok::<(), std::io::Error>(())
                });
                match r {
                    Ok(stat) => {
                        // A stalled reader must not grow this unbounded.
                        let mut v = spread_us.lock().unwrap_or_else(|p| p.into_inner());
                        if v.len() < 1024 {
                            v.push(stat.spread_us);
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = %e, sent, "video: client unreachable — ending session");
                        running.store(false, Ordering::SeqCst);
                        on_lost();
                        return;
                    }
                }
                let _ = pool_tx.try_send(batch);
            }
            tracing::debug!(sent, dropped, "video sender exiting");
        })
        .context("spawn send thread")?;
    Ok(())
}

use crate::send_pacing::percentile;

/// Ignore further IDR requests after emitting one. Floor is 100 ms: `frame_interval * 2` is
/// 16.7 ms at 120 fps, while a client under loss re-asks every ~30 ms — every request would
/// pass and the IDR storm would feed the loss that prompts the next request.
fn keyframe_coalesce_window(frame_interval: Duration) -> Duration {
    (frame_interval * 2).max(Duration::from_millis(100))
}

/// Encode loop over a borrowed capturer. Send is a dedicated thread so a send spike cannot
/// stall capture/encode.
#[allow(clippy::too_many_arguments)]
fn stream_body(
    // `&mut Box` so a capture-loss rebuild can swap the capturer in place.
    capturer: &mut Box<dyn Capturer>,
    // Virtual-display: re-open on capture loss. `None` for portal/synthetic — propagate.
    rebuild: Option<&dyn Fn() -> Result<Box<dyn Capturer>>>,
    // Encoder composites cursor bitmaps. `false` = pointer is embedded (or absent).
    cursor_blend: bool,
    sock: &UdpSocket,
    cfg: StreamConfig,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    // Client 0x0201 loss counters — 1 Hz adaptation reads deltas.
    loss: &super::GsLossStats,
    gcm_key: Option<[u8; 16]>,
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
    client_label: &str,
    on_lost: &super::OnSessionLost,
) -> Result<()> {
    let mut frame = capturer.next_frame().context("capture first frame")?;
    if frame.width != cfg.width || frame.height != cfg.height {
        // Not fatal. Expected for a mirror (panel's own mode); a fault on a virtual display
        // created at the negotiated size. Encoder opens at the captured size.
        tracing::warn!(
            captured = ?(frame.width, frame.height),
            negotiated = ?(cfg.width, cfg.height),
            "captured size != negotiated size — the client decodes a stream that disagrees with \
             what it negotiated (expected when mirroring a monitor; a virtual-display backend fault \
             otherwise — see the vdisplay lines above)"
        );
    }
    // Sunshine default 20. `PUNKTFUNK_FEC_PCT=0` is data-only. Read before the encoder opens:
    // encoder rate is derived under this parity.
    let fec_pct: u8 = std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    // Client bitrate is a wire budget: parity + framing fit inside it. `mut` because 1 Hz
    // adaptation re-derives; mid-stream rebuilds reopen at the live rate.
    let mut enc_bps = gs_encoder_bps(cfg.bitrate_kbps, fec_pct, cfg.packet_size);
    // `PUNKTFUNK_GS_ADAPT=0` pins FEC and budget at the configured values.
    let mut adapt = GsAdapt::new(fec_pct, cfg.bitrate_kbps);
    let mut adapt_lost_seen: u64 = 0;
    // Software paths refuse in-place retarget; raising FEC then overshoots the budget.
    let mut adapt_supported = true;
    let mut enc = encode::open_video(
        cfg.codec,
        frame.format,
        frame.width,
        frame.height,
        cfg.fps,
        enc_bps,
        frame.is_cuda(),
        gs_bit_depth(frame.format),
        // Stock Moonlight cannot decode 4:4:4.
        encode::ChromaFormat::Yuv420,
        cursor_blend,
        cfg.slices,
    )
    .context("open video encoder for stream")?;
    // Without this, an in-place backend bounds itself by an env cap and the capturer rotates
    // a texture out from under a live encode — torn frames, never an error.
    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    // Source can change size/format under this loop with nothing negotiating it.
    let mut enc_src = (frame.format, frame.width, frame.height);
    let mut pk = VideoPacketizer::new(cfg.packet_size, fec_pct, cfg.min_fec);
    if cfg.encrypt_video {
        match gcm_key {
            Some(key) => pk.set_encryption_key(key),
            // Streaming plaintext to a client expecting ciphertext is a black screen.
            None => anyhow::bail!("SS_ENC_VIDEO negotiated but the session key is gone"),
        }
    }

    // Compositors emit on damage; re-encode the last frame or a static desktop starves the client.
    let target_fps = cfg.fps.clamp(1, 240);
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let mut fps_count: u32 = 0;
    let mut fps_t = Instant::now();
    let stream_start = Instant::now();
    let mut sent_batches: u64 = 0;
    let mut dropped_batches: u64 = 0;

    // Depth-2 queues: a slow stage buffers one frame; beyond that the newest drops (FEC/RFI).
    let goodput = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
    // Depth 4 > the two depth-2 queues combined, so a batch always has a return slot.
    let (pool_tx, pool_rx) = std::sync::mpsc::sync_channel::<PacketBatch>(4);
    let spread_us = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    // 3× default: the link carries 1× sustained, so a bounded 3× excursion is safe.
    // `PUNKTFUNK_PACE_FACTOR=0` restores the legacy deadline-fraction spread.
    let pace_factor: f64 = std::env::var("PUNKTFUNK_PACE_FACTOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|f: &f64| f.is_finite() && *f >= 0.0)
        .unwrap_or(3.0);
    let pace_rate_bps = Arc::new(std::sync::atomic::AtomicU64::new(
        (enc_bps as f64 * pace_factor) as u64,
    ));
    let fec_pct_live = Arc::new(std::sync::atomic::AtomicU8::new(fec_pct));
    spawn_sender(
        sock.try_clone().context("clone video socket")?,
        batch_rx,
        Duration::from_secs_f64(1.0 / target_fps as f64),
        pace_rate_bps.clone(),
        pool_tx,
        spread_us.clone(),
        running.clone(),
        on_lost.clone(),
    )?;
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawFrame>(2);
    spawn_packetizer(
        raw_rx,
        batch_tx,
        pool_rx,
        pk,
        fec_pct_live.clone(),
        goodput.clone(),
    )?;

    let perf = pf_host_config::config().perf;
    let (mut mx_cap, mut mx_enc, mut mx_pkt, mut mx_send, mut uniq) =
        (0u128, 0u128, 0u128, 0u128, 0u32);
    let codec_name = cfg.codec.label();
    let mut sid: Option<u32> = None;
    let (mut v_cap, mut v_enc, mut v_pkt, mut v_send): (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut last_dropped_batches: u64 = 0;
    let mut next_frame = Instant::now();
    // Loop-local so a mid-stream rebuild cannot reopen the overshoot.
    let mut cap_credit = crate::send_pacing::CaptureCredit::new(Instant::now());
    // Session-fixed; query once so encoders without NVENC RFI skip the always-false invalidate.
    let mut supports_rfi = enc.caps().supports_rfi;

    // A delivered frame clears this; a permanently dead source ends the stream after the cap.
    const MAX_REBUILDS: u32 = 5;
    let mut rebuilds: u32 = 0;
    // Submit/poll failure or a silent stall rebuilds in place (native `reset_stalled_encoder`).
    // `last_au_at` is the silent-wedge watchdog: poll returning `None` forever never errors.
    const MAX_ENCODER_RESETS: u32 = 5;
    let mut encoder_resets: u32 = 0;
    let mut last_au_at = Instant::now();

    // Without RFI each request is a full IDR. One IDR resolves pending loss; NVENC
    // invalidate is never rate-limited.
    let keyframe_coalesce = keyframe_coalesce_window(frame_interval);
    let mut last_keyframe: Option<Instant> = None;
    // A pipeline-head drop consumes no frameIndex; the client cannot see the gap. Arm an IDR
    // through the same coalesce gate so a burst of drops cannot become an IDR storm.
    let mut recover_after_drop = false;
    // Wire frameIndex owned here. `submit_indexed(au_seq + enc_inflight)` keeps RFI 1:1
    // with Moonlight across in-place rebuilds (an internal counter would desync).
    let mut au_seq: u32 = 0;
    let mut enc_inflight: u32 = 0;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();
        let measure = perf || stats.is_armed();
        match capturer.try_latest() {
            Ok(Some(f)) => {
                frame = f;
                uniq += 1;
                rebuilds = 0;
            }
            Ok(None) => {}
            Err(e) => {
                // Rebuild in place (send/packetizer/socket/RTP survive). No rebuild → propagate.
                let Some(rebuild) = rebuild else {
                    return Err(e).context("capture frame");
                };
                rebuilds += 1;
                if rebuilds > MAX_REBUILDS {
                    return Err(e).context("capture lost — rebuild attempts exhausted");
                }
                tracing::warn!(error = %format!("{e:#}"), rebuild = rebuilds,
                    "gamestream: capture lost — rebuilding source in place (following a session switch)");
                let rebuild_deadline = Instant::now() + Duration::from_secs(40);
                let new_cap = loop {
                    match rebuild() {
                        Ok(c) => break c,
                        Err(e2) => {
                            if !running.load(Ordering::SeqCst) || Instant::now() >= rebuild_deadline
                            {
                                return Err(e2)
                                    .context("capture lost — no source within the rebuild budget");
                            }
                            tracing::warn!(error = %format!("{e2:#}"),
                                "gamestream: source not up yet — retrying");
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                };
                *capturer = new_cap;
                capturer.set_active(true);
                frame = capturer.next_frame().context("first frame after rebuild")?;
                enc = encode::open_video(
                    cfg.codec,
                    frame.format,
                    frame.width,
                    frame.height,
                    cfg.fps,
                    enc_bps,
                    frame.is_cuda(),
                    gs_bit_depth(frame.format),
                    encode::ChromaFormat::Yuv420,
                    cursor_blend,
                    cfg.slices,
                )
                .context("reopen encoder after rebuild")?;
                enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
                enc_src = (frame.format, frame.width, frame.height);
                supports_rfi = enc.caps().supports_rfi;
                enc.request_keyframe();
                last_keyframe = Some(Instant::now());
                next_frame = Instant::now();
                // Old encoder died with in-flight AUs; numbering restarts at `au_seq`.
                enc_inflight = 0;
                tracing::info!("gamestream: source rebuilt — stream continues");
                continue;
            }
        }
        let t_cap = tick.elapsed();
        // Source changed size/format with nothing negotiating it. The encoder cannot follow a
        // resolution change in place; reopen at the delivered size. GameStream has no mid-stream
        // mode message — the client is not told.
        if enc_src != (frame.format, frame.width, frame.height) {
            match encode::open_video(
                cfg.codec,
                frame.format,
                frame.width,
                frame.height,
                cfg.fps,
                enc_bps,
                frame.is_cuda(),
                gs_bit_depth(frame.format),
                encode::ChromaFormat::Yuv420,
                cursor_blend,
                cfg.slices,
            ) {
                Ok(e) => {
                    tracing::info!(
                        from = %format!("{}x{} {:?}", enc_src.1, enc_src.2, enc_src.0),
                        to = %format!("{}x{} {:?}", frame.width, frame.height, frame.format),
                        negotiated = ?(cfg.width, cfg.height),
                        "gamestream: the capture source changed mode mid-stream — reopened the \
                         encoder at the delivered size (the client is not told; a strict decoder \
                         may not follow — see the note at this guard)"
                    );
                    enc = e;
                    enc_src = (frame.format, frame.width, frame.height);
                    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
                    supports_rfi = enc.caps().supports_rfi;
                    enc.request_keyframe();
                    last_keyframe = Some(Instant::now());
                    // Old encoder died with in-flight AUs; numbering restarts at `au_seq`.
                    enc_inflight = 0;
                    encoder_resets = 0;
                    last_au_at = Instant::now();
                }
                Err(e) => {
                    // First failed open is a settling driver; spend the shared reset budget.
                    encoder_resets += 1;
                    if encoder_resets > MAX_ENCODER_RESETS {
                        return Err(e).context("reopen encoder at the source's new mode");
                    }
                    let backoff = frame_interval
                        .max(Duration::from_millis(100u64 << (encoder_resets - 1).min(4)));
                    tracing::warn!(error = %format!("{e:#}"), reset = encoder_resets,
                        max = MAX_ENCODER_RESETS,
                        "gamestream: reopening the encoder at the source's new mode failed — retrying");
                    next_frame = Instant::now() + backoff;
                    std::thread::sleep(backoff);
                    continue;
                }
            }
        }
        let mut want_keyframe = recover_after_drop;
        if let Some((first, last)) = rfi_range.lock().unwrap().take() {
            // Wider than RFI_MAX_RANGE is a phantom range — keyframe, never a force-reference.
            let width = (last as u32).wrapping_sub(first as u32);
            if width > punktfunk_core::packet::RFI_MAX_RANGE
                || !(supports_rfi && enc.invalidate_ref_frames(first, last))
            {
                want_keyframe = true;
            }
        }
        if force_idr.swap(false, Ordering::SeqCst) {
            want_keyframe = true;
        }
        if want_keyframe {
            let now = Instant::now();
            let emit = match last_keyframe {
                Some(t) => now.duration_since(t) >= keyframe_coalesce,
                None => true,
            };
            if emit {
                enc.request_keyframe();
                last_keyframe = Some(now);
                // Satisfied only by an emitted IDR. Coalesced away it is never retried and
                // leaves duplicate wire indices for a later RFI to anchor on.
                recover_after_drop = false;
            } else {
                tracing::debug!("video: keyframe request coalesced (IDR still in flight)");
            }
        }
        // Stock Moonlight tone-maps from in-band mastering/CLL SEI on keyframes. `None` is a no-op.
        enc.set_hdr_meta(capturer.hdr_meta());
        if let Err(e) = enc.submit_indexed(&frame, au_seq.wrapping_add(enc_inflight)) {
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS || !enc.reset() {
                tracing::error!(
                    error = %format!("{e:#}"),
                    resets = encoder_resets,
                    "encoder did not recover after repeated in-place rebuilds — ending the \
                     stream (see the error above for the cause)"
                );
                return Err(e).context("encoder submit");
            }
            // Owed AUs died with the discarded state. IDR bypasses coalesce: the client must resync.
            enc_inflight = 0;
            enc.request_keyframe();
            last_keyframe = Some(Instant::now());
            last_au_at = Instant::now();
            tracing::warn!(error = %format!("{e:#}"), reset = encoder_resets,
                max = MAX_ENCODER_RESETS,
                "encoder submit failed — encoder rebuilt in place, forcing an IDR");
            // Five instant retries burn out inside one driver hiccup.
            let backoff =
                frame_interval.max(Duration::from_millis(100u64 << (encoder_resets - 1).min(4)));
            next_frame = Instant::now() + backoff;
            std::thread::sleep(backoff);
            continue;
        }
        enc_inflight = enc_inflight.wrapping_add(1);
        let t_enc = tick.elapsed();

        // 90 kHz RTP from wall-clock so a variable capture rate stays correct.
        let ts = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
        let mut aus: Vec<(Vec<u8>, FrameType, u32)> = Vec::new();
        // Carry a poll error to stall recovery after already-drained AUs are handed off.
        let mut poll_err: Option<anyhow::Error> = None;
        loop {
            let au = match enc.poll() {
                Ok(Some(au)) => au,
                Ok(None) => break,
                Err(e) => {
                    poll_err = Some(e);
                    break;
                }
            };
            let ft = if au.keyframe {
                FrameType::Idr
            } else {
                FrameType::P
            };
            let idx = au_seq.wrapping_add(aus.len() as u32);
            aus.push((au.data, ft, idx));
            enc_inflight = enc_inflight.saturating_sub(1);
            last_au_at = Instant::now();
            encoder_resets = 0;
        }
        let t_pkt = tick.elapsed();

        // Never block: a full queue drops this frame (FEC/RFI covers) so encode is never capped.
        if !aus.is_empty() {
            let batch_len = aus.len() as u32;
            match raw_tx.try_send(RawFrame {
                aus,
                ts,
                cap_at: tick,
            }) {
                Ok(()) => {
                    sent_batches += 1;
                    au_seq = au_seq.wrapping_add(batch_len);
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    dropped_batches += 1;
                    recover_after_drop = true;
                    if dropped_batches.is_power_of_two() {
                        tracing::warn!(
                            dropped_batches,
                            "video: pipeline queue full — frame dropped"
                        );
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    break;
                }
            }
        }
        // Poll error, or no AU while frames are owed. Window scales so low-fps cannot false-trip.
        let stall_window = Duration::from_secs(2).max(frame_interval * 8);
        if poll_err.is_some() || (enc_inflight > 0 && last_au_at.elapsed() >= stall_window) {
            let why = match &poll_err {
                Some(e) => format!("poll failed: {e:#}"),
                None => format!(
                    "no AU for {} ms with {} frame(s) owed",
                    last_au_at.elapsed().as_millis(),
                    enc_inflight
                ),
            };
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS || !enc.reset() {
                return Err(poll_err.unwrap_or_else(|| anyhow::anyhow!("{why}")))
                    .context("encoder stalled — in-place rebuild unavailable or exhausted");
            }
            enc_inflight = 0;
            enc.request_keyframe();
            last_keyframe = Some(Instant::now());
            last_au_at = Instant::now();
            tracing::warn!(reset = encoder_resets, max = MAX_ENCODER_RESETS, %why,
                "encode stall detected — encoder rebuilt in place, forcing an IDR");
            let backoff =
                frame_interval.max(Duration::from_millis(100u64 << (encoder_resets - 1).min(4)));
            next_frame = Instant::now() + backoff;
            std::thread::sleep(backoff);
            continue;
        }
        if measure {
            let t_send = tick.elapsed();
            let cap_us = t_cap.as_micros();
            let enc_us = (t_enc - t_cap).as_micros();
            // Both should be small; if not, a full queue is stalling encode.
            let poll_us = (t_pkt - t_enc).as_micros();
            let enqueue_us = (t_send - t_pkt).as_micros();
            mx_cap = mx_cap.max(cap_us);
            mx_enc = mx_enc.max(enc_us);
            mx_pkt = mx_pkt.max(poll_us);
            mx_send = mx_send.max(enqueue_us);
            v_cap.push(cap_us as u32);
            v_enc.push(enc_us as u32);
            v_pkt.push(poll_us as u32);
            v_send.push(enqueue_us as u32);
        }

        fps_count += 1;
        if fps_t.elapsed() >= Duration::from_secs(1) {
            let secs = fps_t.elapsed().as_secs_f64();
            let win_bytes = goodput.swap(0, std::sync::atomic::Ordering::Relaxed);
            // Drain every window so the sender's bounded push buffer stays fresh.
            let mut v_spread =
                std::mem::take(&mut *spread_us.lock().unwrap_or_else(|p| p.into_inner()));
            if perf {
                tracing::info!(
                    fps = fps_count,
                    uniq,
                    enc_us = mx_enc,
                    pkt_us = mx_pkt,
                    send_us = mx_send,
                    cap_us = mx_cap,
                    "video: streaming (perf)"
                );
            } else {
                tracing::debug!(
                    fps = fps_count,
                    sent_batches,
                    dropped_batches,
                    "video: streaming"
                );
            }
            // Host send side has no receiver-side loss / FEC-recovery / EAGAIN counters; leave 0.
            if stats.is_armed() {
                let session_id = *sid.get_or_insert_with(|| {
                    stats.register_session(
                        "gamestream",
                        cfg.width,
                        cfg.height,
                        cfg.fps,
                        codec_name,
                        client_label,
                    )
                });
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0,
                    session_id,
                    stages: vec![
                        crate::stats_recorder::StageTiming {
                            name: "capture".into(),
                            p50_us: percentile(&mut v_cap, 0.50) as f32,
                            p99_us: percentile(&mut v_cap, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "encode".into(),
                            p50_us: percentile(&mut v_enc, 0.50) as f32,
                            p99_us: percentile(&mut v_enc, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "packetize".into(),
                            p50_us: percentile(&mut v_pkt, 0.50) as f32,
                            p99_us: percentile(&mut v_pkt, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "send".into(),
                            p50_us: percentile(&mut v_send, 0.50) as f32,
                            p99_us: percentile(&mut v_send, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "send_spread".into(),
                            p50_us: percentile(&mut v_spread, 0.50) as f32,
                            p99_us: percentile(&mut v_spread, 0.99) as f32,
                        },
                    ],
                    fps: (uniq as f64 / secs) as f32,
                    repeat_fps: (fps_count.saturating_sub(uniq) as f64 / secs) as f32,
                    mbps: (win_bytes as f64 * 8.0 / secs / 1_000_000.0) as f32,
                    // Live wire budget, not the client's ask.
                    bitrate_kbps: adapt.budget_kbps,
                    frames_dropped: dropped_batches.saturating_sub(last_dropped_batches) as u32,
                    packets_dropped: 0,
                    send_dropped: 0,
                    fec_recovered: 0,
                };
                stats.push_sample(session_id, sample);
            }
            // Wire never exceeds the live budget. A refused in-place retarget disables
            // adaptation: raising FEC with a frozen encoder rate would overshoot.
            if adapt_supported && gs_adapt_enabled() {
                let lost_total = loss.lost.load(std::sync::atomic::Ordering::Relaxed);
                let lost_delta = lost_total.saturating_sub(adapt_lost_seen);
                adapt_lost_seen = lost_total;
                if adapt.step(lost_delta) {
                    let new_enc = gs_encoder_bps(adapt.budget_kbps, adapt.fec_pct, cfg.packet_size);
                    if enc.reconfigure_bitrate(new_enc) {
                        enc_bps = new_enc;
                        fec_pct_live.store(adapt.fec_pct, std::sync::atomic::Ordering::Relaxed);
                        pace_rate_bps.store(
                            (new_enc as f64 * pace_factor) as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        tracing::info!(
                            lost = lost_delta,
                            fec_pct = adapt.fec_pct,
                            budget_kbps = adapt.budget_kbps,
                            enc_bps = new_enc,
                            "gamestream: adapted FEC/bitrate to client-reported loss"
                        );
                    } else {
                        adapt = GsAdapt::new(fec_pct, cfg.bitrate_kbps);
                        adapt_supported = false;
                        tracing::info!(
                            "gamestream: encoder can't retarget in place — loss adaptation off \
                             for this session (FEC/bitrate stay at the configured values)"
                        );
                    }
                }
            }
            mx_cap = 0;
            mx_enc = 0;
            mx_pkt = 0;
            mx_send = 0;
            uniq = 0;
            v_cap.clear();
            v_enc.clear();
            v_pkt.clear();
            v_send.clear();
            last_dropped_batches = dropped_batches;
            fps_count = 0;
            fps_t = Instant::now();
        }
        // Absolute clock. Behind a slow frame: resync to now rather than bursting to catch up.
        next_frame += frame_interval;
        if crate::send_pacing::frame_driven_enabled() && capturer.supports_arrival_wait() {
            // 0.9× floor leaves jitter headroom; credit pins the long-run average so a faster
            // mirrored panel cannot overdrive the wire. +0.5× deadline keeps static-desktop
            // re-encode at ~1.5×interval (client liveness).
            cap_credit.charge();
            let earliest = std::cmp::max(
                tick + frame_interval.mul_f32(0.9),
                cap_credit.earliest(Instant::now(), frame_interval),
            );
            if let Some(d) = earliest.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            capturer.wait_arrival(tick + frame_interval.mul_f32(1.5));
            // Arrivals are the clock; re-anchor so a rebuild back to fixed cadence stays sane.
            next_frame = Instant::now() + frame_interval;
        } else {
            match next_frame.checked_duration_since(Instant::now()) {
                Some(d) => std::thread::sleep(d),
                None => next_frame = Instant::now(),
            }
        }
    }
    Ok(())
}

/// Encoder rate under the client's wire budget: 32 B framing per `packetSize + 16 − 32`
/// payload bytes plus FEC parity. No audio reservation (Moonlight bitrate is video-only).
/// Floor 500 kbps: a degenerate ask still streams.
fn gs_encoder_bps(bitrate_kbps: u32, fec_pct: u8, packet_size: usize) -> u64 {
    let pps = (packet_size + 16).saturating_sub(32).max(1) as u64;
    let blocksize = pps + 32;
    let video = bitrate_kbps as u64 * 1000 * pps * 100 / (blocksize * (100 + fec_pct as u64));
    video.max(500_000)
}

/// `PUNKTFUNK_GS_ADAPT=0` pins the GameStream plane's loss adaptation off — FEC percent and
/// wire budget stay at their configured values for the whole session (the A/B lever).
fn gs_adapt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_GS_ADAPT").as_deref() != Ok("0"))
}

/// Loss-driven FEC percent and wire budget, stepped ~1 s from client `0x0201` reports.
/// FEC climbs `+max(5, pct/2)` per lossy window (cap [`Self::FEC_MAX`]) and decays −5 per
/// 8 clean (floor `base_pct`). Budget de-rates ×0.85 from the 2nd consecutive lossy window
/// (floor max(¼ cap, 5 Mbps)) and climbs cap/20 per 4 clean. Apply site re-derives the
/// encoder rate under (budget, percent) so the wire never exceeds the live budget.
#[derive(Clone, Copy, Debug)]
struct GsAdapt {
    /// Decay floor.
    base_pct: u8,
    /// Recovery ceiling (client negotiated kbps).
    cap_kbps: u32,
    fec_pct: u8,
    budget_kbps: u32,
    clean: u32,
    lossy: u32,
}

impl GsAdapt {
    const FEC_MAX: u8 = 50;

    fn new(base_pct: u8, cap_kbps: u32) -> GsAdapt {
        GsAdapt {
            base_pct,
            cap_kbps,
            fec_pct: base_pct,
            budget_kbps: cap_kbps,
            clean: 0,
            lossy: 0,
        }
    }

    /// Fold one window's client-reported loss in. Returns whether either lever moved.
    fn step(&mut self, lost_delta: u64) -> bool {
        let before = (self.fec_pct, self.budget_kbps);
        if lost_delta > 0 {
            self.clean = 0;
            self.lossy += 1;
            self.fec_pct = self
                .fec_pct
                .saturating_add((self.fec_pct / 2).max(5))
                .min(Self::FEC_MAX);
            if self.lossy >= 2 {
                let floor = (self.cap_kbps / 4).max(5_000).min(self.cap_kbps);
                self.budget_kbps = ((self.budget_kbps as u64 * 85 / 100) as u32).max(floor);
            }
        } else {
            self.lossy = 0;
            self.clean += 1;
            if self.clean % 8 == 0 && self.fec_pct > self.base_pct {
                self.fec_pct = self.fec_pct.saturating_sub(5).max(self.base_pct);
            }
            if self.clean % 4 == 0 && self.budget_kbps < self.cap_kbps {
                self.budget_kbps = self
                    .budget_kbps
                    .saturating_add((self.cap_kbps / 20).max(500))
                    .min(self.cap_kbps);
            }
        }
        (self.fec_pct, self.budget_kbps) != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, cmd: Option<&str>) -> super::super::apps::AppEntry {
        super::super::apps::AppEntry {
            id: 1,
            title: title.to_string(),
            compositor: None,
            cmd: cmd.map(str::to_string),
            library_id: None,
            prep: Vec::new(),
        }
    }

    /// Desktop / no command must take no lease — a plain desktop stream stays inert.
    #[test]
    fn only_an_entry_that_launches_something_gets_tracked() {
        assert!(resolve_gs_app(None).is_none());
        assert!(resolve_gs_app(Some(&entry("Desktop", None))).is_none());
        assert!(resolve_gs_app(Some(&entry("Blank", Some("   ")))).is_none());

        // Operator-typed command: title is the whole identity; no store id for box art.
        let t = resolve_gs_app(Some(&entry(
            "Steam Big Picture",
            Some("  steam -gamepadui  "),
        )))
        .expect("a command entry is tracked");
        assert_eq!(t.command.as_deref(), Some("steam -gamepadui"));
        assert_eq!(t.game.id, None);
        assert_eq!(t.game.store, None);
        assert_eq!(t.game.title, "Steam Big Picture");
        // PATH lookup, not an absolute executable — the host's child tracks it.
        assert!(t.detect.is_empty());

        let t = resolve_gs_app(Some(&entry("", Some("/opt/game/run")))).expect("tracked");
        assert_eq!(t.game.title, "/opt/game/run");
    }

    /// Window is time, not frames: at 120 fps two intervals is 16.7 ms, under a ~30 ms re-ask.
    #[test]
    fn keyframe_coalesce_window_outlasts_a_clients_request_cadence() {
        let at_120 = keyframe_coalesce_window(Duration::from_secs_f64(1.0 / 120.0));
        assert!(
            at_120 >= Duration::from_millis(100),
            "120 fps window {at_120:?} does not outlast a ~30 ms request cadence"
        );
        // 60 fps is 33.3 ms — also under the floor.
        assert!(keyframe_coalesce_window(Duration::from_secs_f64(1.0 / 60.0)) >= at_120);
        // Slow stream keeps the frame-scaled window; the floor only raises.
        assert_eq!(
            keyframe_coalesce_window(Duration::from_millis(200)),
            Duration::from_millis(400)
        );
    }

    /// End-to-end check of the send thread: batches pushed on the channel arrive, complete and
    /// byte-identical, at a peer socket via the paced sendmmsg path.
    #[test]
    fn sender_delivers_batches() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Saturated CI can starve this thread for seconds between recv() wakeups.
        rx_sock
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let tx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx_sock.connect(rx_sock.local_addr().unwrap()).unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
        let (pool_tx, pool_rx) = std::sync::mpsc::sync_channel::<PacketBatch>(4);
        let spread = Arc::new(std::sync::Mutex::new(Vec::new()));
        spawn_sender(
            tx_sock,
            rx,
            Duration::from_millis(8),
            Arc::new(std::sync::atomic::AtomicU64::new(3 * 20_000_000)),
            pool_tx,
            spread.clone(),
            running.clone(),
            Arc::new(|| {}),
        )
        .unwrap();

        // Default rmem (~212 KB) holds ~80 × 1200 B datagrams (~2.5 KB truesize).
        // A bigger burst is silently dropped if this thread never drains concurrently.
        const PER_FRAME: usize = 20;
        let mut sent = Vec::new();
        for f in 0..3u8 {
            let batch: PacketBatch = (0..PER_FRAME as u8)
                .map(|i| {
                    let mut p = vec![0u8; 1200];
                    p[0] = f;
                    p[1] = i;
                    p
                })
                .collect();
            sent.extend(batch.iter().cloned());
            tx.send(batch).unwrap();
        }
        drop(tx);

        let mut got = 0usize;
        let mut buf = [0u8; 2048];
        while got < sent.len() {
            let n = rx_sock.recv(&mut buf).expect("packet within timeout");
            assert_eq!(n, 1200);
            let (f, i) = (buf[0] as usize, buf[1] as usize);
            assert_eq!(&buf[..n], &sent[f * PER_FRAME + i][..], "payload intact");
            got += 1;
        }
        assert_eq!(got, 3 * PER_FRAME);
        assert!(running.load(Ordering::SeqCst), "no spurious client-gone");
        let mut recycled = 0;
        while pool_rx.try_recv().is_ok() {
            recycled += 1;
        }
        assert!(
            recycled >= 1,
            "sender must return spent batches to the pool"
        );
        assert!(
            spread.lock().unwrap().len() >= 3,
            "per-frame spread recorded"
        );
    }

    /// Framing (32 B per 1376 B payload) and FEC parity fit inside the configured wire budget.
    #[test]
    fn encoder_rate_fits_inside_the_wire_budget() {
        // 20 Mbps, 20 % FEC, packetSize 1392: enc = 20e6 × 1376/1408 × 100/120.
        let enc = gs_encoder_bps(20_000, 20, 1392);
        assert_eq!(enc, 16_287_878);
        let wire = enc * 1408 / 1376 * 120 / 100;
        assert!(wire <= 20_000_000, "wire {wire} exceeds the 20 Mbps budget");
        assert!(
            wire >= 19_800_000,
            "wire {wire} leaves more than 1 % of the budget unused"
        );
        assert_eq!(gs_encoder_bps(20_000, 0, 1392), 20_000_000 * 1376 / 1408);
        assert_eq!(gs_encoder_bps(0, 20, 1392), 500_000);
    }

    /// FEC climbs fast and decays slow; budget de-rates only under sustained loss.
    #[test]
    fn loss_adaptation_steps_and_bounds() {
        let mut a = GsAdapt::new(20, 20_000);
        for _ in 0..100 {
            assert!(!a.step(0), "clean windows must not change anything at rest");
        }
        assert_eq!((a.fec_pct, a.budget_kbps), (20, 20_000));

        // First lossy: FEC +10, budget holds (one window is a blip).
        assert!(a.step(7));
        assert_eq!((a.fec_pct, a.budget_kbps), (30, 20_000));
        // Second consecutive: budget de-rates ×0.85.
        assert!(a.step(3));
        assert_eq!((a.fec_pct, a.budget_kbps), (45, 17_000));
        for _ in 0..30 {
            a.step(1);
        }
        assert_eq!(a.fec_pct, GsAdapt::FEC_MAX);
        assert_eq!(a.budget_kbps, 5_000, "floor = max(cap/4, 5 Mbps)");

        let mut changes = 0;
        for _ in 0..400 {
            if a.step(0) {
                changes += 1;
            }
            assert!(a.fec_pct >= 20 && a.fec_pct <= GsAdapt::FEC_MAX);
            assert!(a.budget_kbps >= 5_000 && a.budget_kbps <= 20_000);
        }
        assert_eq!((a.fec_pct, a.budget_kbps), (20, 20_000));
        assert!(changes > 0, "recovery must actually step");
        // Relapse resets the clean streak: one lossy window climbs FEC, does not de-rate.
        let budget_before_relapse = a.budget_kbps;
        a.step(2);
        assert_eq!(a.budget_kbps, budget_before_relapse);
        assert!(a.fec_pct > 20);
    }

    /// Budget floor cannot exceed the cap; a session below 5 Mbps never de-rates.
    #[test]
    fn loss_adaptation_tiny_session_never_derates_below_itself() {
        let mut a = GsAdapt::new(20, 4_000);
        for _ in 0..10 {
            a.step(9);
        }
        assert_eq!(a.budget_kbps, 4_000, "floor clamps to the cap");
        assert_eq!(a.fec_pct, GsAdapt::FEC_MAX);
    }
}
