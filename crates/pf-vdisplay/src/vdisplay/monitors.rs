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

/// One head as the compositor currently reports it.
///
/// **The two halves live in different spaces, and that is not an accident.** `x`/`y` are LOGICAL —
/// the compositor's global layout coordinates, the same space libei regions use — while
/// `width`/`height` are the current mode in PIXELS, because that is what every backend actually
/// reports (KWin's `current_mode` size, `hyprctl`'s mode, the CCD path's source mode) and what a
/// capturer has to open against. `scale` is the factor between them: see [`Self::logical_size`],
/// which is the only correct way to compare a size against `x`/`y`. An earlier version of this doc
/// claimed logical geometry "throughout", which is a trap for exactly the consumer that mixes them.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalMonitor {
    /// Connector name — `DP-1`, `HDMI-A-2`, `eDP-1`. The id `PUNKTFUNK_CAPTURE_MONITOR` names.
    pub connector: String,
    /// Human label for a picker (`make model`, else the connector). Never used for matching.
    pub description: String,
    /// Current mode, in PIXELS (not the logical size — see the type doc and [`Self::logical_size`]).
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
    /// The head's extent in the SAME space as `x`/`y` — mode pixels divided by `scale`.
    ///
    /// The bridge between the two spaces this type carries, and the only correct way to ask "does
    /// this head's box contain that layout coordinate?". A consumer that compares `width`/`height`
    /// against `x`/`y` directly is right only at scale 1.0 and silently wrong on every fractional
    /// KDE/GNOME desk (a 3840-px panel at 150 % occupies 2560 logical units, so a naive
    /// `x + width` overlaps the head to its right by 1280).
    ///
    /// A non-positive scale can only come from a backend that reported nonsense; it is treated as
    /// 1.0 rather than dividing by zero.
    pub fn logical_size(&self) -> (f64, f64) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        (
            f64::from(self.width) / scale,
            f64::from(self.height) / scale,
        )
    }

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
        // Via the `kwin` backend rather than `kwin_output_mgmt` directly: it owns the
        // in-process-then-`kscreen-doctor` ladder, so this read degrades the same way every other
        // KWin operation does instead of being the one that hard-fails on a wedged/old compositor.
        #[cfg(target_os = "linux")]
        Compositor::Kwin => crate::kwin::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Mutter => crate::mutter::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Wlroots => crate::wlroots::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Hyprland => crate::hyprland::list_monitors(),
        // gamescope is only *sometimes* nested. A Bazzite/SteamOS Game Mode session is the DRM
        // master and drives a real connector; the ones this crate spawns are headless and drive
        // none. `gamescope::list_monitors` tells those apart and answers an empty list — not an
        // error — for the nested/headless shapes, which is what this arm used to hard-code for all
        // of them (and why the picker was permanently empty on a TV box).
        #[cfg(target_os = "linux")]
        Compositor::Gamescope => crate::gamescope::list_monitors(),
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
    // `Ok` even when the inventory is empty, exactly as [`list`] promises: an empty CCD database is
    // a real state (every panel off — measured on .173 with the TV powered down), not a failure.
    // Everything past the OS call is the pure mapping, so it lives where a test can reach it.
    Ok(from_inventory(
        pf_win_display::win_display::target_inventory(),
    ))
}

