// Procedural cover art for the screenshot shelf. The store's library frames used to render the
// deterministic text-placeholder posters (`artLoader: nil`), which read as an empty library next
// to the Android listing's populated one. These four posters are drawn with CoreGraphics at
// capture time — no bundled assets, nothing in a release build, and the same designs the Android
// harness draws in Canvas, so the two listings show the same shelf.

#if DEBUG
import CoreText
import Foundation
import ImageIO
import PunktfunkKit
import UniformTypeIdentifiers

/// A canned `LibraryArtSource`: poster bytes by URL, no network. What the screenshot shelf hands
/// the real coverflow in place of the paired-host loader.
struct ShotArtSource: LibraryArtSource {
    let fixtures: [String: Data]

    func data(for url: URL) async throws -> Data {
        guard let data = fixtures[url.absoluteString] else {
            throw CocoaError(.fileNoSuchFile)
        }
        return data
    }

    func close() async {}
}

enum ShotPosterArt {
    /// Art for `ShotMock.games` — keyed by the `shot://art/…` URLs those entries carry.
    static let source = ShotArtSource(fixtures: [
        "shot://art/aurora": poster("AURORA DRIFT", draw: drawAurora),
        "shot://art/starfall": poster("STARFALL VALE", draw: drawStarfall),
        "shot://art/neon": poster("NEON CIRCUIT", draw: drawNeon),
        "shot://art/ember": poster("EMBER PEAKS", draw: drawEmber),
    ])

    private static let W = 600
    private static let H = 900

    // MARK: - Canvas plumbing

    private static func poster(_ title: String, draw: (CGContext) -> Void) -> Data {
        let space = CGColorSpace(name: CGColorSpace.sRGB)!
        let ctx = CGContext(
            data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0,
            space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        draw(ctx)
        drawTitle(ctx, title)
        let image = ctx.makeImage()!
        let out = NSMutableData()
        let dest = CGImageDestinationCreateWithData(
            out, UTType.png.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, image, nil)
        CGImageDestinationFinalize(dest)
        return out as Data
    }

    private static func rgb(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
        CGColor(
            srgbRed: CGFloat((hex >> 16) & 0xff) / 255,
            green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
    }

    /// Vertical gradient over the full canvas; `stops` bottom-to-top as (location, color).
    private static func sky(_ ctx: CGContext, _ stops: [(CGFloat, CGColor)]) {
        let gradient = CGGradient(
            colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
            colors: stops.map(\.1) as CFArray,
            locations: stops.map(\.0))!
        ctx.drawLinearGradient(
            gradient, start: .zero, end: CGPoint(x: 0, y: CGFloat(H)), options: [])
    }

    private static func glowDot(
        _ ctx: CGContext, at center: CGPoint, radius: CGFloat, color: CGColor
    ) {
        let clear = color.copy(alpha: 0)!
        let gradient = CGGradient(
            colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
            colors: [color, clear] as CFArray, locations: [0, 1])!
        ctx.drawRadialGradient(
            gradient, startCenter: center, startRadius: 0,
            endCenter: center, endRadius: radius, options: [])
    }

    /// Stroke `path` three times, wide-and-faint to thin-and-bright, in screen blend — the cheap
    /// neon-glow trick every one of these posters leans on.
    private static func glowStroke(
        _ ctx: CGContext, _ path: CGPath, width: CGFloat, color: CGColor
    ) {
        ctx.saveGState()
        ctx.setBlendMode(.screen)
        ctx.setLineCap(.round)
        ctx.setLineJoin(.round)
        for (mult, alpha) in [(2.6, 0.12), (1.3, 0.28), (0.55, 0.85)] {
            ctx.addPath(path)
            ctx.setLineWidth(width * mult)
            ctx.setStrokeColor(color.copy(alpha: alpha)!)
            ctx.strokePath()
        }
        ctx.restoreGState()
    }

    private static func drawTitle(_ ctx: CGContext, _ title: String) {
        // A soft floor behind the caption keeps it legible over any art.
        sky(ctx, [(0, rgb(0x000000, 0.55)), (0.22, rgb(0x000000, 0))])
        let font = CTFontCreateWithName("HelveticaNeue-CondensedBold" as CFString, 46, nil)
        let text = NSAttributedString(string: title, attributes: [
            .font: font, .kern: 5, .foregroundColor: rgb(0xFFFFFF, 0.94),
        ] as [NSAttributedString.Key: Any])
        let line = CTLineCreateWithAttributedString(text)
        let bounds = CTLineGetBoundsWithOptions(line, [])
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -2), blur: 8, color: rgb(0x000000, 0.6))
        ctx.textPosition = CGPoint(x: (CGFloat(W) - bounds.width) / 2, y: 72)
        CTLineDraw(line, ctx)
        ctx.restoreGState()
    }

