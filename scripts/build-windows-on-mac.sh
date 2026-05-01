#!/usr/bin/env bash
# Cross-build the Windows release on macOS / Linux via the windows-builder Docker image.
# Usage:
#   scripts/build-windows-on-mac.sh [VERSION]
#
# Env overrides:
#   CCDS_REPO=owner/repo     embed asset URLs in latest.json
#   CCDS_SKIP_INSTALLER=1    skip NSIS Setup .exe (still produces portable + onefile)
#   IMAGE_TAG=name:tag       override Docker image tag (default codex-app-transfer-win:latest)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-1.0.0}"
IMAGE_TAG="${IMAGE_TAG:-codex-app-transfer-win:latest}"

if ! command -v docker >/dev/null 2>&1; then
    if [[ -x "$HOME/.orbstack/bin/docker" ]]; then
        export PATH="$HOME/.orbstack/bin:$PATH"
    elif [[ -x "/Applications/OrbStack.app/Contents/MacOS/xbin/docker" ]]; then
        export PATH="/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH"
    else
        cat >&2 <<'EOF'
ERROR: docker not found.

Install Docker Desktop (https://www.docker.com/products/docker-desktop/) or
OrbStack (https://orbstack.dev/, recommended on Apple Silicon — lighter than
Docker Desktop). After install, start the daemon and retry.
EOF
        exit 127
    fi
fi

echo "==> Building Docker image: $IMAGE_TAG"
docker build --platform linux/amd64 -t "$IMAGE_TAG" -f docker/windows-builder/Dockerfile .

echo "==> Running Windows build container (version=$VERSION)"
docker run --rm --platform linux/amd64 \
    -v "$ROOT":/workspace \
    -e CCDS_VERSION="$VERSION" \
    -e CCDS_SKIP_INSTALLER="${CCDS_SKIP_INSTALLER:-0}" \
    -e CCDS_REPO="${CCDS_REPO:-}" \
    "$IMAGE_TAG"

echo
echo "Windows build done. Artifacts:"
ls -la "$ROOT/release" | grep -E "Windows|public.pem|latest" || true

cat <<'EOF'

Reminder: PyInstaller-via-Wine builds can have subtle issues with pywebview /
pystray on real Windows (WebView2, tray icon transparency, etc). Smoke-test the
.exe on a real Windows 10/11 machine before publishing.
EOF