/// The CCD inventory → [`PhysicalMonitor`] mapping, split from the OS call so the Windows test leg
/// can exercise it (`list_windows` touches the display database on its first line, which left the
/// only mapping that decides what an operator can PIN with no coverage on the one platform that
/// runs it).
#[cfg(windows)]
fn from_inventory(inv: Vec<pf_win_display::win_display::TargetInventory>) -> Vec<PhysicalMonitor> {
    inv.into_iter()
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
        .collect()
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

    /// The two spaces this type carries: the mode is pixels, `x`/`y` are logical, and `scale` is
    /// the only thing that relates them. A 4K panel at KDE's 150 % really does occupy 2560x1440
    /// logical units, which is what a consumer comparing against `x`/`y` must use.
    #[test]
    fn logical_size_divides_the_mode_by_the_scale() {
        let mut m = mon("DP-1");
        m.width = 3840;
        m.height = 2160;
        m.scale = 1.5;
        assert_eq!(m.logical_size(), (2560.0, 1440.0));
        // Unscaled: the two spaces coincide, which is why the trap goes unnoticed on most desks.
        m.scale = 1.0;
        assert_eq!(m.logical_size(), (3840.0, 2160.0));
        // A backend that reported nonsense must not produce an infinity or a NaN.
        m.scale = 0.0;
        assert_eq!(m.logical_size(), (3840.0, 2160.0));
    }

    #[test]
    fn mode_label_drops_an_unknown_refresh() {
        let mut m = mon("DP-1");
        assert_eq!(m.mode_label(), "1920x1080@60");
        m.refresh_mhz = 0;
        assert_eq!(m.mode_label(), "1920x1080");
    }
}

/// The Windows inventory mapping. Windows-only because it maps a Windows-only type — the CI leg
/// that runs it (`windows-host.yml`, `cargo test --release -p pf-vdisplay`) already exists; until
/// [`from_inventory`] was split out of the OS call there was simply nothing there to run.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use pf_win_display::win_display::TargetInventory;

    /// One inventory row. Built through a single helper so a field rename shows up in one place —
    /// the struct is another crate's and carries no `Default`.
    fn target(target_id: u32, gdi_name: &str, active: bool) -> TargetInventory {
        TargetInventory {
            target_id,
            active,
            external_physical: true,
            internal_panel: false,
            tech: "HDMI",
            friendly: "ACME TV".into(),
            monitor_device_path: r"\\?\DISPLAY#ACM1234#".into(),
            ours: false,
            gdi_name: gdi_name.into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            refresh_mhz: 59940,
            primary: active,
        }
    }

    /// An INACTIVE path has no source and therefore no GDI name. It must still be listed (the
    /// "why can't I pick it?" contract) under an id that can actually be pinned — a blank connector
    /// could never be resolved, and an operator would have no way to name the head at all.
    #[test]
    fn an_inactive_path_gets_a_target_id_connector_and_enabled_false() {
        let mons = from_inventory(vec![target(4352, "", false)]);
        assert_eq!(mons.len(), 1);
        assert_eq!(mons[0].connector, "target-4352");
        assert!(!mons[0].enabled);
        // Windows applies DPI per application rather than a compositor-global logical scale, so
        // the geometry above is pixels and the factor is honestly 1.0 — see the fn doc.
        assert_eq!(mons[0].scale, 1.0);
    }

    /// The two halves must agree: whatever connector this mapping synthesizes has to be a name
    /// [`resolve`] can find, because that pair is the whole pin round-trip the console offers.
    #[test]
    fn resolve_can_find_a_synthesized_target_name() {
        let mons = from_inventory(vec![
            target(4352, "", false),
            target(1, r"\\.\DISPLAY1", true),
        ]);
        assert_eq!(
            resolve(&mons, "target-4352")
                .expect("synthesized name")
                .width,
            1920
        );
        // An active path keeps its GDI name — the id an operator recognises.
        assert_eq!(
            resolve(&mons, r"\\.\DISPLAY1").expect("gdi name").connector,
            r"\\.\DISPLAY1"
        );
        assert!(
            resolve(&mons, r"\\.\display1").is_ok(),
            "and case-insensitively, as `resolve` promises"
        );
    }

    /// Our own IddCx display is flagged, so a picker can grey it out — the one thing Windows can
    /// answer reliably and the Linux backends cannot.
    #[test]
    fn our_own_idd_is_marked_managed() {
        let mut ours = target(257, r"\\.\DISPLAY2", true);
        ours.ours = true;
        let mons = from_inventory(vec![ours]);
        assert!(mons[0].managed);
        assert!(!from_inventory(vec![target(1, r"\\.\DISPLAY1", true)])[0].managed);
    }
}
