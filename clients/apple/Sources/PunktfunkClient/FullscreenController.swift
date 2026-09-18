import PunktfunkKit
import SwiftUI

#if os(macOS)
import AppKit

/// Drives the hosting window in/out of native fullscreen from SwiftUI state, and mirrors the
/// window's ACTUAL fullscreen state back into `isFullscreen` (the user can also toggle it with the
/// green button / ⌃⌘F — ContentView keys the session view's safe-area handling off the real state,
/// not the setting). It toggles only on an `active` edge, and leaves only a fullscreen it entered.
///
/// SwiftUI rebuilds this view at every home ⇄ stream switch, and the old instance's pending pass
/// still runs. So the edge and the ownership live in `edge`, which the window's root view owns
/// and every instance shares: the first pass to see an edge acts on it, the rest see no edge.
struct FullscreenController: NSViewRepresentable {
    let active: Bool
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
        let isFullscreen = $isFullscreen
        let appDriven = $appDriven
        let edge = edge
        let coordinator = context.coordinator
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            observeTransitions(of: window, coordinator: coordinator)
            let isFull = window.styleMask.contains(.fullScreen)
            if isFullscreen.wrappedValue != isFull { isFullscreen.wrappedValue = isFull }
            guard edge.lastActive != want else { return }
            edge.lastActive = want
            if want, !isFull {
                window.toggleFullScreen(nil)
                edge.droveEntry = true
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

    /// `willEnter` (not did) so the video goes edge-to-edge while the title bar is already
    /// animating away; `didExit` so the top inset returns only once the title bar is back —
    /// no black gap in either direction.
    private func observeTransitions(of window: NSWindow, coordinator: Coordinator) {
        guard coordinator.observedWindow !== window else { return }
        coordinator.observers.forEach(NotificationCenter.default.removeObserver(_:))
        coordinator.observers.removeAll()
        coordinator.observedWindow = window
        let isFullscreen = $isFullscreen
        for (name, value) in [
            (NSWindow.willEnterFullScreenNotification, true),
            (NSWindow.didExitFullScreenNotification, false),
        ] {
            coordinator.observers.append(NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { _ in
                isFullscreen.wrappedValue = value
            })
        }
        // The Stream menu's "Toggle Fullscreen" (⌃⌘F) and InputCapture's captured-state interception
        // both post this; flip the KEY window only (posted app-wide, object nil). The transition
        // observers above then mirror the real state back into the binding.
        coordinator.observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkToggleFullscreen, object: nil, queue: .main
        ) { [weak window] _ in
            guard let window, window.isKeyWindow else { return }
            window.toggleFullScreen(nil)
        })
    }
}
#endif
