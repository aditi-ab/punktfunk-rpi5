// PunktfunkShared is now a wire contract between the app (writer) and the widget extension +
// deep-link senders (readers). These pin the two formats that cross that boundary:
//   • the `StoredHost` JSON codec — the widget decodes the exact bytes the app persisted, and
//     older saved JSON (missing `mgmtPort` / `macAddresses`) must still decode;
//   • the `punktfunk://` deep-link grammar — the widget builds URLs the app parses.

import XCTest

@testable import PunktfunkKit
import PunktfunkShared

final class SharedFoundationTests: XCTestCase {
    // MARK: - StoredHost JSON codec

    func testStoredHostRoundTrips() throws {
        let host = StoredHost(
            id: UUID(uuidString: "11111111-2222-3333-4444-555555555555")!,
            name: "Tower", address: "192.168.1.173", port: 9777,
            pinnedSHA256: Data([0xDE, 0xAD, 0xBE, 0xEF]),
            lastConnected: Date(timeIntervalSince1970: 1_700_000_000),
            mgmtPort: 47990, macAddresses: ["aa:bb:cc:dd:ee:ff"])

        let data = try JSONEncoder().encode(host)
        let decoded = try JSONDecoder().decode(StoredHost.self, from: data)
        XCTAssertEqual(decoded, host)
    }

    /// Older saved hosts predate `mgmtPort`/`macAddresses` — a missing key must decode to nil, not
    /// throw (synthesized Decodable treats a missing Optional as nil). This is the forward-compat
    /// guarantee the widget depends on when reading a store written by any prior build.
    func testStoredHostDecodesLegacyJSONWithoutOptionalKeys() throws {
        let json = """
        {"id":"11111111-2222-3333-4444-555555555555","name":"Old","address":"10.0.0.5","port":9777}
        """.data(using: .utf8)!

        let decoded = try JSONDecoder().decode(StoredHost.self, from: json)
        XCTAssertEqual(decoded.name, "Old")
        XCTAssertNil(decoded.mgmtPort)
        XCTAssertNil(decoded.macAddresses)
        XCTAssertNil(decoded.pinnedSHA256)
        XCTAssertNil(decoded.lastConnected)
        // Resolvers fall back cleanly.
        XCTAssertEqual(decoded.effectiveMgmtPort, punktfunkDefaultMgmtPort)
        XCTAssertEqual(decoded.wakeMacs, [])
        XCTAssertEqual(decoded.displayName, "Old")
    }

    func testStoredHostDisplayNameFallsBackToAddress() {
        let host = StoredHost(name: "", address: "10.0.0.9")
        XCTAssertEqual(host.displayName, "10.0.0.9")
    }

    // MARK: - DeepLink grammar

    func testDeepLinkConnectRoundTrips() {
        let id = UUID()
        let link = DeepLink.connect(host: id, launchID: nil)
        let parsed = DeepLink(link.url)
        XCTAssertEqual(parsed, link)
        XCTAssertEqual(parsed, .connect(host: id, launchID: nil))
    }

    func testDeepLinkConnectWithLaunchRoundTrips() {
        let id = UUID()
        // A store-qualified GameEntry.id with a reserved char must survive percent-encoding.
        let launch = "steam:570"
        let link = DeepLink.connect(host: id, launchID: launch)
        XCTAssertEqual(DeepLink(link.url), .connect(host: id, launchID: launch))
    }

    func testDeepLinkParsesCanonicalString() throws {
        let id = UUID()
        let url = try XCTUnwrap(URL(string: "punktfunk://connect/\(id.uuidString)"))
        XCTAssertEqual(DeepLink(url), .connect(host: id, launchID: nil))
    }

    func testDeepLinkRejectsForeignSchemeAndBadHost() throws {
        let id = UUID()
        XCTAssertNil(DeepLink(try XCTUnwrap(URL(string: "https://connect/\(id.uuidString)"))))
        XCTAssertNil(DeepLink(try XCTUnwrap(URL(string: "punktfunk://connect/not-a-uuid"))))
        XCTAssertNil(DeepLink(try XCTUnwrap(URL(string: "punktfunk://bogus/\(id.uuidString)"))))
    }

    func testDeepLinkSchemeIsCaseInsensitive() throws {
        let id = UUID()
        let url = try XCTUnwrap(URL(string: "PUNKTFUNK://connect/\(id.uuidString)"))
        XCTAssertEqual(DeepLink(url), .connect(host: id, launchID: nil))
    }
}
