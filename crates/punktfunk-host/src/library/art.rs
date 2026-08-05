//! Artwork cache + background warmer: the on-disk poster cache, the per-store fetchers, and the
//! `fetch_box_art` dispatch the management art proxy serves from. Split out of the `library` facade (plan §W5).

use super::*;

/// The persisted art cache: GameEntry id → resolved [`Artwork`]. An entry's PRESENCE means "already
/// resolved" (even an empty Artwork = fetched, none found) so the warmer never re-fetches it.
fn art_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Artwork>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Artwork>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let loaded = std::fs::read_to_string(art_cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        std::sync::Mutex::new(loaded)
    })
}

/// The art cache lives in the canonical HOST config dir (`%ProgramData%\punktfunk` on Windows /
/// `~/.config/punktfunk` on Linux — `pf_paths::config_dir`, NOT the legacy XDG/HOME `config_dir`
/// below that the custom store still uses).
fn art_cache_path() -> PathBuf {
    pf_paths::config_dir().join("library-art-cache.json")
}

/// The cached art for a library id, if it has been resolved (positive or negative). `None` = not yet
/// warmed → the provider shows title-only until the warmer fills it in.
pub(crate) fn cached_art(id: &str) -> Option<Artwork> {
    art_cache().lock().unwrap().get(id).cloned()
}

/// Record resolved art for a library id + persist the cache (write-then-rename; best-effort).
fn store_art(id: &str, art: Artwork) {
    let mut cache = art_cache().lock().unwrap();
    cache.insert(id.to_string(), art);
    if let Ok(json) = serde_json::to_string(&*cache) {
        let path = art_cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Start the host-lifetime cover-art warmer: every few minutes, fetch + cache art for any library
/// entry whose store needs a network lookup (GOG / Xbox) and isn't cached yet. Idempotent — once
/// everything is cached a pass makes no network calls (and a host with only self-art stores never
/// fetches at all). Call once from `serve()`; the returned handle can be dropped to detach it.
pub fn start_art_warmer() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("pf-art-warmer".into())
        .spawn(|| loop {
            warm_art_once();
            std::thread::sleep(std::time::Duration::from_secs(300));
        })
        .expect("spawn art warmer thread")
}

/// One warming pass: resolve uncached GOG/Xbox art. Other stores carry their own art (Steam CDN
/// template, Heroic CDN URLs, Lutris data: URLs, custom user URLs) and are skipped.
fn warm_art_once() {
    for g in all_games() {
        if cached_art(&g.id).is_some() {
            continue;
        }
        let Some((store, localid)) = g.id.split_once(':') else {
            continue;
        };
        let art = match store {
            "gog" => fetch_gog_art(localid),
            // The xbox id is the StoreId when present, else the PFN (contains '_', no displaycatalog
            // entry) → cache empty for those so they aren't retried every pass.
            "xbox" if !localid.contains('_') => fetch_xbox_art(localid),
            "xbox" => Artwork::default(),
            _ => continue, // steam/heroic/lutris/custom resolve their own art
        };
        store_art(&g.id, art);
    }
}

/// HTTP GET + parse JSON with a bounded timeout. `None` on any network/parse failure (best-effort —
/// art is non-essential, so a failure just leaves the title-only card).
fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        // Don't follow redirects — a redirect target (`3xx` → `http://169.254.169.254/…` or an
        // internal host) would be an SSRF pivot from the privileged host. Matches the webhook path
        // (security-review 2026-07-17). A rare legitimately-redirecting CDN just yields no art.
        .redirects(0)
        .build();
    let body = agent.get(url).call().ok()?.into_string().ok()?;
    serde_json::from_str(&body).ok()
}

