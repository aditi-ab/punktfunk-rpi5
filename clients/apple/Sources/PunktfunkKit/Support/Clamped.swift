import CoreGraphics

extension CGFloat {
    /// Clamp into `range` — keeps the absolute-cursor mapping inside the host's pixel
    /// bounds even if a stray event reports a point a hair past the video rect.
    func clamped(to range: ClosedRange<CGFloat>) -> CGFloat {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
