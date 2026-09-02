//! Install-kind and channel detection for the host and the Linux client.
//!
//! The apply path (and the command hint when there is none) hangs off the
//! install kind. The ladder reads facts the request side cannot influence:
//! a root-owned marker, a merged sysext extension-release, a flatpak
//! sandbox, a Nix store path.
//!
//! Host and client share the ladder; [`Product`] picks the marker, whether
//! a flatpak rung exists, and what a user-owned binary means. [`classify`]
//! is a pure function over [`Probe`], so each rung is unit-testable without
//! a box.

use crate::version::{conf_channel, windows_channel_of, Channel};
use std::path::{Path, PathBuf};

/// Which program is asking. Mixing host and client would misreport a client-only box as an un-updatable host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Host,
    Client,
}

impl Product {
    /// First word = kind (`apt`|`dnf`|`pacman`), optional second = channel (`stable`|`canary`).
    ///
    /// Separate files: a box can carry both packages, and two packages owning one path is a
    /// packaging conflict. The client file is in its own directory because the host RPM
    /// claims `%{_datadir}/punktfunk/*` — a sibling would be owned by both subpackages.
    pub fn marker_path(self) -> &'static str {
        match self {
            Product::Host => "/usr/share/punktfunk/install-kind",
            Product::Client => "/usr/share/punktfunk-client/install-kind",
        }
    }

    /// Merged sysext identity. Presence means `/usr` came from that image, even if a leftover marker remains.
    pub fn sysext_marker(self) -> &'static str {
        match self {
            Product::Host => "/usr/lib/extension-release.d/extension-release.punktfunk",
            Product::Client => "/usr/lib/extension-release.d/extension-release.punktfunk-client",
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            Product::Host => "punktfunk-host",
            Product::Client => "punktfunk-client",
        }
    }
}

