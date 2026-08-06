// Minimal HTTP/1.1 response parsing for `MgmtTransport`.
//
// We speak HTTP ourselves because the management API has to be reached OUTSIDE the URL loading
// system: App Transport Security applies to URLSession and cannot be relaxed for the arbitrary,
// user-supplied addresses a punktfunk host lives at (see MgmtTransport for the full story). What
// we need is one unauthenticated-by-header GET per request, so this covers exactly that and
// nothing more — no redirects, no keep-alive, no request bodies, no content negotiation.
//
// Deliberately free of any Network.framework / PunktfunkCore dependency: it is pure bytes-in,
// value-out, so it can be unit-tested (and typechecked) on its own.

import Foundation

/// A parsed HTTP/1.1 response. `headers` keys are lowercased, so lookups are case-insensitive the
/// way the grammar requires.
struct HTTPResponse: Sendable {
    let status: Int
    let headers: [String: String]
    let body: Data

    func header(_ name: String) -> String? { headers[name.lowercased()] }
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
    /// Parse a complete response — everything read from the socket until EOF.
    static func parse(_ raw: Data) throws -> HTTPResponse {
        let bytes = [UInt8](raw)
        guard let headEnd = findHeaderEnd(bytes) else { throw HTTPParseError.truncated }
        // Header field-values are ISO-8859-1 by the grammar; decoding that way never fails, which
        // keeps a stray non-UTF-8 byte in some header from failing the whole response.
        let head = String(decoding: bytes[0..<headEnd], as: UTF8.self)
        var lines = head.components(separatedBy: "\r\n")
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

        let rest = Data(bytes[(headEnd + 4)...])
        let body: Data
        if headers["transfer-encoding"]?.lowercased().contains("chunked") == true {
            body = try decodeChunked(rest)
        } else if let lengthField = headers["content-length"] {
            guard let length = Int(lengthField.trimmingCharacters(in: .whitespaces)), length >= 0
            else { throw HTTPParseError.malformedHeader }
            guard rest.count >= length else { throw HTTPParseError.truncated }
            body = rest.prefix(length)
        } else {
            // No framing header: the body runs to EOF, which is exactly what we read.
            body = rest
        }
        return HTTPResponse(status: status, headers: headers, body: body)
    }

    /// Index of the CRLFCRLF that ends the header block.
    private static func findHeaderEnd(_ b: [UInt8]) -> Int? {
        guard b.count >= 4 else { return nil }
        for i in 0...(b.count - 4) where b[i] == 0x0D && b[i + 1] == 0x0A
            && b[i + 2] == 0x0D && b[i + 3] == 0x0A {
            return i
        }
        return nil
    }

    /// `Transfer-Encoding: chunked` decoding. hyper streams the art proxy this way, so this is a
    /// live path, not defensive dead code.
    static func decodeChunked(_ data: Data) throws -> Data {
        let b = [UInt8](data)
        var i = 0
        var out = Data()
        while true {
            guard let lineEnd = findCRLF(b, from: i) else { throw HTTPParseError.malformedChunk }
            // "1a" or "1a;ext=value" — the size is hex, any chunk extension is ignored.
            let sizeField = String(decoding: b[i..<lineEnd], as: UTF8.self)
                .split(separator: ";", maxSplits: 1, omittingEmptySubsequences: false)[0]
                .trimmingCharacters(in: .whitespaces)
            guard let size = Int(sizeField, radix: 16), size >= 0 else {
                throw HTTPParseError.malformedChunk
            }
            i = lineEnd + 2
            if size == 0 { return out } // terminal chunk; trailers (if any) are ignored
            guard i + size <= b.count else { throw HTTPParseError.malformedChunk }
            out.append(contentsOf: b[i..<(i + size)])
            i += size
            guard i + 1 < b.count, b[i] == 0x0D, b[i + 1] == 0x0A else {
                throw HTTPParseError.malformedChunk
            }
            i += 2
        }
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
