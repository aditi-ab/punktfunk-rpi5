// `clients/shared/device-gyro-vectors.json` against `DeviceGyroRemap` and the wire scale — the
// same rows the Android `DeviceGyro.remap` runs, so a phone clipped landscape yaws the host's
// pad the same way on both. The enum sits outside the iOS gate, so this runs under `swift test`.

import XCTest

@testable import PunktfunkKit

final class DeviceGyroVectorTests: XCTestCase {
    private func vectors() throws -> [String: Any] {
        try SharedVector.load("device-gyro-vectors.json")
    }

    private func remap(_ pose: String) -> DeviceGyroRemap? {
        switch pose {
        case "top_up": return .identity
        case "top_down": return .flipped
        case "top_left": return .topLeft
        case "top_right": return .topRight
        default: return nil
        }
    }

    func testEverySharedRemapAgrees() throws {
        let rows = try XCTUnwrap(try vectors()["remap"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(rows.count, 12, "the vector file is the contract; keep it rich")
        for row in rows {
            let pose = try XCTUnwrap(row["pose"] as? String)
            let input = try XCTUnwrap(row["input"] as? [Double]).map(Float.init)
            let want = try XCTUnwrap(row["expect"] as? [Double]).map(Float.init)
            let remap = try XCTUnwrap(remap(pose), "a pose this test does not know: \(pose)")
            let got = remap.apply(x: input[0], y: input[1], z: input[2])
            XCTAssertEqual([got.x, got.y, got.z], want, "\(pose) \(input)")
        }
    }

    func testTheWireScaleMatchesTheVector() throws {
        let units = try XCTUnwrap(try vectors()["wire_units"] as? [String: Double])
        XCTAssertEqual(Double(GamepadWire.gyroLSBPerRadS), units["gyro_lsb_per_rad_s"]!, accuracy: 1e-3)
        XCTAssertEqual(Double(GamepadWire.accelLSBPerG), units["accel_lsb_per_g"]!, accuracy: 1e-3)
    }
}
