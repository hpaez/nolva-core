#!/usr/bin/env bash
# Generates Kotlin and/or Swift bindings from a compiled core library.
#
# Usage: scripts/generate-bindings.sh <kotlin|swift> <path-to-libnolva_core> <out-dir>
#
# Library mode reads the UniFFI metadata embedded in the binary, so any build
# of the library (host or target) works as input.
set -euo pipefail

LANGUAGE="${1:?language (kotlin|swift) required}"
LIBRARY="${2:?path to compiled library required}"
OUT_DIR="${3:?output directory required}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

mkdir -p "$OUT_DIR"
cargo run --quiet --release -p uniffi-bindgen -- generate \
  --library "$LIBRARY" \
  --language "$LANGUAGE" \
  --no-format \
  --out-dir "$OUT_DIR"

echo "Generated $LANGUAGE bindings in $OUT_DIR"