/// Fetch one image URL for the GameStream `/appasset` cover proxy, as `(bytes, content-type)`. Handles
/// `data:` URLs (Lutris inlines art that way) by decoding inline, and `http(s)` URLs by a bounded GET
/// (8 MiB cap so a hostile/huge art URL can't balloon host memory). `None` on any non-image scheme,
/// network/decoder error, or empty body. Blocking (ureq) — call off the async runtime.
pub(crate) fn fetch_image(url: &str) -> Option<(Vec<u8>, String)> {
    use base64::Engine as _;
    use std::io::Read as _;
    if let Some(rest) = url.strip_prefix("data:") {
        // data:[<mediatype>][;base64],<payload>
        let (meta, data) = rest.split_once(',')?;
        let ctype = meta
            .split(';')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = if meta.contains(";base64") {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?
        } else {
            data.as_bytes().to_vec()
        };
        return (!bytes.is_empty()).then_some((bytes, ctype));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        // Don't follow redirects (SSRF pivot): this is called on launcher-cache- and custom-entry-
        // supplied URLs, so a `3xx` to an internal/metadata endpoint must not be chased by the
        // privileged host. Matches the webhook path (security-review 2026-07-17).
        .redirects(0)
        .build();
    let resp = agent.get(url).call().ok()?;
    let ctype = resp
        .header("Content-Type")
        .unwrap_or("image/jpeg")
        .to_string();
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty()).then_some((bytes, ctype))
}

/// A stored [`Artwork`] value that is a **local filesystem path** to an image on the host — as
/// opposed to an `http(s)`/`data:` URL or an already-relative host proxy path. Provider plugins that
/// run on the host (e.g. the Playnite sync plugin) set these: the reconcile payload stays tiny
/// (paths, not inlined bytes, so it scales to thousands of titles) and the host serves the bytes
/// through the art proxy, exactly like Steam's cache art. Windows-shaped only (`C:\…`, `C:/…`, or a
/// `\\server\share` UNC) — Playnite, the only local-art provider, is Windows-only, and this keeps the
/// check from ever mistaking the `/api/…` proxy path (or a POSIX abs path) for a local file.
pub fn is_local_art_path(v: &str) -> bool {
    if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("data:") {
        return false;
    }
    let b = v.as_bytes();
    (b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || v.starts_with("\\\\")
}

/// The filesystem roots the art proxy is allowed to read from.
///
/// The proxy runs in the **host process** — LocalSystem on Windows — and both the path and the
/// read-back are reachable from the plugin lane, which runs as the much weaker LocalService. Without
/// a root, "serve this entry's cover" is "read any file on the box as SYSTEM" (2026-08-05 review
/// H-2): `mgmt-token`, `key.pem`, the SAM hive. So the value is confined here, at the one place
/// bytes are read, rather than trusted because of where it was written.
///
/// Default: the users base (`C:\Users`), which is where every launcher keeps its art cache —
/// Playnite, the only local-art provider, stores covers under `%APPDATA%\Playnite`. Derived from
/// `%PUBLIC%`'s parent because the host runs as SYSTEM, whose own `%USERPROFILE%` is
/// `…\config\systemprofile` and tells us nothing about where the operator's launchers live.
/// `PUNKTFUNK_LIBRARY_ART_ROOTS` (`;`-separated) replaces the default for an operator whose library
/// is on another drive.
fn art_roots() -> Vec<PathBuf> {
    if let Some(configured) = std::env::var_os("PUNKTFUNK_LIBRARY_ART_ROOTS") {
        return std::env::split_paths(&configured)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
    }
    let mut roots = Vec::new();
    // `%PUBLIC%` is `C:\Users\Public` on every supported Windows; its parent is the users base.
    if let Some(public) = std::env::var_os("PUBLIC") {
        if let Some(base) = PathBuf::from(public).parent() {
            roots.push(base.to_path_buf());
        }
    }
    if roots.is_empty() {
        if let Some(drive) = std::env::var_os("SystemDrive") {
            roots.push(PathBuf::from(drive).join("Users"));
        }
    }
    roots
}

/// Whether `path` resolves inside one of [`art_roots`] and outside the host config dir.
///
/// Canonicalizes first, so a junction/symlink pointing out of the root is resolved before the
/// containment test rather than after it. The config-dir exclusion is unconditional — it holds even
/// if an operator's `PUNKTFUNK_LIBRARY_ART_ROOTS` were to contain it — because that directory is
/// where every host secret lives.
fn art_path_is_confined(path: &Path) -> bool {
    // A UNC value (`\\attacker\share\a.png`) is refused outright: reading it would coerce the host's
    // machine account into outbound SMB authentication to a peer of the caller's choosing.
    if path.to_string_lossy().starts_with(r"\\") {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    if let Ok(config) = pf_paths::config_dir().canonicalize() {
        if real.starts_with(&config) {
            return false;
        }
    }
    art_roots()
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|root| real.starts_with(&root))
}

/// Sniff an image container from its leading bytes → the content type to serve. `None` for anything
/// that is not a recognized image.
///
/// The proxy serves what the bytes ARE, not what the extension claims, and refuses to serve at all
/// when they are not an image — which is what keeps an extensionless secret like `mgmt-token` (or a
/// `key.pem` renamed `cover.png`) from being returned as `application/octet-stream`.
fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.starts_with(sig);
    if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if starts(b"BM") {
        return Some("image/bmp");
    }
    if starts(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("image/x-icon");
    }
    // TGA has no magic number. Validate the fixed header fields instead (colour-map type is 0/1,
    // image type is one of the six defined codes) — enough that no plausible secret passes.
    if bytes.len() >= 18
        && matches!(bytes[1], 0 | 1)
        && matches!(bytes[2], 0 | 1 | 2 | 3 | 9 | 10 | 11)
    {
        return Some("image/x-tga");
    }
    None
}

