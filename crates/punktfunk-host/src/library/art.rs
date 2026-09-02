//! Artwork serving: local-file confinement, the art-proxy rewrite, and GameStream `/appasset` fetch.
//!
//! The host serves art a plugin already published on the entry. It never fetches covers on its own
//! schedule. A local path is rewritten to `/api/v1/library/art/<id>/<kind>` on `GET /library`; the
//! proxy then reads those bytes from an allowed root. `http(s)`/`data:` URLs pass through.
//!
//! Confinement is load-bearing: the proxy runs in the host process and the plugin lane supplies the
//! path. `PUNKTFUNK_LIBRARY_ART_ROOTS` replaces the default roots. A stale `library-art-cache.json`
//! from an older host is ignored.

use super::*;

/// Fetch one cover URL as `(bytes, content-type)`. `data:` is decoded inline (Lutris inlines art);
/// `http(s)` is a GET with an 8 MiB cap so a huge URL cannot balloon host memory. `None` on any
/// other scheme, error, or empty body. Blocking (`ureq`) — call off the async runtime.
pub(crate) fn fetch_image(url: &str) -> Option<(Vec<u8>, String)> {
    use base64::Engine as _;
    if let Some(rest) = url.strip_prefix("data:") {
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
        // Do not follow redirects. These URLs come from launcher caches and custom entries; a
        // `3xx` to an internal endpoint must not be chased by the privileged host.
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
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    (!bytes.is_empty()).then_some((bytes, ctype))
}

/// Accepted: `file://…` (the plugin-kit contract, [`file_url_to_path`]), Windows drive-absolute
/// and UNC, and POSIX absolute. Two `/`-leading shapes are excluded because POSIX absolute would
/// otherwise swallow them: the host's own `/api/v1/library/art/…` (must survive a second
/// [`proxy_local_art`] pass) and a protocol-relative URL (`//cdn/…`).
pub fn is_local_art_path(v: &str) -> bool {
    if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("data:") {
        return false;
    }
    if v.starts_with("file://") {
        return true;
    }
    let b = v.as_bytes();
    if (b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || v.starts_with("\\\\") {
        return true;
    }
    v.starts_with('/') && !v.starts_with("//") && !v.starts_with("/api/")
}

/// Decode a `file://` art value to a filesystem path. The kit emits percent-encoded URLs;
/// a raw path with no `%` round-trips either way.
///
/// Empty authority: `file:///home/u/c.jpg` → `/home/u/c.jpg`, and `file:///C:/covers/c.jpg` →
/// `C:/covers/c.jpg` (the drive letter arrives after the extra slash). Non-empty authority is
/// UNC: `file://nas/share/c.jpg` → `\\nas\share\c.jpg`. Anything else is returned untouched.
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
        // `file://server/share/…` — a UNC path spelled as a URL.
        None => Cow::Owned(format!("\\\\{}", decoded.replace('/', "\\"))),
    }
}

/// Percent-decode `%XX`. Invalid escapes stay verbatim — a bare `%` in a real path is likelier
/// than a malformed kit URL — and a wrong decode then fails the regular-file check, never a
/// wrong read.
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

/// Filesystem roots the art proxy may read from.
///
/// The proxy runs in the host process (LocalSystem on Windows) and the plugin lane supplies the
/// path, so a missing root would make "serve this cover" equal "read any file as SYSTEM". Default
/// is the users base — `%PUBLIC%`'s parent, because SYSTEM's `%USERPROFILE%` is
/// `…\config\systemprofile` and is not where launchers live — plus [`steam_art_roots`] and
/// [`super::launch::playnite_art_roots`] (a portable Playnite keeps covers beside the exe).
/// `PUNKTFUNK_LIBRARY_ART_ROOTS` (`;`-separated) replaces the whole default.
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
    // Portable Playnite keeps `library\files\…` beside the exe, outside every profile.
    #[cfg(windows)]
    roots.extend(super::launch::playnite_art_roots());
    // `$HOME` is the POSIX analogue of the Windows users base. An empty list here would
    // silently serve no plugin art: POSIX absolute paths are classified as local.
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            roots.push(home);
        }
    }
    roots
}

