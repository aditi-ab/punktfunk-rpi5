// Raw-HID DualSense rumble for macOS.
//
// Apple's GameController/CHHapticEngine path does NOT drive the DualSense's rumble motors on
// macOS — a documented platform gap: adaptive triggers, lightbar and player LEDs all work
// (different APIs), but `CHHapticEngine` output never reaches the motors. So we write the motor
// amplitudes straight into the DualSense HID output report, exactly the way SDL and the Linux
// `hid-playstation` driver do (the same report that already rumbles this pad on a Linux host).
//
// USB (report 0x02, 48 bytes, no CRC) and Bluetooth (report 0x31, 78 bytes, trailing CRC32) are
// both handled. The App Sandbox permits the raw-HID access via the app's `device.usb` +
// `device.bluetooth` entitlements, and this coexists with GameController holding the same device
// (non-seized open). Output-only, so no run-loop scheduling is needed.
//
// macOS-only: IOKit HID device access isn't available to apps on iOS/tvOS.

#if os(macOS)
import Foundation
import IOKit
import IOKit.hid
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "gamepad")

/// Opens the first connected Sony DualSense and forwards motor rumble to it over raw HID.
/// Single-pad model (we forward exactly one controller), so the first match is the right one.
final class DualSenseHID {
    private let manager: IOHIDManager
    private var device: IOHIDDevice?
    private var bluetooth = false
    private var closed = false

    private static let vendorSony = 0x054C
    // DualSense (0x0CE6) and DualSense Edge (0x0DF2). The DualShock 4 uses a different report
    // layout and is intentionally not handled here.
    private static let productIDs = [0x0CE6, 0x0DF2]

    /// "USB" or "Bluetooth" — for logs / the debug panel. Valid after a successful `open()`.
    var transport: String { bluetooth ? "Bluetooth" : "USB" }

    init() {
        manager = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
    }

    deinit { close() }

    /// Find and open the first connected DualSense. Returns false if none is present or it can't
    /// be opened (caller then falls back to CoreHaptics).
    func open() -> Bool {
        let matches = Self.productIDs.map { pid in
            [kIOHIDVendorIDKey: Self.vendorSony, kIOHIDProductIDKey: pid] as CFDictionary
        }
        IOHIDManagerSetDeviceMatchingMultiple(manager, matches as CFArray)
        guard IOHIDManagerOpen(manager, IOOptionBits(kIOHIDOptionsTypeNone)) == kIOReturnSuccess else {
            log.info("rumble: DualSense HID manager open failed — falling back to CoreHaptics")
            return false
        }
        guard let devices = IOHIDManagerCopyDevices(manager) as? Set<IOHIDDevice>,
              let dev = devices.first
        else {
            log.info("rumble: no DualSense HID device found — falling back to CoreHaptics")
            IOHIDManagerClose(manager, IOOptionBits(kIOHIDOptionsTypeNone))
            return false
        }
        device = dev
        let transport = IOHIDDeviceGetProperty(dev, kIOHIDTransportKey as CFString) as? String
        bluetooth = transport?.lowercased().contains("bluetooth") ?? false
        log.info("rumble: DualSense raw-HID rumble active (transport=\(self.transport, privacy: .public))")
        return true
    }

    /// Drive the motors. `low` = left/heavy (low-frequency), `high` = right/light (high-frequency),
    /// each 0...255. (0, 0) stops.
    func rumble(low: UInt8, high: UInt8) {
        guard let dev = device else { return }
        let report = bluetooth
            ? Self.bluetoothReport(low: low, high: high)
            : Self.usbReport(low: low, high: high)
        let rc = report.withUnsafeBufferPointer { buf in
            IOHIDDeviceSetReport(
                dev, kIOHIDReportTypeOutput, CFIndex(report[0]), buf.baseAddress!, buf.count)
        }
        if rc != kIOReturnSuccess {
            log.error("rumble: IOHIDDeviceSetReport failed (0x\(String(format: "%08x", rc), privacy: .public))")
        }
    }

    func close() {
        guard !closed else { return }
        closed = true
        if device != nil { rumble(low: 0, high: 0) } // silence the motors before releasing
        device = nil
        IOHIDManagerClose(manager, IOOptionBits(kIOHIDOptionsTypeNone))
    }

    // MARK: - Report builders

    // DualSense effects payload (DS5EffectsState_t / hid-playstation `common`) — offsets relative
    // to the payload start:
    //   0  flag0 (enable bits)   2  motor_right (high-freq)   3  motor_left (low-freq)
    //   1  flag1                 38 flag2 (enhanced enable)
    // We mirror the Linux driver: flag0 = COMPATIBLE_VIBRATION | HAPTICS_SELECT, flag2 =
    // COMPATIBLE_VIBRATION2 (the enhanced-firmware path), motors sent directly. valid_flag1 stays
    // 0 so this rumble-only report leaves the lightbar / triggers / player LEDs (driven by
    // GameController) untouched.
    private static func fillEffects(_ data: inout [UInt8], at base: Int, low: UInt8, high: UInt8) {
        data[base + 0] = 0x03 // COMPATIBLE_VIBRATION (0x01) | HAPTICS_SELECT (0x02)
        data[base + 2] = high // motor_right
        data[base + 3] = low // motor_left
        data[base + 38] = 0x04 // COMPATIBLE_VIBRATION2 (enhanced rumble, firmware ≥ 0x0224)
    }

    // `usbReport` / `bluetoothReport` / `crc32` are internal (not private) so the unit tests can
    // pin the exact wire layout against the SDL / hid-playstation spec without a physical pad.
    static func usbReport(low: UInt8, high: UInt8) -> [UInt8] {
        var d = [UInt8](repeating: 0, count: 48)
        d[0] = 0x02 // report id
        fillEffects(&d, at: 1, low: low, high: high)
        return d
    }

    static func bluetoothReport(low: UInt8, high: UInt8) -> [UInt8] {
        var d = [UInt8](repeating: 0, count: 78)
        d[0] = 0x31 // report id
        d[1] = 0x00 // seq/tag (static, as SDL)
        d[2] = 0x10 // magic
        fillEffects(&d, at: 3, low: low, high: high)
        // Trailing CRC32 over a 0xA2 seed byte + the report minus its 4 CRC bytes, little-endian.
        let crc = Self.crc32(seed: 0xA2, d[0..<(d.count - 4)])
        d[74] = UInt8(crc & 0xFF)
        d[75] = UInt8((crc >> 8) & 0xFF)
        d[76] = UInt8((crc >> 16) & 0xFF)
        d[77] = UInt8((crc >> 24) & 0xFF)
        return d
    }

    /// Standard reflected CRC32 (zlib poly 0xEDB88320, init 0xFFFFFFFF, final XOR) over `seed`
    /// followed by `bytes` — the DualSense Bluetooth output-report checksum (seed 0xA2). Matches
    /// SDL's `SDL_crc32`/the kernel's `crc32_le` framing.
    static func crc32<S: Sequence>(seed: UInt8, _ bytes: S) -> UInt32
    where S.Element == UInt8 {
        var crc: UInt32 = 0xFFFF_FFFF
        func step(_ b: UInt8) {
            crc ^= UInt32(b)
            for _ in 0..<8 {
                crc = (crc & 1) != 0 ? (crc >> 1) ^ 0xEDB8_8320 : crc >> 1
            }
        }
        step(seed)
        for b in bytes { step(b) }
        return ~crc
    }
}
#endif
