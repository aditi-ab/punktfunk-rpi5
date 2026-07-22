// Swift wrapper around the punktfunk-core C ABI's punktfunk/1 connection API.
//
// Threading contract (mirrors the C header): one PunktfunkConnection is pumped from a single
// video thread via nextAU(); nextAudio() runs on its own (single) drain thread, and
// nextRumble()/nextHidOutput() share one feedback drain thread (two core planes, one puller
// each — polling them sequentially from one thread is within the contract); the core keeps
// per-plane borrow slots, so the planes never alias. send() is enqueue-only and safe
// alongside all of them. The pointers inside an AU/audio packet are only valid until the
// next call of the same kind, so we copy into Data here — the copies are small and keep the
// Swift side memory-safe.
//
// Trust: pass the host's pinned certificate fingerprint (the host logs it at startup, and
// `hostFingerprint` reports what a trust-on-first-use connect observed — persist it, e.g.
// in UserDefaults keyed by host, and pin it from then on).
//
// close() is safe from any thread: it flags the pullers to exit at their next poll
// boundary, then takes the per-plane locks (each held across its blocking C poll), so the
// handle is never freed under an in-flight call — the C contract ("never close with a
// next_au/next_audio call in flight") is enforced here rather than left to callers. After
// close, the pull methods throw `.closed` and the threads unwind on their own.

import Foundation
import PunktfunkCore

// cbindgen's C17-compatible header spells the typedefs as plain integers
// (`typedef int32_t PunktfunkStatus`, `typedef uint8_t PunktfunkInputKind`) while the enum
// constants import as a distinct same-named Swift type — bridge by raw value once here.
private let statusOK: Int32 = PUNKTFUNK_STATUS_OK.rawValue
private let statusNoFrame: Int32 = PUNKTFUNK_STATUS_NO_FRAME.rawValue
private let statusClosed: Int32 = PUNKTFUNK_STATUS_CLOSED.rawValue

/// One reassembled, FEC-recovered, decrypted access unit (Annex-B HEVC from the host).
public struct AccessUnit: Sendable {
    public let data: Data
    public let ptsNs: UInt64
    public let frameIndex: UInt32
    public let flags: UInt32
    /// Client `CLOCK_REALTIME` instant the AU finished reassembly in the core (post-FEC,
    /// decrypted — `PunktfunkFrame.received_ns`, ABI v9) — the **received** measurement point of
    /// design/stats-unification.md. NOT the pull instant: stamping at the pull folded the
    /// pre-decode hand-off wait into the network term, which is how the 2026-07 two-pair
    /// standing-latency plateau hid as "network". The decode stage is `decodedNs - receivedNs`,
    /// both client-local (no skew offset applies).
    public let receivedNs: Int64
    /// Client `CLOCK_REALTIME` instant this pull returned. `pulledNs - receivedNs` is the
    /// client-queue wait (kernel hand-off + FrameChannel dwell) — the term the HUD splits out
    /// so a client-side standing backlog can never masquerade as network latency again.
    public let pulledNs: Int64

    /// `pulledNs` defaults to `receivedNs` (zero queue wait) for callers with no pull instant —
    /// the synthetic probe AUs and decode tests, where the split is meaningless.
    public init(
        data: Data, ptsNs: UInt64, frameIndex: UInt32, flags: UInt32,
        receivedNs: Int64, pulledNs: Int64? = nil
    ) {
        self.data = data
        self.ptsNs = ptsNs
        self.frameIndex = frameIndex
        self.flags = flags
        self.receivedNs = receivedNs
        self.pulledNs = pulledNs ?? receivedNs
    }
}

/// One Opus audio packet (48 kHz stereo, 5 ms frames) — decode with AVAudioConverter
/// (`kAudioFormatOpus`) or libopus into an AVAudioEngine source node.
public struct AudioPacket: Sendable {
    public let data: Data
    public let ptsNs: UInt64
    public let seq: UInt32
}

public enum PunktfunkClientError: Error {
    /// Connect failed — wrong host/port, timeout, or a certificate-pin mismatch.
    case connectFailed
    /// `pinSHA256` was non-nil but not exactly 32 bytes. Failing closed: connecting
    /// unpinned when the caller asked for verification would be a silent trust downgrade.
    case invalidPin
    /// Pairing rejected — wrong PIN.
    case wrongPIN
    case closed
    case status(Int32)
    /// The host deliberately turned the attempt away and said why (its typed QUIC
    /// application close) — distinct from `.connectFailed` (unreachable/timeout) so the UI
    /// can show the stated reason instead of blaming the network.
    case rejected(HostRejection)
}

/// Why a host turned a connect/pair attempt away — decoded from the
/// `PUNKTFUNK_STATUS_REJECTED_*` block. Lets the UI say "approve the request on the host"
/// or "pairing isn't armed" instead of a generic "could not connect".
public enum HostRejection: Sendable {
    case pairingNotArmed
    case pairingBoundToOtherDevice
    case pairingRateLimited
    case identityRequired
    case denied
    case approvalTimeout
    case superseded
    case wireVersionMismatch
    case busy

    init?(status: Int32) {
        switch status {
        case PUNKTFUNK_STATUS_REJECTED_NOT_ARMED.rawValue: self = .pairingNotArmed
        case PUNKTFUNK_STATUS_REJECTED_BOUND_OTHER.rawValue: self = .pairingBoundToOtherDevice
        case PUNKTFUNK_STATUS_REJECTED_RATE_LIMITED.rawValue: self = .pairingRateLimited
        case PUNKTFUNK_STATUS_REJECTED_IDENTITY_REQUIRED.rawValue: self = .identityRequired
        case PUNKTFUNK_STATUS_REJECTED_DENIED.rawValue: self = .denied
        case PUNKTFUNK_STATUS_REJECTED_APPROVAL_TIMEOUT.rawValue: self = .approvalTimeout
        case PUNKTFUNK_STATUS_REJECTED_SUPERSEDED.rawValue: self = .superseded
        case PUNKTFUNK_STATUS_REJECTED_WIRE_VERSION.rawValue: self = .wireVersionMismatch
        case PUNKTFUNK_STATUS_REJECTED_BUSY.rawValue: self = .busy
        default: return nil
        }
    }

    /// User-facing sentence — wording shared with the desktop clients.
    public var userMessage: String {
        switch self {
        case .pairingNotArmed:
            return "Pairing isn't armed on the host — arm it on the host's Pairing page, "
                + "then try again."
        case .pairingBoundToOtherDevice:
            return "The host's pairing window is armed for a different device — arm it "
                + "for this one."
        case .pairingRateLimited:
            return "Too many pairing attempts — wait a couple of seconds and try again."
        case .identityRequired:
            return "The host requires pairing — pair this device (PIN or request access) first."
        case .denied:
            return "The host declined this device's request."
        case .approvalTimeout:
            return "Nobody approved the request on the host in time — approve this device "
                + "in the host's console or web UI, then request access again."
        case .superseded:
            return "A newer request from this device replaced this one — approve the "
                + "latest request on the host."
        case .wireVersionMismatch:
            return "Client and host versions don't match — update both to the same release."
        case .busy:
            return "The host is busy with another session."
        }
    }
}

/// `withCString` over an optional — nil maps to a NULL C pointer.
func withOptionalCString<R>(_ s: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
    guard let s else { return body(nil) }
    return s.withCString { body($0) }
}

public extension PunktfunkConnection {
    /// Whether the Wake-on-LAN broadcast path is usable on this platform/build. macOS can always
    /// broadcast (its App Sandbox network entitlements cover it). iOS/tvOS need the managed
    /// `com.apple.developer.networking.multicast` entitlement — now approved and enabled (see
    /// `Config/Punktfunk.entitlements`), so wake is available on every platform. Kept as the single
    /// switch every call site gates on, should a future build ever need to disable it.
    static var wakeOnLANAvailable: Bool { true }

