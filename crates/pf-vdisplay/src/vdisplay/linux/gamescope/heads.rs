//! The physical head a gamescope session drives — the enumeration half of per-monitor capture.
//!
//! gamescope was originally written off here as "nested, therefore no physical heads of its own"
//! (`design/per-monitor-portal-capture.md` §2). That is true of a gamescope running *inside*
//! another compositor, and of the headless ones this crate spawns itself — but it is false for the
//! deployment most people actually stream from: a Bazzite/SteamOS **Game Mode session is the DRM
//! master**, driving the TV directly. On such a box the head exists, the operator can see it, and
//! refusing to list it meant the console's picker was permanently empty on exactly the hardware the
//! feature was asked for.
//!
//! So this module answers a narrow question — *is this gamescope on the DRM backend, and which
//! connector is it lighting?* — from two sources that need no cooperation from gamescope:
//!
//! * `/proc/<pid>/cmdline`, for the backend flag and `--prefer-output` (gamescope has no protocol
//!   that reports its own output; the [`gamescope_control`] extension only sets things), and
//! * `/sys/class/drm`, for which connectors are actually plugged in, plus each one's EDID.
//!
//! **Only the head gamescope drives is listed.** Mirroring here is an attach to gamescope's own
//! composited PipeWire node (see [`super::heads`]'s caller in `vdisplay::mirror`), so a connector
//! gamescope is *not* driving could never be streamed — offering it in a picker would be offering
//! something that cannot work.

use crate::monitors::{describe, PhysicalMonitor};
use std::path::Path;

/// The head this box's gamescope session is driving, or empty when it drives none.
///
/// Empty — never an error — for the three "no physical head" shapes, which are ordinary states and
/// not failures to reach anything: no gamescope running, a gamescope on a non-DRM backend
/// (`--backend headless`, or nested in a parent compositor), and a DRM gamescope with nothing
/// plugged in. That matches [`crate::monitors::list`]'s contract, where `Ok(vec![])` means "asked,
/// and there are none".
pub(crate) fn list_monitors() -> anyhow::Result<Vec<PhysicalMonitor>> {
    Ok(heads_under(
        Path::new("/sys/class/drm"),
        &super::gamescope_argvs(),
    ))
}

/// [`list_monitors`] against an arbitrary sysfs root and a supplied argv set — the unit-testable
/// core.
///
/// The head's size comes from the `-W`/`-H` of the argv selected HERE, which OUTRANKS the EDID's
/// preferred timing because it is the size the capture node actually produces. It used to arrive as
/// a parameter filled by a scan over ALL gamescopes on the box — including the nested child this
/// function had just deliberately rejected, and any headless one the crate spawned itself. On a
/// Deck driving eDP-1 at 1280x800 with a game nested at `-W 1920 -H 1080`, the panel was listed as
/// 1920x1080, and `mirror::create` publishes that row verbatim as the `preferred_mode` the stream
/// negotiates against — a mode the composited node never produces, and one `check_mirrorable` waves
/// through because it only rejects `0x0`.
fn heads_under(base: &Path, argvs: &[Vec<String>]) -> Vec<PhysicalMonitor> {
    // A gamescope that isn't on DRM has no head of its own. Any DRM-backed one qualifies the box:
    // a Deck streaming from Game Mode often has a second, nested gamescope running the game inside
    // the session one, and that child must not disqualify its parent.
    let Some(argv) = argvs.iter().find(|a| drives_drm(a)) else {
        return Vec::new();
    };
    let output_size = super::gamescope_output_size(argv);
    let connected = connected_connectors(base);
    if connected.is_empty() {
        return Vec::new();
    }
    let driven = resolve_driven(&connected, prefer_output(argv));
    driven
        .into_iter()
        .map(|c| {
            let edid = std::fs::read(base.join(&c.dir).join("edid"))
                .ok()
                .and_then(|b| parse_edid(&b));
            let (make, model) = edid
                .as_ref()
                .map(|e| (e.make.clone(), e.model.clone()))
                .unwrap_or_default();
            // Size: what the capture node produces (`-W`/`-H`) beats the panel's preferred timing,
            // since that is what the client will actually decode. Refresh only ever comes from the
            // EDID — gamescope's `-r`/`--nested-refresh` is the rate it composites the NESTED game
            // at, not the rate the connector runs, and reading it as the latter would tell the
            // pacer to cap a 120 Hz TV at whatever the session happened to be launched with.
            let (w, h) = output_size
                .or_else(|| edid.as_ref().map(|e| (e.width, e.height)))
                .or_else(|| preferred_sysfs_mode(base, &c.dir))
                .unwrap_or((0, 0));
            PhysicalMonitor {
                description: describe(&make, &model, &c.connector),
                connector: c.connector,
                width: w,
                height: h,
                refresh_mhz: edid.as_ref().map(|e| e.refresh_mhz).unwrap_or(0),
                // gamescope composites ONE output; there is no global space to be offset in, so
                // the origin is the origin. (`monitors`' identity key degenerates here, which is
                // sound precisely because there is never more than one head to tell apart.)
                x: 0,
                y: 0,
                scale: 1.0,
                primary: true,
                enabled: c.enabled,
                // Never ours: a gamescope WE created is headless, and `drives_drm` excluded it.
                managed: false,
            }
        })
        .collect()
}

