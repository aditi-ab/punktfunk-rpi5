//! The video data plane: on RTSP PLAY, learn the client's UDP endpoint (it pings the video
//! port), then run capture → NVENC encode → [`VideoPacketizer`] → UDP send. The source is
//! either real portal desktop capture (`PUNKTFUNK_VIDEO_SOURCE=portal`, the portal PipeWire path) or
//! a synthetic test pattern (default). Runs on its own native thread.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

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
    /// Client requested HDR (`dynamicRangeMode != 0`) AND the host can deliver it ([`host_hdr_capable`]).
    /// Drives the capturer's proactive advanced-color enable; the encoder picks Main10 from the captured
    /// (P010) frame format. Always `false` on a non-HDR host, so the SDR path is unchanged.
    pub hdr: bool,
    /// Client's `x-nv-video[0].videoEncoderSlicesPerFrame` — the per-frame slice count its
    /// decoder wants (moonlight-common-c requests 1 for every HARDWARE decoder and 4 only for
    /// software slice-threading). Honored as the encoder's slicing ceiling: hardware TV decoders
    /// (Amlogic — Chromecast with Google TV) wedge the whole device on multi-slice AUs they
    /// never asked for (the 0.17.0 4-slice-default field regression). Absent ⇒ 1.
    pub slices: u32,
}

/// A pooled capturer plus the three properties reuse must match on — its HDR-ness, its
/// metadata-cursor mode (both fixed at PipeWire-negotiation time) and **which screen it is
/// actually capturing**: the `capture_monitor` pin, or `None` for the portal's own pick. A
/// mismatch on any of them needs a fresh screencast session (see `AppState::video_cap`).
///
/// The pin belongs in the key because it is a *live* setting — the console can re-aim the host
/// between two GameStream connects (`design/per-monitor-portal-capture.md` §7.3). Without it the
/// second connect would silently keep streaming the previous screen, which is the exact failure
/// the pin exists to prevent.
pub type PooledCapturer = (Box<dyn Capturer>, bool, bool, Option<String>);

/// Slot for the persistent screen capturer, shared with the control plane and reused across
/// streams so a reconnect doesn't open a second (conflicting) screencast session.
pub type CapturerSlot = Arc<std::sync::Mutex<Option<PooledCapturer>>>;

/// A pending client reference-frame-invalidation range (lost `firstFrame..=lastFrame`), set by the
/// control plane and drained by the video thread (see [`AppState::rfi_range`](super::AppState)).
pub type RfiSlot = Arc<std::sync::Mutex<Option<(i64, i64)>>>;

