// Reusable library widgets, shared by the touch grid (LibraryView's `GameCard`) and the gamepad
// coverflow (LibraryCoverflowView's cover cell).

import ImageIO
import PunktfunkKit
import SwiftUI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

/// The store-provenance badge (Steam vs. a user-curated custom entry) overlaid on a poster —
/// shared by the touch grid's `GameCard` and the gamepad coverflow's cover cell.
struct StoreBadge: View {
    /// Which store surfaced the entry, already resolved to a display name (`GameEntry.storeLabel`).
    let label: String
    /// A launcher entry (design D4) gets the brand fill, so "opens Steam" is legible at poster size
    /// without reading the title.
    var isLauncher: Bool = false
    /// Fill the chip with a flat wash instead of a frosted material.
    ///
    /// The coverflow MUST pass true. Its cards ride a `.scrollTransition` that composites them
    /// with `opacity < 1` and a 3D rotation, and a material cannot sample a backdrop through an
    /// offscreen composite — so the frost stayed blank on every card and only appeared on the one
    /// card sitting at exactly full opacity in the centre, reading as a flash on focus. A flat
    /// wash has no backdrop to sample: it is simply always there. (Deliberately black, not
    /// palette ink: the chip sits on cover art, whose colours the palette has no business
    /// fighting.)
    var solid: Bool = false

    private var fill: AnyShapeStyle {
        if isLauncher { return AnyShapeStyle(Color.brand) }
        return solid ? AnyShapeStyle(Color.black.opacity(0.58)) : AnyShapeStyle(.ultraThinMaterial)
    }

    var body: some View {
        Text(label)
            .font(.geist(11, .semibold, relativeTo: .caption2))
            .foregroundStyle(isLauncher || solid ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(fill, in: Capsule())
            .padding(6)
    }
}

/// "This one is already running on the host" — the Resume affordance, overlaid on a poster.
///
/// A badge rather than a changed button title because the grid's tiles have no titles to change:
/// the poster *is* the control. It says `Resume` rather than `Running` on purpose — the player
/// does not need a status report, they need to know what tapping it will do.
///
/// Flat-filled for the same reason `StoreBadge(solid:)` exists: the coverflow composites its cards
/// offscreen, where a material has no backdrop to sample.
struct RunningBadge: View {
    var solid: Bool = false
    /// Glyph only, no word. The grid's tiles go down to ~130 pt wide and already carry the store
    /// chip in the opposite corner; at that size "Resume" plus an icon leaves the two badges
    /// touching in the middle. The coverflow's cards are several times wider and take the word.
    var compact: Bool = false