    /// Send a Wake-on-LAN magic packet to wake a sleeping host. `macs` are the host's NIC MAC(s)
    /// (`aa:bb:cc:dd:ee:ff`, learned from its mDNS `mac` TXT while awake); malformed entries are
    /// skipped. `lastKnownIP`, when set, is additionally unicast. The core broadcasts to every
    /// interface's subnet-directed broadcast + 255.255.255.255 on ports 9/7, repeated.
    ///
    /// Returns true if at least one datagram went out. Does blocking sends — call OFF the main
    /// thread. On iOS/tvOS this requires the `com.apple.developer.networking.multicast` entitlement
    /// (broadcast is otherwise blocked by the OS); macOS needs only the existing network entitlements.
    @discardableResult
    static func wakeOnLAN(macs: [String], lastKnownIP: String? = nil) -> Bool {
        var bytes: [UInt8] = []
        var count = 0
        for mac in macs {
            let parts = mac.split(separator: ":")
            guard parts.count == 6 else { continue }
            let octets = parts.compactMap { UInt8($0, radix: 16) }
            guard octets.count == 6 else { continue }
            bytes.append(contentsOf: octets)
            count += 1
        }
        guard count > 0 else { return false }
        let rc: Int32 = bytes.withUnsafeBufferPointer { buf in
            withOptionalCString(lastKnownIP) { ip in
                punktfunk_wake_on_lan(buf.baseAddress, UInt(count), ip)
            }
        }
        return rc == statusOK
    }

    /// Bounded, trust-agnostic QUIC-handshake reachability probe to `host:port` — mDNS-INDEPENDENT,
    /// so a host reached over a routed network (Tailscale/VPN/another subnet), which never
    /// advertises, still reports reachable. No pin/identity presented. The display-side companion
    /// to the dial-first connect fix: lets saved-host "online" pips reflect real reachability.
    /// Blocking (builds its own runtime) — call OFF the main thread.
    static func probe(host: String, port: UInt16, timeoutMs: UInt32 = 1500) -> Bool {
        let rc: Int32 = host.withCString { punktfunk_probe($0, port, timeoutMs) }
        return rc == statusOK
    }
}

public final class PunktfunkConnection {
    private var handle: OpaquePointer?
    /// Set by close() before it contends for the plane locks: the pullers see it at their
    /// next poll boundary and exit, so close() can't be starved by back-to-back polls
    /// (NSLock is not fair).
    private var closeRequested = false
    /// Serializes send()/close() against each other and guards `handle`/`closeRequested`.
    private let abiLock = NSLock()
    /// Held across the blocking next_au call; close() takes it (same plane-lock → abiLock
    /// order as the pullers) so it can never free the handle under an in-flight poll.
    private let pumpLock = NSLock()
    /// Same role for the audio drain thread (its own plane in the core).
    private let audioLock = NSLock()
    /// Same role for the feedback drain thread (rumble + HID-output — two core planes,
    /// drained sequentially by one thread).
    private let feedbackLock = NSLock()
    /// Same role for the host-timing (0xCF) puller — its own plane in the core, drained
    /// non-blockingly by the app's 1 s stats tick (never contends with the blocking pullers).
    private let statsLock = NSLock()
    /// Same role for the shared-clipboard drain thread (`nextClipboard` — its own plane in the
    /// core). The clip *sends* (`clipControl`/`clipOffer`/`clipServe`…) share this lock too:
    /// they're quick non-blocking enqueues, and a single lock keeps close() ordering simple.
    private let clipboardLock = NSLock()
    /// Serializes the (single) cursor pull thread against close() — both cursor planes are
    /// drained by ONE thread, so one lock covers them.
    private let cursorLock = NSLock()

    /// Negotiated session mode (host-confirmed).
    public private(set) var width: UInt32 = 0
    public private(set) var height: UInt32 = 0
    public private(set) var refreshHz: UInt32 = 0

    /// SHA-256 fingerprint of the certificate the host presented (32 bytes). After a
    /// trust-on-first-use connect, persist this and pass it as `pinSHA256` next time.
    public private(set) var hostFingerprint: Data = Data()

    /// Compositor preference for the host's per-session virtual output (the
    /// `PUNKTFUNK_COMPOSITOR_*` ABI values). `.auto` lets the host auto-detect from its
    /// running desktop; a concrete backend is honored only if available on the host right
    /// now — else the host falls back to auto-detect and logs the real choice.
    public enum Compositor: UInt32, CaseIterable, Sendable {
        case auto = 0
        case kwin = 1
        case wlroots = 2
        case mutter = 3
        case gamescope = 4

        /// Loose name parsing for env/dev hooks ("kde" and "sway" are accepted aliases,
        /// mirroring the host's `CompositorPref::from_name`).
        public init?(name: String) {
            switch name.lowercased() {
            case "auto": self = .auto
            case "kwin", "kde": self = .kwin
            case "wlroots", "sway", "hyprland": self = .wlroots
            case "mutter", "gnome": self = .mutter
            case "gamescope": self = .gamescope
            default: return nil
            }
        }
    }

    /// Which virtual gamepad the host creates for this session's pads (the
    /// `PUNKTFUNK_GAMEPAD_*` ABI values). `.auto` lets the host decide (its env var, else
    /// X-Box 360); `.dualSense` / `.dualShock4` are honored only on hosts with UHID (Linux) —
    /// games then see a real PlayStation pad and its lightbar (and, on a DualSense,
    /// adaptive-trigger / player-LED) writes come back on the HID-output plane
    /// (`nextHidOutput`). `.xboxOne` is an X-Box-Series-glyph variant of `.xbox360` (same
    /// buttons/sticks/triggers + rumble, no touchpad/motion/lightbar). The host's actual
    /// choice is `resolvedGamepad`.
    public enum GamepadType: UInt32, CaseIterable, Sendable {
        case auto = 0
        case xbox360 = 1
        case dualSense = 2
        case xboxOne = 3
        case dualShock4 = 4
        // Valve Steam Controller / Steam Deck (Linux UHID hid-steam hosts). Parity only on Apple —
        // GameController never surfaces a 0x28DE HID device, so the client can't capture one; these
        // exist so the resolved type round-trips and name parsing matches the host.
        case steamController = 5
        case steamDeck = 6
        /// DualSense Edge (Linux UHID / Windows UMDF hosts): the DualSense plus native back/Fn
        /// buttons. GameController exposes the Edge as a `GCDualSenseGamepad` with its own
        /// product category; paddle CAPTURE is still gated on G22, but the declared identity +
        /// rich planes match the physical pad.
        case dualSenseEdge = 7
        /// Nintendo Switch Pro Controller (Linux UHID hid-nintendo hosts): correct Nintendo
        /// glyphs + positional layout on the host side.
        case switchPro = 8
        /// New Steam Controller (2026, `28DE:1302`), passed through as-is on Linux hosts (raw
        /// report mirroring; Steam Input is the consumer). Parity only on Apple — GameController
        /// never surfaces the raw Valve device, so the client can't capture one; exists so the
        /// resolved type round-trips and name parsing matches the host.
        case steamController2 = 9

        /// Loose name parsing for env/dev hooks, mirroring the host's
        /// `GamepadPref::from_name`.
        public init?(name: String) {
            switch name.lowercased() {
            case "auto", "default": self = .auto
            case "xbox", "xbox360", "x360", "uinput": self = .xbox360
            case "dualsense", "ds", "ds5", "ps5": self = .dualSense
            case "xboxone", "xbox-one", "xboxseries", "series": self = .xboxOne
            case "dualshock4", "dualshock", "ds4", "ps4": self = .dualShock4
            case "steamdeck", "steam-deck", "deck": self = .steamDeck
            case "steamcontroller", "steam-controller", "steamcon": self = .steamController
            case "steamcontroller2", "steam-controller-2", "steamcon2", "sc2", "ibex":
                self = .steamController2
            case "dualsenseedge", "dualsense-edge", "edge", "dsedge": self = .dualSenseEdge
            case "switchpro", "switch-pro", "switch", "procontroller", "pro-controller":
                self = .switchPro
            default: return nil
            }
        }
    }

    /// The virtual gamepad backend the host actually resolved (the Welcome's echo of the
    /// requested `gamepad`). `.auto` = an older host that didn't say — assume Xbox 360, no
    /// DualSense feedback.
    public private(set) var resolvedGamepad: GamepadType = .auto