/// What the stream thread needs to give the launched game a lifetime
/// (design/session-game-lifetime.md). Bundled because the three only exist together — the control
/// plane builds them from the live `AppState` at RTSP PLAY, and the stream thread is where they are
/// spent.
pub struct GameLifetime {
    /// The session's deliberate-quit flag ([`super::AppState::quit`]), read when the stream ends: a
    /// decision may end the game, a drop gets a reconnect window first.
    pub quit: Arc<AtomicBool>,
    /// Hex cert fingerprint of the paired client that owns the launch, so only it can reclaim its own
    /// game. `None` when the peer cert couldn't be read.
    pub fingerprint: Option<String>,
    /// Ends the whole session, deliberately — the action for "the launched game exited".
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
    video_cap: CapturerSlot,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    on_lost: super::OnSessionLost,
    life: GameLifetime,
) {
    let _ = std::thread::Builder::new()
        .name("punktfunk-video".into())
        .spawn(move || {
            // Same scheduling posture as the native path's capture/encode thread (Linux nice -10 /
            // Windows HIGHEST + session tuning) — GameStream previously ran unboosted on Linux.
            crate::native::boost_thread_priority(true);
            // A GameStream viewer may be video-only too — hold the suspend/idle inhibitor for
            // this stream's lifetime (plane parity with the native LiveSessionGuard).
            let _sleep = crate::sleep_inhibit::hold();
            tracing::info!(?cfg, "video stream starting");
            // Lifecycle events + the script-facing marker file, plane parity with the native loop
            // (RFC §4): `announce` emits `stream.started`/`stream.stopped` and holds the marker for
            // the span between. It runs BEFORE `run` because `run` is what launches the app — the
            // marker has to exist by the time the title's own wrapper script executes, or the
            // wrapper takes its "not streaming" branch mid-stream. The RTSP layer carries no client
            // device name, so `client` is empty here — the `plane` field is what hooks key on.
            // `client.connected` fires alongside `stream.started` because a Moonlight client has no
            // persistent connection to anchor it to.
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
            // GPU clock pin (Linux, opt-in `PUNKTFUNK_PIN_CLOCKS`): hold the box-wide vendor clock
            // floor while this compat-plane stream runs, refcounted with every other live session
            // across both planes. Released when the closure exits (stream stopped) — so idle clocks
            // aren't pinned between Moonlight sessions. No-op off Linux / when the flag is unset.
            #[cfg(target_os = "linux")]
            let _clock_pin = crate::gpuclocks::session_pin();
            let result = run(
                cfg,
                app.as_ref(),
                &running,
                &force_idr,
                &rfi_range,
                &video_cap,
                &stats,
                &on_lost,
                &life,
            );
            // A clean return is a stop (RTSP teardown / cancel / client unreachable) → `quit`;
            // an error return is `error`. The compat plane can't tell a user stop from an idle
            // vanish the way the native plane's typed close code can.
            let reason = match &result {
                Ok(()) => crate::events::DisconnectReason::Quit,
                Err(_) => crate::events::DisconnectReason::Error,
            };
            if let Err(e) = result {
                tracing::error!(error = %format!("{e:#}"), "video stream failed");
            }
            running.store(false, Ordering::SeqCst);
            // Retract the marker and fire `stream.stopped` — explicitly here, before
            // `client.disconnected`, so the compat plane keeps the native loop's event order.
            drop(stream_marker);
            crate::events::emit(crate::events::EventKind::ClientDisconnected {
                client: event_client,
                reason,
            });
            tracing::info!("video stream stopped");
        });
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    video_cap: &std::sync::Mutex<Option<PooledCapturer>>,
    // Shared stats recorder for the web-console capture/graph. Threaded into `stream_body` (the
    // encode loop); per-frame sample emission is wired by a later pass.
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
    // Whole-session teardown for the send thread's client-unreachable detection.
    on_lost: &super::OnSessionLost,
    // The launched game's lifetime wiring (quit flag, launch owner, game-exit teardown).
    life: &GameLifetime,
) -> Result<()> {
    // GameStream capture/encode thread: apply Windows session tuning (no-op off Windows).
    pf_frame::session_tuning::on_hot_thread();
    // Reject an out-of-range client mode before allocating capture/encode buffers.
    encode::validate_dimensions(cfg.codec, cfg.width, cfg.height)
        .context("client-requested video mode")?;
    let sock = UdpSocket::bind(("0.0.0.0", VIDEO_PORT)).context("bind video UDP")?;
    // Grow SO_SNDBUF/RCVBUF (avoid host-side ENOBUFS at high bitrate) like the native plane.
    // The opt-in DSCP/QoS tag happens after connect below (Windows qWAVE derives the flow from
    // the connected 5-tuple).
    punktfunk_core::transport::grow_socket_buffers(&sock);
    // The client pings the video port so we learn where to send; it re-pings until video
    // flows, so a missed early ping is fine.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    tracing::info!(
        port = VIDEO_PORT,
        "video: awaiting client ping to learn endpoint"
    );
    let mut probe = [0u8; 256];
    let (_, client) = sock
        .recv_from(&mut probe)
        .context("video: no client ping within 10s")?;
    sock.connect(client)
        .context("connect client video endpoint")?;
    // Opt-in DSCP/QoS-tag this as the video class (PUNKTFUNK_DSCP=1); the guard keeps the
    // Windows qWAVE flow alive for the whole stream (this function's scope IS the stream).
    let _qos_flow = punktfunk_core::transport::set_media_qos(
        &sock,
        punktfunk_core::transport::MediaClass::Video,
    );
    tracing::info!(%client, "video: client endpoint learned");
    // Short label for web-console stats captures: the client's peer IP.
    let client_label = client.ip().to_string();

    // Native client-resolution source: create a compositor virtual output sized to the client's
    // request and capture it (no scaling). Self-contained — deliberately NOT pooled in
    // `video_cap`, since a reconnect at a different resolution needs a freshly-sized output; the
    // output is released when this capturer drops at stream end (RAII via its keepalive).
    if pf_host_config::config().video_source.as_deref() == Some("virtual") {
        // Reference point for adopting the launched game's processes: anything the host will call
        // "this session's game" has to have started after this instant. Taken HERE — before the prep
        // steps, before the source (a bare-spawn gamescope nests the game inside it), before the
        // launch — because a reading taken later would reject the very process it is meant to find.
        // Erring early can only ever include more of our own launch, never a copy from before it.
        let launch_stamp = crate::gamelease::launch_clock();
        // Everything the host knows about the title being launched, resolved in ONE library scan:
        // what to run, what to call it, and how to recognize it once it is up.
        let target = resolve_gs_app(app);
        // Moonlight has no session resume, so a client coming back for a game it left behind does it
        // by launching the title again. Reprieve that game before anything starts, so the copy the
        // player is about to be handed isn't killed out from under them when the old window closes.
        if let Some(t) = target.as_ref() {
            let reprieved =
                crate::gamelease::readopt(life.fingerprint.as_deref(), t.game.id.as_deref());
            if reprieved > 0 {
                tracing::info!(
                    reprieved,
                    title = %t.game.title,
                    "gamestream: this client came back for its game — keeping it"
                );
            }
        }
        // Per-app prep steps (RFC §6): the entry's own `prep` plus a custom library title's,
        // run synchronously BEFORE the virtual output opens or anything launches (an HDR
        // toggle / sink switch must land first — and gamescope's nested launch happens inside
        // `open_gs_virtual_source`). The guard's drop runs the undos at stream end — reverse
        // order, best-effort, on every exit path including a panic-unwind.
        let mut prep_cmds = app.map(|a| a.prep.clone()).unwrap_or_default();
        if let Some(lib_id) = app.and_then(|a| a.library_id.as_deref()) {
            prep_cmds.extend(crate::library::prep_for(lib_id));
        }
        let prep_env = [(
            "PF_APP_TITLE".to_string(),
            app.map(|a| a.title.clone()).unwrap_or_default(),
        )];
        let _prep = (!prep_cmds.is_empty()).then(|| crate::hooks::run_prep(&prep_cmds, &prep_env));
        // Open the virtual-display source: pick the live compositor, normalize the session env
        // (apply_session_env/apply_input_env — gamescope ATTACH/resize + KWin/Mutter retargeting,
        // exactly like the native plane), create a virtual output at the client mode, and capture it.
        // Re-runnable: the encode loop calls it again on a mid-stream capture loss to FOLLOW a
        // Desktop<->Game switch.
        let (mut capturer, compositor, gamescope_route) =
            open_gs_virtual_source(cfg, app, target.as_ref(), &life.quit)?;
        // Only the Linux `launch_is_nested` reads it; gamescope does not exist on Windows.
        #[cfg(not(target_os = "linux"))]
        let _ = &gamescope_route;
        // Register this session in the admission table. GameStream acquires a REAL display but
        // never registered, so `admit`'s Windows budgets — `max_displays` and the NVENC session
        // headroom — could not see it: both gate on `!live.is_empty()`, so a Moonlight-held display
        // was invisible to them and a native connect could be admitted past a budget the box had
        // already spent. Identity is `None` (the compat plane has no cert fingerprint), which is the
        // same "anonymous" the conflict policy already handles. Dropped at the end of `run`.
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
        // Launch the app's command now that capture is live, for the backends that DON'T nest it via
        // set_launch_command above: Windows (no gamescope) and, on Linux, everything but gamescope's
        // bare-spawn sub-mode (kwin/mutter/wlroots stream the existing desktop; a managed/attached
        // gamescope is a running session to launch INTO — `launch_session_command` routes both).
        // A library title (Steam/Epic/GOG/Xbox/custom, surfaced in /applist) carries its
        // store-qualified id — resolved against the host's OWN library (the client can only pick an
        // existing title, never inject a command). An apps.json entry instead carries an
        // operator-typed `cmd`. Library id wins when both are set.
        #[cfg(windows)]
        if let Some(t) = target.as_ref() {
            // A library title launches by its store-qualified id (the interactive-session spawner
            // resolves the store's own recipe); an operator-typed command runs as itself.
            let launched = match (t.game.id.as_deref(), t.command.as_deref()) {
                (Some(id), _) => crate::library::launch_gamestream_library(id),
                (None, Some(cmd)) => crate::library::launch_gamestream_command(cmd),
                (None, None) => Ok(()),
            };
            if let Err(e) = launched {
                tracing::warn!(title = %t.game.title, error = %e, "gamestream: could not launch app");
            }
        }
        // Linux keeps the spawned child rather than dropping it: it is the primary liveness signal
        // for a title whose store told us nothing else, and the handle the termination ladder
        // signals. A gamescope bare spawn already nested the command (`set_launch_command` in the
        // source open), so launching again would start it twice.
        #[cfg(target_os = "linux")]
        let spawned_launch = match target.as_ref().and_then(|t| t.command.as_deref()) {
            Some(_) if crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref()) => {
                None
            }
            Some(cmd) => match crate::library::launch_session_command(compositor, cmd) {
                Ok(spawned) => Some(spawned),
                Err(e) => {
                    tracing::warn!(command = %cmd, error = %e, "gamestream: could not launch app");
                    None
                }
            },
            None => None,
        };

        // The launched game's lifetime, in both directions (design/session-game-lifetime.md) — the
        // compat plane's half of what the native plane already does:
        //
        // * **its exit ends the session**, so Moonlight returns to its app list instead of leaving
        //   the player on a bare desktop or a hidden launcher.
        // * **this session ending can end it** — never by default; only when the operator asked, and
        //   for a mere drop only after a reconnect window (the guard's drop). Moonlight can't resume
        //   a session, but the window still protects unsaved progress on a network blip, and a
        //   relaunch of the same title reclaims the game (above).
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
                    // Read the setting at fire time, so flipping it mid-session takes effect. The
                    // lease itself keeps running either way — the status surface still reports the
                    // game.
                    if !crate::session_settings::get().session_on_game_exit {
                        tracing::info!(
                            "the launched game exited, but ending the session on game exit is off — \
                             leaving the stream up"
                        );
                        return;
                    }
                    tracing::info!("the launched game exited — ending the session");
                    // Deliberate: the player finished. The display skips its keep-alive linger and
                    // the launch state is cleared, so Moonlight's next `/launch` starts cleanly.
                    on_game_exit();
                })
            };
            let lease = crate::gamelease::open(
                crate::gamelease::LeaseRequest {
                    game: t.game.clone(),
                    // RTSP carries no client device name, so the peer IP is the best label there is
                    // (the same one the stats capture uses).
                    client: client_label.clone(),
                    plane: crate::events::Plane::Gamestream,
                    spec: t.detect.clone(),
                    nested,
                    child,
                    launch_stamp,
                },
                on_exit,
            );
            // Declared first so it drops first: the console loses the live row before the policy
            // below can replace it with a `grace` one, rather than briefly showing both.
            let published = crate::session_status::publish_gamestream_game(lease.shared());
            (
                published,
                crate::gamelease::SessionGuard::new(
                    lease,
                    life.quit.clone(),
                    life.fingerprint.clone(),
                ),
            )
        });
        // Rebuild closure: re-open the source on a mid-stream capture loss, RE-DETECTING the live
        // compositor — so a Desktop<->Game switch (at the client's fixed mode) is FOLLOWED in place
        // without a Moonlight reconnect. (A resolution change can't be followed mid-stream on
        // GameStream — WxH is locked at ANNOUNCE — but a session toggle keeps the negotiated mode.)
        let rebuild =
            || open_gs_virtual_source(cfg, app, target.as_ref(), &life.quit).map(|(c, _, _)| c);
        return stream_body(
            &mut capturer,
            Some(&rebuild),
            // Mirrors the source's own `set_hw_cursor` request (`open_gs_virtual_source`):
            // cursor-as-metadata + host blend wherever the backend composites — the
            // compositor-EMBEDS fallback never paints on a Mutter virtual stream (the
            // native plane's no-channel rule, `session_plan::cursor_blend_for`). gamescope
            // remains the pointerless residual — its capture carries no cursor either way
            // (the native plane's XFixes source is not wired on this plane).
            compositor != crate::vdisplay::Compositor::Gamescope
                && blend_capable_metadata_cursor(&cfg),
            &sock,
            cfg,
            running,
            force_idr,
            rfi_range,
            stats,
            &client_label,
            on_lost,
        );
    }

    // Reuse the persistent capturer (one screencast session → clean reconnect); create it on
    // the first stream. Borrow it for this stream and return it on exit. Reuse is gated on the
    // pooled capturer's HDR-ness matching this stream's negotiated `cfg.hdr` — the depth is a
    // PipeWire-negotiation-time property of the screencast session, so an HDR↔SDR change needs a
    // fresh session (same pattern as the audio capturer's channel-count gate).
    // Cursor-as-metadata only where the encode backend this session resolves to composites
    // `frame.cursor` (the caps-aware negotiation — mirror of the native plane's); otherwise ask
    // the portal to EMBED the pointer so no backend × cursor-mode combination streams
    // cursorless. Synthetic frames carry no pointer either way.
    let metadata_cursor = blend_capable_metadata_cursor(&cfg);
    // Which screen this stream must show. The host-wide pin (§5.3) applies to the compat plane too:
    // the portal chooser cannot name a head, so a pinned host MIRRORS it here the same way the
    // virtual source does via `vdisplay::open`. Without this a Moonlight client on a pinned host
    // would silently get whichever monitor the portal handed back — "showing the wrong monitor is
    // worse than showing none" is the rule the whole feature is built on.
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
    // Portal/synthetic source: no compositor virtual output to re-detect, so no rebuild closure.
    let result = stream_body(
        &mut capturer,
        None,
        metadata_cursor,
        &sock,
        cfg,
        running,
        force_idr,
        rfi_range,
        stats,
        &client_label,
        on_lost,
    );
    capturer.set_active(false);
    // Re-pool ONLY a capturer that can still produce frames. Every terminal state of the portal
    // backend is sticky (`Capturer::is_alive`): a dead zerocopy-import worker, an exited PipeWire
    // thread, or a compositor that went away all make the NEXT stream fail at exactly the same
    // point — and this path has no rebuild closure (unlike the virtual-output path above), so a
    // re-admitted dead capturer wedged GameStream portal video permanently, at 10 s per reconnect
    // attempt. Dropping it instead costs one fresh screencast session on the next connect. Note
    // `result` may already be `Err` here, which is itself that signal. (`metadata_cursor` and the
    // monitor pin ride along as the other two reuse keys, beside HDR-ness — see `PooledCapturer`.)
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

