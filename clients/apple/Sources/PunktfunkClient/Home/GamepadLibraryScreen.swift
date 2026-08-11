// The library as one of the gamepad shell's in-place layers (iOS): console chrome — a pinned
// title and a close ✕ styled like the settings screen's — around the shared LibraryView, whose
// gamepad branch renders the coverflow. The cover presentation used to get its title and Close
// from the wrapping NavigationStack's bar; a shell layer has no bar, so this restores both in
// the console's own grammar. Everything data-shaped (the fetch, the loading/error/empty states,
// the image session lifecycle) stays LibraryView's.

import PunktfunkKit
import SwiftUI
#if os(iOS)

struct GamepadLibraryScreen: View {
    @Environment(\.gamepadInk) private var ink
    @ObservedObject var store: HostStore
    let target: LibraryTarget
    let onLaunch: (String) -> Void
    let close: () -> Void
    var controllerActive = true

    /// `.compact` in a landscape phone window — tighter chrome, like every gamepad screen.
    @Environment(\.verticalSizeClass) private var vSizeClass
    /// Resolves a pinned shelf's profile name for the title.
    @ObservedObject private var profiles = ProfileStore.shared

    private var compact: Bool { vSizeClass == .compact }

    var body: some View {
        LibraryView(
            store: store, target: target, onLaunch: onLaunch,
            onClose: close, controllerActive: controllerActive)
            .safeAreaInset(edge: .top, spacing: 0) {
                // Leading, like every gamepad heading — no close chrome, B is the exit (the
                // coverflow's, or LibraryView's own back-catcher before the coverflow exists).
                Text("\(target.title(in: profiles)) — Library")
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                    .minimumScaleFactor(0.75)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 24)
                    .padding(.top, gamepadTitleTopPadding(compact: compact))
                    .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
                    .background { GamepadTrayBlur(edge: .top) }
            }
            // A hardware keyboard's Esc still closes, without chrome.
            .background {
                Button("Close") { close() }
                    .keyboardShortcut(.cancelAction)
                    .buttonStyle(.plain)
                    .frame(width: 0, height: 0)
                    .opacity(0)
                    .accessibilityHidden(true)
            }
            .gamepadPaletteInk()
    }
}
#endif
