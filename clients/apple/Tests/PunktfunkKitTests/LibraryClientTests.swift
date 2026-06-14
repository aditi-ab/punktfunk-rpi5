// Unit tests for the game-library models — decoding the management API's GET /api/v1/library
// payload and the poster-art fallback order. (The network fetch itself isn't unit-tested; it's
// exercised live against a host.)

import XCTest
@testable import PunktfunkKit

final class LibraryClientTests: XCTestCase {
    func testDecodesLibraryPayload() throws {
        // A Steam entry (full art + launch) and a custom entry (sparse art, no launch) — the two
        // shapes the host's `GameEntry` serializes (note the host omits null fields).
        let json = """
        [
          {
            "id": "steam:570",
            "store": "steam",
            "title": "Dota 2",
            "art": {
              "portrait": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/library_600x900.jpg",
              "hero": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/library_hero.jpg",
              "logo": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/logo.png",
              "header": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/header.jpg"
            },
            "launch": { "kind": "steam_appid", "value": "570" }
          },
          {
            "id": "custom:abc123",
            "store": "custom",
            "title": "Dolphin",
            "art": { "header": "https://example.com/dolphin.jpg" }
          }
        ]
        """.data(using: .utf8)!

        let games = try JSONDecoder().decode([GameEntry].self, from: json)
        XCTAssertEqual(games.count, 2)

        let steam = games[0]
        XCTAssertEqual(steam.id, "steam:570")
        XCTAssertFalse(steam.isCustom)
        XCTAssertEqual(steam.launch?.kind, "steam_appid")
        XCTAssertEqual(steam.launch?.value, "570")

        let custom = games[1]
        XCTAssertTrue(custom.isCustom)
        XCTAssertNil(custom.launch)
        XCTAssertNil(custom.art.portrait)
    }

    func testPosterCandidatesPreferPortraitThenHeader() {
        let full = Artwork(
            portrait: "https://x/p.jpg", hero: "https://x/hero.jpg",
            logo: "https://x/logo.png", header: "https://x/h.jpg")
        XCTAssertEqual(full.posterCandidates.map(\.absoluteString),
                       ["https://x/p.jpg", "https://x/h.jpg", "https://x/hero.jpg"])

        // No portrait → header leads; absent fields are skipped, not nil-padded.
        let sparse = Artwork(portrait: nil, hero: nil, logo: nil, header: "https://x/h.jpg")
        XCTAssertEqual(sparse.posterCandidates.map(\.absoluteString), ["https://x/h.jpg"])

        XCTAssertTrue(Artwork().posterCandidates.isEmpty)
    }
}
