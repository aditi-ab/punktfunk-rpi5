import PunktfunkKit
import SwiftUI

#if os(macOS)
import AppKit

/// Drives the hosting window in/out of fullscreen from SwiftUI state, and mirrors the window's
/// ACTUAL native-fullscreen state back into `isFullscreen` (the user can also toggle it with the
/// green button / ⌃⌘F). It toggles only on an `active` edge, and leaves only a fullscreen it entered.
///
/// It owns the stream's chrome too: a session hides the title bar, and the traffic lights show
/// only while the pointer is free. Native fullscreen stops below a camera housing, so a picture
/// that shows bigger over the whole panel (`SafeDisplay.coversHousing`) gets the PANEL fullscreen:
/// the window over `screen.frame` with the menu bar and Dock auto-hidden, `PanelFrame` lifting
/// AppKit's clamp. Its rounded corners sit on a black backdrop.
///
/// SwiftUI rebuilds this view at every home ⇄ stream switch, and the old instance's pending pass
/// still runs. So the edge and the ownership live in `edge`, which the window's root view owns
/// and every instance shares: the first pass to see an edge acts on it, the rest see no edge.
struct FullscreenController: NSViewRepresentable {
    let active: Bool
    /// The live session's mode in pixels, nil on the home screens.
    let stream: CGSize?
    /// Input is captured, so the traffic lights hide with the pointer.
    let captured: Bool
    @Binding var isFullscreen: Bool
    /// True only while the window is in a fullscreen THIS controller drove it into. A window the
    /// user fullscreened themselves is not ours to leave, and an alert deferred on "fullscreen"
    /// alone would never show there, since nothing is going to flip it back.
    @Binding var appDriven: Bool
    let edge: Edge

    /// One per window, held in `@State` by the view that mounts the controller.
    final class Edge {
        /// The last `active` value acted on. A mismatch alone never toggles, so a mid-session
        /// ⌃⌘F or green-button toggle stays put.
        var lastActive: Bool?
        /// Did WE put this window into fullscreen? Only then may we take it out.
        var droveEntry = false
        /// The stream `wantsPanel` was measured for.
        var stream: CGSize?
        var wantsPanel = false
        /// Native fullscreen is on its way out so the panel can take over.
        var panelAfterExit = false
        /// Native fullscreen is animating in; AppKit drops a toggle until it lands.
        var entering = false
        /// The window before the panel; non-nil while it covers the panel.
        var panel: (frame: NSRect, styleMask: NSWindow.StyleMask, backdrop: NSWindow)?
        /// The title bar before the session hid it.
        var chrome: (title: NSWindow.TitleVisibility, transparent: Bool)?
    }