/// Windows: Steam install roots. Steam art lives under the install, not the users base
/// (`appcache\librarycache\…` and `userdata\<id>\config\grid\`). POSIX needs no equivalent —
/// native and Flatpak layouts are already under `$HOME`.
#[cfg(windows)]
fn steam_art_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        // `is_dir` before dedup: `%ProgramFiles%` and `%ProgramW6432%` are the same directory
        // on a 64-bit host, and the registry often repeats whichever Steam sits in.
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            push(PathBuf::from(pf).join("Steam"));
        }
    }
    // Off-default Steam (second drive) is only in the registry. HKLM, not HKCU: the host is
    // SYSTEM and its own hive does not know where the operator installed anything.
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

/// Canonicalize first so a junction out of the root is resolved before the containment test.
/// The config-dir exclusion is unconditional even if `PUNKTFUNK_LIBRARY_ART_ROOTS` names it.
fn art_path_is_confined(path: &Path) -> bool {
    // Refuse UNC before touching the filesystem: `canonicalize` would itself coerce the host's
    // machine account into outbound SMB. Any two leading separators — `\\`, `//`, mixed — count;
    // a bare `starts_with(r"\\")` missed the forms Windows accepts equally.
    let lossy = path.to_string_lossy();
    let bytes = lossy.as_bytes();
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    if bytes.len() >= 2 && is_sep(bytes[0]) && is_sep(bytes[1]) {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    resolved_art_path_is_confined(&real)
}

/// Containment half of [`art_path_is_confined`] on an already-resolved path (`canonicalize` or
/// [`final_path_of`]). The read path must judge the object it opened, not the path it was asked.
fn resolved_art_path_is_confined(real: &Path) -> bool {
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

/// Link-resolved path of the object `f` is open on. Re-run confinement on that object, not on
/// a path that a rename after open could retarget.
#[cfg(target_os = "linux")]
fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).ok()
}

/// Windows twin of Linux `final_path_of`: `GetFinalPathNameByHandleW` in normalized DOS form,
/// which carries the same `\\?\` prefix `canonicalize` produces, so `starts_with` matches.
#[cfg(windows)]
fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, GETFINALPATHNAMEBYHANDLE_FLAGS,
    };
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the handle is a live open File for the whole call, and `buf` is a valid
        // mutable u16 buffer of the length the API is told about.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(f.as_raw_handle()),
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(0), // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(std::ffi::OsString::from_wide(&buf[..n]).into());
        }
        buf.resize(n + 1, 0); // n = required length (incl. NUL) when the buffer was too small
    }
}

/// macOS twin (dev builds only — the shipped hosts are Linux and Windows): `F_GETPATH`.
#[cfg(all(unix, not(target_os = "linux")))]
fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: the fd is a live open File and `buf` is the PATH_MAX-sized buffer F_GETPATH
    // requires; the kernel NUL-terminates what it writes.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &buf[..len],
    )))
}

/// Serve what the bytes are, not what the extension claims — an extensionless secret or a
/// `key.pem` renamed `cover.png` must not come back as `application/octet-stream`.
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

/// Decode `file://` first, matching [`local_art_bytes`]. `Path::new` on a raw `file:///…` is a
/// relative path whose first component is `file:`, which canonicalizes against the cwd and
/// reads as "outside every root" — write time would then reject what read time would serve.
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

/// Reject a local-file art value the proxy would refuse to serve, so it never reaches the
/// catalog. URLs and already-proxied paths pass through. `Err` names the field.
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

/// Drop local-file art the proxy would refuse to serve, returning the `(field, value)` pairs
/// removed. URLs and already-proxied paths stay.
///
/// Provider-reconcile counterpart to [`validate_art_paths`]. A custom-entry PUT can 400 on one
/// path; a plugin reconcile that 400s on one cover would drop the whole store. `None` is the
/// no-art shape every client already renders.
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

