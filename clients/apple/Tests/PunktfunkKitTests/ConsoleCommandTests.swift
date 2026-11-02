import XCTest

@testable import PunktfunkKit

/// `clients/shared/console-bridge-vectors.json` holds one sample of every command the console
/// sends. `ConsoleBridge.Command` names exactly those, and the app's switch over it has no
/// `default`, so each one is handled there or the app does not build.
final class ConsoleCommandTests: XCTestCase {
    func testEveryConsoleCommandIsHandled() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // PunktfunkKitTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apple
            .deletingLastPathComponent()  // clients
            .appendingPathComponent("shared/console-bridge-vectors.json")
        let doc = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any]
        let samples = try XCTUnwrap(doc?["commands"] as? [Any])
        let sent = Set(samples.compactMap { $0 as? String ?? ($0 as? [String: Any])?.keys.first })
        XCTAssertEqual(sent.count, samples.count, "one sample per command")
        let handled = Set(ConsoleBridge.Command.allCases.map(\.rawValue))
        XCTAssertEqual(sent.subtracting(handled), [], "commands this app never handles")
        XCTAssertEqual(handled.subtracting(sent), [], "commands the console no longer sends")
    }
}
