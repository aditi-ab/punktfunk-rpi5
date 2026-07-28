//! Physical-monitor enumeration — "what heads does this host actually have?".
//!
//! This is the read-only counterpart to the rest of this crate: everything else here *creates* and
//! owns virtual outputs, while this module only *reports* the heads the compositor already has, so
//! an operator can pin capture at one of them (`PUNKTFUNK_CAPTURE_MONITOR`) and the console can
//! render a picker. See `design/per-monitor-portal-capture.md` §5.1.
//!
//! It lives in pf-vdisplay because monitor enumeration is a **per-compositor** question and this
//! crate already speaks every one of those dialects — KWin's `kde_output_device_v2`, Mutter's
//! `DisplayConfig.GetCurrentState`, `swaymsg -t get_outputs`, `hyprctl -j monitors`. Each backend's
//! implementation sits beside the code that already talks to it; this module is the shared type and
//! the dispatch.
//!
//! **The geometry is the point.** `x`/`y` are what make a head *identifiable*: two monitors can
//! share a size (and then a size-keyed match is a coin flip — see `pf-inject`'s absolute-coordinate
//! region selection), but they can never share an origin in the compositor's global space.

use crate::Compositor;
use anyhow::{bail, Result};

/// One head as the compositor currently reports it. Logical (post-scale) geometry throughout —
/// the same coordinate space libei regions and compositor layout use, *not* pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalMonitor {
    /// Connector name — `DP-1`, `HDMI-A-2`, `eDP-1`. The id `PUNKTFUNK_CAPTURE_MONITOR` names.
    pub connector: String,
    /// Human label for a picker (`make model`, else the connector). Never used for matching.
    pub description: String,
    /// Current mode, in pixels.
    pub width: u32,
    pub height: u32,
    /// Refresh in mHz (60000 = 60 Hz). 0 when the backend doesn't report it.
    pub refresh_mhz: u32,
    /// Top-left in the compositor's global logical space — the identity key (see the module doc).
    pub x: i32,
    pub y: i32,
    /// Logical scale factor (1.0 when unreported).
    pub scale: f64,
    /// The compositor's primary/focused head, when it says.
    pub primary: bool,
    /// Enabled (driven). A disabled head is still listed, so "why can't I pick it?" has an answer.
    pub enabled: bool,
    /// **Best-effort**: this output is one WE created (a managed virtual display), not a real head.
    /// Only KWin can say so reliably (managed outputs carry a name prefix); the other backends
    /// name virtual outputs indistinguishably from physical ones, so this stays `false` there
    /// rather than guessing. Callers use it to grey out nonsense choices, never to filter blindly.
    pub managed: bool,
}

/// Build the picker label from a compositor's make/model, falling back to the connector.
///
/// Compositors fill unknown fields with the literal string `"Unknown"` rather than leaving them
/// empty (seen on-glass: sway reports `"Unknown Unknown"` for a headless output), so treat that as
/// absent too — a picker row reading "Unknown Unknown" is worse than one reading "DP-1".
pub(crate) fn describe(make: &str, model: &str, connector: &str) -> String {
    let known = |s: &str| {
        let s = s.trim();
        !s.is_empty() && !s.eq_ignore_ascii_case("unknown")
    };
    let label = [make, model]
        .iter()
        .map(|s| s.trim())
        .filter(|s| known(s))
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        connector.to_string()
    } else {
        label
    }
}

impl PhysicalMonitor {
    /// `1920x1080@60` — for logs and pickers.
    pub fn mode_label(&self) -> String {
        if self.refresh_mhz == 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            format!(
                "{}x{}@{}",
                self.width,
                self.height,
                (self.refresh_mhz as f64 / 1000.0).round() as u32
            )
        }
    }
}

/// Every head `compositor` reports, in the compositor's own order.
///
/// Errors when the backend can't be reached (compositor not running, IPC unavailable) — that is
/// deliberately distinct from `Ok(vec![])`, which means "reached it, it has no heads" (a headless
/// session). Callers that only want to *offer* a picker can treat both as "nothing to show";
/// callers resolving a pinned monitor must not (see [`resolve`]).
pub fn list(compositor: Compositor) -> Result<Vec<PhysicalMonitor>> {
    match compositor {
        #[cfg(target_os = "linux")]
        Compositor::Kwin => crate::kwin_output_mgmt::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Mutter => crate::mutter::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Wlroots => crate::wlroots::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Hyprland => crate::hyprland::list_monitors(),
        // gamescope is nested: it has no physical heads of its own, and mirroring the desktop it
        // runs on is a question for the compositor UNDER it. Not an error — just nothing to offer.
        #[cfg(target_os = "linux")]
        Compositor::Gamescope => Ok(Vec::new()),
        #[cfg(not(target_os = "linux"))]
        _ => bail!("physical-monitor enumeration is implemented for the Linux backends only"),
    }
}

