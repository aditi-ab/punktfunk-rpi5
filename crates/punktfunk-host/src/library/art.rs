//! Artwork serving: the local-file confinement rules, the art-proxy rewrite, and the
//! `fetch_box_art` dispatch the management art proxy serves from. Split out of the `library` facade (plan §W5).
//!
//! There is no art *cache* or background *warmer* here any more. Both existed for the built-in GOG
//! and Xbox scanners, the only two sources that had to reach a network catalog to learn what a
//! title's cover was; every other source carried its own art. Those scanners were removed in
//! v0.28.0, and the library plugins that replaced them resolve art while they scan and publish it on
//! the entry — so the host now only ever *serves* art it was handed, and never fetches any on its
//! own schedule. A stale `library-art-cache.json` left by an older host is simply ignored.

use super::*;

/// Fetch one image URL for the GameStream `/appasset` cover proxy, as `(bytes, content-type)`. Handles
/// `data:` URLs (Lutris inlines art that way) by decoding inline, and `http(s)` URLs by a bounded GET
/// (8 MiB cap so a hostile/huge art URL can't balloon host memory). `None` on any non-image scheme,
/// network/decoder error, or empty body. Blocking (ureq) — call off the async runtime.
pub(crate) fn fetch_image(url: &str) -> Option<(Vec<u8>, String)> {
    use base64::Engine as _;
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
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        // Don't follow redirects (SSRF pivot): this is called on launcher-cache- and custom-entry-
        // supplied URLs, so a `3xx` to an internal/metadata endpoint must not be chased by the
        // privileged host. Matches the webhook path (security-review 2026-07-17).
        .max_redirects(0)
        .build()
        .into();
    let mut resp = agent.get(url).call().ok()?;
    let ctype = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    // The 8 MiB cap is now the body reader's own limit rather than a `take()` on the stream.
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    (!bytes.is_empty()).then_some((bytes, ctype))
}