/// One connected DRM connector, as sysfs presents it.
struct Connector {
    /// Directory name — `card1-HDMI-A-1`.
    dir: String,
    /// Connector name with the `cardN-` prefix stripped — `HDMI-A-1`, the id an operator pins.
    connector: String,
    enabled: bool,
}

/// Every plugged-in connector under `base`, sorted by name so the list is stable across calls
/// (`read_dir` order is not).
fn connected_connectors(base: &Path) -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out: Vec<Connector> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.file_name().to_str()?.to_string();
            // `cardN-<connector>`; the render/device nodes (`card1`, `renderD128`) have no dash and
            // no status file, and a connector name itself contains dashes (`HDMI-A-1`).
            let (card, connector) = dir.split_once('-')?;
            if !card.starts_with("card") || !card[4..].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            if status.trim() != "connected" {
                return None;
            }
            let enabled = std::fs::read_to_string(e.path().join("enabled"))
                .map(|s| s.trim() == "enabled")
                .unwrap_or(true);
            Some(Connector {
                connector: connector.to_string(),
                dir,
                enabled,
            })
        })
        .collect();
    out.sort_by(|a, b| a.connector.cmp(&b.connector));
    out
}

/// Which of `connected` is gamescope lighting?
///
/// `--prefer-output` is gamescope's own answer and is taken when it names something plugged in —
/// it is a comma-separated preference list (`*,eDP-1` on a Deck), so `*` ("whatever you find") is
/// skipped rather than matched. Failing that, a single connected head is unambiguous.
///
/// When neither settles it, every connected head is returned. That is deliberately NOT a guess:
/// mirroring attaches to gamescope's composited node, so the *stream* is correct whichever row the
/// operator picks — only the label could be off, and showing the plausible labels beats showing an
/// empty picker on a box that demonstrably has a screen.
fn resolve_driven(connected: &[Connector], prefer: Option<&str>) -> Vec<Connector> {
    let clone = |c: &Connector| Connector {
        dir: c.dir.clone(),
        connector: c.connector.clone(),
        enabled: c.enabled,
    };
    if let Some(list) = prefer {
        for want in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            if want == "*" {
                continue;
            }
            if let Some(c) = connected
                .iter()
                .find(|c| c.connector.eq_ignore_ascii_case(want))
            {
                return vec![clone(c)];
            }
        }
    }
    if connected.len() == 1 {
        return vec![clone(&connected[0])];
    }
    tracing::debug!(
        heads = connected.len(),
        "gamescope: --prefer-output does not name a connected head — listing all of them (the \
         mirror attaches to the composited node either way)"
    );
    connected.iter().map(clone).collect()
}

/// Is this gamescope on the DRM backend — i.e. does it own a physical connector?
///
/// gamescope picks DRM when no `--backend` says otherwise, so ABSENCE of the flag is the common
/// yes (a Bazzite session's argv carries `--prefer-output` and no backend at all). `headless` is
/// what this crate spawns and what its own SteamOS takeover writes; `wayland`/`sdl`/`x11` are
/// nested. `-b` is the short form.
fn drives_drm(argv: &[String]) -> bool {
    match backend_flag(argv) {
        Some(b) => b.eq_ignore_ascii_case("drm"),
        None => true,
    }
}

fn backend_flag(argv: &[String]) -> Option<&str> {
    flag_value(argv, &["--backend", "-b"])
}

fn prefer_output(argv: &[String]) -> Option<&str> {
    flag_value(argv, &["--prefer-output", "-O"])
}

