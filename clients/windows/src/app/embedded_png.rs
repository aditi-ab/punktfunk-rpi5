//! PNGs baked into the exe, staged on disk for WinUI. Reactor's `ImageSource` is `file:///`-URI
//! raster only (no vector element, no icon font with brand glyphs), so each table is written
//! once under `%LOCALAPPDATA%\punktfunk\<dir>\` and read back by URI.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One table of embedded PNGs, each staged as `<token>.png`.
pub(super) struct EmbeddedPngs {
    dir: &'static str,
    files: &'static [(&'static str, &'static [u8])],
    path: OnceLock<Option<PathBuf>>,
}

impl EmbeddedPngs {
    pub(super) const fn new(
        dir: &'static str,
        files: &'static [(&'static str, &'static [u8])],
    ) -> EmbeddedPngs {
        EmbeddedPngs {
            dir,
            files,
            path: OnceLock::new(),
        }
    }

    fn path(&self) -> Option<&Path> {
        self.path
            .get_or_init(|| {
                let base = std::env::var_os("LOCALAPPDATA")?;
                Some(PathBuf::from(base).join("punktfunk").join(self.dir))
            })
            .as_deref()
    }

    /// Write the table to disk. Idempotent; a size mismatch rewrites, so a mark re-baked in a
    /// newer build lands. Called once at GUI startup, before any tile renders.
    pub(super) fn install(&self) {
        let Some(dir) = self.path() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return; // tiles just render without the mark
        }
        for (token, bytes) in self.files {
            let p = dir.join(format!("{token}.png"));
            let stale = std::fs::metadata(&p)
                .map(|m| m.len() != bytes.len() as u64)
                .unwrap_or(true);
            if stale {
                let _ = std::fs::write(&p, bytes);
            }
        }
    }

    /// Whether the table ships a PNG for `token`.
    pub(super) fn has(&self, token: &str) -> bool {
        self.files.iter().any(|(name, _)| *name == token)
    }

    /// The `file:///` URI of `token`'s PNG, or `None` when the table has none or it never
    /// reached disk. The table check runs before the path join, so nothing a host sends can
    /// steer this at a file of its choosing.
    pub(super) fn uri(&self, token: &str) -> Option<String> {
        if !self.has(token) {
            return None;
        }
        let p = self.path()?.join(format!("{token}.png"));
        p.exists().then(|| file_uri(&p))
    }
}

/// A local path as the `file:///` URI reactor's `ImageSource` loads.
pub(super) fn file_uri(p: &Path) -> String {
    format!("file:///{}", p.display().to_string().replace('\\', "/"))
}
