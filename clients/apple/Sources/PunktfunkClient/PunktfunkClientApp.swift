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

    init() {
        #if os(iOS)
        // Put Geist on the navigation titles before any bar is built.
        BrandTheme.apply()
        #endif
    }

    var body: some Scene {
        WindowGroup("Punktfunk") {
            // Pin the whole app's tint to the brand purple explicitly — the asset-catalog accent
            // resolution is environment/timing-sensitive and can fall back to system blue. Wraps the
            // screenshot harness too, so captured screens are on-brand.
            Group {
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
            .tint(.brand)
            // Geist Sans is the app's typeface. This sets the default for unstyled text and the
            // form row labels; views that pick an explicit size/weight use `.geist(…)` directly.
            .font(.geist(17, relativeTo: .body))
        }
        // The Stream menu (Release Mouse ⌃⌥⇧Q, Disconnect ⌃⌥⇧D, Show/Hide Statistics ⌃⌥⇧S —
        // the cross-client Ctrl+Alt+Shift set) — a real menu bar on macOS, hardware-keyboard
        // shortcuts on iPad. tvOS has neither.
        #if !os(tvOS)
        .commands { StreamCommands() }
        #endif
        #if os(macOS)
        Settings {
            // A separate scene — `.tint` does not cross scene boundaries, so re-apply the brand
            // tint here or the Preferences window falls back to the (unreliable) asset accent.
            SettingsView()
                .tint(.brand)
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