/// The value following the first of `names` present in `argv`, in both `--flag value` and
/// `--flag=value` spellings.
fn flag_value<'a>(argv: &'a [String], names: &[&str]) -> Option<&'a str> {
    argv.iter().enumerate().find_map(|(i, a)| {
        if let Some((k, v)) = a.split_once('=') {
            if names.contains(&k) {
                return Some(v);
            }
        }
        if names.contains(&a.as_str()) {
            return argv.get(i + 1).map(|s| s.as_str());
        }
        None
    })
}

/// The connector's preferred mode from sysfs (`modes`, most-preferred first, `WIDTHxHEIGHT`).
/// Last-resort size when there is neither a `-W`/`-H` nor a readable EDID.
fn preferred_sysfs_mode(base: &Path, dir: &str) -> Option<(u32, u32)> {
    let modes = std::fs::read_to_string(base.join(dir).join("modes")).ok()?;
    let first = modes.lines().next()?.trim();
    let (w, h) = first.split_once('x')?;
    Some((w.parse().ok()?, h.trim().parse().ok()?))
}

/// The fields a picker needs out of an EDID base block.
struct Edid {
    make: String,
    model: String,
    width: u32,
    height: u32,
    refresh_mhz: u32,
}

/// Parse an EDID 1.x base block: the manufacturer id, the monitor-name descriptor, and the
/// preferred detailed timing.
///
/// Only the first 128-byte block is read. Extension blocks (CTA-861) can carry *more* modes, but
/// never a better answer to "what is this panel called and what is its preferred timing", which is
/// all a picker row needs.
fn parse_edid(bytes: &[u8]) -> Option<Edid> {
    if bytes.len() < 128 || bytes[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    // Manufacturer: three 5-bit letters packed big-endian, 1 = 'A' (bit 15 reserved). This is the
    // PnP id (`SAM`, `LGD`) rather than a marketing name — the same thing sway and KWin report,
    // and `describe` pairs it with the model string.
    let m = u16::from_be_bytes([bytes[8], bytes[9]]);
    let letter = |shift: u16| -> Option<char> {
        let v = ((m >> shift) & 0x1F) as u8;
        (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
    };
    let make: String = match (letter(10), letter(5), letter(0)) {
        (Some(a), Some(b), Some(c)) => [a, b, c].iter().collect(),
        // A zeroed/garbage id is not worth showing; `describe` falls back to the connector.
        _ => String::new(),
    };
    let mut model = String::new();
    let mut timing: Option<(u32, u32, u32)> = None;
    // Four 18-byte descriptors. The FIRST is the preferred timing by definition; the others may be
    // timings too, or tagged text blocks (a zero pixel clock is the tag marker).
    for off in [54usize, 72, 90, 108] {
        let d = &bytes[off..off + 18];
        let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
        if pixel_clock == 0 {
            // 0xFC = monitor name, up to 13 bytes, 0x0A-terminated and space-padded.
            if d[3] == 0xFC && model.is_empty() {
                let raw = &d[5..18];
                let end = raw.iter().position(|&b| b == 0x0A).unwrap_or(raw.len());
                model = String::from_utf8_lossy(&raw[..end]).trim().to_string();
            }
            continue;
        }
        if timing.is_some() {
            continue;
        }
        // Split-nibble encoding: the high 4 bits of the upper byte extend hactive, the low 4 hblank
        // (likewise vactive/vblank).
        let hactive = d[2] as u32 | ((d[4] as u32 & 0xF0) << 4);
        let hblank = d[3] as u32 | ((d[4] as u32 & 0x0F) << 8);
        let vactive = d[5] as u32 | ((d[7] as u32 & 0xF0) << 4);
        let vblank = d[6] as u32 | ((d[7] as u32 & 0x0F) << 8);
        let (htotal, vtotal) = (hactive + hblank, vactive + vblank);
        if hactive == 0 || vactive == 0 || htotal == 0 || vtotal == 0 {
            continue;
        }
        // Pixel clock is in 10 kHz units; ×10_000 gives Hz, and the extra ×1000 gives mHz — done
        // in u64 so a 4K timing's htotal×vtotal cannot overflow the intermediate.
        let refresh_mhz =
            ((pixel_clock as u64 * 10_000_000) / (htotal as u64 * vtotal as u64)) as u32;
        timing = Some((hactive, vactive, refresh_mhz));
    }
    let (width, height, refresh_mhz) = timing.unwrap_or((0, 0, 0));
    Some(Edid {
        make,
        model,
        width,
        height,
        refresh_mhz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// A sysfs tree with the device/render nodes a real `/sys/class/drm` also holds, so the
    /// connector filter is exercised against the noise it has to survive.
    fn sysfs(name: &str, connectors: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("pf-gs-heads-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for n in ["card1", "renderD128"] {
            std::fs::create_dir_all(base.join(n)).unwrap();
        }
        for (dir, status, enabled) in connectors {
            let d = base.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("status"), status).unwrap();
            std::fs::write(d.join("enabled"), enabled).unwrap();
        }
        base
    }

    /// The bug this module exists to fix: a Bazzite Game Mode session (no `--backend`, so DRM)
    /// driving a TV must report that TV, where the old code returned an empty list unconditionally.
    #[test]
    fn a_drm_backed_session_reports_the_head_it_drives() {
        let base = sysfs(
            "drm",
            &[
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
                ("card1-DP-1", "disconnected\n", "disabled\n"),
            ],
        );
        let heads = heads_under(
            &base,
            &[argv("/usr/bin/gamescope --prefer-output HDMI-A-1 --steam")],
        );
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "HDMI-A-1");
        assert!(heads[0].enabled && heads[0].primary && !heads[0].managed);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The original premise, which stays true and must keep returning empty: a gamescope we spawned
    /// headless (or one nested in a parent compositor) owns no connector, even on a box whose TV is
    /// plugged in. This is the state a punktfunk takeover leaves the box in mid-stream.
    #[test]
    fn a_headless_or_nested_session_reports_nothing() {
        let base = sysfs(
            "headless",
            &[("card1-HDMI-A-1", "connected\n", "enabled\n")],
        );
        for a in [
            "gamescope --backend headless -W 1920 -H 1080",
            "gamescope --backend=headless",
            "gamescope -b wayland",
            "gamescope --backend sdl",
        ] {
            assert!(
                heads_under(&base, &[argv(a)]).is_empty(),
                "expected no heads for {a:?}"
            );
        }
        // No gamescope at all is the same answer, not an error.
        assert!(heads_under(&base, &[]).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A Deck in Game Mode runs a SECOND gamescope nested inside the session one for the game. The
    /// nested child must not mask the parent's connector.
    #[test]
    fn a_nested_child_does_not_disqualify_its_drm_parent() {
        let base = sysfs("nested", &[("card1-eDP-1", "connected\n", "enabled\n")]);
        let heads = heads_under(
            &base,
            &[
                argv("gamescope --backend wayland -W 1280 -H 800"),
                argv("/usr/bin/gamescope --prefer-output *,eDP-1 -W 2560 -H 1440 --steam"),
            ],
        );
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "eDP-1");
        // …and the size comes from the DRM PARENT, not from the nested child listed first. Reading
        // it off any-gamescope-on-the-box is what published a 1280x800 panel as the mirror's
        // preferred mode on a box where the game happened to be nested at a different size.
        assert_eq!((heads[0].width, heads[0].height), (2560, 1440));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// `*` means "whatever you find" and must not be matched against a connector name; the next
    /// entry in the preference list is the real answer.
    #[test]
    fn prefer_output_skips_the_wildcard_and_picks_the_named_head() {
        let base = sysfs(
            "wildcard",
            &[
                ("card1-eDP-1", "connected\n", "enabled\n"),
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
            ],
        );
        let heads = heads_under(&base, &[argv("gamescope --prefer-output *,eDP-1")]);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "eDP-1");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Two heads and nothing to choose between them: list both rather than guess or show nothing —
    /// the mirror attaches to the composited node, so any row streams the right pixels.
    #[test]
    fn an_undecidable_multi_head_box_lists_every_connected_head() {
        let base = sysfs(
            "ambiguous",
            &[
                ("card1-eDP-1", "connected\n", "enabled\n"),
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
            ],
        );
        let heads = heads_under(&base, &[argv("gamescope --steam")]);
        assert_eq!(
            heads
                .iter()
                .map(|h| h.connector.as_str())
                .collect::<Vec<_>>(),
            ["HDMI-A-1", "eDP-1"],
            "sorted, so the picker order is stable"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A DRM gamescope with nothing plugged in is a headless box — empty, not an error.
    #[test]
    fn a_drm_session_with_nothing_plugged_in_reports_nothing() {
        let base = sysfs(
            "unplugged",
            &[("card1-HDMI-A-1", "disconnected\n", "disabled\n")],
        );
        assert!(heads_under(&base, &[argv("gamescope --steam")]).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// gamescope's own output size is what the capture node produces, so it outranks the panel's
    /// preferred timing — a supersampled session must not advertise the TV's native size.
    #[test]
    fn the_capture_size_outranks_the_panels_preferred_timing() {
        let base = sysfs("size", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        let heads = heads_under(
            &base,
            &[argv("gamescope -W 2560 -H 1440 --prefer-output HDMI-A-1")],
        );
        assert_eq!((heads[0].width, heads[0].height), (2560, 1440));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A synthetic EDID: 1920x1080 @ 60 Hz preferred timing, make `ACM`, name "TEST PANEL".
    fn edid_1080p60() -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // 'A'=1, 'C'=3, 'M'=13 → (1<<10)|(3<<5)|13
        let id: u16 = (1 << 10) | (3 << 5) | 13;
        e[8..10].copy_from_slice(&id.to_be_bytes());
        // Preferred detailed timing: 148.5 MHz = 14850 (10 kHz units); 1920+280 x 1080+45.
        e[54..56].copy_from_slice(&14850u16.to_le_bytes());
        e[56] = 1920u32 as u8; // hactive low
        e[57] = 280u32 as u8; // hblank low
        e[58] = ((1920 >> 4) as u8 & 0xF0) | ((280 >> 8) as u8 & 0x0F);
        e[59] = (1080 & 0xFF) as u8; // vactive low
        e[60] = 45; // vblank low
        e[61] = ((1080 >> 4) as u8 & 0xF0) | ((45 >> 8) as u8 & 0x0F);
        // Second descriptor: monitor name.
        e[72..74].copy_from_slice(&0u16.to_le_bytes());
        e[75] = 0xFC;
        let name = b"TEST PANEL\x0a";
        e[77..77 + name.len()].copy_from_slice(name);
        e
    }

    #[test]
    fn edid_yields_the_label_and_the_preferred_timing() {
        let e = parse_edid(&edid_1080p60()).expect("valid EDID");
        assert_eq!(e.make, "ACM");
        assert_eq!(e.model, "TEST PANEL");
        assert_eq!((e.width, e.height), (1920, 1080));
        // 148_500_000 / (2200 * 1125) = 60.0 Hz
        assert_eq!(e.refresh_mhz, 60_000);
        assert_eq!(describe(&e.make, &e.model, "HDMI-A-1"), "ACM TEST PANEL");
    }

    /// Garbage must not become a picker row that lies; the caller falls back to the connector name.
    #[test]
    fn a_bad_edid_is_rejected_rather_than_half_parsed() {
        assert!(parse_edid(&[0u8; 128]).is_none(), "no magic header");
        assert!(parse_edid(&[0u8; 12]).is_none(), "too short");
    }

    /// The refresh a mirrored TV runs at comes from the EDID — reading gamescope's
    /// `--nested-refresh` as the connector's rate would let a session launched at 60 cap a 120 Hz
    /// panel for the whole stream.
    #[test]
    fn refresh_comes_from_the_panel_not_from_the_nested_rate() {
        let base = sysfs("refresh", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        std::fs::write(base.join("card1-HDMI-A-1").join("edid"), edid_1080p60()).unwrap();
        let heads = heads_under(
            &base,
            &[argv(
                "gamescope --nested-refresh 30 --prefer-output HDMI-A-1",
            )],
        );
        assert_eq!(heads[0].refresh_mhz, 60_000);
        assert_eq!(heads[0].mode_label(), "1920x1080@60");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn flag_values_parse_in_both_spellings() {
        assert_eq!(
            backend_flag(&argv("gamescope --backend headless")),
            Some("headless")
        );
        assert_eq!(backend_flag(&argv("gamescope --backend=drm")), Some("drm"));
        assert_eq!(backend_flag(&argv("gamescope -b sdl")), Some("sdl"));
        assert_eq!(backend_flag(&argv("gamescope --steam")), None);
        assert_eq!(prefer_output(&argv("gamescope -O DP-2")), Some("DP-2"));
    }

    /// Sysfs `modes` is the last resort when a panel has no readable EDID (some TVs over a KVM).
    #[test]
    fn sysfs_modes_is_the_last_resort_size() {
        let base = sysfs("modes", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        std::fs::write(
            base.join("card1-HDMI-A-1").join("modes"),
            "3840x2160\n1920x1080\n",
        )
        .unwrap();
        let heads = heads_under(&base, &[argv("gamescope --steam")]);
        assert_eq!((heads[0].width, heads[0].height), (3840, 2160));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