    /// The compositor the host actually resolved for this session's virtual output (the
    /// Welcome's echo of the requested `compositor`, with `.auto` resolved to a concrete
    /// backend). `.auto` = an older host that didn't say. Clients use it to decide
    /// client-side cursor behavior: `.gamescope`'s PipeWire capture carries no cursor, so
    /// the client draws its own (a visible system cursor over the stream).
    public private(set) var resolvedCompositor: Compositor = .auto

    /// Host clock minus client clock (nanoseconds), from the connect-time wall-clock skew handshake
    /// (`punktfunk_connection_clock_offset_ns`). Add it to a local `CLOCK_REALTIME` instant to
    /// express that instant in the host's capture clock — the clock each `AccessUnit.ptsNs` is
    /// stamped in — so a glass-to-glass latency (present/enqueue time minus `ptsNs`) is valid across
    /// machines. `0` = no correction (an older host that didn't answer, or synchronized clocks).
    public private(set) var clockOffsetNs: Int64 = 0

    /// The video encoder bitrate (kbps) the host actually configured — the requested
    /// `bitrateKbps` clamped to the host's range ([500, 2 000 000] kbps), or its default
    /// (20 000) when 0 was requested. `0` = an older host that didn't report it.
    public private(set) var resolvedBitrateKbps: UInt32 = 0

    /// The colour signalling the host actually encodes with (CICP code points): `colorPrimaries`
    /// (1=BT.709, 9=BT.2020), `colorTransfer` (1=BT.709, 16=PQ, 18=HLG), `colorMatrix`
    /// (1=BT.709, 9=BT.2020-NCL), `colorFullRange`. BT.709 limited SDR for an older host. Configure
    /// the decoder/presenter from these; mastering metadata arrives via `nextHdrMeta`.
    public private(set) var colorPrimaries: UInt8 = 1
    public private(set) var colorTransfer: UInt8 = 1
    public private(set) var colorMatrix: UInt8 = 1
    public private(set) var colorFullRange: Bool = false
    /// Encoded bit depth (8 or 10).
    public private(set) var bitDepth: UInt8 = 8
    /// The chroma subsampling the host resolved for this session, as the HEVC `chroma_format_idc`:
    /// `1` = 4:2:0 (every pre-4:4:4 host, and the back-compat default) or `3` = full-chroma 4:4:4
    /// (only when this client advertised `videoCap444` *and* the host could open a real 4:4:4
    /// encoder). Drive the decoder's requested pixel format from this. See `isChroma444`.
    public private(set) var chromaFormat: UInt8 = 1
    /// Convenience: the resolved stream is full-chroma 4:4:4 (`chroma_format_idc == 3`).
    public var isChroma444: Bool { chromaFormat == 3 }
    /// True when the negotiated stream is HDR (PQ or HLG transfer) — drive an HDR present path and
    /// drain `nextHdrMeta`.
    public var isHDR: Bool { colorTransfer == 16 || colorTransfer == 18 }

    /// The audio channel count the host resolved for this session (the Welcome's echo of the
    /// requested `audioChannels`, clamped to what the host can capture): `2` (stereo), `6` (5.1)
    /// or `8` (7.1). Build the playback layout from THIS, never the request. `2` for an older host.
    /// PCM from `nextAudioPcm` is interleaved in the canonical wire order FL FR FC LFE RL RR SL SR.
    public private(set) var resolvedAudioChannels: UInt8 = 2

    /// The video codec the host resolved for this session (`Welcome.codec`, `PUNKTFUNK_CODEC_*`):
    /// `2` = HEVC (default / older host), `1` = H.264, `4` = AV1, `8` = PyroWave (only when this
    /// client opted in). Build the decoder from THIS. The resolved value honors the client's
    /// `preferredCodec` when the host could emit it.
    public private(set) var resolvedCodec: UInt8 = 2 // PUNKTFUNK_CODEC_HEVC

    /// The session's negotiated wire shard payload (`Welcome.shard_payload`, bytes) — the
    /// parse-window size for `USER_FLAG_CHUNK_ALIGNED` PyroWave AUs (plan §4.4). Other codecs
    /// never need it.
    public private(set) var shardPayload: UInt32 = 1408

    /// The host capability bitfield (`Welcome.host_caps`): `PUNKTFUNK_HOST_CAP_GAMEPAD_STATE` /
    /// `PUNKTFUNK_HOST_CAP_CLIPBOARD`. `0` for an older host that didn't say.
    public private(set) var hostCaps: UInt8 = 0
    /// Whether this host advertises the shared clipboard (`HOST_CAP_CLIPBOARD`) — the gate for
    /// offering the clipboard toggle. Absent on an older host, or one whose operator policy
    /// (`PUNKTFUNK_CLIPBOARD=off`) keeps the feature dark.
    public var hostSupportsClipboard: Bool {
        hostCaps & UInt8(PUNKTFUNK_HOST_CAP_CLIPBOARD) != 0
    }

    /// The host answered `HOST_CAP_CURSOR`: it stopped compositing the pointer and forwards
    /// shape/state on the cursor planes — the client MUST draw the cursor locally.
    public var hostSupportsCursor: Bool {
        hostCaps & 0x04 != 0
    }

    /// One forwarded host-cursor shape (the cursor channel, ABI v11): straight-alpha RGBA,
    /// `rgba.count == width * height * 4`, hotspot within the bitmap. Cache by `serial` —
    /// states reference shapes by it and a re-shown serial never resends pixels.
    public struct CursorShapeEvent: Sendable {
        public let serial: UInt32
        public let width: Int
        public let height: Int
        public let hotX: Int
        public let hotY: Int
        public let rgba: Data
    }

    /// Per-host-tick cursor state: position (host video px, the pointer/hotspot point),
    /// visibility, and the host-driven relative-mode hint (an app grabbed/hid the pointer ⇒
    /// run captured relative; clear ⇒ absolute, reappearing at `x`/`y`). Latest-wins.
    public struct CursorStateEvent: Sendable {
        public let serial: UInt32
        public let visible: Bool
        public let relativeHint: Bool
        public let x: Int32
        public let y: Int32
    }

