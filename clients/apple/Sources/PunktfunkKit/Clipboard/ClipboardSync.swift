// Shared clipboard, macOS client half (design/clipboard-and-file-transfer.md §5.2).
//
// Bridges NSPasteboard.general to the session's QUIC clipboard plane, both directions lazy:
//
// * **Local copy → host**: a changeCount poll announces the *format list* (`clipOffer`); the
//   bytes cross only when a host app pastes (a `.fetchRequest` event, answered from the live
//   pasteboard by `clipServe`).
// * **Host copy → local**: a `.remoteOffer` writes one NSPasteboardItem whose
//   NSPasteboardItemDataProvider fires only when a Mac app actually pastes — the provider then
//   blocks (on its provider thread, never main) on a `clipFetch` round-trip.
//
// Password-manager respect: pasteboards marked `org.nspasteboard.ConcealedType` or
// `org.nspasteboard.TransientType` are never announced, never fetchable. Echo suppression: the
// changeCount of every write WE make is recorded so the announce poll skips it (§3.4).
//
// Phase 1 formats only (text / RTF / HTML / PNG). Files (NSFilePromiseProvider) ride Phase 2.
#if os(macOS)
import AppKit
import Foundation

/// One live session's clipboard bridge. Created by the session model when streaming begins on a
/// host that advertises `HOST_CAP_CLIPBOARD` and whose per-host toggle is on; `stop()` before the
/// connection closes. All pasteboard traffic runs on one dedicated drain thread plus the
/// AppKit-owned provider threads (paste fulfillment).
public final class ClipboardSync: NSObject {
    /// Wire MIME ↔ NSPasteboard type for the Phase-1 vocabulary (§3.5), in announce order.
    private static let wireToPasteboard: [(wire: String, type: NSPasteboard.PasteboardType)] = [
        ("text/plain;charset=utf-8", .string),
        ("text/rtf", .rtf),
        ("text/html", .html),
        ("image/png", .png),
    ]
    /// Pasteboard marker types that must never cross the wire (password managers mark secrets
    /// with these — see nspasteboard.org).
    private static let concealed = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")
    private static let transient = NSPasteboard.PasteboardType("org.nspasteboard.TransientType")

    /// How long a blocked paste waits for the host's bytes before providing nothing (§5.2).
    private static let fetchTimeout: TimeInterval = 10
    /// Serve chunk size for host-side pastes of our data (bounds the per-call ABI copy).
    private static let serveChunk = 4 << 20

    private let connection: PunktfunkConnection
    /// `CLIP_FLAG_*` sent with the enable (`CLIP_FLAG_FILES` when the session permits files —
    /// always 0 in Phase 1).
    private let controlFlags: UInt8

    /// Host `.state` updates, delivered on the main queue — drives the toggle/footnote UI.
    public var onState: ((_ enabled: Bool, _ policy: UInt8, _ reason: UInt8) -> Void)?

    // Drain-thread state (touched only on the drain thread once started).
    private var offerSeq: UInt32 = 0
    private var lastSeenChangeCount = 0
    /// The changeCount of the last pasteboard write WE made (echo suppression + "do we still
    /// own the pasteboard" on teardown/clear).
    private var ownedChangeCount = -1
    /// The host offer currently installed on the local pasteboard (nil = none).
    private var installedRemoteSeq: UInt32?

    /// Outbound fetches a blocked paste is waiting on. Guarded by `fetchLock` — appended by the
    /// drain thread (`.data` events), consumed by AppKit's provider threads.
    private struct PendingFetch {
        var buffer = Data()
        let done = DispatchSemaphore(value: 0)
        var failed = false
    }
    private let fetchLock = NSLock()
    private var pendingFetches: [UInt32: PendingFetch] = [:]

