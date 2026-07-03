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
    /// The unified latency stages (design/stats-unification.md), ms per 1 s window. `host+network`
    /// = capture→received, skew-corrected across machines via the connect-time clock offset: the
    /// stage-2 HUD shows its p50 in the equation line; the stage-1 fallback shows p50/p95 as its
    /// `capture→received` headline. `hostNetworkValid` is false until the first sample drains (and
    /// whenever no host frames arrived in the last interval). `hostNetworkSkewCorrected` = the host
    /// answered the skew handshake (the number is cross-machine valid, not just same-host).
    @Published var hostNetworkP50Ms = 0.0
    @Published var hostNetworkP95Ms = 0.0
    @Published var hostNetworkValid = false
    @Published var hostNetworkSkewCorrected = false
    /// End-to-end = capture→on-glass, measured directly per frame (never summed from the stages) —
    /// the HUD headline. Only the stage-2 presenter can stamp it (it owns decode + a
    /// CAMetalLayer/display-link present); stays invalid under stage-1, where the layer presents
    /// internally with no per-frame callback.
    @Published var endToEndP50Ms = 0.0
    @Published var endToEndP95Ms = 0.0
    @Published var endToEndValid = false
    @Published var endToEndSkewCorrected = false
    /// The client-local stage terms of the HUD's equation line (single clock, no skew; p50 only):
    /// decode = received→decoded, display = decoded→on-glass (ring wait + render + vsync — the
    /// term the stage-2 presenter exists to shorten).
    @Published var decodeP50Ms = 0.0
    @Published var decodeValid = false
    @Published var displayP50Ms = 0.0
    @Published var displayValid = false
    /// Unrecoverable network frame drops in the last window (FEC couldn't rebuild them) and their
    /// share of frames offered, `lost/(received+lost)`. The HUD hides the line while zero.
    @Published var lostFrames = 0
    @Published var lostPct = 0.0
    /// Mirrors StreamView's capture state (it owns the input capture; this drives the
    /// HUD's "click to capture" / "⌘⎋ releases" hint).
    @Published var mouseCaptured = false

    let meter = FrameMeter()
    /// Capture→received (the host+network stage), fed per AU at receipt by the stream view's
    /// onFrame — under both presenters.
    let latency = LatencyMeter()
    /// The stage-2 meters, passed to StreamView: end-to-end (capture→on-glass, stamped at
    /// present), decode (received→decoded), display (decoded→on-glass).
    let endToEnd = LatencyMeter()
    let decodeStage = LatencyMeter()
    let displayStage = LatencyMeter()
    /// Cumulative reassembler-drop counter at the last stats drain (per-window `lost` delta).
    private var lastFramesDropped: UInt64 = 0
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
    ///
    /// `requestAccess` is the no-PIN delegated-approval path: open an identified connect the host
    /// PARKS until the operator clicks Approve in its console, then admits the SAME connection (no
    /// reconnect). The handshake budget is widened to exceed the host's park window, and a
    /// successful connect streams directly (the approval IS the trust decision) — the caller pins
    /// the observed fingerprint as paired. `host.pinnedSHA256`, when set, pins the advertised cert
    /// for the wait; nil = trust-on-first-use.
    func connect(to host: StoredHost, width: UInt32, height: UInt32, hz: UInt32,
                 compositor: PunktfunkConnection.Compositor = .auto,
                 gamepad: PunktfunkConnection.GamepadType = .auto,
                 bitrateKbps: UInt32 = 0,
                 audioChannels: UInt8 = 2,
                 hdrEnabled: Bool = true,
                 preferredCodec: UInt8 = 0,
                 launchID: String? = nil,
                 allowTofu: Bool = false,
                 autoTrust: Bool = false,
                 requestAccess: Bool = false) {
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
        // 4:4:4 opt-out (default on); the hardware-decode probe below is the real gate.
        let want444 = (UserDefaults.standard.object(forKey: DefaultsKey.enable444) as? Bool) ?? true
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main
            // actor. The persistent identity is presented on every connect so a paired
            // host recognizes this Mac (nil = anonymous, fine for hosts without
            // --require-pairing; Keychain/generation failure must not block connecting).
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            // Advertise 10-bit + HDR10 when enabled: the host upgrades to a BT.2020 PQ Main10 stream
            // only for actual HDR content (its own gate); the VideoToolbox/Metal present path is
            // HDR-capable (P010 + itur_2100_PQ + EDR). 0 keeps the 8-bit BT.709 SDR stream.
            var videoCaps: UInt8 = hdrCapable
                ? (PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCapHDR)
                : 0
            // Advertise full-chroma 4:4:4 only when allowed AND this device can HARDWARE-decode it
            // (software 4:4:4 is too slow for real-time). The host content-gates depth, so an
            // HDR-advertised session can still receive an 8-bit 4:4:4 stream (SDR content) — require
            // BOTH depths there. Otherwise a no-op (the host emits 4:4:4 only if it too opted in);
            // `chromaFormat` on the connection reflects what was actually resolved.
            let canDecode444 =
                hdrCapable
                ? (Stage444Probe.hwDecode444_8bit && Stage444Probe.hwDecode444_10bit)
                : Stage444Probe.hwDecode444_8bit
            if want444, canDecode444 {
                videoCaps |= PunktfunkConnection.videoCap444
            }
            // This client's VideoToolbox path decodes H.264 and HEVC (AV1 isn't wired — hosts don't
            // emit it on the native path yet). The host resolves the emitted codec from these + the
            // soft `preferredCodec`; `resolvedCodec` reflects what it chose.
            let videoCodecs = PunktfunkConnection.codecH264 | PunktfunkConnection.codecHEVC
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: width, height: height, refreshHz: hz,
                pinSHA256: pin, identity: identity, compositor: compositor,
                gamepad: gamepad, bitrateKbps: bitrateKbps, videoCaps: videoCaps,
                audioChannels: audioChannels,
                videoCodecs: videoCodecs, preferredCodec: preferredCodec, launchID: launchID,
                // Delegated approval: the host holds this connect open until the operator approves
                // it (~180 s) — outwait that window so a slow approval still lands here. Normal
                // connects keep the snappy default.
                timeoutMs: requestAccess ? 185_000 : 10_000) }
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
                    if pin != nil || autoTrust || requestAccess {
                        // requestAccess: the operator approved this device on the host, so the
                        // session is trusted — stream directly (the caller pins it as paired).
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
                    if requestAccess {
                        // The delegated-approval connect ended without being admitted: the
                        // operator didn't approve it before the host's park window elapsed (or
                        // the host was unreachable).
                        self.errorMessage = "\(host.displayName) didn't let this device in. "
                            + "Approve it in the host's web console (port 3000 → Pairing), then "
                            + "request access again — the request expires after a few minutes."
                    } else {
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
        hostNetworkValid = false
        endToEndValid = false
        decodeValid = false
        displayValid = false
        lostFrames = 0
        lostPct = 0
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
        lastFramesDropped = 0 // a fresh connection's cumulative drop counter starts at 0
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                let (frames, bytes, total) = self.meter.drain()
                self.fps = frames
                self.mbps = Double(bytes) * 8 / 1_000_000
                self.totalFrames = total
                // Per-window `lost` = the delta of the connector's cumulative reassembler-drop
                // counter (0 after close — treat a rewind as no loss rather than underflowing).
                let dropped = self.connection?.framesDropped() ?? 0
                let lost = dropped >= self.lastFramesDropped
                    ? Int(dropped - self.lastFramesDropped) : 0
                self.lastFramesDropped = dropped
                self.lostFrames = lost
                self.lostPct = lost > 0 ? Double(lost) / Double(frames + lost) * 100 : 0
                if let lat = self.latency.drain() {
                    self.hostNetworkP50Ms = lat.p50Ms
                    self.hostNetworkP95Ms = lat.p95Ms
                    self.hostNetworkSkewCorrected = lat.skewCorrected
                    self.hostNetworkValid = true
                } else {
                    self.hostNetworkValid = false
                }
                if let e = self.endToEnd.drain() {
                    self.endToEndP50Ms = e.p50Ms
                    self.endToEndP95Ms = e.p95Ms
                    self.endToEndSkewCorrected = e.skewCorrected
                    self.endToEndValid = true
                } else {
                    self.endToEndValid = false
                }
                if let d = self.decodeStage.drain() {
                    self.decodeP50Ms = d.p50Ms
                    self.decodeValid = true
                } else {
                    self.decodeValid = false
                }
                if let d = self.displayStage.drain() {
                    self.displayP50Ms = d.p50Ms
                    self.displayValid = true
                } else {
                    self.displayValid = false
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }
}
