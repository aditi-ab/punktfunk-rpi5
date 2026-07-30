//! **How was this host installed, and on which channel?** (design §4.1)
//!
//! The apply strategy — and, until apply lands, the command hint the console shows — hangs
//! off the install kind. Detection is a ladder over root-owned facts: packaging writes a
//! marker (`/usr/share/punktfunk/install-kind`, e.g. `apt stable`), the sysext self-identifies
//! via its merged extension-release, Nix by store path, and so on. The API only ever *reads*
//! this; nothing request-side can influence it.
//!
//! The ladder itself is a pure function over a [`Probe`] so every branch is unit-testable;
//! [`detect`] gathers the real probe once per process.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where the Linux packages stamp how they were installed. First word = kind
/// (`apt`|`dnf`|`pacman`), optional second word = channel (`stable`|`canary`).
const MARKER_PATH: &str = "/usr/share/punktfunk/install-kind";

/// The merged sysext names itself here (written by `build-sysext.sh`); its presence means the
/// running `/usr` overlay came from the sysext image, regardless of any leftover marker.
const SYSEXT_MARKER: &str = "/usr/lib/extension-release.d/extension-release.punktfunk";

/// The sysext updater's own config (`CHANNEL=stable|canary`).
const SYSEXT_CONF: &str = "/etc/punktfunk-sysext.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallKind {
    WindowsInstaller,
    Sysext,
    RpmOstree,
    Apt,
    Dnf,
    Pacman,
    SteamosSource,
    Nix,
    Source,
}

impl InstallKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            InstallKind::WindowsInstaller => "windows-installer",
            InstallKind::Sysext => "sysext",
            InstallKind::RpmOstree => "rpm-ostree",
            InstallKind::Apt => "apt",
            InstallKind::Dnf => "dnf",
            InstallKind::Pacman => "pacman",
            InstallKind::SteamosSource => "steamos-source",
            InstallKind::Nix => "nix",
            InstallKind::Source => "source",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    Stable,
    Canary,
}

impl Channel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }
}

/// The root-owned facts the ladder reads, gathered once by [`gather`] (tests build these
/// directly).
#[derive(Debug, Default)]
pub(crate) struct Probe {
    /// Running on Windows (cfg, not a file).
    pub windows: bool,
    /// The running exe's path.
    pub exe: PathBuf,
    /// `$HOME`, if any.
    pub home: Option<PathBuf>,
    /// Contents of [`MARKER_PATH`], if present.
    pub marker: Option<String>,
    /// [`SYSEXT_MARKER`] exists (merged sysext overlay).
    pub sysext: bool,
    /// Contents of [`SYSEXT_CONF`], if present.
    pub sysext_conf: Option<String>,
    /// `/run/ostree-booted` exists (rpm-ostree / bootc family).
    pub ostree_booted: bool,
}

fn gather() -> Probe {
    Probe {
        windows: cfg!(target_os = "windows"),
        exe: std::env::current_exe().unwrap_or_default(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        marker: std::fs::read_to_string(MARKER_PATH).ok(),
        sysext: Path::new(SYSEXT_MARKER).exists(),
        sysext_conf: std::fs::read_to_string(SYSEXT_CONF).ok(),
        ostree_booted: Path::new("/run/ostree-booted").exists(),
    }
}

/// The ladder (design §4.1). Order matters and each rung is a root-owned fact:
/// sysext overlay > Nix store path > dev/source tree > user-owned Deck build > package
/// marker (flipped to rpm-ostree when the box is ostree-booted) > `source` fallback.
pub(crate) fn classify(p: &Probe) -> (InstallKind, Channel) {
    if p.windows {
        // The installer is the only supported Windows delivery; a loose cargo build shows
        // itself by not living under Program Files. Channel: canary installers carry the CI
        // run as the third component (`M.m.<run>`), see `windows_channel_of`.
        let installed = p
            .exe
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("\\program files\\punktfunk");
        return if installed {
            (
                InstallKind::WindowsInstaller,
                windows_channel_of(env!("PUNKTFUNK_VERSION")),
            )
        } else {
            (InstallKind::Source, Channel::Stable)
        };
    }

    if p.sysext {
        let channel = p
            .sysext_conf
            .as_deref()
            .and_then(conf_channel)
            .unwrap_or(Channel::Stable);
        return (InstallKind::Sysext, channel);
    }

    if p.exe.starts_with("/nix/store") {
        return (InstallKind::Nix, Channel::Stable);
    }

    // A cargo tree anywhere (CI, dev box, the Deck checkout mid-build) is `source`; the
    // Deck's install script runs the binary out of `~/punktfunk/target-steamos/`, which is
    // user-owned but NOT a plain `target/` dir — that distinction is the marker here.
    let exe_str = p.exe.to_string_lossy().to_string();
    if exe_str.contains("/target/") {
        return (InstallKind::Source, Channel::Stable);
    }
    if let Some(home) = &p.home {
        if p.exe.starts_with(home) {
            return (InstallKind::SteamosSource, Channel::Canary);
        }
    }

    if let Some(marker) = &p.marker {
        let mut words = marker.split_whitespace();
        let kind = words.next().unwrap_or("");
        let channel = match words.next() {
            Some("canary") => Channel::Canary,
            _ => Channel::Stable,
        };
        let kind = match kind {
            "apt" => Some(InstallKind::Apt),
            // An ostree-booted box consumed the RPM by layering (or an image build); either
            // way `dnf upgrade` is not how it updates. bootc-vs-layered is refined in U2 via
            // `rpm-ostree status` — until then both report `rpm-ostree` (notify text is
            // identical in U0).
            "dnf" if p.ostree_booted => Some(InstallKind::RpmOstree),
            "dnf" => Some(InstallKind::Dnf),
            "pacman" => Some(InstallKind::Pacman),
            _ => None,
        };
        if let Some(kind) = kind {
            return (kind, channel);
        }
    }

    (InstallKind::Source, Channel::Stable)
}

/// `CHANNEL=canary` in `/etc/punktfunk-sysext.conf` (the sysext updater's own format).
fn conf_channel(conf: &str) -> Option<Channel> {
    for line in conf.lines() {
        if let Some(v) = line.trim().strip_prefix("CHANNEL=") {
            return Some(match v.trim() {
                "canary" => Channel::Canary,
                _ => Channel::Stable,
            });
        }
    }
    None
}

/// Windows canary installers are versioned `M.m.<run>` where `<run>` is a 4+ digit CI run
/// number; stable patch numbers stay small. Heuristic, documented in the plan (R10).
fn windows_channel_of(version: &str) -> Channel {
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Channel::Canary,
        _ => Channel::Stable,
    }
}

