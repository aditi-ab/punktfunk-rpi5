// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

import Foundation
import PunktfunkKit
import SwiftUI

#if canImport(AppKit)
    import AppKit
#elseif canImport(UIKit)
    import UIKit
#endif

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
    /// Capture→client-receipt latency (ms), skew-corrected across machines via the connect-time
    /// clock offset — p50/p95 for the HUD. `latencyValid` is false until the first sample drains
    /// (and whenever no host frames arrived in the last interval). `latencySkewCorrected` = the host
    /// answered the skew handshake (the number is cross-machine valid, not just same-host).
    @Published var latencyP50Ms = 0.0
    @Published var latencyP95Ms = 0.0
    @Published var latencyValid = false
    @Published var latencySkewCorrected = false
    /// Capture→present (glass-to-glass, modulo the host render→capture term) — only the stage-2
    /// presenter can stamp this (it owns decode + a CAMetalLayer/display-link present). Stays
    /// invalid under stage-1, where the layer presents internally with no per-frame callback.
    @Published var presentLatencyP50Ms = 0.0
    @Published var presentLatencyP95Ms = 0.0
    @Published var presentLatencyValid = false
    @Published var presentLatencySkewCorrected = false
    /// Mirrors StreamView's capture state (it owns the input capture; this drives the
    /// HUD's "click to capture" / "⌘⎋ releases" hint).
    @Published var mouseCaptured = false

    let meter = FrameMeter()
    let latency = LatencyMeter()
    /// Fed by the stage-2 presenter's display link (capture→present). Passed to StreamView.
    let presentLatency = LatencyMeter()
    private var statsTimer: Timer?
    private var audio: SessionAudio?
    private var gamepadCapture: GamepadCapture?
    private var gamepadFeedback: GamepadFeedback?

    var isBusy: Bool { phase != .idle }

    /// `allowTofu` gates the trust-on-first-use prompt for an unpinned host: it is only true
    /// when the host EXPLICITLY advertised `pair=optional` (rule 3a). For any other unpinned host
    /// — `pair=required`, a manually-typed host, or a discovered host with no/unknown `pair`
    /// field — TOFU is forbidden (rule 3b): the connect refuses rather than offering trust, and
    /// the user is routed to PIN pairing by the caller. (A pinned host connects regardless: its
    /// stored fingerprint is the trust decision.)
    func connect(to host: StoredHost, width: UInt32, height: UInt32, hz: UInt32,
                 compositor: PunktfunkConnection.Compositor = .auto,
                 gamepad: PunktfunkConnection.GamepadType = .auto,
                 bitrateKbps: UInt32 = 0,
                 hdrEnabled: Bool = true,
                 launchID: String? = nil,
                 allowTofu: Bool = false,
                 autoTrust: Bool = false) {
        guard phase == .idle else { return }
        phase = .connecting
        activeHost = host
        errorMessage = nil
        let pin = host.pinnedSHA256
        // Capability gate (main-actor — screen APIs): only advertise HDR when this display can
        // actually present it, so the host sends a proper SDR stream to an SDR display rather than
        // BT.2020 PQ the panel would mis-tone-map. The display self-tone-maps HDR from the mastering
        // metadata we apply (Step 2) when it IS HDR.
        let displayHDR: Bool = {
            #if os(macOS)
                return (NSScreen.main?.maximumExtendedDynamicRangeColorComponentValue ?? 1.0) > 1.0
            #else
                return UIScreen.main.potentialEDRHeadroom > 1.0
            #endif
        }()
        let hdrCapable = hdrEnabled && displayHDR
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main
            // actor. The persistent identity is presented on every connect so a paired
            // host recognizes this Mac (nil = anonymous, fine for hosts without
            // --require-pairing; Keychain/generation failure must not block connecting).
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            // Advertise 10-bit + HDR10 when enabled: the host upgrades to a BT.2020 PQ Main10 stream
            // only for actual HDR content (its own gate); the VideoToolbox/Metal present path is
            // HDR-capable (P010 + itur_2100_PQ + EDR). 0 keeps the 8-bit BT.709 SDR stream.
            let videoCaps: UInt8 = hdrCapable
                ? (PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCapHDR)
                : 0
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: width, height: height, refreshHz: hz,
                pinSHA256: pin, identity: identity, compositor: compositor,
                gamepad: gamepad, bitrateKbps: bitrateKbps, videoCaps: videoCaps,
                launchID: launchID) }
            await MainActor.run { [weak self] in
                guard let self else { return }
                // The user may have abandoned this attempt (window closed, another host
                // clicked) while the handshake was in flight — don't resurrect a session
                // for a dead window, and especially don't start its mic uplink.
                guard self.phase == .connecting, self.activeHost?.id == host.id else {
                    if case .success(let conn) = result {
                        Task.detached { conn.close() } // joins Rust threads — off-main
                    }
                    return
                }
                switch result {
                case .success(let conn):
                    if pin != nil || autoTrust {
                        self.connection = conn
                        self.startStatsTimer()
                        self.beginStreaming()
                    } else if allowTofu {
                        // Host advertised pair=optional — offer the reduced-security TOFU prompt
                        // over the live (blurred) stream (rule 3a).
                        self.connection = conn
                        self.startStatsTimer()
                        self.phase = .awaitingTrust(fingerprint: conn.hostFingerprint)
                    } else {
                        // Unpinned and TOFU not permitted (rule 3b): never let this silently
                        // become trustable. Drop the connection; the caller routes to pairing.
                        Task.detached { conn.close() } // joins Rust threads — off-main
                        self.phase = .idle
                        self.activeHost = nil
                        self.errorMessage = "\(host.displayName) is not paired yet. "
                            + "Pair with its PIN before streaming."
                    }
                case .failure:
                    self.phase = .idle
                    self.activeHost = nil
                    self.errorMessage = pin != nil
                        ? "Could not connect to \(host.displayName) — host unreachable, "
                            + "not running, its identity no longer matches the pinned "
                            + "fingerprint, or it requires pairing and no longer "
                            + "recognizes this Mac (right-click the host card to pair "
                            + "again)."
                        : "Could not connect to \(host.displayName) — is punktfunk-host "
                            + "running on \(host.address):\(host.port)? If it requires "
                            + "pairing, right-click the host card and pair with its PIN "
                            + "first."
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
        statsTimer?.invalidate()
        statsTimer = nil
        let audio = self.audio
        self.audio = nil
        // Gamepad capture is main-actor (releases held buttons on the wire while the
        // connection is still up); the feedback drain joins off-main like audio.
        gamepadCapture?.stop()
        gamepadCapture = nil
        let feedback = gamepadFeedback
        gamepadFeedback = nil
        if let conn = connection {
            // Drain-thread teardown waits the pullers out and close() waits out in-flight
            // polls + joins the Rust worker threads — keep all of it off the main actor,
            // in this order (no poll left on any plane when the handle is freed).
            Task.detached {
                audio?.stop()
                feedback?.stop()
                conn.close()
            }
        } else {
            Task.detached {
                audio?.stop()
                feedback?.stop()
            }
        }
        connection = nil
        activeHost = nil
        phase = .idle
        fps = 0
        mbps = 0
        latencyValid = false
        mouseCaptured = false
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
        // Input capture itself is owned by StreamView (engaged by the captureEnabled
        // flip this phase change causes, released/re-engaged by the user from there).
        phase = .streaming
        // Audio starts with streaming, not during the trust prompt — no host sound (or
        // mic uplink!) before the user trusted the host. Devices come from Settings;
        // "" = system default.
        let defaults = UserDefaults.standard
        let audio = SessionAudio(connection: conn)
        audio.start(
            speakerUID: defaults.string(forKey: DefaultsKey.speakerUID) ?? "",
            micUID: defaults.string(forKey: DefaultsKey.micUID) ?? "",
            micEnabled: defaults.object(forKey: DefaultsKey.micEnabled) as? Bool ?? true)
        self.audio = audio
        // Gamepads: forward GamepadManager's active controller as pad 0 and render the
        // host's feedback (rumble always; lightbar/player-LEDs/adaptive-triggers when the
        // session's virtual pad is a DualSense). Same trust gate as audio — nothing is
        // forwarded during the trust prompt.
        let capture = GamepadCapture(connection: conn, manager: .shared)
        capture.start()
        gamepadCapture = capture
        let feedback = GamepadFeedback(connection: conn, manager: .shared)
        feedback.start()
        gamepadFeedback = feedback
    }

    private func startStatsTimer() {
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                let (frames, bytes, total) = self.meter.drain()
                self.fps = frames
                self.mbps = Double(bytes) * 8 / 1_000_000
                self.totalFrames = total
                if let lat = self.latency.drain() {
                    self.latencyP50Ms = lat.p50Ms
                    self.latencyP95Ms = lat.p95Ms
                    self.latencySkewCorrected = lat.skewCorrected
                    self.latencyValid = true
                } else {
                    self.latencyValid = false
                }
                if let p = self.presentLatency.drain() {
                    self.presentLatencyP50Ms = p.p50Ms
                    self.presentLatencyP95Ms = p.p95Ms
                    self.presentLatencySkewCorrected = p.skewCorrected
                    self.presentLatencyValid = true
                } else {
                    self.presentLatencyValid = false
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }
}
