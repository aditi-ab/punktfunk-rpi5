// PunktfunkClient — the macOS client app (also runs unbundled via swift run).
// Hosts grid → trust-on-first-use → StreamView (AVSampleBufferDisplayLayer HEVC) + input.

import AppKit
import SwiftUI

@main
struct PunktfunkClientApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        WindowGroup("punktfunk") {
            ContentView()
        }
        Settings {
            SettingsView()
        }
    }
}

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
