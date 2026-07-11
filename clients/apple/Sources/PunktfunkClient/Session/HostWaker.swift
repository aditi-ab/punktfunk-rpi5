// Wake a sleeping host and WAIT for it to come back before proceeding.
//
// A magic packet is fire-and-forget, and a cold box can take 20–60 s to POST, boot, and start
// advertising on mDNS again — far longer than a connect attempt will sit. The old path fired a
// packet and immediately dialed, so a genuinely-asleep host just failed. This drives a visible
// "Waking…" state instead: it (re-)sends the packet, polls the host's mDNS presence once a second,
// and on success runs `onOnline` (the real connect for a Wake-&-Connect, or nothing for an explicit
// wake-only); on timeout it parks in a retry/cancel state. One wake at a time.

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
final class HostWaker: ObservableObject {
    struct Waking: Equatable {
        let hostID: UUID
        let hostName: String
        /// Whether coming online chains into a connect (Wake & Connect) vs. just stopping.
        let connectsAfter: Bool
        var seconds = 0
        var timedOut = false
    }

    /// nil = idle; non-nil drives the "Waking…" phase of `ConnectOverlay`.
    @Published private(set) var waking: Waking?

    /// How long to wait for the host to reappear before giving up. Generous — a cold boot + service
    /// start can be a minute-plus.
    private let timeoutSeconds = 90
    /// Re-send the packet this often: a single one can be missed, and some NICs only wake on a fresh
    /// packet after dropping into a deeper sleep state.
    private let resendEverySeconds = 6

    private var loop: Task<Void, Never>?
    /// Captured so "Try Again" replays the exact same wait.
    private var replay: (() -> Void)?

    /// Wake `host` and wait for `isOnline()` to go true, then run `onOnline`. `macs`/`lastIP` target
    /// the magic packet. No-ops straight to `onOnline` when there's nothing to wake with or the host
    /// is already up (a race between the caller's check and here).
    func start(
        host: StoredHost, connectsAfter: Bool,
        macs: [String], lastIP: String?,
        isOnline: @escaping () -> Bool, onOnline: @escaping () -> Void
    ) {
        guard !macs.isEmpty, !isOnline() else {
            cancel()
            onOnline()
            return
        }
        replay = { [weak self] in
            self?.run(host: host, connectsAfter: connectsAfter, macs: macs, lastIP: lastIP,
                      isOnline: isOnline, onOnline: onOnline)
        }
        replay?()
    }

    /// Stop waiting and dismiss the overlay (B / Cancel).
    func cancel() {
        loop?.cancel()
        loop = nil
        replay = nil
        waking = nil
    }

    /// Restart the wait after a timeout (A / Try Again).
    func retry() { replay?() }

    private func run(
        host: StoredHost, connectsAfter: Bool, macs: [String], lastIP: String?,
        isOnline: @escaping () -> Bool, onOnline: @escaping () -> Void
    ) {
        loop?.cancel()
        waking = Waking(hostID: host.id, hostName: host.displayName, connectsAfter: connectsAfter)
        let timeout = timeoutSeconds
        let resend = resendEverySeconds
        loop = Task { [weak self] in
            var elapsed = 0
            while !Task.isCancelled {
                if elapsed % resend == 0 { Self.sendPacket(macs: macs, lastIP: lastIP) }
                if isOnline() {
                    guard let self, !Task.isCancelled else { return }
                    self.waking = nil
                    self.loop = nil
                    onOnline()
                    return
                }
                if elapsed >= timeout {
                    self?.waking?.timedOut = true
                    self?.loop = nil
                    return
                }
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                elapsed += 1
                self?.waking?.seconds = elapsed
            }
        }
    }

    /// Blocking sends (see PunktfunkConnection.wakeOnLAN) — off the main thread.
    private static func sendPacket(macs: [String], lastIP: String?) {
        DispatchQueue.global(qos: .userInitiated).async {
            PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: lastIP)
        }
    }

    #if DEBUG
    /// Force a static waking state for the screenshot harness (no timers, no packets).
    func debugSet(_ w: Waking) { waking = w }
    #endif
}