/// A stored [`Artwork`] value that is a **local filesystem path** to an image on the host — as
/// opposed to an `http(s)`/`data:` URL or an already-relative host proxy path. Provider plugins that
/// run on the host (the Playnite sync plugin, and every library scanner plugin) set these: the
/// reconcile payload stays tiny (paths, not inlined bytes, so it scales to thousands of titles) and
/// the host serves the bytes through the art proxy, exactly like Steam's cache art.
///
/// Four accepted shapes:
/// * `file://…` — the **documented plugin contract** ([`file_url_to_path`]), unambiguous on every
///   platform, and what `@punktfunk/plugin-kit/library` emits.
/// * `C:\…` / `C:/…` drive-absolute and `\\server\share` UNC — Windows bare paths, kept for
///   Playnite back-compat (it predates the `file://` contract).
/// * POSIX absolute (`/home/u/covers/x.jpg`) — Lutris covers and Steam's `librarycache`.
///
/// The POSIX widening is why the two `/`-leading shapes must be excluded explicitly: the host's own
/// art-proxy path (`/api/v1/library/art/…`, which [`proxy_local_art`] writes and which must survive a
/// second pass unchanged) and a protocol-relative URL (`//cdn/…`, which GOG's and Microsoft's
/// catalogs return and a plugin may pass straight through). Mistaking either for a file would break
/// the proxy round-trip or silently drop CDN art.
pub fn is_local_art_path(v: &str) -> bool {
    if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("data:") {
        return false;
    }
    if v.starts_with("file://") {
        return true;
    }
    let b = v.as_bytes();
    // Windows drive-absolute (`C:\…`, `C:/…`) or UNC (`\\server\share`).
    if (b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || v.starts_with("\\\\") {
        return true;
    }
    // POSIX absolute, minus the host's own `/`-leading shapes (see the doc comment).
    v.starts_with('/') && !v.starts_with("//") && !v.starts_with("/api/")
}

/// Turn a `file://` art value into a plain filesystem path, percent-decoding it. The kit emits
/// properly encoded URLs (`file:///home/u/My%20Cover.jpg`); a raw path that happens to contain no
/// `%` round-trips either way, which keeps hand-written plugin payloads working.
///
/// `file:///home/u/c.jpg` → `/home/u/c.jpg`; `file:///C:/covers/c.jpg` → `C:/covers/c.jpg` (Windows
/// drive letters arrive after the empty authority's slash); a NON-empty authority
/// (`file://nas/share/c.jpg`) is a UNC reference → `\\nas\share\c.jpg`. Anything without the prefix
/// is returned untouched.
fn file_url_to_path(v: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let Some(rest) = v.strip_prefix("file://") else {
        return Cow::Borrowed(v);
    };
    let decoded = percent_decode(rest);
    match decoded.strip_prefix('/') {
        // `file:///…` — the empty-authority form. A Windows drive letter (`/C:/…`) loses the slash;
        // a POSIX path keeps it.
        Some(after) if after.as_bytes().get(1) == Some(&b':') => Cow::Owned(after.to_string()),
        Some(_) => Cow::Owned(decoded),
        // `file://server/share/…` — a UNC path in URL clothing.
        None => Cow::Owned(format!("\\\\{}", decoded.replace('/', "\\"))),
    }
}

/// Percent-decode `%XX` escapes. Invalid escapes are left verbatim (a bare `%` in a real path is far
/// likelier than a malformed URL from our own kit), and the result is only ever used as a path that
/// must then exist as a regular file — so a wrong decode degrades to "no art", never to a wrong read.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// The filesystem roots the art proxy is allowed to read from.
///
/// The proxy runs in the **host process** — LocalSystem on Windows — and both the path and the
/// read-back are reachable from the plugin lane, which runs as the much weaker LocalService. Without
/// a root, "serve this entry's cover" is "read any file on the box as SYSTEM" (2026-08-05 review
/// H-2): `mgmt-token`, `key.pem`, the SAM hive. So the value is confined here, at the one place
/// bytes are read, rather than trusted because of where it was written.
///
/// Default: the users base (`C:\Users`), where the launchers that install per-user keep their art —
/// Playnite stores covers under `%APPDATA%\Playnite`, Heroic under `%APPDATA%\heroic`. Derived from
/// `%PUBLIC%`'s parent because the host runs as SYSTEM, whose own `%USERPROFILE%` is
/// `…\config\systemprofile` and tells us nothing about where the operator's launchers live. Plus
/// the Steam install root ([`steam_art_roots`]), which is the one launcher that does NOT live under
/// the users base. `PUNKTFUNK_LIBRARY_ART_ROOTS` (`;`-separated) replaces the whole default for an
/// operator whose library is somewhere else again.
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
    #[cfg(windows)]
    roots.extend(steam_art_roots());
    // POSIX: the user's home, which is the exact analogue of the Windows users base above — and
    // where every launcher this host reads art from actually keeps it. Steam's
    // `appcache/librarycache` and `userdata/<id>/config/grid`, Lutris's `coverart`/`banners` (both
    // the `~/.local/share` and `~/.cache` copies), Heroic's caches, and all three Flatpak
    // `~/.var/app/…` variants are under it.
    //
    // Needed because `is_local_art_path` now classifies POSIX absolute paths as local art (the
    // extracted Lutris/Steam plugins emit them). Before that widening this list was legitimately
    // empty here: the only local-art provider was Playnite, which is Windows-only, so nothing on a
    // POSIX host was ever classified local and the confinement had nothing to confine. Leaving it
    // empty now would not be "secure by default" — it would silently serve no plugin art at all.
    //
    // Breadth matches what Windows already ships, and it is not the load-bearing control: a value
    // still has to carry an image extension, canonicalize to a real regular file inside a root,
    // sit outside the host config dir, and CONTAIN image bytes. `PUNKTFUNK_LIBRARY_ART_ROOTS`
    // narrows or relocates this for a library that lives elsewhere.
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            roots.push(home);
        }
    }
    roots
}

