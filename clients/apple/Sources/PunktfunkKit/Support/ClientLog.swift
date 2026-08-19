// The client's own recent-log ring + the drop-in logger that feeds it — the source for the
// "Send logs to host" action (`LibraryClient.sendLogs`), the Apple port of
// `pf_client_core::logring` + `punktfunk-session`'s `ring_layer`.
//
// WHY A RING AND NOT `OSLogStore`. The unified log already holds everything these loggers write,
// and `OSLogStore(scope: .currentProcessIdentifier)` can read it back — but only the levels the
// system PERSISTS (`.notice`/`.error`/`.fault`). `.info` is memory-only and purged under pressure,
// and `.info` is exactly where the lines a field report needs live: the 1 Hz stats line, the
// decoder/presenter setup, the audio underrun notes. On an Apple TV there is no Console.app to
// read any of it on either. So every `ClientLog` call goes to os_log as before AND into this
// process-global ring, and an explicit user action posts the ring to the PAIRED host, where the
// web console shows it next to the host's own log.
//
// Bounded by lines AND bytes so a log-storm can't grow memory; the byte budget stays under the
// host's 1 MiB upload cap so a full ring always uploads whole. `.debug` deliberately skips the
// ring: it is per-key/per-event input chatter here, and a ring that a healthy keyboard can flush
// in thirty seconds is worse than no ring (the session client learned this from a Steam Deck
// bundle whose whole 27-minute session had been evicted by decoder DPB chatter).

import Foundation
import os

/// Process-global bounded ring of formatted log lines. `note` is cheap (one lock, one append);
/// `render` is the upload body.
public enum ClientLogRing {
    /// Newest lines kept — matches the host's own ring depth and the session client's.
    public static let maxLines = 4096
    /// Byte budget — under the host's 1 MiB bundle cap with headroom for the header.
    public static let maxBytes = 768 * 1024

    private static let lock = OSAllocatedUnfairLock(initialState: State())

    private struct State {
        var lines: [String] = []
        /// Index of the oldest live line in `lines` — popped lazily, compacted when half is dead,
        /// so eviction is O(1) amortised without a deque type.
        var head = 0
        var bytes = 0
        var dropped = 0
    }

    /// Append one formatted log line (no trailing newline). Oversized lines are truncated to keep
    /// a single event from evicting the whole ring.
    public static func note(_ line: String) {
        // `decoding:` rather than `String(_:)`: a cut mid-scalar yields U+FFFD, not nil.
        let line = line.utf8.count > 2048
            ? String(decoding: line.utf8.prefix(2048), as: UTF8.self) + "…" : line
        let size = line.utf8.count
        lock.withLock { s in
            s.lines.append(line)
            s.bytes += size
            while s.lines.count - s.head > maxLines || s.bytes > maxBytes, s.head < s.lines.count {
                s.bytes -= s.lines[s.head].utf8.count
                s.head += 1
                s.dropped += 1
            }
            if s.head > 0, s.head * 2 >= s.lines.count {
                s.lines.removeFirst(s.head)
                s.head = 0
            }
        }
    }

    /// The ring rendered as one text bundle, oldest first, prefixed by `header` (the app's own
    /// identity line — name, version, platform) and an eviction note when the ring wrapped.
    public static func render(header: String) -> String {
        lock.withLock { s in
            var out = header + "\n"
            if s.dropped > 0 {
                out += "… \(s.dropped) older lines evicted from the ring …\n"
            }
            for line in s.lines[s.head...] {
                out += line
                out += "\n"
            }
            return out
        }
    }

    /// The bundle's first line: `punktfunk-apple 0.31.0 (42) (iOS 26.0.0 arm64; Apple TV) — client
    /// log bundle` — the same shape as the session client's, so the host's log page reads them alike.
    public static func header() -> String {
        let info = Bundle.main.infoDictionary
        let version = info?["CFBundleShortVersionString"] as? String ?? "dev"
        let build = info?["CFBundleVersion"] as? String
        let os = ProcessInfo.processInfo.operatingSystemVersion
        #if os(macOS)
        let platform = "macOS"
        #elseif os(tvOS)
        let platform = "tvOS"
        #elseif os(iOS)
        let platform = "iOS"
        #else
        let platform = "apple"
        #endif
        #if arch(arm64)
        let arch = "arm64"
        #else
        let arch = "x86_64"
        #endif
        let v = build.map { "\(version) (\($0))" } ?? version
        return "punktfunk-apple \(v) (\(platform) \(os.majorVersion).\(os.minorVersion).\(os.patchVersion) "
            + "\(arch); \(DeviceName.kind)) — client log bundle"
    }

