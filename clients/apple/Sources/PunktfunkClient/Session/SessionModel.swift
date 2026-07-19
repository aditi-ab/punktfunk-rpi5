// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

import Foundation
import os
import PunktfunkKit
import SwiftUI

#if canImport(AppKit)
    import AppKit
#elseif canImport(UIKit)
    import UIKit
#endif
#if os(tvOS)
    import AVFoundation // AVPlayer.eligibleForHDRPlayback — the TV-capability HDR gate
#endif

/// 1 Hz latency-stage line mirrored to the unified log so the stages can be read WITHOUT the
/// on-screen HUD (Console.app, wirelessly on an iPad/Apple TV). The HUD is not a neutral
/// instrument: any visible overlay forces the metal layer through the compositor, which costs a
/// refresh period on the vsync-latched platforms — this is how to measure with it off.
private let statsLog = Logger(subsystem: "io.unom.punktfunk", category: "stats")

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
    /// Phase 2 of the same stage: `host+network` split into its two terms via the host's per-AU
    /// 0xCF timing reports (host = capture→fully-sent as the host measured it, network = the
    /// remainder), matched to receipts by pts in `latencySplit`. `splitValid` is false whenever
    /// no timing matched in the window — an old host that never emits the plane, or heavy 0xCF
    /// loss — and the HUD then falls back to the combined `host+network` term.
    @Published var hostP50Ms = 0.0
    @Published var networkP50Ms = 0.0
    @Published var splitValid = false
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
    /// Resize overlay (design/midstream-resolution-resize.md — client resize UX): true from the
    /// instant a Match-window resize starts steering toward a new size until a frame at that size
    /// decodes (or a safety timeout). Drives the blur+spinner so the unavoidable host-rebuild delay
    /// reads as a deliberate, acknowledged transition instead of a stutter. Pure state lives in
    /// `ResizeIndicator`; this mirrors its `active` for SwiftUI.
    @Published private(set) var resizing = false
    /// START = follower steering (main actor), END = a new-mode IDR's coded dims (decode pump,
    /// hopped to main), TIMEOUT = safety net for a rejected/capped switch that never yields a
    /// differently-sized frame. Ticked from the 1 Hz stats timer.
    private var resizeIndicator = ResizeIndicator()

    let meter = FrameMeter()
    /// Capture→received (the host+network stage), fed per AU at receipt by the stream view's
    /// onFrame — under both presenters.
    let latency = LatencyMeter()
    /// The host/network split of that same stage: onFrame also records (pts, interval) receipts
    /// here, and the 1 s stats tick drains the connection's 0xCF host timings into it — under
    /// both presenters (the receipt path is presenter-independent).
    let latencySplit = HostNetworkSplitter()
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
    #if os(macOS)
    /// The live session's clipboard bridge (design/clipboard-and-file-transfer.md §5) — created
    /// by `beginStreaming` when the per-host toggle is on and the host advertises
    /// `HOST_CAP_CLIPBOARD`; stopped (off-main, drain joined) in `disconnect`.
    private var clipboardSync: ClipboardSync?
    #endif
    /// Whether clipboard sync is live (host-acked `ClipState.enabled`) — drives the Stream menu
    /// item's title and the settings footnote. Always false off-macOS.
    @Published private(set) var clipboardEnabled = false
    /// The host's last `ClipState.reason` (`CLIP_REASON_*`) — why an enable was refused
    /// (backend unavailable / policy disabled / …); 0 = OK.
    @Published private(set) var clipboardReason: UInt8 = 0
    #if os(tvOS)
    /// Siri Remote → host pointer while streaming (touch surface moves, press = left click,
    /// Play/Pause = right click) + the remote's deliberate exit (hold Back ≥ 1 s). See
    /// SiriRemotePointer — same trust gate/lifecycle as the gamepad capture above.
    private var remotePointer: SiriRemotePointer?
    #endif

    var isBusy: Bool { phase != .idle }

    /// True while a streaming session is running in the background under the opt-in keep-alive
    /// (audio plays, video dropped, timeout armed). Drives the Live Activity's stage/countdown (M3)
    /// and is cleared on foreground or teardown. iOS/iPadOS only in practice.
    @Published private(set) var isBackgrounded = false
    /// When the backgrounded keep-alive will auto-disconnect (nil unless backgrounded) — drives the
    /// Live Activity countdown. Set alongside `backgroundTimer`.
    @Published private(set) var backgroundDeadline: Date?
    /// Bounded auto-disconnect for a backgrounded keep-alive session. Fires on `.main`.
    private var backgroundTimer: DispatchSourceTimer?

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
    /// `onUnreachable`, when set, replaces the "could not connect" alert for a plain connect
    /// failure: the caller takes over recovery (the Wake-on-LAN wait for a host that stopped
    /// advertising). It never fires for the delegated-approval path, whose failure text carries
    /// its own instructions.
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
                 requestAccess: Bool = false,
                 onUnreachable: (@MainActor () -> Void)? = nil) {
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
                // POTENTIAL, not current, headroom: `maximumExtendedDynamicRangeColorComponentValue`
                // is the CURRENTLY-ALLOCATED headroom, which macOS hands out on demand — on an idle
                // SDR desktop it reads 1.0 even with HDR enabled and active (external HDR displays
                // like the Samsung G95SC allocate EDR only when content asks). Gating on it means an
                // HDR monitor never gets advertised at connect time. `maximumPotential…` is the
                // mode-independent capability (the macOS analogue of the tvOS/iOS gates below).
                return (NSScreen.main?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1.0) > 1.0
            #elseif os(tvOS)
                // NOT the EDR headroom here: on tvOS that reflects the CURRENT output mode, and
                // Apple's recommended setup runs an SDR home screen with Match Content — an
                // HDR-capable TV would read 1.0 at connect time and never be advertised. The
                // session switches the display to HDR10 itself once streaming (AVDisplayManager —
                // see StreamViewIOS), so gate on the TV's mode-independent capability; if the
                // switch never lands, the presenter's in-shader tone-map keeps PQ safe anyway.
                return AVPlayer.eligibleForHDRPlayback
            #else
                return UIScreen.main.potentialEDRHeadroom > 1.0
            #endif
        }()
        let hdrCapable = hdrEnabled && displayHDR
        // 4:4:4 opt-IN (default off): full chroma is a per-client choice — a clear win for
        // desktop/text work, but at a fixed bitrate it spends bits on chroma that game content
        // doesn't visibly need, and the encode/decode pixel rate rises. The host allows it by
        // default (PUNKTFUNK_444, default on), so this toggle is the one real switch; the
        // hardware-decode probe below still gates what can actually be advertised.
        let want444 = (UserDefaults.standard.object(forKey: DefaultsKey.enable444) as? Bool) ?? false
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
            // This client's VideoToolbox path decodes H.264 and HEVC everywhere, and AV1 when
            // this device has an AV1 hardware decoder (M3-class Macs, A17 Pro-class iPhones —
            // VideoToolbox has no software AV1 decoder, so advertising it elsewhere would invite
            // a stream that can't decode; see AV1.swift). The host resolves the emitted codec
            // from these + the soft `preferredCodec`; `resolvedCodec` reflects what it chose.
            var videoCodecs = PunktfunkConnection.codecH264 | PunktfunkConnection.codecHEVC
            if AV1.hardwareDecodeSupported { videoCodecs |= PunktfunkConnection.codecAV1 }
            // PyroWave (wired LAN) is a pure opt-in: picking it in the codec setting both
            // advertises the bit and prefers it — the host never auto-selects it, and the
            // picker only offers it when the Metal decode probe passed (simdgroup floor ≈ A13;
            // every M-series Mac and the ATV 4K gen 3 pass). The decoder self-configures from
            // the per-frame sequence header (4:2:0/4:4:4, SDR/PQ — design/pyrowave-444-hdr.md),
            // so the session keeps the user's HDR/10-bit/4:4:4 caps exactly like HEVC/AV1.
            if preferredCodec == PunktfunkConnection.codecPyroWave, MetalWaveletDecoder.supported {
                videoCodecs |= PunktfunkConnection.codecPyroWave
            }
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
                case .failure(let error):
                    self.phase = .idle
                    self.activeHost = nil
                    if case PunktfunkClientError.rejected(let rejection) = error {
                        // The host answered and stated its reason (declined / approval timed
                        // out / busy / versions differ) — show that, and never wake-retry a
                        // host that is demonstrably awake.
                        self.errorMessage = "\(host.displayName): \(rejection.userMessage)"
                    } else if let onUnreachable, !requestAccess {
                        // The caller owns recovery (wake-and-retry) — no error alert here; its
                        // own overlay explains what's happening.
                        onUnreachable()
                    } else if requestAccess {
                        // The delegated-approval connect ended without being admitted: the
                        // operator didn't approve it before the host's park window elapsed (or
                        // the host was unreachable).
                        self.errorMessage = "\(host.displayName) didn't let this device in. "
                            + "Approve it in the host's web console (port 47992 → Pairing), then "
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

    // MARK: - Background keep-alive (opt-in, iOS)

    /// Enter the backgrounded keep-alive state: keep audio playing, DROP video decode (no GPU work
    /// off-screen), mute the mic (privacy), and arm a bounded auto-disconnect. The caller
    /// (ContentView's scenePhase driver) gates this on the setting + `.streaming`; a no-op otherwise.
    /// The video-drop seam is read by both pumps every iteration (`connection.isVideoDropped`).
    func enterBackground(timeoutMinutes: Int) {
        guard phase == .streaming, let conn = connection, !isBackgrounded else { return }
        isBackgrounded = true
        conn.setVideoDropped(true)
        audio?.setMicMuted(true)
        // Non-deliberate on fire (keep the host linger) so a user who returns late reconnects fast,
        // exactly like today's network-drop path. min 1 minute guards a nonsense setting.
        let minutes = max(1, timeoutMinutes)
        backgroundDeadline = Date().addingTimeInterval(TimeInterval(minutes * 60))
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .seconds(minutes * 60))
        timer.setEventHandler { [weak self] in
            // The timer fires on `.main`, so the actor's executor is the main thread here.
            MainActor.assumeIsolated { self?.disconnect(deliberate: false) }
        }
        backgroundTimer?.cancel()
        backgroundTimer = timer
        timer.resume()
    }

    /// Return to foreground: cancel the timeout, resume mic + video, and force a clean re-anchor —
    /// request a fresh IDR (infinite GOP: it won't come on its own) and let the pump's freeze gate
    /// withhold the concealed frames until it lands (it auto-arms on the resumed frame-index gap).
    func exitBackground() {
        guard isBackgrounded else { return }
        isBackgrounded = false
        backgroundDeadline = nil
        backgroundTimer?.cancel()
        backgroundTimer = nil
        audio?.setMicMuted(false)
        if let conn = connection {
            conn.setVideoDropped(false)
            conn.requestKeyframe()
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

    /// Tear the session down. `deliberate` (the default) means a user-initiated quit — signal
    /// `disconnectQuit()` so the host skips the keep-alive linger; `sessionEnded()` (a host-ended /
    /// dropped session) passes `false` to leave the linger intact.
    func disconnect(deliberate: Bool = true) {
        statsTimer?.invalidate()
        statsTimer = nil
        // Drop any armed background keep-alive (incl. the timeout that just fired us).
        backgroundTimer?.cancel()
        backgroundTimer = nil
        isBackgrounded = false
        backgroundDeadline = nil
        let audio = self.audio
        self.audio = nil
        // Gamepad capture is main-actor (releases held buttons on the wire while the
        // connection is still up); the feedback drain joins off-main like audio.
        gamepadCapture?.stop()
        gamepadCapture = nil
        #if os(tvOS)
        remotePointer?.stop() // releases any held click while the connection is still up
        remotePointer = nil
        #endif
        let feedback = gamepadFeedback
        gamepadFeedback = nil
        #if os(macOS)
        let clipboard = clipboardSync
        clipboardSync = nil
        #endif
        clipboardEnabled = false
        clipboardReason = 0
        if let conn = connection {
            // Drain-thread teardown waits the pullers out and close() waits out in-flight
            // polls + joins the Rust worker threads — keep all of it off the main actor,
            // in this order (no poll left on any plane when the handle is freed).
            Task.detached {
                audio?.stop()
                feedback?.stop()
                #if os(macOS)
                clipboard?.stop() // disables sync on the wire while the connection is still up
                #endif
                // Deliberate user quit → tell the host to skip the keep-alive linger (must precede close).
                if deliberate { conn.disconnectQuit() }
                conn.close()
            }
        } else {
            Task.detached {
                audio?.stop()
                feedback?.stop()
                #if os(macOS)
                clipboard?.stop()
                #endif
            }
        }
        connection = nil
        activeHost = nil
        phase = .idle
        fps = 0
        mbps = 0
        hostNetworkValid = false
        splitValid = false
        endToEndValid = false
        decodeValid = false
        displayValid = false
        lostFrames = 0
        lostPct = 0
        mouseCaptured = false
        resizing = false
        resizeIndicator = ResizeIndicator() // no stale target/timer into the next session
    }

    /// Called (via the main actor) when the pump hits end-of-session.
    func sessionEnded() {
        guard connection != nil else { return }
        let name = activeHost?.displayName ?? "host"
        disconnect(deliberate: false) // host/network ended it — keep the linger for a reconnect
        errorMessage = "Session ended by \(name)."
    }

    /// Resize overlay START (main actor — from the Match-window follower's `onResizeTarget`): the
    /// window began differing from the live mode, so a `Reconfigure` toward `(width, height)` is
    /// imminent. Show the blur+spinner immediately, before the debounced request even leaves.
    func resizeTargeted(width: UInt32, height: UInt32) {
        resizeIndicator.steering(
            width: width, height: height, now: Date().timeIntervalSinceReferenceDate)
        resizing = resizeIndicator.active
    }

    /// Resize overlay END (main actor — hopped from the decode pump's `onDecodedSize`): a new-mode
    /// IDR decoded at `(width, height)`. Clears the overlay only when that matches the size we're
    /// steering to (a same-size loss-recovery IDR, or the initial connect IDR, is a no-op).
    func resizeDecoded(width: Int, height: Int) {
        resizeIndicator.decoded(width: UInt32(max(width, 0)), height: UInt32(max(height, 0)))
        resizing = resizeIndicator.active
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
            micChannel: defaults.integer(forKey: DefaultsKey.micChannel),
            micEnabled: defaults.object(forKey: DefaultsKey.micEnabled) as? Bool ?? true)
        self.audio = audio
        // Gamepads: forward every controller GamepadManager selected — each on its own wire pad
        // index (a pin forwards only one, Automatic forwards all) — and render the host's feedback
        // back to the pad it's addressed to (rumble always; lightbar/player-LEDs/adaptive-triggers
        // when a pad's virtual device is a DualSense). Same trust gate as audio — nothing is
        // forwarded during the trust prompt.
        let capture = GamepadCapture(connection: conn, manager: .shared)
        // The cross-client escape chord (hold L1+R1+Start+Select 1.5 s) — on tvOS the only
        // controller way out of a stream (B/Menu is swallowed during sessions; see ContentView).
        capture.onDisconnectRequest = { [weak self] in self?.disconnect() }
        capture.start()
        gamepadCapture = capture
        let feedback = GamepadFeedback(connection: conn, manager: .shared)
        feedback.start()
        gamepadFeedback = feedback
        #if os(macOS)
        // Shared clipboard: opt-in per host AND host-advertised (older hosts / operator-disabled
        // hosts never see a ClipControl). Same trust gate as audio — nothing is announced
        // during the trust prompt.
        if activeHost?.clipboardSync == true, conn.hostSupportsClipboard {
            startClipboardSync(conn)
        }
        #endif
        #if os(tvOS)
        let pointer = SiriRemotePointer(connection: conn)
        pointer.onDisconnectRequest = { [weak self] in self?.disconnect() }
        pointer.start()
        remotePointer = pointer
        #endif
    }

    #if os(macOS)
    /// Create + start the session's clipboard bridge and route its host acks into the published
    /// UI state. `ClipboardSync.start()` sends the enable; the host's `.state` answer flips
    /// `clipboardEnabled` (or leaves it false with a `clipboardReason` the UI can explain).
    private func startClipboardSync(_ conn: PunktfunkConnection) {
        let sync = ClipboardSync(connection: conn)
        sync.onState = { [weak self] enabled, _, reason in
            Task { @MainActor in
                self?.clipboardEnabled = enabled
                self?.clipboardReason = reason
            }
        }
        sync.start()
        clipboardSync = sync
    }
    #endif

    /// Flip clipboard sync mid-session (the Stream menu). Off → on requires the host cap; on →
    /// off tears the bridge down (off-main — the drain join must not block the main actor) and
    /// tells the host, which drops any selection we own there. No-op off-macOS or while idle.
    func toggleClipboardSync() {
        #if os(macOS)
        guard let conn = connection, phase == .streaming else { return }
        if let sync = clipboardSync {
            clipboardSync = nil
            clipboardEnabled = false
            clipboardReason = 0
            Task.detached { sync.stop() }
        } else if conn.hostSupportsClipboard {
            startClipboardSync(conn)
        }
        #endif
    }

    private func startStatsTimer() {
        lastFramesDropped = 0 // a fresh connection's cumulative drop counter starts at 0
        latencySplit.reset() // no stale receipts/samples from a previous session
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                // Resize-overlay safety net: clear a stuck overlay when a targeted size never
                // decodes (a rejected/capped switch). The decoded-frame END clears it promptly on
                // success; this only fires after the timeout.
                self.resizeIndicator.tick(now: Date().timeIntervalSinceReferenceDate)
                self.resizing = self.resizeIndicator.active
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
                // Phase 2: drain the window's per-AU host timings (0xCF) into the splitter —
                // non-blocking, bounded (a 240 fps window is ~240 reports; the cap only guards
                // a pathological burst). `try?` flattens (SE-0230); a throw (.closed during
                // teardown) just ends the drain. An old host never emits any → splitValid stays
                // false and the HUD keeps the combined host+network term.
                if let conn = self.connection {
                    var burst = 0
                    while burst < 1024, let t = try? conn.nextHostTiming(timeoutMs: 0) {
                        self.latencySplit.noteHostTiming(ptsNs: t.ptsNs, hostUs: t.hostUs)
                        burst += 1
                    }
                }
                if let s = self.latencySplit.drain() {
                    self.hostP50Ms = s.hostP50Ms
                    self.networkP50Ms = s.networkP50Ms
                    self.splitValid = true
                } else {
                    self.splitValid = false
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
                let displayWindow = self.displayStage.drain()
                if let d = displayWindow {
                    self.displayP50Ms = d.p50Ms
                    self.displayValid = true
                } else {
                    self.displayValid = false
                }
                // Mirror the window to the unified log (see statsLog) — one line per second,
                // stages in ms, only while frames actually flowed. `fps` counts RECEIVED AUs;
                // `presents` counts frames that reached glass (the display meter's sample count)
                // — a presents≪fps gap is the presenter dropping/serializing, an fps deficit is
                // upstream (host capture/encode or the network).
                if frames > 0 {
                    let line = String(
                        format: "fps=%d presents=%d e2e_p50=%.1f e2e_p95=%.1f hostnet_p50=%.1f "
                            + "decode_p50=%.1f display_p50=%.1f lost=%d",
                        frames,
                        displayWindow?.count ?? 0,
                        self.endToEndValid ? self.endToEndP50Ms : -1,
                        self.endToEndValid ? self.endToEndP95Ms : -1,
                        self.hostNetworkValid ? self.hostNetworkP50Ms : -1,
                        self.decodeValid ? self.decodeP50Ms : -1,
                        self.displayValid ? self.displayP50Ms : -1,
                        lost)
                    statsLog.info("\(line, privacy: .public)")
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }
}
