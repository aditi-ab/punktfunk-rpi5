// Swift wrapper around the punktfunk-core C ABI's punktfunk/1 connection API.
//
// Threading contract (mirrors the C header): one PunktfunkConnection is pumped from a single
// video thread via nextAU(); nextAudio()/nextRumble() may each run on their own (single)
// drain thread — the core keeps per-plane borrow slots, so the planes never alias;
// send() is enqueue-only and safe alongside all of them. The pointers inside an AU/audio
// packet are only valid until the next call of the same kind, so we copy into Data here —
// the copies are small and keep the Swift side memory-safe.
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
}

/// This client's persistent self-signed identity. Generate ONCE with `generateIdentity()`,
/// store both PEMs (Keychain), present on every connect — the certificate's fingerprint is
/// how hosts recognize this client after pairing.
public struct ClientIdentity: Sendable {
    public let certPEM: String
    public let keyPEM: String
    public init(certPEM: String, keyPEM: String) {
        self.certPEM = certPEM
        self.keyPEM = keyPEM
    }
}

/// Generate a fresh client identity (self-signed cert + key, PEM).
public func generateIdentity() throws -> ClientIdentity {
    var cert = [CChar](repeating: 0, count: 4096)
    var key = [CChar](repeating: 0, count: 4096)
    let rc = punktfunk_generate_identity(&cert, cert.count, &key, key.count)
    guard rc.rawValue == PUNKTFUNK_STATUS_OK.rawValue else {
        throw PunktfunkClientError.status(rc.rawValue)
    }
    return ClientIdentity(certPEM: String(cString: cert), keyPEM: String(cString: key))
}

/// Run the PIN pairing ceremony: the host displays a 4-digit PIN (its log/UI), the user
/// types it here. On success the host stores this client's identity and the returned
/// fingerprint is the host's now-VERIFIED identity — persist it and pass it as `pinSHA256`
/// to every later connect. Throws `.wrongPIN` when the proof is rejected.
public func pair(
    host: String, port: UInt16 = 9777,
    identity: ClientIdentity, pin: String, name: String,
    timeoutMs: UInt32 = 90_000
) throws -> Data {
    var observed = [UInt8](repeating: 0, count: 32)
    let rc = host.withCString { cs in
        identity.certPEM.withCString { cert in
            identity.keyPEM.withCString { key in
                pin.withCString { p in
                    name.withCString { n in
                        punktfunk_pair(cs, port, cert, key, p, n, &observed, timeoutMs)
                    }
                }
            }
        }
    }
    switch rc.rawValue {
    case PUNKTFUNK_STATUS_OK.rawValue: return Data(observed)
    case PUNKTFUNK_STATUS_CRYPTO.rawValue: throw PunktfunkClientError.wrongPIN
    default: throw PunktfunkClientError.status(rc.rawValue)
    }
}