/// Open a capturer on the **pinned physical monitor** for the compat plane's portal source
/// (`design/per-monitor-portal-capture.md` §5.3). The pin is host-wide, so it has to be honored on
/// every plane that captures a screen — and the portal source is the one that otherwise takes
/// "whichever head the portal hands back".
///
/// Deliberately *not* the `open_gs_virtual_source` path: this source launches nothing and creates no
/// virtual output, so it needs neither the game-lifetime machinery nor the registry (a mirror is
/// [`DisplayOwnership::External`](crate::vdisplay::DisplayOwnership) and would pass straight through
/// it anyway). A missing monitor fails the stream loudly rather than falling back to another screen.
#[cfg(target_os = "linux")]
fn open_gs_mirror_source(
    connector: &str,
    cfg: StreamConfig,
    metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    // Follow the live session first, exactly as the virtual source does — a mirror host that
    // switched Desktop↔Game since startup must be enumerated against the compositor that is up now.
    let active = crate::vdisplay::detect_active_session();
    crate::vdisplay::observe_session_instance(&active);
    crate::vdisplay::apply_session_env(&active);
    let compositor = crate::vdisplay::compositor_for_kind(active.kind)
        .map(Ok)
        .unwrap_or_else(crate::vdisplay::detect)
        .context("detect compositor")?;
    // A mirror streams an existing head — no gamescope sub-mode applies, so the resolved route is
    // deliberately dropped here rather than carried.
    let _ = crate::vdisplay::apply_input_env(compositor, false);
    let mut vd = crate::vdisplay::open_mirror(compositor, connector)?;
    // Cursor mode is the session's negotiated one: metadata where this encode path composites
    // `frame.cursor`, otherwise let the compositor embed it (§7.5 — one resolver, per-backend
    // expression).
    vd.set_hw_cursor(metadata_cursor);
    // The mirror backend ignores the requested mode by design (§7.3 — a panel runs at the mode its
    // owner set, and the client scales); pass the client's anyway so the argument stays honest.
    let vout = vd
        .create(punktfunk_core::Mode {
            width: cfg.width,
            height: cfg.height,
            refresh_hz: cfg.fps,
        })
        .context("start mirroring the pinned monitor")?;
    crate::capture::capture_virtual_output(
        vout,
        pf_frame::OutputFormat::resolve(cfg.hdr, crate::zerocopy::enabled()),
        crate::session_plan::CaptureBackend::resolve(),
    )
    .context("attach a capturer to the mirrored monitor")
}

/// What the compat plane resolved about the app a client launched: identity for the lease, the status
/// surface and the `game.*` events; the signals that recognize the running game; and the command to
/// run it.
struct GsApp {
    game: crate::gamelease::GameRef,
    detect: crate::library::DetectSpec,
    /// The resolved shell command. `Some` on Linux, which runs it itself; `None` for a Windows
    /// library title, which launches by id through the interactive-session spawner instead.
    command: Option<String>,
}