/// `CHANNEL=stable|canary` for the sysext updater.
const SYSEXT_CONF: &str = "/etc/punktfunk-sysext.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    WindowsInstaller,
    /// Client only — the sandboxed GTK app (`io.unom.Punktfunk`).
    Flatpak,
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
    pub fn as_str(self) -> &'static str {
        match self {
            InstallKind::WindowsInstaller => "windows-installer",
            InstallKind::Flatpak => "flatpak",
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

/// Inputs to [`classify`]. Tests construct this; [`gather`] fills it from the box.
#[derive(Debug, Default)]
pub struct Probe {
    pub windows: bool,
    pub exe: PathBuf,
    pub home: Option<PathBuf>,
    pub flatpak: bool,
    pub marker: Option<String>,
    pub sysext: bool,
    pub sysext_conf: Option<String>,
    /// `/run/ostree-booted` (rpm-ostree / bootc).
    pub ostree_booted: bool,
    /// Fed to [`windows_channel_of`].
    pub version: String,
}

/// Live probe for `product`. Consumers cache [`classify`], not this.
pub fn gather(product: Product, version: &str) -> Probe {
    Probe {
        windows: cfg!(target_os = "windows"),
        exe: std::env::current_exe().unwrap_or_default(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        // FLATPAK_ID can be missing after a portal spawn; `/.flatpak-info` still exists.
        flatpak: product == Product::Client
            && (std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists()),
        marker: std::fs::read_to_string(product.marker_path()).ok(),
        sysext: Path::new(product.sysext_marker()).exists(),
        sysext_conf: std::fs::read_to_string(SYSEXT_CONF).ok(),
        ostree_booted: Path::new("/run/ostree-booted").exists(),
        version: version.to_string(),
    }
}

/// Order matters; each rung is a fact the caller cannot forge.
/// flatpak > sysext > Nix store > cargo `target/` > user-owned Deck build >
/// package marker (rpm-ostree when ostree-booted) > `source`.
pub fn classify(p: &Probe, product: Product) -> (InstallKind, Channel) {
    if p.windows {
        // Only the installer is a Windows delivery. A cargo build is not under
        // Program Files. Canary installers put the CI run in the third version
        // component (`M.m.<run>`).
        let installed = p
            .exe
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("\\program files\\punktfunk");
        return if installed {
            (
                InstallKind::WindowsInstaller,
                windows_channel_of(&p.version),
            )
        } else {
            (InstallKind::Source, Channel::Stable)
        };
    }

    // Sandbox `/usr` is the runtime; file rungs below would read the wrong box. Stay first.
    if p.flatpak {
        return (InstallKind::Flatpak, Channel::Stable);
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

    // Cargo `target/` is source even under $HOME. The Deck install lives in
    // `target-steamos/`, user-owned but not a cargo tree.
    let exe_str = p.exe.to_string_lossy().to_string();
    if exe_str.contains("/target/") {
        return (InstallKind::Source, Channel::Stable);
    }
    if let Some(home) = &p.home {
        if p.exe.starts_with(home) {
            // Only the host has an on-device Deck build (`scripts/steamdeck/update.sh`).
            // A client under $HOME is a private copy — report `source`.
            return match product {
                Product::Host => (InstallKind::SteamosSource, Channel::Canary),
                Product::Client => (InstallKind::Source, Channel::Stable),
            };
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
            // Ostree consumed the RPM by layering; `dnf upgrade` is not the update path.
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

/// One-line, copy-pastable "how to update" hint. No placeholders.
pub fn update_command(kind: InstallKind, product: Product) -> String {
    let bin = product.binary();
    match (kind, product) {
        (InstallKind::WindowsInstaller, _) => {
            "winget upgrade unom.PunktfunkHost   (or re-run the newer installer)".into()
        }
        (InstallKind::Flatpak, _) => "flatpak update --user io.unom.Punktfunk".into(),
        // The signed feed carries the host image only; the client sysext is a local rebuild.
        (InstallKind::Sysext, Product::Host) => "sudo punktfunk-sysext update".into(),
        (InstallKind::Sysext, Product::Client) => {
            "rebuild the client sysext: bash packaging/arch/build-sysext.sh <new .pkg.tar.zst> \
             && sudo cp punktfunk-client.raw /var/lib/extensions/ && sudo systemd-sysext refresh"
                .into()
        }
        (InstallKind::RpmOstree, Product::Host) => {
            "sudo /usr/share/punktfunk/update-punktfunk.sh   (staged; reboot to finish)".into()
        }
        (InstallKind::RpmOstree, Product::Client) => {
            format!("sudo rpm-ostree update --uninstall {bin} --install {bin}   (staged; reboot to finish)")
        }
        (InstallKind::Apt, _) => {
            format!("sudo apt update && sudo apt install --only-upgrade {bin}")
        }
        (InstallKind::Dnf, Product::Host) => "sudo dnf upgrade punktfunk".into(),
        (InstallKind::Dnf, Product::Client) => "sudo dnf upgrade punktfunk-client".into(),
        (InstallKind::Pacman, _) => "sudo pacman -Syu".into(),
        (InstallKind::SteamosSource, _) => {
            "bash ~/punktfunk/scripts/steamdeck/update.sh --pull".into()
        }
        (InstallKind::Nix, _) => "nix flake update punktfunk   (then rebuild your system)".into(),
        (InstallKind::Source, Product::Host) => {
            "git pull && cargo build --release -p punktfunk-host".into()
        }
        (InstallKind::Source, Product::Client) => {
            "git pull && cargo build --release -p punktfunk-client-linux".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(exe: &str) -> Probe {
        Probe {
            windows: false,
            exe: PathBuf::from(exe),
            home: Some(PathBuf::from("/home/deck")),
            ..Default::default()
        }
    }

    fn host_probe() -> Probe {
        probe("/usr/bin/punktfunk-host")
    }

    #[test]
    fn ladder_sysext_beats_marker() {
        let mut p = host_probe();
        p.sysext = true;
        p.marker = Some("dnf canary".into());
        p.sysext_conf = Some("CHANNEL=canary\n".into());
        assert_eq!(
            classify(&p, Product::Host),
            (InstallKind::Sysext, Channel::Canary)
        );
        p.sysext_conf = None;
        assert_eq!(
            classify(&p, Product::Host),
            (InstallKind::Sysext, Channel::Stable)
        );
    }

    #[test]
    fn ladder_nix_store_path() {
        let mut p = host_probe();
        p.exe = PathBuf::from("/nix/store/abc123-punktfunk-host-0.22.2/bin/punktfunk-host");
        assert_eq!(classify(&p, Product::Host).0, InstallKind::Nix);
    }

    #[test]
    fn ladder_cargo_target_is_source_even_under_home() {
        let mut p = host_probe();
        p.exe = PathBuf::from("/home/deck/punktfunk/target/release/punktfunk-host");
        assert_eq!(classify(&p, Product::Host).0, InstallKind::Source);
    }

    #[test]
    fn ladder_deck_build_is_steamos_source() {
        let mut p = host_probe();
        p.exe = PathBuf::from("/home/deck/punktfunk/target-steamos/release/punktfunk-host");
        assert_eq!(classify(&p, Product::Host).0, InstallKind::SteamosSource);
    }

    #[test]
    fn ladder_user_owned_client_is_plain_source() {
        let mut p = probe("/home/deck/.local/bin/punktfunk-client");
        p.marker = None;
        assert_eq!(classify(&p, Product::Client).0, InstallKind::Source);
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
            let mut p = host_probe();
            p.marker = Some(marker.into());
            p.ostree_booted = ostree;
            assert_eq!(
                classify(&p, Product::Host),
                (kind, channel),
                "marker `{marker}`"
            );
        }
    }

    #[test]
    fn ladder_unknown_marker_falls_through_to_source() {
        let mut p = host_probe();
        p.marker = Some("snap stable".into());
        assert_eq!(classify(&p, Product::Host).0, InstallKind::Source);
    }

    #[test]
    fn flatpak_wins_over_every_file_rung() {
        let mut p = probe("/app/bin/punktfunk-client");
        p.flatpak = true;
        p.sysext = true;
        p.marker = Some("pacman canary".into());
        assert_eq!(
            classify(&p, Product::Client),
            (InstallKind::Flatpak, Channel::Stable)
        );
    }

    #[test]
    fn windows_installed_vs_loose_build() {
        let mut p = Probe {
            windows: true,
            exe: PathBuf::from("C:\\Program Files\\Punktfunk\\punktfunk-host.exe"),
            version: "0.23.10118".into(),
            ..Default::default()
        };
        assert_eq!(
            classify(&p, Product::Host),
            (InstallKind::WindowsInstaller, Channel::Canary)
        );
        p.exe = PathBuf::from("C:\\src\\punktfunk\\target\\release\\punktfunk-host.exe");
        assert_eq!(classify(&p, Product::Host).0, InstallKind::Source);
    }

    #[test]
    fn hints_are_product_specific() {
        assert!(update_command(InstallKind::Apt, Product::Client).contains("punktfunk-client"));
        assert!(update_command(InstallKind::Apt, Product::Host).contains("punktfunk-host"));
        assert!(update_command(InstallKind::Flatpak, Product::Client).contains("flatpak update"));
    }
}
