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

let releaseURL = "https://api.github.com/repos/hpaez/nolva-core/releases/assets/790334641.zip" // nolva:url
let releaseChecksum = "888f62eb302c65f3834c372f72764fd08ecd18d6d722c02c7a199df06e90d0bc" // nolva:checksum

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
