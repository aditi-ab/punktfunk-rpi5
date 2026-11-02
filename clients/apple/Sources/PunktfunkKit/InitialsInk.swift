// A profile's initials as a shape: the letters in the host card's mark and the picker's circle.

import SwiftUI

/// Initials as glyph outlines: centred across on their ink, down on the cap height. `Text`
/// centres its advance box and lands on whole pixels, a pixel off in the host card's mark.
public struct InitialsInk: Shape {
    /// Baseline at y = 0, y up.
    private let ink: Path
    private let capHeight: CGFloat

    /// nil when a glyph has no outline, as an emoji has none: the caller draws text instead.
    public init?(_ letters: String, fontSize: CGFloat) {
        #if os(macOS)
        let base = NSFont.systemFont(ofSize: fontSize, weight: .bold).fontDescriptor
        #else
        let base = UIFont.systemFont(ofSize: fontSize, weight: .bold).fontDescriptor
        #endif
        let font = CTFontCreateWithFontDescriptor(
            (base.withDesign(.rounded) ?? base) as CTFontDescriptor, fontSize, nil)
        let line = CTLineCreateWithAttributedString(NSAttributedString(
            string: letters,
            attributes: [NSAttributedString.Key(kCTFontAttributeName as String): font]))
        var ink = Path()
        for run in CTLineGetGlyphRuns(line) as! [CTRun] {
            // The run's face, not ours: a script ours lacks arrives in a fallback.
            let face = (CTRunGetAttributes(run) as NSDictionary)[kCTFontAttributeName] as! CTFont
            let count = CTRunGetGlyphCount(run)
            var glyphs = [CGGlyph](repeating: 0, count: count)
            var origins = [CGPoint](repeating: .zero, count: count)
            CTRunGetGlyphs(run, CFRange(), &glyphs)
            CTRunGetPositions(run, CFRange(), &origins)
            for (glyph, origin) in zip(glyphs, origins) {
                guard let outline = CTFontCreatePathForGlyph(face, glyph, nil) else { return nil }
                ink.addPath(
                    Path(outline), transform: CGAffineTransform(translationX: origin.x, y: origin.y))
            }
        }
        guard !ink.isEmpty else { return nil }
        self.ink = ink
        capHeight = CTFontGetCapHeight(font)
    }

    public func path(in rect: CGRect) -> Path {
        let box = ink.boundingRect
        // Wider than the circle: shrink to fit.
        let fit = min(1, rect.width / box.width)
        return ink.applying(
            CGAffineTransform(translationX: rect.midX, y: rect.midY)
                .scaledBy(x: fit, y: -fit)
                .translatedBy(x: -box.midX, y: -capHeight / 2))
    }
}
