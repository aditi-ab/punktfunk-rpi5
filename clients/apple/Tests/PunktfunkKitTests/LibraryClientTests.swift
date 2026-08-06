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

    func testArtworkResolvedRewritesOnlyHostRelativePaths() {
        let base = URL(string: "https://192.168.1.70:47990/api/v1/library")!
        // Steam art now comes back as host-relative proxy paths; external CDN URLs (GOG/Heroic/Xbox)
        // and `data:` URLs (Lutris) are untouched.
        let art = Artwork(
            portrait: "/api/v1/library/art/steam:3527290/portrait",
            hero: "https://cdn.example.com/hero.jpg",
            logo: nil,
            header: "/api/v1/library/art/steam:3527290/header")
        let resolved = art.resolved(against: base)
        XCTAssertEqual(
            resolved.portrait, "https://192.168.1.70:47990/api/v1/library/art/steam:3527290/portrait")
        XCTAssertEqual(
            resolved.header, "https://192.168.1.70:47990/api/v1/library/art/steam:3527290/header")
        XCTAssertEqual(resolved.hero, "https://cdn.example.com/hero.jpg") // unchanged
        XCTAssertNil(resolved.logo)
    }

    // MARK: - HTTP response parsing (MgmtTransport)

    // The management API is reached over Network.framework rather than URLSession (ATS cannot be
    // relaxed for the arbitrary addresses a host lives at — see MgmtTransport), so we parse HTTP
    // ourselves. These cover the framings hyper actually emits, plus the failure modes where
    // getting it wrong would be silent.

    private func raw(_ text: String) -> Data { Data(text.utf8) }

    func testParsesContentLengthFramedJSON() throws {
        let body = #"[{"id":"steam:570"}]"#
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                + "Content-Length: \(body.utf8.count)\r\n\r\n\(body)"))
        XCTAssertEqual(response.status, 200)
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), body)
        // Field names are case-insensitive per RFC 9110.
        XCTAssertEqual(response.header("CONTENT-TYPE"), "application/json")
    }

    func testParsesChunkedBody() throws {
        // How hyper streams the art proxy.
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                + "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "hello world")
    }

    func testChunkExtensionsAndTrailersAreIgnored() throws {
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                + "3;foo=bar\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "abc")
    }

    func testUnauthorizedStatusSurvives() throws {
        // What an unpaired certificate gets from the host — the status is the whole signal.
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertEqual(response.status, 401)
        XCTAssertTrue(response.body.isEmpty)
    }

    func testBodyRunsToEOFWithoutFramingHeaders() throws {
        let response = try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\n\r\nraw-to-eof"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "raw-to-eof")
    }

    func testTruncatedBodyThrowsRatherThanReturningPartialJSON() {
        // The one that matters: a body cut short must NOT come back as success. A clipped JSON
        // array would decode to fewer games — "this host has no games" — instead of an error.
        XCTAssertThrowsError(
            try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nshort")))
    }

    func testOverLongBodyIsClippedToContentLength() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "abc")
    }

    func testMalformedResponsesThrow() {
        // Header block never terminated (peer hung up), a non-HTTP greeting, and a chunked stream
        // cut mid-chunk.
        XCTAssertThrowsError(
            try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n")))
        XCTAssertThrowsError(try HTTPResponseParser.parse(raw("NOT-HTTP\r\n\r\n")))
        XCTAssertThrowsError(try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabc")))
    }

    func testMultiWordReasonPhraseParses() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertEqual(response.status, 404)
    }

    func testBinaryBodySurvivesByteForByte() throws {
        // Posters are PNG/JPEG: the body must never be round-tripped through a String.
        let png: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0x0D, 0x0A]
        var message = raw("HTTP/1.1 200 OK\r\nContent-Length: \(png.count)\r\n\r\n")
        message.append(contentsOf: png)
        let response = try HTTPResponseParser.parse(message)
        XCTAssertEqual([UInt8](response.body), png)
    }

    func testBaseURLBracketsIPv6Only() {
        XCTAssertEqual(LibraryClient.baseURL(address: "192.168.1.70", port: 47990),
                       "https://192.168.1.70:47990")
        XCTAssertEqual(LibraryClient.baseURL(address: "100.101.102.103", port: 47990),
                       "https://100.101.102.103:47990")
        XCTAssertEqual(LibraryClient.baseURL(address: "fd7a:115c::1", port: 47990),
                       "https://[fd7a:115c::1]:47990")
        // An address the user pasted already bracketed must not end up double-bracketed.
        XCTAssertEqual(LibraryClient.baseURL(address: "[fd7a:115c::1]", port: 47990),
                       "https://[fd7a:115c::1]:47990")
    }
}
