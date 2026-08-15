// Minimal HTTP/1.1 response parsing for `MgmtTransport`.
//
// We speak HTTP ourselves because the management API has to be reached OUTSIDE the URL loading
// system: App Transport Security applies to URLSession and cannot be relaxed for the arbitrary,
// user-supplied addresses a punktfunk host lives at (see MgmtTransport for the full story). What
// we need is GETs against one host, so this covers exactly that and nothing more — no redirects,
// no request bodies, no content negotiation.
//
// It does need to find where a response ENDS without waiting for the peer to hang up, because the
// connection is reused across a grid's worth of poster fetches (`messageLength`). Both framings
// hyper emits are handled: `Content-Length`, and `Transfer-Encoding: chunked` for the art proxy.
//
// Deliberately free of any Network.framework / PunktfunkCore dependency: pure bytes-in,
// value-out, so it can be unit-tested (and typechecked) on its own.

import Foundation

/// A parsed HTTP/1.1 response. `headers` keys are lowercased, so lookups are case-insensitive the
/// way the grammar requires.
struct HTTPResponse: Sendable {
    let status: Int
    let headers: [String: String]
    let body: Data

    func header(_ name: String) -> String? { headers[name.lowercased()] }

    /// Did the peer ask to close? HTTP/1.1 keeps the connection open unless it says otherwise.
    var wantsClose: Bool {
        header("connection")?.lowercased().contains("close") ?? false
    }
}

enum HTTPParseError: Error, Sendable {
    /// The header block never terminated, or the body is shorter than `Content-Length` promised —
    /// i.e. the peer hung up mid-response. Never surface a truncated body as success: a clipped
    /// JSON array would read as "this host has no games" rather than as the failure it is.
    case truncated
    case malformedStatusLine
    case malformedHeader
    case malformedChunk
}

enum HTTPResponseParser {
    /// Byte length of the first complete response in `raw`, or nil when more bytes are needed.
    ///
    /// Also nil when the response carries no framing header at all, since then the body runs
    /// until the peer closes and has no knowable length — a connection that answers that way
    /// cannot be reused.
    static func messageLength(in raw: Data) throws -> Int? {
        let b = [UInt8](raw)
        guard let head = try parseHead(b) else { return nil }
        if head.headers["transfer-encoding"]?.lowercased().contains("chunked") == true {
            return try chunkedEnd(b, from: head.bodyStart)
        }
        if let field = head.headers["content-length"] {
            guard let length = Int(field.trimmingCharacters(in: .whitespaces)), length >= 0 else {
                throw HTTPParseError.malformedHeader
            }
            // A malicious host can send Content-Length = Int.max; `bodyStart + length` would then
            // overflow, and Swift integer overflow TRAPS (uncatchable crash), not throws. Add
            // reporting overflow and reject instead. security-review 2026-08-15 (low: HTTPResponse
            // Int overflow).
            let (end, overflow) = head.bodyStart.addingReportingOverflow(length)
            if overflow { throw HTTPParseError.malformedHeader }
            return b.count >= end ? end : nil
        }
        return nil // framed by connection close
    }

    /// Parse one complete response. `raw` must hold exactly one message (use `messageLength` to
    /// slice it) or, for a close-framed response, everything read up to EOF.
    static func parse(_ raw: Data) throws -> HTTPResponse {
        let b = [UInt8](raw)
        guard let head = try parseHead(b) else { throw HTTPParseError.truncated }
        let rest = Data(b[head.bodyStart...])
        let body: Data
        if head.headers["transfer-encoding"]?.lowercased().contains("chunked") == true {
            body = try decodeChunked(rest)
        } else if let field = head.headers["content-length"] {
            guard let length = Int(field.trimmingCharacters(in: .whitespaces)), length >= 0 else {
                throw HTTPParseError.malformedHeader
            }
            guard rest.count >= length else { throw HTTPParseError.truncated }
            body = rest.prefix(length)
        } else {
            body = rest // framed by connection close: what we read is what there is
        }
        return HTTPResponse(status: head.status, headers: head.headers, body: body)
    }

    private struct Head {
        let status: Int
        let headers: [String: String]
        /// Offset of the first body byte (just past the CRLFCRLF).
        let bodyStart: Int
    }