/// Whether a local art path is servable at all: known image extension, inside an allowed root. The
/// write-time half of the art confinement — [`validate_art_paths`] refuses to persist a value this
/// rejects, so an out-of-root path never reaches the catalog in the first place, and
/// [`local_art_bytes`] re-checks at read time so an entry written before this existed is still safe.
pub fn art_path_is_servable(value: &str) -> bool {
    let p = Path::new(value);
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "ico" | "tga"
            )
        });
    ext_ok && art_path_is_confined(p)
}

/// Reject any **local-file** art value that the proxy would refuse to serve, so an unservable path
/// (out of root, not an image, a UNC share) can never be persisted. URLs and already-proxied paths
/// are not this function's business and pass through. `Err` carries the offending field name.
pub fn validate_art_paths(art: &Artwork) -> Result<(), String> {
    for (field, value) in [
        ("portrait", &art.portrait),
        ("hero", &art.hero),
        ("logo", &art.logo),
        ("header", &art.header),
    ] {
        let Some(v) = value.as_deref() else { continue };
        if is_local_art_path(v) && !art_path_is_servable(v) {
            return Err(format!(
                "art.{field}: local art must be an image file (jpg/png/webp/gif/bmp/ico/tga) inside \
                 an allowed art root — set PUNKTFUNK_LIBRARY_ART_ROOTS if the library lives \
                 elsewhere, or send an http(s) URL instead"
            ));
        }
    }
    Ok(())
}

/// Read a local image file into `(bytes, content-type)` for the art proxy. `None` if it isn't an
/// existing regular file, is empty, exceeds 16 MiB (a cover never approaches that; the cap bounds
/// host memory), resolves outside the allowed art roots ([`art_path_is_confined`]), or does not
/// actually contain an image ([`sniff_image_type`]).
///
/// This is the single place local art bytes are read — the mgmt art proxy and the GameStream
/// `/appasset` proxy both land here — so the confinement holds for every caller.
pub fn local_art_bytes(path: &str) -> Option<(Vec<u8>, String)> {
    if !art_path_is_servable(path) {
        tracing::debug!(
            path,
            "art proxy: refusing a path outside the allowed art roots"
        );
        return None;
    }
    let p = std::path::Path::new(path);
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > 16 * 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    // Serve what the bytes ARE. A file that is not an image is not served at all.
    let ctype = sniff_image_type(&bytes)?;
    Some((bytes, ctype.to_string()))
}

