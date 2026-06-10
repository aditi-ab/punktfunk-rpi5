// Session state for the app shell: owns the connection, the input capture, and the
// pump-thread → main-actor stats relay.

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
    @Published var connection: PunktfunkConnection?
    @Published var connecting = false
    @Published var errorMessage: String?
    @Published var fps = 0
    @Published var mbps = 0.0
    @Published var totalFrames = 0

    let meter = FrameMeter()
    private var inputCapture: InputCapture?
    private var statsTimer: Timer?

    func connect(host: String, port: UInt16, width: UInt32, height: UInt32, hz: UInt32) {
        guard !connecting else { return }
        connecting = true
        errorMessage = nil
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main actor.
            let result = Result { try PunktfunkConnection(
                host: host, port: port, width: width, height: height, refreshHz: hz) }
            await MainActor.run { [weak self] in
                guard let self else { return }
                self.connecting = false
                switch result {
                case .success(let conn):
                    self.connection = conn
                    self.startInput(conn)
                    self.startStatsTimer()
                case .failure:
                    self.errorMessage = "Connection failed — is the host running? " +
                        "(punktfunk-host m3-host on \(host):\(port))"
                }
            }
        }
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
        fps = 0
        mbps = 0
    }

    /// Called (via the main actor) when the pump hits end-of-session.
    func sessionEnded() {
        guard connection != nil else { return }
        disconnect()
        errorMessage = "Session ended by host."
    }

    private func startInput(_ conn: PunktfunkConnection) {
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
