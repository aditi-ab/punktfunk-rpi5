// Match-window resize follower (design/midstream-resolution-resize.md D1/D2, client C3).
//
// The presenting view feeds this its PHYSICAL-PIXEL size on every layout; it debounces to
// resize-end, spaces requests ≥ 1 s apart, and asks the connection to switch the host's virtual
// display + encoder to match (`PunktfunkConnection.requestMode`) — so a windowed macOS session or
// an iPad Stage Manager / Split View scene streams native-resolution pixels instead of scaling.
// The decode/present side needs nothing: VideoToolbox recreates its session on the keyframe-derived
// format-description change (the first new-mode AU is an IDR with fresh parameter sets).
//
// The trigger discipline is the shared cross-client one (mirrors the session binary's
// `resize_decision`): physical pixels rounded DOWN to even (the host rejects odd dimensions) and
// clamped ≥ 320×200; debounce to resize-end; ≥ 1 s between requests; skip a size equal to the live
// mode; and request each distinct size at most once — which both stops re-asking a rejected size
// and keeps a host-side rollback (accepted, rebuild failed, corrective ack restored the old mode)
// from looping request → rollback → request.

import Foundation

/// The pure, side-effect-free core of the Match-window trigger — so the normalize/skip discipline
/// is unit-tested without a live connection or a UI (`MatchWindowTests`).
public enum MatchWindow {
    /// Even-floor + clamp a physical-pixel size to a host-valid mode dimension: the host's
    /// `validate_dimensions` rejects odd sizes, and we never ask below 320×200.
    public static func normalize(widthPx: Int, heightPx: Int) -> (width: UInt32, height: UInt32) {
        let evenClamp: (Int, UInt32) -> UInt32 = { px, minimum in
            let even = UInt32(max(px, 0)) / 2 * 2
            return max(even, minimum)
        }
        return (evenClamp(widthPx, 320), evenClamp(heightPx, 200))
    }

    /// Whether to request `target` now (the debounce has already settled; spacing is the caller's
    /// timer): `nil` to skip — equal to the live mode, or already requested once (a rejected size /
    /// a host rollback must not loop). `target` is expected already-[normalize]d.
    public static func request(
        target: (width: UInt32, height: UInt32),
        current: (width: UInt32, height: UInt32),
        lastRequested: (width: UInt32, height: UInt32)?
    ) -> (width: UInt32, height: UInt32)? {
        if target.width == current.width, target.height == current.height { return nil }
        if let lr = lastRequested, lr.width == target.width, lr.height == target.height { return nil }
        return target
    }
}

/// Owns the debounce timer + serialization state and drives `PunktfunkConnection.requestMode` from
/// the stream view's layout callbacks. Main-actor: the views feed it on the main thread and it reads
/// the connection's live mode there. Enabled per session from the `matchWindow` setting.
@MainActor
public final class MatchWindowFollower {
    private weak var connection: PunktfunkConnection?
    private let debounce: TimeInterval
    private let minSpacing: TimeInterval
    private var enabled: Bool

    private var work: DispatchWorkItem?
    private var pendingSize: (width: Int, height: Int)?
    private var lastRequested: (width: UInt32, height: UInt32)?
    private var lastRequestAt: Date?

    /// `debounce` = quiet time after the last size event before requesting (Win32 gets
    /// `WM_EXITSIZEMOVE` for free; we debounce). `minSpacing` = floor between accepted requests
    /// (a full host pipeline rebuild each). Defaults match the other clients.
    public init(
        connection: PunktfunkConnection,
        enabled: Bool,
        debounce: TimeInterval = 0.4,
        minSpacing: TimeInterval = 1.0
    ) {
        self.connection = connection
        self.enabled = enabled
        self.debounce = debounce
        self.minSpacing = minSpacing
    }

    /// Turn following on/off live (a mid-session settings change; off cancels a pending request).
    public func setEnabled(_ on: Bool) {
        enabled = on
        if !on {
            work?.cancel()
            work = nil
            pendingSize = nil
        }
    }

    /// Feed the presenting view's current PHYSICAL-PIXEL size (its `bounds` × the backing/display
    /// scale). Called from every layout pass; coalesced by the debounce so a drag-resize sends one
    /// request at its end, never one per frame.
    public func noteSize(widthPx: Int, heightPx: Int) {
        guard enabled else { return }
        pendingSize = (widthPx, heightPx)
        schedule()
    }

    private func schedule() {
        work?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.fire() }
        work = item
        DispatchQueue.main.asyncAfter(deadline: .now() + debounce, execute: item)
    }

    private func fire() {
        guard enabled, let connection, let size = pendingSize else { return }
        // ≥ 1 s spacing: a request went out recently → re-arm the debounce and retry later rather
        // than fire early (keeps at most ~one request outstanding — the accept ack round-trips in
        // milliseconds, ahead of the host's rebuild).
        if let last = lastRequestAt, Date().timeIntervalSince(last) < minSpacing {
            schedule()
            return
        }
        let target = MatchWindow.normalize(widthPx: size.width, heightPx: size.height)
        let mode = connection.currentMode()
        pendingSize = nil
        guard let req = MatchWindow.request(
            target: target,
            current: (mode.width, mode.height),
            lastRequested: lastRequested
        ) else { return }
        // Keep the current refresh — Match-window follows SIZE, not rate.
        connection.requestMode(width: req.width, height: req.height, refreshHz: mode.refreshHz)
        lastRequested = req
        lastRequestAt = Date()
    }
}