    /// Deterministic LCG so every capture draws the identical poster.
    private struct Rand {
        var state: UInt64
        mutating func next() -> CGFloat {
            state = state &* 6364136223846793005 &+ 1442695040888963407
            return CGFloat(state >> 33) / CGFloat(UInt64(1) << 31)
        }
        mutating func in_(_ lo: CGFloat, _ hi: CGFloat) -> CGFloat { lo + next() * (hi - lo) }
    }

    // MARK: - The four posters

    private static func drawAurora(_ ctx: CGContext) {
        sky(ctx, [(0, rgb(0x221E5C)), (0.45, rgb(0x141040)), (1, rgb(0x0B0830))])
        var rng = Rand(state: 11)
        for _ in 0..<48 {
            let p = CGPoint(x: rng.in_(0, 600), y: rng.in_(300, 890))
            glowDot(ctx, at: p, radius: rng.in_(1.4, 3.2), color: rgb(0xFFFFFF, rng.in_(0.25, 0.8)))
        }
        let ribbons: [(base: CGFloat, amp: CGFloat, freq: CGFloat, phase: CGFloat, w: CGFloat, c: UInt32)] = [
            (700, 55, 1.15, 0.4, 30, 0x6656F2),
            (615, 70, 1.4, 2.2, 24, 0x8F7BFF),
            (530, 45, 0.95, 4.1, 18, 0x35D0C5),
        ]
        for r in ribbons {
            let path = CGMutablePath()
            for i in 0...60 {
                let t = CGFloat(i) / 60
                let p = CGPoint(
                    x: t * 600,
                    y: r.base + r.amp * sin(t * .pi * r.freq + r.phase) + 40 * t)
                if i == 0 { path.move(to: p) } else { path.addLine(to: p) }
            }
            glowStroke(ctx, path, width: r.w, color: rgb(r.c))
        }
        // A low ridge grounds the scene — without it the poster's bottom half is bare sky.
        for (fill, baseline, rough) in [
            (rgb(0x191345), CGFloat(212), CGFloat(30)),
            (rgb(0x0E0A2E), CGFloat(148), CGFloat(38)),
        ] {
            let path = CGMutablePath()
            path.move(to: CGPoint(x: 0, y: 0))
            path.addLine(to: CGPoint(x: 0, y: baseline + rng.in_(-rough, rough)))
            for i in 1...9 {
                let x = CGFloat(i) / 9 * 600
                path.addLine(to: CGPoint(x: x, y: baseline + rng.in_(-rough, rough)))
            }
            path.addLine(to: CGPoint(x: 600, y: 0))
            path.closeSubpath()
            ctx.setFillColor(fill)
            ctx.addPath(path)
            ctx.fillPath()
        }
    }

