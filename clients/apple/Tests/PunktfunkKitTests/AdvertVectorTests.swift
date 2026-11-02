// `clients/shared/advert-vectors.json` against what the Apple client makes of an mDNS advert:
// which saved host it is (`DiscoveredHost.matches`) and the OS chain it carries — the same
// rows the Rust `discovery::same_host` / `os` and the Android client run, so a dual-boot box
// and a junk `os` TXT read the same on every client.

import PunktfunkKit
import PunktfunkShared
import XCTest

@MainActor
final class AdvertVectorTests: XCTestCase {
    private func vectors() throws -> [String: Any] {
        try SharedVector.load("advert-vectors.json")
    }

    func testEverySharedMatchAgrees() throws {
        let cases = try XCTUnwrap(try vectors()["match"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 10, "the vector file is the contract; keep it rich")
        for c in cases {
            let name = try XCTUnwrap(c["name"] as? String)
            let saved = try XCTUnwrap(c["saved"] as? [String: Any])
            let advert = try XCTUnwrap(c["advert"] as? [String: Any])
            let fp = try XCTUnwrap(advert["fp"] as? String)
            let host = HostDiscovery.debugAdvert(
                id: name, name: name, host: try XCTUnwrap(advert["addr"] as? String),
                port: UInt16(try XCTUnwrap(advert["port"] as? Int)),
                fingerprintHex: fp.isEmpty ? nil : fp)
            let pin = try XCTUnwrap(saved["fp"] as? String)
            let got = host.matches(
                pin: pin.isEmpty ? nil : pin, address: try XCTUnwrap(saved["addr"] as? String),
                port: UInt16(try XCTUnwrap(saved["port"] as? Int)))
            XCTAssertEqual(got, c["same"] as? Bool, name)
        }
    }

    func testEverySharedOsChainAgrees() throws {
        let file = try vectors()
        let chains = try XCTUnwrap(file["os_chain"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(chains.count, 10, "the vector file is the contract; keep it rich")
        for c in chains {
            let raw = try XCTUnwrap(c["raw"] as? String)
            XCTAssertEqual(sanitizeOsChain(raw), c["expect"] as? String, raw)
        }
        for c in try XCTUnwrap(file["os_icon_tokens"] as? [[String: Any]]) {
            let chain = try XCTUnwrap(c["chain"] as? String)
            XCTAssertEqual(osIconTokens(chain), c["expect"] as? [String], chain)
        }
    }
}
