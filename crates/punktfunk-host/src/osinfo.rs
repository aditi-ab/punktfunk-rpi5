//! Host OS identity for client-facing surfaces: an icon-friendly **specificity chain** plus a
//! human-readable name.
//!
//! The chain is slash-separated, generic → specific, `family[/like][/id]`:
//! `windows`, `macos`, `linux`, `linux/debian/ubuntu`, `linux/fedora/bazzite`,
//! `linux/arch/steamos`. Clients walk it most-specific-first and show the first token they have
//! art for — a client with no Bazzite mark lands on `fedora`, then generic `linux` — so the host
//! always emits the *full* chain and clients need zero distro→parent knowledge. On Linux the
//! middle token is the first `ID_LIKE` ancestor we recognize ([`FAMILIES`]; `ID_LIKE` is ordered
//! most-similar-first per the os-release spec) and the leaf is `ID` verbatim, so even a distro we
//! never heard of still degrades to its family's icon.
//!
//! The chain rides the mDNS `os=` TXT record ([`crate::discovery`]) and the mgmt API's
//! `HostInfo.os`; the pretty name (`PRETTY_NAME`) is REST-only — TXT stays small. Both values are
//! advisory identity, same trust posture as the `mac` TXT key: unauthenticated, and a wrong value
//! only draws a wrong icon.

use std::sync::OnceLock;

/// The host's OS identity, detected once per process (os-release is static for its lifetime).
pub struct OsInfo {
    /// `windows` | `macos` | `linux[/<family>][/<id>]` — see the module doc for the grammar.
    pub chain: String,
    /// Human-readable OS name (os-release `PRETTY_NAME`; `"Windows"`/`"macOS"` elsewhere).
    pub pretty: String,
}

/// Detect (once) and return the host's OS identity.
pub fn detect() -> &'static OsInfo {
    static OS: OnceLock<OsInfo> = OnceLock::new();
    OS.get_or_init(|| {
        if cfg!(target_os = "windows") {
            OsInfo {
                chain: "windows".into(),
                pretty: "Windows".into(),
            }
        } else if cfg!(target_os = "macos") {
            // The host only runs on Windows/Linux; macOS is the dev-build platform.
            OsInfo {
                chain: "macos".into(),
                pretty: "macOS".into(),
            }
        } else {
            linux_os_info()
        }
    })
}

/// `ID_LIKE` ancestors worth keeping as the chain's middle token — the distros clients plausibly
/// have icon art for. Anything else (or no `ID_LIKE` at all) yields a two-segment chain and the
/// leaf still identifies the distro exactly.
const FAMILIES: &[&str] = &[
    "debian", "ubuntu", "fedora", "rhel", "arch", "opensuse", "suse", "gentoo", "alpine", "nixos",
];

/// Read os-release from its two spec'd locations (`/etc` overrides the `/usr/lib` fallback);
/// a host with neither (or unreadable) is simply `linux`.
fn linux_os_info() -> OsInfo {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_os_release(&s))
        .unwrap_or_else(|| OsInfo {
            chain: "linux".into(),
            pretty: "Linux".into(),
        })
}

/// Pure os-release parser (compiled on every target so the tests run on any dev box).
///
/// Grammar per the freedesktop spec subset we need: `KEY=value` lines, values optionally
/// `"`/`'`-quoted. `ID`/`ID_LIKE` tokens are lowercased and sanitized before they feed a DNS TXT
/// record; `PRETTY_NAME` falls back `NAME` → capitalized `ID` → `"Linux"`.
fn parse_os_release(contents: &str) -> OsInfo {
    let (mut id, mut id_like, mut pretty, mut name) = (None, None, None, None);
    for line in contents.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("ID=") {
            id = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
            pretty = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("NAME=") {
            name = Some(unquote(v));
        }
    }

    let id_tok = id.as_deref().and_then(sanitize_token);
    // First *recognized* ID_LIKE ancestor wins (the list is ordered most-similar-first), skipping
    // a token that merely repeats ID (some distros write `ID_LIKE` reflexively).
    let like_tok = id_like
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .filter_map(sanitize_token)
        .find(|t| FAMILIES.contains(&t.as_str()) && Some(t) != id_tok.as_ref());

    let mut chain = String::from("linux");
    for tok in [&like_tok, &id_tok].into_iter().flatten() {
        chain.push('/');
        chain.push_str(tok);
    }

    let pretty = [pretty, name]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .or_else(|| id_tok.as_deref().map(capitalize))
        .unwrap_or_else(|| "Linux".into());

    OsInfo { chain, pretty }
}

