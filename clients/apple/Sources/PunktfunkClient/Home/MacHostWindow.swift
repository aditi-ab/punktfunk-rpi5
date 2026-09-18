// The Mac's host page (design/apple-touch-ui-overhaul.md §4): a window per host, opened from a
// card's ⓘ or Host Details…, showing `HostSectionsView`. Streaming, browsing, waking and pairing
// belong to the main window, so the page hands those over and its window closes.

#if os(macOS)
import PunktfunkKit
import SwiftUI

/// Carries one request from a host window to a main window. The first main window to `take` it
/// acts on it; `mainWindows` says whether one is open to take it at all.
@MainActor
final class MacHostRouter: ObservableObject {
    static let shared = MacHostRouter()
    @Published private(set) var pending: HostPageRequest?
    var mainWindows = 0
    /// A busy window is opening a window for `pending`.
    private var opening = false

    func send(_ request: HostPageRequest) { pending = request }

    func take() -> HostPageRequest? {
        defer {
            pending = nil
            opening = false
        }
        return pending
    }

    /// Whether a busy window should open a window for `request`: nobody took it and no window
    /// is opening for it yet.
    func claimOpening(_ request: HostPageRequest) -> Bool {
        guard pending == request, !opening else { return false }
        opening = true
        return true
    }
}

struct MacHostWindow: View {
    static let sceneID = "host"
    let hostID: StoredHost.ID
    @ObservedObject var store: HostStore
    var section: HostSection = .overview
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        HostSectionsView(hostID: hostID, store: store, section: section) { request in
            let router = MacHostRouter.shared
            router.send(request)
            if router.mainWindows == 0 { openWindow(id: PunktfunkClientApp.mainSceneID) }
            dismiss()
        }
    }
}
#endif
