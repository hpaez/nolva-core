#!/usr/bin/env bash
# Builds NolvaCoreFFI.xcframework (+ zip and SwiftPM checksum) and the Swift
# bindings. Requires macOS with Xcode.
#
# Outputs:
#   build/apple/NolvaCoreFFI.xcframework
#   build/apple/NolvaCoreFFI.xcframework.zip
#   build/apple/NolvaCoreFFI.xcframework.zip.checksum
#   apple/Sources/NolvaCore/NolvaCore.swift   (generated, committed on release)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "error: iOS builds require macOS with Xcode" >&2
  exit 1
fi

export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"

LIB_NAME="libnolva_core.a"
DEVICE_TARGET="aarch64-apple-ios"
SIM_TARGETS=("aarch64-apple-ios-sim" "x86_64-apple-ios")

OUT="build/apple"
GEN="$OUT/generated"
HEADERS="$OUT/headers"
XCFRAMEWORK="$OUT/NolvaCoreFFI.xcframework"

rm -rf "$OUT"
mkdir -p "$GEN" "$HEADERS" "$OUT/sim"

rustup target add "$DEVICE_TARGET" "${SIM_TARGETS[@]}"

for target in "$DEVICE_TARGET" "${SIM_TARGETS[@]}"; do
  cargo build --release -p core-ffi --target "$target"
done

# Simulator slices must be a single fat library per platform.
lipo -create \
  "target/aarch64-apple-ios-sim/release/$LIB_NAME" \
  "target/x86_64-apple-ios/release/$LIB_NAME" \
  -output "$OUT/sim/$LIB_NAME"

bash scripts/generate-bindings.sh swift "target/$DEVICE_TARGET/release/$LIB_NAME" "$GEN"

# xcframework headers dir: C header + a modulemap named module.modulemap.
cp "$GEN/NolvaCoreFFI.h" "$HEADERS/"
cp "$GEN/NolvaCoreFFI.modulemap" "$HEADERS/module.modulemap"

xcodebuild -create-xcframework \
  -library "target/$DEVICE_TARGET/release/$LIB_NAME" -headers "$HEADERS" \
  -library "$OUT/sim/$LIB_NAME" -headers "$HEADERS" \
  -output "$XCFRAMEWORK"

(cd "$OUT" && ditto -c -k --sequesterRsrc --keepParent NolvaCoreFFI.xcframework NolvaCoreFFI.xcframework.zip)
swift package compute-checksum "$OUT/NolvaCoreFFI.xcframework.zip" > "$OUT/NolvaCoreFFI.xcframework.zip.checksum"

mkdir -p apple/Sources/NolvaCore
cp "$GEN/NolvaCore.swift" apple/Sources/NolvaCore/NolvaCore.swift

echo "XCFramework: $XCFRAMEWORK"
echo "Checksum:    $(cat "$OUT/NolvaCoreFFI.xcframework.zip.checksum")"
