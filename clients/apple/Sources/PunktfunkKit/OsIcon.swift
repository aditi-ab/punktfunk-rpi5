// The host cards' OS marks: template vector imagesets in Resources/OsIcons.xcassets
// (derived from the repo's assets/os-icons masters — Font Awesome Free brands CC BY 4.0 +
// Simple Icons CC0; provenance in that directory's README), resolved from the host's
// OS-identity chain via PunktfunkShared's `osIconTokens` walk. Template rendering means
// they tint with `foregroundStyle` like an SF Symbol.

import PunktfunkShared
import SwiftUI

/// The icon tokens this client ships art for. A distro without its own mark (Bazzite,
/// CachyOS, ...) degrades to its family's and finally to Tux via the chain walk.
private let osIconTokensShipped: Set<String> = [
    "windows", "apple", "linux", "steam", "ubuntu", "fedora", "arch", "debian", "nixos",
    "opensuse",
]

/// The mark for an OS-identity chain (`linux/fedora/bazzite`, ...), or nil — no view at
/// all — when the host doesn't advertise one / nothing in the chain is recognized, so
/// those cards render exactly as they did before the field existed.
public func osIconImage(for chain: String?) -> Image? {
    osIconTokens(chain)
        .first(where: osIconTokensShipped.contains)
        .map { Image("os-\($0)", bundle: .module) }
}
