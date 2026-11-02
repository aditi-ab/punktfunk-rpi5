// The core marks the first AU of a new stream format; the stage-2 pump rebuilds its decoder on it.

import PunktfunkCore
import XCTest
@testable import PunktfunkKit

final class StreamFormatTests: XCTestCase {
    private func au(flags: UInt32) -> AccessUnit {
        AccessUnit(data: Data(), ptsNs: 0, frameIndex: 0, flags: flags, receivedNs: 0)
    }

    /// Only core's bit marks a new format: every wire flag together does not.
    func testNewFormatReadsTheCoreFlag() {
        let mark = UInt32(PUNKTFUNK_FRAME_NEW_FORMAT)
        XCTAssertTrue(au(flags: mark).newFormat)
        XCTAssertTrue(au(flags: mark | 0x7FFF).newFormat)
        XCTAssertFalse(au(flags: 0x7FFF).newFormat)
    }
}
