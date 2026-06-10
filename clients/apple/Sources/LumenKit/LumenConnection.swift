// Swift wrapper around the lumen-core C ABI's lumen/1 connection API.
//
// Threading contract (mirrors the C header): one LumenConnection is used from a single
// pump thread for nextAU(); sendInput() is enqueue-only and safe alongside it. The pointer
// inside an AU is only valid until the next nextAU() call, so we copy into Data here —
// the copy is small (an encoded AU, tens of KB) and keeps the Swift side memory-safe.
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

public enum LumenClientError: Error {
    case connectFailed
    case closed
}

public final class LumenConnection {
    private var handle: OpaquePointer?

    /// Negotiated session mode (host-confirmed).
    public private(set) var width: UInt32 = 0
    public private(set) var height: UInt32 = 0
    public private(set) var refreshHz: UInt32 = 0

    /// Connect and start a session at the requested mode (the host creates a native virtual
    /// output at exactly this size/refresh). Blocks up to `timeoutMs`.
    public init(
        host: String, port: UInt16 = 9777,
        width: UInt32, height: UInt32, refreshHz: UInt32,
        timeoutMs: UInt32 = 10_000
    ) throws {
        handle = host.withCString { cs in
            lumen_connect(cs, port, width, height, refreshHz, timeoutMs)
        }
        guard handle != nil else { throw LumenClientError.connectFailed }
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
}
