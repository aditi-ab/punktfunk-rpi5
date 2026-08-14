// App Store screenshot harness — the in-app "shot mode" root.
//
// Launched with PUNKTFUNK_SHOT_SCENE=<name> (one of ShotScenes.all), the app shows that single
// mock-populated scene full-bleed instead of ContentView, so the OS can screenshot the REAL,
// fully-rendered UI (materials, NavigationStack, glass — all the things ImageRenderer can't
// rasterize offscreen). tools/screenshots.sh drives one launch per scene per device.
//
// Capture per platform:
//   • iOS / tvOS simulator → `xcrun simctl io booted screenshot` (native pixels = exact size).
//   • macOS → `screencapture -l<windowID>` of the borderless capture window (the configurator
//     prints `PF_SHOT_WINDOW=<id>`), or the no-permission self-capture fallback
//     (PUNKTFUNK_SHOT_SELFCAPTURE=<dir> → cacheDisplay; renders the real hierarchy but, like all
//     non-window-server capture, omits material blur).
//
// Every screen prints `PF_SHOT_READY scene=<name>` to stdout once it has settled, so the driver
// can wait for layout instead of guessing with a fixed sleep.

#if DEBUG
import PunktfunkKit
import SwiftUI
#if os(macOS)
import AppKit
import ImageIO
#endif

@MainActor
enum ScreenshotMode {
    /// This process was launched to capture a screenshot. Cheap enough to consult from the
    /// stores' persistence paths (`HostStore` / `ProfileStore`), which must NOT write their
    /// mock contents back into a real user's App Group when the harness runs on a dev Mac.
    static var isActive: Bool {
        !(ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SCENE"] ?? "").isEmpty
    }

    /// The scene requested via PUNKTFUNK_SHOT_SCENE, or nil for a normal launch.
    static var requestedScene: ShotScene? {
        let name = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SCENE"] ?? ""
        guard !name.isEmpty else { return nil }
        return ShotScenes.all.first { $0.name == name }
    }
}

/// Full-bleed host for a single scene, with per-platform window sizing / orientation and a
/// readiness ping for the capture script.
struct ScreenshotHostView: View {
    let scene: ShotScene

    init(scene: ShotScene) {
        self.scene = scene
        // Pin the palette for the capture. The aurora screens read the LIVE `uiPalette` default,
        // and a reused Simulator (or a dev Mac) carries whatever was last picked there — the
        // Apple TV set once shipped out on a sunset palette that a test device had persisted.
        // Idempotent, and only ever runs in shot mode (this view exists behind that gate).
        UserDefaults.standard.set(
            ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_PALETTE"] ?? "violet",
            forKey: DefaultsKey.uiPalette)
    }
    #if os(iOS)
    @Environment(\.horizontalSizeClass) private var hSizeClass
    @Environment(\.verticalSizeClass) private var vSizeClass
    #endif

    /// The gamepad UI's form-metric tier, published here for the same reason ContentView does it:
    /// this harness mounts those screens DIRECTLY, with no ContentView in the tree, so without it
    /// an iPad capture renders every gamepad screen at iPhone scale — a capture that doesn't look
    /// like the app.
    private var gamepadMetrics: GamepadFormMetrics {
        #if os(iOS)
        .forWindow(h: hSizeClass, v: vSizeClass)
        #else
        .platformDefault
        #endif
    }

    var body: some View {
        scene.make()
            .environment(\.colorScheme, scene.colorScheme)
            .environment(\.gamepadMetrics, gamepadMetrics)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            // Black fills the display, but the SCENE keeps its safe area. Ignoring it wholesale
            // here pushed the stream hero's HUD under the Dynamic Island (the resolution/bitrate
            // line was unreadable in every 6.9" capture); scenes that genuinely want full bleed —
            // the streamed frame itself — ignore it themselves.
            .background(Color.black.ignoresSafeArea())
            #if os(macOS)
            .background(MacShotWindowConfigurator(scene: scene))
            #elseif os(iOS)
            .background(IOSOrientationConfigurator(orientation: scene.orientation))
            #endif
            .task {
                // Let layout + materials settle, then signal the driver.
                try? await Task.sleep(nanoseconds: 900_000_000)
                announceReady()
            }
    }

