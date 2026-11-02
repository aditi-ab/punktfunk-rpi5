import XCTest

#if os(macOS)
@testable import PunktfunkKit

/// The macOS 26 variable-refresh notice fires on that release and an adaptive panel only, and
/// never again once the player mutes it.
final class VrrWarningTests: XCTestCase {
    func testWarnsOnMacOS26AdaptivePanelsUntilMuted() {
        XCTAssertTrue(StreamLayerView.warnsAboutVrr(osMajor: 26, adaptive: true, muted: false))
        XCTAssertFalse(StreamLayerView.warnsAboutVrr(osMajor: 27, adaptive: true, muted: false))
        XCTAssertFalse(StreamLayerView.warnsAboutVrr(osMajor: 26, adaptive: false, muted: false))
        XCTAssertFalse(StreamLayerView.warnsAboutVrr(osMajor: 26, adaptive: true, muted: true))
    }
}
#endif
