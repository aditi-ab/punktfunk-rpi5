// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

import Foundation
import PunktfunkKit
import SwiftUI

/// Pump-thread-side frame counters; a 1 Hz main-actor timer drains them into @Published
/// values. NSLock instead of an actor — the writer is the (non-async) pump thread.
final class FrameMeter: @unchecked Sendable {
    private let lock = NSLock()
    private var frames = 0
    private var bytes = 0
    private var totalFrames = 0

    func note(byteCount: Int) {
        lock.lock()
        frames += 1
        bytes += byteCount
        totalFrames += 1
        lock.unlock()
    }

    /// Returns and resets the per-interval counters (the running total stays).
    func drain() -> (frames: Int, bytes: Int, total: Int) {
        lock.lock()
        defer {
            frames = 0
            bytes = 0
            lock.unlock()
        }
        return (frames, bytes, totalFrames)
    }
}

@MainActor
final class SessionModel: ObservableObject {
    enum Phase: Equatable {
        case idle
        case connecting
        /// Connected to an unpinned host: the stream is live (and pumping — the opening
        /// IDR must not be missed) but input/cursor capture wait for the user to confirm
        /// the observed fingerprint.
        case awaitingTrust(fingerprint: Data)
        case streaming
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var connection: PunktfunkConnection?
    /// The host this session is for (a value copy; identity = id).
    @Published private(set) var activeHost: StoredHost?
    @Published var errorMessage: String?
    @Published var fps = 0
    @Published var mbps = 0.0
    @Published var totalFrames = 0

    let meter = FrameMeter()
    private var inputCapture: InputCapture?
    private var statsTimer: Timer?

    var isBusy: Bool { phase != .idle }

    func connect(to host: StoredHost, width: UInt32, height: UInt32, hz: UInt32,
                 autoTrust: Bool = false) {
        guard phase == .idle else { return }
        phase = .connecting
        activeHost = host
        errorMessage = nil
        let pin = host.pinnedSHA256
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main actor.
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: width, height: height, refreshHz: hz,
                pinSHA256: pin) }
            await MainActor.run { [weak self] in
                guard let self else { return }
                switch result {
                case .success(let conn):
                    self.connection = conn
                    self.startStatsTimer()
                    if pin != nil || autoTrust {
                        self.beginStreaming()
                    } else {
                        self.phase = .awaitingTrust(fingerprint: conn.hostFingerprint)
                    }
                case .failure:
                    self.phase = .idle
                    self.activeHost = nil
                    self.errorMessage = pin != nil
                        ? "Could not connect to \(host.displayName) — host unreachable, "
                            + "not running, or its identity no longer matches the pinned "
                            + "fingerprint."
                        : "Could not connect to \(host.displayName) — is punktfunk-host "
                            + "running on \(host.address):\(host.port)?"
                }
            }
        }
    }

    /// The user confirmed the fingerprint: returns it for pinning and enters streaming.
    func confirmTrust() -> Data? {
        guard case .awaitingTrust(let fingerprint) = phase else { return nil }
        beginStreaming()
        return fingerprint
    }

    func rejectTrust() {
        disconnect()
    }

    func disconnect() {
        inputCapture?.stop()
        inputCapture = nil
        statsTimer?.invalidate()
        statsTimer = nil
        if let conn = connection {
            // close() waits out an in-flight poll (≤100 ms) and joins the Rust worker
            // threads — keep that off the main actor.
            Task.detached { conn.close() }
        }
        connection = nil
        activeHost = nil
        phase = .idle
        fps = 0
        mbps = 0
    }

    /// Called (via the main actor) when the pump hits end-of-session.
    func sessionEnded() {
        guard connection != nil else { return }
        let name = activeHost?.displayName ?? "host"
        disconnect()
        errorMessage = "Session ended by \(name)."
    }

    private func beginStreaming() {
        guard let conn = connection else { return }
        phase = .streaming
        let capture = InputCapture(connection: conn)
        capture.start()
        inputCapture = capture
    }

    private func startStatsTimer() {
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                let (frames, bytes, total) = self.meter.drain()
                self.fps = frames
                self.mbps = Double(bytes) * 8 / 1_000_000
                self.totalFrames = total
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }
}
