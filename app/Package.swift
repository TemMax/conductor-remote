// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "ConductorRemoteKit",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "ConductorRemoteKit", targets: ["ConductorRemoteKit"]),
    ],
    targets: [
        .target(name: "ConductorRemoteKit"),
        .testTarget(name: "ConductorRemoteKitTests", dependencies: ["ConductorRemoteKit"]),
    ]
)