    /// Holds the window's fullscreen-transition observers so they're rebound on a window change
    /// and removed on dismantle.
    final class Coordinator {
        var observers: [NSObjectProtocol] = []
        weak var observedWindow: NSWindow?
        deinit { observers.forEach(NotificationCenter.default.removeObserver(_:)) }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let want = active
        let stream = stream
        let captured = captured
        let isFullscreen = $isFullscreen
        let appDriven = $appDriven
        let edge = edge
        let coordinator = context.coordinator
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            observeTransitions(of: window, coordinator: coordinator)
            defer { Self.applyChrome(window, streaming: stream != nil, captured: captured, edge: edge) }
            let isFull = window.styleMask.contains(.fullScreen)
            if isFullscreen.wrappedValue != isFull { isFullscreen.wrappedValue = isFull }
            if edge.stream != stream {
                edge.stream = stream
                edge.wantsPanel = Self.coversHousing(window.screen, stream)
                if edge.panel != nil, !edge.wantsPanel {
                    // The home screens never live in the panel; "Always" falls back to native.
                    Self.exitPanel(window, edge)
                    if want { window.toggleFullScreen(nil) }
                } else if isFull, edge.wantsPanel, edge.droveEntry, !edge.panelAfterExit {
                    edge.panelAfterExit = true // didExit enters the panel
                    if !edge.entering { window.toggleFullScreen(nil) } // else didEnter leaves
                }
            }
            guard edge.lastActive != want else { return }
            edge.lastActive = want
            if want, !isFull, edge.panel == nil {
                if edge.wantsPanel { Self.enterPanel(window, edge) } else { window.toggleFullScreen(nil) }
                edge.droveEntry = true
            } else if !want, edge.droveEntry, edge.panel != nil {
                Self.exitPanel(window, edge)
                edge.droveEntry = false
            } else if !want, isFull, edge.droveEntry {
                window.toggleFullScreen(nil)
                edge.droveEntry = false
            } else if !want {
                // The session ended in a fullscreen the USER chose — leave the window in it.
                edge.droveEntry = false
            }
            if appDriven.wrappedValue != edge.droveEntry {
                appDriven.wrappedValue = edge.droveEntry
            }
        }
    }

    /// `isFullscreen` spans `willEnter` to `didExit`: the whole time a sheet on the window would
    /// make AppKit drop a toggle. A native fullscreen handed to the panel leaves once its entry
    /// has landed, and the panel takes over at `didExit`.
    private func observeTransitions(of window: NSWindow, coordinator: Coordinator) {
        guard coordinator.observedWindow !== window else { return }
        coordinator.observers.forEach(NotificationCenter.default.removeObserver(_:))
        coordinator.observers.removeAll()
        coordinator.observedWindow = window
        let isFullscreen = $isFullscreen
        let edge = edge
        let center = NotificationCenter.default
        coordinator.observers.append(center.addObserver(
            forName: NSWindow.willEnterFullScreenNotification, object: window, queue: .main
        ) { _ in
            isFullscreen.wrappedValue = true
            edge.entering = true
        })
        coordinator.observers.append(center.addObserver(
            forName: NSWindow.didEnterFullScreenNotification, object: window, queue: .main
        ) { [weak window] _ in
            edge.entering = false
            guard edge.panelAfterExit else { return }
            guard edge.wantsPanel else { edge.panelAfterExit = false; return }
            // Still inside the transition here; a toggle now is dropped.
            DispatchQueue.main.async { window?.toggleFullScreen(nil) }
        })
        coordinator.observers.append(center.addObserver(
            forName: NSWindow.didExitFullScreenNotification, object: window, queue: .main
        ) { [weak window] _ in
            isFullscreen.wrappedValue = false
            guard edge.panelAfterExit, let window else { return }
            edge.panelAfterExit = false
            // The stream may have ended during the exit animation.
            if edge.wantsPanel { Self.enterPanel(window, edge) } else { window.toggleFullScreen(nil) }
        })
        // The panel's presentation options are app-wide; a closing window hands them back.
        coordinator.observers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { [weak window] _ in
            if let window { Self.exitPanel(window, edge) }
        })
        // The Stream menu's "Toggle Fullscreen" (⌃⌘F) and InputCapture's captured-state interception
        // both post this; flip the KEY window only (posted app-wide, object nil). The transition
        // observers above then mirror the real state back into the binding.
        coordinator.observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkToggleFullscreen, object: nil, queue: .main
        ) { [weak window] _ in
            guard let window, window.isKeyWindow else { return }
            if edge.panel != nil {
                Self.exitPanel(window, edge)
            } else if edge.wantsPanel, !window.styleMask.contains(.fullScreen) {
                Self.enterPanel(window, edge)
            } else {
                window.toggleFullScreen(nil)
            }
        })
    }

    /// A session hides the title bar and runs the video under it. The traffic lights come back
    /// with a free pointer, never over the panel.
    private static func applyChrome(_ window: NSWindow, streaming: Bool, captured: Bool, edge: Edge) {
        if streaming {
            if edge.chrome == nil {
                edge.chrome = (window.titleVisibility, window.titlebarAppearsTransparent)
            }
            window.titleVisibility = .hidden
            window.titlebarAppearsTransparent = true
        } else if let chrome = edge.chrome {
            edge.chrome = nil
            window.titleVisibility = chrome.title
            window.titlebarAppearsTransparent = chrome.transparent
        }
        showButtons(window, !(streaming && (captured || edge.panel != nil)))
    }

    private static func showButtons(_ window: NSWindow, _ show: Bool) {
        for kind in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            window.standardWindowButton(kind)?.isHidden = !show
        }
    }

    /// Whether `stream` shows bigger over this screen's whole panel than below its housing.
    private static func coversHousing(_ screen: NSScreen?, _ stream: CGSize?) -> Bool {
        guard let screen, let stream, screen.safeAreaInsets.top > 0 else { return false }
        return SafeDisplay.coversHousing(
            content: (Int(stream.width), Int(stream.height)),
            panel: screen.panelPixelSize, safe: screen.notchSafePixelSize)
    }

    private static func enterPanel(_ window: NSWindow, _ edge: Edge) {
        guard edge.panel == nil, let screen = window.screen else { return }
        let backdrop = NSWindow(
            contentRect: screen.frame, styleMask: .borderless, backing: .buffered, defer: false)
        backdrop.isReleasedWhenClosed = false // `edge` owns it
        backdrop.backgroundColor = .black
        backdrop.ignoresMouseEvents = true
        backdrop.isExcludedFromWindowsMenu = true
        edge.panel = (window.frame, window.styleMask, backdrop)
        PanelFrame.windows.add(window)
        NSApp.presentationOptions = [.autoHideMenuBar, .autoHideDock]
        window.styleMask.remove(.resizable)
        window.isMovable = false
        window.hasShadow = false
        showButtons(window, false)
        window.setFrame(screen.frame, display: true)
        backdrop.order(.below, relativeTo: window.windowNumber)
    }

    private static func exitPanel(_ window: NSWindow, _ edge: Edge) {
        guard let panel = edge.panel else { return }
        edge.panel = nil
        PanelFrame.windows.remove(window)
        panel.backdrop.close()
        NSApp.presentationOptions = []
        window.styleMask = panel.styleMask
        window.isMovable = true
        window.hasShadow = true
        window.setFrame(panel.frame, display: true)
    }
}

/// AppKit's `constrainFrameRect(_:to:)` keeps a titled window below the camera housing, even with
/// the menu bar hidden. The windows listed here keep the frame they are given. Borderless escapes
/// the clamp too, but that style change re-hosts SwiftUI's view, which reads as the window closing.
private enum PanelFrame {
    static let windows: NSHashTable<NSWindow> = {
        let windows = NSHashTable<NSWindow>.weakObjects()
        typealias Constrain = @convention(c) (NSWindow, Selector, NSRect, NSScreen?) -> NSRect
        let sel = #selector(NSWindow.constrainFrameRect(_:to:))
        if let method = class_getInstanceMethod(NSWindow.self, sel) {
            let original = unsafeBitCast(method_getImplementation(method), to: Constrain.self)
            let answer: @convention(block) (NSWindow, NSRect, NSScreen?) -> NSRect = {
                window, rect, screen in
                windows.contains(window) ? rect : original(window, sel, rect, screen)
            }
            method_setImplementation(method, imp_implementationWithBlock(answer))
        }
        return windows
    }()
}
#endif