/// Resolve one art value to bytes for the Moonlight `/appasset` proxy: a local host file
/// ([`is_local_art_path`]) is read directly, anything else is a URL fetched by [`fetch_image`].
fn resolve_art_bytes(v: &str) -> Option<(Vec<u8>, String)> {
    if is_local_art_path(v) {
        local_art_bytes(v)
    } else {
        fetch_image(v)
    }
}

/// Rewrite any **local-file** art paths on an entry into host art-proxy URLs
/// (`/api/v1/library/art/<id>/<kind>`, the same relative-proxy shape Steam art uses, resolved by the
/// client against the host). `http(s)`/`data:` URLs and already-relative proxy paths are left as-is.
/// Applied to the `GET /library` response so a client fetches a provider's local covers from the host
/// instead of receiving an unreachable `C:\…` path.
pub fn proxy_local_art(id: &str, art: &mut Artwork) {
    let rw = |field: &mut Option<String>, kind: &str| {
        if field.as_deref().is_some_and(is_local_art_path) {
            *field = Some(format!("/api/v1/library/art/{id}/{kind}"));
        }
    };
    rw(&mut art.portrait, "portrait");
    rw(&mut art.hero, "hero");
    rw(&mut art.logo, "logo");
    rw(&mut art.header, "header");
}

/// Resolve + fetch the best box-art cover for a library id (the GameStream `/appasset` proxy — Moonlight
/// fetches per-app covers from the HOST, not the CDN, so we proxy the bytes). Tries the portrait (tall
/// capsule Moonlight wants) → header → hero → logo, returning the first that fetches as
/// `(bytes, content-type)`. Resolves the id against the host's OWN library. Blocking — call off the
/// async runtime (e.g. `spawn_blocking`).
pub fn fetch_box_art(id: &str) -> Option<(Vec<u8>, String)> {
    // Steam's `Artwork` fields are now relative proxy paths (see `steam_art`) the *client* resolves
    // against the host — meaningless to `fetch_image`, which expects an absolute URL. Resolve
    // those kinds directly instead of going through the URL fields.
    if let Some(appid) = id
        .strip_prefix("steam:")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return [
            ArtKind::Portrait,
            ArtKind::Header,
            ArtKind::Hero,
            ArtKind::Logo,
        ]
        .into_iter()
        .find_map(|kind| steam_art_bytes(appid, kind));
    }
    let g = all_games().into_iter().find(|g| g.id == id)?;
    [g.art.portrait, g.art.header, g.art.hero, g.art.logo]
        .into_iter()
        .flatten()
        .find_map(|url| resolve_art_bytes(&url))
}

/// Make a protocol-relative URL (`//host/...`, common in GOG + MS catalog responses) absolute https.
fn abs_url(u: &str) -> String {
    u.strip_prefix("//")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| u.to_string())
}

/// GOG cover art via the public (no-auth) product API. Field names / URL shapes are GOG-specific and
/// best-effort (worth on-box confirmation); a wrong URL just degrades to the title card client-side.
fn fetch_gog_art(product_id: &str) -> Artwork {
    let Some(v) = fetch_json(&format!(
        "https://api.gog.com/products/{product_id}?expand=images"
    )) else {
        return Artwork::default();
    };
    let img = |k: &str| {
        v.get("images")
            .and_then(|i| i.get(k))
            .and_then(|u| u.as_str())
            .map(abs_url)
    };
    Artwork {
        portrait: img("verticalCover"),
        hero: img("background"),
        logo: img("logo2x"),
        header: img("logo"),
    }
}

