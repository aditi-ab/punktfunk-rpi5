// Swift wrapper around the lumen-core C ABI's lumen/1 connection API.
//
// Threading contract (mirrors the C header): one LumenConnection is used from a single
// pump thread for nextAU(); nextAudio() may run on its own (single) audio thread;
// sendInput() is enqueue-only and safe alongside both. The pointers inside an AU/audio
// packet are only valid until the next call of the same kind, so we copy into Data here —
// the copies are small and keep the Swift side memory-safe.
//
// Trust: pass the host's pinned certificate fingerprint (the host logs it at startup, and
// `hostFingerprint` reports what a trust-on-first-use connect observed — persist it, e.g.
// in UserDefaults keyed by host, and pin it from then on).
//
// SCAFFOLD: written on the Linux host, not yet compiled against Xcode — expect to fix
// trivial issues on first build (see README.md "Handoff").

import Foundation
import LumenCore

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

public enum LumenClientError: Error {
    /// Connect failed — wrong host/port, timeout, or a certificate-pin mismatch.
    case connectFailed
    /// `pinSHA256` was non-nil but not exactly 32 bytes. Failing closed: connecting
    /// unpinned when the caller asked for verification would be a silent trust downgrade.
    case invalidPin
    case closed
}

public final class LumenConnection {
    private var handle: OpaquePointer?

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
    public init(
        host: String, port: UInt16 = 9777,
        width: UInt32, height: UInt32, refreshHz: UInt32,
        pinSHA256: Data? = nil,
        timeoutMs: UInt32 = 10_000
    ) throws {
        if let pin = pinSHA256, pin.count != 32 { throw LumenClientError.invalidPin }
        var observed = [UInt8](repeating: 0, count: 32)
        handle = host.withCString { cs in
            if let pin = pinSHA256 {
                return pin.withUnsafeBytes { p in
                    lumen_connect(
                        cs, port, width, height, refreshHz,
                        p.bindMemory(to: UInt8.self).baseAddress, &observed, timeoutMs)
                }
            }
            return lumen_connect(cs, port, width, height, refreshHz, nil, &observed, timeoutMs)
        }
        guard handle != nil else { throw LumenClientError.connectFailed }
        hostFingerprint = Data(observed)
        var w: UInt32 = 0, h: UInt32 = 0, hz: UInt32 = 0
        _ = lumen_connection_mode(handle, &w, &h, &hz)
        self.width = w
        self.height = h
        self.refreshHz = hz
    }

    /// Pull the next access unit; nil on timeout, throws once the session is closed.
    public func nextAU(timeoutMs: UInt32 = 100) throws -> AccessUnit? {
        var frame = LumenFrame()
        switch lumen_connection_next_au(handle, &frame, timeoutMs) {
        case LUMEN_STATUS_OK:
            let data = Data(bytes: frame.data, count: frame.len) // copy: ptr valid only until next call
            return AccessUnit(
                data: data, ptsNs: frame.pts_ns,
                frameIndex: frame.frame_index, flags: frame.flags)
        case LUMEN_STATUS_NO_FRAME:
            return nil
        case LUMEN_STATUS_CLOSED:
            throw LumenClientError.closed
        default:
            throw LumenClientError.closed
        }
    }

    /// Pull the next Opus audio packet; nil on timeout, throws once the session is closed.
    /// Drain from a dedicated audio thread — packets arrive every 5 ms (320 ms buffered).
    public func nextAudio(timeoutMs: UInt32 = 100) throws -> AudioPacket? {
        var pkt = LumenAudioPacket()
        switch lumen_connection_next_audio(handle, &pkt, timeoutMs) {
        case LUMEN_STATUS_OK:
            let data = Data(bytes: pkt.data, count: pkt.len) // copy: ptr valid only until next call
            return AudioPacket(data: data, ptsNs: pkt.pts_ns, seq: pkt.seq)
        case LUMEN_STATUS_NO_FRAME:
            return nil
        default:
            throw LumenClientError.closed
        }
    }

