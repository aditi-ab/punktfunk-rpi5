// The app's "Stream" menu (macOS menu bar + iPad hardware-keyboard shortcuts). These live at
// the Scene level so they keep working when the HUD overlay is hidden — in particular ⌘D
// disconnect, which used to be reachable only via the HUD's button. The toggle just flips the
// shared `hudEnabled` setting; ContentView reads the same @AppStorage and reacts.
//
// tvOS has no menu bar / hardware-keyboard command surface (disconnect there is the Siri
// Remote's Menu button, handled by ContentView's `.onExitCommand`), so this whole file is
// non-tvOS only.

#if !os(tvOS)
import PunktfunkKit
import SwiftUI

/// The live session's menu-reachable actions, published by ContentView via
/// `.focusedSceneValue` so the Scene-level commands can drive it.
struct SessionFocus {
    var isStreaming: Bool
    var disconnect: () -> Void
}

private struct SessionFocusKey: FocusedValueKey {
    typealias Value = SessionFocus
}

extension FocusedValues {
    var sessionFocus: SessionFocus? {
        get { self[SessionFocusKey.self] }
        set { self[SessionFocusKey.self] = newValue }
    }
}

struct StreamCommands: Commands {
    @FocusedValue(\.sessionFocus) private var session
    @AppStorage(DefaultsKey.hudEnabled) private var hudEnabled = true

    var body: some Commands {
        CommandMenu("Stream") {
            Button(hudEnabled ? "Hide Statistics" : "Show Statistics") {
                hudEnabled.toggle()
            }
            .keyboardShortcut("s", modifiers: [.command, .shift])
            Divider()
            Button("Disconnect") { session?.disconnect() }
                .keyboardShortcut("d", modifiers: .command)
                .disabled(session?.isStreaming != true)
        }
    }
}
#endif