/// Windows: every Steam install root that exists on this box.
///
/// Steam is the one launcher whose art is NOT under the users base: it installs to
/// `C:\Program Files (x86)\Steam`, and both places the `steam` library plugin publishes covers from
/// — `appcache\librarycache\<appid>\…` and each account's `userdata\<id>\config\grid\` overrides —
/// live under that root. Without this the users base rejected every one of them, and because an
/// unservable path used to fail the WHOLE reconcile payload the plugin synced NO GAMES AT ALL, not
/// merely no art. That is a v0.28.0 regression: the built-in scanner this plugin replaced served its
/// covers through the legacy `steam:` art-proxy branch, which never passed through this confinement.
/// (POSIX needs no equivalent — every Steam layout there, native and Flatpak, is already under
/// `$HOME`.)
///
/// This does not widen what the host can be *tricked* into reading. The confinement exists to close
/// one asymmetry: the host reads as SYSTEM, while the plugin lane that supplies the path is the far
/// weaker LocalService (2026-08-05 review H-2). The Steam directory is readable by LocalService
/// already, so nothing reachable through it is reachable *because* the host is privileged. The
/// extension, regular-file, magic-byte and config-dir gates all still apply on top, so Steam's own
/// `config.vdf` and `ssfn*` credential blobs are not servable from it either.
#[cfg(windows)]
fn steam_art_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        // `is_dir` before dedup: `%ProgramFiles%` and `%ProgramW6432%` are the same directory on a
        // 64-bit host, and the registry commonly repeats whichever of the two Steam sits in.
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            push(PathBuf::from(pf).join("Steam"));
        }
    }
    // A Steam installed off the default path — a second drive is common — is only discoverable from
    // the registry. HKLM and not HKCU, for the same reason the plugin reads HKLM: the host is
    // SYSTEM, whose own hive knows nothing about where the operator installed anything.
    for key in [r"SOFTWARE\WOW6432Node\Valve\Steam", r"SOFTWARE\Valve\Steam"] {
        if let Some(p) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(key)
            .ok()
            .and_then(|k| k.get_value::<String, _>("InstallPath").ok())
        {
            push(PathBuf::from(p));
        }
    }
    out
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
///
/// A `file://` value is decoded to a plain path FIRST, exactly as [`local_art_bytes`] does. Both
/// halves of the confinement must judge the *same* string or they disagree: `Path::new` on a raw
/// `file:///home/u/c.jpg` yields a RELATIVE path whose first component is `file:`, which
/// canonicalizes against the cwd, fails, and reads as "outside every root". That is not a
/// conservative failure — it rejected every `file://` cover the plugin kit emits (`fileUrl`, the
/// documented way for a library plugin to publish local art), so the Lutris and Steam scanners
/// could not reconcile a single entry while the read path would have served those same files
/// happily.
pub fn art_path_is_servable(value: &str) -> bool {
    // Idempotent for the already-decoded caller: the decoded form no longer carries the prefix,
    // so `local_art_bytes` passing its own output back through here is a no-op, not a second
    // percent-decode of a path that legitimately contains `%`.
    let value = file_url_to_path(value);
    let p = Path::new(&*value);
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

/// Strip every **local-file** art value the proxy would refuse to serve, returning the
/// `(field, value)` pairs dropped. URLs and already-proxied paths are left alone.
///
/// The provider-reconcile counterpart to [`validate_art_paths`]. Both enforce the same invariant —
/// an unservable path never reaches `library.json` — and differ only on what the REST of the payload
/// is worth. An operator writing one custom entry typed that path by hand, so a hard 400 is the
/// feedback they need. A plugin reconciling its whole entry set did not: it publishes hundreds of
/// covers it resolved from disk, and refusing the payload over one of them costs the operator their
/// entire library for that store.
///
/// That is not hypothetical. A default Windows Steam install put every cover outside the art roots,
/// so `PUT /library/provider/steam` 400'd, the plugin could only report `HostRequestError`, and the
/// grid stayed empty with no indication that the games themselves were fine. [`steam_art_roots`]
/// fixes that specific mismatch; this makes the NEXT one cost a cover instead of a library.
///
/// Dropping rather than rewriting is deliberate: `None` is exactly what an entry with no art
/// carries, and every client already renders that.
pub fn sanitize_art_paths(art: &mut Artwork) -> Vec<(&'static str, String)> {
    let mut dropped = Vec::new();
    for (field, value) in [
        ("portrait", &mut art.portrait),
        ("hero", &mut art.hero),
        ("logo", &mut art.logo),
        ("header", &mut art.header),
    ] {
        let unservable = value
            .as_deref()
            .is_some_and(|v| is_local_art_path(v) && !art_path_is_servable(v));
        if unservable {
            if let Some(v) = value.take() {
                dropped.push((field, v));
            }
        }
    }
    dropped
}

/// Read a local image file into `(bytes, content-type)` for the art proxy. `None` if it isn't an
/// existing regular file, is empty, exceeds 16 MiB (a cover never approaches that; the cap bounds
/// host memory), resolves outside the allowed art roots ([`art_path_is_confined`]), or does not
/// actually contain an image ([`sniff_image_type`]).
///
/// This is the single place local art bytes are read — the mgmt art proxy and the GameStream
/// `/appasset` proxy both land here — so the confinement holds for every caller.
///
/// A `file://` value is converted to a path FIRST ([`file_url_to_path`]), so the confinement check
/// and the read see the same decoded path. Ordering matters: percent-decoding before
/// canonicalization is what stops a `%2e%2e` escape being invisible to the traversal check.
pub fn local_art_bytes(path: &str) -> Option<(Vec<u8>, String)> {
    let path = file_url_to_path(path);
    if !art_path_is_servable(&path) {
        tracing::debug!(
            path = %path,
            "art proxy: refusing a path outside the allowed art roots"
        );
        return None;
    }
    let p = std::path::Path::new(&*path);
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
    // Same resolution as the management art proxy (WP1.2): the stored catalog, for ANY id, so a
    // library plugin's entries resolve without this ever knowing which store they came from. That
    // used to be the first of three branches — the other two served the built-in scanners (a
    // `steam:` id whose art was a relative proxy path, and the CDN-URL scanners) and went with them.
    let entry = entry_for_library_id(id)?;
    [
        ArtKind::Portrait,
        ArtKind::Header,
        ArtKind::Hero,
        ArtKind::Logo,
    ]
    .into_iter()
    .filter_map(|kind| art_field(&entry.art, kind))
    .find_map(|v| resolve_art_bytes(&v))
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

    /// The full accept/exclude table (WP1.2). The exclusions are the load-bearing half: two of the
    /// three `/`-leading shapes here are emitted by the host ITSELF, so a POSIX rule that swallowed
    /// them would break the proxy round-trip and silently drop CDN art.
    #[test]
    fn local_art_path_detection() {
        // Windows-shaped local paths a provider (Playnite) would store.
        assert!(is_local_art_path(r"C:\Users\me\cover.jpg"));
        assert!(is_local_art_path("C:/Users/me/cover.png"));
        assert!(is_local_art_path(r"\\nas\share\art.jpg"));
        // The `file://` plugin contract, on both platform shapes.
        assert!(is_local_art_path("file:///home/u/covers/x.jpg"));
        assert!(is_local_art_path("file:///C:/covers/x.jpg"));
        // POSIX absolute — lutris covers, steam librarycache.
        assert!(is_local_art_path("/home/u/.cache/lutris/coverart/x.jpg"));
        assert!(is_local_art_path("/var/lib/steam/librarycache/570/h.jpg"));
        // URLs are NOT local files.
        assert!(!is_local_art_path("https://cdn/x.jpg"));
        assert!(!is_local_art_path("http://host/x.jpg"));
        assert!(!is_local_art_path("data:image/png;base64,AAAA"));
        // …nor is the host's OWN art-proxy path (it must survive a second `proxy_local_art` pass).
        assert!(!is_local_art_path(
            "/api/v1/library/art/custom:abc/portrait"
        ));
        assert!(!is_local_art_path("/api/v1/library/art/steam:570/hero"));
        // …nor a protocol-relative CDN URL (what GOG / the MS catalog return).
        assert!(!is_local_art_path("//images.gog.com/abc_vertical.jpg"));
        // A relative path is not absolute — nothing to serve.
        assert!(!is_local_art_path("covers/x.jpg"));
        assert!(!is_local_art_path(""));
    }

    #[test]
    fn file_url_converts_to_a_path_and_percent_decodes() {
        assert_eq!(file_url_to_path("file:///home/u/c.jpg"), "/home/u/c.jpg");
        // Percent-encoded spaces — what a correct URL encoder emits for a real-world cover path.
        assert_eq!(
            file_url_to_path("file:///home/u/My%20Games/c%2Bx.jpg"),
            "/home/u/My Games/c+x.jpg"
        );
        // Windows drive letters arrive after the empty authority's slash and lose it.
        assert_eq!(
            file_url_to_path("file:///C:/covers/c.jpg"),
            "C:/covers/c.jpg"
        );
        // A non-empty authority is a UNC reference.
        assert_eq!(
            file_url_to_path("file://nas/share/c.jpg"),
            r"\\nas\share\c.jpg"
        );
        // Non-`file://` values are returned untouched (bare paths still work).
        assert_eq!(file_url_to_path("/home/u/c.jpg"), "/home/u/c.jpg");
        assert_eq!(file_url_to_path(r"C:\c.jpg"), r"C:\c.jpg");
        // A lone `%` (a legal path character) is not mangled into a decode failure.
        assert_eq!(file_url_to_path("file:///home/100%.jpg"), "/home/100%.jpg");
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

    /// A POSIX local cover — the shape the lutris and steam plugins emit — is classified as local
    /// art and rewritten to the proxy path. This is the case G4 blocked (Lutris art was inlined as
    /// `data:` URLs and blew the 2 MB body limit at 49 covers).
    ///
    /// Deliberately free of filesystem and env: the READ half is confined, and lives in
    /// `local_art_bytes_is_confined_and_image_only` so that only ONE test mutates
    /// `PUNKTFUNK_LIBRARY_ART_ROOTS` (cargo runs these in parallel threads of one process, so two
    /// would race).
    #[test]
    fn posix_local_art_is_classified_and_proxied() {
        let path = if cfg!(windows) {
            r"C:\covers\cover.jpg".to_string()
        } else {
            "/home/u/.cache/lutris/coverart/cover.jpg".to_string()
        };
        let url = file_url(std::path::Path::new(&path));
        let mut art = Artwork {
            portrait: Some(path.clone()),
            hero: Some(url),
            logo: Some("https://cdn/l.png".into()),
            header: None,
        };
        assert!(is_local_art_path(&path));
        proxy_local_art("lutris:42", &mut art);
        assert_eq!(
            art.portrait.as_deref(),
            Some("/api/v1/library/art/lutris:42/portrait")
        );
        assert_eq!(
            art.hero.as_deref(),
            Some("/api/v1/library/art/lutris:42/hero"),
            "a file:// value is local art too"
        );
        assert_eq!(art.logo.as_deref(), Some("https://cdn/l.png"));

        // Re-running the rewrite is a no-op — the emitted proxy path must not be mistaken for a file.
        let before = art.portrait.clone();
        proxy_local_art("lutris:42", &mut art);
        assert_eq!(art.portrait, before);
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];

    /// `PUNKTFUNK_LIBRARY_ART_ROOTS` is process-global while cargo runs tests as threads, so the
    /// tests that repoint it must not overlap — one clearing the variable mid-flight makes the
    /// other's temp root stop being a root, which fails as a confinement bug that isn't there.
    /// Poisoning is recovered rather than propagated: a panic in one test should report ITS
    /// failure, not cascade into an unrelated `PoisonError`.
    static ART_ROOTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_art_roots() -> std::sync::MutexGuard<'static, ()> {
        ART_ROOTS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The art proxy reads bytes in the HOST process (LocalSystem on Windows) from a path the
    /// plugin lane can write — so what it will and will not read IS the security boundary
    /// (2026-08-05 review H-2). Confinement, extension, and content are all load-bearing.
    #[test]
    fn local_art_bytes_is_confined_and_image_only() {
        let _guard = lock_art_roots();
        let dir = std::env::temp_dir().join(format!("pf-art-test-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Confine the proxy to `dir` for the duration of this test.
        // SAFETY: `_guard` holds ART_ROOTS_LOCK (`lock_art_roots`), which serializes every test
        // that writes or reads this variable in the binary.
        unsafe { std::env::set_var("PUNKTFUNK_LIBRARY_ART_ROOTS", &dir) };

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
        // A directory is not a servable cover — the proxy must never become a directory reader.
        assert!(local_art_bytes(dir.to_str().unwrap()).is_none());

        // The `file://` plugin contract reaches the SAME bytes through the SAME gate. This is the
        // half that matters for the extracted scanners: they emit `file://` values, so if the
        // conversion happened after the confinement check the check would be inspecting a string
        // that is not the path being read.
        let as_url = file_url(&cover);
        assert_eq!(
            local_art_bytes(&as_url)
                .expect("file:// reads the same cover")
                .0,
            PNG
        );
        // …and a `file://` value is confined exactly like a bare one — no bypass by spelling.
        assert!(
            local_art_bytes(&file_url(&elsewhere)).is_none(),
            "file:// must not escape the art roots"
        );
        // Percent-encoded traversal is decoded BEFORE canonicalization, so it cannot hide from the
        // `..` check.
        assert!(
            local_art_bytes(&format!(
                "{}/%2e%2e/{}/cover.png",
                file_url(&dir),
                outside.file_name().unwrap().to_str().unwrap()
            ))
            .is_none(),
            "percent-encoded traversal must be refused"
        );

        // A UNC path is refused outright (outbound SMB auth coercion), before any filesystem hit.
        assert!(!art_path_is_servable(r"\\attacker\share\a.png"));

        // SAFETY: still under `_guard` — the same ART_ROOTS_LOCK serialization as the set.
        unsafe { std::env::remove_var("PUNKTFUNK_LIBRARY_ART_ROOTS") };
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Build a `file://` value the way the kit's `fileUrl` does, so these tests exercise the real
    /// plugin contract on both platforms. A POSIX path keeps the two-slash form
    /// (`file:///home/u/c.png` — empty authority, then the leading `/`); a Windows path becomes
    /// `file:///C:/covers/c.png`, i.e. three slashes and forward separators. Building it as
    /// `format!("file://{path}")` on Windows yields `file://C:\covers\c.png`, whose authority is
    /// `C:` — that is a UNC reference, not a local file, and the parser is right to refuse it.
    fn file_url(p: &std::path::Path) -> String {
        let posix = p.to_str().unwrap().replace('\\', "/");
        if posix.starts_with('/') {
            format!("file://{posix}")
        } else {
            format!("file:///{posix}")
        }
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

    /// The write gate and the read gate must judge the SAME string.
    ///
    /// Regression for 2026-08-08: `validate_art_paths` handed the raw value to `Path::new`, so a
    /// `file:///…` cover became a *relative* path starting with a `file:` component, canonicalized
    /// against the cwd, failed, and was refused as "outside every art root" — while
    /// `local_art_bytes` decoded the very same value and served the file. Every Lutris and Steam
    /// entry carrying local art was rejected with a 400 the plugin could only report as
    /// `HostRequestError`, so neither scanner could sync a single game. Asserting servable and
    /// readable together is the point: either alone passes with the bug present.
    #[test]
    fn file_url_art_is_accepted_at_write_time_exactly_as_at_read_time() {
        let _guard = lock_art_roots();
        let dir = std::env::temp_dir().join(format!("pf-art-wr-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-wr-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // SAFETY: `_guard` holds ART_ROOTS_LOCK (`lock_art_roots`), which serializes every test
        // that writes or reads this variable in the binary.
        unsafe { std::env::set_var("PUNKTFUNK_LIBRARY_ART_ROOTS", &dir) };

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();

        // What the kit's `fileUrl` actually emits for a Lutris/Steam cover.
        let url = file_url(&cover);
        assert!(
            is_local_art_path(&url),
            "a file:// value is local art, so the confinement applies to it"
        );
        assert!(
            art_path_is_servable(&url),
            "write time must accept the file:// form of a servable cover"
        );
        assert!(
            validate_art_paths(&Artwork {
                portrait: Some(url.clone()),
                header: Some(url),
                ..Default::default()
            })
            .is_ok(),
            "a real Lutris-shaped payload must reconcile"
        );

        // A percent-encoded name (the reason the decode exists at all) survives the round trip.
        let spaced = dir.join("My Cover.png");
        std::fs::write(&spaced, PNG).unwrap();
        let spaced_url = file_url(&spaced).replace(' ', "%20");
        assert!(
            art_path_is_servable(&spaced_url),
            "percent-encoded names must decode before the containment test: {spaced_url}"
        );
        assert!(local_art_bytes(&spaced_url).is_some(), "read time agrees");

        // Loosening the write gate must not loosen the confinement: outside the root is still
        // refused in file:// clothing, which is what the raw-string bug was accidentally doing.
        let elsewhere = outside.join("cover.png");
        std::fs::write(&elsewhere, PNG).unwrap();
        assert!(
            !art_path_is_servable(&file_url(&elsewhere)),
            "file:// must not escape the art roots at write time either"
        );
        assert!(
            validate_art_paths(&Artwork {
                portrait: Some(file_url(&elsewhere)),
                ..Default::default()
            })
            .is_err(),
            "an out-of-root file:// cover is still refused"
        );

        // SAFETY: still under `_guard` — the same ART_ROOTS_LOCK serialization as the set.
        unsafe { std::env::remove_var("PUNKTFUNK_LIBRARY_ART_ROOTS") };
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A reconcile keeps its entries when a cover is unservable — it drops the cover.
    ///
    /// Regression for the report that opened this: on a default Windows Steam install every
    /// `appcache\librarycache` path fell outside the users base, `validate_art_paths` refused the
    /// whole `PUT /library/provider/steam` payload, and the operator's grid stayed EMPTY. The games
    /// were never the problem. Asserting the survivors matters as much as the drop: a sanitizer that
    /// cleared the whole struct would also "pass" a drop-only test.
    #[test]
    fn sanitize_drops_only_the_unservable_local_art() {
        let _guard = lock_art_roots();
        let dir = std::env::temp_dir().join(format!("pf-art-san-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: `_guard` holds ART_ROOTS_LOCK (`lock_art_roots`), which serializes every test
        // that writes or reads this variable in the binary.
        unsafe { std::env::set_var("PUNKTFUNK_LIBRARY_ART_ROOTS", &dir) };

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();
        let cover_url = file_url(&cover);
        let outside = if cfg!(windows) {
            r"C:\Program Files (x86)\Steam\appcache\librarycache\570\a\library_hero.jpg".to_string()
        } else {
            "/opt/steam/appcache/librarycache/570/a/library_hero.jpg".to_string()
        };

        let mut art = Artwork {
            portrait: Some(cover_url.clone()),
            hero: Some(outside.clone()),
            logo: Some("https://cdn/l.png".into()),
            header: Some("/api/v1/library/art/steam:570/header".into()),
        };
        let dropped = sanitize_art_paths(&mut art);
        assert_eq!(
            dropped,
            vec![("hero", outside)],
            "only the out-of-root local path is dropped, and it is reported"
        );
        assert!(art.hero.is_none(), "the unservable value is gone, not kept");
        // A servable local cover, a remote URL and an already-proxied path all survive untouched —
        // the entry still renders everything it legitimately can.
        assert_eq!(art.portrait.as_deref(), Some(cover_url.as_str()));
        assert_eq!(art.logo.as_deref(), Some("https://cdn/l.png"));
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/steam:570/header")
        );
        // Idempotent: what survived one pass survives the next, and nothing new is reported.
        assert!(sanitize_art_paths(&mut art).is_empty());

        // The invariant the hard 400 used to hold is still held — nothing the write gate would
        // refuse comes out the other side.
        assert!(validate_art_paths(&art).is_ok());

        // SAFETY: still under `_guard` — the same ART_ROOTS_LOCK serialization as the set.
        unsafe { std::env::remove_var("PUNKTFUNK_LIBRARY_ART_ROOTS") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows only, and the actual bug report: a Steam cover under Program Files is servable with
    /// NO `PUNKTFUNK_LIBRARY_ART_ROOTS` set.
    ///
    /// Drives the whole chain the `steam` plugin's payload traverses — Program Files probe →
    /// [`steam_art_roots`] → [`art_roots`] → confinement → [`art_path_is_servable`] →
    /// [`local_art_bytes`] — against a synthetic Steam tree, by repointing `%ProgramFiles(x86)%` at
    /// a temp dir. Hermetic on purpose: asserting over whatever Steam this box happens to have would
    /// pass vacuously on every CI runner, which is exactly the shape of test that let this ship.
    #[cfg(windows)]
    #[test]
    fn steam_librarycache_cover_is_servable_without_configuration() {
        let _guard = lock_art_roots();
        let base = std::env::temp_dir().join(format!("pf-art-steam-{}", std::process::id()));
        // `appcache\librarycache\<appid>\<hash>\library_hero.jpg` — the exact shape the plugin
        // publishes, and the exact field the reported failure named.
        let hero = base
            .join("Steam")
            .join("appcache")
            .join("librarycache")
            .join("570")
            .join("abcdef")
            .join("library_hero.jpg");
        std::fs::create_dir_all(hero.parent().unwrap()).unwrap();
        std::fs::write(&hero, PNG).unwrap();

        // Restored at the end: `%ProgramFiles(x86)%` is a real variable on this box that later
        // tests in the same process may legitimately read.
        let saved = std::env::var_os("ProgramFiles(x86)");
        // SAFETY: `_guard` holds ART_ROOTS_LOCK, which serializes every test that reads or writes
        // the variables the art roots are derived from.
        unsafe {
            std::env::remove_var("PUNKTFUNK_LIBRARY_ART_ROOTS");
            std::env::set_var("ProgramFiles(x86)", &base);
        }

        let steam_root = base.join("Steam");
        assert!(
            steam_art_roots().contains(&steam_root),
            "the Program Files probe must find the Steam install"
        );
        assert!(
            art_roots().contains(&steam_root),
            "the DEFAULT art roots must include it — the whole point is that no env var is needed"
        );

        // The plugin sends `file://`, so that is what has to be accepted; before the fix this was
        // false and `validate_art_paths` 400'd the entire reconcile.
        let url = file_url(&hero);
        assert!(art_path_is_servable(&url), "{url} must be servable");
        assert!(
            validate_art_paths(&Artwork {
                hero: Some(url.clone()),
                ..Default::default()
            })
            .is_ok(),
            "a Steam-shaped payload must reconcile"
        );
        assert!(
            sanitize_art_paths(&mut Artwork {
                hero: Some(url.clone()),
                ..Default::default()
            })
            .is_empty(),
            "and nothing about it is dropped"
        );
        assert_eq!(
            local_art_bytes(&url).expect("read time serves it too").0,
            PNG
        );

        // The confinement did not go slack on the way: a secret next door is still not servable,
        // and neither is a non-image that merely wears the extension.
        let secret = base.join("Steam").join("config").join("config.vdf");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, b"\"Accounts\"\n{\n\"user\" \"token\"\n}\n").unwrap();
        assert!(
            local_art_bytes(secret.to_str().unwrap()).is_none(),
            "Steam's own credential blob must not be servable from an art root"
        );
        let disguised = base.join("Steam").join("config.png");
        std::fs::write(&disguised, b"\"Accounts\" { \"user\" \"token\" }").unwrap();
        assert!(
            local_art_bytes(disguised.to_str().unwrap()).is_none(),
            "an image extension is still not enough — the bytes must BE an image"
        );

        // SAFETY: still under `_guard` — same serialization as the set above.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("ProgramFiles(x86)", v),
                None => std::env::remove_var("ProgramFiles(x86)"),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
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