    /// Pull the next forwarded cursor SHAPE (nil = timeout). Only a session connected with
    /// `clientCaps` cursor bit against a `hostSupportsCursor` host receives any. Drain shape
    /// AND state from ONE dedicated cursor thread (they share a lock).
    public func nextCursorShape(timeoutMs: UInt32 = 0) throws -> CursorShapeEvent? {
        cursorLock.lock()
        defer { cursorLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }
        var out = PunktfunkCursorShape()
        let rc = punktfunk_connection_next_cursor_shape(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            // Copy out of the ABI borrow (valid until the next shape call) immediately.
            let bytes = out.rgba.map { Data(bytes: $0, count: Int(out.len)) } ?? Data()
            return CursorShapeEvent(
                serial: out.serial, width: Int(out.w), height: Int(out.h),
                hotX: Int(out.hot_x), hotY: Int(out.hot_y), rgba: bytes)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Pull the next cursor STATE (nil = timeout). Latest-wins — drain the queue and apply
    /// only the newest. Same thread + gate as [`nextCursorShape`].
    public func nextCursorState(timeoutMs: UInt32 = 0) throws -> CursorStateEvent? {
        cursorLock.lock()
        defer { cursorLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }
        var out = PunktfunkCursorState()
        let rc = punktfunk_connection_next_cursor_state(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            return CursorStateEvent(
                serial: out.serial,
                visible: out.flags & 0x01 != 0,
                relativeHint: out.flags & 0x02 != 0,
                x: out.x, y: out.y)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }
    /// The resolved codec as a `VideoCodec` (H.264 / HEVC / AV1) — drives the bitstream framing
    /// (Annex-B NAL parsing vs the AV1 OBU repack).
    public var videoCodec: VideoCodec { VideoCodec(wire: resolvedCodec) }

    /// Connect and start a session at the requested mode (the host creates a native virtual
    /// output at exactly this size/refresh). Blocks up to `timeoutMs`.
    ///
    /// `pinSHA256`: the host's expected certificate fingerprint (exactly 32 bytes, else
    /// `invalidPin` is thrown — never silently downgraded); nil = trust on first use
    /// (check `hostFingerprint` afterwards). A pinned mismatch throws.
    ///
    /// `identity`: this client's persistent identity (from `generateIdentity()`, stored in
    /// the Keychain) — presented so a host recognizes a paired client. nil = anonymous;
    /// hosts running `--require-pairing` reject anonymous sessions.
    ///
    /// `compositor`: which backend should drive the virtual output host-side (see
    /// `Compositor`; `.auto` = host decides).
    ///
    /// `gamepad`: which virtual pad the host creates for this session's controllers (see
    /// `GamepadType`; `.auto` = host decides). Check `resolvedGamepad` afterwards.
    ///
    /// `bitrateKbps`: requested video encoder bitrate (0 = host default; the host clamps
    /// to its supported range). Check `resolvedBitrateKbps` afterwards — a speed test
    /// (`startSpeedTest`) is how a client picks an informed value.
    public init(
        host: String, port: UInt16 = 9777,
        width: UInt32, height: UInt32, refreshHz: UInt32,
        pinSHA256: Data? = nil,
        identity: ClientIdentity? = nil,
        compositor: Compositor = .auto,
        gamepad: GamepadType = .auto,
        bitrateKbps: UInt32 = 0,
        videoCaps: UInt8 = 0,
        audioChannels: UInt8 = 2,
        videoCodecs: UInt8 = 0x02, // PUNKTFUNK_CODEC_HEVC — the codecs this client can decode
        preferredCodec: UInt8 = 0, // 0 = auto; else PUNKTFUNK_CODEC_* soft preference
        clientCaps: UInt8 = 0, // ABI v11: PUNKTFUNK_CLIENT_CAP_CURSOR = render the host cursor locally
        launchID: String? = nil,
        timeoutMs: UInt32 = 10_000
    ) throws {
        if let pin = pinSHA256, pin.count != 32 { throw PunktfunkClientError.invalidPin }
        var observed = [UInt8](repeating: 0, count: 32)
        // Why a failed connect failed (PunktfunkStatus): lets a typed host rejection
        // ("denied in the console", "approval timed out", "host busy") surface as
        // `.rejected` instead of the undifferentiated `.connectFailed`.
        var connectStatus: Int32 = 0
        // `videoCaps` advertises decode/present capability (PUNKTFUNK_VIDEO_CAP_10BIT | _HDR): the
        // host upgrades to a 10-bit / BT.2020 PQ stream only when set. 0 = 8-bit BT.709 SDR.
        // `launchID` (a host library id like "steam:570") asks the host to launch that title in
        // the session; the host resolves it against its own library — nil = the host's default.
        handle = host.withCString { cs in
            withOptionalCString(identity?.certPEM) { cert in
                withOptionalCString(identity?.keyPEM) { key in
                    withOptionalCString(launchID) { launch in
                        if let pin = pinSHA256 {
                            return pin.withUnsafeBytes { p in
                                punktfunk_connect_ex9(
                                    cs, port, width, height, refreshHz, compositor.rawValue,
                                    gamepad.rawValue, bitrateKbps, videoCaps, audioChannels,
                                    videoCodecs, preferredCodec, clientCaps, launch,
                                    p.bindMemory(to: UInt8.self).baseAddress, &observed,
                                    cert, key, timeoutMs, &connectStatus)
                            }
                        }
                        return punktfunk_connect_ex9(
                            cs, port, width, height, refreshHz, compositor.rawValue,
                            gamepad.rawValue, bitrateKbps, videoCaps, audioChannels,
                            videoCodecs, preferredCodec, clientCaps, launch,
                            nil, &observed, cert, key, timeoutMs, &connectStatus)
                    }
                }
            }
        }
        guard handle != nil else {
            if let rejection = HostRejection(status: connectStatus) {
                throw PunktfunkClientError.rejected(rejection)
            }
            throw PunktfunkClientError.connectFailed
        }
        hostFingerprint = Data(observed)
        var w: UInt32 = 0, h: UInt32 = 0, hz: UInt32 = 0
        _ = punktfunk_connection_mode(handle, &w, &h, &hz)
        self.width = w
        self.height = h
        self.refreshHz = hz
        var gp: UInt32 = 0
        _ = punktfunk_connection_gamepad(handle, &gp)
        resolvedGamepad = GamepadType(rawValue: gp) ?? .auto
        var comp: UInt32 = 0
        _ = punktfunk_connection_compositor(handle, &comp)
        resolvedCompositor = Compositor(rawValue: comp) ?? .auto
        var offset: Int64 = 0
        _ = punktfunk_connection_clock_offset_ns(handle, &offset)
        clockOffsetNs = offset
        var br: UInt32 = 0
        _ = punktfunk_connection_bitrate(handle, &br)
        resolvedBitrateKbps = br
        var prim: UInt8 = 1, trc: UInt8 = 1, mtx: UInt8 = 1, fullRange: UInt8 = 0, depth: UInt8 = 8
        _ = punktfunk_connection_color_info(handle, &prim, &trc, &mtx, &fullRange, &depth)
        colorPrimaries = prim
        colorTransfer = trc
        colorMatrix = mtx
        colorFullRange = fullRange != 0
        bitDepth = depth
        var cf: UInt8 = 1
        _ = punktfunk_connection_chroma_format(handle, &cf)
        chromaFormat = cf
        var ac: UInt8 = 2
        _ = punktfunk_connection_audio_channels(handle, &ac)
        resolvedAudioChannels = ac
        var codec: UInt8 = 2 // PUNKTFUNK_CODEC_HEVC
        _ = punktfunk_connection_codec(handle, &codec)
        resolvedCodec = codec
        var shard: UInt32 = 1408
        _ = punktfunk_connection_shard_payload(handle, &shard)
        shardPayload = shard
        var caps: UInt8 = 0
        _ = punktfunk_connection_host_caps(handle, &caps)
        hostCaps = caps
    }

    /// A bandwidth speed-test measurement (see `startSpeedTest`). Partial until `done`.
    public struct ProbeResult: Sendable, Equatable {
        /// The host's end-of-burst report arrived — the numbers are final.
        public let done: Bool
        /// Probe payload bytes / packets the client received.
        public let recvBytes: UInt64
        public let recvPackets: UInt32
        /// Probe payload bytes / packets the host reported sending.
        public let hostBytes: UInt64
        public let hostPackets: UInt32
        /// Client-measured receive window (first→last probe AU), milliseconds.
        public let elapsedMs: UInt32
        /// Measured goodput, kilobits per second.
        public let throughputKbps: UInt32
        /// Delivery loss `(hostBytes − recvBytes) / hostBytes`, percent (0 if unknown).
        public let lossPct: Float
    }

    /// Start a bandwidth speed test: the host bursts filler over the data plane at
    /// `targetKbps` of goodput for `durationMs` (clamped host-side to ≤ 3 Gbps / ≤ 5 s),
    /// briefly pausing video. Non-blocking — poll `probeResult()` until `done`. Starting
    /// a probe resets any prior measurement. Silently dropped after close.
    public func startSpeedTest(targetKbps: UInt32, durationMs: UInt32) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_speed_test(h, targetKbps, durationMs)
    }

    /// The current speed-test measurement (zeros before any probe; partial until `done`).
    /// Safe to poll from any thread; nil after close.
    public func probeResult() -> ProbeResult? {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return nil }
        var out = PunktfunkProbeResult()
        guard punktfunk_connection_probe_result(h, &out) == statusOK else { return nil }
        return ProbeResult(
            done: out.done != 0,
            recvBytes: out.recv_bytes, recvPackets: out.recv_packets,
            hostBytes: out.host_bytes, hostPackets: out.host_packets,
            elapsedMs: out.elapsed_ms, throughputKbps: out.throughput_kbps,
            lossPct: out.loss_pct)
    }

    /// Ask the host to switch the live session to a new mode (window resized) — no
    /// reconnect. Non-blocking; on acceptance the stream continues at the new mode (the
    /// first new-mode AU is an IDR with fresh parameter sets — `AnnexB.formatDescription`
    /// refresh-on-IDR already handles it) and `currentMode()` reflects the switch.
    public func requestMode(width: UInt32, height: UInt32, refreshHz: UInt32) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_request_mode(h, width, height, refreshHz)
    }

