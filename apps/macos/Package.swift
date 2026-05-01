// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "Rubberdux",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "Rubberdux",
            path: "Sources/Rubberdux"
        ),
        .testTarget(
            name: "RubberduxTests",
            dependencies: ["Rubberdux"],
            path: "Tests/RubberduxTests"
        ),
    ]
)
