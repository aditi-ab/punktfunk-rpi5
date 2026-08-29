// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

// AVFoundation: AVCaptureDevice.authorizationStatus (the mic TCC grant behind `micAvailable`)
// and, on tvOS, AVPlayer.eligibleForHDRPlayback (the TV-capability HDR gate).
import AVFoundation
import Foundation
import os
import PunktfunkKit
import SwiftUI

#if canImport(AppKit)
    import AppKit
#elseif canImport(UIKit)
    import UIKit
#endif

/// 1 Hz latency-stage line mirrored to the unified log so the stages can be read WITHOUT the
/// on-screen HUD (Console.app, wirelessly on an iPad/Apple TV). The HUD is not a neutral
/// instrument: any visible overlay forces the metal layer through the compositor, which costs a
/// refresh period on the vsync-latched platforms — this is how to measure with it off.
private let statsLog = ClientLog(category: "stats")
/// The session's lifecycle — connect asked/landed/refused, how it ended. Until this existed a
/// client log bundle had a 1 Hz stats line and no sentence saying which host it was streaming
/// from, with what, or why it stopped; the host's own log has always said all three.
private let sessionLog = ClientLog(category: "session")
/// Mirror the 1 Hz vitals line to STDOUT as well as the unified log.
///
/// Exists for **tvOS, where the unified log is unreachable**: `log stream --device` is gone from
/// modern macOS, `log collect --device-name` needs root and then fails "Device not configured"
/// (an Apple TV has no USB to fall back to), and libimobiledevice pairs against a different
/// database than Xcode. Stdout, however, IS bridged — `xcrun devicectl device process launch
/// --console -e '{"PUNKTFUNK_STATS_STDOUT":"1"}' io.unom.punktfunk` streams these lines straight
/// to the Mac. That is the only way to read a session's numbers with the **stats overlay OFF**,
/// which matters because the overlay is itself a composited layer over the Metal one — i.e. a
/// plausible cause of the very present-floor inflation the overlay is used to measure.
/// Env-gated: no cost, and no stdout noise, unless someone is deliberately measuring.
private let statsToStdout = ProcessInfo.processInfo.environment["PUNKTFUNK_STATS_STDOUT"] == "1"

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
    /// The library entry this session was launched with (`connect(launchID:)`), or nil if the user
    /// just connected to the host's desktop. Kept because where the client should go when the
    /// session ends depends on where it came FROM: a title launched out of the library belongs back
    /// in that library when its game exits, not on the host-selection screen.
    private var launchedTitleID: String?
    /// WHICH library shelf that title was launched from — a host's own, or one of its pinned
    /// host+profile cards (§5.2a). The host alone would not answer it: a pinned card's shelf
    /// launches with that card's profile, so returning to the host's default shelf would quietly
    /// change what the next title streams with.
    private var launchedShelf: LibraryTarget?
    /// Set when a session ended because its game exited and it began as a library launch: the
    /// shelf to reopen. The view layer consumes it and sets it back to nil.
    @Published var returnToLibrary: LibraryTarget?
    /// The settings THIS session runs on — the globals with its profile overlaid, resolved once at
    /// connect (design/client-settings-profiles.md §4.2). Also mirrored into `SessionSettings` for
    /// the readers that live in PunktfunkKit and can't see this model.
    @Published private(set) var settings = EffectiveSettings()
    /// The stats-overlay tier for this session: the resolved one at connect, then whatever the
    /// live cycle surfaces (⌃⌥⇧S, the three-finger tap) move it to. Separate from the @AppStorage
    /// global so a profile that overrides the tier actually gets it, without the cycle breaking.
    @Published var statsVerbosity: StatsVerbosity = .normal
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
    /// Client-queue wait: core reassembly receipt → the pump's pull (`AccessUnit.pulledNs −
    /// receivedNs`, ABI v9 receipt split — the 2026-07 two-pair investigation). ~0 on a healthy
    /// stream; a persistent value is a client-side standing backlog that used to hide inside
    /// "network". Shown in the detailed tier only when it says something (≥ ~2 ms).
    @Published var clientQueueP50Ms = 0.0
    @Published var clientQueueValid = false
    /// The measured OS present floor (design/apple-presentation-rebuild.md): the deadline
    /// engine's vend→glass pipeline depth — an OS property no client can pace under (~2 refresh
    /// intervals composited; would read ~1 under direct-to-display). The HUD subtracts it from
    /// the shown display/e2e so the numbers describe Punktfunk's own pipeline; raw values stay
    /// in the detailed tier + the stats log. Invalid (0) on macOS arrival (sync-off ≈ no floor)
    /// and under stage-1.
    @Published var osFloorP50Ms = 0.0
    @Published var osFloorValid = false
    /// The deadline link's `preferredFrameLatency` ASK beside its property READBACK (see
    /// `PresentLinkInfo` — it exists because tvOS has no reachable log). ⚠ The readback is NOT
    /// a grant: it is a plain float property, so it echoes whatever was stored unless the
    /// system clamps the setter. readback ≠ ask ⇒ a visible clamp (the one signal the API can
    /// give); readback == ask proves nothing — `osFloorP50Ms` (the measured vend lead) is the
    /// truth-teller (field 2026-08-13: readback 1.00 beside a 32.5 ms floor).
    @Published var linkLatencyAskFrames: Float = 0
    @Published var linkLatencyFrames: Float = 0
    @Published var linkRangeMinHz: Float = 0
    @Published var linkRangeMaxHz: Float = 0
    @Published var linkDrawables = 0
    @Published var linkInfoValid = false
    /// Impossible samples the HOST-ANCHORED meters (host+network, end-to-end) refused this
    /// second (`LatencyMeter.drainTrimmed`). Nonzero means the clock offset is lying and every
    /// host-anchored p50/p95 this window is a TRUNCATED distribution — the HUD marks the window
    /// suspect instead of letting a trimmed tail pose as a healthy small number (the field
    /// "e2e 0–3 ms" reading, 2026-08-13). Client-local stages can't go negative, so they carry
    /// no such term.
    @Published var skewTrimPerS = 0
    /// The AUDIO plane's latency, from the playback ring (`SessionAudio.Stats`): how much decoded
    /// audio is queued ahead of the speaker, and where that PUTS it relative to the picture
    /// (positive = audio behind). `audioValid` is false until playback runs.
    ///
    /// Both numbers, never just the depth — a deep ring on a jittery link is the adaptive floor
    /// doing its job, and only the offset separates that from audio simply being held late. They
    /// existed nowhere a surface could render them until now, which is why a field report of "the
    /// audio delay seems way too high" was triaged all the way to a conclusion without them.
    @Published var audioBufferMs = 0
    @Published var audioAvOffsetMs = 0
    @Published var audioValid = false
    /// The audio format the host RESOLVED, for the HUD — `nil` on an ordinary Opus session, where
    /// there is nothing to say and a line saying "48 kHz" would be noise.
    ///
    /// The resolved format, emphatically not the requested one. A UI that reads "96 kHz" because
    /// the user picked 96 kHz, on a session the host declined, is the exact bug
    /// design/hi-res-audio.md §4.3 names wearing a different hat — and it is the one place a user
    /// would ever look to check that the bandwidth they are spending is buying anything.
    @Published var audioFormatLabel: String?

    /// A resolved sample rate as the kHz figure a listener recognises: `48`, `96`, and — since the
    /// 44.1 kHz family was admitted — `44.1`, `88.2`, `176.4`.
    ///
    /// ⚠ This exists because `rateHz / 1000` is INTEGER division, and a HUD reading "44 kHz" on a
    /// 44 100 Hz session would be the one surface whose whole job is naming what the host resolved,
    /// naming it wrong.
    ///
    /// Built from integer parts rather than `String(format: "%.1f", …)` for the reason the Android
    /// port records at its own copy of this: that formatter renders through the current locale and
    /// would print "44,1 kHz" across most of Europe, a decimal comma facing a settings row that
    /// says "44.1 kHz" — and this line exists precisely to be compared at a glance with what was
    /// asked for. Interpolating `Int`s is locale-independent, and every rate the plane carries is a
    /// whole number of hundreds of hertz, so the tenths digit is exact.
    private static func kHzLabel(_ rateHz: UInt32) -> String {
        let whole = rateHz / 1000
        let tenths = (rateHz % 1000) / 100 // 44 100 → 1, 176 400 → 4; 0 for the 48 kHz family
        return tenths == 0 ? "\(whole)" : "\(whole).\(tenths)"
    }

    /// The resolved speaker layout, spelled the way the settings row spells it. Named on this line
    /// because the lossless plane is no longer stereo-only: what a surround lossless session costs
    /// is three or four times the stereo figure, so "which layout did I actually get" is now part
    /// of "is the bandwidth I am spending buying anything".
    private static func layoutLabel(_ channels: UInt8) -> String {
        switch channels {
        case 6: return "5.1"
        case 8: return "7.1"
        default: return "stereo"
        }
    }

    /// The floor-shaved values every HUD tier displays (raw − floor, never below 0). Identical
    /// to the raw values whenever no floor is measured.
    var displayAdjP50Ms: Double { max(0, displayP50Ms - (osFloorValid ? osFloorP50Ms : 0)) }
    var endToEndAdjP50Ms: Double { max(0, endToEndP50Ms - (osFloorValid ? osFloorP50Ms : 0)) }
    var endToEndAdjP95Ms: Double { max(0, endToEndP95Ms - (osFloorValid ? osFloorP50Ms : 0)) }
    /// Unrecoverable network frame drops in the last window (FEC couldn't rebuild them) and their
    /// share of frames offered, `lost/(received+lost)`. The HUD hides the line while zero.
    @Published var lostFrames = 0
    @Published var lostPct = 0.0
    /// Mirrors StreamView's capture state (it owns the input capture; this drives the
    /// HUD's "click to capture" / "⌘⎋ releases" hint).
    @Published var mouseCaptured = false
    /// The USER's in-stream mic mute (the HUD button, the Stream menu's ⌃⌥⇧A, the captured-state
    /// chord, the iOS mic disc) — session state, deliberately NOT persisted: a mute is for the
    /// people in the room right now, so every new session starts live if the mic is on at all.
    /// One of the two inputs to the effective mute; `isBackgrounded` is the other, and
    /// `applyMicMute` composes them — a user mute survives a trip through the background, and the
    /// background's privacy mute never clears the user's choice. Local and instant: it gates
    /// capture on this device, nothing is sent to the host.
    @Published private(set) var micMuted = false
    /// The kind a controller declared when it turned out this session cannot carry its motion —
    /// set once per such pad, cleared after `motionHintSeconds`. Nil the rest of the time.
    ///
    /// It exists because the failure is otherwise entirely silent: the gyro simply does nothing,
    /// with no way for the player to tell a dead sensor from a session that resolved a backend
    /// without a motion plane. The fix is a settings change, so the hint has to name it.
    @Published private(set) var motionUnreachableKind: PunktfunkConnection.GamepadType?
    /// Drops `motionUnreachableKind` again — held so a second pad's hint replaces the first
    /// cleanly, and so ending the session cancels a pending clear rather than letting it fire
    /// into a torn-down model.
    private var motionHintTimer: Task<Void, Never>?
    /// How long the motion hint stays up — the start-of-stream shortcut banner's 6 s, since the
    /// two share the bottom-centre stack and a player reads them the same way.
    private static let motionHintSeconds: UInt64 = 6
    /// The touch model is passthrough, but this host drops contacts (no `HOST_CAP2_TOUCH`): the
    /// stream view runs the trackpad model instead, and this says so once, for
    /// `motionHintSeconds`, in the same bottom-centre slot. Otherwise the setting is silently
    /// ignored and every finger vanishes.
    @Published private(set) var touchFallbackNotice = false
    private var touchHintTimer: Task<Void, Never>?
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
    /// Client-queue sampler (see `clientQueueP50Ms`) — fed per AU by the stream view's onFrame,
    /// drained by the same 1 s tick as the stage meters.
    let clientQueue = LatencyMeter()
    /// The OS present floor sampler (see `osFloorP50Ms`) — fed one sample per display-link
    /// update by the deadline engine, drained by the same 1 s tick as the stage meters.
    let presentFloor = LatencyMeter()
    /// Cumulative reassembler-drop counter at the last stats drain (per-window `lost` delta).
    private var lastFramesDropped: UInt64 = 0
    private var statsTimer: Timer?
    private var audio: SessionAudio?
    private var gamepadCapture: GamepadCapture?
    private var gamepadFeedback: GamepadFeedback?
    #if !os(tvOS)
    /// The live session's clipboard bridge (design/clipboard-and-file-transfer.md §5) — created
    /// by `beginStreaming` when the per-host toggle is on and the host advertises
    /// `HOST_CAP_CLIPBOARD`; stopped (off-main, drain joined) in `disconnect`.
    private var clipboardSync: ClipboardSync?
    #endif
    /// Whether clipboard sync is live (host-acked `ClipState.enabled`) — drives the Stream menu
    /// item's title and the settings footnote. Always false on tvOS, which has no pasteboard.
    @Published private(set) var clipboardEnabled = false
    /// The host's last `ClipState.reason` (`CLIP_REASON_*`) — why an enable was refused
    /// (backend unavailable / policy disabled / …); 0 = OK.
    @Published private(set) var clipboardReason: UInt8 = 0

    // MARK: - Per-client access (design/per-client-access.md §7)

    /// The session's access preset, derived live from the grants mask (§3.2 — the label is
    /// never stored). `.fullControl` against every old host and for every full-grant device,
    /// so nothing below changes today's look there.
    @Published private(set) var accessLevel: PunktfunkConnection.AccessLevel = .fullControl
    /// Seconds until this session's access expires; `0` = permanent. Ticks down at the 1 Hz
    /// stats cadence — the chip's countdown renders straight from it.
    @Published private(set) var accessRemainingSecs: UInt32 = 0
    /// Anything about this session's access differs from full-and-permanent — the visibility
    /// gate for the chip (and the tvOS stats-overlay line). False = today's look, untouched.
    @Published private(set) var accessLimited = false
    /// The transient expiry-warning toast ("Access ends in 5 m") — non-nil for a few seconds
    /// around the T−5 m / T−1 m marks the host also warns at via `AccessUpdate`.
    @Published private(set) var accessWarning: String?
    /// One-shot latches for the two warning marks (reset per session).
    private var accessWarned5m = false
    private var accessWarned1m = false
    /// Auto-dismiss for `accessWarning` — held so a newer warning replaces a pending clear.
    private var accessWarningTimer: Task<Void, Never>?
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

    /// Holds off display sleep (and, on macOS, the screen saver) for the life of a session —
    /// nothing about watching a stream looks like user activity to the OS, least of all a
    /// controller-only session. Acquired in `beginStreaming`, released in `disconnect`.
    private let displaySleepGuard = DisplaySleepGuard()

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
    /// `effective` is the whole stream mode + input/audio configuration for this session, already
    /// resolved from the globals and the session's profile by the caller — the ONE place that
    /// resolution happens (§4.4). It is latched into `SessionSettings` here so the kit-side
    /// readers (the presenter, the input paths, the match-window follower) see the same values
    /// this connect asked the host for, instead of re-reading the globals mid-session.
    func connect(to host: StoredHost, effective: EffectiveSettings,
                 gamepad: PunktfunkConnection.GamepadType = .auto,
                 launchID: String? = nil,
                 /// The library shelf `launchID` was picked on, so a game exit can return to it.
                 /// Only meaningful alongside a `launchID`; nil for a plain desktop connect.
                 shelf: LibraryTarget? = nil,
                 allowTofu: Bool = false,
                 autoTrust: Bool = false,
                 requestAccess: Bool = false,
                 onUnreachable: (@MainActor () -> Void)? = nil) {
        guard phase == .idle else { return }
        phase = .connecting
        activeHost = host
        launchedTitleID = launchID
        launchedShelf = shelf
        errorMessage = nil
        settings = effective
        statsVerbosity = StatsVerbosity(rawValue: effective.statsVerbosity) ?? .normal
        SessionSettings.begin(effective)
        let mode = RenderScale.apply(
            baseWidth: effective.width, baseHeight: effective.height,
            scale: effective.renderScale,
            maxDimension: RenderScale.maxDimension(codec: effective.codec))
        let (width, height) = (mode.width, mode.height)
        let hz = UInt32(clamping: effective.refreshHz)
        let compositor = PunktfunkConnection.Compositor(
            rawValue: UInt32(clamping: effective.compositor)) ?? .auto
        var bitrateKbps = UInt32(clamping: effective.bitrateKbps)
        let audioChannels = UInt8(clamping: effective.audioChannels)
        // The audio format this session ASKS for — the user's choice, at every channel count.
        //
        // This used to be forced to Opus for 5.1/7.1, on the reasoning that a lossless surround
        // frame does not fit one datagram. That was a statement about ONE frame length: the ladder
        // is sized from `(rate, depth, channels, max_datagram)`, so a surround session negotiates a
        // shorter frame rather than failing, and only the top of the rate ladder has no rung that
        // fits. Deciding that here, from a rule this side cannot measure, meant a client guess
        // standing in for the host's measurement — and guessing "no" costs a session that would
        // have worked. The host's gate is the one place that knows the connection's real datagram
        // size; asking and being declined is one `Welcome` field, and it is the honest shape.
        //
        // **The request is never the answer.** `resolvedAudioRateHz`/`resolvedAudioBits`/
        // `isLosslessAudio` on the connection are what the host actually granted, SessionAudio
        // opens the device from THOSE, and `audioFormatLabel` below reports THOSE.
        let audioFormat = effective.audioFormatChoice
        let (audioRateHz, audioBits) = audioFormat.wire
        let hdrEnabled = effective.hdrEnabled
        let preferredCodec = PunktfunkConnection.codecByte(effective.codec)
        // PyroWave is always Automatic bitrate (ABR overhaul RFC §5.2): a fixed kbps is
        // ill-defined for the all-intra codec (bpp is the operating point) and used to bypass
        // the host's operator ceiling — send 0 and let the host pin its per-mode rate. Gated
        // like the advertisement below: a device that failed the Metal probe never offers the
        // codec, falls back to H.26x, and the user's rate must survive there. The stored
        // setting is untouched, so switching codecs back restores it.
        if preferredCodec == PunktfunkConnection.codecPyroWave, MetalWaveletDecoder.supported {
            bitrateKbps = 0
        }
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
        let want444 = effective.enable444
        let connectLine = "connect \(host.displayName) \(host.address):\(host.port) "
            + "mode=\(width)x\(height)@\(hz) codec=\(effective.codec) bitrate=\(bitrateKbps)kbps "
            + "hdr=\(hdrCapable) 444=\(want444) audio=\(audioChannels)ch/\(audioRateHz)Hz/\(audioBits)bit "
            + "pinned=\(pin != nil) tofu=\(allowTofu) launch=\(launchID ?? "-")"
        sessionLog.info("\(connectLine, privacy: .public)")
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
            // Cursor channel (remote-desktop-sweep M2, macOS): sessions STARTING in the desktop
            // mouse model advertise local cursor rendering — the host then stops compositing
            // the pointer and forwards shape/state, which StreamView draws as the real
            // NSCursor. Capture-mode sessions keep today's composited pointer.
            #if os(macOS)
            let presentCaps: UInt8 =
                (MouseInputMode(rawValue: effective.mouseMode) ?? .capture) == .desktop ? 0x01 : 0
            #else
            // iOS/tvOS run the stage-4 deadline presenter, whose link thread feeds
            // reportPhase — advertise the vsync-aware presenter (0x02, CLIENT_CAP_PHASE_LOCK).
            // macOS stays without it: the stage-2 arrival presenter has no latch grid.
            let presentCaps: UInt8 = 0x02
            #endif
            // "Keep host audio playing": the host taps its default playback device instead of
            // parking it on a silent endpoint, so the speakers on the host PC stay live. Pure
            // REQUEST — no host-cap echo — so an older host simply goes quiet as it always did.
            let clientCaps =
                presentCaps
                | (effective.keepHostAudio ? PunktfunkConnection.clientCapKeepHostAudio : 0)
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: width, height: height, refreshHz: hz,
                pinSHA256: pin, identity: identity, compositor: compositor,
                gamepad: gamepad, bitrateKbps: bitrateKbps, videoCaps: videoCaps,
                audioChannels: audioChannels,
                audioRateHz: audioRateHz, audioBits: audioBits,
                videoCodecs: videoCodecs, preferredCodec: preferredCodec,
                clientCaps: clientCaps, launchID: launchID,
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
                    // A LATER connect has already latched its own settings; only release the
                    // latch when nothing took over, or this would blank the live session's.
                    if self.phase == .idle { SessionSettings.end() }
                    return
                }
                switch result {
                case .success(let conn):
                    let landed = "connected \(host.displayName) "
                        + "mode=\(conn.width)x\(conn.height)@\(conn.refreshHz) "
                        + "codec=\(conn.videoCodec) bitrate=\(conn.resolvedBitrateKbps)kbps "
                        + "depth=\(conn.bitDepth) chroma=\(conn.isChroma444 ? "444" : "420") hdr=\(conn.isHDR) "
                        + "audio=\(conn.resolvedAudioChannels)ch/\(conn.resolvedAudioRateHz)Hz/\(conn.resolvedAudioBits)bit "
                        + "shard=\(conn.shardPayload) compositor=\(conn.resolvedCompositor.rawValue) "
                        + "gamepad=\(conn.resolvedGamepad.rawValue) mgmt=\(conn.hostMgmtPort)"
                    sessionLog.info("\(landed, privacy: .public)")
                    if pin != nil || autoTrust || requestAccess {
                        // requestAccess: the operator approved this device on the host, so the
                        // session is trusted — stream directly (the caller pins it as paired).
                        self.connection = conn
                        self.noteTouchFallback(conn)
                        self.startStatsTimer()
                        self.beginStreaming()
                    } else if allowTofu {
                        // Host advertised pair=optional — offer the reduced-security TOFU prompt
                        // over the live (blurred) stream (rule 3a).
                        self.connection = conn
                        self.noteTouchFallback(conn)
                        self.startStatsTimer()
                        self.phase = .awaitingTrust(fingerprint: conn.hostFingerprint)
                    } else {
                        // Unpinned and TOFU not permitted (rule 3b): never let this silently
                        // become trustable. Drop the connection; the caller routes to pairing.
                        Task.detached { conn.close() } // joins Rust threads — off-main
                        self.phase = .idle
                        self.activeHost = nil
                        SessionSettings.end() // no session, so nothing may keep its settings latched
                        self.errorMessage = "\(host.displayName) is not paired yet. "
                            + "Pair with its PIN before streaming."
                    }
                case .failure(let error):
                    sessionLog.warning(
                        "connect \(host.displayName, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                    self.phase = .idle
                    self.activeHost = nil
                    SessionSettings.end() // the dial failed — back to the plain globals
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
        applyMicMute() // now muted for privacy — on top of the user's own mute, not instead of it
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
        applyMicMute() // back to the user's own choice — which may well still be "muted"
        if let conn = connection {
            conn.setVideoDropped(false)
            conn.requestKeyframe()
        }
    }

    // MARK: - Microphone mute (in-stream, per session)

    /// Whether this session has a mic uplink there is any point in muting: the mic must be on in
    /// the session's RESOLVED settings (a profile can turn it on or off), the platform must have
    /// an app-accessible input at all, and the OS must not have refused us one. Drives whether the
    /// mute control is offered — a live-looking mute button over a session that sends no
    /// microphone would be a lie. Same three conditions `SessionAudio` starts an uplink on
    /// (`.notDetermined` counts: the prompt is pending and a grant starts the uplink mid-session).
    var micAvailable: Bool {
        #if os(tvOS)
        return false // no app-accessible microphone — SessionAudio never opens an uplink either
        #else
        // The session's grants must include MIC (per-client access §7 — hide the mic UI when
        // ungranted; a mute button over a mic the host drops would be a lie twice over).
        guard settings.micEnabled, connection?.canUseMic != false else { return false }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized, .notDetermined: return true
        default: return false // denied / restricted — there is no uplink to mute
        }
        #endif
    }

    /// Flip the user's mute. The in-stream surfaces (HUD button, Stream menu, ⌃⌥⇧A while
    /// captured, the iOS mic disc) all land here.
    func toggleMicMute() {
        setMicMuted(!micMuted)
    }

    /// Set the user's mute directly (the badge's tap-to-unmute). Ignored when the session has no
    /// microphone, so a stale surface can't leave a phantom "muted" badge over a session that was
    /// never sending anything.
    func setMicMuted(_ muted: Bool) {
        guard micAvailable, micMuted != muted else { return }
        micMuted = muted
        applyMicMute()
    }

    /// A forwarded controller has a gyro this session cannot carry (see
    /// `GamepadCapture.onMotionUnreachable`). Show it briefly, then let it go.
    ///
    /// Last pad wins, and its timer restarts: two such pads are the same one fact to a player, and
    /// a second hint appearing under a still-visible first would only read as a stutter.
    /// Raise `touchFallbackNotice` when the passthrough touch model meets a host without touch
    /// injection — the same fallback `StreamLayerUIView` applies to the fingers themselves.
    private func noteTouchFallback(_ conn: PunktfunkConnection) {
        #if os(iOS)
        guard TouchInputMode.current == .touch, !conn.hostSupportsTouch else { return }
        touchFallbackNotice = true
        touchHintTimer?.cancel()
        touchHintTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.touchFallbackNotice = false
        }
        #endif
    }

    private func noteMotionUnreachable(_ kind: PunktfunkConnection.GamepadType) {
        motionUnreachableKind = kind
        motionHintTimer?.cancel()
        motionHintTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.motionUnreachableKind = nil
        }
    }

    /// Push the EFFECTIVE mute — the user's choice OR the background keep-alive's privacy mute —
    /// onto the audio engine. The two reasons are composed here and nowhere else: whichever one
    /// changed, the other still holds, so returning from the background can't un-mute a user who
    /// muted mid-stream, and a user unmuting while backgrounded (Live Activity, another window)
    /// doesn't open the mic behind their back.
    private func applyMicMute() {
        audio?.setMicMuted(micMuted || isBackgrounded)
    }

    // MARK: - Per-client access (chip state + expiry warnings)

    /// Refresh the published access state from the connection's LIVE grants + countdown —
    /// called by the 1 Hz stats tick, which is also what makes a mid-session `AccessUpdate`
    /// (a console edit) reach the chip and the capture gates within a second. The equality
    /// guards keep a full-and-permanent session (every old host) from publishing anything.
    private func updateAccessState() {
        guard let conn = connection else { return }
        let grants = conn.accessGrants
        let level = PunktfunkConnection.AccessLevel(grants: grants)
        let remaining = conn.accessExpiresInSeconds
        if accessLevel != level { accessLevel = level }
        if accessRemainingSecs != remaining { accessRemainingSecs = remaining }
        let limited = level != .fullControl || remaining != 0
        if accessLimited != limited { accessLimited = limited }
        // A mid-session edit that removed BOTH input classes releases an engaged capture:
        // holding a frozen cursor and swallowed keys over input the host now drops is
        // exactly the "keyboard does nothing and nobody says why" failure §7 exists to
        // prevent. (Engage is gated at the stream views; this is the live-revoke half.)
        if mouseCaptured,
           grants & (PunktfunkConnection.grantPointer | PunktfunkConnection.grantKeyboard) == 0 {
            NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
        }
        // The T−5 m / T−1 m warning toasts (§7). Derived from the countdown CROSSING the
        // marks rather than from the AccessUpdate messages alone: the host's warnings
        // re-anchor the same countdown, so this shows them when they arrive AND still fires
        // on plain clock progress if a warning datagram never lands. One shot each; an edit
        // that extends the deadline back above a mark re-arms it.
        guard remaining != 0 else { return }
        if remaining > 300 {
            accessWarned5m = false
            accessWarned1m = false
        } else if remaining > 60 {
            accessWarned1m = false
            if !accessWarned5m {
                accessWarned5m = true
                showAccessWarning("Access ends in \(Self.accessCountdown(remaining))")
            }
        } else if !accessWarned1m {
            accessWarned1m = true
            accessWarned5m = true
            showAccessWarning("Access ends in under a minute")
        }
    }

    /// Put one warning toast up for a few seconds (the motion hint's pattern: last one wins,
    /// its timer restarts, teardown cancels a pending clear).
    private func showAccessWarning(_ text: String) {
        accessWarning = text
        accessWarningTimer?.cancel()
        accessWarningTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.accessWarning = nil
        }
    }

    /// "1 h 58 m" / "12 m" / "45 s" — the countdown wording the chip and the warnings share.
    static func accessCountdown(_ secs: UInt32) -> String {
        let s = Int(secs)
        if s >= 3600 { return "\(s / 3600) h \((s % 3600) / 60) m" }
        if s >= 60 { return "\(s / 60) m" }
        return "\(s) s"
    }

    /// Follow a live stats-overlay cycle (⌃⌥⇧S, the three-finger tap, the Stream menu). Those
    /// surfaces write the GLOBAL setting as they always have; this moves the session's own tier
    /// with it, so cycling still works in a session a profile put on a different tier.
    func setStatsVerbosity(_ tier: StatsVerbosity) {
        guard statsVerbosity != tier else { return }
        statsVerbosity = tier
        settings.statsVerbosity = tier.rawValue
        SessionSettings.setStatsVerbosity(tier.rawValue)
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
        if connection != nil {
            let line = "disconnect \(activeHost?.displayName ?? "-") deliberate=\(deliberate) phase=\(phase)"
            sessionLog.info("\(line, privacy: .public)")
        }
        statsTimer?.invalidate()
        statsTimer = nil
        // Release the session's resolved settings: from here every reader falls back to the plain
        // globals, which is exactly what they saw before profiles existed.
        SessionSettings.end()
        // No-op when this session never reached `.streaming` (a refused/aborted connect).
        displaySleepGuard.release()
        // Drop any armed background keep-alive (incl. the timeout that just fired us).
        backgroundTimer?.cancel()
        backgroundTimer = nil
        isBackgrounded = false
        backgroundDeadline = nil
        // The mic mute is per-session and never persisted: the next stream starts live (if the
        // mic is enabled), rather than silently carrying a mute nobody remembers making.
        micMuted = false
        // Cancel before clearing: a pending clear firing into a torn-down session would be
        // harmless but pointless, and leaving the hint set would carry it into the next stream.
        motionHintTimer?.cancel()
        motionHintTimer = nil
        motionUnreachableKind = nil
        touchHintTimer?.cancel()
        touchHintTimer = nil
        touchFallbackNotice = false
        // Access state is per-session: back to the invisible full-and-permanent default, and
        // no warning latch may carry into the next stream (same discipline as the mic mute).
        accessWarningTimer?.cancel()
        accessWarningTimer = nil
        accessWarning = nil
        accessLevel = .fullControl
        accessRemainingSecs = 0
        accessLimited = false
        accessWarned5m = false
        accessWarned1m = false
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
        #if !os(tvOS)
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
                #if !os(tvOS)
                // Disables sync on the wire while the connection is still up — and on iOS pulls a
                // host offer the user has not pasted yet down to real bytes, which needs that
                // connection, so it must stay ahead of the close below.
                clipboard?.stop()
                #endif
                // Deliberate user quit → tell the host to skip the keep-alive linger (must precede close).
                if deliberate { conn.disconnectQuit() }
                conn.close()
            }
        } else {
            Task.detached {
                audio?.stop()
                feedback?.stop()
                #if !os(tvOS)
                clipboard?.stop()
                #endif
            }
        }
        connection = nil
        activeHost = nil
        // Read by `sessionEnded` BEFORE it calls us, so clearing here can't rob it of the answer.
        launchedTitleID = nil
        launchedShelf = nil
        phase = .idle
        fps = 0
        mbps = 0
        hostNetworkValid = false
        splitValid = false
        endToEndValid = false
        decodeValid = false
        displayValid = false
        clientQueueValid = false
        osFloorValid = false
        linkInfoValid = false
        // Drop the previous session's grant too — the shared box outlives the session, and a new
        // link may never come up (a non-deadline rung has none at all).
        PresentLinkInfo.shared.clear()
        audioValid = false
        audioFormatLabel = nil
        lostFrames = 0
        lostPct = 0
        mouseCaptured = false
        resizing = false
        resizeIndicator = ResizeIndicator() // no stale target/timer into the next session
    }

    /// Called (via the main actor) when the pump hits end-of-session.
    func sessionEnded() {
        guard let conn = connection else { return }
        let name = activeHost?.displayName ?? "host"
        // WHY it ended, asked while the connection is still up — `disconnect` tears it down.
        let reason = conn.sessionEndReason
        // A typed mid-session rejection outranks the coarse reason: an access-expiry close
        // (per-client access §4) files under `.hostError` there, and "ended with an error"
        // is the wrong sentence for "your access expired".
        let rejection = conn.endRejection
        // Where a game exit sends us: back into the library this title was launched from, so the
        // next one is a tap away. Only for a launch that CAME from the library — a game exiting in
        // a plain desktop session has no library to return to.
        let host = activeHost
        let cameFromLibrary = launchedTitleID != nil
        // The shelf it came off — falling back to the host's own if a caller launched a title
        // without naming one, which is what that launch effectively browsed.
        let shelf = launchedShelf ?? activeHost.map { LibraryTarget(host: $0) }
        let endLine = "session ended by \(name) reason=\(reason) "
            + "rejection=\(rejection.map { String(describing: $0) } ?? "-")"
        sessionLog.info("\(endLine, privacy: .public)")
        disconnect(deliberate: false) // host/network ended it — keep the linger for a reconnect
        if let rejection {
            // The shared typed-rejection wording ("Your access to this host has expired…").
            errorMessage = "\(name): \(rejection.userMessage)"
            return
        }
        switch reason {
        case .gameExited:
            // The player quit their own game. Not a failure, and they are probably after the next
            // title — so no banner, and back to the library it came from.
            if cameFromLibrary, host != nil, let shelf {
                returnToLibrary = shelf
            }
        case .hostEnded, .local:
            // Someone asked for this: an operator "End" on the host, or our own close racing in.
            // Say it plainly, without the error framing.
            errorMessage = "\(name) ended the session."
        case .hostError:
            errorMessage = "\(name) ended the session with an error."
        case .lost:
            errorMessage = "Lost the connection to \(name)."
        case .none:
            // No verdict (an older core, or the close raced the read): keep the wording this path
            // has always used rather than inventing one.
            errorMessage = "Session ended by \(name)."
        }
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
        displaySleepGuard.acquire()
        // Audio starts with streaming, not during the trust prompt — no host sound (or
        // mic uplink!) before the user trusted the host. Devices and the mic switch come from the
        // session's resolved settings ("" = system default), so a profile that turns the mic on
        // for work calls applies to the uplink too.
        let audio = SessionAudio(connection: conn)
        audio.start(
            speakerUID: settings.speakerUID,
            micUID: settings.micUID,
            micChannel: settings.micChannel,
            // Deny-at-setup for an ungranted mic (per-client access §5): no MIC bit, no
            // uplink at all — a capture the host would only drop is pure privacy downside.
            micEnabled: settings.micEnabled && conn.canUseMic,
            echoCancel: settings.echoCancel,
            // The A/V sync reference: `endToEnd` is capture→on-glass, the one figure that says
            // where the picture actually IS, and the audio ring steers its depth to land with it.
            // The same meter object the presenter writes per presented frame, so audio reads the
            // video plane's own measurement rather than a second estimate of it — and under the
            // stage-1 fallback presenter, which stamps nothing, it stays empty and the loop
            // correctly declines to correct.
            videoLatency: endToEnd)
        self.audio = audio
        // Only when the session is genuinely on the lossless plane — the HUD says nothing for an
        // ordinary Opus one. Read from the connection's Welcome, so a request the host's gate
        // declined shows the fallback it actually landed on rather than what was asked for.
        audioFormatLabel = conn.isLosslessAudio
            ? "lossless \(Self.kHzLabel(conn.resolvedAudioRateHz)) kHz / "
                + "\(conn.resolvedAudioBits)-bit \(Self.layoutLabel(conn.resolvedAudioChannels))"
            : nil
        // Gamepads: forward every controller GamepadManager selected — each on its own wire pad
        // index (a pin forwards only one, Automatic forwards all) — and render the host's feedback
        // back to the pad it's addressed to (rumble always; lightbar/player-LEDs/adaptive-triggers
        // when a pad's virtual device is a DualSense). Same trust gate as audio — nothing is
        // forwarded during the trust prompt.
        // `gamepadForwarding` off means the host gets this device's pads from somewhere else
        // (USB passthrough, or a pad plugged into the host) — capture still runs, and still
        // watches for the escape chord, but puts nothing on the wire.
        // System-button routing: whether raw guide/share presses ride the wire, and whether
        // hold-Select arms as the alternate guide route (auto = on everywhere but macOS —
        // iOS reserves the physical Home press, tvOS never delivers it).
        let capture = GamepadCapture(
            connection: conn, manager: .shared, forwarding: settings.gamepadForwarding,
            systemForward: settings.systemButtonsForward,
            guideGesture: settings.guideGestureEnabled)
        // The cross-client escape chord (hold L1+R1+Start+Select 1.5 s) — on tvOS the only
        // controller way out of a stream (B/Menu is swallowed during sessions; see ContentView).
        capture.onDisconnectRequest = { [weak self] in self?.disconnect() }
        // A pad with a gyro that this session cannot carry — say so once, briefly, and name the
        // setting that fixes it. Already main-actor (GamepadCapture fires it there).
        capture.onMotionUnreachable = { [weak self] kind in self?.noteMotionUnreachable(kind) }
        capture.start()
        gamepadCapture = capture
        let feedback = GamepadFeedback(connection: conn, manager: .shared)
        feedback.start()
        gamepadFeedback = feedback
        #if !os(tvOS)
        // Shared clipboard: opt-in per host AND host-advertised (older hosts / operator-disabled
        // hosts never see a ClipControl) AND granted to this device (per-client access §5 —
        // without the bit the host would refuse with CLIP_REASON_NOT_PERMITTED anyway; not
        // asking keeps the UI honest). Same trust gate as audio — nothing is announced
        // during the trust prompt.
        if activeHost?.clipboardSync == true, conn.hostSupportsClipboard, conn.canUseClipboard {
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

    #if !os(tvOS)
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
    /// tells the host, which drops any selection we own there. No-op on tvOS or while idle.
    func toggleClipboardSync() {
        #if !os(tvOS)
        guard let conn = connection, phase == .streaming else { return }
        if let sync = clipboardSync {
            clipboardSync = nil
            clipboardEnabled = false
            clipboardReason = 0
            Task.detached { sync.stop() }
        } else if conn.hostSupportsClipboard, conn.canUseClipboard {
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
                // Access chip + expiry warnings: the same tick that drives every other live
                // readout also walks the countdown and picks up mid-session grant edits.
                self.updateAccessState()
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
                // Drained even when the stats drains came back empty — with a badly wrong offset
                // an entire window is refused and only this counter still tells the story.
                self.skewTrimPerS =
                    self.latency.drainTrimmed() + self.endToEnd.drainTrimmed()
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
                if let f = self.presentFloor.drain() {
                    self.osFloorP50Ms = f.p50Ms
                    self.osFloorValid = true
                } else {
                    self.osFloorValid = false
                }
                // The display link's latency ask + property readback (deadline rung only) — a
                // LEVEL, not a window, so it is read rather than drained.
                if let l = PresentLinkInfo.shared.snapshot() {
                    self.linkLatencyAskFrames = l.ask
                    self.linkLatencyFrames = l.latency
                    self.linkRangeMinHz = l.rangeMin
                    self.linkRangeMaxHz = l.rangeMax
                    self.linkDrawables = l.drawables
                    self.linkInfoValid = true
                } else {
                    self.linkInfoValid = false
                }
                if let q = self.clientQueue.drain() {
                    self.clientQueueP50Ms = q.p50Ms
                    self.clientQueueValid = true
                } else {
                    self.clientQueueValid = false
                }
                // The audio plane is a LEVEL, not a window: the ring's depth and the sync loop's
                // smoothed offset are both current values, so they are read rather than drained.
                if let a = self.audio?.stats {
                    self.audioBufferMs = a.bufferMS
                    self.audioAvOffsetMs = a.avOffsetMS
                    self.audioValid = true
                } else {
                    self.audioValid = false
                }
                // Mirror the window to the unified log (see statsLog) — one line per second,
                // stages in ms, only while frames actually flowed. `fps` counts RECEIVED AUs;
                // `presents` counts frames that reached glass (the display meter's sample count)
                // — a presents≪fps gap is the presenter dropping/serializing, an fps deficit is
                // upstream (host capture/encode or the network).
                if frames > 0 {
                    // The classic fields stay RAW (cross-session comparability with every log
                    // captured before the 2026-07 floor policy); the appended trio carries the
                    // measured OS present floor and the floor-shaved values the HUD displays.
                    let line = String(
                        // Swift Int is 64-bit → %lld, NOT %d (which is a 32-bit C int); macOS 26's
                        // strict String(format:) validator rejects the %d/Int mismatch and drops
                        // the whole line (a cascade error that also mis-blames the float args).
                        //
                        // ⚠ Every invalid-field fallback below MUST be a typed `-1.0` (or a
                        // `Double(...)`-wrapped value), never a bare `-1`: in this variadic
                        // `CVarArg` context the ternary does NOT unify to Double — the untyped
                        // literal goes in as Int, and `%f` then reads Int64(-1)'s all-ones bit
                        // pattern, which IS a quiet NaN. Field 2026-08-13 (tvOS, stage-1, the
                        // first session ever to have invalid fields while frames flowed): every
                        // fallback printed `nan`. Latent since the line was added.
                        format: "fps=%lld presents=%lld e2e_p50=%.1f e2e_p95=%.1f hostnet_p50=%.1f "
                            + "decode_p50=%.1f display_p50=%.1f lost=%lld "
                            + "floor_p50=%.1f display_adj=%.1f e2e_adj=%.1f queue_p50=%.1f "
                            // Appended LAST, so every existing parser of this line is unaffected.
                            // In the log as well as on the HUD because the overlay is only up when
                            // someone thought to turn it on, and the reports that need these
                            // numbers arrive after the fact.
                            + "audio_buffer=%lld audio_av_offset=%lld "
                            // The deadline link's latency ask + property readback (both -1 on
                            // non-deadline rungs) — appended so the PUNKTFUNK_FRAME_LATENCY
                            // ladder is readable over the stdout channel with the HUD off,
                            // which is the only honest way to run it on a tvOS device.
                            + "link_ask=%.2f link_readback=%.2f "
                            // Impossible samples the host-anchored meters refused this window:
                            // nonzero ⇒ the clock offset is lying and e2e/hostnet above are
                            // truncated distributions — disregard their p50/p95.
                            + "skew_trim=%lld",
                        frames,
                        displayWindow?.count ?? 0,
                        self.endToEndValid ? self.endToEndP50Ms : -1.0,
                        self.endToEndValid ? self.endToEndP95Ms : -1.0,
                        self.hostNetworkValid ? self.hostNetworkP50Ms : -1.0,
                        self.decodeValid ? self.decodeP50Ms : -1.0,
                        self.displayValid ? self.displayP50Ms : -1.0,
                        lost,
                        self.osFloorValid ? self.osFloorP50Ms : -1.0,
                        self.displayValid ? self.displayAdjP50Ms : -1.0,
                        self.endToEndValid ? self.endToEndAdjP50Ms : -1.0,
                        self.clientQueueValid ? self.clientQueueP50Ms : -1.0,
                        self.audioValid ? self.audioBufferMs : -1,
                        self.audioValid ? self.audioAvOffsetMs : 0,
                        self.linkInfoValid ? Double(self.linkLatencyAskFrames) : -1.0,
                        self.linkInfoValid ? Double(self.linkLatencyFrames) : -1.0,
                        self.skewTrimPerS)
                    statsLog.info("\(line, privacy: .public)")
                    if statsToStdout { print("pf.stats \(line)") }
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }
}
