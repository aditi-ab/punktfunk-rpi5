// Where the player was in a host's library, so the round trip back from a stream doesn't lose it.
//
// Leaving a stream re-presents `LibraryView` from scratch — new `@State`, new ScrollView — so a
// library of any size came back at the top every time. For the loop this screen exists to serve
// (browse → play → quit → browse), that means re-scrolling to the same place on every lap. Field
// note, 2026-08-16: "when leaving the stream the client should stay at the last scroll position in
// the library view instead of jumping back".
//
// The position is remembered as the ID OF THE TITLE the player last opened, not as a pixel offset.
// An offset is meaningless across the things that legitimately change between visits — a rotation,
// a window resize, a Split View, a host that gained or lost titles, or the running-first ordering
// this view now applies. A title id survives all of them, and `scrollTo` turns it back into a
// position at whatever the current layout is.

import Foundation

/// Per-host "last title opened", in `UserDefaults`.
///
/// Small, non-sensitive and worth surviving a relaunch — the app being killed in the background
/// while a stream is up is exactly when this is most useful — so `UserDefaults` rather than an
/// in-memory cache. One key per host, namespaced so nothing else can collide with it.
enum LibraryScrollMemory {
    private static func key(forHost hostID: String) -> String {
        "punktfunk.library.lastTitle.\(hostID)"
    }

    /// The title last opened from this host's library, if any is remembered.
    static func last(forHost hostID: String) -> String? {
        UserDefaults.standard.string(forKey: key(forHost: hostID))
    }

    /// Remember a title as this host's position. Called when one is launched, which is the only
    /// moment the player is definitely leaving the grid for it.
    static func remember(_ gameID: String, forHost hostID: String) {
        UserDefaults.standard.set(gameID, forKey: key(forHost: hostID))
    }

    /// Forget a host's position — part of removing the host, so a forgotten host leaves no trace
    /// of what somebody was playing behind on the device.
    static func forget(hostID: String) {
        UserDefaults.standard.removeObject(forKey: key(forHost: hostID))
    }
}