    private static func drawStarfall(_ ctx: CGContext) {
        sky(ctx, [(0, rgb(0x2A0C24)), (0.35, rgb(0x7A2B58)), (0.8, rgb(0xE86FA8)), (1, rgb(0xF7A8C8))])
        var rng = Rand(state: 23)
        for _ in 0..<6 {
            let head = CGPoint(x: rng.in_(60, 560), y: rng.in_(420, 840))
            let len = rng.in_(90, 170)
            let dir = CGVector(dx: cos(2.15), dy: sin(2.15)) // ~123° — up-left tails
            let path = CGMutablePath()
            path.move(to: head)
            path.addLine(to: CGPoint(x: head.x + dir.dx * len, y: head.y + dir.dy * len))
            glowStroke(ctx, path, width: 4, color: rgb(0xFFE3EF))
            glowDot(ctx, at: head, radius: 11, color: rgb(0xFFFFFF, 0.9))
        }
        for (fill, baseline, rough) in [
            (rgb(0x3A1430), CGFloat(300), CGFloat(26)),
            (rgb(0x1D0818), CGFloat(216), CGFloat(34)),
        ] {
            let path = CGMutablePath()
            path.move(to: CGPoint(x: 0, y: 0))
            path.addLine(to: CGPoint(x: 0, y: baseline))
            for i in 1...8 {
                let x = CGFloat(i) / 8 * 600
                path.addLine(to: CGPoint(x: x, y: baseline + rng.in_(-rough, rough)))
            }
            path.addLine(to: CGPoint(x: 600, y: 0))
            path.closeSubpath()
            ctx.setFillColor(fill)
            ctx.addPath(path)
            ctx.fillPath()
        }
    }

    private static func drawNeon(_ ctx: CGContext) {
        sky(ctx, [(0, rgb(0x0A2A33)), (1, rgb(0x04161C))])
        var rng = Rand(state: 7)
        let ring = CGPath(
            ellipseIn: CGRect(x: 300 - 105, y: 560 - 105, width: 210, height: 210), transform: nil)
        glowStroke(ctx, ring, width: 10, color: rgb(0x35D0C5))
        for i in 0..<9 {
            // Right-angle traces on a 40 px grid, some feeding out of the ring's four gates.
            var p = i < 4
                ? CGPoint(x: 300 + [-105, 105, 0, 0][i], y: 560 + [0, 0, -105, 105][i])
                : CGPoint(x: 40 * (rng.in_(1, 14)).rounded(), y: 40 * (rng.in_(1, 21)).rounded())
            let path = CGMutablePath()
            path.move(to: p)
            var horizontal = rng.next() > 0.5
            for _ in 0..<Int(rng.in_(3, 6)) {
                let step = 40 * rng.in_(1, 4).rounded() * (rng.next() > 0.5 ? 1 : -1)
                p = horizontal ? CGPoint(x: min(max(p.x + step, 20), 580), y: p.y)
                               : CGPoint(x: p.x, y: min(max(p.y + step, 20), 880))
                path.addLine(to: p)
                horizontal.toggle()
            }
            let color = rng.next() > 0.6 ? rgb(0x7FE8DE) : rgb(0x35D0C5)
            glowStroke(ctx, path, width: 5, color: color)
            glowDot(ctx, at: p, radius: 12, color: color.copy(alpha: 0.9)!)
        }
    }

    private static func drawEmber(_ ctx: CGContext) {
        sky(ctx, [(0, rgb(0x200A04)), (0.3, rgb(0x7A2E12)), (0.42, rgb(0xEF8F4B)), (1, rgb(0x2A0E06))])
        glowDot(ctx, at: CGPoint(x: 300, y: 385), radius: 160, color: rgb(0xFFC37A, 0.85))
        var rng = Rand(state: 41)
        for (fill, baseline, rough) in [
            (rgb(0x5A2410), CGFloat(340), CGFloat(42)),
            (rgb(0x401708), CGFloat(255), CGFloat(56)),
            (rgb(0x200A04), CGFloat(165), CGFloat(48)),
        ] {
            let path = CGMutablePath()
            path.move(to: CGPoint(x: 0, y: 0))
            path.addLine(to: CGPoint(x: 0, y: baseline + rng.in_(-rough, rough)))
            for i in 1...10 {
                let x = CGFloat(i) / 10 * 600
                path.addLine(to: CGPoint(x: x, y: baseline + rng.in_(-rough, rough)))
            }
            path.addLine(to: CGPoint(x: 600, y: 0))
            path.closeSubpath()
            ctx.setFillColor(fill)
            ctx.addPath(path)
            ctx.fillPath()
        }
        for _ in 0..<20 {
            let p = CGPoint(x: rng.in_(30, 570), y: rng.in_(180, 620))
            glowDot(ctx, at: p, radius: rng.in_(2.5, 6), color: rgb(0xFFB067, rng.in_(0.35, 0.9)))
        }
    }
}
#endif