/// `withCString` over an optional — nil maps to a NULL C pointer.
func withOptionalCString<R>(_ s: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
    guard let s else { return body(nil) }
    return s.withCString { body($0) }
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
    /// Same role for the audio/rumble drain thread (its own plane in the core).
    private let audioLock = NSLock()

    /// Negotiated session mode (host-confirmed).
    public private(set) var width: UInt32 = 0
    public private(set) var height: UInt32 = 0
    public private(set) var refreshHz: UInt32 = 0

    /// SHA-256 fingerprint of the certificate the host presented (32 bytes). After a
    /// trust-on-first-use connect, persist this and pass it as `pinSHA256` next time.
    public private(set) var hostFingerprint: Data = Data()

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
    public init(
        host: String, port: UInt16 = 9777,
        width: UInt32, height: UInt32, refreshHz: UInt32,
        pinSHA256: Data? = nil,
        identity: ClientIdentity? = nil,
        timeoutMs: UInt32 = 10_000
    ) throws {
        if let pin = pinSHA256, pin.count != 32 { throw PunktfunkClientError.invalidPin }
        var observed = [UInt8](repeating: 0, count: 32)
        handle = host.withCString { cs in
            withOptionalCString(identity?.certPEM) { cert in
                withOptionalCString(identity?.keyPEM) { key in
                    if let pin = pinSHA256 {
                        return pin.withUnsafeBytes { p in
                            punktfunk_connect(
                                cs, port, width, height, refreshHz,
                                p.bindMemory(to: UInt8.self).baseAddress, &observed,
                                cert, key, timeoutMs)
                        }
                    }
                    return punktfunk_connect(
                        cs, port, width, height, refreshHz, nil, &observed, cert, key, timeoutMs)
                }
            }
        }
        guard handle != nil else { throw PunktfunkClientError.connectFailed }
        hostFingerprint = Data(observed)
        var w: UInt32 = 0, h: UInt32 = 0, hz: UInt32 = 0
        _ = punktfunk_connection_mode(handle, &w, &h, &hz)
        self.width = w
        self.height = h
        self.refreshHz = hz
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
            return AccessUnit(
                data: data, ptsNs: frame.pts_ns,
                frameIndex: frame.frame_index, flags: frame.flags)
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

    /// Pull the next force-feedback update for the GCController haptics engine:
    /// `(pad, lowFrequency, highFrequency)` with 0...0xFFFF amplitudes, (0, 0) = stop.
    /// Shares the audio drain thread's plane (call from that thread).
    public func nextRumble(timeoutMs: UInt32 = 0) throws -> (pad: UInt16, low: UInt16, high: UInt16)? {
        audioLock.lock()
        defer { audioLock.unlock() }
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

    /// Send one input event (delivered to the host as a QUIC datagram). Thread-safe;
    /// silently dropped after close.
    public func send(_ event: PunktfunkInputEvent) {
        var ev = event
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = punktfunk_connection_send_input(h, &ev)
    }

    /// Close the connection and free the handle. Safe from any thread, idempotent; waits
    /// for in-flight pulls (≤ their timeouts) before tearing down.
    public func close() {
        abiLock.lock()
        closeRequested = true
        abiLock.unlock()
        pumpLock.lock() // pullers exit at their next poll boundary, releasing these
        audioLock.lock()
        abiLock.lock()
        let h = handle
        handle = nil
        abiLock.unlock()
        audioLock.unlock()
        pumpLock.unlock()
        if let h {
            punktfunk_connection_close(h) // joins the connection's internal Rust threads
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

// Convenience constructors for the wire input events (field semantics match
// punktfunk_core::input::InputEvent; see punktfunk_core.h).
public extension PunktfunkInputEvent {
    private static func make(
        _ kind: UInt32, code: UInt32, x: Int32, y: Int32, flags: UInt32 = 0
    ) -> PunktfunkInputEvent {
        PunktfunkInputEvent(kind: UInt8(kind), _pad: (0, 0, 0), code: code, x: x, y: y, flags: flags)
    }
    static func mouseMove(dx: Int32, dy: Int32) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_MOUSE_MOVE.rawValue, code: 0, x: dx, y: dy)
    }
    /// GameStream button ids: 1=left 2=middle 3=right 4=X1 5=X2 (host maps to evdev BTN_*).
    static func mouseButton(_ button: UInt32, down: Bool) -> PunktfunkInputEvent {
        make(
            (down ? PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_DOWN : PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_UP).rawValue,
            code: button, x: 0, y: 0)
    }
    /// `vk` is a Windows virtual-key code (the host's vk_to_evdev table consumes these).
    static func key(_ vk: UInt32, down: Bool) -> PunktfunkInputEvent {
        make((down ? PUNKTFUNK_INPUT_KIND_KEY_DOWN : PUNKTFUNK_INPUT_KIND_KEY_UP).rawValue, code: vk, x: 0, y: 0)
    }
    /// WHEEL_DELTA(120)-scaled; positive = up (vertical) / right (horizontal) — the
    /// convention Moonlight/SDL use; the host maps onto the ei/wl axes.
    static func scroll(_ delta: Int32, horizontal: Bool = false) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_MOUSE_SCROLL.rawValue, code: horizontal ? 1 : 0, x: delta, y: 0)
    }

    // Gamepad (wire contract in punktfunk_core::input::gamepad): one transition per event,
    // `pad` = controller index, accumulated host-side into a virtual Xbox 360 pad.

    /// `button` is a GameStream buttonFlags bit (A=0x1000 B=0x2000 X=0x4000 Y=0x8000,
    /// dpad=0x1/2/4/8, start=0x10 back=0x20 LS=0x40 RS=0x80 LB=0x100 RB=0x200 guide=0x400).
    static func gamepadButton(_ button: UInt32, down: Bool, pad: UInt32 = 0) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_GAMEPAD_BUTTON.rawValue,
            code: button, x: down ? 1 : 0, y: 0, flags: pad)
    }

    /// Axis ids: 0=LSX 1=LSY 2=RSX 3=RSY (−32768...32767, XInput convention: +y = UP —
    /// `GCControllerDirectionPad.yAxis` already matches, no flip), 4=LT 5=RT (0...255).
    static func gamepadAxis(_ axis: UInt32, value: Int32, pad: UInt32 = 0) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_GAMEPAD_AXIS.rawValue, code: axis, x: value, y: 0, flags: pad)
    }
}
