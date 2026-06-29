// App-wide brand chrome. SwiftUI has no single switch to put a custom font on every navigation
// title, so the iOS large/inline nav titles are themed through UINavigationBar's appearance proxy
// (set once at launch). Backgrounds are left at the system defaults — transparent at the scroll
// edge (the large title floats on the content), blurred once scrolled — so only the typeface
// changes: Geist, matching the cards and the website.

#if os(iOS)
import PunktfunkKit
import UIKit

enum BrandTheme {
    static func apply() {
        BrandFont.registerIfNeeded()

        let scrollEdge = UINavigationBarAppearance()
        scrollEdge.configureWithTransparentBackground()
        applyFonts(to: scrollEdge)

        let standard = UINavigationBarAppearance()
        standard.configureWithDefaultBackground()
        applyFonts(to: standard)

        let proxy = UINavigationBar.appearance()
        proxy.scrollEdgeAppearance = scrollEdge
        proxy.standardAppearance = standard
        proxy.compactAppearance = standard
    }

    /// Override only the title fonts; leave colors/backgrounds at the configured defaults.
    private static func applyFonts(to appearance: UINavigationBarAppearance) {
        if let large = UIFont(name: "Geist-Bold", size: 34) {
            appearance.largeTitleTextAttributes[.font] = large
        }
        if let inline = UIFont(name: "Geist-SemiBold", size: 17) {
            appearance.titleTextAttributes[.font] = inline
        }
    }
}
#endif
