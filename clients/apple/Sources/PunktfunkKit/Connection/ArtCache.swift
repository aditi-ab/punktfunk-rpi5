// On-disk cache for library cover art.
//
// Posters are the bulk of what the library screen transfers and they essentially never change, so
// re-fetching them on every visit is pure waste — the Windows client has cached them on disk for
// this reason and Apple did not. It matters more now that host art rides `MgmtTransport`: a cache
// hit costs no connection at all.
//
// Lives in the CACHES directory on purpose: every byte here is re-derivable from the host, so the
// system is welcome to evict it under storage pressure. Entries are keyed by the SHA-256 of the
// absolute URL, which covers both host-proxy paths and store CDN URLs without either colliding.
//
// Deliberately free of any Network.framework / PunktfunkCore dependency, so it can be unit-tested
// against a temporary directory.

import CryptoKit
import Foundation

/// A size- and age-bounded blob cache. An actor so disk work stays off whichever thread the
/// SwiftUI poster view happens to be on, and so pruning can never race a write.
actor ArtCache {
    private let directory: URL
    private let maxBytes: Int
    private let maxAge: TimeInterval
    private let fileManager = FileManager.default

    /// `directory` is created on demand. Defaults: 128 MB — a 200-title library of 600×900
    /// capsules lands far under that — and 30 days, which only matters for art a host later
    /// replaces.
    init(directory: URL, maxBytes: Int = 128 * 1024 * 1024, maxAge: TimeInterval = 30 * 24 * 3600) {
        self.directory = directory
        self.maxBytes = maxBytes
        self.maxAge = maxAge
    }

    /// The app's standard location, or nil if the caches directory is unavailable (in which case
    /// callers simply run without a cache rather than failing).
    static func standard() -> ArtCache? {
        guard let caches = FileManager.default.urls(
            for: .cachesDirectory, in: .userDomainMask).first
        else { return nil }
        return ArtCache(directory: caches.appendingPathComponent("PunktfunkArt", isDirectory: true))
    }

    func data(for url: URL) -> Data? {
        let file = path(for: url)
        guard let data = try? Data(contentsOf: file) else { return nil }
        // Age out stale art rather than serving it forever.
        if let modified = modificationDate(of: file), Date().timeIntervalSince(modified) > maxAge {
            try? fileManager.removeItem(at: file)
            return nil
        }
        // Touch, so eviction can order by last USE rather than last write.
        try? fileManager.setAttributes([.modificationDate: Date()], ofItemAtPath: file.path)
        return data
    }

    func store(_ data: Data, for url: URL) {
        // An empty body is not art, and a `data:` URL is already inline — caching either is a
        // pure loss.
        guard !data.isEmpty, url.scheme?.lowercased() != "data" else { return }
        do {
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
            try data.write(to: path(for: url), options: .atomic)
        } catch {
            return // a cache that can't write is a slow cache, not a broken app
        }
        prune()
    }

    /// Drop the oldest entries until the directory fits the budget. Also removes anything past
    /// `maxAge` so a cache that is under budget still doesn't hoard stale art forever.
    func prune() {
        let keys: [URLResourceKey] = [.contentModificationDateKey, .fileSizeKey]
        guard let entries = try? fileManager.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: keys, options: .skipsHiddenFiles)
        else { return }

        var files: [(url: URL, date: Date, size: Int)] = []
        var total = 0
        let now = Date()
        for entry in entries {
            let values = try? entry.resourceValues(forKeys: Set(keys))
            let date = values?.contentModificationDate ?? .distantPast
            let size = values?.fileSize ?? 0
            if now.timeIntervalSince(date) > maxAge {
                try? fileManager.removeItem(at: entry)
                continue
            }
            files.append((entry, date, size))
            total += size
        }
        guard total > maxBytes else { return }

        // Oldest first — `data(for:)` touches on read, so this is least-recently-USED.
        for file in files.sorted(by: { $0.date < $1.date }) {
            guard total > maxBytes else { break }
            try? fileManager.removeItem(at: file.url)
            total -= file.size
        }
    }

    /// Wipe the cache — for a "clear cached data" affordance, and for tests.
    func clear() {
        try? fileManager.removeItem(at: directory)
    }

    private func path(for url: URL) -> URL {
        let digest = SHA256.hash(data: Data(url.absoluteString.utf8))
        let name = digest.map { String(format: "%02x", $0) }.joined()
        return directory.appendingPathComponent(name, isDirectory: false)
    }

    private func modificationDate(of file: URL) -> Date? {
        (try? file.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
    }
}