    /// Status line + header block, or nil if the block hasn't fully arrived.
    private static func parseHead(_ b: [UInt8]) throws -> Head? {
        guard let headEnd = findHeaderEnd(b) else { return nil }
        let text = String(decoding: b[0..<headEnd], as: UTF8.self)
        var lines = text.components(separatedBy: "\r\n")
        guard !lines.isEmpty else { throw HTTPParseError.malformedStatusLine }

        // "HTTP/1.1 200 OK" — the reason phrase is optional and ignored.
        let statusLine = lines.removeFirst().split(separator: " ", maxSplits: 2,
                                                   omittingEmptySubsequences: false)
        guard statusLine.count >= 2, statusLine[0].hasPrefix("HTTP/"),
              let status = Int(statusLine[1])
        else { throw HTTPParseError.malformedStatusLine }

        var headers: [String: String] = [:]
        for line in lines where !line.isEmpty {
            // A leading space/tab marks an obsolete folded continuation line. Nothing we talk to
            // emits them, and silently mis-parsing one as a field is worse than refusing it.
            guard !line.hasPrefix(" "), !line.hasPrefix("\t"),
                  let colon = line.firstIndex(of: ":")
            else { throw HTTPParseError.malformedHeader }
            let name = String(line[line.startIndex..<colon]).lowercased()
            let value = String(line[line.index(after: colon)...])
                .trimmingCharacters(in: .whitespaces)
            // Repeated fields join with ", " per RFC 9110; none of ours repeat, but dropping one
            // silently would be a lie.
            headers[name] = headers[name].map { "\($0), \(value)" } ?? value
        }
        return Head(status: status, headers: headers, bodyStart: headEnd + 4)
    }

    /// Index just past the CRLFCRLF that ends the header block.
    private static func findHeaderEnd(_ b: [UInt8]) -> Int? {
        guard b.count >= 4 else { return nil }
        for i in 0...(b.count - 4) where b[i] == 0x0D && b[i + 1] == 0x0A
            && b[i + 2] == 0x0D && b[i + 3] == 0x0A {
            return i
        }
        return nil
    }

    /// Offset just past a complete chunked body (terminal chunk plus any trailers), or nil if it
    /// hasn't all arrived.
    private static func chunkedEnd(_ b: [UInt8], from start: Int) throws -> Int? {
        var i = start
        while true {
            guard let lineEnd = findCRLF(b, from: i) else { return nil }
            guard let size = chunkSize(b, i, lineEnd) else { throw HTTPParseError.malformedChunk }
            i = lineEnd + 2
            if size == 0 {
                // Terminal chunk. Trailers (if any) run to the next empty line.
                var j = i
                while true {
                    guard let end = findCRLF(b, from: j) else { return nil }
                    if end == j { return j + 2 }
                    j = end + 2
                }
            }
            guard i + size + 2 <= b.count else { return nil }
            i += size + 2 // payload plus its trailing CRLF
        }
    }

    /// `Transfer-Encoding: chunked` decoding. hyper streams the art proxy this way, so this is a
    /// live path, not defensive dead code.
    static func decodeChunked(_ data: Data) throws -> Data {
        let b = [UInt8](data)
        var i = 0
        var out = Data()
        while true {
            guard let lineEnd = findCRLF(b, from: i) else { throw HTTPParseError.malformedChunk }
            guard let size = chunkSize(b, i, lineEnd) else { throw HTTPParseError.malformedChunk }
            i = lineEnd + 2
            if size == 0 { return out } // terminal chunk; trailers are ignored
            guard i + size <= b.count else { throw HTTPParseError.malformedChunk }
            out.append(contentsOf: b[i..<(i + size)])
            i += size
            guard i + 1 < b.count, b[i] == 0x0D, b[i + 1] == 0x0A else {
                throw HTTPParseError.malformedChunk
            }
            i += 2
        }
    }

    /// "1a" or "1a;ext=value" → 26. Nil if it isn't a hex size.
    private static func chunkSize(_ b: [UInt8], _ from: Int, _ to: Int) -> Int? {
        let field = String(decoding: b[from..<to], as: UTF8.self)
            .split(separator: ";", maxSplits: 1, omittingEmptySubsequences: false)[0]
            .trimmingCharacters(in: .whitespaces)
        guard let size = Int(field, radix: 16), size >= 0 else { return nil }
        return size
    }

    private static func findCRLF(_ b: [UInt8], from: Int) -> Int? {
        guard from >= 0, b.count >= 2 else { return nil }
        var i = from
        while i + 1 < b.count {
            if b[i] == 0x0D && b[i + 1] == 0x0A { return i }
            i += 1
        }
        return nil
    }
}
