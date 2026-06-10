// Swift wrapper around the lumen-core C ABI's lumen/1 connection API.
//
// Threading contract (mirrors the C header): one LumenConnection is pumped from a single
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
import LumenCore

// cbindgen's C17-compatible header spells the typedefs as plain integers
// (`typedef int32_t LumenStatus`, `typedef uint8_t LumenInputKind`) while the enum
// constants import as a distinct same-named Swift type — bridge by raw value once here.
private let statusOK: Int32 = LUMEN_STATUS_OK.rawValue
private let statusNoFrame: Int32 = LUMEN_STATUS_NO_FRAME.rawValue
private let statusClosed: Int32 = LUMEN_STATUS_CLOSED.rawValue

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
    case status(Int32)
}

public final class LumenConnection {
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

    /// Pull the next access unit; nil on timeout, throws `.closed` once the session ended.
    /// Call from a single pump thread.
    public func nextAU(timeoutMs: UInt32 = 100) throws -> AccessUnit? {
        pumpLock.lock()
        defer { pumpLock.unlock() }
        guard let h = liveHandle() else { throw LumenClientError.closed }

        var frame = LumenFrame()
        let rc = lumen_connection_next_au(h, &frame, timeoutMs)
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
            throw LumenClientError.closed
        default:
            throw LumenClientError.status(rc)
        }
    }

    /// Pull the next Opus audio packet; nil on timeout, throws `.closed` once the session
    /// ended. Drain from a dedicated audio thread — packets arrive every 5 ms (the core
    /// buffers 320 ms and drops the newest when the puller lags).
    public func nextAudio(timeoutMs: UInt32 = 100) throws -> AudioPacket? {
        audioLock.lock()
        defer { audioLock.unlock() }
        guard let h = liveHandle() else { throw LumenClientError.closed }

        var pkt = LumenAudioPacket()
        let rc = lumen_connection_next_audio(h, &pkt, timeoutMs)
        switch rc {
        case statusOK:
            guard let base = pkt.data, pkt.len > 0 else { return nil }
            let data = Data(bytes: base, count: Int(pkt.len)) // copy: ptr valid only until next call
            return AudioPacket(data: data, ptsNs: pkt.pts_ns, seq: pkt.seq)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw LumenClientError.closed
        default:
            throw LumenClientError.status(rc)
        }
    }

    /// Pull the next force-feedback update for the GCController haptics engine:
    /// `(pad, lowFrequency, highFrequency)` with 0...0xFFFF amplitudes, (0, 0) = stop.
    /// Shares the audio drain thread's plane (call from that thread).
    public func nextRumble(timeoutMs: UInt32 = 0) throws -> (pad: UInt16, low: UInt16, high: UInt16)? {
        audioLock.lock()
        defer { audioLock.unlock() }
        guard let h = liveHandle() else { throw LumenClientError.closed }

        var pad: UInt16 = 0, low: UInt16 = 0, high: UInt16 = 0
        let rc = lumen_connection_next_rumble(h, &pad, &low, &high, timeoutMs)
        switch rc {
        case statusOK:
            return (pad, low, high)
        case statusNoFrame:
            return nil
        case statusClosed:
            throw LumenClientError.closed
        default:
            throw LumenClientError.status(rc)
        }
    }

    /// Send one input event (delivered to the host as a QUIC datagram). Thread-safe;
    /// silently dropped after close.
    public func send(_ event: LumenInputEvent) {
        var ev = event
        abiLock.lock()
        defer { abiLock.unlock() }
        guard let h = handle, !closeRequested else { return }
        _ = lumen_connection_send_input(h, &ev)
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
            lumen_connection_close(h) // joins the connection's internal Rust threads
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
// lumen_core::input::InputEvent; see lumen_core.h).
public extension LumenInputEvent {
    private static func make(
        _ kind: UInt32, code: UInt32, x: Int32, y: Int32, flags: UInt32 = 0
    ) -> LumenInputEvent {
        LumenInputEvent(kind: UInt8(kind), _pad: (0, 0, 0), code: code, x: x, y: y, flags: flags)
    }
    static func mouseMove(dx: Int32, dy: Int32) -> LumenInputEvent {
        make(LUMEN_INPUT_KIND_MOUSE_MOVE.rawValue, code: 0, x: dx, y: dy)
    }
    /// GameStream button ids: 1=left 2=middle 3=right 4=X1 5=X2 (host maps to evdev BTN_*).
    static func mouseButton(_ button: UInt32, down: Bool) -> LumenInputEvent {
        make(
            (down ? LUMEN_INPUT_KIND_MOUSE_BUTTON_DOWN : LUMEN_INPUT_KIND_MOUSE_BUTTON_UP).rawValue,
            code: button, x: 0, y: 0)
    }
    /// `vk` is a Windows virtual-key code (the host's vk_to_evdev table consumes these).
    static func key(_ vk: UInt32, down: Bool) -> LumenInputEvent {
        make((down ? LUMEN_INPUT_KIND_KEY_DOWN : LUMEN_INPUT_KIND_KEY_UP).rawValue, code: vk, x: 0, y: 0)
    }
    /// WHEEL_DELTA(120)-scaled; positive = up (vertical) / right (horizontal) — the
    /// convention Moonlight/SDL use; the host maps onto the ei/wl axes.
    static func scroll(_ delta: Int32, horizontal: Bool = false) -> LumenInputEvent {
        make(LUMEN_INPUT_KIND_MOUSE_SCROLL.rawValue, code: horizontal ? 1 : 0, x: delta, y: 0)
    }

    // Gamepad (wire contract in lumen_core::input::gamepad): one transition per event,
    // `pad` = controller index, accumulated host-side into a virtual Xbox 360 pad.

    /// `button` is a GameStream buttonFlags bit (A=0x1000 B=0x2000 X=0x4000 Y=0x8000,
    /// dpad=0x1/2/4/8, start=0x10 back=0x20 LS=0x40 RS=0x80 LB=0x100 RB=0x200 guide=0x400).
    static func gamepadButton(_ button: UInt32, down: Bool, pad: UInt32 = 0) -> LumenInputEvent {
        make(
            LUMEN_INPUT_KIND_GAMEPAD_BUTTON.rawValue,
            code: button, x: down ? 1 : 0, y: 0, flags: pad)
    }

    /// Axis ids: 0=LSX 1=LSY 2=RSX 3=RSY (−32768...32767, XInput convention: +y = UP —
    /// `GCControllerDirectionPad.yAxis` already matches, no flip), 4=LT 5=RT (0...255).
    static func gamepadAxis(_ axis: UInt32, value: Int32, pad: UInt32 = 0) -> LumenInputEvent {
        make(LUMEN_INPUT_KIND_GAMEPAD_AXIS.rawValue, code: axis, x: value, y: 0, flags: pad)
    }
}
