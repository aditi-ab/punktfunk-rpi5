// The gamepad wire contract shared by capture (GamepadCapture), feedback (GamepadFeedback),
// and the tests — the pad count, button bits, axis ids, and the touchpad/motion unit conversions.

import Foundation

/// The gamepad wire contract (mirrors `punktfunk_core::input::gamepad`).
public enum GamepadWire {
    /// Gamepads addressable on the wire — the pad index rides the low byte of `flags` on every
    /// per-pad event, 0...15 (`punktfunk_core::input::MAX_PADS`).
    public static let maxPads: Int = 16

    public static let dpadUp: UInt32 = 0x0001
    public static let dpadDown: UInt32 = 0x0002
    public static let dpadLeft: UInt32 = 0x0004
    public static let dpadRight: UInt32 = 0x0008
    public static let start: UInt32 = 0x0010
    public static let back: UInt32 = 0x0020
    public static let leftStickClick: UInt32 = 0x0040
    public static let rightStickClick: UInt32 = 0x0080
    public static let leftShoulder: UInt32 = 0x0100
    public static let rightShoulder: UInt32 = 0x0200
    public static let guide: UInt32 = 0x0400
    public static let a: UInt32 = 0x1000
    public static let b: UInt32 = 0x2000
    public static let x: UInt32 = 0x4000
    public static let y: UInt32 = 0x8000
    /// DualSense touchpad click (Moonlight's extended-button bit position).
    public static let touchpadClick: UInt32 = 0x10_0000

    public static let allButtons: [UInt32] = [
        dpadUp, dpadDown, dpadLeft, dpadRight, start, back,
        leftStickClick, rightStickClick, leftShoulder, rightShoulder, guide,
        a, b, x, y, touchpadClick,
    ]

    public static let axisLSX: UInt32 = 0
    public static let axisLSY: UInt32 = 1
    public static let axisRSX: UInt32 = 2
    public static let axisRSY: UInt32 = 3
    public static let axisLT: UInt32 = 4
    public static let axisRT: UInt32 = 5

    /// Raw DualSense gyro units per rad/s: hid-playstation's calibration over the host's
    /// fixed blob resolves to 20 LSB per deg/s.
    public static let gyroLSBPerRadS: Float = 20 * 180 / .pi
    /// Raw DualSense accelerometer units per g (same derivation).
    public static let accelLSBPerG: Float = 10_000

    /// GC touchpad coordinates (±1, +y up) → wire (0...65535, origin top-left, +y down).
    public static func touchpad(x: Float, y: Float) -> (x: UInt16, y: UInt16) {
        let wx = ((x.clamped(to: -1...1) + 1) / 2 * 65535).rounded()
        let wy = ((1 - y.clamped(to: -1...1)) / 2 * 65535).rounded()
        return (UInt16(wx), UInt16(wy))
    }

    /// Scale + clamp one motion component into the raw signed-16 sensor domain.
    public static func motionRaw(_ value: Float, scale: Float) -> Int16 {
        Int16((value * scale).rounded().clamped(to: Float(Int16.min)...Float(Int16.max)))
    }
}

extension Float {
    fileprivate func clamped(to range: ClosedRange<Float>) -> Float {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
