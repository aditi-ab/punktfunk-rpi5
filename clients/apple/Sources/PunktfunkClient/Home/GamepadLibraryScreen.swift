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
    let host: StoredHost
    let onLaunch: (String) -> Void
    let close: () -> Void
    var controllerActive = true

    /// `.compact` in a landscape phone window — tighter chrome, like every gamepad screen.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }

    var body: some View {
        LibraryView(
            store: store, host: host, onLaunch: onLaunch,
            onClose: close, controllerActive: controllerActive)
            .safeAreaInset(edge: .top, spacing: 0) {
                Text("\(host.displayName) — Library")
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                    .minimumScaleFactor(0.75)
                    .frame(maxWidth: .infinity)
                    .overlay(alignment: .trailing) { closeButton.padding(.trailing, 20) }
                    .padding(.top, gamepadTitleTopPadding(compact: compact))
                    .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
                    .background { GamepadTrayScrim(edge: .top) }
            }
            .gamepadPaletteInk()
    }

    /// Touch/click fallback for closing — the controller path is B (the coverflow's onDismiss),
    /// and it also covers the loading/error/empty states, which the coverflow (and its B) never
    /// mounts under. A hardware keyboard's Esc rides the cancel action.
    private var closeButton: some View {
        Button { close() } label: {
            Image(systemName: "xmark")
                .font(.system(size: GamepadFormMetrics.closeFont, weight: .semibold))
                .foregroundStyle(ink.fg)
                .frame(width: GamepadFormMetrics.closeSide, height: GamepadFormMetrics.closeSide)
                .consoleGlassBackground(Circle(), interactive: true)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .keyboardShortcut(.cancelAction)
        .accessibilityLabel("Close library")
    }
}
#endif
