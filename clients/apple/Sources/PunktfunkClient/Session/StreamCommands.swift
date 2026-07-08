// The app's "Stream" menu (macOS menu bar + iPad hardware-keyboard shortcuts). These live at
// the Scene level so they keep working when the HUD overlay is hidden. The shortcuts are the
// CROSS-CLIENT set every punktfunk client reserves — Ctrl+Alt+Shift+Q (release the captured
// mouse) / +D (disconnect) / +S (stats) — and the menu is their discoverable surface on macOS
// (the Linux client has its GTK Shortcuts window, Windows its start-of-stream banner). While
// input is CAPTURED these key equivalents never reach the menu (the stream view swallows
// keys); InputCapture's monitor detects the same combos there and performs the same actions —
// the menu covers the released state and discoverability. The stats toggle just flips the
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
            .keyboardShortcut("s", modifiers: [.control, .option, .shift])
            // Reaches the key window's stream view via NotificationCenter — capture is view
            // state the Scene can't touch directly. (Captured, the combo is handled by
            // InputCapture's monitor before menus see it; this item is the released-state
            // path and the shortcut's menu-bar documentation.)
            Button("Release Mouse") {
                NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
            }
            .keyboardShortcut("q", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true)
            Divider()
            Button("Disconnect") { session?.disconnect() }
                .keyboardShortcut("d", modifiers: [.control, .option, .shift])
                .disabled(session?.isStreaming != true)
        }
    }
}
#endif
