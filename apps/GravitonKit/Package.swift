// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "GravitonKit",
    platforms: [.macOS(.v15), .iOS(.v18)],
    products: [
        .library(name: "GravitonKit", targets: ["GravitonKit"]),
    ],
    targets: [
        .target(name: "GravitonKit"),
        .testTarget(name: "GravitonKitTests", dependencies: ["GravitonKit"]),
    ]
)
