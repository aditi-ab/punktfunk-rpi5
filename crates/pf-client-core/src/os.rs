//! Client half of the host's OS-identity advertisement (mDNS `os=` TXT; producer
//! is the host crate's `osinfo.rs`): sanitize the untrusted chain once, then the
//! icon-lookup order every front-end walks.
//!
//! Slash-separated, generic → specific (`linux[/<family>][/<id>]`). A UI walks
//! [`os_icon_tokens`] most-specific-first (brand aliases applied) and takes the
//! first token it has art for. Empty or unknown chains fall through to the UI's
//! fallback glyph. UI-agnostic so every shell resolves identically.

/// Untrusted mDNS `os=` TXT. Empty is an older host that does not advertise
/// `os`.
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

/// Most-specific-first after sanitize. Empty means no OS icon.
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
