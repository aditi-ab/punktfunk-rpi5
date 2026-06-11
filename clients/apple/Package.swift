// swift-tools-version: 5.9
// PunktfunkKit — Swift wrapper around the punktfunk-core C ABI (punktfunk/1 client connector) plus the
// SwiftUI/VideoToolbox presentation layer. Build PunktfunkCore.xcframework first:
//   bash ../../scripts/build-xcframework.sh   (on a Mac; see README.md)
import PackageDescription

let package = Package(
    name: "PunktfunkKit",
    platforms: [.macOS(.v14), .iOS(.v17), .tvOS(.v17)],
    products: [
        .library(name: "PunktfunkKit", targets: ["PunktfunkKit"]),
        .executable(name: "PunktfunkClient", targets: ["PunktfunkClient"]),
    ],
    targets: [
        .binaryTarget(name: "PunktfunkCore", path: "PunktfunkCore.xcframework"),
        .target(
            name: "PunktfunkKit",
            dependencies: ["PunktfunkCore"],
            linkerSettings: [
                // Rust staticlib system deps.
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("resolv"),
            ]
        ),
        // Development app shell (swift run PunktfunkClient): connect form → stream + input.
        .executableTarget(name: "PunktfunkClient", dependencies: ["PunktfunkKit"]),
        .testTarget(name: "PunktfunkKitTests", dependencies: ["PunktfunkKit"]),
    ]
)