    var body: some View {
        Group {
            if compact {
                Image(systemName: "play.fill")
            } else {
                Label("Resume", systemImage: "play.fill").labelStyle(.titleAndIcon)
            }
        }
            .font(.geist(11, .semibold, relativeTo: .caption2))
            .foregroundStyle(.white)
            // Semantic green rather than the brand violet: this is a state the host reports, not a
            // Punktfunk surface, and it has to stay distinguishable from the launcher badge — which
            // already owns the brand fill one corner away.
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(Color.green.opacity(solid ? 0.92 : 0.85), in: Capsule())
            .padding(6)
            .accessibilityLabel("Running on the host — resume")
    }
}

#if canImport(UIKit)
private typealias PlatformImage = UIImage
#elseif canImport(AppKit)
private typealias PlatformImage = NSImage
#endif

private extension Image {
    init(platformImage: PlatformImage) {
        #if canImport(UIKit)
        self.init(uiImage: platformImage)
        #elseif canImport(AppKit)
        self.init(nsImage: platformImage)
        #endif
    }
}

/// Decode cover art at the size it will be DRAWN, not the size the CDN shipped.
///
/// A Steam capsule is 600×900 (some custom art 1000×1500); decoded, that is 2–6 MB per poster
/// and stays resident for as long as its tile does. A coverflow holds a dozen; a grid on an iPad
/// Pro or an Apple TV holds forty, and Apple TV's memory ceiling is the lowest of the three.
/// This is the desktop console's own lesson (its grid was a slideshow until posters were decoded
/// at twice their cell size): `CGImageSourceCreateThumbnailAtIndex` decodes straight to a
/// bounded bitmap and never materialises the full-size one. `maxPixels` is the longer edge, in
/// PIXELS (the caller multiplies its point size by the screen scale, ×2 for headroom under the
/// focus pop). nil ⇒ decode as shipped — still capped by DECLARED pixels, since the wire bound
/// caps bytes, not those.
private func decodePoster(_ data: Data, maxPixels: Int?) -> PlatformImage? {
    guard let maxPixels, maxPixels > 0 else { return imageWithinPixelCap(data) }
    guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
    let options: [CFString: Any] = [
        kCGImageSourceCreateThumbnailFromImageAlways: true,
        kCGImageSourceCreateThumbnailWithTransform: true,
        kCGImageSourceShouldCacheImmediately: true,
        kCGImageSourceThumbnailMaxPixelSize: maxPixels,
    ]
    guard let cg = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else {
        // A format ImageIO can't thumbnail (rare) still gets the decode — with the same pixel
        // cap, since a full-size bitmap is the unbounded case.
        return imageWithinPixelCap(data)
    }
    #if canImport(UIKit)
    return UIImage(cgImage: cg)
    #elseif canImport(AppKit)
    return NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
    #endif
}

/// `data` decoded as shipped — but only if its declared dimensions pass the cap. A 16 MB body
/// can name 100 000×100 000 pixels; decoding that at draw time is a memory bomb the wire bound
/// can't see, so anything past ~4× the biggest real capsule is refused instead of a hole.
private let maxFullDecodePixels = 16_777_216 // 4096×4096

private func imageWithinPixelCap(_ data: Data) -> PlatformImage? {
    guard let source = CGImageSourceCreateWithData(data as CFData, nil),
          let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
          let width = props[kCGImagePropertyPixelWidth] as? Int,
          let height = props[kCGImagePropertyPixelHeight] as? Int,
          width > 0, height > 0, width * height <= maxFullDecodePixels
    else { return nil }
    return PlatformImage(data: data)
}

/// Where each library poster last drew, in global (window) coordinates, by entry id.
///
/// The launch hold's cover flies out of the tile the player picked, and by the time it mounts that
/// tile is on its way out — the shelf dismisses on launch. So the rect is recorded as the shelf
/// lays out and read once, at the tap. Deliberately plain storage rather than observable state:
/// every poster writes here on every layout pass, and a published change would re-render the
/// shelf from its own scrolling.
///
/// A recycled `LazyVGrid` tile stops updating when it scrolls off, so an entry can hold a stale
/// rect. That is harmless — the hold checks the rect is still on screen and otherwise just scales
/// its cover up in place.
@MainActor
enum TileFrames {
    private static var frames: [String: CGRect] = [:]

    static func record(_ id: String, _ rect: CGRect) { frames[id] = rect }

    static func rect(_ id: String) -> CGRect? {
        frames[id].flatMap { $0.width > 1 && $0.height > 1 ? $0 : nil }
    }
}

/// One poster's rect on its way up to `TileFrames`. A preference rather than a direct read of
/// the proxy, because this has to survive the tile MOVING (a scroll, a window resize, the
/// shelf's own entrance) and not just resizing.
private struct TileFramePreference: PreferenceKey {
    static let defaultValue: CGRect? = nil

    static func reduce(value: inout CGRect?, nextValue: () -> CGRect?) {
        value = nextValue() ?? value
    }
}

/// Sequentially tries cover-art URLs over `loader` (so a paired client can reach the host's own
/// art proxy, not just public CDNs — see `LibraryArtLoader`), advancing past any that fail to
/// load, then a placeholder. The loaded image is hard-clipped to fill the card's actual frame
/// regardless of its own aspect ratio: a portrait capsule fills it as intended, and a fallback
/// banner (wide hero/header art, used when a title has no portrait capsule) is cropped to the same
/// tile rather than allowed to size it — see the `Color.clear` in `body` for why that takes more
/// than a `.frame(maxWidth:)` and a `.clipped()`. Not `private` —
/// the gamepad coverflow (`LibraryCoverflowView`) reuses it directly rather than re-fetching art.
struct PosterImage: View {
    let candidates: [URL]
    let title: String
    let loader: (any LibraryArtSource)?
    /// The entry's brand-mark token (`GameEntry.iconToken`), when it has one. A launcher tile ships
    /// no cover art by design, so for those the mark IS the poster — see `placeholder`.
    var icon: String?
    /// The size this poster is drawn at, in POINTS — the decode is bounded to twice its longer
    /// edge in pixels (see `decodePoster`). nil decodes the art as shipped.
    var drawnSize: CGSize?
    /// Fires once this poster has settled — art loaded, or every candidate exhausted and the
    /// placeholder is what it will be. The gamepad coverflow waits on a few of these before
    /// playing its entrance, so the cards swing in carrying artwork rather than grey rectangles.
    var onLoaded: (() -> Void)?
    /// Publish this poster's on-screen rect to `TileFrames` under this id (the entry's). What the
    /// launch hold flies its cover out of; nil for a poster nothing launches from.
    var frameID: String?
    @State private var index = 0
    @State private var image: PlatformImage?
    @Environment(\.displayScale) private var displayScale

