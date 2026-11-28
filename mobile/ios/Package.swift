// swift-tools-version: 6.1

import PackageDescription

let package = Package(
    name: "CagentsMobileSSH",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "CagentsSSH", targets: ["CagentsSSH"]),
        .executable(name: "cagents-ssh-probe", targets: ["CagentsSSHProbe"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-nio-ssh.git", exact: "0.15.0"),
        .package(url: "https://github.com/apple/swift-nio.git", exact: "2.105.2"),
        .package(url: "https://github.com/apple/swift-crypto.git", exact: "4.5.2"),
    ],
    targets: [
        .target(name: "CagentsSSH", dependencies: [
            .product(name: "Crypto", package: "swift-crypto"),
            .product(name: "NIOCore", package: "swift-nio"),
            .product(name: "NIOPosix", package: "swift-nio"),
            .product(name: "NIOSSH", package: "swift-nio-ssh"),
        ]),
        .executableTarget(name: "CagentsSSHProbe", dependencies: ["CagentsSSH"]),
    ],
    swiftLanguageModes: [.v5]
)