/// Resolve a `/launch`ed catalog entry against the host's **own** library — the client sends only an
/// appid, and everything the session does with the title afterwards comes from what the host knows
/// about it.
///
/// A library pick carries its store's detect signals. An operator-typed `apps.json` command has no
/// library entry behind it, so its title is the whole identity and its own first token — when that is
/// an absolute executable — the only signal there is; the host spawns it directly anyway, so the child
/// is the primary tracking either way. `None` = nothing to launch (Desktop, or an unresolvable entry).
fn resolve_gs_app(app: Option<&super::apps::AppEntry>) -> Option<GsApp> {
    let app = app?;
    if let Some(id) = app.library_id.as_deref() {
        match crate::library::resolve_launch(id) {
            Some(t) => {
                return Some(GsApp {
                    game: t.game,
                    detect: t.detect,
                    command: t.command,
                })
            }
            // Same fallback (and same warning) the plain command resolution has always had, so a
            // client picking a stale title sees why nothing started.
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

/// Open the virtual-display video source for a GameStream session: pick the LIVE compositor + normalize
/// the session env (apply_session_env/apply_input_env — gamescope ATTACH/resize, KWin/Mutter
/// retargeting) exactly like the native plane (native.rs resolve_compositor), create a virtual
/// output at the client's mode, and capture it. Returns the capturer (it owns the output's keepalive;
/// the stateless VirtualDisplay factory is dropped here) plus the resolved compositor. An apps.json
/// entry can PIN a compositor (skips the live detect/retarget). Re-run on a mid-stream capture loss to
/// FOLLOW a Desktop<->Game switch: it re-detects the now-live compositor and re-targets at it. Does NOT
/// launch the app (that happens once at stream start; a rebuild must not re-spawn it).
/// Cursor-as-metadata for this plane (GameStream has no cursor channel): only where the encode
/// backend this session resolves to composites `frame.cursor` — the same CUDA-payload
/// prediction `SessionPlan`/`handshake::cursor_forward` make (the NVIDIA resolution plus the
/// zero-copy master switch). Shared by the monitor-mirror and virtual-output sources so their
/// `set_hw_cursor` request and `stream_body`'s blend flag cannot drift.
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

fn open_gs_virtual_source(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
    // The resolved title (see [`resolve_gs_app`]) — its command is what a bare-spawn gamescope nests
    // and what decides whether this session is a dedicated game session at all. Resolved once by the
    // caller, so a mid-stream rebuild can't re-resolve to something different.
    launch: Option<&GsApp>,
    // The session's deliberate-quit flag, handed to the display's keep-alive lease.
    quit: &Arc<AtomicBool>,
) -> Result<(
    Box<dyn Capturer>,
    crate::vdisplay::Compositor,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    let (compositor, gamescope_route) = if let Some(c) = app.and_then(|a| a.compositor) {
        // An app-pinned compositor still needs a route resolved, or `create` falls through to a
        // bare spawn on a box pinned to the managed session (mirrors `native::resolve_compositor`).
        let r = crate::vdisplay::resolve_gamescope_route(c, false);
        (c, r)
    } else {
        // Windows has a single virtual-display backend (pf-vdisplay); `vdisplay::open` ignores the
        // compositor arg there, so short-circuit the Linux session-detection state machine with a
        // placeholder — mirrors `native::resolve_compositor`. Without this, the Linux `detect()`
        // below bails on Windows ("could not detect compositor … XDG_CURRENT_DESKTOP=''"), which
        // killed the GameStream video thread → black screen (the native plane was already guarded).
        #[cfg(target_os = "windows")]
        {
            (crate::vdisplay::Compositor::Kwin, None)
        }
        #[cfg(not(target_os = "windows"))]
        {
            // A client is (re)connecting → cancel any pending TV-session restore (review #3).
            crate::vdisplay::cancel_pending_tv_restore();
            let active = crate::vdisplay::detect_active_session();
            // A4: fold any compositor-instance change (idle-time Game↔Desktop switch) into the epoch
            // before acquiring, so a GameStream reconnect never reuses a dead-instance node.
            crate::vdisplay::observe_session_instance(&active);
            crate::vdisplay::apply_session_env(&active);
            // Dedicated game session (B0): a GameStream app whose launch RESOLVES to a command (library
            // id / apps.json command), under `game_session=dedicated` with gamescope available, gets its
            // own headless gamescope spawn at the client mode — same routing as the native plane. Gate on
            // the resolved command so an unresolvable entry falls back to auto routing (review #9).
            let has_launch = launch.and_then(|t| t.command.as_deref()).is_some();
            if crate::vdisplay::wants_dedicated_game_session(has_launch) {
                let r =
                    crate::vdisplay::apply_input_env(crate::vdisplay::Compositor::Gamescope, true);
                (crate::vdisplay::Compositor::Gamescope, r)
            } else {
                let c = crate::vdisplay::compositor_for_kind(active.kind)
                    .map(Ok)
                    .unwrap_or_else(crate::vdisplay::detect)
                    .context("detect compositor")?;
                let r = crate::vdisplay::apply_input_env(c, false);
                (c, r)
            }
        }
    };
    let mut vd = crate::vdisplay::open(compositor).context("open virtual display")?;
    // Out-of-band cursor for the virtual source (the native plane's no-channel rule, mirrored):
    // GameStream has no cursor channel, and the compositor-EMBEDS fallback never paints on a
    // Mutter virtual stream (stage-global overlay suppression since Mutter 48 — see
    // pf-vdisplay `mutter.rs`), so ask for cursor-as-metadata wherever the resolved backend
    // composites `frame.cursor`; `stream_body`'s blend flag mirrors this request. gamescope
    // stays off: its capture carries no metadata either way, and the request would cost the
    // native-NV12 shape for nothing.
    vd.set_hw_cursor(
        compositor != crate::vdisplay::Compositor::Gamescope && blend_capable_metadata_cursor(&cfg),
    );
    // Carry the resolved launch command on the backend instance (per-session) rather than a
    // process-global env var, so concurrent sessions can't stomp each other's launch target. It is
    // the RESOLVED command, so gamescope's bare spawn nests a library title exactly like an
    // apps.json command (it previously nested only `cmd`, silently dropping library picks). Off
    // Linux this is a no-op backend-side, and a library title resolves to no command at all — the
    // interactive-session spawner launches it by id instead.
    vd.set_launch_command(launch.and_then(|t| t.command.clone()));
    // This plane's resolved gamescope sub-mode, on the instance for the same reason as the launch
    // command above — the GameStream and native planes both call `apply_input_env`, so publishing
    // through the process env let either retarget the other's `create`.
    vd.set_gamescope_route(gamescope_route.clone());
    // Serialize with the punktfunk/1 plane's IDD-push setup dance (Goal-1 §2.5). A GameStream
    // connect used to skip it entirely, so it could ADD/reconfigure the shared monitor while a
    // native session was mid-build (and vice versa), and its sealed-channel delivery would replace
    // the native session's ring (newest-wins) — each plane could freeze the other. GameStream has
    // no cooperative stop-flag plumbing, so it registers a flag nobody reads: a LATER session that
    // preempts this one signals it, waits the 3 s release grace, then force-preempts the monitor —
    // this session then fails on capture and tears down cleanly (the intended handover). GameStream
    // is anonymous (no client cert), so it holds the ANONYMOUS slot (0) — GS stays single-display,
    // and only a later slot-0 session (another GS/anonymous connect) preempts it.
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
        // GameStream's deliberate quit is the Moonlight "Quit App" (nvhttp `h_cancel`), the
        // management stop, or the launched game exiting — not a QUIC close code. All three set the
        // session's quit flag ([`super::AppState::quit`]), so the display skips its keep-alive linger
        // for a stop the way it does on the native plane, and only a real drop lingers.
        quit.clone(),
        None, // fresh session — no display superseded
    )
    .context("create virtual output at client resolution")?;
    // HDR: pass the negotiated `cfg.hdr` (client asked for HDR AND the host can deliver it). On the
    // Windows IDD-push path this proactively enables advanced color on the virtual display so a Main10
    // PQ stream flows even from an SDR desktop; an already-HDR desktop streams PQ regardless (the
    // capturer follows the display). No-op on Linux: virtual-output capture is SDR-only upstream
    // (Mutter RecordVirtual), and `host_hdr_capable` therefore keeps `cfg.hdr` false for this
    // source — the Linux HDR path is the portal monitor mirror (`video_source=portal`).
    let mut capturer = capture::capture_virtual_output(
        vout,
        capture::OutputFormat::resolve(cfg.hdr, crate::encode::resolved_backend_is_gpu()),
        crate::session_plan::CaptureBackend::resolve(),
    )
    .context("capture virtual output")?;
    capturer.set_active(true);
    Ok((capturer, compositor, gamescope_route))
}

/// The encoder bit depth implied by the captured frame's pixel format: a 10-bit (HDR) source — the
/// Windows IDD-push capturer's `P010`/`Rgb10a2` when the desktop is HDR — opens NVENC as HEVC Main10
/// (BT.2020 PQ); everything else is 8-bit. The encoder backends already key the real profile off the
/// `format`, so this just keeps the `bit_depth` argument honest (the old hard-coded `8` mislabeled an
/// HDR stream that the format had already promoted to 10-bit).
fn gs_bit_depth(format: crate::capture::PixelFormat) -> u8 {
    use crate::capture::PixelFormat;
    match format {
        // Windows IDD-push HDR formats, and the Linux GNOME 50+ portal HDR formats.
        PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10 => {
            10
        }
        _ => 8,
    }
}

/// One frame's packets, handed from the encode thread to the send thread.
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

/// Windows: coalesce each paced burst's equal-size packets into `WSASendMsg(UDP_SEND_MSG_SIZE)`
/// super-buffers (UDP Send Offload — the Windows analogue of Linux GSO), so a 16-packet burst is one
/// syscall instead of 16. Reuses the proven core USO primitive; it returns how many leading packets
/// it sent, and we send any remainder (USO off via `PUNKTFUNK_GSO=0`, a size-mixed burst, or a
/// frame's short final packet) with a per-packet `send`. The socket is connected.
#[cfg(target_os = "windows")]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    let refs: Vec<&[u8]> = pkts.iter().map(|p| p.as_slice()).collect();
    let n = punktfunk_core::transport::send_uso_all(sock, &refs)?;
    for p in &pkts[n..] {
        sock.send(p)?;
    }
    Ok(())
}

/// Portable fallback (other non-Linux dev builds, e.g. macOS — GameStream hosting never ships there):
/// one syscall per packet.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    for p in pkts {
        sock.send(p)?;
    }
    Ok(())
}

