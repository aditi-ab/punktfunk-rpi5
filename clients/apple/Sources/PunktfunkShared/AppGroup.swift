// The App-Group foundation shared by the app and its extensions (Widgets / Live Activity).
//
// PunktfunkShared is deliberately dependency-free: it links NEITHER PunktfunkKit (which drags in
// the Rust staticlib + presentation layer) NOR any Apple UI framework. A widget process gets ~30 MB,
// so everything an extension needs — the stored-host model + its JSON codec, the settings-key names,
// the deep-link grammar, and (later) the Live Activity attributes — lives here and here only.

import Foundation

/// The one App-Group identifier, matched by `Config/*.entitlements`
/// (`com.apple.security.application-groups`). Registered on the developer portal for both the app
/// id (`io.unom.punktfunk`) and the widget extension id (`io.unom.punktfunk.widgets`).
public enum AppGroup {
    public static let suiteName = "group.io.unom.punktfunk"

    /// The shared defaults suite. Non-nil in a correctly-entitled process; falls back to
    /// `.standard` if the group is somehow unavailable (unsigned `swift run`, a misprovisioned
    /// build) so the app still functions single-process rather than crashing — the widget just
    /// won't see the same store there.
    public static var defaults: UserDefaults {
        UserDefaults(suiteName: suiteName) ?? .standard
    }
}
