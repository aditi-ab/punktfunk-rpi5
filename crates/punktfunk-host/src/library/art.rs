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
        .find_map(|url| fetch_image(&url))
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
}