/// One encoded frame handed from the encode loop to the packetizer thread: the frame's access
/// units (owned buffers, each with its frame type) plus the shared 90 kHz RTP timestamp. FEC
/// packetization runs on the packetizer thread — off the encode loop — so it never serializes
/// behind encode (measured ~3 ms/frame at 4K, which capped GameStream's frame rate well below what
/// the encoder alone can sustain).
struct RawFrame {
    /// `(bitstream, type, wire frameIndex)` per AU. The stream loop assigns the index (it owns
    /// the numbering — see its `au_seq`), so the encoder's RFI bookkeeping stays 1:1 with what
    /// Moonlight sees across mid-stream encoder rebuilds.
    aus: Vec<(Vec<u8>, FrameType, u32)>,
    ts: u32,
}

/// Packetizer thread: turns each [`RawFrame`]'s access units into wire datagrams (data + Reed–Solomon
/// FEC parity shards) via the stateful [`VideoPacketizer`], then hands the batch to the paced sender.
/// It sits between encode and send so the FEC never blocks the encode loop. Backpressure: the hand-off
/// to the sender BLOCKS, so if the paced sender falls behind, the packetizer stalls and the
/// encode→packetizer queue fills — the encode loop then drops the newest frame (see the loop) rather
/// than stalling. Tallies goodput (bytes handed to the wire) into `goodput` for the encode loop's stats
/// window. Exits when either neighbor's channel closes (session teardown / client gone).
fn spawn_packetizer(
    rx: std::sync::mpsc::Receiver<RawFrame>,
    tx: std::sync::mpsc::SyncSender<PacketBatch>,
    mut pk: VideoPacketizer,
    goodput: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("punktfunk-pkt".into())
        .spawn(move || {
            // Above-normal, like the send thread — this stage is on the per-frame critical path.
            crate::native::boost_thread_priority(false);
            while let Ok(frame) = rx.recv() {
                let mut batch: PacketBatch = Vec::new();
                for (au, ft, idx) in frame.aus {
                    batch.extend(pk.packetize(&au, ft, frame.ts, Some(idx)));
                }
                if batch.is_empty() {
                    continue;
                }
                let bytes: u64 = batch.iter().map(|p| p.len() as u64).sum();
                // Blocking send: propagates the paced sender's backpressure upstream (see above).
                if tx.send(batch).is_err() {
                    break; // sender exited (client gone)
                }
                goodput.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .context("spawn packetizer thread")?;
    Ok(())
}

/// Dedicated send thread: one [`PacketBatch`] per frame arrives on `rx`; its packets go out in
/// `sendmmsg` chunks, paced so the frame's data spreads over ~3/4 of the frame interval — the
/// shared [`send_pacing`](crate::send_pacing) policy at the GameStream parameterization: no
/// microburst stage, a BOUNDED step count (≤ 12, chunk ≥ 16, see the policy's docs for the
/// "send queue full" history that bound guards), each step ending in a sleep toward its slice
/// of the fixed budget. On send failure (client gone) it ends the whole session via `on_lost` —
/// not just this thread: audio would otherwise keep streaming at the dead endpoint and the stale
/// launch state would wedge the next connect (see `AppState::end_session`).
fn spawn_sender(
    sock: UdpSocket,
    rx: std::sync::mpsc::Receiver<PacketBatch>,
    frame_interval: Duration,
    running: Arc<AtomicBool>,
    on_lost: super::OnSessionLost,
) -> Result<()> {
    std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn(move || {
            // Transmit thread: above-normal, matching the native path's send thread (includes the
            // Windows session tuning/MMCSS this used to call directly; adds the Linux nice -5).
            crate::native::boost_thread_priority(false);
            let budget = frame_interval.mul_f32(0.75);
            let cfg = crate::send_pacing::PaceCfg {
                burst_bytes: None, // no microburst stage — the whole frame spreads
                chunk: crate::send_pacing::ChunkPolicy::Bounded {
                    min_chunk: 16,
                    max_steps: 12,
                },
                sleep_floor: Duration::from_micros(500),
            };
            let mut sent: u64 = 0;
            let mut dropped: u64 = 0;
            while let Ok(mut batch) = rx.recv() {
                // FEC test knob (PUNKTFUNK_VIDEO_DROP) — same knob the native plane honors.
                dropped += crate::send_pacing::inject_video_drop(&mut batch);
                if batch.is_empty() {
                    continue;
                }
                let r = crate::send_pacing::pace_frame(
                    &batch,
                    crate::send_pacing::PaceBudget::Fixed(budget),
                    &cfg,
                    |chunk| {
                        sendmmsg_all(&sock, chunk)?;
                        sent += chunk.len() as u64;
                        Ok::<(), std::io::Error>(())
                    },
                );
                if let Err(e) = r {
                    tracing::info!(error = %e, sent, "video: client unreachable — ending session");
                    running.store(false, Ordering::SeqCst);
                    on_lost();
                    return;
                }
            }
            tracing::debug!(sent, dropped, "video sender exiting");
        })
        .context("spawn send thread")?;
    Ok(())
}

use crate::send_pacing::percentile;

/// The encode → packetize loop, over a borrowed capturer. Sending runs on a dedicated thread
/// (see [`spawn_sender`]) so a send spike can never stall capture/encode.
#[allow(clippy::too_many_arguments)]
fn stream_body(
    // `&mut Box` (not `&mut dyn`) so a mid-stream capture-loss rebuild can SWAP the capturer in place.
    capturer: &mut Box<dyn Capturer>,
    // Re-open the video source on capture loss (virtual-display path → follow a Desktop<->Game switch);
    // `None` for the portal/synthetic source, which has nothing to re-detect (propagate the error).
    rebuild: Option<&dyn Fn() -> Result<Box<dyn Capturer>>>,
    // The capture hands the encoder cursor bitmaps to composite (cursor-as-metadata negotiated
    // because the resolved backend blends — see the callers). `false` = the pointer is embedded
    // in the pixels (or absent), so the encoder is asked to composite nothing.
    cursor_blend: bool,
    sock: &UdpSocket,
    cfg: StreamConfig,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    // Shared stats recorder. The encode loop reads `stats.is_armed()` per frame to decide whether
    // to accumulate the per-stage split, then emits a `StatsSample` at its 1 s aggregation boundary.
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
    // Short client label (peer IP) seeded into the capture meta on the first armed registration.
    client_label: &str,
    // Whole-session teardown, handed to the send thread's client-unreachable detection.
    on_lost: &super::OnSessionLost,
) -> Result<()> {
    // The first frame establishes the authoritative size/format for the encoder.
    let mut frame = capturer.next_frame().context("capture first frame")?;
    if frame.width != cfg.width || frame.height != cfg.height {
        tracing::warn!(
            captured = ?(frame.width, frame.height),
            negotiated = ?(cfg.width, cfg.height),
            "captured size != negotiated size — Moonlight expects the negotiated size; resize the output"
        );
    }
    let mut enc = encode::open_video(
        cfg.codec,
        frame.format,
        frame.width,
        frame.height,
        cfg.fps,
        cfg.bitrate_kbps as u64 * 1000,
        frame.is_cuda(),
        // 8-bit SDR, or 10-bit when the captured frame is HDR (P010) — see `gs_bit_depth`.
        gs_bit_depth(frame.format),
        // GameStream/Moonlight stays 4:2:0 — stock Moonlight clients can't decode 4:4:4, and the
        // Windows IDD-push capturer can't yet deliver full-chroma frames. 4:4:4 is punktfunk/1-native only.
        encode::ChromaFormat::Yuv420,
        // True only when THIS session's capture negotiated cursor-as-metadata — which the
        // callers grant only where the resolved backend composites (`cursor_blend_capable`).
        cursor_blend,
        // The client's requested slices-per-frame (its DECODER's ceiling) — see `StreamConfig`.
        cfg.slices,
    )
    .context("open video encoder for stream")?;
    // Tell the encoder how deep the capturer lets it pipeline. Without this an in-place backend
    // (Windows direct-NVENC, which encodes the capturer's textures with no CopyResource) bounds
    // itself by an env cap instead of the ring it is actually reading, and the capturer rotates a
    // texture out from under a live encode — torn/mixed frames, never an error. The backend now
    // also fails safe when nobody tells it, but pass the REAL depth: `idd_depth` is configurable
    // and a deeper ring is free pipelining the fallback would forfeit.
    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    // FEC overhead percent (Sunshine default 20). Override with PUNKTFUNK_FEC_PCT (0 = data-only).
    let fec_pct: u8 = std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let pk = VideoPacketizer::new(cfg.packet_size, fec_pct, cfg.min_fec);

    // Pace at the client's negotiated frame rate, re-encoding the last captured frame when the
    // compositor produced no new one. Compositors only emit frames on damage, so a static or
    // slow-updating desktop would otherwise starve the client into a "network too slow" abort.
    // Re-encoding an unchanged frame is cheap — NVENC emits a near-empty P-frame. The upper
    // bound just guards against an absurd client request (the encoder is opened at `cfg.fps`).
    let target_fps = cfg.fps.clamp(1, 240);
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let mut fps_count: u32 = 0;
    let mut fps_t = Instant::now();
    let stream_start = Instant::now();
    let mut sent_batches: u64 = 0;
    let mut dropped_batches: u64 = 0;

    // Three-stage pipeline so FEC packetization never blocks encode: `encode loop → [raw AUs] →
    // packetizer (FEC/RS) → [wire batch] → paced sender`, each stage on its own thread joined by a
    // depth-2 bounded queue. Depth 2 means a slow stage can buffer one frame while the next is
    // produced; beyond that the NEWEST frame is dropped (the client recovers via FEC/RFI) rather than
    // stalling the encode loop. Backpressure chains up: a slow sender blocks the packetizer, which
    // fills the encode→packetizer queue, which makes the encode loop drop — encode itself never
    // waits. Goodput (bytes handed to the wire) is tallied by the packetizer into `goodput`, read at
    // the encode loop's 1 s stats boundary (the old inline batch-byte sum moved with packetization).
    let goodput = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
    spawn_sender(
        sock.try_clone().context("clone video socket")?,
        batch_rx,
        Duration::from_secs_f64(1.0 / target_fps as f64),
        running.clone(),
        on_lost.clone(),
    )?;
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawFrame>(2);
    spawn_packetizer(raw_rx, batch_tx, pk, goodput.clone())?;

    // Per-stage timing (PUNKTFUNK_PERF=1): max µs/stage per second + unique vs re-encoded frames,
    // to pinpoint stalls. `unique` counts genuinely-new captured frames (vs re-encoded holds).
    let perf = pf_host_config::config().perf;
    let (mut mx_cap, mut mx_enc, mut mx_pkt, mut mx_send, mut uniq) =
        (0u128, 0u128, 0u128, 0u128, 0u32);
    // Web-console stats accumulation (active when `perf` OR a capture is armed): per-stage vectors
    // for p50/p99, the goodput bytes queued to the sender this window, the previous window's
    // dropped-frame count for delta computation, and the registration id cached on the first sample.
    let codec_name = cfg.codec.label();
    let mut sid: Option<u32> = None;
    let (mut v_cap, mut v_enc, mut v_pkt, mut v_send): (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut last_dropped_batches: u64 = 0;
    // Absolute next-frame deadline — the single pacing clock for the loop.
    let mut next_frame = Instant::now();
    // RFI capability is fixed for the session (probed at encoder open). Query it once so the
    // recovery path skips the always-`false` invalidate call on encoders without NVENC RFI and
    // forces a keyframe directly instead.
    let mut supports_rfi = enc.caps().supports_rfi;

    // Bound consecutive capture-loss rebuilds (a delivered frame clears the counter) so a permanently
    // dead source can't loop forever — it ends the stream after the cap, falling back to a reconnect.
    const MAX_REBUILDS: u32 = 5;
    let mut rebuilds: u32 = 0;
    // Encode-stall recovery, the GameStream twin of the native path's ladder (native/stream.rs,
    // `reset_stalled_encoder`): a submit/poll failure or a silent stall rebuilds the encoder in
    // place — bounded — instead of ending the stream. The backends deliberately turn a wedged
    // GPU into a bounded error so the caller can do exactly this; without the ladder here, every
    // such error cost a Moonlight client a full disconnect/reconnect. `last_au_at` feeds the
    // silent-wedge watchdog below (backends whose non-blocking poll returns `None` forever
    // instead of erroring); every received AU clears the reset budget.
    const MAX_ENCODER_RESETS: u32 = 5;
    let mut encoder_resets: u32 = 0;
    let mut last_au_at = Instant::now();

    // Coalesce forced keyframes. Under loss Moonlight spams IDR/RFI requests; on an encoder without
    // RFI (VAAPI/AMD — `supports_rfi=false`) each one becomes a full IDR, so an un-coalesced request
    // stream turns EVERY frame into a 4K IDR, saturates the send path, and collapses the session
    // instead of recovering. One fresh IDR already resolves all pending loss, so after emitting one
    // we ignore further keyframe requests for a short in-flight window (~2 frames). NVENC
    // ref-invalidation (cheap, no IDR spike) is never rate-limited — only full keyframes are.
    let keyframe_coalesce = frame_interval * 2;
    let mut last_keyframe: Option<Instant> = None;
    // A frame dropped at the pipeline head (below) breaks the reference chain for the following
    // P-frames: the client never receives it, but the encoder advanced its references past it, and —
    // packetization being downstream now — a dropped frame consumes no frameIndex for the client to
    // detect the gap. So the host re-anchors itself: a drop arms a keyframe on the next iteration,
    // routed through the same coalesce gate as client IDR requests so a burst of drops (congestion)
    // can't become an IDR storm.
    let mut recover_after_drop = false;
    // The stream's wire frameIndex numbering, owned HERE (the index of the next AU handed to the
    // packetizer thread; a dropped-at-the-queue frame consumes none). A submission's future index
    // is `au_seq + enc_inflight` (AUs are emitted FIFO, one per submission); passing it to
    // `Encoder::submit_indexed` keeps the encoder's RFI bookkeeping 1:1 with Moonlight's frame
    // numbers across the in-place encoder rebuild above (an internal counter would desync there).
    // A pipeline-head drop desyncs the prediction by the dropped AU count for the frames already
    // in flight — bounded and self-healing: the drop arms `recover_after_drop`, whose forced IDR
    // resets the encoder's reference state (stale LTR/DPB bookkeeping dies with it).
    let mut au_seq: u32 = 0;
    let mut enc_inflight: u32 = 0;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();
        // Measure per-stage timing when `PUNKTFUNK_PERF` is set OR a web-console stats capture is
        // armed (cheap Relaxed atomic, re-read each frame).
        let measure = perf || stats.is_armed();
        // Advance to the freshest captured frame if one arrived; otherwise reuse the last.
        match capturer.try_latest() {
            Ok(Some(f)) => {
                frame = f;
                uniq += 1;
                rebuilds = 0; // a delivered frame clears the consecutive-loss counter
            }
            Ok(None) => {} // no new frame — reuse the last (static/idle desktop)
            Err(e) => {
                // The capture source went away — the compositor was torn down on a Desktop<->Game
                // switch, or the virtual output was removed. On the virtual-display path, re-detect the
                // now-live compositor and re-attach IN PLACE (the send thread + packetizer + socket +
                // RTP clock all survive), then force an IDR so Moonlight resyncs — so the stream FOLLOWS
                // the switch with no client reconnect. Build the new source BEFORE dropping the old.
                // Bounded by a counter + a ~40s budget; on exhaustion, end the stream (Moonlight
                // reconnect). The portal/synthetic path has no rebuild closure → propagate as before.
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
                // Re-open the encoder for the new source (same negotiated WxH → same SPS profile) and
                // force an IDR so Moonlight resyncs on the first emitted AU.
                enc = encode::open_video(
                    cfg.codec,
                    frame.format,
                    frame.width,
                    frame.height,
                    cfg.fps,
                    cfg.bitrate_kbps as u64 * 1000,
                    frame.is_cuda(),
                    gs_bit_depth(frame.format),
                    encode::ChromaFormat::Yuv420, // GameStream stays 4:2:0
                    cursor_blend,                 // same capture cursor mode — see the first open
                    cfg.slices,                   // client slicing ceiling — see the first open
                )
                .context("reopen encoder after rebuild")?;
                // A rebuilt encoder starts unconfigured — same reason as the first open above.
                enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
                supports_rfi = enc.caps().supports_rfi;
                enc.request_keyframe();
                last_keyframe = Some(Instant::now());
                next_frame = Instant::now();
                // The old encoder died with its in-flight submissions — their AUs will never
                // arrive, so the numbering prediction restarts at `au_seq` (the fresh encoder's
                // reference state is empty, so the reused predictions meet no stale bookkeeping).
                enc_inflight = 0;
                tracing::info!("gamestream: source rebuilt — stream continues");
                continue;
            }
        }
        let t_cap = tick.elapsed();
        // Honor a client recovery request. Prefer reference-frame invalidation (the encoder
        // re-references an older still-valid frame — no costly IDR spike); if the encoder can't
        // invalidate (range too old, or no NVENC RFI) it returns false and we force a keyframe.
        // A prior pipeline drop needs a fresh keyframe to re-anchor the reference chain (see
        // below). Consumed only when the keyframe is actually EMITTED (in the coalesce gate) —
        // read-and-clear here let the gate swallow the request for good.
        let mut want_keyframe = recover_after_drop;
        if let Some((first, last)) = rfi_range.lock().unwrap().take() {
            // Prefer reference-frame invalidation when the encoder supports it (no costly IDR
            // spike); otherwise — or if the range is too old to invalidate — fall back to a keyframe.
            // Sanity-cap the range first: wider than RFI_MAX_RANGE exceeds any encoder's reference
            // history (or is a phantom range from a desynced counter) — keyframe, never a
            // force-reference that could ship corruption as a clean frame.
            let width = (last as u32).wrapping_sub(first as u32);
            if width > punktfunk_core::packet::RFI_MAX_RANGE
                || !(supports_rfi && enc.invalidate_ref_frames(first, last))
            {
                want_keyframe = true;
            }
        }
        // An explicit IDR request (or a rangeless RFI) asks for a keyframe so the client resyncs
        // immediately instead of waiting for the next GOP boundary.
        if force_idr.swap(false, Ordering::SeqCst) {
            want_keyframe = true;
        }
        // Coalesce: emit at most one forced keyframe per in-flight window, so a burst of recovery
        // requests during one loss event doesn't turn every frame into a full IDR (see above).
        if want_keyframe {
            let now = Instant::now();
            let emit = match last_keyframe {
                Some(t) => now.duration_since(t) >= keyframe_coalesce,
                None => true,
            };
            if emit {
                enc.request_keyframe();
                last_keyframe = Some(now);
                // A drop-recovery request is satisfied by an EMITTED keyframe, not by being
                // read: coalesced away it would be lost — never retried — leaving duplicate wire
                // indices in the encoder's reference table for a later RFI to anchor on (the
                // stale-anchor case rfi.rs exists to prevent). Keep it armed until this point.
                recover_after_drop = false;
            } else {
                tracing::debug!("video: keyframe request coalesced (IDR still in flight)");
            }
        }
        if let Err(e) = enc.submit_indexed(&frame, au_seq.wrapping_add(enc_inflight)) {
            // The input half of an encode stall (see native/stream.rs): rebuild the encoder in
            // place instead of ending the stream. A backend without an in-place rebuild
            // (`reset` = false) or an exhausted budget still fails the session, with the cause.
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
            // The owed AUs died with the discarded encoder state; numbering restarts at `au_seq`,
            // and the rebuilt encoder's reference state is empty so the reused predictions meet
            // no stale bookkeeping (same reasoning as the capture rebuild above). The IDR
            // bypasses the coalesce gate: a rebuilt encoder MUST resync the client.
            enc_inflight = 0;
            enc.request_keyframe();
            last_keyframe = Some(Instant::now());
            last_au_at = Instant::now();
            tracing::warn!(error = %format!("{e:#}"), reset = encoder_resets,
                max = MAX_ENCODER_RESETS,
                "encoder submit failed — encoder rebuilt in place, forcing an IDR");
            // Real backoff between attempts, not a frame period: five instant retries burn out
            // inside one driver hiccup (the native ladder's 2026-07 field lesson).
            let backoff =
                frame_interval.max(Duration::from_millis(100u64 << (encoder_resets - 1).min(4)));
            next_frame = Instant::now() + backoff;
            std::thread::sleep(backoff);
            continue;
        }
        enc_inflight = enc_inflight.wrapping_add(1);
        let t_enc = tick.elapsed();

        // 90 kHz RTP timestamp from wall-clock, so a variable capture rate stays correct.
        let ts = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
        // Drain the encoder's access units (owned buffers) — FEC/packetization runs on the
        // packetizer thread, off this loop, so it never serializes behind encode. Each AU is
        // stamped with its wire frameIndex here (`au_seq + position`); the numbering only
        // ADVANCES if the batch is actually enqueued below (a dropped batch consumes none).
        let mut aus: Vec<(Vec<u8>, FrameType, u32)> = Vec::new();
        // A poll error is the output half of an encode stall (e.g. a bounded fence timeout from
        // a wedged GPU) — carry it to the shared stall recovery below, after the AUs already
        // drained are handed off, instead of killing the session outright.
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
            // Every AU proves the encoder is alive.
            last_au_at = Instant::now();
            encoder_resets = 0;
        }
        let t_pkt = tick.elapsed();

        // Hand the frame's AUs to the pipeline; never block here. A full queue means the pipeline
        // (packetizer, or the paced sender behind it) is behind — drop this frame (FEC/RFI covers the
        // client) and keep encoding, so a downstream stall can never cap the encode rate.
        if !aus.is_empty() {
            let batch_len = aus.len() as u32;
            match raw_tx.try_send(RawFrame { aus, ts }) {
                Ok(()) => {
                    sent_batches += 1;
                    au_seq = au_seq.wrapping_add(batch_len);
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    dropped_batches += 1;
                    recover_after_drop = true; // re-anchor the reference chain on the next frame
                    if dropped_batches.is_power_of_two() {
                        tracing::warn!(
                            dropped_batches,
                            "video: pipeline queue full — frame dropped"
                        );
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    break; // packetizer/sender exited (client gone)
                }
            }
        }
        // Encode-stall recovery, the poll half (mirrors the native path's watchdog): an explicit
        // poll error, or no AU within the window while frames are owed — the silent wedge, where
        // a non-blocking poll returns `None` forever and nothing else ever errors. The window
        // scales with the frame interval so low-fps modes can't false-trip.
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
            // `poll` = drain the encoder's AUs; `enqueue` = hand-off to the pipeline. FEC/packetize
            // and the paced send now run on their own threads, off this loop — so both of these
            // should be small; if they aren't, the encode loop is being stalled by pipeline
            // backpressure (a full queue), which is the signal that a downstream stage can't keep up.
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
            // Bytes handed to the wire this window, tallied by the packetizer thread (goodput).
            let win_bytes = goodput.swap(0, std::sync::atomic::Ordering::Relaxed);
            if perf {
                // Max µs/stage this second on the ENCODE loop: cap=drain channel, enc=submit
                // (zero-copy device copy + NVENC), pkt=poll (AU drain), send=enqueue to the pipeline.
                // FEC/packetize and the paced send run on their own threads now, so pkt/send here
                // should be near-zero — a nonzero value means encode is being stalled by pipeline
                // backpressure. `uniq`=new captured frames (vs re-encoded).
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
            // Web-console capture: build the aggregated sample. The host send side exposes no
            // receiver-side packet loss / FEC-recovery / send-buffer EAGAIN counters, so those stay
            // 0 (not fabricated); `frames_dropped` is the per-frame pipeline-queue overflow delta.
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
                    t_ms: 0, // stamped by push_sample from the capture's monotonic start
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
                    ],
                    fps: (uniq as f64 / secs) as f32,
                    repeat_fps: (fps_count.saturating_sub(uniq) as f64 / secs) as f32,
                    mbps: (win_bytes as f64 * 8.0 / secs / 1_000_000.0) as f32,
                    bitrate_kbps: cfg.bitrate_kbps,
                    frames_dropped: dropped_batches.saturating_sub(last_dropped_batches) as u32,
                    packets_dropped: 0,
                    send_dropped: 0,
                    fec_recovered: 0,
                };
                stats.push_sample(session_id, sample);
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
        // Single pacing authority: hold a steady cadence at the target rate from an absolute
        // clock. No double-sleep. If a slow frame put us behind, resync to now rather than
        // bursting to catch up.
        next_frame += frame_interval;
        match next_frame.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next_frame = Instant::now(),
        }
    }
    Ok(())
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

    /// What the compat plane decides to track. The negative cases are the load-bearing ones: a
    /// Desktop entry launches nothing, so it must take no lease at all — the feature has to stay
    /// completely inert for a plain desktop stream.
    #[test]
    fn only_an_entry_that_launches_something_gets_tracked() {
        // Nothing selected (no `/launch` app) → nothing to track.
        assert!(resolve_gs_app(None).is_none());
        // Desktop: a title with no command and no library id.
        assert!(resolve_gs_app(Some(&entry("Desktop", None))).is_none());
        // A blank command is the same as none.
        assert!(resolve_gs_app(Some(&entry("Blank", Some("   ")))).is_none());

        // An operator-typed apps.json command: no library entry behind it, so the title is the whole
        // identity and there is no store-qualified id for the console to match box art on.
        let t = resolve_gs_app(Some(&entry(
            "Steam Big Picture",
            Some("  steam -gamepadui  "),
        )))
        .expect("a command entry is tracked");
        assert_eq!(t.command.as_deref(), Some("steam -gamepadui"));
        assert_eq!(t.game.id, None);
        assert_eq!(t.game.store, None);
        assert_eq!(t.game.title, "Steam Big Picture");
        // `steam` is a PATH lookup, not an absolute executable, so nothing is asserted about the
        // process — the host's own child is what tracks it (see `library::spec_from_command`).
        assert!(t.detect.is_empty());

        // A titleless entry still shows up as something a human can read.
        let t = resolve_gs_app(Some(&entry("", Some("/opt/game/run")))).expect("tracked");
        assert_eq!(t.game.title, "/opt/game/run");
    }

    /// End-to-end check of the send thread: batches pushed on the channel arrive, complete and
    /// byte-identical, at a peer socket via the paced sendmmsg path.
    #[test]
    fn sender_delivers_batches() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Generous: on a CI host saturated by parallel release builds, this thread can be
        // starved for whole seconds between recv() wakeups.
        rx_sock
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let tx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx_sock.connect(rx_sock.local_addr().unwrap()).unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
        spawn_sender(
            tx_sock,
            rx,
            Duration::from_millis(8), // ~120fps frame interval
            running.clone(),
            Arc::new(|| {}),
        )
        .unwrap();

        // 3 frames of 20 packets, content-tagged for verification. The TOTAL burst must fit
        // the receive socket's DEFAULT buffer even if this thread never drains concurrently
        // (a starved CI runner): a 1200 B datagram costs ~2.5 KB kernel truesize, and the
        // default rmem (~212 KB) holds only ~80 — a bigger burst gets silently dropped by
        // the kernel and the test can never complete (the old 3×100 flaked exactly there).
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
        drop(tx); // sender drains then exits

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
    }
}
