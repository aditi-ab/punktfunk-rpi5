//! The monitor-mirror display backend: stream a **physical** head the compositor already has,
//! instead of creating a virtual one. See `design/per-monitor-portal-capture.md`.
//!
//! It implements [`VirtualDisplay`] so it drops into the session machinery unchanged — but it
//! creates nothing, and that difference is load-bearing:
//!
//! * [`DisplayOwnership::External`] ("someone else's display, merely mirrored: no keep-alive, no
//!   topology, no reuse") is exactly the contract for a head we don't own, so the registry's
//!   pooling/linger/exclusive machinery passes it through. §7.1 of the design doc — never disable,
//!   re-position or "restore" a monitor the user is sitting in front of — holds *by construction*
//!   here rather than by everyone remembering it.
//! * `create()` ignores the requested [`Mode`]. A panel runs at the mode the user set; the client
//!   scales. Reconfiguring someone's physical display to match a phone would be the same class of
//!   rudeness as blanking it (§7.3).
//!
//! Selection is a **host-level pin** (`PUNKTFUNK_CAPTURE_MONITOR`), not a per-session choice —
//! the host-pinned decision of record (§5.3), which is also what makes the input anchor below
//! sound: one pin for the whole host, so the shared injector can't be re-aimed per session.

use super::backend::{DisplayOwnership, VirtualDisplay, VirtualOutput};
use super::monitors;
use crate::{Compositor, Mode};
use anyhow::{bail, Context, Result};

/// What a backend hands back when it starts recording an existing head.
///
/// `remote_fd` is the split: KWin and Mutter publish the node on the user's own PipeWire daemon
/// (nothing to carry), while the portal-based backends (sway/xdpw, Hyprland/xdph) hand back a
/// sandboxed remote fd that the capturer must connect through — the same distinction their
/// *virtual*-output paths already make.
pub(crate) struct MirrorStream {
    pub node_id: u32,
    pub remote_fd: Option<std::os::fd::OwnedFd>,
    /// The cursor mode the xdg ScreenCast portal NEGOTIATED for this recording, for the two
    /// portal-based backends; `None` for the compositor-protocol ones (KWin/Mutter/gamescope),
    /// which get what they ask for. Reported on to the host as
    /// [`VirtualDisplay::last_portal_cursor_mode`] — same split as `remote_fd` above, and for the
    /// same reason: only the portal path has an answer that can differ from the request.
    pub cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Dropping this ends the recording. It never owns the monitor — we did not create it.
    pub keepalive: Box<dyn Send>,
}

/// Streams an existing monitor, named by connector.
pub struct MirrorDisplay {
    compositor: Compositor,
    connector: String,
    hw_cursor: bool,
    /// What the portal gave the most recent [`create`](VirtualDisplay::create), when this mirror
    /// delegated to a portal-based backend. See [`VirtualDisplay::last_portal_cursor_mode`].
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
}

impl MirrorDisplay {
    pub fn new(compositor: Compositor, connector: String) -> Result<Self> {
        Ok(MirrorDisplay {
            compositor,
            connector,
            hw_cursor: false,
            last_cursor_mode: None,
        })
    }
}

impl VirtualDisplay for MirrorDisplay {
    fn name(&self) -> &'static str {
        "mirror"
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn poolable_now(&self) -> bool {
        // Never. `create` below always reports `DisplayOwnership::External` — we did not make this
        // head and must not keep it — so the registry never pools a mirror, and the trait's `true`
        // default was a claim this backend cannot honour on any request. It costs nothing today
        // (the reuse lookup can only miss: no `"mirror"` entry ever enters the pool), but it is the
        // answer the registry consults BEFORE `create` gets to declare ownership, so leaving it
        // optimistic means the one pre-create statement of intent contradicts the post-create fact.
        false
    }