    /// Ask the host's encoder to emit a fresh IDR keyframe now — recovery when the local
    /// decoder has wedged. The host opens the infinite-GOP stream with one IDR and then sends
    /// P-frames only, so a stalled decode (a lost/corrupt opening IDR, a bad early P-frame —
    /// most likely on the cold first connect) would otherwise stay frozen until the next
    /// loss-triggered recovery keyframe, which may be far off. Fire-and-forget; the recovered
    /// keyframe is the only ack. THROTTLE at the call site — the decode stays wedged for
    /// several frames until the IDR lands, so requesting every frame would flood the control
    /// stream. Silently dropped after close.
    public func requestKeyframe() {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_request_keyframe(h)
    }

    /// Background-keep-alive video drop (opt-in). While true, both video pumps keep DRAINING
    /// `nextAU()` (so QUIC flow control and host pacing stay healthy) but DISCARD each AU before any
    /// VideoToolbox/Metal decode or render — the crash/jetsam-safe way to hold a backgrounded
    /// session (audio keeps rendering; no GPU work off-screen). Set on `SessionModel.enterBackground`,
    /// cleared on `exitBackground` (which then requests a fresh IDR; the pump's re-anchor gate
    /// auto-arms on the resumed frame-index gap). Its own tiny lock — read on the pump thread every
    /// iteration, written on the main actor; never contends the ABI/plane locks.
    private let videoDropLock = NSLock()
    private var videoDropped = false
    public var isVideoDropped: Bool {
        videoDropLock.lock(); defer { videoDropLock.unlock() }
        return videoDropped
    }
    public func setVideoDropped(_ dropped: Bool) {
        videoDropLock.lock(); videoDropped = dropped; videoDropLock.unlock()
    }

