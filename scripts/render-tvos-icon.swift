// Usage: swift scripts/render-tvos-icon.swift <layer-export-dir> 
//        "clients/apple/App/Assets.xcassets/App Icon & Top Shelf Image.brandassets"
// Icon Composer has no tvOS support — tvOS wants flat parallax layers (brand assets),
// so this renders them from the same Affinity layer exports the .icon bundle uses.
//
// Renders the tvOS parallax icon layers from the flat Icon Composer layer exports:
// gradient background (the icon.json automatic-gradient violet) + the three art layers
// on transparent canvases, at every size the brand asset needs.
import AppKit

let export = CommandLine.arguments[1]
let outDir = CommandLine.arguments[2]

func bitmap(_ size: NSSize, draw: () -> Void) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(size.width), pixelsHigh: Int(size.height),
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = size
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw()
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

func write(_ data: Data, _ path: String) {
    let url = URL(fileURLWithPath: outDir).appendingPathComponent(path)
    try! FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    try! data.write(to: url)
    print(path)
}

// icon.json: automatic-gradient display-p3 (0.395, 0.306, 0.963) — approximated as a
// vertical sRGB gradient around the brand violet.
func gradientPNG(_ size: NSSize) -> Data {
    bitmap(size) {
        let top = NSColor(srgbRed: 0.49, green: 0.42, blue: 0.97, alpha: 1)
        let bottom = NSColor(srgbRed: 0.35, green: 0.26, blue: 0.91, alpha: 1)
        NSGradient(starting: top, ending: bottom)!
            .draw(in: NSRect(origin: .zero, size: size), angle: -90)
    }
}

func layerPNG(_ name: String, _ size: NSSize, heightFraction: CGFloat) -> Data {
    let image = NSImage(contentsOfFile: "\(export)/\(name)")!
    return bitmap(size) {
        let h = size.height * heightFraction
        let rect = NSRect(x: (size.width - h) / 2, y: (size.height - h) / 2, width: h, height: h)
        image.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1)
    }
}

let l1 = "punktfunk_Minimal_Icon-Composer_Layer-1.png" // light circle (back-most art)
let l2 = "punktfunk_Minimal_Icon-Composer_Layer-2.png" // dark circle
let l3 = "punktfunk_Minimal_Icon-Composer_Layer-3.png" // blob (front)

// App icon stacks: art at 92% of canvas height (tvOS crops edges in the focus effect).
for (stack, sizes) in [
    ("App Icon.imagestack", [("@1x", NSSize(width: 400, height: 240)),
                             ("@2x", NSSize(width: 800, height: 480))]),
    ("App Icon - App Store.imagestack", [("@1x", NSSize(width: 1280, height: 768))]),
] {
    for (suffix, size) in sizes {
        write(gradientPNG(size), "\(stack)/Back.imagestacklayer/Content.imageset/back\(suffix).png")
        write(layerPNG(l1, size, heightFraction: 0.92), "\(stack)/Circle1.imagestacklayer/Content.imageset/circle1\(suffix).png")
        write(layerPNG(l2, size, heightFraction: 0.92), "\(stack)/Circle2.imagestacklayer/Content.imageset/circle2\(suffix).png")
        write(layerPNG(l3, size, heightFraction: 0.92), "\(stack)/Front.imagestacklayer/Content.imageset/front\(suffix).png")
    }
}

// Top shelf images: flat composite (no parallax stack), mark at 70% height.
func shelfPNG(_ size: NSSize) -> Data {
    let images = [l1, l2, l3].map { NSImage(contentsOfFile: "\(export)/\($0)")! }
    return bitmap(size) {
        let top = NSColor(srgbRed: 0.49, green: 0.42, blue: 0.97, alpha: 1)
        let bottom = NSColor(srgbRed: 0.35, green: 0.26, blue: 0.91, alpha: 1)
        NSGradient(starting: top, ending: bottom)!
            .draw(in: NSRect(origin: .zero, size: size), angle: -90)
        let h = size.height * 0.7
        let rect = NSRect(x: (size.width - h) / 2, y: (size.height - h) / 2, width: h, height: h)
        for image in images {
            image.draw(in: rect, from: .zero, operation: .sourceOver, fraction: 1)
        }
    }
}
write(shelfPNG(NSSize(width: 1920, height: 720)), "Top Shelf Image.imageset/shelf@1x.png")
write(shelfPNG(NSSize(width: 3840, height: 1440)), "Top Shelf Image.imageset/shelf@2x.png")
write(shelfPNG(NSSize(width: 2320, height: 720)), "Top Shelf Image Wide.imageset/shelf-wide@1x.png")
write(shelfPNG(NSSize(width: 4640, height: 1440)), "Top Shelf Image Wide.imageset/shelf-wide@2x.png")
