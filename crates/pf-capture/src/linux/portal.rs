//! xdg ScreenCast / RemoteDesktop control plane: ashpd handshake on a dedicated
//! tokio runtime, cursor-mode ladder, and GNOME's BT.2100 colour-mode probe.
//!
//! Nothing here is per-frame. The handshake runs once; the thread then parks until
//! `PortalSession`'s `Drop` (parent module) fires `quit_rx`. ashpd's `Session` has
//! no `Drop` — releasing the zbus connection is what ends the compositor's cast.
//! Drop the runtime before signalling done so that session is actually gone.
//!
//! HDR offer is scoped to `PUNKTFUNK_CAPTURE_MONITOR` when set; unpinned it is
//! "any head in BT.2100". See `design/per-monitor-portal-capture.md`. The probe
//! is one session-bus round-trip; call from control-plane threads only.

use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;

/// Mutter advertises 10-bit PQ only while the mirrored head is BT.2100.
/// `false` on any error (not GNOME, no colour modes, no monitors) so the
/// caller offers SDR. Blocking session-bus round-trip; control-plane only.
///
/// When `PUNKTFUNK_CAPTURE_MONITOR` is set, only that connector counts — an
/// HDR neighbour must not pull PQ onto an SDR panel. Unpinned: any head.
/// See `design/per-monitor-portal-capture.md`.
pub fn gnome_hdr_monitor_active() -> bool {
    use ashpd::zbus;
    // `color-mode` is on the monitor properties dict, not the logical-monitor one.
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
        // zbus is async-only here (ashpd's tokio). Throwaway current-thread runtime:
        // one round-trip, not the handshake path that needs a pumped reader.
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

/// Pinned: only that connector's BT.2100 bit. A pin that names no live head is
/// SDR, not "any" — the session is about to fail on that missing monitor, and an
/// HDR offer would be a second wrong answer it would not fail on. Unpinned: any head.
fn hdr_offer_for(heads: &[(&str, bool)], pinned: Option<&str>) -> bool {
    match pinned {
        Some(want) => heads
            .iter()
            .find(|(connector, _)| connector.eq_ignore_ascii_case(want))
            .is_some_and(|(_, hdr)| *hdr),
        None => heads.iter().any(|(_, hdr)| *hdr),
    }
}

/// Ladder against `AvailableCursorModes`. Metadata keeps the HW cursor plane
/// and ships `SPA_META_Cursor`; Embedded burns the pointer into every frame
/// (gamescope: that defeats its HW plane). Prefer Metadata when `want_metadata`;
/// otherwise Embedded — a metadata cursor with no blend stage is never drawn.
/// A failed query defaults Embedded so an older portal does not silently hide it.
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
            // Embedded wanted, not offered. Metadata still beats Hidden: the CPU
            // path composites `SPA_META_Cursor` inline.
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

/// Handshake then park on `quit_rx`. ashpd `Session` has no `Drop`; holding
/// the zbus connection is what keeps the compositor's cast alive.
pub(super) fn portal_thread(
    setup_tx: std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
    want_metadata_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Multi-thread: zbus's background reader must stay pumped across
    // create_session → select_sources → start, or the portal returns
    // "Invalid session". A current-thread runtime starves it.
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
                        // wlroots advertises MONITOR only (`AvailableSourceTypes=1`).
                        // Asking for an unsupported type invalidates the session.
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

            // Hold `proxy` + `session` (the zbus connection). ashpd `Session` has
            // no `Drop`; the compositor ends the cast when that connection drops.
            let _keep_alive = (&proxy, &session);
            let _ = quit_rx.await;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
    // Drop the runtime before the caller signals done: the two workers finishing
    // is what releases the zbus connection, so the compositor session is gone
    // (`PortalSession::drop`).
    drop(rt);
}

/// RemoteDesktop+ScreenCast on one session (KWin/GNOME). Sources are selected on
/// a RemoteDesktop session so a single `start` grant — the `kde-authorized`
/// headless bypass, same as libei — covers capture. ScreenCast has no such
/// bypass; a standalone path would show a dialog. Same fd + node id, same
/// `quit_rx` park as [`portal_thread`].
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
            // Device selection is required even though this session never
            // `connect_to_eis` (inject has its own). Without it, `start` is not
            // the RemoteDesktop grant `kde-authorized` covers.
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

            // Same as `portal_thread`: hold the zbus connection until `quit_rx`.
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