    /// What re-runs the load task: the next candidate, or a loader arriving. A remounted
    /// shelf draws its restored tiles a tick before `load()` has built one, and keying on
    /// `index` alone spent every candidate on `nil` — the placeholder for good.
    private struct LoadKey: Equatable {
        var index: Int
        var hasLoader: Bool
    }

    var body: some View {
        Group {
            if let image {
                // `Color.clear` is what takes the proposed size; the art rides along as its
                // overlay, where it can be DRAWN but never MEASURED. Handing the image the sizing
                // role instead is what let a fallback banner escape the tile: `scaledToFill`
                // reports a size that covers the proposal, and the flexible frame below clamps it
                // to `.infinity` — i.e. not at all. Measured offscreen, a 460×215 `header.jpg` in a
                // 170pt grid column resolved the tile to 545×255 and overran its neighbours, while
                // a 300×450 cover in the same chain came out correct — which is why this only ever
                // showed on the titles whose cover was missing.
                Color.clear
                    .overlay {
                        Image(platformImage: image)
                            .resizable()
                            .scaledToFill()
                    }
                    .transition(.opacity)
            } else if index < candidates.count {
                ZStack { placeholder; ProgressView() }
                    .transition(.opacity)
            } else {
                placeholder
                    .transition(.opacity)
            }
        }
        // Art crosses over its placeholder instead of replacing it between two frames. Cover
        // fetches land one by one, so without this a freshly opened library is a run of cards
        // visibly snapping from grey to artwork after the strip has already settled.
        .animation(.easeOut(duration: 0.3), value: image != nil)
        .background {
            if frameID != nil {
                GeometryReader { geo in
                    Color.clear.preference(
                        key: TileFramePreference.self, value: geo.frame(in: .global))
                }
            }
        }
        .onPreferenceChange(TileFramePreference.self) { rect in
            guard let frameID, let rect else { return }
            TileFrames.record(frameID, rect)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .task(id: LoadKey(index: index, hasLoader: loader != nil)) { await loadCurrent() }
    }

    private func loadCurrent() async {
        // Past the end: the placeholder IS the final look, so this poster has settled.
        guard index < candidates.count else {
            onLoaded?()
            return
        }
        // No loader yet is not a failed candidate — the task refires when one arrives.
        guard let loader else { return }
        // Twice the drawn edge: headroom for the focus pop and a Retina-crisp cover, without
        // ever holding the CDN's 600×900 (or larger) bitmap for the life of the tile.
        let maxPixels = drawnSize.map { Int(max($0.width, $0.height) * displayScale * 2) }
        guard let data = try? await loader.data(for: candidates[index]) else {
            // A cancelled fetch is the shelf leaving, not a dead URL: keep `index` here so
            // the next appearance retries this candidate rather than skipping it.
            if !Task.isCancelled { index += 1 }
            return
        }
        // Decoding is CPU work a scroll shouldn't pay on the main actor.
        let loaded = await Task.detached(priority: .userInitiated) {
            decodePoster(data, maxPixels: maxPixels)
        }.value
        guard !Task.isCancelled else { return }
        guard let loaded else {
            index += 1
            return
        }
        image = loaded
        onLoaded?()
    }

    private var placeholder: some View {
        ZStack {
            Rectangle().fill(.quaternary)
            // A launcher's brand mark, drawn at poster size and tinted like the text it replaces.
            // `scaledToFit` inside a fraction of the card keeps a non-square master (the Steam mark
            // is 496×512, Playnite's 1024×1024) in its own aspect ratio rather than stretched.
            // Falling back to the title is the pre-icon design, so an unshipped mark loses nothing.
            if let mark = launcherIconImage(for: icon) {
                GeometryReader { geo in
                    mark
                        .resizable()
                        .scaledToFit()
                        .foregroundStyle(.secondary)
                        .frame(width: geo.size.width * 0.44, height: geo.size.height * 0.44)
                        .frame(width: geo.size.width, height: geo.size.height)
                }
            } else {
                Text(title)
                    .font(.geist(17, .semibold, relativeTo: .headline))
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .padding(8)
            }
        }
    }
}

/// A saved host's desktop in the Library tab's Desktops row: its mark and name, a presence dot,
/// and what a tap does — `Desktop`, or `Resume <title>` while the host has a game up.
struct LibraryDesktopTile: View {
    let host: StoredHost
    let isOnline: Bool
    let nowPlaying: String?
    let action: () -> Void

