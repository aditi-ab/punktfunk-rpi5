// Pins the phone-gyro mirror's device→controller frame remap (DeviceGyro.swift). The matrix is
// derived (like the wire scale constants), so these tests are the contract: if on-glass says an
// axis is wrong, fix the enum AND these expectations together.

#if os(iOS)
import UIKit
import XCTest

@testable import PunktfunkKit

final class DeviceGyroRemapTests: XCTestCase {
    /// A distinct vector per axis so a swapped or flipped component can't cancel out.
    private let v: (x: Float, y: Float, z: Float) = (1, 2, 3)

    func testPortraitIsIdentity() {
        let r = DeviceGyroRemap.identity.apply(x: v.x, y: v.y, z: v.z)
        XCTAssertEqual([r.x, r.y, r.z], [1, 2, 3])
    }

    func testUpsideDownFlipsInPlane() {
        let r = DeviceGyroRemap.flipped.apply(x: v.x, y: v.y, z: v.z)
        XCTAssertEqual([r.x, r.y, r.z], [-1, -2, 3])
    }

    /// Device top to the player's LEFT: player-right = device-bottom (−y), player-up =
    /// device-right (+x). z (out of the screen) never changes — the screen faces the player.
    func testTopLeftLandscape() {
        let r = DeviceGyroRemap.topLeft.apply(x: v.x, y: v.y, z: v.z)
        XCTAssertEqual([r.x, r.y, r.z], [-2, 1, 3])
    }

    /// Device top to the player's RIGHT: player-right = device-top (+y), player-up =
    /// device-left (−x).
    func testTopRightLandscape() {
        let r = DeviceGyroRemap.topRight.apply(x: v.x, y: v.y, z: v.z)
        XCTAssertEqual([r.x, r.y, r.z], [2, -1, 3])
    }

    /// Interface orientation → remap: `.landscapeRight` means the Home edge is on the
    /// player's right, i.e. the device top points LEFT (and vice versa).
    func testOrientationMapping() {
        XCTAssertEqual(DeviceGyroRemap(.portrait), .identity)
        XCTAssertEqual(DeviceGyroRemap(.portraitUpsideDown), .flipped)
        XCTAssertEqual(DeviceGyroRemap(.landscapeRight), .topLeft)
        XCTAssertEqual(DeviceGyroRemap(.landscapeLeft), .topRight)
        XCTAssertEqual(DeviceGyroRemap(.unknown), .identity)
    }

    /// Every remap must stay a proper rotation (right-handed): x̂ × ŷ = ẑ after mapping.
    func testHandednessPreserved() {
        for remap in [DeviceGyroRemap.identity, .flipped, .topLeft, .topRight] {
            let x = remap.apply(x: 1, y: 0, z: 0)
            let y = remap.apply(x: 0, y: 1, z: 0)
            // Cross product of the two mapped in-plane basis vectors.
            let cross = (
                x: x.y * 0 - 0 * y.y, y: 0 * y.x - x.x * 0, z: x.x * y.y - x.y * y.x
            )
            XCTAssertEqual(cross.z, 1, "left-handed remap: \(remap)")
        }
    }
}
#endif
