// swift-tools-version:5.9
//
// Swift Package for the Nolva core. The binary target is updated by the
// release workflow (scripts/update-package-swift.sh); do not edit the two
// marked lines by hand.
//
// Local development: build with scripts/build-ios.sh and set
// NOLVA_CORE_LOCAL=1 to use build/apple/NolvaCoreFFI.xcframework instead of
// the released artifact.

import Foundation
import PackageDescription

let releaseURL = "https://api.github.com/repos/hpaez/nolva-core/releases/assets/774771448.zip" // nolva:url
let releaseChecksum = "1c59601d504961fc2708917a6882ba52265f5be3b8c89015c5701942ce7d5eb2" // nolva:checksum

let useLocalBinary = ProcessInfo.processInfo.environment["NOLVA_CORE_LOCAL"] == "1"
    || releaseURL.isEmpty

let ffiTarget: Target = useLocalBinary
    ? .binaryTarget(name: "NolvaCoreFFI", path: "build/apple/NolvaCoreFFI.xcframework")
    : .binaryTarget(name: "NolvaCoreFFI", url: releaseURL, checksum: releaseChecksum)

let package = Package(
    name: "NolvaCore",
    platforms: [.iOS(.v16)],
    products: [
        .library(name: "NolvaCore", targets: ["NolvaCore"]),
    ],
    targets: [
        ffiTarget,
        .target(
            name: "NolvaCore",
            dependencies: ["NolvaCoreFFI"],
            path: "apple/Sources/NolvaCore"
        ),
    ]
)
