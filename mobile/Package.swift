// swift-tools-version: 6.1

import PackageDescription

// Assemble the debug XCFramework via :shared:assembleCagentsMobileSharedDebugXCFramework.
let package = Package(
    name: "CagentsNativeTransport",
    platforms: [.iOS(.v15)],
    products: [.library(name: "CagentsNativeTransport", targets: ["CagentsNativeTransport"])],
    dependencies: [.package(path: "ios")],
    targets: [
        .binaryTarget(name: "CagentsMobileShared",
                      path: "shared/build/XCFrameworks/debug/CagentsMobileShared.xcframework"),
        .target(name: "CagentsNativeTransport",
                dependencies: ["CagentsMobileShared", .product(name: "CagentsSSH", package: "ios")],
                path: "ios/Sources/CagentsNativeTransport"),
    ],
    swiftLanguageModes: [.v5]
)
