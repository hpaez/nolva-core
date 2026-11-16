#!/usr/bin/env bash
# Points Package.swift's remote binary target at a released XCFramework.
#
# Usage: scripts/update-package-swift.sh <url> <checksum>
set -euo pipefail

URL="${1:?url required}"
CHECKSUM="${2:?checksum required}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FILE="$ROOT/Package.swift"

[[ "$CHECKSUM" =~ ^[0-9a-f]{64}$ ]] || { echo "error: invalid checksum" >&2; exit 1; }
[[ "$URL" =~ ^https:// ]] || { echo "error: url must be https" >&2; exit 1; }

# Lines are tagged with marker comments so the edit is exact and idempotent.
perl -0pi -e "s|let releaseURL = \".*?\" // nolva:url|let releaseURL = \"$URL\" // nolva:url|" "$FILE"
perl -0pi -e "s|let releaseChecksum = \".*?\" // nolva:checksum|let releaseChecksum = \"$CHECKSUM\" // nolva:checksum|" "$FILE"

grep -q "$CHECKSUM" "$FILE" || { echo "error: Package.swift not updated" >&2; exit 1; }
echo "Package.swift -> $URL ($CHECKSUM)"
