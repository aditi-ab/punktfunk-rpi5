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

    // MARK: - Message framing (keep-alive)

    // Connections are pooled and reused, so a response has to be delimited WITHOUT waiting for
    // the peer to hang up. Getting this wrong either truncates a response or bleeds one response
    // into the next, and both would be silent.

    func testMessageLengthDelimitsContentLengthFraming() throws {
        let complete = raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: complete), complete.count)
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel")))
    }

    func testMessageLengthDelimitsChunkedFraming() throws {
        let complete = raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: complete), complete.count)
        // Mid-chunk, and terminal chunk without its closing blank line.
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel")))
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n")))
        // Trailers belong to the message and must be consumed with it.
        let trailered = raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            + "3\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: trailered), trailered.count)
    }

    func testMessageLengthIsNilWithoutFraming() throws {
        // No Content-Length and not chunked ⇒ the body runs to EOF and the connection can't be
        // reused. Partial header blocks are likewise "not yet".
        XCTAssertNil(try HTTPResponseParser.messageLength(in: raw("HTTP/1.1 200 OK\r\n\r\nto-eof")))
        XCTAssertNil(try HTTPResponseParser.messageLength(in: raw("HTTP/1.1 200 OK\r\nContent-Len")))
    }

    func testBackToBackResponsesSplitExactly() throws {
        // The reuse case that matters: two responses arriving in one read.
        let first = raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        let second = raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbye")
        let both = first + second
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: both), first.count)
        XCTAssertEqual(
            String(decoding: try HTTPResponseParser.parse(both.prefix(first.count)).body,
                   as: UTF8.self), "hello")
        XCTAssertEqual(
            String(decoding: try HTTPResponseParser.parse(both.dropFirst(first.count)).body,
                   as: UTF8.self), "bye")
    }

    func testConnectionCloseIsDetected() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertTrue(response.wantsClose)
        // HTTP/1.1 keeps the connection open unless told otherwise.
        XCTAssertFalse(try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")).wantsClose)
    }

    // MARK: - Art cache

    private func temporaryCacheDirectory() -> URL {
        URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("pf-art-test-\(UUID().uuidString)", isDirectory: true)
    }

    func testArtCacheRoundTripsBinaryAndSeparatesKeys() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory)

        let png = Data([0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF, 0x0D, 0x0A])
        let portrait = URL(string: "https://100.64.1.2:47990/api/v1/library/art/steam:570/portrait")!
        let header = URL(string: "https://100.64.1.2:47990/api/v1/library/art/steam:570/header")!

        var hit = await cache.data(for: portrait)
        XCTAssertNil(hit, "cold cache must miss")
        await cache.store(png, for: portrait)
        hit = await cache.data(for: portrait)
        XCTAssertEqual(hit, png, "posters are binary; the body must survive byte-for-byte")
        // Sibling art of the same title must not collide.
        let sibling = await cache.data(for: header)
        XCTAssertNil(sibling)
    }

    func testArtCacheRefusesEmptyAndInlineData() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory)

        let empty = URL(string: "https://cdn.example.com/empty.jpg")!
        await cache.store(Data(), for: empty)
        let emptyHit = await cache.data(for: empty)
        XCTAssertNil(emptyHit, "an empty body is not art")

        let inline = URL(string: "data:image/png;base64,iVBORw0KGgo=")!
        await cache.store(Data("x".utf8), for: inline)
        let inlineHit = await cache.data(for: inline)
        XCTAssertNil(inlineHit, "a data: URL is already inline — caching it is a pure loss")
    }

    func testArtCacheAgesEntriesOut() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory, maxAge: 0.4)
        let url = URL(string: "https://cdn.example.com/stale.jpg")!

        await cache.store(Data("stale".utf8), for: url)
        let fresh = await cache.data(for: url)
        XCTAssertNotNil(fresh)
        try await Task.sleep(nanoseconds: 700_000_000)
        let expired = await cache.data(for: url)
        XCTAssertNil(expired)
    }

    func testArtCacheEvictsLeastRecentlyUsedOverBudget() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        // Budget holds three of these; the fourth store must evict.
        let cache = ArtCache(directory: directory, maxBytes: 300)
        let blob = Data(repeating: 0x41, count: 100)
        var urls: [URL] = []
        for i in 0..<4 {
            let url = URL(string: "https://cdn.example.com/blob\(i).jpg")!
            urls.append(url)
            await cache.store(blob, for: url)
            try await Task.sleep(nanoseconds: 60_000_000) // distinct mtimes for LRU ordering
        }
        let evicted = await cache.data(for: urls[0])
        XCTAssertNil(evicted, "the oldest entry should have been evicted")
        let newest = await cache.data(for: urls[3])
        XCTAssertEqual(newest, blob)
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
