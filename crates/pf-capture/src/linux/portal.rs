//! The portal CONTROL PLANE: the xdg ScreenCast / RemoteDesktop handshake (async, `ashpd` over
//! zbus, on its own tokio runtime), the cursor-mode choice, and GNOME's BT.2100 colour-mode probe.
//!
//! Split out of `linux/mod.rs` (sweep Phase 5.3) to separate the async control plane from the
//! realtime half: nothing here runs per frame — the handshake happens once, then the thread parks
//! until `PortalSession`'s `Drop` (in the parent) releases it, and DROPPING the zbus
//! connection is what ends the compositor's cast. The probe is likewise a one-shot D-Bus round-trip
//! for a control-plane caller.

use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;

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
/// PipeWire remote, hand the fd + node id back, then keep the session alive until `quit_rx`
/// resolves (the capturer's `Drop` — see [`PortalSession`]).
pub(super) fn portal_thread(
    setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
) {
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
            // has no `Drop`) — which now happens when this park returns, not at process exit.
            let _keep_alive = (&proxy, &session);
            let _ = quit_rx.await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
    // Drop the runtime HERE, before the caller signals completion: shutting the 2 workers down is
    // what finishes releasing the zbus connection, so a `done` signal sent after this means the
    // compositor-side session is really gone (see `PortalSession::drop`).
    drop(rt);
}

/// Combined RemoteDesktop+ScreenCast portal setup (KWin/GNOME). ScreenCast sources are selected
/// on a session created via RemoteDesktop, so a single RemoteDesktop `start` grant —
/// pre-authorized headlessly via the `kde-authorized` permission, exactly like the libei input
/// path — also covers screen capture, with no separate ScreenCast dialog (which has no such
/// bypass). Yields the same PipeWire fd + node id as the standalone path; the consumer is
/// identical, as is the `quit_rx` teardown park (see [`PortalSession`]).
pub(super) fn portal_thread_remote_desktop(
    setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
) {
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

            // Keep the proxies + session (and their zbus connection) alive for the capture, until
            // the capturer's `Drop` fires the quit channel.
            let _keep_alive = (&remote, &screencast, &session);
            let _ = quit_rx.await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
    // See `portal_thread`: drop the runtime before the caller's completion signal.
    drop(rt);
}
