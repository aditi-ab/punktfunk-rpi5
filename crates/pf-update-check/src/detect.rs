//! **How was this installed, and on which channel?** (design §4.1)
//!
//! The apply strategy — and, where no apply leg exists, the command hint a UI shows — hangs
//! off the install kind. Detection is a ladder over facts nothing request-side can influence:
//! packaging writes a root-owned marker, a sysext self-identifies via its merged
//! extension-release, a flatpak by its sandbox, Nix by store path, and so on.
//!
//! The ladder is shared between the host and the client because the delivery channels are the
//! same ones; what differs is spelled out in [`Product`] — which marker file to read, whether
//! a flatpak rung exists at all, and what a user-owned binary means. The ladder itself is a
//! pure function over a [`Probe`], so every rung is unit-testable without a box.

use crate::version::{conf_channel, windows_channel_of, Channel};
use std::path::{Path, PathBuf};

/// Which punktfunk program is asking. The rungs differ (see the module docs), and mixing them
/// up would misreport a client-only box as an un-updatable host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Host,
    Client,
}

impl Product {
    /// Where this product's packaging stamps how it was installed. First word = kind
    /// (`apt`|`dnf`|`pacman`), optional second word = channel (`stable`|`canary`).
    ///
    /// The two products use SEPARATE files on purpose: a box can carry both packages, and
    /// every packaging format we ship treats two packages owning one path as a hard conflict.
    ///
    /// The client's lives in its own DIRECTORY, not just under its own name, because the host
    /// RPM claims `%{_datadir}/punktfunk/*` with a glob — a sibling file in there would be
    /// owned by both subpackages and `dnf install punktfunk punktfunk-client` would refuse.
    pub fn marker_path(self) -> &'static str {
        match self {
            Product::Host => "/usr/share/punktfunk/install-kind",
            Product::Client => "/usr/share/punktfunk-client/install-kind",
        }
    }

    /// The merged sysext names itself here (written by the sysext build scripts); its presence
    /// means the running `/usr` overlay came from that image, regardless of any leftover marker.
    pub fn sysext_marker(self) -> &'static str {
        match self {
            Product::Host => "/usr/lib/extension-release.d/extension-release.punktfunk",
            Product::Client => "/usr/lib/extension-release.d/extension-release.punktfunk-client",
        }
    }

    /// The binary name, for the command hints.
    pub fn binary(self) -> &'static str {
        match self {
            Product::Host => "punktfunk-host",
            Product::Client => "punktfunk-client",
        }
    }
}

/// The sysext updater's own config (`CHANNEL=stable|canary`).
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

/// The facts the ladder reads, gathered once by [`gather`] (tests build these directly).
#[derive(Debug, Default)]
pub struct Probe {
    /// Running on Windows (cfg, not a file).
    pub windows: bool,
    /// The running exe's path.
    pub exe: PathBuf,
    /// `$HOME`, if any.
    pub home: Option<PathBuf>,
    /// This process is inside a flatpak sandbox.
    pub flatpak: bool,
    /// Contents of [`Product::marker_path`], if present.
    pub marker: Option<String>,
    /// [`Product::sysext_marker`] exists (merged sysext overlay).
    pub sysext: bool,
    /// Contents of [`SYSEXT_CONF`], if present.
    pub sysext_conf: Option<String>,
    /// `/run/ostree-booted` exists (rpm-ostree / bootc family).
    pub ostree_booted: bool,
    /// The asking binary's own version string, for the Windows channel heuristic.
    pub version: String,
}

/// Gather the real probe for `product`. Cheap (a few `stat`s and two small reads); consumers
/// cache the classification, not this.
pub fn gather(product: Product, version: &str) -> Probe {
    Probe {
        windows: cfg!(target_os = "windows"),
        exe: std::env::current_exe().unwrap_or_default(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        // Both are set by flatpak inside the sandbox; the exe path is the belt to that
        // env-var braces, since a portal-spawned process can inherit a stripped environment.
        flatpak: product == Product::Client
            && (std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists()),
        marker: std::fs::read_to_string(product.marker_path()).ok(),
        sysext: Path::new(product.sysext_marker()).exists(),
        sysext_conf: std::fs::read_to_string(SYSEXT_CONF).ok(),
        ostree_booted: Path::new("/run/ostree-booted").exists(),
        version: version.to_string(),
    }
}

/// The ladder (design §4.1). Order matters and each rung is a fact the caller can't forge:
/// flatpak sandbox > sysext overlay > Nix store path > dev/source tree > user-owned Deck
/// build > package marker (flipped to rpm-ostree when the box is ostree-booted) > `source`.
pub fn classify(p: &Probe, product: Product) -> (InstallKind, Channel) {
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
                windows_channel_of(&p.version),
            )
        } else {
            (InstallKind::Source, Channel::Stable)
        };
    }

    // Inside the sandbox `/usr` is the runtime's, so every file rung below would read the
    // WRONG box's facts. This must stay first.
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

    // A cargo tree anywhere (CI, dev box, the Deck checkout mid-build) is `source`; the
    // Deck's install script runs the binary out of `~/punktfunk/target-steamos/`, which is
    // user-owned but NOT a plain `target/` dir — that distinction is the marker here.
    let exe_str = p.exe.to_string_lossy().to_string();
    if exe_str.contains("/target/") {
        return (InstallKind::Source, Channel::Stable);
    }
    if let Some(home) = &p.home {
        if p.exe.starts_with(home) {
            // Only the HOST has an on-device Deck build (scripts/steamdeck/update.sh builds
            // the host). A client binary under $HOME is someone's own build or a copy into
            // ~/.local/bin — nothing knows how to update it, so say `source` and mean it.
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
            // An ostree-booted box consumed the RPM by layering (or an image build); either
            // way `dnf upgrade` is not how it updates.
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

/// The per-kind "how to update" command a UI shows while (or instead of) an apply path
/// existing (design §5). One line, copy-pastable, no placeholders.
pub fn update_command(kind: InstallKind, product: Product) -> String {
    let bin = product.binary();
    match (kind, product) {
        (InstallKind::WindowsInstaller, _) => {
            "winget upgrade unom.PunktfunkHost   (or re-run the newer installer)".into()
        }
        (InstallKind::Flatpak, _) => "flatpak update --user io.unom.Punktfunk".into(),
        // The signed sysext feed carries the HOST image only; a client sysext is the local
        // `packaging/arch/build-sysext.sh` wrapper, which has no feed to update from.
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

    /// The same user-owned path means something else for the client: there is no on-device
    /// client build script, so it must not claim the Deck source-rebuild leg.
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

    /// Inside the sandbox every /usr fact belongs to the flatpak runtime, not the box — so a
    /// leftover host marker on the real system must not win.
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

    /// Command hints are user-facing copy — they must name the right package per product.
    #[test]
    fn hints_are_product_specific() {
        assert!(update_command(InstallKind::Apt, Product::Client).contains("punktfunk-client"));
        assert!(update_command(InstallKind::Apt, Product::Host).contains("punktfunk-host"));
        assert!(update_command(InstallKind::Flatpak, Product::Client).contains("flatpak update"));
    }
}
