// PunktfunkClient — the macOS client app (also runs unbundled via swift run).
// Hosts grid → trust-on-first-use → StreamView (AVSampleBufferDisplayLayer HEVC) + input.

#if os(macOS)
import AppKit
#endif
import SwiftUI

@main
struct PunktfunkClientApp: App {
    #if os(macOS)
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #endif

    var body: some Scene {
        WindowGroup("Punktfunk") {
            #if DEBUG
            // PUNKTFUNK_SHOT_SCENE=<name> → show that single mock-populated screen full-bleed for
            // the App Store screenshot capture (tools/screenshots.sh). Normal launch otherwise;
            // the whole path is absent from Release builds.
            if let scene = ScreenshotMode.requestedScene {
                ScreenshotHostView(scene: scene)
            } else {
                ContentView()
            }
            #else
            ContentView()
            #endif
        }
        // The Stream menu (Disconnect ⌘D, Show/Hide Statistics ⌘⇧S) — a real menu bar on
        // macOS, hardware-keyboard shortcuts on iPad. tvOS has neither.
        #if !os(tvOS)
        .commands { StreamCommands() }
        #endif
        #if os(macOS)
        Settings {
            SettingsView()
        }
        #endif
    }
}

#if os(macOS)
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // `swift run` launches an unbundled binary; promote it to a regular app so the
        // window fronts and receives keyboard/mouse focus (GameController needs focus).
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
#endif
