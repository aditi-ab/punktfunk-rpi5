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

/// Whether the monitor this host would mirror is currently in BT.2100 (HDR) colour mode — the
/// precondition for Mutter's monitor screencast advertising the 10-bit PQ formats (GNOME 50+;
/// Mutter only appends the HDR formats while the mirrored monitor's colour state is BT.2020+PQ).
/// Queried over the session bus: `DisplayConfig.GetCurrentState`, monitor property
/// `"color-mode" == 1` (`META_COLOR_MODE_BT2100`). `false` on any error — not GNOME, a pre-48
/// Mutter without colour modes, no monitors — so callers fall back to the honest SDR offer.
/// Blocking (one D-Bus round-trip on a fresh connection); call from control-plane threads only.
///
/// **Scoped to `PUNKTFUNK_CAPTURE_MONITOR` when it is set** (`design/per-monitor-portal-capture.md`
/// §7.4). Without a pin this asks "is ANY monitor in HDR mode", which was a fair heuristic while the
/// capture path took whatever monitor it was handed — but once the operator names the head, an
/// HDR-capable *neighbour* must not talk this host into offering PQ formats for an SDR panel.
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
            // `spec.0` is the connector; "color-mode" 1 is META_COLOR_MODE_BT2100.
            let heads: Vec<(&str, bool)> = monitors
                .iter()
                .map(|(spec, _modes, props)| {
                    let hdr = props
                        .get("color-mode")
                        .and_then(|v| u32::try_from(v).ok())
                        .is_some_and(|mode| mode == 1);
                    (spec.0.as_str(), hdr)
                })
                .collect();
            Ok(hdr_offer_for(
                &heads,
                pf_host_config::config().capture_monitor.as_deref(),
            ))
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

/// Should this host offer the HDR (10-bit PQ) formats, given each head as `(connector, is_bt2100)`
/// and the `PUNKTFUNK_CAPTURE_MONITOR` pin?
///
/// Pinned: only that head's colour mode counts — an HDR-capable neighbour must not talk the host
/// into offering PQ for the SDR panel it is actually streaming. A pin naming no live head reports
/// SDR rather than falling back to "any": the session is about to fail on that same missing
/// monitor, and an over-claimed HDR offer would be a second, quieter wrong answer.
/// Unpinned: the pre-existing "any monitor is in HDR mode" heuristic, unchanged.
fn hdr_offer_for(heads: &[(&str, bool)], pinned: Option<&str>) -> bool {
    match pinned {
        Some(want) => heads
            .iter()
            .find(|(connector, _)| connector.eq_ignore_ascii_case(want))
            .is_some_and(|(_, hdr)| *hdr),
        None => heads.iter().any(|(_, hdr)| *hdr),
    }
}

/// Pick the ScreenCast cursor mode from what the backend advertises (`AvailableCursorModes`).
/// With `want_metadata` the ladder prefers **cursor-as-metadata**: the compositor keeps its cheap
/// hardware cursor plane and ships the pointer as PipeWire `SPA_META_Cursor` metadata (position +
/// an occasional bitmap), which the consumer composites itself — avoiding the producer burning the
/// cursor into every frame (`Embedded`), which on gamescope would defeat its HW cursor plane.
/// Without it — the session's encode path has no compositing stage for a metadata cursor
/// (`pf-encode`'s `cursor_blend_capable` said the resolved backend can't blend) — the ladder
/// prefers `Embedded`, so the pointer is in the pixels instead of in metadata nothing would draw.
/// Both ladders fall through to the other mode, then `Hidden`; a failed property query (an older
/// portal) keeps the prior `Embedded` behavior so the cursor is never silently lost.
async fn choose_cursor_mode(
    proxy: &ashpd::desktop::screencast::Screencast,
    want_metadata: bool,
) -> ashpd::desktop::screencast::CursorMode {
    use ashpd::desktop::screencast::CursorMode;
    match proxy.available_cursor_modes().await {
        Ok(avail) if want_metadata && avail.contains(CursorMode::Metadata) => {
            tracing::info!(
                ?avail,
                "ScreenCast: requesting cursor-as-metadata (SPA_META_Cursor)"
            );
            CursorMode::Metadata
        }
        Ok(avail) if avail.contains(CursorMode::Embedded) => {
            if want_metadata {
                tracing::info!(
                    ?avail,
                    "ScreenCast: cursor metadata unavailable — requesting Embedded cursor"
                );
            } else {
                tracing::info!(
                    ?avail,
                    "ScreenCast: requesting Embedded cursor (this session's encoder does not \
                     composite a metadata cursor)"
                );
            }
            CursorMode::Embedded
        }
        Ok(avail) if avail.contains(CursorMode::Metadata) => {
            // Embedded wanted but not offered. Metadata still beats Hidden: the CPU capture
            // path composites `SPA_META_Cursor` inline, so part of the matrix keeps a pointer.
            tracing::warn!(
                ?avail,
                "ScreenCast: Embedded cursor not advertised — requesting cursor-as-metadata \
                 (only CPU-path frames will composite it)"
            );
            CursorMode::Metadata
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
    want_metadata_cursor: bool,
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
            let cursor_mode = choose_cursor_mode(&proxy, want_metadata_cursor).await;
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
    want_metadata_cursor: bool,
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
            let cursor_mode = choose_cursor_mode(&screencast, want_metadata_cursor).await;
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

#[cfg(test)]
mod hdr_offer_tests {
    use super::hdr_offer_for;

    #[test]
    fn unpinned_keeps_the_any_monitor_heuristic() {
        assert!(hdr_offer_for(&[("DP-1", false), ("HDMI-A-1", true)], None));
        assert!(!hdr_offer_for(&[("DP-1", false)], None));
    }

    /// The regression this exists to prevent: an HDR TV on HDMI while the pinned head is an SDR
    /// desk monitor. Before scoping, the host offered PQ formats for a panel that can't show them.
    #[test]
    fn a_pin_ignores_an_hdr_neighbour() {
        let heads = [("DP-1", false), ("HDMI-A-1", true)];
        assert!(!hdr_offer_for(&heads, Some("DP-1")));
        assert!(hdr_offer_for(&heads, Some("HDMI-A-1")));
    }

    #[test]
    fn a_pin_matches_case_insensitively_like_the_resolver() {
        assert!(hdr_offer_for(&[("HDMI-A-1", true)], Some("hdmi-a-1")));
    }

    #[test]
    fn a_pin_naming_no_live_head_reports_sdr() {
        assert!(!hdr_offer_for(&[("DP-1", true)], Some("DP-9")));
    }
}
