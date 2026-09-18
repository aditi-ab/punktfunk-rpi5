// PunktfunkClient — the macOS client app (also runs unbundled via swift run).
// Hosts grid → trust-on-first-use → StreamView (AVSampleBufferDisplayLayer HEVC) + input.

#if os(macOS)
import AppKit
#elseif os(iOS)
import UIKit
#endif
import PunktfunkKit
import SwiftUI

@main
struct PunktfunkClientApp: App {
    /// The main window's scene, for a host window that finds none open.
    static let mainSceneID = "main"

    #if os(macOS)
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #elseif os(iOS)
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #endif

    init() {
        // Before anything touches the core, so its first lines (identity load, the first connect's
        // transport setup) land in the log ring "Send logs to host" uploads.
        CoreLog.install()
        #if os(iOS)
        // Put Geist on the navigation titles before any bar is built.
        BrandTheme.apply()
        #endif
    }

    var body: some Scene {
        WindowGroup("Punktfunk", id: Self.mainSceneID) {
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
            // NOT on tvOS: under the tvOS 26 glass button style a tinted UNFOCUSED control fills
            // AND labels itself in the tint — every plain Button/TextField renders as a blank
            // brand-violet pill until focused. Untinted, tvOS keeps the system glass look
            // (visible labels, white focus lift); brand color stays on explicit Color.brand uses.
            #if !os(tvOS)
            .tint(.brand)
            #endif
            // Geist Sans at each platform's own body size: the default for unstyled text, form
            // rows and fields. The phone's 17 pt shrank every tvOS control (29 pt there) and
            // bloated every Mac one (13 pt); views with an explicit size use `.geist(…)`.
            #if os(tvOS)
            .font(.geist(29, relativeTo: .body))
            #elseif os(macOS)
            .font(.geist(13, relativeTo: .body))
            #else
            .font(.geist(17, relativeTo: .body))
            #endif
        }
        // The Stream menu (Release Mouse ⌃⌥⇧Q, Disconnect ⌃⌥⇧D, Show/Hide Statistics ⌃⌥⇧S —
        // the cross-client Ctrl+Alt+Shift set) — a real menu bar on macOS, hardware-keyboard
        // shortcuts on iPad. tvOS has neither.
        #if os(macOS)
        .commands {
            StreamCommands()
            MacNavigationCommands()
        }
        #elseif !os(tvOS)
        .commands { StreamCommands() }
        #endif
        #if os(macOS)
        // A host's page, one window per host.
        WindowGroup("Host", id: MacHostWindow.sceneID, for: StoredHost.ID.self) { $hostID in
            if let hostID {
                MacHostWindow(hostID: hostID, store: .shared)
                    .tint(.brand)
            }
        }
        .defaultSize(width: 720, height: 540)
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
        // A second window opened over a fullscreen one would join it as a tab: hidden behind
        // the first stream, and AppKit throws when that tab enters fullscreen on its own.
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
#elseif os(iOS)
final class AppDelegate: NSObject, UIApplicationDelegate {
    /// An attached monitor's scene goes to `ExternalDisplaySceneDelegate`, so a stream can fill it
    /// instead of the letterboxed mirror. Every other scene stays SwiftUI's.
    func application(
        _ application: UIApplication,
        configurationForConnecting session: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let config = UISceneConfiguration(name: nil, sessionRole: session.role)
        if session.role == .windowExternalDisplayNonInteractive {
            config.delegateClass = ExternalDisplaySceneDelegate.self
        }
        return config
    }
}
#endif
