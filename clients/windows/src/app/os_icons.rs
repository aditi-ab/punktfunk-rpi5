//! The host card's OS mark — the avatar's face, and the only place this shell draws the host's
//! operating system: the monochrome PNGs under `assets/os/` (WHITE, because the avatar is an
//! accent-filled circle on both themes; derived from the `assets/os-icons` masters, see that
//! README for provenance/licensing), staged through [`EmbeddedPngs`].

use super::embedded_png::EmbeddedPngs;

/// Embedded PNG per icon token: the families the chain walk can land on, plus the distro
/// leaves that earn their own mark because "a Bazzite box" and "a Fedora box" are
/// different machines to the person reading the tile. A distro with no mark of its own
/// still degrades to its family's and finally to Tux.
static ICONS: EmbeddedPngs = EmbeddedPngs::new(
    "os-icons",
    &[
        ("windows", include_bytes!("../../assets/os/windows.png")),
        ("apple", include_bytes!("../../assets/os/apple.png")),
        ("linux", include_bytes!("../../assets/os/linux.png")),
        ("steam", include_bytes!("../../assets/os/steam.png")),
        ("ubuntu", include_bytes!("../../assets/os/ubuntu.png")),
        ("fedora", include_bytes!("../../assets/os/fedora.png")),
        ("arch", include_bytes!("../../assets/os/arch.png")),
        ("debian", include_bytes!("../../assets/os/debian.png")),
        ("nixos", include_bytes!("../../assets/os/nixos.png")),
        ("opensuse", include_bytes!("../../assets/os/opensuse.png")),
        ("bazzite", include_bytes!("../../assets/os/bazzite.png")),
        ("cachyos", include_bytes!("../../assets/os/cachyos.png")),
        ("nobara", include_bytes!("../../assets/os/nobara.png")),
        ("omarchy", include_bytes!("../../assets/os/omarchy.png")),
    ],
);

/// Stage the marks on disk. Called once at GUI startup, before any tile renders.
pub fn install() {
    ICONS.install();
}

/// The `file:///` URI of the mark for an OS-identity chain: walk most-specific-first
/// (pf-client-core's shared order/aliases) and take the first token we ship art for.
/// `None` (no image element at all) when the host doesn't advertise a chain or nothing in it
/// is recognized — which is what keeps such a host's avatar on its name's initial rather than
/// leaving an empty circle.
pub fn uri(chain: &str) -> Option<String> {
    let token = pf_client_core::os::os_icon_tokens(chain)
        .into_iter()
        .find(|t| ICONS.has(t))?;
    ICONS.uri(&token)
}
