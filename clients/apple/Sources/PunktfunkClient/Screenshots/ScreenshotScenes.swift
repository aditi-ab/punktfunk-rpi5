// App Store screenshot scenes — the actual screens we render, each wired with mock data so it
// looks populated without a live host. Every scene is built from the REAL app views (HomeView,
// SettingsView, PairSheet, TrustCardView) so the screenshots track the shipping UI; only the
// live stream is faked (StreamView needs a real punktfunk/1 connection — see ShotStreamHero).

#if DEBUG
import PunktfunkKit
import SwiftUI

/// One screen to capture: a name (→ file suffix), the canvas orientation, a color scheme, and a
/// factory that builds the populated view on the main actor.
struct ShotScene {
    let name: String
    let orientation: ShotOrientation
    let colorScheme: ColorScheme
    let make: @MainActor () -> AnyView
}

@MainActor
enum ShotScenes {
    static let all: [ShotScene] = [
        ShotScene(name: "01-stream", orientation: .landscape, colorScheme: .dark) {
            AnyView(ShotStreamHero())
        },
        ShotScene(name: "02-hosts", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotHome())
        },
        ShotScene(name: "03-pair", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotPair())
        },
        ShotScene(name: "04-trust", orientation: .landscape, colorScheme: .dark) {
            AnyView(ShotTrust())
        },
        ShotScene(name: "05-settings", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotSettings())
        },
    ]
}

// MARK: - Mock data

@MainActor
enum ShotMock {
    /// A populated saved-host grid: a pinned recent host, a couple more, mixed online state.
    static func hostStore() -> HostStore {
        let store = HostStore()
        store.hosts = [
            StoredHost(name: "Battlestation", address: "192.168.1.20", port: 9777,
                       pinnedSHA256: fingerprint, lastConnected: Date().addingTimeInterval(-420)),
            StoredHost(name: "Living Room PC", address: "192.168.1.41", port: 9777,
                       pinnedSHA256: fingerprint),
            StoredHost(name: "Workshop", address: "10.0.0.7", port: 9777),
        ]
        return store
    }

    static let host = StoredHost(name: "Battlestation", address: "192.168.1.20", port: 9777,
                                 pinnedSHA256: fingerprint)

    /// A plausible-looking 32-byte SHA-256 for the trust card / pin lock glyphs.
    static let fingerprint = Data((0..<32).map { UInt8(($0 &* 37 &+ 0x1d) & 0xff) })
}

// MARK: - Home

private struct ShotHome: View {
    @StateObject private var store = ShotMock.hostStore()
    @StateObject private var model = SessionModel()
    @StateObject private var discovery = HostDiscovery()

    var body: some View {
        #if os(macOS)
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: .constant(false), pairingTarget: .constant(nil),
            speedTestTarget: .constant(nil), libraryTarget: .constant(nil),
            connect: { _ in }, connectDiscovered: { _ in },
            onPaired: { _, _ in }, onLaunchTitle: { _, _ in })
        #else
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: .constant(false), pairingTarget: .constant(nil),
            speedTestTarget: .constant(nil), libraryTarget: .constant(nil),
            showSettings: .constant(false),
            connect: { _ in }, connectDiscovered: { _ in },
            onPaired: { _, _ in }, onLaunchTitle: { _, _ in })
        #endif
    }
}

// MARK: - Settings

private struct ShotSettings: View {
    var body: some View {
        #if os(macOS)
        // The mac Settings window is a fixed-size tabbed panel — float it over a dimmed host
        // grid so the shot reads as the preferences window over the running app.
        ZStack {
            ShotHome().blur(radius: 24).overlay(Color.black.opacity(0.45))
            SettingsView()
                .fixedSize()
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .shadow(radius: 40, y: 16)
        }
        #elseif os(iOS)
        NavigationStack {
            SettingsView()
                .navigationTitle("Settings")
                .navigationBarTitleDisplayMode(.inline)
        }
        #else
        NavigationStack { SettingsView() }
        #endif
    }
}

// MARK: - Pair (PIN ceremony)

private struct ShotPair: View {
    var body: some View {
        ZStack {
            ShotHome().blur(radius: 28).overlay(Color.black.opacity(0.5))
            PairSheet(host: ShotMock.host, onPaired: { _ in })
                .frame(maxWidth: 460)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 18))
                .clipShape(RoundedRectangle(cornerRadius: 18))
                .shadow(radius: 40, y: 16)
                .padding(40)
        }
    }
}

// MARK: - Trust (TOFU card over the blurred live stream)

private struct ShotTrust: View {
    var body: some View {
        ZStack {
            ShotDesktopFrame()
                .blur(radius: 32)
                .overlay(Color.black.opacity(0.45))
            TrustCardView(
                fingerprint: ShotMock.fingerprint, hostName: "Battlestation",
                onCancel: {}, onTrust: {}, onPairInstead: {})
        }
    }
}

// MARK: - Stream hero

/// The marketing hero: a stand-in streamed frame with the real glass HUD chip on top.
/// StreamView can't render here (it needs a live punktfunk/1 connection), so the frame is
/// synthetic — set `PUNKTFUNK_SHOT_HERO=/path/to/frame.png` to drop in a real captured frame.
private struct ShotStreamHero: View {
    var body: some View {
        ZStack(alignment: .topTrailing) {
            ShotDesktopFrame()
            ShotHUD()
        }
        .background(Color.black)
    }
}