/// Xbox cover art via the (unofficial, no-auth) Microsoft display catalog, keyed by StoreId. Best-
/// effort: the endpoint is internal/unstable, so on drift this just yields no art (title-only).
fn fetch_xbox_art(store_id: &str) -> Artwork {
    let Some(v) = fetch_json(&format!(
        "https://displaycatalog.mp.microsoft.com/v7.0/products/{store_id}?market=US&languages=en-us&fieldsTemplate=Details"
    )) else {
        return Artwork::default();
    };
    let images = v
        .get("Products")
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("LocalizedProperties"))
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .and_then(|lp| lp.get("Images"))
        .and_then(|i| i.as_array());
    let mut art = Artwork::default();
    for img in images.into_iter().flatten() {
        let (Some(purpose), Some(uri)) = (
            img.get("ImagePurpose").and_then(|v| v.as_str()),
            img.get("Uri").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let url = abs_url(uri);
        match purpose {
            "Poster" => art.portrait = Some(url),
            "SuperHeroArt" | "Hero" => art.hero = Some(url),
            "Logo" => art.logo = Some(url),
            "BoxArt" => art.header = Some(url),
            _ => {}
        }
    }
    art
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_kind_parses_known_names_only() {
        assert_eq!(ArtKind::parse("portrait"), Some(ArtKind::Portrait));
        assert_eq!(ArtKind::parse("hero"), Some(ArtKind::Hero));
        assert_eq!(ArtKind::parse("logo"), Some(ArtKind::Logo));
        assert_eq!(ArtKind::parse("header"), Some(ArtKind::Header));
        assert_eq!(ArtKind::parse("background"), None);
    }

    #[test]
    fn fetch_image_decodes_data_url() {
        // "Hi" base64 == "SGk=" — the data: branch is pure (no network), so it's deterministic.
        let (bytes, ctype) = fetch_image("data:image/png;base64,SGk=").expect("data url decodes");
        assert_eq!(bytes, b"Hi");
        assert_eq!(ctype, "image/png");
        // A non-image scheme is rejected (no launcher art ever points at file://, but be defensive).
        assert!(fetch_image("file:///etc/passwd").is_none());
        // Empty payload → None (never serve a 0-byte cover).
        assert!(fetch_image("data:image/png;base64,").is_none());
    }

    #[test]
    fn local_art_path_detection() {
        // Windows-shaped local paths a provider (Playnite) would store.
        assert!(is_local_art_path(r"C:\Users\me\cover.jpg"));
        assert!(is_local_art_path("C:/Users/me/cover.png"));
        assert!(is_local_art_path(r"\\nas\share\art.jpg"));
        // URLs and the host proxy path are NOT local files.
        assert!(!is_local_art_path("https://cdn/x.jpg"));
        assert!(!is_local_art_path("http://host/x.jpg"));
        assert!(!is_local_art_path("data:image/png;base64,AAAA"));
        assert!(!is_local_art_path(
            "/api/v1/library/art/custom:abc/portrait"
        ));
    }

    #[test]
    fn proxy_local_art_rewrites_only_local_paths() {
        let mut art = Artwork {
            portrait: Some(r"C:\art\p.jpg".into()),
            hero: Some("https://cdn/h.jpg".into()),
            logo: None,
            header: Some("/api/v1/library/art/custom:x/header".into()),
        };
        proxy_local_art("custom:abc", &mut art);
        // The local path becomes a host proxy URL; the remote URL and an already-proxied path stay.
        assert_eq!(
            art.portrait.as_deref(),
            Some("/api/v1/library/art/custom:abc/portrait")
        );
        assert_eq!(art.hero.as_deref(), Some("https://cdn/h.jpg"));
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/custom:x/header")
        );
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];

    /// The art proxy reads bytes in the HOST process (LocalSystem on Windows) from a path the
    /// plugin lane can write — so what it will and will not read IS the security boundary
    /// (2026-08-05 review H-2). Confinement, extension, and content are all load-bearing.
    #[test]
    fn local_art_bytes_is_confined_and_image_only() {
        let dir = std::env::temp_dir().join(format!("pf-art-test-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Confine the proxy to `dir` for the duration of this test.
        std::env::set_var("PUNKTFUNK_LIBRARY_ART_ROOTS", &dir);

        // A real image inside the root: served, with the content type SNIFFED from the bytes.
        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();
        let (bytes, ctype) = local_art_bytes(cover.to_str().unwrap()).expect("reads a real cover");
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");

        // A secret is not served, however it is dressed up. This is the H-2 primitive: the plugin
        // writes the path, the host reads it as SYSTEM, and `mgmt-token` is full admin.
        let secret = dir.join("mgmt-token");
        std::fs::write(&secret, b"super-secret-admin-token").unwrap();
        assert!(
            local_art_bytes(secret.to_str().unwrap()).is_none(),
            "an extensionless secret must not be served as application/octet-stream"
        );
        let disguised = dir.join("mgmt-token.png");
        std::fs::write(&disguised, b"super-secret-admin-token").unwrap();
        assert!(
            local_art_bytes(disguised.to_str().unwrap()).is_none(),
            "an image extension must not be enough — the bytes must BE an image"
        );

        // Outside the configured root: refused even though it is a genuine image.
        let elsewhere = outside.join("cover.png");
        std::fs::write(&elsewhere, PNG).unwrap();
        assert!(
            local_art_bytes(elsewhere.to_str().unwrap()).is_none(),
            "a path outside every art root must be refused"
        );
        // …and a path that only *escapes* via traversal is caught, because we canonicalize first.
        let traversal = dir
            .join("..")
            .join(outside.file_name().unwrap())
            .join("cover.png");
        assert!(
            local_art_bytes(traversal.to_str().unwrap()).is_none(),
            "`..` out of the root must be refused after canonicalization"
        );

        assert!(local_art_bytes(dir.join("nope.png").to_str().unwrap()).is_none());
        // A UNC path is refused outright (outbound SMB auth coercion), before any filesystem hit.
        assert!(!art_path_is_servable(r"\\attacker\share\a.png"));

        std::env::remove_var("PUNKTFUNK_LIBRARY_ART_ROOTS");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Write-time validation refuses what read-time would refuse, so an unservable path never even
    /// reaches `library.json`. URLs are none of its business.
    #[test]
    fn validate_art_paths_rejects_unservable_local_paths() {
        let ok = Artwork {
            portrait: Some("https://cdn/x.jpg".into()),
            hero: Some("data:image/png;base64,AAAA".into()),
            logo: Some("/api/v1/library/art/custom:x/logo".into()),
            header: None,
        };
        assert!(validate_art_paths(&ok).is_ok(), "URLs pass through");

        let unc = Artwork {
            portrait: Some(r"\\attacker\share\a.png".into()),
            ..Default::default()
        };
        assert!(
            validate_art_paths(&unc).is_err(),
            "UNC is refused at write time"
        );

        let secret = Artwork {
            hero: Some(r"C:\ProgramData\punktfunk\mgmt-token".into()),
            ..Default::default()
        };
        let err = validate_art_paths(&secret).expect_err("a secret path is refused");
        assert!(
            err.starts_with("art.hero"),
            "the error names the field: {err}"
        );
    }

    #[test]
    fn sniff_image_type_recognizes_containers_and_rejects_secrets() {
        assert_eq!(sniff_image_type(PNG), Some("image/png"));
        assert_eq!(
            sniff_image_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(
            sniff_image_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_type(b"BM\0\0"), Some("image/bmp"));
        // The shapes a stolen secret actually has.
        assert_eq!(sniff_image_type(b"-----BEGIN PRIVATE KEY-----"), None);
        assert_eq!(sniff_image_type(b"9f8a7b6c5d4e3f2a1b0c"), None);
        assert_eq!(sniff_image_type(b""), None);
    }
}