    /// `2026-08-15T12:03:47.123Z` — wall time, so a bundle correlates with the host log it lands
    /// next to (the session client's `wallclock`).
    static func stamp(_ date: Date = Date()) -> String {
        Date.ISO8601FormatStyle(includingFractionalSeconds: true).format(date)
    }
}

/// Drop-in for `Logger(subsystem: "io.unom.punktfunk", category:)`: the same call shape (string
/// interpolation with `privacy:`/`format:` options), forwarded to os_log AND noted in
/// `ClientLogRing`. Interpolated values are rendered in the clear in both places — the unified log
/// was already being read with the app attached, and the ring only ever leaves the device by an
/// explicit "Send logs to host" to a host the user paired with.
public struct ClientLog: Sendable {
    public let category: String
    private let logger: Logger

    public init(category: String) {
        self.category = category
        self.logger = Logger(subsystem: "io.unom.punktfunk", category: category)
    }

    /// os_log only — see the file comment for why debug stays out of the ring.
    public func debug(_ message: ClientLogMessage) {
        logger.debug("\(message.text, privacy: .public)")
    }

    public func info(_ message: ClientLogMessage) {
        logger.info("\(message.text, privacy: .public)")
        note("INFO", message.text)
    }

    public func notice(_ message: ClientLogMessage) {
        logger.notice("\(message.text, privacy: .public)")
        note("INFO", message.text)
    }

    public func warning(_ message: ClientLogMessage) {
        logger.warning("\(message.text, privacy: .public)")
        note("WARN", message.text)
    }

    public func error(_ message: ClientLogMessage) {
        logger.error("\(message.text, privacy: .public)")
        note("ERROR", message.text)
    }

    public func fault(_ message: ClientLogMessage) {
        logger.fault("\(message.text, privacy: .public)")
        note("ERROR", message.text)
    }

    private func note(_ level: String, _ text: String) {
        ClientLogRing.note("\(ClientLogRing.stamp()) \(level.padding(toLength: 5, withPad: " ", startingAt: 0)) \(category) \(text)")
    }
}

/// The interpolated message: accepts the `OSLogMessage` options the call sites use (`privacy:`,
/// `format:`) so swapping `Logger` for `ClientLog` touches one declaration per file, not every
/// log line. Privacy is accepted and ignored (see `ClientLog`); `.fixed(precision:)` is honoured.
public struct ClientLogMessage: ExpressibleByStringInterpolation, ExpressibleByStringLiteral, Sendable {
    public let text: String

    public init(stringLiteral value: String) { text = value }
    public init(stringInterpolation: StringInterpolation) { text = stringInterpolation.out }

    public struct StringInterpolation: StringInterpolationProtocol, Sendable {
        var out = ""
        public init(literalCapacity: Int, interpolationCount: Int) {
            out.reserveCapacity(literalCapacity + interpolationCount * 8)
        }
        public mutating func appendLiteral(_ literal: String) { out += literal }
        public mutating func appendInterpolation<T>(_ value: T, privacy: OSLogPrivacy = .auto) {
            out += String(describing: value)
        }
        public mutating func appendInterpolation<T: BinaryFloatingPoint>(
            _ value: T, format: ClientLogFloatFormat, privacy: OSLogPrivacy = .auto
        ) {
            switch format {
            case .fixed(let precision):
                out += String(format: "%.\(precision)f", Double(value))
            }
        }
    }
}

/// The one float format the call sites use. `OSLogFloatFormatting` cannot be pattern-matched, so
/// the message type names its own — same spelling at the call site: `format: .fixed(precision: 2)`.
public enum ClientLogFloatFormat: Sendable {
    case fixed(precision: Int)
}
