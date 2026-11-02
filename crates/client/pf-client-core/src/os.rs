//! Client half of the host's OS-identity advertisement (mDNS `os=` TXT; producer
//! is the host crate's `osinfo.rs`): sanitize the untrusted chain once, then the
//! icon-lookup order every front-end walks.
//!
//! Slash-separated, generic → specific (`linux[/<family>][/<id>]`). A UI walks
//! [`os_icon_tokens`] most-specific-first (brand aliases applied) and takes the
//! first token it has art for. Empty or unknown chains fall through to the UI's
//! fallback glyph. UI-agnostic so every shell resolves identically.

pub use punktfunk_core::discovery::sanitize_os;

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

    /// The grammar and the walk every client runs, from the shared vector file.
    #[test]
    fn shared_advert_vectors_agree_on_the_os_chain() {
        let raw = include_str!("../../../../clients/shared/advert-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let chains = file["os_chain"].as_array().expect("os_chain cases");
        assert!(
            chains.len() >= 10,
            "the vector file is the contract; keep it rich"
        );
        for case in chains {
            let raw = case["raw"].as_str().unwrap();
            assert_eq!(
                sanitize_os(raw),
                case["expect"].as_str().unwrap(),
                "{raw:?}"
            );
        }
        for case in file["os_icon_tokens"]
            .as_array()
            .expect("os_icon_tokens cases")
        {
            let chain = case["chain"].as_str().unwrap();
            let want: Vec<&str> = case["expect"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap())
                .collect();
            assert_eq!(os_icon_tokens(chain), want, "{chain:?}");
        }
    }

    #[test]
    fn walk_of_nothing_is_empty() {
        assert!(os_icon_tokens("").is_empty());
        assert!(os_icon_tokens("!!!").is_empty());
    }
}
