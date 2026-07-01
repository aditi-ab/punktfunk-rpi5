// GamepadUIEnvironment.isActive is a pure AND — table-tested exhaustively over its 2x2 inputs.

import XCTest

@testable import PunktfunkKit

final class GamepadUIEnvironmentTests: XCTestCase {
    func testActiveOnlyWhenEnabledAndConnected() {
        XCTAssertTrue(GamepadUIEnvironment.isActive(gamepadConnected: true, enabledSetting: true))
        XCTAssertFalse(GamepadUIEnvironment.isActive(gamepadConnected: true, enabledSetting: false))
        XCTAssertFalse(GamepadUIEnvironment.isActive(gamepadConnected: false, enabledSetting: true))
        XCTAssertFalse(GamepadUIEnvironment.isActive(gamepadConnected: false, enabledSetting: false))
    }
}