/// 16 MiB cap (covers never approach that; the cap bounds host memory). Convert `file://`
/// first ([`file_url_to_path`]) so confinement and the read see the same path. Percent-decode
/// before canonicalize, or a `%2e%2e` escape is invisible to the traversal check.
pub fn local_art_bytes(path: &str) -> Option<(Vec<u8>, String)> {
    const MAX_ART_BYTES: u64 = 16 * 1024 * 1024;
    let path = file_url_to_path(path);
    if !art_path_is_servable(&path) {
        tracing::debug!(
            path = %path,
            "art proxy: refusing a path outside the allowed art roots"
        );
        return None;
    }
    // Re-check confinement and size on the opened handle, then read that handle with a hard cap.
    // A link swap cannot substitute a different file between validation and consumption.
    let p = std::path::Path::new(&*path);
    let mut f = std::fs::File::open(p).ok()?;
    let real = final_path_of(&f)?;
    if !resolved_art_path_is_confined(&real) {
        tracing::debug!(
            path = %path,
            "art proxy: opened file resolves outside the allowed art roots"
        );
        return None;
    }
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ART_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    use std::io::Read as _;
    (&mut f)
        .take(MAX_ART_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ART_BYTES {
        return None;
    }
    let ctype = sniff_image_type(&bytes)?;
    Some((bytes, ctype.to_string()))
}

fn resolve_art_bytes(v: &str) -> Option<(Vec<u8>, String)> {
    if is_local_art_path(v) {
        local_art_bytes(v)
    } else {
        fetch_image(v)
    }
}

/// Rewrite local-file art paths to host proxy URLs on `GET /library`, so a client does not
/// receive an unreachable `C:\…` path. `http(s)`/`data:` URLs and already-relative proxy paths stay.
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

/// Best box-art for a library id, for GameStream `/appasset` (Moonlight fetches covers from the
/// host, not the CDN). Blocking — call off the async runtime.
pub fn fetch_box_art(id: &str) -> Option<(Vec<u8>, String)> {
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
        let (bytes, ctype) = fetch_image("data:image/png;base64,SGk=").expect("data url decodes");
        assert_eq!(bytes, b"Hi");
        assert_eq!(ctype, "image/png");
        assert!(fetch_image("file:///etc/passwd").is_none());
        assert!(fetch_image("data:image/png;base64,").is_none());
    }

    /// Exclusions are the load-bearing half: two `/`-leading shapes are emitted by the host
    /// itself, so a POSIX rule that swallowed them would break the proxy round-trip.
    #[test]
    fn local_art_path_detection() {
        assert!(is_local_art_path(r"C:\Users\me\cover.jpg"));
        assert!(is_local_art_path("C:/Users/me/cover.png"));
        assert!(is_local_art_path(r"\\nas\share\art.jpg"));
        assert!(is_local_art_path("file:///home/u/covers/x.jpg"));
        assert!(is_local_art_path("file:///C:/covers/x.jpg"));
        assert!(is_local_art_path("/home/u/.cache/lutris/coverart/x.jpg"));
        assert!(is_local_art_path("/var/lib/steam/librarycache/570/h.jpg"));
        assert!(!is_local_art_path("https://cdn/x.jpg"));
        assert!(!is_local_art_path("http://host/x.jpg"));
        assert!(!is_local_art_path("data:image/png;base64,AAAA"));
        // The host's own art-proxy path must survive a second `proxy_local_art` pass.
        assert!(!is_local_art_path(
            "/api/v1/library/art/custom:abc/portrait"
        ));
        assert!(!is_local_art_path("/api/v1/library/art/steam:570/hero"));
        assert!(!is_local_art_path("//images.gog.com/abc_vertical.jpg"));
        assert!(!is_local_art_path("covers/x.jpg"));
        assert!(!is_local_art_path(""));
    }

    #[test]
    fn file_url_converts_to_a_path_and_percent_decodes() {
        assert_eq!(file_url_to_path("file:///home/u/c.jpg"), "/home/u/c.jpg");
        assert_eq!(
            file_url_to_path("file:///home/u/My%20Games/c%2Bx.jpg"),
            "/home/u/My Games/c+x.jpg"
        );
        assert_eq!(
            file_url_to_path("file:///C:/covers/c.jpg"),
            "C:/covers/c.jpg"
        );
        assert_eq!(
            file_url_to_path("file://nas/share/c.jpg"),
            r"\\nas\share\c.jpg"
        );
        assert_eq!(file_url_to_path("/home/u/c.jpg"), "/home/u/c.jpg");
        assert_eq!(file_url_to_path(r"C:\c.jpg"), r"C:\c.jpg");
        // A lone `%` is a legal path character, not a decode failure.
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

    /// No env mutation: only `local_art_bytes_is_confined_and_image_only` touches
    /// `PUNKTFUNK_LIBRARY_ART_ROOTS` — cargo runs these tests in parallel threads of one process.
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

        // Re-running the rewrite is a no-op: the emitted proxy path must not be mistaken for a file.
        let before = art.portrait.clone();
        proxy_local_art("lutris:42", &mut art);
        assert_eq!(art.portrait, before);
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];

    /// Art-root env vars are process-global and cargo runs tests as threads, so mutating tests
    /// must not overlap. Poison is recovered: a panic here must not cascade as `PoisonError`.
    static ART_ROOTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Holds `ART_ROOTS_LOCK` and the overrides one test needs; restores previous values on
    /// drop, including unwind. The only writer of these env vars in the binary.
    struct ArtRootsEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ArtRootsEnv {
        /// `None` unsets the variable for the test's duration.
        fn set(vars: &[(&'static str, Option<&Path>)]) -> Self {
            let _lock = ART_ROOTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                // SAFETY: `_lock` is held for this guard's whole lifetime, and this type is the
                // only writer of these variables in the binary — so no other thread is reading
                // them while they change.
                unsafe { write_env(key, value.map(|p| p.as_os_str())) };
            }
            Self { _lock, saved }
        }
    }

    impl Drop for ArtRootsEnv {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                // SAFETY: still under `_lock`, which outlives this loop — same argument as `set`.
                unsafe { write_env(key, value.as_deref()) };
            }
        }
    }

    /// # Safety
    /// The caller must hold `ART_ROOTS_LOCK`; the process environment is global and unsound to
    /// mutate while another thread reads it.
    unsafe fn write_env(key: &str, value: Option<&std::ffi::OsStr>) {
        match value {
            // SAFETY: the caller holds `ART_ROOTS_LOCK` (this function's documented contract), and
            // `ArtRootsEnv` is the only writer in the binary — so no other thread is reading the
            // environment while it changes.
            Some(v) => unsafe { std::env::set_var(key, v) },
            // SAFETY: as above — the caller's lock is what makes this sound.
            None => unsafe { std::env::remove_var(key) },
        }
    }

    fn confine_art_to(dir: &Path) -> ArtRootsEnv {
        ArtRootsEnv::set(&[("PUNKTFUNK_LIBRARY_ART_ROOTS", Some(dir))])
    }

    /// Confinement, extension, and content sniff are all load-bearing: the proxy reads in the
    /// host process from a path the plugin lane can write.
    #[test]
    fn local_art_bytes_is_confined_and_image_only() {
        let dir = std::env::temp_dir().join(format!("pf-art-test-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let _env = confine_art_to(&dir);

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();
        let (bytes, ctype) = local_art_bytes(cover.to_str().unwrap()).expect("reads a real cover");
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");

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

        let elsewhere = outside.join("cover.png");
        std::fs::write(&elsewhere, PNG).unwrap();
        assert!(
            local_art_bytes(elsewhere.to_str().unwrap()).is_none(),
            "a path outside every art root must be refused"
        );
        // Canonicalize first, or `..` out of the root would look contained.
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

        // Decode `file://` before confinement, or the check inspects a string that is not the path.
        let as_url = file_url(&cover);
        assert_eq!(
            local_art_bytes(&as_url)
                .expect("file:// reads the same cover")
                .0,
            PNG
        );
        assert!(
            local_art_bytes(&file_url(&elsewhere)).is_none(),
            "file:// must not escape the art roots"
        );
        // Percent-decode before canonicalize, or `%2e%2e` hides from the `..` check.
        assert!(
            local_art_bytes(&format!(
                "{}/%2e%2e/{}/cover.png",
                file_url(&dir),
                outside.file_name().unwrap().to_str().unwrap()
            ))
            .is_none(),
            "percent-encoded traversal must be refused"
        );

        // UNC is refused before any filesystem hit (outbound SMB auth coercion).
        assert!(!art_path_is_servable(r"\\attacker\share\a.png"));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Kit-shaped `file://` for both platforms. POSIX keeps two slashes (`file:///home/…`);
    /// Windows needs three (`file:///C:/…`). `format!("file://{path}")` on Windows is
    /// `file://C:\…`, whose authority is `C:` — a UNC reference, not a local file.
    fn file_url(p: &std::path::Path) -> String {
        let posix = p.to_str().unwrap().replace('\\', "/");
        if posix.starts_with('/') {
            format!("file://{posix}")
        } else {
            format!("file:///{posix}")
        }
    }

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

    /// Write gate and read gate must judge the same string. `Path::new` on raw `file:///…` is
    /// a relative `file:` path; asserting servable and readable together is the point — either
    /// alone still passes if only one side decodes.
    #[test]
    fn file_url_art_is_accepted_at_write_time_exactly_as_at_read_time() {
        let dir = std::env::temp_dir().join(format!("pf-art-wr-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-wr-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let _env = confine_art_to(&dir);

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();

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

        let spaced = dir.join("My Cover.png");
        std::fs::write(&spaced, PNG).unwrap();
        let spaced_url = file_url(&spaced).replace(' ', "%20");
        assert!(
            art_path_is_servable(&spaced_url),
            "percent-encoded names must decode before the containment test: {spaced_url}"
        );
        assert!(local_art_bytes(&spaced_url).is_some(), "read time agrees");

        // Write-gate decode must not skip confinement: out-of-root `file://` is still refused.
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

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Assert the survivors too: a sanitizer that cleared the whole struct would pass a
    /// drop-only test.
    #[test]
    fn sanitize_drops_only_the_unservable_local_art() {
        let dir = std::env::temp_dir().join(format!("pf-art-san-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _env = confine_art_to(&dir);

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
        assert_eq!(art.portrait.as_deref(), Some(cover_url.as_str()));
        assert_eq!(art.logo.as_deref(), Some("https://cdn/l.png"));
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/steam:570/header")
        );
        assert!(sanitize_art_paths(&mut art).is_empty());

        assert!(validate_art_paths(&art).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Steam cover under Program Files is servable with no `PUNKTFUNK_LIBRARY_ART_ROOTS`.
    /// Hermetic: `%ProgramFiles(x86)%` is pointed at a temp tree. Asserting over whatever
    /// Steam this box has would pass vacuously on CI.
    #[cfg(windows)]
    #[test]
    fn steam_librarycache_cover_is_servable_without_configuration() {
        let base = std::env::temp_dir().join(format!("pf-art-steam-{}", std::process::id()));
        let hero = base
            .join("Steam")
            .join("appcache")
            .join("librarycache")
            .join("570")
            .join("abcdef")
            .join("library_hero.jpg");
        std::fs::create_dir_all(hero.parent().unwrap()).unwrap();
        std::fs::write(&hero, PNG).unwrap();

        // No configured roots; `%ProgramFiles(x86)%` is a real variable later tests may read.
        let _env = ArtRootsEnv::set(&[
            ("PUNKTFUNK_LIBRARY_ART_ROOTS", None),
            ("ProgramFiles(x86)", Some(&base)),
        ]);

        let steam_root = base.join("Steam");
        assert!(
            steam_art_roots().contains(&steam_root),
            "the Program Files probe must find the Steam install"
        );
        assert!(
            art_roots().contains(&steam_root),
            "the DEFAULT art roots must include it — the whole point is that no env var is needed"
        );

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

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Playnite roots must be in the default confinement with no `PUNKTFUNK_LIBRARY_ART_ROOTS`.
    /// Vacuous on a box with no Playnite — the registry half cannot be faked from here.
    #[cfg(windows)]
    #[test]
    fn playnite_roots_reach_the_art_confinement() {
        let _env = ArtRootsEnv::set(&[("PUNKTFUNK_LIBRARY_ART_ROOTS", None)]);
        let roots = art_roots();
        for root in crate::library::launch::playnite_art_roots() {
            assert!(root.is_dir(), "{root:?} is offered as an art root");
            assert!(
                roots.contains(&root),
                "{root:?} must be an allowed art root with no env var set"
            );
        }
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
        assert_eq!(sniff_image_type(b"-----BEGIN PRIVATE KEY-----"), None);
        assert_eq!(sniff_image_type(b"9f8a7b6c5d4e3f2a1b0c"), None);
        assert_eq!(sniff_image_type(b""), None);
    }
}
