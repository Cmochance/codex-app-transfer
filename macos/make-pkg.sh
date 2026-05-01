#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON_BIN="${PYTHON_BIN:-python3}"
detect_version() {
  "$PYTHON_BIN" - "$ROOT/main.py" <<'PY'
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(r'^APP_VERSION\s*=\s*["\']([^"\']+)["\']', text, re.MULTILINE)
if not match:
    raise SystemExit("APP_VERSION not found in main.py")
print(match.group(1))
PY
}

VERSION="${1:-${CCDS_VERSION:-$(detect_version)}}"
APP_PATH="${2:-dist/mac/Codex App Transfer.app}"
OUTPUT_PKG="${3:-dist/mac/Codex-App-Transfer-v${VERSION}-macOS.pkg}"

if [[ ! -d "$APP_PATH" ]]; then
  echo "App bundle not found: $APP_PATH" >&2
  exit 1
fi

if ! command -v pkgbuild >/dev/null 2>&1; then
  echo "pkgbuild is required to create a macOS installer package." >&2
  exit 1
fi

PKG_ROOT="$ROOT/.tmp/pkg-root"
SCRIPTS_DIR="$ROOT/macos/pkg-scripts"
APP_NAME="$(basename "$APP_PATH")"

rm -rf "$PKG_ROOT"
mkdir -p "$PKG_ROOT/Applications"
mkdir -p "$(dirname "$OUTPUT_PKG")"

ditto "$APP_PATH" "$PKG_ROOT/Applications/$APP_NAME"

rm -f "$OUTPUT_PKG"
pkgbuild \
  --root "$PKG_ROOT" \
  --install-location "/" \
  --identifier "io.github.lonr6.codexappswitch" \
  --version "$VERSION" \
  --scripts "$SCRIPTS_DIR" \
  "$OUTPUT_PKG"