    var body: some View {
        let m = Metrics.current
        return Button(action: action) {
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Group {
                        if let mark = osIconImage(for: host.osChain) {
                            mark.resizable().scaledToFit()
                        } else {
                            Image(systemName: "desktopcomputer").resizable().scaledToFit()
                        }
                    }
                    .frame(width: m.mark, height: m.mark)
                    .foregroundStyle(Color.brand)
                    Spacer(minLength: 0)
                    Circle()
                        .fill(isOnline ? Color.green : Color.secondary.opacity(0.4))
                        .frame(width: m.dot, height: m.dot)
                }
                Spacer(minLength: 0)
                Text(host.displayName)
                    .font(.geist(m.name, .bold, relativeTo: .headline))
                    .foregroundStyle(.primary)
                    .lineLimit(1)
                Text(nowPlaying.map { "Resume \($0)" } ?? "Desktop")
                    .font(.geist(m.status, relativeTo: .caption))
                    .foregroundStyle(nowPlaying == nil ? AnyShapeStyle(.secondary) : AnyShapeStyle(Color.green))
                    .lineLimit(1)
            }
            .padding(m.padding)
            .frame(width: m.width, height: m.height, alignment: .leading)
            #if !os(tvOS)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(.quaternary, lineWidth: 1)
            }
            #endif
        }
        #if os(tvOS)
        // The card style owns the platter and the focus lift, as on the host cards.
        .buttonStyle(.card)
        #else
        .buttonStyle(.plain)
        #endif
        .accessibilityElement(children: .combine)
    }

    /// Touch and pointer sizes, and 10-foot ones on a TV.
    private struct Metrics {
        let width, height, padding, mark, dot, name, status: CGFloat

        static var current: Metrics {
            #if os(tvOS)
            Metrics(width: 380, height: 200, padding: 24, mark: 40, dot: 12, name: 28, status: 22)
            #else
            Metrics(width: 190, height: 108, padding: 12, mark: 22, dot: 7, name: 15, status: 12)
            #endif
        }
    }
}

/// What the host recorded about playing a title, in words: the grid's captions and the details
/// sheet's stats line share the phrasing.
enum PlayStatsText {
    /// `2 hr. ago`, or nil for a title never played.
    static func lastPlayed(_ stats: GameStats?) -> String? {
        guard let ms = stats?.lastPlayedUnixMs, ms > 0 else { return nil }
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        return relativeDate.localizedString(for: date, relativeTo: Date())
    }

    /// `14 hr` in all.
    static func playTime(_ stats: GameStats?) -> String? { duration(stats?.playTimeMs) }

    /// `1 hr`: the latest run, still growing while it runs.
    static func lastSession(_ stats: GameStats?) -> String? { duration(stats?.lastRunMs) }

    /// Under a minute says nothing: a launch that never really ran is not play time.
    private static func duration(_ ms: UInt64?) -> String? {
        guard let ms, ms >= 60_000 else { return nil }
        return Duration.milliseconds(Int64(clamping: ms)).formatted(
            .units(allowed: [.hours, .minutes], width: .abbreviated, maximumUnitCount: 1))
    }

    /// `Last played 2 hr. ago · 14 hr total · 12 launches`, or nil with nothing recorded.
    static func summary(_ stats: GameStats?) -> String? {
        var parts: [String] = []
        if let last = lastPlayed(stats) { parts.append("Last played \(last)") }
        if let total = playTime(stats) { parts.append("\(total) total") }
        if let count = stats?.launchCount, count > 0 {
            parts.append(count == 1 ? "1 launch" : "\(count) launches")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " \u{b7} ")
    }

    private static let relativeDate: RelativeDateTimeFormatter = {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter
    }()
}

/// The Library tab with no paired host: nothing to browse yet, and the way to fix that.
struct LibraryNoHostView: View {
    let showHosts: () -> Void

    var body: some View {
        ContentUnavailableView {
            Label("No Library Yet", systemImage: "square.grid.2x2")
        } description: {
            Text("Pair a host to browse its games here.")
        } actions: {
            Button("Show Hosts", action: showHosts)
        }
    }
}
