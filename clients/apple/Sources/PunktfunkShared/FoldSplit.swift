// A hinge across the session (design/touch-client-overlay.md §4.4). A foldable half-open and set
// down like a laptop is two screens: the picture takes the upright half, and the flat half takes
// the virtual pad while it is up, else a trackpad. The posture is the whole trigger, so opening
// the device flat puts the picture back over the full panel with nothing to undo. Twin of the
// Android client's `FoldSplit.kt`.

import CoreGraphics

/// The halves in points: the picture keeps `video`, the hinge eats `hinge`, the flat half takes
/// the rest.
public struct FoldSplit: Equatable, Sendable {
    public let video: CGFloat
    public let hinge: CGFloat

    public init(video: CGFloat, hinge: CGFloat) {
        self.video = video
        self.hinge = hinge
    }

    /// The split for a fold at `hinge` across a `size` container, or nil when this fold can't
    /// carry one. A hinge that doesn't span the width folds the screen left and right (book
    /// posture, with no flat half to rest a pad on), and a hinge close to either edge leaves a
    /// half too small to be a picture or a controller.
    ///
    /// Both are in the container's own coordinates: the session is edge to edge.
    public init?(hinge: CGRect, in size: CGSize) {
        guard size.width > 0, size.height > 0 else { return nil }
        // Half a point of slack: a region measured on a 3x panel lands off the integer grid.
        guard hinge.minX <= 0.5, hinge.maxX >= size.width - 0.5 else { return nil }
        let video = min(max(hinge.minY, 0), size.height)
        let gap = min(max(hinge.height, 0), size.height - video)
        let least = size.height / 5
        guard video >= least, size.height - video - gap >= least else { return nil }
        self.init(video: video, hinge: gap)
    }
}
