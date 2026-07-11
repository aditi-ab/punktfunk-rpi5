// Resize-in-progress indicator state (design/midstream-resolution-resize.md — client UX).
//
// A mid-stream resize takes the host 0.3–2 s to rebuild its virtual display + encoder, and the
// first new-mode frame is an IDR that the decoder re-inits on. Rather than let the stream scale
// (stretch/blur) to the changing window during that gap, the client EMBRACES the delay: it shows a
// deliberate blur + spinner the instant a resize starts and clears it the instant the sharp
// new-resolution frame is on screen — so the wait reads as intentional, not as lag.
//
// This is driven ENTIRELY by signals the client already has (no new protocol):
//   * START — the Match-window follower reports the size it is steering toward (instant, on the
//     first resize layout, before the debounced request even leaves).
//   * END — the decode pipeline reports each new-mode IDR's dimensions; when they reach the target
//     the new picture is here.
//   * TIMEOUT — the safety net for a switch that never delivers the exact target: the host rejected
//     it (gamescope), capped it to an advertised mode, or a corrective ack landed a different size.
//
// Pure + side-effect-free so the transition logic is unit-tested without a live session or UI
// (`ResizeIndicatorTests`); `SessionModel` owns an instance and mirrors `active` into a @Published.

import Foundation

/// The pure state of the resize overlay. `now` is a monotonic time in seconds (the caller passes
/// `ProcessInfo.processInfo.systemUptime` or a test clock).
public struct ResizeIndicator {
    /// Whether the blur + spinner should be shown.
    public private(set) var active = false
    /// The size the follower is steering toward — cleared once a decoded frame reaches it.
    private var target: (width: UInt32, height: UInt32)?
    /// When the current `active` span began — the timeout is measured from here.
    private var since: TimeInterval?
    /// How long to keep the overlay up if the target frame never arrives (rejected / capped switch).
    public var timeout: TimeInterval

    public init(timeout: TimeInterval = 2.5) { self.timeout = timeout }

    /// The follower is steering toward `width`×`height` — a resize is under way. Show the overlay now
    /// (instant feedback). Called only for a genuine change (the follower skips a target equal to the
    /// live mode), possibly many times as a drag moves through sizes; the timeout re-arms whenever the
    /// target actually changes so a slow drag never trips it mid-gesture.
    public mutating func steering(width: UInt32, height: UInt32, now: TimeInterval) {
        if !active || target?.width != width || target?.height != height {
            since = now
        }
        target = (width, height)
        active = true
    }

    /// A decoded frame arrived at `width`×`height` (a new-mode IDR). Clears the overlay once it
    /// matches the steered target — the sharp new-resolution picture is on glass.
    public mutating func decoded(width: UInt32, height: UInt32) {
        guard active, let t = target, t.width == width, t.height == height else { return }
        active = false
        since = nil
    }

    /// Timeout safety net: stop showing the overlay once `timeout` has elapsed with no matching frame
    /// (a rejected or host-capped switch never delivers the exact target).
    public mutating func tick(now: TimeInterval) {
        guard active, let s = since, now - s >= timeout else { return }
        active = false
        since = nil
    }
}
