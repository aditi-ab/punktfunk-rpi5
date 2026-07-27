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

/// Streams an existing monitor, named by connector.
pub struct MirrorDisplay {
    compositor: Compositor,
    connector: String,
    hw_cursor: bool,
}

impl MirrorDisplay {
    pub fn new(compositor: Compositor, connector: String) -> Result<Self> {
        Ok(MirrorDisplay {
            compositor,
            connector,
            hw_cursor: false,
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

    fn create(&mut self, _mode: Mode) -> Result<VirtualOutput> {
        // Resolve the pin against the live head list FIRST: it yields the geometry the input anchor
        // needs, and it turns "that monitor is gone" into one clear error before any compositor
        // call. `resolve` refuses to substitute a different head (see its doc).
        let monitors = monitors::list(self.compositor)
            .with_context(|| format!("enumerate monitors to mirror {:?}", self.connector))?;
        let target = monitors::resolve(&monitors, &self.connector)?;
        check_mirrorable(target)?;
        let origin = (target.x, target.y);
        let dims = (target.width, target.height, refresh_hz(target.refresh_mhz));

        let (node_id, keepalive) = match self.compositor {
            #[cfg(target_os = "linux")]
            Compositor::Kwin => {
                crate::kwin::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            other => bail!(
                "mirroring an existing monitor is not implemented for the {} backend yet — \
                 unset PUNKTFUNK_CAPTURE_MONITOR",
                other.id()
            ),
        };

        // NOTE: aiming absolute input at this head is the HOST's job, not ours — this crate must
        // not depend on pf-inject (see the crate doc: "never on capture/inject"). The host sets the
        // anchor from the same pin at startup; §7.2 of the design doc explains why it is host-level
        // rather than set here per session.
        tracing::info!(
            connector = %target.connector,
            mode = %target.mode_label(),
            at = %format!("+{}+{}", origin.0, origin.1),
            node_id,
            "mirroring an existing monitor (no virtual display created)"
        );

        // The keepalive IS the recording: dropping it stops the cast and leaves the monitor exactly
        // as it was (we created nothing, so there is nothing to restore).
        let mut out = VirtualOutput::owned(node_id, Some(dims), keepalive);
        // Never pooled, never lingered, never made primary/exclusive: we don't own this head.
        out.ownership = DisplayOwnership::External;
        Ok(out)
    }
}

/// Can this head be mirrored at all? Both rejections are things a compositor would answer with
/// silence or a black stream, so they are caught here where the reason can be stated.
fn check_mirrorable(target: &monitors::PhysicalMonitor) -> Result<()> {
    if !target.enabled {
        bail!(
            "monitor {:?} is disabled — enable it before streaming it",
            target.connector
        );
    }
    if target.managed {
        bail!(
            "monitor {:?} is one of punktfunk's own virtual displays, not a physical head — unset \
             PUNKTFUNK_CAPTURE_MONITOR to use the normal virtual-display path",
            target.connector
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
        assert!(check_mirrorable(&head("DP-2")).is_ok());
    }

    /// Streaming a dark head would be a black rectangle with no explanation — say why instead.
    #[test]
    fn a_disabled_head_is_refused_with_the_reason() {
        let mut m = head("DP-2");
        m.enabled = false;
        let err = check_mirrorable(&m).unwrap_err().to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    /// Mirroring our OWN virtual display is a loop with extra steps; catch the misconfiguration
    /// rather than streaming something confusing.
    #[test]
    fn one_of_our_own_virtual_displays_is_refused() {
        let mut m = head("Virtual-punktfunk-1");
        m.managed = true;
        let err = check_mirrorable(&m).unwrap_err().to_string();
        assert!(err.contains("virtual displays"), "{err}");
    }

    /// A head listed but not driving a mode (enabled yet modeless) would negotiate a 0x0 stream.
    #[test]
    fn a_head_with_no_current_mode_is_refused() {
        let mut m = head("DP-2");
        m.width = 0;
        m.height = 0;
        let err = check_mirrorable(&m).unwrap_err().to_string();
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
