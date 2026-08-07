// Reusable library widgets, shared by the touch grid (LibraryView's `GameCard`) and the gamepad
// coverflow (LibraryCoverflowView's cover cell).

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

/// Sequentially tries cover-art URLs over `session` (so a paired client can reach the host's own
/// art proxy, not just public CDNs — see `LibraryImageLoader`), advancing past any that fail to
/// load, then a placeholder. The loaded image is hard-clipped to fill the card's actual frame
/// regardless of its own aspect ratio: a portrait capsule fills it as intended, but a fallback
/// banner (wide hero/header art, used when a title has no portrait capsule) would otherwise report
/// a much wider intrinsic size than the card and overflow into neighboring cards. Not `private` —
/// the gamepad coverflow (`LibraryCoverflowView`) reuses it directly rather than re-fetching art.
struct PosterImage: View {
    let candidates: [URL]
    let title: String
    let session: URLSession?
    @State private var index = 0
    @State private var image: PlatformImage?

    var body: some View {
        Group {
            if let image {
                Image(platformImage: image)
                    .resizable()
                    .scaledToFill()
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
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .task(id: index) { await loadCurrent() }
    }

    private func loadCurrent() async {
        guard index < candidates.count else { return }
        guard let session, let data = try? await session.data(from: candidates[index]).0,
              let loaded = PlatformImage(data: data)
        else {
            index += 1 // advance to the next candidate (or past the end → placeholder)
            return
        }
        image = loaded
    }

    private var placeholder: some View {
        ZStack {
            Rectangle().fill(.quaternary)
            Text(title)
                .font(.geist(17, .semibold, relativeTo: .headline))
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(8)
        }
    }
}