    private final class StopFlag: @unchecked Sendable {
        private let lock = NSLock()
        private var stopped = false
        func stop() {
            lock.lock()
            stopped = true
            lock.unlock()
        }
        var isStopped: Bool {
            lock.lock()
            defer { lock.unlock() }
            return stopped
        }
    }
    private let flag = StopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    private var started = false
    /// Set by the app-activation observer, cleared by the drain loop: the user may have copied
    /// elsewhere and is coming back to paste — announce immediately instead of waiting out the
    /// poll interval.
    private final class OneShot: @unchecked Sendable {
        private let lock = NSLock()
        private var raised = false
        func raise() {
            lock.lock()
            raised = true
            lock.unlock()
        }
        func takeIfRaised() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            let was = raised
            raised = false
            return was
        }
    }
    private let checkNow = OneShot()
    private var activationObserver: NSObjectProtocol?

    public init(connection: PunktfunkConnection, allowFiles: Bool = false) {
        self.connection = connection
        self.controlFlags = 0 // CLIP_FLAG_FILES rides Phase 2
        _ = allowFiles
        super.init()
    }

    deinit { flag.stop() }

    /// Enable sync with the host and start the drain thread. The host answers the enable with a
    /// `.state` event (surfaced via `onState`) — `BACKEND_UNAVAILABLE` et al. arrive there.
    public func start() {
        guard !started else { return }
        started = true
        connection.clipControl(enabled: true, flags: controlFlags)
        // Baseline: whatever is on the pasteboard when sync starts is announced immediately —
        // the "copy first, then connect and paste" flow must work.
        lastSeenChangeCount = -1
        activationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: nil
        ) { [checkNow] _ in checkNow.raise() }
        let connection = self.connection
        let flag = self.flag
        let thread = Thread { [weak self] in
            var lastAnnounceCheck = Date.distantPast
            while !flag.isStopped {
                // Drain events (bounded burst so a chatty host can't starve the announce poll).
                var drained = 0
                while drained < 32, !flag.isStopped {
                    let ev: PunktfunkConnection.ClipEvent?
                    do {
                        ev = try connection.nextClipboard(timeoutMs: drained == 0 ? 200 : 0)
                    } catch {
                        flag.stop() // session closed
                        break
                    }
                    guard let ev else { break }
                    drained += 1
                    self?.handle(ev)
                }
                // Announce poll: every 500 ms, or immediately after app activation (§5.2).
                let now = Date()
                if now.timeIntervalSince(lastAnnounceCheck) >= 0.5
                    || self?.checkNow.takeIfRaised() == true
                {
                    lastAnnounceCheck = now
                    self?.announceIfChanged()
                }
            }
            self?.drainDone.signal()
        }
        thread.name = "punktfunk-clipboard"
        thread.qualityOfService = .utility
        thread.start()
    }

    /// Disable sync and join the drain thread. Called off-main before `connection.close()`
    /// (the same discipline as the audio/feedback drains). If the local pasteboard still holds
    /// our remote-offer items, they are cleared — their providers die with us.
    public func stop() {
        guard started else { return }
        started = false
        if let obs = activationObserver {
            NotificationCenter.default.removeObserver(obs)
            activationObserver = nil
        }
        connection.clipControl(enabled: false, flags: 0)
        flag.stop()
        drainDone.wait()
        // Fail every paste still blocked on us so no provider thread waits out its timeout.
        fetchLock.lock()
        for (_, pending) in pendingFetches {
            pending.done.signal()
        }
        pendingFetches.removeAll()
        fetchLock.unlock()
        let pb = NSPasteboard.general
        if installedRemoteSeq != nil, pb.changeCount == ownedChangeCount {
            pb.clearContents()
        }
    }

    // MARK: - Local copy → host (announce)

    /// Announce the local pasteboard's format list when it changed (skipping our own writes and
    /// concealed/transient pasteboards). Runs on the drain thread.
    private func announceIfChanged() {
        let pb = NSPasteboard.general
        let count = pb.changeCount
        guard count != lastSeenChangeCount else { return }
        lastSeenChangeCount = count
        if count == ownedChangeCount { return } // our own write (a remote offer) — never echo
        installedRemoteSeq = nil // a local copy replaced the host's offer
        let types = pb.types ?? []
        if types.contains(Self.concealed) || types.contains(Self.transient) { return }
        offerSeq &+= 1
        var kinds = Self.wireToPasteboard
            .filter { types.contains($0.type) }
            .map { PunktfunkConnection.ClipKind(mime: $0.wire) }
        // Images: macOS image copies usually carry TIFF (browsers add WebP/AVIF/GIF, screenshots
        // TIFF) and only sometimes PNG — announce the portable `image/png` whenever ANY
        // convertible image type is present; `serveFetch` converts at fetch time (lazy, §3.5).
        if !kinds.contains(where: { $0.mime == "image/png" }),
            types.contains(.tiff) || types.contains(NSPasteboard.PasteboardType("public.heic"))
        {
            kinds.append(PunktfunkConnection.ClipKind(mime: "image/png"))
        }
        // Empty = the pasteboard holds nothing we sync (or was cleared) — clears the host side.
        connection.clipOffer(seq: offerSeq, kinds: kinds)
    }

    // MARK: - Event handling (drain thread)

    private func handle(_ ev: PunktfunkConnection.ClipEvent) {
        switch ev {
        case let .state(enabled, policy, reason):
            if let onState {
                DispatchQueue.main.async { onState(enabled, policy, reason) }
            }
        case let .remoteOffer(seq, kinds):
            installRemoteOffer(seq: seq, kinds: kinds)
        case let .fetchRequest(reqId, seq, _, mime):
            serveFetch(reqId: reqId, seq: seq, mime: mime)
        case let .data(xferId, chunk, last):
            fetchLock.lock()
            if var pending = pendingFetches[xferId] {
                pending.buffer.append(chunk)
                pendingFetches[xferId] = pending
                if last {
                    pendingFetches[xferId]?.done.signal()
                }
            }
            fetchLock.unlock()
        case let .cancelled(id), let .error(id, _):
            fetchLock.lock()
            if var pending = pendingFetches[id] {
                pending.failed = true
                pendingFetches[id] = pending
                pending.done.signal()
            }
            fetchLock.unlock()
        }
    }

    // MARK: - Host copy → local (lazy install + blocked-paste fetch)

    /// Write one NSPasteboardItem advertising the host's formats, each backed by a lazy data
    /// provider — bytes cross only when a Mac app pastes. Empty `kinds` = the host cleared its
    /// clipboard: drop our item if it's still current.
    private func installRemoteOffer(seq: UInt32, kinds: [PunktfunkConnection.ClipKind]) {
        let pb = NSPasteboard.general
        let types = kinds.compactMap { kind in
            Self.wireToPasteboard.first(where: { $0.wire == kind.mime })?.type
        }
        guard !types.isEmpty else {
            if installedRemoteSeq != nil, pb.changeCount == ownedChangeCount {
                pb.clearContents()
                ownedChangeCount = pb.changeCount
                lastSeenChangeCount = pb.changeCount
            }
            installedRemoteSeq = nil
            return
        }
        let item = NSPasteboardItem()
        item.setDataProvider(RemoteOfferProvider(sync: self, seq: seq), forTypes: types)
        pb.clearContents()
        pb.writeObjects([item])
        installedRemoteSeq = seq
        ownedChangeCount = pb.changeCount
        lastSeenChangeCount = pb.changeCount
    }

    /// Blocked-paste fulfillment: fetch one wire format of host offer `seq` and wait (provider
    /// thread) for the drain thread to assemble the chunks. Nil on timeout/cancel/error — the
    /// paste then provides nothing rather than hanging (§3.4).
    ///
    /// `fetchLock` is held ACROSS the `clipFetch` so the pending entry exists before the drain
    /// thread can process the first `.data` event (its `handle` takes `fetchLock` after
    /// releasing the connection's clipboard lock — no cycle).
    fileprivate func fetchBlocking(seq: UInt32, wireMime: String) -> Data? {
        fetchLock.lock()
        guard let xferId = connection.clipFetch(seq: seq, mime: wireMime) else {
            fetchLock.unlock()
            return nil
        }
        pendingFetches[xferId] = PendingFetch()
        let done = pendingFetches[xferId]!.done
        fetchLock.unlock()
        let outcome = done.wait(timeout: .now() + Self.fetchTimeout)
        fetchLock.lock()
        let pending = pendingFetches.removeValue(forKey: xferId)
        fetchLock.unlock()
        if outcome == .timedOut {
            connection.clipCancel(id: xferId)
            return nil
        }
        guard let pending, !pending.failed else { return nil }
        return pending.buffer
    }

    // MARK: - Host paste of our data (serve)

    /// Answer a host paste of our offered data from the live pasteboard. A stale `seq` (the
    /// local clipboard changed since that announce) is cancelled — never serve mismatched bytes.
    private func serveFetch(reqId: UInt32, seq: UInt32, mime: String) {
        let pb = NSPasteboard.general
        guard seq == offerSeq, pb.changeCount == lastSeenChangeCount,
            let data = Self.readWireData(pb, mime)
        else {
            connection.clipCancel(id: reqId)
            return
        }
        var offset = 0
        while offset < data.count {
            let end = min(offset + Self.serveChunk, data.count)
            connection.clipServe(
                reqId: reqId, data: data.subdata(in: offset..<end), last: end == data.count)
            offset = end
        }
        if data.isEmpty {
            connection.clipServe(reqId: reqId, data: Data(), last: true)
        }
    }

    /// Read one wire format from the pasteboard, converting where macOS stores a different
    /// native type: `image/png` is served from a real `.png` entry when present, else converted
    /// from whatever image representation the pasteboard holds (TIFF from screenshots/Preview,
    /// WebP/AVIF/GIF from browsers — `NSImage` decodes them all) into PNG at fetch time.
    private static func readWireData(_ pb: NSPasteboard, _ mime: String) -> Data? {
        guard mime == "image/png" else {
            guard let type = wireToPasteboard.first(where: { $0.wire == mime })?.type else {
                return nil
            }
            return pb.data(forType: type)
        }
        if let png = pb.data(forType: .png) {
            return png
        }
        // No native PNG: decode whatever image the pasteboard carries and re-encode.
        guard let img = NSImage(pasteboard: pb),
            let tiff = img.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff)
        else {
            return nil
        }
        return rep.representation(using: .png, properties: [:])
    }
}

/// The lazy paste hook: AppKit calls `provideDataForType` only when a Mac app actually pastes;
/// the fetch then blocks this provider thread (never main) until the host's bytes arrive or the
/// timeout provides nothing. One provider per installed remote offer — a dead sync (weak) or a
/// superseded offer provides nothing.
private final class RemoteOfferProvider: NSObject, NSPasteboardItemDataProvider {
    private weak var sync: ClipboardSync?
    private let seq: UInt32

    init(sync: ClipboardSync, seq: UInt32) {
        self.sync = sync
        self.seq = seq
    }

    func pasteboard(
        _ pasteboard: NSPasteboard?, item: NSPasteboardItem,
        provideDataForType type: NSPasteboard.PasteboardType
    ) {
        guard let sync,
            let wire = wireMime(for: type),
            let data = sync.fetchBlocking(seq: seq, wireMime: wire)
        else { return }
        item.setData(data, forType: type)
    }

    private func wireMime(for type: NSPasteboard.PasteboardType) -> String? {
        switch type {
        case .string: return "text/plain;charset=utf-8"
        case .rtf: return "text/rtf"
        case .html: return "text/html"
        case .png: return "image/png"
        default: return nil
        }
    }
}
#endif
