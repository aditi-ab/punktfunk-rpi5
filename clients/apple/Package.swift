// swift-tools-version: 5.9
// LumenKit — Swift wrapper around the lumen-core C ABI (lumen/1 client connector) plus the
// SwiftUI/VideoToolbox presentation layer. Build LumenCore.xcframework first:
//   bash ../../scripts/build-xcframework.sh   (on a Mac; see README.md)
import PackageDescription

let package = Package(
    name: "LumenKit",
    platforms: [.macOS(.v14), .iOS(.v17)],
    products: [
        .library(name: "LumenKit", targets: ["LumenKit"])
    ],
    targets: [
        .binaryTarget(name: "LumenCore", path: "LumenCore.xcframework"),
        .target(
            name: "LumenKit",
            dependencies: ["LumenCore"],
            linkerSettings: [
                // Rust staticlib system deps.
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("resolv"),
            ]
        ),
    ]
)
