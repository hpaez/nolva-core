#!/usr/bin/env bash
# Builds the Android native libraries and Kotlin bindings into the Gradle
# library module at android/nolva-core. Requires the Android NDK and cargo-ndk.
#
# Environment:
#   ANDROID_NDK_HOME  NDK r28+ (falls back to ANDROID_NDK_LATEST_HOME on CI)
#   ANDROID_ABIS      space-separated ABIs (default: "arm64-v8a armeabi-v7a x86_64")
#   CARGO_PROFILE     cargo profile (default: release; CI uses "ci")
#   ANDROID_MIN_SDK   default 26
#
# Outputs (both git-ignored, regenerated on every build):
#   android/nolva-core/src/main/jniLibs/<abi>/libnolva_core.so
#   android/nolva-core/src/main/kotlin/app/nolva/core/nolva_core.kt
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# GitHub's Ubuntu runners expose the newest preinstalled NDK as
# ANDROID_NDK_LATEST_HOME; use it when ANDROID_NDK_HOME isn't set.
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}"
if [[ -z "$ANDROID_NDK_HOME" || ! -d "$ANDROID_NDK_HOME" ]]; then
  echo "error: set ANDROID_NDK_HOME to an Android NDK (r28+) directory" >&2
  exit 1
fi
echo "Using NDK at $ANDROID_NDK_HOME"

ANDROID_MIN_SDK="${ANDROID_MIN_SDK:-26}"
CARGO_PROFILE="${CARGO_PROFILE:-release}"
read -r -a ABIS <<< "${ANDROID_ABIS:-arm64-v8a armeabi-v7a x86_64}"

rust_target() {
  case "$1" in
    arm64-v8a) echo "aarch64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86_64) echo "x86_64-linux-android" ;;
    x86) echo "i686-linux-android" ;;
    *) echo "error: unknown ABI '$1'" >&2; return 1 ;;
  esac
}

MODULE="android/nolva-core/src/main"
JNI_LIBS="$MODULE/jniLibs"
KOTLIN_OUT="$MODULE/kotlin"

command -v cargo-ndk >/dev/null || {
  echo "error: cargo-ndk not found (cargo install cargo-ndk --locked)" >&2
  exit 1
}

targets=()
ndk_args=()
for abi in "${ABIS[@]}"; do
  targets+=("$(rust_target "$abi")")
  ndk_args+=(-t "$abi")
done

rustup target add "${targets[@]}"
rm -rf "$JNI_LIBS" "$KOTLIN_OUT/app/nolva/core"

cargo ndk "${ndk_args[@]}" --platform "$ANDROID_MIN_SDK" -o "$JNI_LIBS" \
  build --profile "$CARGO_PROFILE" -p core-ffi

# cargo puts the "dev" profile under target/<triple>/debug.
profile_dir="$CARGO_PROFILE"
[[ "$profile_dir" == "dev" ]] && profile_dir="debug"

bash scripts/generate-bindings.sh kotlin \
  "target/${targets[0]}/$profile_dir/libnolva_core.so" "$KOTLIN_OUT"

echo "Android libraries (${ABIS[*]}) in $JNI_LIBS, Kotlin bindings in $KOTLIN_OUT"