/// A faithful copy of StreamHUDView's overlay (which needs a live PunktfunkConnection for the
/// mode line) with representative numbers, reusing the app's real `.glassBackground`.
private struct ShotHUD: View {
    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            HStack(spacing: 6) {
                Circle().fill(Color.accentColor).frame(width: 7, height: 7)
                Text("5120×1440@240  240 fps  812.4 Mb/s")
                    .font(.system(.caption, design: .monospaced))
            }
            Text("capture→client 1.3/2.1 ms p50/p95")
                .font(.system(.caption2, design: .monospaced))
                .foregroundStyle(.secondary)
            #if os(macOS)
            Text("⌘⎋ releases the mouse")
                .font(.caption2).foregroundStyle(.secondary)
            #elseif os(tvOS)
            Text("Press Menu to disconnect")
                .font(.caption).foregroundStyle(.secondary)
            #endif
        }
        .padding(10)
        .glassBackground(RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }
}

/// A synthetic "streamed frame" — a synthwave scene that reads as game content without shipping
/// any real art. Replaced wholesale when `PUNKTFUNK_SHOT_HERO` points at a real PNG.
private struct ShotDesktopFrame: View {
    var body: some View {
        if let image = Self.overrideImage {
            image.resizable().scaledToFill()
        } else {
            synthetic
        }
    }

    private var synthetic: some View {
        ZStack {
            LinearGradient(
                colors: [
                    Color(red: 0.05, green: 0.02, blue: 0.16),
                    Color(red: 0.35, green: 0.05, blue: 0.42),
                    Color(red: 0.95, green: 0.30, blue: 0.35),
                    Color(red: 0.99, green: 0.62, blue: 0.32),
                ],
                startPoint: .top, endPoint: .bottom)
            Canvas { ctx, size in
                let horizon = size.height * 0.52
                // Sun.
                let sunR = size.height * 0.20
                let sun = CGRect(x: size.width / 2 - sunR, y: horizon - sunR * 1.6,
                                 width: sunR * 2, height: sunR * 2)
                ctx.fill(Path(ellipseIn: sun),
                         with: .linearGradient(
                            Gradient(colors: [Color(red: 1, green: 0.95, blue: 0.5),
                                              Color(red: 1, green: 0.35, blue: 0.45)]),
                            startPoint: CGPoint(x: sun.midX, y: sun.minY),
                            endPoint: CGPoint(x: sun.midX, y: sun.maxY)))
                // Sun scanlines — clip a copy so the base context stays unclipped (GraphicsContext
                // is a value type; there is no resetClip).
                var sunCtx = ctx
                sunCtx.clip(to: Path(ellipseIn: sun))
                for i in 0..<7 {
                    let y = sun.minY + sun.height * (0.55 + Double(i) * 0.07)
                    let bar = CGRect(x: sun.minX, y: y, width: sun.width,
                                     height: sun.height * (0.012 + Double(i) * 0.006))
                    sunCtx.fill(Path(bar), with: .color(.black.opacity(0.85)))
                }
                // Perspective grid below the horizon.
                ctx.opacity = 0.55
                let cx = size.width / 2
                for col in -10...10 {
                    var p = Path()
                    p.move(to: CGPoint(x: cx, y: horizon))
                    p.addLine(to: CGPoint(x: cx + Double(col) * size.width * 0.11,
                                          y: size.height))
                    ctx.stroke(p, with: .color(Color(red: 0.6, green: 0.95, blue: 1)),
                               lineWidth: 1.5)
                }
                var row = horizon
                var step = size.height * 0.012
                while row < size.height {
                    var p = Path()
                    p.move(to: CGPoint(x: 0, y: row))
                    p.addLine(to: CGPoint(x: size.width, y: row))
                    ctx.stroke(p, with: .color(Color(red: 0.6, green: 0.95, blue: 1)),
                               lineWidth: 1.5)
                    step *= 1.32
                    row += step
                }
            }
        }
        .overlay(alignment: .bottomLeading) {
            // A small "now playing" chip so the frame reads as live content, not a wallpaper.
            HStack(spacing: 8) {
                Image(systemName: "gamecontroller.fill")
                Text("Streaming from Battlestation")
                    .font(.system(.callout, weight: .semibold))
            }
            .padding(.horizontal, 14).padding(.vertical, 9)
            .glassBackground(Capsule())
            .padding(18)
        }
        .ignoresSafeArea()
    }

    /// `PUNKTFUNK_SHOT_HERO=/abs/path.png` → use a real captured frame as the hero background.
    static var overrideImage: Image? {
        guard let path = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_HERO"],
              !path.isEmpty, FileManager.default.fileExists(atPath: path) else { return nil }
        #if os(macOS)
        guard let ns = NSImage(contentsOfFile: path) else { return nil }
        return Image(nsImage: ns)
        #else
        guard let ui = UIImage(contentsOfFile: path) else { return nil }
        return Image(uiImage: ui)
        #endif
    }
}
#endif