/// Is this an **Omarchy** box? (`ID=omarchy`, which the chain carries verbatim as its leaf.)
///
/// A *flavour*, never a family: `ID_LIKE=arch` already routes everything family-shaped — the
/// install ladder, `InstallKind::Pacman`, the docs — correctly, and nothing here should change
/// that. What the flavour decides is narrower and listed in one place so it stays auditable:
///
///  * the update tier (`crate::update`): Omarchy's own `omarchy update` owns the pacman
///    transaction, and a pacman guard blocks the direct `pacman -Syu` our root helper would run,
///    so the console reports **notify-only** and names their command;
///  * diagnostics rows (`crate::diagnostics`), which is where an operator finds out what an
///    Omarchy-specific check saw.
///
/// Detected from the same `os-release` parse as [`detect`], which Omarchy rewrites on every
/// `omarchy-settings` upgrade — so it survives updates, which a marker file in our own package
/// would not. Misdetection degrades to plain-Arch behaviour, which is exactly the old behaviour.
pub fn is_omarchy() -> bool {
    detect().chain.ends_with("/omarchy")
}

/// Strip one matching pair of surrounding `"` or `'` quotes.
fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    v.to_string()
}

/// Lowercase and reduce an `ID`/`ID_LIKE` token to TXT-safe `[a-z0-9._-]`, capped at 32 chars;
/// `None` when nothing survives (a hostile or garbage value must not reach the advert).
fn sanitize_token(raw: &str) -> Option<String> {
    let tok: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        .take(32)
        .collect();
    (!tok.is_empty()).then_some(tok)
}