/// Every head Windows reports — the non-compositor counterpart to [`list`].
///
/// Windows has no compositor to ask, so this reads the same CCD database the rest of the Windows
/// backend drives and reports what it finds. Until this existed the mgmt API answered
/// `/display/monitors` on Windows with an empty list and a LINUX error string (`detect()` fell
/// through to an `XDG_CURRENT_DESKTOP` sniff), so the console could neither show the operator's
/// screen nor honestly say why.
///
/// INACTIVE heads are listed too, with zeroed geometry and `enabled: false` — the same contract
/// [`list`] documents, so "why can't I pick it?" still has an answer.
///
/// Two fields cannot mean here what they mean on Linux, and are reported honestly rather than
/// invented:
/// * `scale` is always `1.0`. Windows scaling is per-monitor DPI applied by each application, not
///   a compositor-global logical scale, so there is no factor that would make these coordinates
///   "logical" the way the module doc means. The geometry below is therefore PIXELS.
/// * `refresh_mhz` comes from the path's own rational rate, which keeps 59.94 distinct from 60.
#[cfg(windows)]
pub fn list_windows() -> Result<Vec<PhysicalMonitor>> {
    let inv = pf_win_display::win_display::target_inventory();
    if inv.is_empty() {
        // Distinguish "reached it, nothing there" from a failure, exactly as [`list`] promises:
        // an empty CCD database is a real state (every panel off — measured on .173 with the TV
        // powered down), not an error.
        return Ok(Vec::new());
    }
    Ok(inv
        .into_iter()
        .map(|t| {
            // The GDI name is what an operator recognises and what capture pins on; an inactive
            // path has none, so fall back to the stable target id rather than an empty string —
            // `resolve` matches on this, and a blank id can never be pinned.
            let connector = if t.gdi_name.is_empty() {
                format!("target-{}", t.target_id)
            } else {
                t.gdi_name
            };
            PhysicalMonitor {
                description: describe("", &t.friendly, &connector),
                connector,
                width: t.width,
                height: t.height,
                refresh_mhz: t.refresh_mhz,
                x: t.x,
                y: t.y,
                scale: 1.0,
                primary: t.primary,
                enabled: t.active,
                // Unlike the Linux backends, Windows CAN say this reliably: our IddCx monitors
                // carry our own EDID manufacturer id in their device path.
                managed: t.ours,
            }
        })
        .collect())
}

/// Resolve a configured monitor name against `monitors`, exactly then case-insensitively.
///
/// **A miss is a hard error carrying the available names**, never a silent fall-back to some other
/// head: an operator who pinned `DP-2` and gets `DP-1` streamed has been shown the wrong screen,
/// which is worse than a host that refuses to start a session and says why
/// (`design/per-monitor-portal-capture.md` §5.2).
pub fn resolve<'a>(monitors: &'a [PhysicalMonitor], want: &str) -> Result<&'a PhysicalMonitor> {
    if let Some(m) = monitors.iter().find(|m| m.connector == want) {
        return Ok(m);
    }
    if let Some(m) = monitors
        .iter()
        .find(|m| m.connector.eq_ignore_ascii_case(want))
    {
        return Ok(m);
    }
    let available = monitors
        .iter()
        .map(|m| m.connector.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if available.is_empty() {
        bail!("no monitor named {want:?} — this compositor reports no monitors at all");
    }
    bail!("no monitor named {want:?} — this host has: {available}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(connector: &str) -> PhysicalMonitor {
        PhysicalMonitor {
            connector: connector.into(),
            description: String::new(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled: true,
            managed: false,
        }
    }

    #[test]
    fn resolve_matches_exactly_then_case_insensitively() {
        let ms = [mon("DP-1"), mon("HDMI-A-2")];
        assert_eq!(resolve(&ms, "DP-1").unwrap().connector, "DP-1");
        assert_eq!(resolve(&ms, "hdmi-a-2").unwrap().connector, "HDMI-A-2");
    }

    #[test]
    fn resolve_prefers_the_exact_match_over_a_case_fold() {
        // Connector names are case-sensitive to the compositor; if both spellings exist, the
        // exact one wins rather than whichever the fold happened to reach first.
        let ms = [mon("dp-1"), mon("DP-1")];
        assert_eq!(resolve(&ms, "DP-1").unwrap().connector, "DP-1");
    }

    #[test]
    fn a_miss_lists_what_is_available() {
        let ms = [mon("DP-1"), mon("HDMI-A-2")];
        let err = resolve(&ms, "DP-9").unwrap_err().to_string();
        assert!(err.contains("DP-1") && err.contains("HDMI-A-2"), "{err}");
    }

    #[test]
    fn a_miss_with_no_monitors_says_so() {
        let err = resolve(&[], "DP-1").unwrap_err().to_string();
        assert!(err.contains("no monitors at all"), "{err}");
    }

    /// Compositors write the literal "Unknown" instead of leaving make/model empty (sway does it
    /// for headless outputs), so a picker must not end up showing "Unknown Unknown".
    #[test]
    fn describe_falls_back_to_the_connector_for_empty_or_unknown_fields() {
        assert_eq!(describe("ACME", "U2720Q", "DP-1"), "ACME U2720Q");
        assert_eq!(describe("Unknown", "Unknown", "HEADLESS-1"), "HEADLESS-1");
        assert_eq!(describe("", "", "DP-1"), "DP-1");
        // A half-known pair keeps the half that means something.
        assert_eq!(describe("Unknown", "U2720Q", "DP-1"), "U2720Q");
        assert_eq!(describe("  ", "unknown", "DP-2"), "DP-2");
    }

    #[test]
    fn mode_label_drops_an_unknown_refresh() {
        let mut m = mon("DP-1");
        assert_eq!(m.mode_label(), "1920x1080@60");
        m.refresh_mhz = 0;
        assert_eq!(m.mode_label(), "1920x1080");
    }
}
