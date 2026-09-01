//! The wizard's brand pieces: the lens mark, staged to disk for reactor's raster-only image
//! element (the os_icons disk-cache-to-URI pattern), and the brand violet the stepper fills
//! with (D9).
//!
//! The PNG is rendered from `web/public/favicon.svg`'s geometry by
//! `scripts/gen-setup-brand-mark.py` — regenerate and commit when the mark changes.

use std::path::PathBuf;
use std::sync::OnceLock;

const MARK: &[u8] = include_bytes!("../assets/lens-mark.png");

/// pf-console-ui's dark-appearance brand value; the terminal theme carries the same one.
pub const VIOLET: windows_reactor::Color = windows_reactor::Color {
    a: 255,
    r: 0x86,
    g: 0x78,
    b: 0xf5,
};

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("punktfunk").join("setup-brand"))
}

/// Materialize the mark (idempotent; size mismatch rewrites so a re-baked mark lands).
pub fn install() {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return; // the Welcome page just renders without the mark
    }
    let p = dir.join("lens-mark.png");
    let fresh = std::fs::metadata(&p)
        .map(|m| m.len() != MARK.len() as u64)
        .unwrap_or(true);
    if fresh {
        let _ = std::fs::write(&p, MARK);
    }
}

/// The mark's `file:///` URI; `None` (no image element at all) when staging failed.
pub fn mark_uri() -> Option<String> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let dir = DIR.get_or_init(dir).as_ref()?;
    let p = dir.join("lens-mark.png");
    p.exists()
        .then(|| format!("file:///{}", p.display().to_string().replace('\\', "/")))
}
