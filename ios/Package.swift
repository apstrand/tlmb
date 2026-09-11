// swift-tools-version: 6.0

import PackageDescription
import Foundation

// The Rust staticlib lives in the workspace target/ dir, next to this package.
let workspace = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()   // ios/
    .deletingLastPathComponent()   // repo root
    .path
let rustLibDir = "\(workspace)/target/aarch64-apple-ios/release"

let package = Package(
    name: "mailbox",
    platforms: [
        .iOS(.v18),
    ],
    products: [
        .library(
            name: "mailbox",
            targets: ["mailbox"]
        ),
    ],
    targets: [
        .systemLibrary(
            name: "CMailbox",
            path: "CMailbox"
        ),
        .target(
            name: "mailbox",
            dependencies: ["CMailbox"],
            path: "Mailbox",
            exclude: [
                "Info.plist",
                "Mailbox-Bridging-Header.h",
                "PrivacyInfo.xcprivacy",
                "Assets.xcassets",
            ],
            swiftSettings: [
                // The Xcode target builds in Swift 5 mode (SWIFT_VERSION = 5.0);
                // keep the same language mode here.
                .swiftLanguageMode(.v5),
            ],
            linkerSettings: [
                .unsafeFlags(["-L\(rustLibDir)"]),
                .linkedLibrary("mailbox_ffi"),
            ]
        ),
    ]
)