/// The process-wide answer, computed once.
pub(crate) fn detect() -> (InstallKind, Channel) {
    static DETECTED: OnceLock<(InstallKind, Channel)> = OnceLock::new();
    *DETECTED.get_or_init(|| classify(&gather()))
}

// ---------------------------------------------------------------- version comparison

/// Leading `major.minor.patch` of a version string, ignoring any suffix (`~ci…`, `-1`, `+…`).
pub(crate) fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    // Split on any non-digit: "0.23.0~ci10250.gab" → 0,23,0,10250… — take the first three
    // ONLY if the string actually starts with digits (else it's not a version at all).
    if !v.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// The CI run number embedded in a canary version string, wherever the channel's format hid
/// it: `0.23.0~ci10250.g<sha>` (deb), `0.23.0-0.ci10250.g<sha>` (rpm), `0.23.10250`
/// (Windows/decky style, run-as-patch). A stable string yields `None`.
pub(crate) fn canary_run(version: &str) -> Option<u64> {
    // `ci` immediately followed by digits, anywhere.
    let mut rest = version;
    while let Some(pos) = rest.find("ci") {
        let digits: String = rest[pos + 2..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
        rest = &rest[pos + 2..];
    }
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Some(patch),
        _ => None,
    }
}

/// Is the manifest's release newer than what this process runs? Definitive-or-false: an
/// unparseable pair never flags (the console still shows both version strings — the badge
/// just doesn't light up on guesswork). Canary compares `(major, minor)` then the CI run,
/// because canary patch fields mean different things per channel (R10).
pub(crate) fn is_newer(
    manifest_version: &str,
    manifest_ci_run: Option<u64>,
    current: &str,
    channel: Channel,
) -> bool {
    let (Some(m), Some(c)) = (triple(manifest_version), triple(current)) else {
        return false;
    };
    match channel {
        Channel::Stable => m > c,
        Channel::Canary => {
            if (m.0, m.1) != (c.0, c.1) {
                return (m.0, m.1) > (c.0, c.1);
            }
            let manifest_run = manifest_ci_run.or_else(|| canary_run(manifest_version));
            match (manifest_run, canary_run(current)) {
                (Some(mr), Some(cr)) => mr > cr,
                _ => false,
            }
        }
    }
}

