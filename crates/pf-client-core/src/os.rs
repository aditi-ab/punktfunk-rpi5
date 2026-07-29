//! The client half of the host's OS-identity advertisement (the mDNS `os=` TXT record — see the
//! host crate's `osinfo.rs` for the producer): sanitize the untrusted chain once, and turn it
//! into the icon-lookup order every front-end walks.
//!
//! The chain is slash-separated, generic → specific (`windows`, `macos`,
//! `linux[/<family>][/<id>]`, e.g. `linux/fedora/bazzite`). A UI resolves an icon by walking
//! [`os_icon_tokens`] (most-specific-first, brand aliases applied) and taking the first token it
//! has art for — so a client with no Bazzite mark lands on `fedora`, then generic `linux`, and an
//! unknown chain simply falls through to the UI's fallback glyph. Kept UI-agnostic here so the
//! GTK, Windows and console shells (and the Swift/Kotlin ports, held to the same rules) resolve
//! identically.

/// Reduce a raw `os` TXT value to the trusted grammar: lowercase slash-separated tokens of
/// `[a-z0-9._-]` (each capped at 32 chars, at most 5 of them). mDNS is unauthenticated input —
/// anything outside the grammar is dropped, and a value that sanitizes to nothing becomes `""`
/// (same rendering as an older host that doesn't advertise `os` at all).
pub fn sanitize_os(raw: &str) -> String {
    let tokens: Vec<String> = raw
        .to_lowercase()
        .split('/')
        .map(|t| {
            t.chars()
                .filter(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                })
                .take(32)
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .take(5)
        .collect();
    tokens.join("/")
}

/// The icon-lookup order for a chain: sanitized tokens most-specific-first, with brand aliases
/// applied (`macos` → `apple` art, `steamos` → `steam` art). A UI takes the first token it has
/// art for; an empty result (empty/garbage chain) means "no OS icon", exactly like an older host
/// that doesn't advertise one.
pub fn os_icon_tokens(chain: &str) -> Vec<String> {
    sanitize_os(chain)
        .split('/')
        .rev()
        .filter(|t| !t.is_empty())
        .map(|t| match t {
            "macos" => "apple".to_string(),
            "steamos" => "steam".to_string(),
            t => t.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_passes_well_formed_chains() {
        assert_eq!(sanitize_os("windows"), "windows");
        assert_eq!(sanitize_os("linux/fedora/bazzite"), "linux/fedora/bazzite");
        assert_eq!(
            sanitize_os("linux/opensuse/opensuse-tumbleweed"),
            "linux/opensuse/opensuse-tumbleweed"
        );
    }

    #[test]
    fn sanitize_folds_case_and_drops_junk() {
        assert_eq!(sanitize_os("Linux/Fedora"), "linux/fedora");
        assert_eq!(sanitize_os("linux/fe do ra!/§"), "linux/fedora");
        assert_eq!(sanitize_os("///"), "");
        assert_eq!(sanitize_os(""), "");
    }

    #[test]
    fn sanitize_caps_token_length_and_count() {
        let long = "x".repeat(80);
        assert_eq!(sanitize_os(&long), "x".repeat(32));
        assert_eq!(sanitize_os("a/b/c/d/e/f/g"), "a/b/c/d/e");
    }

    #[test]
    fn walk_is_most_specific_first() {
        assert_eq!(
            os_icon_tokens("linux/fedora/bazzite"),
            ["bazzite", "fedora", "linux"]
        );
        assert_eq!(os_icon_tokens("windows"), ["windows"]);
    }

    #[test]
    fn walk_applies_brand_aliases() {
        assert_eq!(os_icon_tokens("macos"), ["apple"]);
        assert_eq!(
            os_icon_tokens("linux/arch/steamos"),
            ["steam", "arch", "linux"]
        );
    }

    #[test]
    fn walk_of_nothing_is_empty() {
        assert!(os_icon_tokens("").is_empty());
        assert!(os_icon_tokens("!!!").is_empty());
    }
}
