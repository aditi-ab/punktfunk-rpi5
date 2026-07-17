import PunktfunkKit
import SwiftUI

#if os(macOS)
import AppKit

/// Drives the hosting window in/out of native fullscreen from SwiftUI state, and mirrors the
/// window's ACTUAL fullscreen state back into `isFullscreen` (the user can also toggle it with the
/// green button / ⌃⌘F — ContentView keys the session view's safe-area handling off the real state,
/// not the setting). Mounted invisibly in the view tree; on each `active` change it captures the
/// window and toggles fullscreen only when the current state differs (so it never fights a toggle
/// already in flight, and never touches a window the user fullscreened manually unless `active`
/// says otherwise).
struct FullscreenController: NSViewRepresentable {
    let active: Bool
    @Binding var isFullscreen: Bool

    /// Holds the window's fullscreen-transition observers so they're rebound on a window change
    /// and removed on dismantle.
    final class Coordinator {
        var observers: [NSObjectProtocol] = []
        weak var observedWindow: NSWindow?
        /// The last `active` value we DROVE the window to. We toggle only when `active` itself
        /// changes (stream start/end) — never to correct a mismatch — so a deliberate mid-session
        /// toggle (⌃⌘F / the green button) isn't snapped back on the next SwiftUI update.
        var lastActive: Bool?
        deinit { observers.forEach(NotificationCenter.default.removeObserver(_:)) }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let want = active
        let isFullscreen = $isFullscreen
        let coordinator = context.coordinator
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            observeTransitions(of: window, coordinator: coordinator)
            let isFull = window.styleMask.contains(.fullScreen)
            if isFullscreen.wrappedValue != isFull { isFullscreen.wrappedValue = isFull }
            // Drive the window only on an `active` EDGE (stream start/end), not to close a mismatch —
            // so a user's ⌃⌘F / green-button toggle stays put. First pass (lastActive == nil) just
            // records the state without toggling, so mounting never yanks a window into fullscreen.
            if coordinator.lastActive != want {
                coordinator.lastActive = want
                if want != isFull { window.toggleFullScreen(nil) }
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
