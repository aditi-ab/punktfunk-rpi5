// Coloured icons inside a `Menu`.
//
// A tinted `Label` icon comes out monochrome in a menu on every Apple platform: SwiftUI hands the
// row's icon to UIKit/AppKit as a TEMPLATE image, and a template image is a stencil — the tint is
// thrown away and the system's own colour is filled in. `.foregroundStyle(.red)` on a trash symbol
// and `.foregroundStyle(profile.accentColor)` on a swatch both vanish the same way.
//
// So the icon is rasterised first. A bitmap marked `.renderingMode(.original)` is not a stencil,
// and the colour survives into the menu. This is the only place in the app that needs the trick —
// everywhere else a tinted symbol just works.

import SwiftUI

@MainActor
enum MenuIcon {
    /// A filled dot in `color` — a profile's chip, at menu-icon size.
    static func swatch(_ color: Color, scheme: ColorScheme) -> Image? {
        render(key: "dot-\(scheme)-\(color)", scheme: scheme) {
            Circle().fill(color).frame(width: 12, height: 12)
        }
    }

    /// An SF Symbol that keeps `color` — the red trash the destructive role doesn't give us
    /// (iOS colours the row itself, macOS menus have no destructive styling at all).
    static func symbol(_ name: String, color: Color, scheme: ColorScheme) -> Image? {
        render(key: "sym-\(name)-\(scheme)-\(color)", scheme: scheme) {
            Image(systemName: name)
                .font(.system(size: 14, weight: .regular))
                .foregroundStyle(color)
        }
    }

    /// Rasterised once per (icon, appearance). A menu rebuilds its rows on every open and on
    /// every catalog change, and re-rendering a bitmap each time to draw a 12pt dot would be
    /// silly; the set of icons is tiny and bounded by the palette.
    private static var cache: [String: Image] = [:]

    private static func render<Content: View>(
        key: String, scheme: ColorScheme, @ViewBuilder content: () -> Content
    ) -> Image? {
        if let cached = cache[key] { return cached }
        // The scheme goes INTO the rendered content: `Color.brand` (and the system reds) are
        // dynamic, and a renderer with no appearance resolves them to the light variant whatever
        // the app is showing.
        let renderer = ImageRenderer(content: content().environment(\.colorScheme, scheme))
        renderer.scale = 3
        let image: Image?
        #if canImport(UIKit)
        image = renderer.uiImage.map { Image(uiImage: $0).renderingMode(.original) }
        #else
        image = renderer.nsImage.map { Image(nsImage: $0).renderingMode(.original) }
        #endif
        if let image { cache[key] = image }
        return image
    }
}
