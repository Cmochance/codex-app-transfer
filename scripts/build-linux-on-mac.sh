#!/usr/bin/env bash
# Build a Linux x86_64 release on macOS / Linux via the linux-builder Docker image.
# Usage:
#   scripts/build-linux-on-mac.sh [VERSION]
#
# Env overrides:
#   CCDS_REPO=owner/repo   embed asset URLs in latest.json
#   IMAGE_TAG=name:tag     override Docker image tag (default codex-app-transfer-linux:latest)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-1.0.0}"
IMAGE_TAG="${IMAGE_TAG:-codex-app-transfer-linux:latest}"

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
docker build --platform linux/amd64 -t "$IMAGE_TAG" -f docker/linux-builder/Dockerfile .

echo "==> Running Linux build container (version=$VERSION)"
docker run --rm --platform linux/amd64 \
    -v "$ROOT":/workspace \
    -e CCDS_VERSION="$VERSION" \
    -e CCDS_REPO="${CCDS_REPO:-}" \
    "$IMAGE_TAG"

echo
echo "Linux build done. Artifacts:"
ls -la "$ROOT/release" | grep -E "Linux|public.pem|latest" || true