    /// Pull the next force-feedback update for the GCController haptics engine:
    /// `(pad, lowFrequency, highFrequency)` with 0...0xFFFF amplitudes, (0, 0) = stop.
    public func nextRumble(timeoutMs: UInt32 = 100) throws -> (pad: UInt16, low: UInt16, high: UInt16)? {
        var pad: UInt16 = 0, low: UInt16 = 0, high: UInt16 = 0
        switch lumen_connection_next_rumble(handle, &pad, &low, &high, timeoutMs) {
        case LUMEN_STATUS_OK:
            return (pad, low, high)
        case LUMEN_STATUS_NO_FRAME:
            return nil
        default:
            throw LumenClientError.closed
        }
    }

    /// Send one input event (delivered to the host as a QUIC datagram).
    public func send(_ event: LumenInputEvent) {
        var ev = event
        _ = lumen_connection_send_input(handle, &ev)
    }

    public func close() {
        if let h = handle {
            lumen_connection_close(h)
            handle = nil
        }
    }

    deinit { close() }
}

// Convenience constructors for the wire input events (field semantics match
// lumen_core::input::InputEvent; see lumen_core.h).
public extension LumenInputEvent {
    static func mouseMove(dx: Int32, dy: Int32) -> LumenInputEvent {
        LumenInputEvent(kind: LUMEN_INPUT_KIND_MOUSE_MOVE, _pad: (0, 0, 0), code: 0, x: dx, y: dy, flags: 0)
    }
    static func mouseButton(_ button: UInt32, down: Bool) -> LumenInputEvent {
        LumenInputEvent(
            kind: down ? LUMEN_INPUT_KIND_MOUSE_BUTTON_DOWN : LUMEN_INPUT_KIND_MOUSE_BUTTON_UP,
            _pad: (0, 0, 0), code: button, x: 0, y: 0, flags: 0)
    }
    static func key(_ vk: UInt32, down: Bool) -> LumenInputEvent {
        LumenInputEvent(
            kind: down ? LUMEN_INPUT_KIND_KEY_DOWN : LUMEN_INPUT_KIND_KEY_UP,
            _pad: (0, 0, 0), code: vk, x: 0, y: 0, flags: 0)
    }
    static func scroll(_ delta: Int32) -> LumenInputEvent {
        LumenInputEvent(kind: LUMEN_INPUT_KIND_MOUSE_SCROLL, _pad: (0, 0, 0), code: 0, x: delta, y: 0, flags: 0)
    }

    // Gamepad (wire contract in lumen_core::input::gamepad): one transition per event,
    // `pad` = controller index, accumulated host-side into a virtual Xbox 360 pad.

    /// `button` is a GameStream buttonFlags bit (A=0x1000 B=0x2000 X=0x4000 Y=0x8000,
    /// dpad=0x1/2/4/8, start=0x10 back=0x20 LS=0x40 RS=0x80 LB=0x100 RB=0x200 guide=0x400).
    static func gamepadButton(_ button: UInt32, down: Bool, pad: UInt32 = 0) -> LumenInputEvent {
        LumenInputEvent(
            kind: LUMEN_INPUT_KIND_GAMEPAD_BUTTON,
            _pad: (0, 0, 0), code: button, x: down ? 1 : 0, y: 0, flags: pad)
    }

    /// Axis ids: 0=LSX 1=LSY 2=RSX 3=RSY (−32768...32767, XInput convention: +y = UP —
    /// `GCControllerDirectionPad.yAxis` already matches, no flip), 4=LT 5=RT (0...255).
    static func gamepadAxis(_ axis: UInt32, value: Int32, pad: UInt32 = 0) -> LumenInputEvent {
        LumenInputEvent(
            kind: LUMEN_INPUT_KIND_GAMEPAD_AXIS,
            _pad: (0, 0, 0), code: axis, x: value, y: 0, flags: pad)
    }
}
