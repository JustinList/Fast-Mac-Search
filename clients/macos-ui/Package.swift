// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FastMacSearchUI",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(name: "FastMacSearchUI", targets: ["FastMacSearchUI"])
    ],
    targets: [
        .executableTarget(
            name: "FastMacSearchUI",
            path: "Sources/FastMacSearchUI",
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("Carbon"),
                .linkedFramework("Quartz"),
                .linkedFramework("ServiceManagement")
            ]
        )
    ]
)
