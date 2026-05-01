#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON_BIN="${PYTHON_BIN:-python3}"
detect_version() {
  # 唯一版本源:backend/config.py:APP_VERSION
  "$PYTHON_BIN" - "$ROOT/backend/config.py" <<'PY'
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(r'^APP_VERSION\s*=\s*["\']([^"\']+)["\']', text, re.MULTILINE)
if not match:
    raise SystemExit("APP_VERSION not found in backend/config.py")
print(match.group(1))
PY
}

VERSION="${1:-${CCDS_VERSION:-$(detect_version)}}"
SOURCE_PATH="${2:-dist/mac/Codex App Transfer.app}"
OUTPUT_DMG="${3:-dist/mac/Codex-App-Transfer-v${VERSION}-macOS.dmg}"

if [[ ! -e "$SOURCE_PATH" ]]; then
  echo "DMG source not found: $SOURCE_PATH" >&2
  exit 1
fi

if ! command -v hdiutil >/dev/null 2>&1; then
  echo "hdiutil is required to create a DMG." >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT_DMG")"
STAGING="$(mktemp -d)"
cleanup() {
  rm -rf "$STAGING"
}
trap cleanup EXIT

cp -R "$SOURCE_PATH" "$STAGING/"
if [[ "$SOURCE_PATH" == *.app ]]; then
  ln -s /Applications "$STAGING/Applications"
fi

rm -f "$OUTPUT_DMG"
hdiutil create \
  -volname "Codex App Transfer ${VERSION}" \
  -srcfolder "$STAGING" \
  -ov \
  -format UDZO \
  "$OUTPUT_DMG"