/// The per-kind "how to update" command the console shows while (or instead of) an apply
/// path existing (design §5). One line, copy-pastable, no placeholders.
pub(crate) fn channel_hint(kind: InstallKind) -> &'static str {
    match kind {
        InstallKind::WindowsInstaller => {
            "winget upgrade unom.PunktfunkHost   (or re-run the newer installer)"
        }
        InstallKind::Sysext => "sudo punktfunk-sysext update",
        InstallKind::RpmOstree => {
            "sudo /usr/share/punktfunk/update-punktfunk.sh   (staged; reboot to finish)"
        }
        InstallKind::Apt => "sudo apt update && sudo apt install --only-upgrade punktfunk-host",
        InstallKind::Dnf => "sudo dnf upgrade punktfunk",
        InstallKind::Pacman => "sudo pacman -Syu",
        InstallKind::SteamosSource => "bash ~/punktfunk/scripts/steamdeck/update.sh --pull",
        InstallKind::Nix => "nix flake update punktfunk   (then rebuild your system)",
        InstallKind::Source => "git pull && cargo build --release -p punktfunk-host",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Probe {
        Probe {
            windows: false,
            exe: PathBuf::from("/usr/bin/punktfunk-host"),
            home: Some(PathBuf::from("/home/deck")),
            ..Default::default()
        }
    }

    #[test]
    fn ladder_sysext_beats_marker() {
        let mut p = probe();
        p.sysext = true;
        p.marker = Some("dnf canary".into());
        p.sysext_conf = Some("CHANNEL=canary\n".into());
        assert_eq!(classify(&p), (InstallKind::Sysext, Channel::Canary));
        p.sysext_conf = None;
        assert_eq!(classify(&p), (InstallKind::Sysext, Channel::Stable));
    }

    #[test]
    fn ladder_nix_store_path() {
        let mut p = probe();
        p.exe = PathBuf::from("/nix/store/abc123-punktfunk-host-0.22.2/bin/punktfunk-host");
        assert_eq!(classify(&p).0, InstallKind::Nix);
    }

    #[test]
    fn ladder_cargo_target_is_source_even_under_home() {
        let mut p = probe();
        p.exe = PathBuf::from("/home/deck/punktfunk/target/release/punktfunk-host");
        assert_eq!(classify(&p).0, InstallKind::Source);
    }

    #[test]
    fn ladder_deck_build_is_steamos_source() {
        let mut p = probe();
        p.exe = PathBuf::from("/home/deck/punktfunk/target-steamos/release/punktfunk-host");
        assert_eq!(classify(&p).0, InstallKind::SteamosSource);
    }

    #[test]
    fn ladder_markers() {
        for (marker, ostree, kind, channel) in [
            ("apt stable", false, InstallKind::Apt, Channel::Stable),
            ("apt canary", false, InstallKind::Apt, Channel::Canary),
            ("dnf stable", false, InstallKind::Dnf, Channel::Stable),
            ("dnf stable", true, InstallKind::RpmOstree, Channel::Stable),
            ("pacman canary", false, InstallKind::Pacman, Channel::Canary),
        ] {
            let mut p = probe();
            p.marker = Some(marker.into());
            p.ostree_booted = ostree;
            assert_eq!(classify(&p), (kind, channel), "marker `{marker}`");
        }
    }

    #[test]
    fn ladder_unknown_marker_falls_through_to_source() {
        let mut p = probe();
        p.marker = Some("snap stable".into());
        assert_eq!(classify(&p).0, InstallKind::Source);
    }

    #[test]
    fn triples() {
        assert_eq!(triple("0.23.0"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.0~ci10250.gab12cd34"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.10250"), Some((0, 23, 10250)));
        assert_eq!(triple("garbage"), None);
        assert_eq!(triple("1.2"), None);
    }

    #[test]
    fn canary_runs() {
        assert_eq!(canary_run("0.23.0~ci10250.gab12cd34"), Some(10250));
        assert_eq!(canary_run("0.23.0-0.ci777.g12345678"), Some(777));
        assert_eq!(canary_run("0.23.10250"), Some(10250)); // run-as-patch (Windows/decky)
        assert_eq!(canary_run("0.23.0"), None); // stable string
        assert_eq!(canary_run("0.23.0-1"), None);
    }

    #[test]
    fn newer_stable() {
        assert!(is_newer("0.23.0", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.2", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.1", None, "0.22.2", Channel::Stable)); // downgrade never flags
        assert!(!is_newer("not-a-version", None, "0.22.2", Channel::Stable));
    }

    #[test]
    fn newer_canary_compares_runs_not_patch() {
        // deb canary current vs Windows-style manifest version, same run ⇒ NOT newer,
        // even though a naive triple compare says 10250 > 0.
        assert!(!is_newer(
            "0.23.10250",
            Some(10250),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        assert!(is_newer(
            "0.23.10251",
            Some(10251),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        // Minor bump wins outright.
        assert!(is_newer(
            "0.24.100",
            Some(100),
            "0.23.0~ci10250.g12",
            Channel::Canary
        ));
        // No run extractable on either side ⇒ conservative false.
        assert!(!is_newer("0.23.10250", None, "0.23.0", Channel::Canary));
    }

    #[test]
    fn windows_channel_heuristic() {
        assert_eq!(windows_channel_of("0.22.2"), Channel::Stable);
        assert_eq!(windows_channel_of("0.23.10118"), Channel::Canary);
    }
}
