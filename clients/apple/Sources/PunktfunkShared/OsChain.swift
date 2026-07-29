// The client half of the host's OS-identity advertisement (mDNS `os` TXT — producer:
// punktfunk-host's osinfo.rs): sanitize the untrusted chain once, and turn it into the
// icon-lookup order every surface walks. Mirrors pf-client-core's `os.rs` (and the Kotlin
// port) so all platforms resolve identically; in the dependency-free shared module so the
// widget could use it too. The Image lookup itself lives in PunktfunkKit (OsIcon.swift).

import Foundation

/// Reduce a raw `os` TXT value (e.g. `linux/fedora/bazzite`) to the trusted grammar:
/// lowercase slash-separated tokens of `[a-z0-9._-]`, each ≤ 32 chars, at most 5. mDNS is
/// unauthenticated input — anything outside the grammar is dropped, and a value that
/// sanitizes to nothing becomes `""` (same rendering as a host that advertises no `os`).
public func sanitizeOsChain(_ raw: String) -> String {
    raw.lowercased()
        .split(separator: "/", omittingEmptySubsequences: true)
        .map { token -> String in
            let kept = token.unicodeScalars.filter { s in
                (0x61...0x7A).contains(s.value) // a-z
                    || (0x30...0x39).contains(s.value) // 0-9
                    || s == "." || s == "_" || s == "-"
            }
            return String(String.UnicodeScalarView(kept.prefix(32)))
        }
        .filter { !$0.isEmpty }
        .prefix(5)
        .joined(separator: "/")
}

/// The icon-lookup order for a chain: sanitized tokens most-specific-first, with brand
/// aliases applied (`macos` → `apple` art, `steamos` → `steam` art). A surface takes the
/// first token it has art for; empty means "no OS icon" (older host / garbage advert).
public func osIconTokens(_ chain: String?) -> [String] {
    sanitizeOsChain(chain ?? "")
        .split(separator: "/")
        .reversed()
        .map {
            switch $0 {
            case "macos": return "apple"
            case "steamos": return "steam"
            default: return String($0)
            }
        }
}
