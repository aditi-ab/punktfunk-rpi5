//! Lutris store provider: installed games from the Lutris SQLite DB + lutris.net CDN art. Split out of the `library` facade (plan §W5).

use super::*;

/// Reads the **local** Lutris library DB (`pga.db`) — no network. Installed titles only; cover art
/// from Lutris's on-disk cache, inlined as `data:` URLs. Linux-only (Lutris is Linux-only).
#[cfg(target_os = "linux")]
pub struct LutrisProvider;

#[cfg(target_os = "linux")]
impl LibraryProvider for LutrisProvider {
    fn store(&self) -> &'static str {
        "lutris"
    }

    fn list(&self) -> Vec<GameEntry> {
        let Some(db) = lutris_db() else {
            return Vec::new();
        };
        lutris_games(&db).unwrap_or_else(|e| {
            tracing::warn!(error = %e, db = %db.display(), "lutris pga.db read failed — skipping");
            Vec::new()
        })
    }
}

/// The first existing Lutris `pga.db`: XDG data dir, the classic `~/.local/share`, or Flatpak.
#[cfg(target_os = "linux")]
fn lutris_db() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(d).join("lutris/pga.db"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/lutris/pga.db"));
        candidates.push(home.join(".var/app/net.lutris.Lutris/data/lutris/pga.db"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Installed games from a Lutris `pga.db`. Opened **read-only + immutable** (via a SQLite URI) so a
/// running Lutris holding the file can't make us block or fail, and we never write to it.
#[cfg(target_os = "linux")]
fn lutris_games(db: &Path) -> rusqlite::Result<Vec<GameEntry>> {
    use rusqlite::OpenFlags;
    // `immutable=1` treats the DB as read-only-and-unchanging → no locking against a live Lutris. The
    // path goes into the URI literally; a `?`/`#` in it (vanishingly rare on Linux) would mis-parse,
    // so fall back to a plain read-only open in that case.
    let path = db.to_string_lossy();
    let conn = if path.contains('?') || path.contains('#') {
        rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        rusqlite::Connection::open_with_flags(
            format!("file:{path}?immutable=1"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?
    };
    // `directory` (the game's install dir — our detect signal) is not load-bearing for the library, so
    // a pga.db schema without it must not cost the whole Lutris store: try the richer query first and
    // fall back to the historical one on any prepare error.
    const SELECT_WITH_DIR: &str = "SELECT id, slug, name, directory FROM games \
         WHERE installed = 1 AND name IS NOT NULL AND name <> '' \
         ORDER BY name COLLATE NOCASE";
    const SELECT_PLAIN: &str = "SELECT id, slug, name, NULL FROM games \
         WHERE installed = 1 AND name IS NOT NULL AND name <> '' \
         ORDER BY name COLLATE NOCASE";
    let mut stmt = match conn.prepare(SELECT_WITH_DIR) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "lutris pga.db has no `directory` column — listing without \
                 install dirs (game-exit detection unavailable for Lutris titles)");
            conn.prepare(SELECT_PLAIN)?
        }
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut games = Vec::new();
    for (id, slug, name, directory) in rows.flatten() {
        games.push(GameEntry {
            provider: None,
            id: format!("lutris:{id}"),
            store: "lutris".into(),
            title: name,
            art: slug.as_deref().map(lutris_art).unwrap_or_default(),
            launch: Some(LaunchSpec {
                kind: "lutris_id".into(),
                value: id.to_string(),
            }),
            // Lutris stamps no per-game env marker we can rely on, so the install dir is the whole
            // recipe; a game with none (an emulator entry pointing at a bare ROM) stays untracked.
            detect: directory
                .filter(|d| !d.trim().is_empty())
                .map(DetectSpec::dir)
                .unwrap_or_default(),
        });
    }
    Ok(games)
}

/// Lutris cover art (local files keyed by slug) inlined as `data:` URLs — Lutris has no public CDN
/// keyed by a stable id (unlike Steam/Heroic), and `Artwork` fields are URLs the client fetches, so a
/// self-contained `data:` URL needs no host-served endpoint. `coverart` → portrait, `banners` → header.
#[cfg(target_os = "linux")]
fn lutris_art(slug: &str) -> Artwork {
    Artwork {
        portrait: lutris_image("coverart", slug),
        header: lutris_image("banners", slug),
        ..Default::default()
    }
}

/// Find `<kind>/<slug>.jpg` across the current (0.5.18+), legacy (`~/.cache`), and Flatpak Lutris
/// dirs and inline it as `data:image/jpeg;base64,…`. Skips a missing or implausibly large file (a
/// 1 MiB cap bounds the catalog JSON so a few big files can't bloat it).
#[cfg(target_os = "linux")]
fn lutris_image(kind: &str, slug: &str) -> Option<String> {
    use base64::Engine as _;
    // `slug` comes verbatim from Lutris's `pga.db` (untrusted at this layer). Reject any path
    // separator, parent ref, or NUL so a crafted slug can't escape the art roots and read an
    // arbitrary `<slug>.jpg` off disk — the bytes are base64-inlined into the `/api/v1/library`
    // JSON a paired client can GET, so an escape is an arbitrary-file-read exfil primitive
    // (security-review 2026-07-17). Real Lutris slugs are `[a-z0-9-]`.
    if slug.is_empty()
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains("..")
        || slug.contains('\0')
    {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let roots = [
        home.join(".local/share/lutris"),
        home.join(".cache/lutris"),
        home.join(".var/app/net.lutris.Lutris/data/lutris"),
        home.join(".var/app/net.lutris.Lutris/cache/lutris"),
    ];
    for root in roots {
        let p = root.join(kind).join(format!("{slug}.jpg"));
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > 1024 * 1024 {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&p) {
            let enc = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Some(format!("data:image/jpeg;base64,{enc}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn lutris_games_reads_installed_only() {
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join(format!("pf-lutris-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pga.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
            "CREATE TABLE games (id INTEGER PRIMARY KEY, slug TEXT, name TEXT, installed INTEGER);
             INSERT INTO games (id,slug,name,installed) VALUES (42,'elden-ring','ELDEN RING',1);
             INSERT INTO games (id,slug,name,installed) VALUES (7,'owned','Owned Only',0);
             INSERT INTO games (id,slug,name,installed) VALUES (9,'noname',NULL,1);",
        )
        .unwrap();
        }
        let games = lutris_games(&db).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // Only the installed, named row; the uninstalled + NULL-name rows are filtered out.
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].id, "lutris:42");
        assert_eq!(games[0].store, "lutris");
        assert_eq!(games[0].title, "ELDEN RING");
        let l = games[0].launch.as_ref().unwrap();
        assert_eq!((l.kind.as_str(), l.value.as_str()), ("lutris_id", "42"));
    }
}
