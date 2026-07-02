// Geist — the punktfunk brand typeface (the same family the website ships). Bundled as static
// OTF weights in this kit's resources and registered with Core Text at first use, so it works
// identically in the Xcode app and the `swift run` dev shell (Bundle.module resolves to the
// package resource bundle in both). Geist Sans carries titles/UI; Geist Mono carries the technical
// readouts — host addresses, status labels, the stream-stats HUD — for the industrial look.
//
// Licensed under the SIL Open Font License 1.1 (Resources/Fonts/Geist-OFL.txt).

import CoreText
import SwiftUI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

public enum BrandFont {
    public enum Weight {
        case regular, medium, semibold, bold
    }

    /// PostScript names of the bundled faces (verified from each OTF's name table). Geist Sans only
    /// — Geist Mono is intentionally not shipped; the app's typeface is Geist Sans throughout.
    private static let sansFaces = ["Geist-Regular", "Geist-Medium", "Geist-SemiBold", "Geist-Bold"]

    /// Registered exactly once per process — a static `let` initializer is run lazily and is
    /// guaranteed thread-safe + run-at-most-once by the runtime.
    private static let registered: Void = {
        for face in sansFaces {
            guard let url = Bundle.module.url(
                forResource: face, withExtension: "otf", subdirectory: "Fonts") else {
                #if DEBUG
                print("BrandFont: bundled face \(face).otf not found — text will fall back to system")
                #endif
                continue
            }
            var error: Unmanaged<CFError>?
            if !CTFontManagerRegisterFontsForURL(url as CFURL, .process, &error) {
                #if DEBUG
                let message = error?.takeRetainedValue().localizedDescription ?? "unknown error"
                print("BrandFont: failed to register \(face): \(message)")
                #endif
            }
        }
    }()

    /// Force registration before the first `Font.custom` lookup. Cheap to call repeatedly.
    public static func registerIfNeeded() { _ = registered }

    fileprivate static func sansFace(_ weight: Weight) -> String {
        switch weight {
        case .regular: return "Geist-Regular"
        case .medium: return "Geist-Medium"
        case .semibold: return "Geist-SemiBold"
        case .bold: return "Geist-Bold"
        }
    }
}

public extension Color {
    /// The punktfunk brand purple (the app-icon lens / website `--brand`). Defined explicitly,
    /// independent of the asset-catalog accent — `Color.accentColor` resolution is environment- and
    /// timing-sensitive (it can fall back to system blue), and the brand mark must never drift.
    /// Light: #6656F2, Dark: #8678F5 (the lighter violet reads better on dark surfaces).
    static let brand: Color = {
        #if canImport(UIKit)
        return Color(UIColor { traits in
            traits.userInterfaceStyle == .dark
                ? UIColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
                : UIColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
        })
        #elseif canImport(AppKit)
        return Color(NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
                : NSColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
        })
        #else
        // Non-Apple fallback: the light brand value, so all branches agree on a canonical color.
        return Color(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255)
        #endif
    }()
}

public extension Font {
    /// Geist Sans at an explicit point size, scaling with Dynamic Type relative to `textStyle`.
    static func geist(
        _ size: CGFloat, _ weight: BrandFont.Weight = .regular,
        relativeTo textStyle: TextStyle = .body
    ) -> Font {
        BrandFont.registerIfNeeded()
        return .custom(BrandFont.sansFace(weight), size: size, relativeTo: textStyle)
    }

    /// Geist Sans at a FIXED point size that does not scale with Dynamic Type — for glyphs pinned
    /// inside a fixed-size container (e.g. the monogram tile), where a scaled letter would overflow.
    static func geistFixed(_ size: CGFloat, _ weight: BrandFont.Weight = .regular) -> Font {
        BrandFont.registerIfNeeded()
        return .custom(BrandFont.sansFace(weight), fixedSize: size)
    }
}
