// The app's "Stream" menu (macOS menu bar + iPad hardware-keyboard shortcuts). These live at
// the Scene level so they keep working when the HUD overlay is hidden. The shortcuts are the
// CROSS-CLIENT set every punktfunk client reserves — Ctrl+Alt+Shift+Q (release the captured
// mouse) / +D (disconnect) / +S (stats) — and the menu is their discoverable surface on macOS
// (the Linux client has its GTK Shortcuts window, Windows its start-of-stream banner). While
// input is CAPTURED these key equivalents never reach the menu (the stream view swallows
// keys); InputCapture's monitor detects the same combos there and performs the same actions —
// the menu covers the released state and discoverability. The stats item cycles the shared
// `statsVerbosity` tier (off → compact → normal → detailed → off); ContentView reads the same
// @AppStorage and reacts.
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
    /// The connected host advertises `HOST_CAP_CLIPBOARD` (gates the Share Clipboard item —
    /// macOS-only UI, but the fact is platform-neutral).
    var clipboardAvailable: Bool
    /// Clipboard sync is live (host-acked) — drives the item's Stop/Share title.
    var clipboardOn: Bool
    var toggleClipboard: () -> Void
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

    var body: some Commands {
        CommandMenu("Stream") {
            // Through the shared cycle so it advances from the LIVE session's tier — a profile
            // that starts a session on Detailed must cycle to Off from here, not from whatever
            // the global default happens to be.
            Button("Cycle Statistics") { StatsVerbosity.cycle() }
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
            #if os(macOS)
            // Mid-session clipboard flip (design/clipboard-and-file-transfer.md §5.3). Greyed
            // when the host doesn't advertise the cap (older host / operator policy off).
            Button(session?.clipboardOn == true ? "Stop Sharing Clipboard" : "Share Clipboard") {
                session?.toggleClipboard()
            }
            .keyboardShortcut("c", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true || session?.clipboardAvailable != true)
            // Toggle the window's fullscreen. ⌃⌘F is the macOS-standard fullscreen combo; here it's
            // explicit so it's discoverable AND survives capture — while streaming the stream view
            // swallows keys, so InputCapture's monitor detects the same combo and posts the same
            // notification the key window's FullscreenController observes.
            Button("Toggle Fullscreen") {
                NotificationCenter.default.post(name: .punktfunkToggleFullscreen, object: nil)
            }
            .keyboardShortcut("f", modifiers: [.control, .command])
            #endif
            Divider()
            Button("Disconnect") { session?.disconnect() }
                .keyboardShortcut("d", modifiers: [.control, .option, .shift])
                .disabled(session?.isStreaming != true)
        }
    }
}
#endif