    fn create(&mut self, _mode: Mode) -> Result<VirtualOutput> {
        // Resolve the pin against the live head list FIRST: it yields the geometry the input anchor
        // needs, and it turns "that monitor is gone" into one clear error before any compositor
        // call. `resolve` refuses to substitute a different head (see its doc).
        let monitors = monitors::list(self.compositor)
            .with_context(|| format!("enumerate monitors to mirror {:?}", self.connector))?;
        let target = monitors::resolve(&monitors, &self.connector)?;
        check_mirrorable(target, self.compositor)?;
        let origin = (target.x, target.y);
        let dims = (target.width, target.height, refresh_hz(target.refresh_mhz));

        let stream = match self.compositor {
            #[cfg(target_os = "linux")]
            Compositor::Kwin => {
                crate::kwin::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Mutter => {
                crate::mutter::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Wlroots => {
                crate::wlroots::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Hyprland => {
                crate::hyprland::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            // gamescope on the DRM backend (a Bazzite/SteamOS Game Mode session) DOES drive a real
            // head — `monitors::list` reports it, and mirroring it is an attach to the composited
            // node this session already publishes. A nested or headless gamescope reports no heads,
            // so `resolve` fails above and this arm is never reached for one.
            #[cfg(target_os = "linux")]
            Compositor::Gamescope => {
                crate::gamescope::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            // Gated to non-Linux (`monitors::list`'s shape), NOT the bare `#[allow(unreachable_
            // patterns)] other =>` this replaced: with it, a newly added `Compositor` variant fell
            // through to a runtime bail on the very platform that would define it, silently, in the
            // one place that decides which backends can mirror a head. Cfg'd out on Linux, the match
            // is exhaustive and the new variant is a compile error here instead. The arm exists at
            // all only because every arm above is itself `cfg(target_os = "linux")` — this module is
            // Linux-only today, so it is a placeholder that keeps the shape honest if that changes.
            #[cfg(not(target_os = "linux"))]
            other => bail!(
                "mirroring an existing monitor is not supported on the {} backend",
                other.id()
            ),
        };

        // Latched for `last_portal_cursor_mode` — the delegate's verdict is this mirror's verdict.
        self.last_cursor_mode = stream.cursor_mode;

        // NOTE: aiming absolute input at this head is the HOST's job, not ours — this crate must
        // not depend on pf-inject (see the crate doc: "never on capture/inject"). The host sets the
        // anchor from the same pin at startup; §7.2 of the design doc explains why it is host-level
        // rather than set here per session. We only CARRY the head's name out (`output_name`
        // below), which is what the wlr injector needs to bind its virtual pointer to this head —
        // the libei anchor above cannot serve it, because that backend selects by region.
        tracing::info!(
            connector = %target.connector,
            mode = %target.mode_label(),
            at = %format!("+{}+{}", origin.0, origin.1),
            node_id = stream.node_id,
            "mirroring an existing monitor (no virtual display created)"
        );

        // The keepalive IS the recording: dropping it stops the cast and leaves the monitor exactly
        // as it was (we created nothing, so there is nothing to restore).
        let mut out = VirtualOutput::owned(stream.node_id, Some(dims), stream.keepalive);
        // Portal-based backends (sway/xdpw, Hyprland/xdph) publish on a sandboxed remote the
        // capturer must connect through; KWin/Mutter use the user's own daemon.
        out.remote_fd = stream.remote_fd;
        // Never pooled, never lingered, never made primary/exclusive: we don't own this head.
        out.ownership = DisplayOwnership::External;
        // The head absolute input maps into is the one we mirror — its connector IS its
        // `wl_output.name` on the wlroots/Hyprland backends, where the injector matches on it.
        out.output_name = Some(target.connector.clone());
        Ok(out)
    }
}

/// Can this head be mirrored at all? Each rejection is something a compositor would otherwise
/// answer with silence or a black stream, so it is caught here where the reason can be stated.
fn check_mirrorable(target: &monitors::PhysicalMonitor, compositor: Compositor) -> Result<()> {
    if !target.enabled {
        bail!(
            "monitor {:?} is disabled — enable it before streaming it",
            target.connector
        );
    }
    if target.managed {
        // `managed` is only *conclusive* where the name is ours by construction: KWin's
        // `Virtual-punktfunk` prefix, Hyprland's `PF-N`. Sway names EVERY headless output
        // `HEADLESS-N` — its own included — so refusing there would block a legitimate setup
        // (a headless sway box whose outputs are all HEADLESS-N is exactly the remote case this
        // feature serves). Warn and continue; a user who really did pin our own virtual display
        // sees a mirror of it and the log says why.
        if names_ours_conclusively(compositor) {
            bail!(
                "monitor {:?} is one of punktfunk's own virtual displays, not a physical head — \
                 clear the streamed-screen setting to use the normal virtual-display path",
                target.connector
            );
        }
        tracing::warn!(
            connector = %target.connector,
            "the pinned monitor looks like a headless output — on this compositor that name is \
             ambiguous (it may be one of punktfunk's own virtual displays), mirroring it anyway"
        );
    }
    if target.width == 0 || target.height == 0 {
        bail!(
            "monitor {:?} reports no current mode ({}x{}) — it is not driving a signal",
            target.connector,
            target.width,
            target.height
        );
    }
    Ok(())
}

/// Does this compositor's `managed` flag mean "ours, for certain"?
///
/// EXHAUSTIVE on purpose, unlike the `matches!` it used to be. This is the one table in the crate
/// whose un-listed default is the UNSAFE direction: a `false` sends [`check_mirrorable`] down the
/// warn-and-proceed branch, which for a backend that DOES name its managed outputs by construction
/// (the KWin/Hyprland shape — i.e. both backends that have the property today) means streaming
/// punktfunk's own virtual display back to the client, the capture loop
/// `one_of_our_own_virtual_displays_is_refused` exists to forbid. Adding a `Compositor` variant must
/// therefore be a compile error here rather than a silent opt-out. (Contrast
/// [`Compositor::needs_live_session`], also a `matches!` — its omitted default is the safe one.)
fn names_ours_conclusively(compositor: Compositor) -> bool {
    match compositor {
        // Ours by construction: KWin outputs carry the `Virtual-punktfunk-<id>` name the identity
        // module hands the backend, Hyprland's are `PF-N`. Nothing else mints those names.
        Compositor::Kwin | Compositor::Hyprland => true,
        // Sway names EVERY headless output `HEADLESS-N`, its own included; Mutter's virtual monitors
        // carry no distinguishing name at all (it won't take one from us); and gamescope's
        // `list_monitors` only ever reports the real DRM head a Game Mode session drives, so
        // `managed` is never even set there. A hint at most — refusing would break the legitimate
        // headless-sway setup this feature serves.
        Compositor::Wlroots | Compositor::Mutter | Compositor::Gamescope => false,
    }
}

/// mHz → whole Hz for [`VirtualOutput::preferred_mode`], never 0 (the negotiation treats 0 as
/// "unset", and a head that didn't report a refresh is still running at *some* rate — 60 is the
/// same assumption KWin's own virtual outputs are born with).
fn refresh_hz(mhz: u32) -> u32 {
    let hz = (mhz as f64 / 1000.0).round() as u32;
    if hz == 0 {
        60
    } else {
        hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(connector: &str) -> monitors::PhysicalMonitor {
        monitors::PhysicalMonitor {
            connector: connector.into(),
            description: "ACME 27".into(),
            width: 2560,
            height: 1440,
            refresh_mhz: 144_000,
            x: 1920,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled: true,
            managed: false,
        }
    }

    #[test]
    fn a_real_enabled_head_is_mirrorable() {
        assert!(check_mirrorable(&head("DP-2"), Compositor::Kwin).is_ok());
    }

    /// Streaming a dark head would be a black rectangle with no explanation — say why instead.
    #[test]
    fn a_disabled_head_is_refused_with_the_reason() {
        let mut m = head("DP-2");
        m.enabled = false;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    /// Mirroring our OWN virtual display is a loop with extra steps; catch the misconfiguration
    /// rather than streaming something confusing.
    #[test]
    fn one_of_our_own_virtual_displays_is_refused() {
        let mut m = head("Virtual-punktfunk-1");
        m.managed = true;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("virtual displays"), "{err}");
    }

    /// Sway names every headless output `HEADLESS-N`, its own included, so the `managed` hint is
    /// NOT proof there — refusing would block a headless sway box, which is exactly the remote
    /// setup this feature serves. (Found on-glass: a two-output headless sway had both heads
    /// flagged, and the KWin-strength rule would have refused to mirror either.)
    #[test]
    fn a_headless_sway_output_is_mirrored_despite_the_ambiguous_name() {
        let mut m = head("HEADLESS-2");
        m.managed = true;
        assert!(check_mirrorable(&m, Compositor::Wlroots).is_ok());
        // Hyprland's `PF-N` IS our naming, so it stays conclusive.
        assert!(check_mirrorable(&m, Compositor::Hyprland).is_err());
    }

    /// Pin the conclusive-naming table per variant. The answer is a safety decision whose wrong
    /// direction is the SILENT one: a backend that mints punktfunk-named outputs but is missing
    /// from the `true` arm takes the warn-and-proceed branch and streams our own virtual display
    /// back to the client. Exhaustive `match` + this test = the new variant has to be considered.
    #[test]
    fn the_conclusive_naming_table_is_pinned_per_backend() {
        assert!(names_ours_conclusively(Compositor::Kwin));
        assert!(names_ours_conclusively(Compositor::Hyprland));
        assert!(!names_ours_conclusively(Compositor::Wlroots));
        assert!(!names_ours_conclusively(Compositor::Mutter));
        assert!(!names_ours_conclusively(Compositor::Gamescope));
    }

    /// The registry asks `poolable_now` BEFORE `create` gets to report ownership, so the two must
    /// agree: a mirror's `create` always reports `External` (we did not make this head), therefore
    /// no mirror request is ever poolable.
    #[test]
    fn a_mirrored_head_is_never_registry_poolable() {
        let vd = MirrorDisplay::new(Compositor::Kwin, "DP-2".into()).unwrap();
        assert!(!vd.poolable_now());
    }

    /// A head listed but not driving a mode (enabled yet modeless) would negotiate a 0x0 stream.
    #[test]
    fn a_head_with_no_current_mode_is_refused() {
        let mut m = head("DP-2");
        m.width = 0;
        m.height = 0;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no current mode"), "{err}");
    }

    #[test]
    fn refresh_rounds_and_never_reports_zero() {
        assert_eq!(refresh_hz(60000), 60);
        assert_eq!(refresh_hz(59940), 60);
        assert_eq!(refresh_hz(119_920), 120);
        // An unreported refresh must not become a 0 Hz preferred mode.
        assert_eq!(refresh_hz(0), 60);
    }
}
