// The shared vector files under `clients/shared/`, for the vector tests that replay them.

import XCTest

enum SharedVector {
    /// The top-level object of `clients/shared/<name>`. Read from the source tree, not copied
    /// into the bundle: a copy would be a second contract.
    static func load(_ name: String) throws -> [String: Any] {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/\(name)")
        let data = try Data(contentsOf: url)
        return try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
    }
}
