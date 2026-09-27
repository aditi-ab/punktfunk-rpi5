// visionOS: the stream on a cinema screen in an immersive space, and the window chrome that
// sends it there. The screen is a VideoPlayerComponent, so the system spills its light into the
// room and tints passthrough to match; a mirrored copy under a dark glossy floor reflects it.
// Both draw from the session's decoded frames (TheaterRenderers) — no second decode.

#if os(visionOS)
import PunktfunkKit
import RealityKit
import SwiftUI

/// The app's one theater — visionOS opens one immersive space at a time — and the stream in it.
@MainActor @Observable
final class TheaterStage {
    static let shared = TheaterStage()
    static let spaceID = "theater"
    /// The Digital Crown reveals the room from 20 %; the theater opens fully dark.
    static let style = ProgressiveImmersionStyle(immersion: 0.2...1.0, initialAmount: 1.0)

    /// The renderers the screen shows while the space is open.
    private(set) var renderers: TheaterRenderers?
    /// The connection presenting into them.
    private(set) var owner: ObjectIdentifier?

    /// Point the theater at `connection`. A stream that held it goes back to its window.
    func enter(_ connection: PunktfunkConnection) {
        owner = ObjectIdentifier(connection)
        renderers = TheaterRenderers()
    }

    func leave() {
        owner = nil
        renderers = nil
    }

    func renderers(for connection: PunktfunkConnection) -> TheaterRenderers? {
        owner == ObjectIdentifier(connection) ? renderers : nil
    }
}

/// The immersive space's content.
struct TheaterView: View {
    @State private var scene = TheaterScene()

    var body: some View {
        RealityView { content in
            content.add(scene.root)
            scene.updates = content.subscribe(to: SceneEvents.Update.self) { [scene] _ in
                scene.fitScreen()
            }
        } update: { _ in
            scene.bind(TheaterStage.shared.renderers)
        }
        .onDisappear { TheaterStage.shared.leave() }
    }
}

/// The screen, its reflection and the floor between them.
@MainActor
final class TheaterScene {
    /// Screen width, its distance from the viewer and its lower edge above the floor, in metres.
    private static let width: Float = 6
    private static let distance: Float = 8
    private static let lift: Float = 0.4

    let root = Entity()
    /// The per-frame `fitScreen` call; dropping it cancels it.
    var updates: EventSubscription?
    private let screen = Entity()
    private let reflection = ModelEntity()
    private var bound: TheaterRenderers?
    private var fitted: SIMD2<Float> = .zero

    init() {
        var floor = PhysicallyBasedMaterial()
        floor.baseColor = .init(tint: .init(white: 0.02, alpha: 1))
        floor.roughness = 0.15
        // What lets the mirrored screen below show through as a reflection.
        floor.blending = .transparent(opacity: 0.8)
        root.addChild(ModelEntity(
            mesh: .generatePlane(width: 60, depth: 60), materials: [floor]))
        root.addChild(screen)
        root.addChild(reflection)
    }

    func bind(_ renderers: TheaterRenderers?) {
        guard renderers !== bound else { return }
        bound = renderers
        fitted = .zero
        guard let renderers else {
            screen.components.remove(VideoPlayerComponent.self)
            reflection.model = nil
            return
        }
        var player = VideoPlayerComponent(videoRenderer: renderers.screen)
        player.isPassthroughTintingEnabled = true
        screen.components.set(player)
        var mirror = VideoMaterial(videoRenderer: renderers.reflection)
        mirror.faceCulling = .none // the negative y scale that mirrors it flips its winding
        reflection.model = ModelComponent(
            mesh: .generatePlane(width: 1, height: 1), materials: [mirror])
    }

    /// Size the screen to `width` once the component knows the picture's aspect, and mirror
    /// the reflection across the floor. Runs every frame; returns early once fitted.
    func fitScreen() {
        guard let player = screen.components[VideoPlayerComponent.self] else { return }
        let size = player.playerScreenSize
        guard size.x > 0, size.y > 0, size != fitted else { return }
        fitted = size
        let scale = Self.width / size.x
        let height = size.y * scale
        let centre = Self.lift + height / 2
        screen.scale = [scale, scale, scale]
        screen.position = [0, centre, -Self.distance]
        reflection.scale = [Self.width, -height, 1]
        reflection.position = [0, -centre, -Self.distance]
    }
}

/// The stream window's controls, below it.
struct StreamOrnament: View {
    let connection: PunktfunkConnection
    let quickActions: () -> Void
    @Environment(\.openImmersiveSpace) private var openImmersiveSpace
    @Environment(\.dismissImmersiveSpace) private var dismissImmersiveSpace

    private var inTheater: Bool { TheaterStage.shared.renderers(for: connection) != nil }

    var body: some View {
        HStack(spacing: 12) {
            Button(action: quickActions) {
                Label("Quick Actions", systemImage: "ellipsis.circle")
            }
            if TheaterRenderers.supports(connection) {
                Button { Task { await toggleTheater() } } label: {
                    Label(
                        inTheater ? "Leave Theater" : "Theater",
                        systemImage: inTheater ? "rectangle.inset.filled" : "sparkles.tv")
                }
            }
            NewWindowButton()
        }
        .labelStyle(.iconOnly)
        .padding(12)
        .glassBackgroundEffect()
        // A stream that ends in the theater takes the theater down with it.
        .onDisappear {
            guard inTheater else { return }
            Task { await dismissImmersiveSpace() }
        }
    }

    private func toggleTheater() async {
        let stage = TheaterStage.shared
        if inTheater {
            await dismissImmersiveSpace()
            return
        }
        let open = stage.renderers != nil
        stage.enter(connection)
        if open { return } // another window's stream hands the open theater over
        switch await openImmersiveSpace(id: TheaterStage.spaceID) {
        case .opened: break
        default: stage.leave()
        }
    }
}

/// Covers a stream window whose picture is in the theater.
struct InTheaterPlaceholder: View {
    var body: some View {
        ContentUnavailableView(
            "Playing in the theater", systemImage: "sparkles.tv",
            description: Text("Controllers and the keyboard keep playing."))
    }
}

/// Opens another main window: its own host list and, once connected, its own stream.
struct NewWindowButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button { openWindow(id: PunktfunkClientApp.mainSceneID) } label: {
            Label("New Window", systemImage: "macwindow.badge.plus")
        }
    }
}
#endif
