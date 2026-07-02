// SettingsView's navigation and presentation helpers: the iOS settings categories, the iPad
// sheet sizing, and the bounded-slider clamp.

import SwiftUI

#if os(iOS)
/// The settings groups, mirroring the macOS preference tabs. On iPad each is a sidebar row that
/// drives the detail pane; on iPhone the same list collapses to pushed sub-pages. Internal (not
/// private) so the screenshot harness can open SettingsView on a specific category.
enum SettingsCategory: String, CaseIterable, Identifiable {
    case general, display, audio, controllers, advanced, about

    var id: Self { self }

    var title: String {
        switch self {
        case .general: return "General"
        case .display: return "Display"
        case .audio: return "Audio"
        case .controllers: return "Controllers"
        case .advanced: return "Advanced"
        case .about: return "About"
        }
    }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .display: return "display"
        case .audio: return "speaker.wave.2"
        case .controllers: return "gamecontroller"
        case .advanced: return "slider.horizontal.3"
        case .about: return "info.circle"
        }
    }
}

extension View {
    /// Present the settings sheet large on iPad so the NavigationSplitView has room for its
    /// sidebar + detail — a default form sheet is too narrow and the split view would collapse to
    /// the iPhone push list. No-op on iPhone (the standard sheet is already right) and on iOS 17
    /// (no `presentationSizing` — it falls back to the default sheet, which still degrades cleanly
    /// to the push list).
    @ViewBuilder
    func settingsSheetSizing() -> some View {
        if UIDevice.current.userInterfaceIdiom == .pad, #available(iOS 18, *) {
            presentationSizing(.page)
        } else {
            self
        }
    }
}
#endif

extension Double {
    /// The log-scale slider mapping needs a bounded input (Automatic stores 0).
    func clamped(_ lo: Double, _ hi: Double) -> Double {
        Swift.min(Swift.max(self, lo), hi)
    }
}
