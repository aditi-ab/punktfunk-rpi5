// WakePolicy: the one wake rule the touch UI, the console and the intent share.

import XCTest

@testable import PunktfunkKit

final class WakePolicyTests: XCTestCase {
    private let macs = ["aa:bb:cc:dd:ee:ff"]

    /// A Wake Host action needs a platform that sends and a MAC to send to; the switch is not
    /// asked (the explicit wake works with Auto-wake off).
    func testCanWakeNeedsAPlatformAndAMac() {
        XCTAssertTrue(WakePolicy.canWake(macs, available: true))
        XCTAssertFalse(WakePolicy.canWake([], available: true))
        XCTAssertFalse(WakePolicy.canWake(macs, available: false))
        XCTAssertEqual(
            WakePolicy.canWake(macs), PunktfunkConnection.wakeOnLANAvailable)
    }

    /// Every input of the automatic wake, each one able to veto it.
    func testAutoWakeNeedsTheSwitchAPlatformAndAMac() {
        for autoWake in [false, true] {
            for available in [false, true] {
                for known in [[], macs] {
                    XCTAssertEqual(
                        WakePolicy.autoWakes(known, autoWake: autoWake, available: available),
                        autoWake && available && !known.isEmpty,
                        "\(autoWake) \(available) \(known)")
                }
            }
        }
    }

    /// 0d8b710cd: with Auto-wake off a library open sends nothing, yet the host can still be
    /// offered a Wake Host.
    func testAutoWakeOffLeavesTheExplicitWake() {
        XCTAssertFalse(WakePolicy.autoWakes(macs, autoWake: false, available: true))
        XCTAssertTrue(WakePolicy.canWake(macs, available: true))
    }

    /// f33badd4a: a host with no MAC is never woken, so a load asks it once.
    func testNoMacNeverWakes() {
        XCTAssertFalse(WakePolicy.autoWakes([], autoWake: true, available: true))
    }
}