    private func announceReady() {
        print("PF_SHOT_READY scene=\(scene.name)")
        fflush(stdout)
        #if os(macOS)
        MacSelfCapture.captureIfRequested(scene: scene)
        #endif
    }
}

#if os(macOS)
/// Sizes the hosting window to the mac canvas, strips the title bar to a clean full-bleed
/// surface, and prints the CGWindowID for `screencapture -l`.
private struct MacShotWindowConfigurator: NSViewRepresentable {
    let scene: ShotScene

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async {
            guard let window = view.window, !context.coordinator.configured else { return }
            context.coordinator.configured = true
            // NavigationStack / Form / material chrome follow the WINDOW's appearance, not the
            // SwiftUI colorScheme — without this the dark scenes render on a light window (white
            // background, washed-out materials).
            window.appearance = NSAppearance(named: scene.colorScheme == .dark ? .darkAqua : .aqua)
            let size = ShotDevice.mac.points(scene.orientation)
            window.styleMask = [.titled, .fullSizeContentView]
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.isMovable = false
            for button in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
                window.standardWindowButton(button)?.isHidden = true
            }
            window.setContentSize(size)
            window.center()
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            print("PF_SHOT_WINDOW=\(window.windowNumber) scene=\(scene.name) "
                + "size=\(Int(size.width))x\(Int(size.height))pt")
            fflush(stdout)
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }
    final class Coordinator { var configured = false }
}

/// No-permission fallback: capture the window's view tree via cacheDisplay. Renders the real
/// hierarchy (NavigationStack/Form/cards — unlike ImageRenderer) but omits material blur, which
/// only the window server (screencapture) composites. Used when PUNKTFUNK_SHOT_SELFCAPTURE is set.
enum MacSelfCapture {
    static func captureIfRequested(scene: ShotScene) {
        guard let dir = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SELFCAPTURE"],
              !dir.isEmpty,
              let window = NSApp.windows.first(where: { $0.isVisible }),
              let content = window.contentView else { return }
        let outDir = URL(fileURLWithPath: (dir as NSString).expandingTildeInPath, isDirectory: true)
        try? FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
        guard let rep = content.bitmapImageRepForCachingDisplay(in: content.bounds) else { return }
        content.cacheDisplay(in: content.bounds, to: rep)
        let url = outDir.appendingPathComponent("\(ShotDevice.mac.id)-\(scene.name).png")
        if let dest = CGImageDestinationCreateWithURL(
            url as CFURL, "public.png" as CFString, 1, nil), let cg = rep.cgImage {
            CGImageDestinationAddImage(dest, cg, nil)
            CGImageDestinationFinalize(dest)
            print("PF_SHOT_SAVED \(url.path) \(rep.pixelsWide)x\(rep.pixelsHigh)px")
        }
        fflush(stdout)
        exit(0)
    }
}
#endif

#if os(iOS)
/// Orientation lock for the requested scene (landscape for the stream hero, portrait for chrome).
/// Requires the app to allow those orientations in Info.plist — it does, for both.
private struct IOSOrientationConfigurator: UIViewControllerRepresentable {
    let orientation: ShotOrientation

    func makeUIViewController(context: Context) -> ShotOrientationController {
        ShotOrientationController(mask: mask)
    }

    func updateUIViewController(_ vc: ShotOrientationController, context: Context) {
        vc.mask = mask
        vc.applyGeometry()
    }

    private var mask: UIInterfaceOrientationMask {
        orientation == .landscape ? .landscapeRight : .portrait
    }
}

/// Asks the window scene to rotate, from a place where there IS a window.
///
/// The previous version made the request inside `updateUIViewController`, where `view.window` is
/// still nil: SwiftUI makes exactly one update pass for a representable mounted as a `.background`,
/// before the hierarchy is in a window, so the `guard` fell through and nothing ever asked again.
/// Every scene declared `.landscape` — the stream hero and the trust card — was therefore captured
/// in PORTRAIT at the portrait App Store size. Overriding `supportedInterfaceOrientations` as well
/// keeps the scene from rotating back if the simulator reports a device orientation change.
final class ShotOrientationController: UIViewController {
    var mask: UIInterfaceOrientationMask

    init(mask: UIInterfaceOrientationMask) {
        self.mask = mask
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask { mask }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        applyGeometry()
    }

    func applyGeometry() {
        // `view.window` once mounted; the connected-scene lookup covers the first update pass,
        // which still runs before this controller is in a window.
        let scene = view.window?.windowScene
            ?? UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first
        guard let scene else { return }
        // Report a refusal instead of silently shipping the wrong orientation — that is exactly
        // how every landscape scene went out as a portrait PNG for as long as it did.
        scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask)) { error in
            print("PF_SHOT_ORIENTATION_REFUSED \(error.localizedDescription)")
            fflush(stdout)
        }
        setNeedsUpdateOfSupportedInterfaceOrientations()
    }
}
#endif
#endif