/// `ubuntu` → `Ubuntu` (ASCII-only; the fallback label when os-release names are absent).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(contents: &str) -> (String, String) {
        let info = parse_os_release(contents);
        (info.chain, info.pretty)
    }

    #[test]
    fn ubuntu_chains_through_debian() {
        let (chain, pretty) = parsed(
            "PRETTY_NAME=\"Ubuntu 24.04.2 LTS\"\nNAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n",
        );
        assert_eq!(chain, "linux/debian/ubuntu");
        assert_eq!(pretty, "Ubuntu 24.04.2 LTS");
    }

    #[test]
    fn bazzite_quoted_id_like() {
        let (chain, pretty) =
            parsed("ID=bazzite\nID_LIKE=\"fedora\"\nPRETTY_NAME=\"Bazzite 42 (Kinoite)\"\n");
        assert_eq!(chain, "linux/fedora/bazzite");
        assert_eq!(pretty, "Bazzite 42 (Kinoite)");
    }

    /// Omarchy 4.x writes `ID=omarchy` / `ID_LIKE=arch` / `VERSION_ID=<pkgver>` and *rewrites*
    /// os-release on every `omarchy-settings` upgrade, so this is the detection that survives an
    /// `omarchy update`. It must land in the arch family (the install ladder and `InstallKind`
    /// depend on it) AND keep `omarchy` as the leaf (the flavour predicate reads it).
    #[test]
    fn omarchy_is_arch_family_and_keeps_its_leaf() {
        let (chain, pretty) = parsed(
            "NAME=\"Omarchy\"\nPRETTY_NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\nVERSION_ID=4.0.1\n",
        );
        assert_eq!(chain, "linux/arch/omarchy");
        assert_eq!(pretty, "Omarchy");
        assert!(chain.ends_with("/omarchy"), "is_omarchy() reads this");
        // …and a plain Arch box must NOT trip the flavour.
        let (arch, _) = parsed("ID=arch\nPRETTY_NAME=\"Arch Linux\"\n");
        assert!(!arch.ends_with("/omarchy"));
    }

    #[test]
    fn steamos_is_arch_family() {
        let (chain, _) = parsed("ID=steamos\nID_LIKE=arch\nPRETTY_NAME=\"SteamOS\"\n");
        assert_eq!(chain, "linux/arch/steamos");
    }

    #[test]
    fn cachyos_is_arch_family() {
        let (chain, _) = parsed("ID=cachyos\nID_LIKE=\"arch\"\nNAME=\"CachyOS Linux\"\n");
        assert_eq!(chain, "linux/arch/cachyos");
    }

    #[test]
    fn nobara_takes_rhel_the_first_recognized_ancestor() {
        // Verified against a real Nobara 44 box: it declares `ID_LIKE="rhel centos fedora"`,
        // so the family token is *rhel*, not the fedora one might expect. Clients ship a
        // `nobara` mark, so the leaf matches before the family ever gets a say — but a
        // RHEL-family distro we have no art for lands on plain Tux, not on Fedora.
        let (chain, pretty) = parsed(
            "NAME=\"Nobara Linux\"\nID=nobara\nID_LIKE=\"rhel centos fedora\"\nPRETTY_NAME=\"Nobara Linux 44 (KDE Plasma Desktop Edition)\"\n",
        );
        assert_eq!(chain, "linux/rhel/nobara");
        assert_eq!(pretty, "Nobara Linux 44 (KDE Plasma Desktop Edition)");
    }

    #[test]
    fn nixos_without_id_like_is_two_segments() {
        let (chain, _) = parsed("ID=nixos\nPRETTY_NAME=\"NixOS 25.05 (Warbler)\"\n");
        assert_eq!(chain, "linux/nixos");
    }

    #[test]
    fn plain_root_distro() {
        let (chain, _) = parsed("ID=arch\nNAME=\"Arch Linux\"\n");
        assert_eq!(chain, "linux/arch");
    }

    #[test]
    fn multi_ancestor_id_like_takes_first_recognized() {
        // centos: ID_LIKE is most-similar-first — rhel wins over fedora.
        let (chain, _) = parsed("ID=\"centos\"\nID_LIKE=\"rhel fedora\"\n");
        assert_eq!(chain, "linux/rhel/centos");
    }

    #[test]
    fn pop_takes_ubuntu_over_debian() {
        let (chain, _) = parsed("ID=pop\nID_LIKE=\"ubuntu debian\"\n");
        assert_eq!(chain, "linux/ubuntu/pop");
    }

    #[test]
    fn reflexive_id_like_is_skipped() {
        let (chain, _) = parsed("ID=fedora\nID_LIKE=fedora\n");
        assert_eq!(chain, "linux/fedora");
    }

    #[test]
    fn unrecognized_id_like_is_dropped_leaf_kept() {
        let (chain, _) = parsed("ID=chimera\nID_LIKE=frontier\n");
        assert_eq!(chain, "linux/chimera");
    }

    #[test]
    fn garbage_and_empty_fall_back_to_linux() {
        assert_eq!(parsed("").0, "linux");
        assert_eq!(parsed("not an os-release file at all\n===\n").0, "linux");
        assert_eq!(parsed("").1, "Linux");
    }

    #[test]
    fn hostile_id_is_sanitized_for_txt() {
        // Quotes/spaces/slashes must never reach the TXT record or split the chain.
        let (chain, _) = parsed("ID=\"Ubu ntu/EVIL=1\"\n");
        assert_eq!(chain, "linux/ubuntuevil1");
    }

    #[test]
    fn overlong_id_is_capped() {
        let long = "x".repeat(80);
        let (chain, _) = parsed(&format!("ID={long}\n"));
        assert_eq!(chain, format!("linux/{}", "x".repeat(32)));
    }

    #[test]
    fn pretty_falls_back_name_then_id() {
        assert_eq!(
            parsed("ID=debian\nNAME=\"Debian GNU/Linux\"\n").1,
            "Debian GNU/Linux"
        );
        assert_eq!(parsed("ID=debian\n").1, "Debian");
    }

    #[test]
    fn single_quoted_values() {
        let (chain, pretty) = parsed("ID='opensuse-tumbleweed'\nID_LIKE='opensuse suse'\nPRETTY_NAME='openSUSE Tumbleweed'\n");
        assert_eq!(chain, "linux/opensuse/opensuse-tumbleweed");
        assert_eq!(pretty, "openSUSE Tumbleweed");
    }

    #[test]
    fn crlf_and_padding_tolerated() {
        let (chain, _) = parsed("  ID=ubuntu \r\n\tID_LIKE=debian\r\n");
        assert_eq!(chain, "linux/debian/ubuntu");
    }
}
