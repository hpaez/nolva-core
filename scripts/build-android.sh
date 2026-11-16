#!/usr/bin/env bash
# Builds the Android native libraries and Kotlin bindings into the Gradle
# library module at android/nolva-core. Requires the Android NDK
# (ANDROID_NDK_HOME) and cargo-ndk.
#
# Outputs (both git-ignored, regenerated on every build):
#   android/nolva-core/src/main/jniLibs/<abi>/libnolva_core.so
#   android/nolva-core/src/main/kotlin/app/nolva/core/nolva_core.kt
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

: "${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must point to an Android NDK (r28+)}"
ANDROID_MIN_SDK="${ANDROID_MIN_SDK:-26}"

ABIS=("arm64-v8a" "armeabi-v7a" "x86_64")
RUST_TARGETS=("aarch64-linux-android" "armv7-linux-androideabi" "x86_64-linux-android")

MODULE="android/nolva-core/src/main"
JNI_LIBS="$MODULE/jniLibs"
KOTLIN_OUT="$MODULE/kotlin"

command -v cargo-ndk >/dev/null || {
  echo "error: cargo-ndk not found (cargo install cargo-ndk --locked)" >&2
  exit 1
}

rustup target add "${RUST_TARGETS[@]}"
rm -rf "$JNI_LIBS" "$KOTLIN_OUT/app/nolva/core"

ndk_args=()
for abi in "${ABIS[@]}"; do ndk_args+=(-t "$abi"); done

cargo ndk "${ndk_args[@]}" --platform "$ANDROID_MIN_SDK" -o "$JNI_LIBS" \
  build --release -p core-ffi

bash scripts/generate-bindings.sh kotlin \
  "target/aarch64-linux-android/release/libnolva_core.so" "$KOTLIN_OUT"

echo "Android libraries in $JNI_LIBS, Kotlin bindings in $KOTLIN_OUT"
