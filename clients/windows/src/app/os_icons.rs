//! The host tiles' OS marks. Reactor's `ImageSource` is `file:///`-URI raster only (no
//! vector element, no icon font with brand glyphs), so the monochrome PNGs under
//! `assets/os/` (mid-gray — legible on both WinUI themes; derived from the
//! `assets/os-icons` masters, see that README for provenance/licensing) are embedded in
//! the exe and materialized once into `%LOCALAPPDATA%\punktfunk\os-icons\` — the same
//! disk-cache-to-URI pattern as the library's poster art.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Embedded PNG per icon token: the families the chain walk can land on, plus the gaming
/// distros that earn their own mark because "a Bazzite box" and "a Fedora box" are
/// different machines to the person reading the tile. A distro with no mark of its own
/// still degrades to its family's and finally to Tux.
const ICONS: &[(&str, &[u8])] = &[
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
];

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("punktfunk").join("os-icons"))
}

/// Materialize the embedded PNGs to disk (idempotent; size mismatch rewrites, so an icon
/// refresh in a newer build lands). Called once at GUI startup, before any tile renders.
pub fn install() {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return; // tiles just render without the mark
    }
    for (token, bytes) in ICONS {
        let p = dir.join(format!("{token}.png"));
        let fresh = std::fs::metadata(&p)
            .map(|m| m.len() != bytes.len() as u64)
            .unwrap_or(true);
        if fresh {
            let _ = std::fs::write(&p, bytes);
        }
    }
}

/// The `file:///` URI of the mark for an OS-identity chain: walk most-specific-first
/// (pf-client-core's shared order/aliases) and take the first token we ship art for.
/// `None` (no image element at all) when the host doesn't advertise a chain or nothing
/// in it is recognized — the tile then renders exactly as it did before the field.
pub fn uri(chain: &str) -> Option<String> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let dir = DIR.get_or_init(dir).as_ref()?;
    let token = pf_client_core::os::os_icon_tokens(chain)
        .into_iter()
        .find(|t| ICONS.iter().any(|(name, _)| name == t))?;
    let p = dir.join(format!("{token}.png"));
    p.exists()
        .then(|| format!("file:///{}", p.display().to_string().replace('\\', "/")))
}