    /// Feed each received AU's `frameIndex` (in receive order) so the client recovers from loss with a
    /// cheap reference-frame invalidation instead of always paying for a full IDR. On a forward gap —
    /// a `frameIndex` jump means the intervening frames were lost and the following AUs reference a
    /// picture that never arrived — the core fires a THROTTLED RFI request for the lost range, and an
    /// RFI-capable host (AMD LTR / NVENC) recovers with a clean P-frame rather than a 20-40× IDR
    /// spike. Call it for every received AU; the `framesDropped`-driven `requestKeyframe()` path stays
    /// the backstop for when the recovery frame itself is lost. Cheap; silently dropped after close.
    public func noteFrameIndex(_ frameIndex: UInt32) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_note_frame_index(h, frameIndex, nil)
    }

    /// Like `noteFrameIndex`, but also reports whether the core saw a FORWARD frame-index gap — the
    /// signal that intervening frames were lost and the following AUs reference a picture that never
    /// arrived. The post-loss re-anchor gate arms its display freeze on a gap (the earliest, most
    /// precise loss trigger — ahead of the `framesDropped` climb). Same core side effect as
    /// `noteFrameIndex` (the throttled RFI request); call it for every received AU. Returns false
    /// after close.
    public func noteFrameIndexGap(_ frameIndex: UInt32) -> Bool {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return false }
        var gap = false
        _ = punktfunk_connection_note_frame_index(h, frameIndex, &gap)
        return gap
    }

    /// Cumulative access units the host→client reassembler dropped as unrecoverable (FEC couldn't
    /// rebuild them). The video pump polls this and calls `requestKeyframe()` when it climbs — the
    /// correct loss trigger under the host's infinite GOP, where unrecoverable loss yields
    /// reference-missing delta frames the decoder *silently conceals* (a frozen / garbage picture,
    /// no decode error and no `.failed` layer), so a decode-error trigger rarely fires. Monotonic
    /// for the session; 0 after close. Cheap (an atomic load) — safe to poll every pump iteration.
    public func framesDropped() -> UInt64 {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return 0 }
        var out: UInt64 = 0
        _ = punktfunk_connection_frames_dropped(h, &out)
        return out
    }

    /// Report one decoded frame's decode-stage latency, in microseconds (the AU leaving `nextAU`
    /// through its VideoToolbox output). This feeds the Automatic bitrate controller's decode
    /// signal — the only one that sees this device's decoder — so the rate is capped at the real
    /// decode limit instead of climbing to the network link ceiling and choking the decoder. Cheap;
    /// silently dropped after close. Only worth calling when `wantsDecodeLatency()` is true.
    public func reportDecodeUs(_ us: UInt32) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_report_decode_us(h, us)
    }

    /// Whether `reportDecodeUs` is worth calling this session: true only when the adaptive-bitrate
    /// controller is armed (Automatic bitrate, non-PyroWave). Query once — constant for the session
    /// — and skip the per-frame decode measurement entirely when it's false. False after close.
    public func wantsDecodeLatency() -> Bool {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return false }
        var out = false
        _ = punktfunk_connection_wants_decode_latency(h, &out)
        return out
    }

    /// The currently active session mode (updated by accepted `requestMode` switches).
    public func currentMode() -> (width: UInt32, height: UInt32, refreshHz: UInt32) {
        abiLock.lock()
        defer { abiLock.unlock() }
        var w: UInt32 = 0, h: UInt32 = 0, hz: UInt32 = 0
        if let hd = handle, !closeRequested {
            _ = punktfunk_connection_mode(hd, &w, &h, &hz)
        }
        return (w, h, hz)
    }

    /// Pull the next access unit; nil on timeout, throws `.closed` once the session ended.
    /// Call from a single pump thread.
    public func nextAU(timeoutMs: UInt32 = 100) throws -> AccessUnit? {
        pumpLock.lock()
        defer { pumpLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var frame = PunktfunkFrame()
        let rc = punktfunk_connection_next_au(h, &frame, timeoutMs)
        switch rc {
        case statusOK:
            guard let base = frame.data, frame.len > 0 else { return nil }
            let data = Data(bytes: base, count: Int(frame.len)) // copy: ptr valid only until next call
            var ts = timespec()
            clock_gettime(CLOCK_REALTIME, &ts)
            let pulledNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
            // Receipt = the core's reassembly-completion stamp (ABI v9); the pull instant is
            // kept separately so the client-queue wait is its own measured term. 0 would mean a
            // pre-v9 core — impossible here (core and Kit ship in one binary), but fall back to
            // the pull instant rather than record a 1970 receipt.
            let receivedNs = frame.received_ns > 0 ? Int64(frame.received_ns) : pulledNs
            return AccessUnit(
                data: data, ptsNs: frame.pts_ns,
                frameIndex: frame.frame_index, flags: frame.flags,
                receivedNs: receivedNs, pulledNs: pulledNs)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Pull the next Opus audio packet; nil on timeout, throws `.closed` once the session
    /// ended. Drain from a dedicated audio thread — packets arrive every 5 ms (the core
    /// buffers 320 ms and drops the newest when the puller lags).
    public func nextAudio(timeoutMs: UInt32 = 100) throws -> AudioPacket? {
        audioLock.lock()
        defer { audioLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var pkt = PunktfunkAudioPacket()
        let rc = punktfunk_connection_next_audio(h, &pkt, timeoutMs)
        switch rc {
        case statusOK:
            guard let base = pkt.data, pkt.len > 0 else { return nil }
            let data = Data(bytes: base, count: Int(pkt.len)) // copy: ptr valid only until next call
            return AudioPacket(data: data, ptsNs: pkt.pts_ns, seq: pkt.seq)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// One decoded audio frame from `nextAudioPcm`: interleaved 32-bit float at 48 kHz, in the
    /// canonical wire channel order FL FR FC LFE RL RR SL SR (the first `channels`).
    public struct AudioPCM: Sendable {
        /// Interleaved f32 samples (`frameCount * channels` long), wire channel order.
        public let samples: [Float]
        /// Samples per channel.
        public let frameCount: Int
        /// Channel count (2/6/8) — `resolvedAudioChannels`.
        public let channels: Int
        public let ptsNs: UInt64
        public let seq: UInt32
    }

    /// Pull the next audio frame, **decoded in-core** to interleaved f32 PCM — Apple's AudioToolbox
    /// Opus path is stereo-only, so surround (and, for uniformity, stereo too) is decoded by the
    /// Rust core (libopus multistream) and handed back as PCM. nil on timeout, throws `.closed` once
    /// the session ended. Drain from a dedicated audio thread (do NOT also call `nextAudio` — they
    /// share the underlying queue). The returned `samples` are copied out, so the buffer is owned.
    public func nextAudioPcm(timeoutMs: UInt32 = 100) throws -> AudioPCM? {
        audioLock.lock()
        defer { audioLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var out = PunktfunkAudioPcm()
        let rc = punktfunk_connection_next_audio_pcm(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            let channels = Int(out.channels)
            let total = Int(out.frame_count) * channels
            guard let base = out.samples, total > 0 else { return nil }
            // Copy: the pointer borrows connection memory only until the next PCM call.
            let samples = Array(UnsafeBufferPointer(start: base, count: total))
            return AudioPCM(
                samples: samples, frameCount: Int(out.frame_count),
                channels: channels, ptsNs: out.pts_ns, seq: out.seq)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Pull the next force-feedback update for the GCController haptics engine:
    /// `(pad, lowFrequency, highFrequency)` with 0...0xFFFF amplitudes, (0, 0) = stop.
    /// Drain from the (single) feedback thread, alongside `nextHidOutput`. Drops the v2
    /// self-termination TTL — use `nextRumble2` to honor the host lease.
    public func nextRumble(timeoutMs: UInt32 = 0) throws -> (pad: UInt16, low: UInt16, high: UInt16)? {
        feedbackLock.lock()
        defer { feedbackLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var pad: UInt16 = 0, low: UInt16 = 0, high: UInt16 = 0
        let rc = punktfunk_connection_next_rumble(h, &pad, &low, &high, timeoutMs)
        switch rc {
        case statusOK:
            return (pad, low, high)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Pull the next force-feedback update *including its self-termination TTL* (v2 envelopes):
    /// `(pad, low, high, ttlMs)`. `ttlMs` is how long to render this level before silencing unless
    /// the host renews it; `RumbleTuning.noTTL` (`UInt32.max`) means "no lease" — a legacy host, so
    /// fall back to a client-side staleness timeout. The reorder gate (seq) already ran in the
    /// core, so a stale/reordered envelope never surfaces here. Drain from the (single) feedback
    /// thread, alongside `nextHidOutput`.
    public func nextRumble2(timeoutMs: UInt32 = 0) throws
        -> (pad: UInt16, low: UInt16, high: UInt16, ttlMs: UInt32)?
    {
        feedbackLock.lock()
        defer { feedbackLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var pad: UInt16 = 0, low: UInt16 = 0, high: UInt16 = 0, ttl: UInt32 = .max
        let rc = punktfunk_connection_next_rumble2(h, &pad, &low, &high, &ttl, timeoutMs)
        switch rc {
        case statusOK:
            return (pad, low, high, ttl)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Pull the next EFFECTIVE rumble command from the core's shared rumble policy engine — the
    /// uniform replacement for per-platform rumble policy. The engine owns every decision
    /// (v2 lease expiry, legacy-host staleness at a uniform 1 s, connection-close drain zeros),
    /// so apply commands verbatim: `(0, 0)` = stop now, non-zero = run at this level.
    /// `backstopMs` is a safety-net duration for duration-parameterized platform APIs — the
    /// CoreHaptics renderer ignores it (its finite segment ceiling is the equivalent net).
    /// Drain from the (single) feedback thread, alongside `nextHidOutput`.
    public func nextRumbleCommand(timeoutMs: UInt32 = 0) throws
        -> (pad: UInt16, low: UInt16, high: UInt16, backstopMs: UInt32)?
    {
        feedbackLock.lock()
        defer { feedbackLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var pad: UInt16 = 0, low: UInt16 = 0, high: UInt16 = 0, backstop: UInt32 = 0
        let rc = punktfunk_connection_next_rumble_cmd(h, &pad, &low, &high, &backstop, timeoutMs)
        switch rc {
        case statusOK:
            return (pad, low, high, backstop)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// One DualSense feedback event a game wrote to the host's virtual pad — replay it on
    /// the real controller (GCDeviceLight, GCControllerPlayerIndex,
    /// GCDualSenseAdaptiveTrigger). Only a `.dualSense` session emits these.
    public enum HidOutputEvent: Sendable, Equatable {
        /// Lightbar color.
        case led(pad: UInt8, r: UInt8, g: UInt8, b: UInt8)
        /// Player-indicator LEDs (low 5 bits).
        case playerLEDs(pad: UInt8, bits: UInt8)
        /// Adaptive-trigger effect: `which` 0 = L2, 1 = R2; `effect` is the raw DualSense
        /// trigger parameter block (mode byte + params, ≤ 11 bytes) — parse with
        /// `DualSenseTriggerEffect`.
        case triggerEffect(pad: UInt8, which: UInt8, effect: [UInt8])
    }

    /// Pull the next PlayStation-pad feedback event (lightbar / player LEDs / adaptive
    /// triggers); nil on timeout, throws `.closed` once the session ended. Drain from the
    /// (single) feedback thread, alongside `nextRumble`. Nothing arrives unless the session's
    /// virtual pad is a DualSense (all three) or a DualShock 4 (lightbar only) — poll with a
    /// short timeout, never spin.
    public func nextHidOutput(timeoutMs: UInt32 = 0) throws -> HidOutputEvent? {
        feedbackLock.lock()
        defer { feedbackLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var out = PunktfunkHidOutput()
        let rc = punktfunk_connection_next_hidout(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            switch Int32(out.kind) {
            case PUNKTFUNK_HIDOUT_LED:
                return .led(pad: out.pad, r: out.r, g: out.g, b: out.b)
            case PUNKTFUNK_HIDOUT_PLAYER_LEDS:
                return .playerLEDs(pad: out.pad, bits: out.player_bits)
            case PUNKTFUNK_HIDOUT_TRIGGER:
                // The fixed C array imports as a tuple — copy out the valid prefix.
                let len = Int(min(out.effect_len, UInt8(PUNKTFUNK_HID_EFFECT_MAX)))
                let effect = withUnsafeBytes(of: out.effect) { Array($0.prefix(len)) }
                return .triggerEffect(pad: out.pad, which: out.which, effect: effect)
            default:
                return nil // unknown kind from a newer host — skip (forward-compatible)
            }
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Video-capability bit: the client can decode a 10-bit (Main10) HEVC stream.
    public static let videoCap10Bit: UInt8 = UInt8(PUNKTFUNK_VIDEO_CAP_10BIT)
    /// Video-capability bit: the client can present BT.2020 PQ HDR10 (implies 10-bit).
    public static let videoCapHDR: UInt8 = UInt8(PUNKTFUNK_VIDEO_CAP_HDR)
    /// Video-capability bit: the client can decode a full-chroma 4:4:4 HEVC stream (Range
    /// Extensions). Advertise only when the device can *hardware*-decode it (`Stage444Probe`);
    /// the host then emits 4:4:4 only if it too opted in. `chromaFormat` reflects the real value.
    public static let videoCap444: UInt8 = UInt8(PUNKTFUNK_VIDEO_CAP_444)

    /// Codec bits for `videoCodecs` / `preferredCodec` and the value `resolvedCodec` returns.
    public static let codecH264: UInt8 = UInt8(PUNKTFUNK_CODEC_H264)
    public static let codecHEVC: UInt8 = UInt8(PUNKTFUNK_CODEC_HEVC)
    public static let codecAV1: UInt8 = UInt8(PUNKTFUNK_CODEC_AV1)
    /// PyroWave (opt-in wired-LAN wavelet codec, 8-bit SDR): the host only ever resolves it
    /// when the client both advertises the bit AND names it `preferredCodec` — never
    /// auto-selected. Decoded by the Metal wavelet decoder, not VideoToolbox.
    public static let codecPyroWave: UInt8 = UInt8(PUNKTFUNK_CODEC_PYROWAVE)

    /// `AccessUnit.flags` bit: the AU is shard-aligned self-delimiting chunks (the wire's
    /// `USER_FLAG_CHUNK_ALIGNED`, PyroWave datagram-aligned mode §4.4) — walk it
    /// window-by-window at `shardPayload`. (The C `#define` doesn't import into Swift.)
    public static let userFlagChunkAligned: UInt32 = 64

    /// Static HDR mastering metadata (SMPTE ST.2086 + content light level) the host sent for an HDR
    /// session. Mirrors the wire/ABI `PunktfunkHdrMeta`; primaries are in ST.2086 **G, B, R** order,
    /// 1/50000 units; mastering luminance in 0.0001 cd/m²; MaxCLL/MaxFALL in nits.
    public struct HdrMeta: Sendable, Equatable {
        public let primariesX: [UInt16] // [green, blue, red]
        public let primariesY: [UInt16]
        public let whitePointX: UInt16
        public let whitePointY: UInt16
        public let maxMasteringLuminance: UInt32 // 0.0001 cd/m²
        public let minMasteringLuminance: UInt32 // 0.0001 cd/m²
        public let maxCLL: UInt16
        public let maxFALL: UInt16

        /// The 24-byte `mastering_display_colour_volume` payload (big-endian, ST.2086 G,B,R) — pass
        /// directly to `kCVImageBufferMasteringDisplayColorVolumeKey` or `CAEDRMetadata`'s displayInfo.
        public func masteringDisplayColorVolume() -> Data {
            var d = Data()
            func be16(_ v: UInt16) { d.append(UInt8(v >> 8)); d.append(UInt8(v & 0xFF)) }
            func be32(_ v: UInt32) {
                d.append(UInt8((v >> 24) & 0xFF)); d.append(UInt8((v >> 16) & 0xFF))
                d.append(UInt8((v >> 8) & 0xFF)); d.append(UInt8(v & 0xFF))
            }
            for i in 0..<3 { be16(primariesX[i]); be16(primariesY[i]) } // G, B, R
            be16(whitePointX); be16(whitePointY)
            be32(maxMasteringLuminance); be32(minMasteringLuminance)
            return d
        }

        /// The 4-byte `content_light_level_info` payload (big-endian: MaxCLL, MaxFALL) — for
        /// `kCVImageBufferContentLightLevelInfoKey` or `CAEDRMetadata`'s contentInfo.
        public func contentLightLevelInfo() -> Data {
            var d = Data()
            func be16(_ v: UInt16) { d.append(UInt8(v >> 8)); d.append(UInt8(v & 0xFF)) }
            be16(maxCLL); be16(maxFALL)
            return d
        }
    }

    /// Pull the next static HDR metadata update; nil on timeout, throws `.closed` once the session
    /// ended. Drain from the feedback thread alongside `nextRumble`/`nextHidOutput`. Nothing arrives
    /// unless `isHDR` — poll with a short timeout, never spin.
    public func nextHdrMeta(timeoutMs: UInt32 = 0) throws -> HdrMeta? {
        feedbackLock.lock()
        defer { feedbackLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var out = PunktfunkHdrMeta()
        let rc = punktfunk_connection_next_hdr_meta(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            // The fixed C `uint16_t[3]` arrays import as tuples — copy them out.
            let px = withUnsafeBytes(of: out.display_primaries_x) {
                Array($0.bindMemory(to: UInt16.self))
            }
            let py = withUnsafeBytes(of: out.display_primaries_y) {
                Array($0.bindMemory(to: UInt16.self))
            }
            return HdrMeta(
                primariesX: px, primariesY: py,
                whitePointX: out.white_point_x, whitePointY: out.white_point_y,
                maxMasteringLuminance: out.max_display_mastering_luminance,
                minMasteringLuminance: out.min_display_mastering_luminance,
                maxCLL: out.max_cll, maxFALL: out.max_fall)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// One per-AU host-timing report (0xCF): the host's capture→fully-sent duration for the
    /// access unit whose `AccessUnit.ptsNs` equals `ptsNs` exactly. The stats consumer derives
    /// `network = (receivedNs + clockOffsetNs − ptsNs) − hostUs` — the host/network split of the
    /// HUD's `host+network` stage (design/stats-unification.md Phase 2).
    public struct HostTiming: Sendable, Equatable {
        /// The AU's capture stamp (host capture clock — matches the AU's `ptsNs`).
        public let ptsNs: UInt64
        /// Host capture→sent duration, µs.
        public let hostUs: UInt32
    }

    /// Pull the next per-AU host timing; nil on timeout, throws `.closed` once the session
    /// ended. Best-effort plane: an older host never emits any — keep showing the combined
    /// `host+network` stage then. Drain non-blockingly (`timeoutMs: 0`) from ONE stats
    /// consumer (its own core plane, safe alongside the other pullers).
    public func nextHostTiming(timeoutMs: UInt32 = 0) throws -> HostTiming? {
        statsLock.lock()
        defer { statsLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }

        var out = PunktfunkHostTiming()
        let rc = punktfunk_connection_next_host_timing(h, &out, timeoutMs)
        switch rc {
        case statusOK:
            return HostTiming(ptsNs: out.pts_ns, hostUs: out.host_us)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Send one input event (delivered to the host as a QUIC datagram). Thread-safe;
    /// silently dropped after close.
    public func send(_ event: PunktfunkInputEvent) {
        var ev = event
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_send_input(h, &ev)
    }

    /// Signal a **deliberate** user-initiated quit before ``close()``: the connection closes with
    /// `QUIT_CLOSE_CODE` (81) so the host tears the session down immediately instead of holding the
    /// keep-alive linger for a reconnect. Call only from an explicit "Disconnect" action — NOT from a
    /// network drop / host-ended / app-background (those keep the linger). Idempotent, safe pre-close.
    public func disconnectQuit() {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        punktfunk_connection_disconnect_quit(h)
    }

    /// Close the connection and free the handle. Safe from any thread, idempotent; waits
    /// for in-flight pulls (≤ their timeouts) before tearing down.
    public func close() {
        abiLock.lock()
        closeRequested = true
        abiLock.unlock()
        pumpLock.lock() // pullers exit at their next poll boundary, releasing these
        audioLock.lock()
        feedbackLock.lock()
        statsLock.lock()
        clipboardLock.lock()
        cursorLock.lock()
        abiLock.lock()
        let h = handle
        handle = nil
        abiLock.unlock()
        cursorLock.unlock()
        clipboardLock.unlock()
        statsLock.unlock()
        feedbackLock.unlock()
        audioLock.unlock()
        pumpLock.unlock()
        if let h {
            punktfunk_connection_close(h) // joins the connection's internal Rust threads
        }
    }

    /// Send one Opus mic frame (48 kHz) to the host, where it feeds a virtual
    /// microphone source the host's apps can record. Non-blocking enqueue, safe
    /// alongside the pull threads (same discipline as `send`). `seq`/`ptsNs` are the
    /// caller's own counters (host uses them only for diagnostics); empty `opus` is a
    /// DTX silence frame.
    public func sendMic(_ opus: Data, seq: UInt32, ptsNs: UInt64) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        opus.withUnsafeBytes { p in
            _ = punktfunk_connection_send_mic(
                h, p.bindMemory(to: UInt8.self).baseAddress, UInt(opus.count), seq, ptsNs)
        }
    }

    /// Send one DualSense touchpad contact to the host's virtual pad (rich-input plane).
    /// `x`/`y` are normalized 0...65535 across the touchpad, origin top-left, +y down.
    /// Non-blocking enqueue (same discipline as `send`); pointless on non-DualSense
    /// sessions — the host ignores it there.
    public func sendTouchpad(pad: UInt8 = 0, finger: UInt8, active: Bool, x: UInt16, y: UInt16) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        var rich = PunktfunkRichInput()
        rich.kind = UInt8(PUNKTFUNK_RICH_TOUCHPAD)
        rich.pad = pad
        rich.finger = finger
        rich.active = active ? 1 : 0
        rich.x = x
        rich.y = y
        _ = punktfunk_connection_send_rich_input(h, &rich)
    }

    /// Send one DualSense motion sample to the host's virtual pad (rich-input plane). The
    /// values are raw DualSense sensor units, written verbatim into the virtual pad's input
    /// report — convert with `GamepadCapture`'s scale constants (gyro: rad/s → 20 LSB per
    /// deg/s; accel: g → 10000 LSB per g).
    public func sendMotion(
        pad: UInt8 = 0,
        gyro: (Int16, Int16, Int16), accel: (Int16, Int16, Int16)
    ) {
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        var rich = PunktfunkRichInput()
        rich.kind = UInt8(PUNKTFUNK_RICH_MOTION)
        rich.pad = pad
        rich.gyro = gyro
        rich.accel = accel
        _ = punktfunk_connection_send_rich_input(h, &rich)
    }

    // MARK: - Shared clipboard (design/clipboard-and-file-transfer.md §5)

    /// One advertised clipboard format in a lazy offer — the format list crosses the wire,
    /// the bytes only on a fetch.
    public struct ClipKind: Sendable, Equatable {
        public let mime: String
        /// Best-effort size in bytes; `0` = unknown.
        public let sizeHint: UInt64
        public init(mime: String, sizeHint: UInt64 = 0) {
            self.mime = mime
            self.sizeHint = sizeHint
        }
    }

    /// A shared-clipboard event from `nextClipboard`. The drain thread turns these into
    /// NSPasteboard operations (`ClipboardSync`).
    public enum ClipEvent: Sendable, Equatable {
        /// The host copied: its lazy format list (empty = the host clipboard was cleared).
        /// Fetch a format with `clipFetch(seq:mime:)` when a local app pastes.
        case remoteOffer(seq: UInt32, kinds: [ClipKind])
        /// Host ack / policy / backend update for `clipControl` (`CLIP_REASON_*`).
        case state(enabled: Bool, policy: UInt8, reason: UInt8)
        /// The host is pasting OUR offered data — answer with `clipServe(reqId:...)`.
        case fetchRequest(reqId: UInt32, seq: UInt32, fileIndex: UInt32, mime: String)
        /// Bytes for a fetch we started (`last` = final chunk).
        case data(xferId: UInt32, chunk: Data, last: Bool)
        /// A transfer was cancelled (either side).
        case cancelled(id: UInt32)
        /// A transfer failed (`status` = a PunktfunkStatus code).
        case error(id: UInt32, status: Int32)
    }

    /// Enable/disable the shared clipboard for this session. Opt-in: nothing is announced or
    /// served until enabled. The host answers with a `.state` event carrying the resolved
    /// outcome (its operator policy is authoritative). Best-effort — a dropped call on a
    /// closing session is fine.
    public func clipControl(enabled: Bool, flags: UInt8 = 0) {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { return }
        _ = punktfunk_connection_clipboard_control(h, enabled, flags)
    }

    /// Announce that the local pasteboard changed — the lazy format-list offer (`seq` monotonic,
    /// newest wins; empty `kinds` clears the host side). The bytes cross only if the host fetches.
    public func clipOffer(seq: UInt32, kinds: [ClipKind]) {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { return }
        guard !kinds.isEmpty else {
            _ = punktfunk_connection_clipboard_offer(h, seq, nil, 0)
            return
        }
        // The C array borrows NUL-terminated strings for the duration of the call only.
        let cStrings = kinds.map { strdup($0.mime) }
        defer { cStrings.forEach { free($0) } }
        let arr = zip(cStrings, kinds).map {
            PunktfunkClipKind(mime: $0.map { UnsafePointer($0) }, size_hint: $1.sizeHint)
        }
        _ = arr.withUnsafeBufferPointer {
            punktfunk_connection_clipboard_offer(h, seq, $0.baseAddress, UInt(arr.count))
        }
    }

    /// Start pulling one format of the host's offer `seq` (a local app is pasting). Returns the
    /// transfer id echoed on the resulting `.data`/`.error`/`.cancelled` events, or nil when the
    /// session is closing.
    public func clipFetch(seq: UInt32, mime: String, fileIndex: UInt32 = UInt32.max) -> UInt32? {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { return nil }
        var xfer: UInt32 = 0
        let rc = mime.withCString {
            punktfunk_connection_clipboard_fetch(h, seq, $0, fileIndex, &xfer)
        }
        return rc == statusOK ? xfer : nil
    }

    /// Provide bytes answering a `.fetchRequest` (the host is pasting our offered data). Call
    /// repeatedly to stream; `last = true` completes the transfer. An empty final chunk is fine.
    public func clipServe(reqId: UInt32, data: Data, last: Bool) {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { return }
        if data.isEmpty {
            _ = punktfunk_connection_clipboard_serve(h, reqId, nil, 0, last)
        } else {
            data.withUnsafeBytes { p in
                _ = punktfunk_connection_clipboard_serve(
                    h, reqId, p.bindMemory(to: UInt8.self).baseAddress, UInt(data.count), last)
            }
        }
    }

    /// Cancel a clipboard transfer by id — an outbound fetch's `xferId` or an inbound
    /// `.fetchRequest`'s `reqId`.
    public func clipCancel(id: UInt32) {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { return }
        _ = punktfunk_connection_clipboard_cancel(h, id)
    }

    /// Pull the next shared-clipboard event; nil on timeout, throws `.closed` once the session
    /// ended. Drain from a single dedicated thread (`ClipboardSync`) — the event's borrowed
    /// payload is copied into the returned `ClipEvent` before the next poll can overwrite it.
    public func nextClipboard(timeoutMs: UInt32) throws -> ClipEvent? {
        clipboardLock.lock()
        defer { clipboardLock.unlock() }
        guard let h = liveHandle() else { throw PunktfunkClientError.closed }
        var ev = PunktfunkClipEvent()
        let rc = punktfunk_connection_next_clipboard(h, &ev, timeoutMs)
        switch rc {
        case statusOK:
            return Self.decodeClipEvent(ev)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw PunktfunkClientError.closed
        default:
            throw PunktfunkClientError.status(rc)
        }
    }

    /// Copy a raw C clip event (whose `data` borrows a per-connection slot) into an owned Swift
    /// value. Unknown kinds (a newer core) decode to nil and are skipped by the drain.
    private static func decodeClipEvent(_ ev: PunktfunkClipEvent) -> ClipEvent? {
        let payload = ev.data.map { Data(bytes: $0, count: Int(ev.len)) } ?? Data()
        switch Int32(ev.kind) {
        case PUNKTFUNK_CLIP_REMOTE_OFFER:
            // One `mime\tsize_hint\n` line per advertised format.
            let kinds = String(decoding: payload, as: UTF8.self)
                .split(separator: "\n")
                .compactMap { line -> ClipKind? in
                    let parts = line.split(separator: "\t", maxSplits: 1)
                    guard let mime = parts.first, !mime.isEmpty else { return nil }
                    let hint = parts.count > 1 ? UInt64(parts[1]) ?? 0 : 0
                    return ClipKind(mime: String(mime), sizeHint: hint)
                }
            return .remoteOffer(seq: ev.transfer_id, kinds: kinds)
        case PUNKTFUNK_CLIP_STATE:
            return .state(enabled: ev.enabled != 0, policy: ev.policy, reason: ev.reason)
        case PUNKTFUNK_CLIP_FETCH_REQUEST:
            return .fetchRequest(
                reqId: ev.transfer_id, seq: ev.seq, fileIndex: ev.file_index,
                mime: String(decoding: payload, as: UTF8.self))
        case PUNKTFUNK_CLIP_DATA:
            return .data(xferId: ev.transfer_id, chunk: payload, last: ev.last != 0)
        case PUNKTFUNK_CLIP_CANCELLED:
            return .cancelled(id: ev.transfer_id)
        case PUNKTFUNK_CLIP_ERROR:
            return .error(id: ev.transfer_id, status: ev.status)
        default:
            return nil
        }
    }

    deinit { close() }

    /// Snapshot the handle unless close is pending (callers hold their plane lock).
    private func liveHandle() -> OpaquePointer? {
        abiLock.lock()
        defer { abiLock.unlock() }
        return closeRequested ? nil : handle
    }
}
