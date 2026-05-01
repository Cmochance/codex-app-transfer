#!/usr/bin/env bash
# Runs INSIDE the linux-builder container.
#
# Steps:
#   1. PyInstaller folder build  -> dist/linux-folder/Codex-App-Transfer/
#   2. PyInstaller onefile build -> dist/linux-onefile/Codex-App-Transfer
#   3. release_assets.py --include linux -> release/* (.tar.gz + onefile + sigs)

set -euo pipefail

cd /workspace

VERSION="${CCDS_VERSION:-1.0.0}"

echo "==> Cleaning previous Linux artifacts"
rm -rf dist/linux-folder dist/linux-onefile build/build-linux 2>/dev/null || true

echo "==> PyInstaller folder mode (Linux x86_64)"
unset CCDS_ONEFILE
python3 -m PyInstaller --noconfirm --clean \
    --distpath dist/linux-folder --workpath build/build-linux \
    build.spec

echo "==> PyInstaller onefile mode (Linux x86_64)"
CCDS_ONEFILE=1 python3 -m PyInstaller --noconfirm --clean \
    --distpath dist/linux-onefile --workpath build/build-linux \
    build.spec

if [[ ! -d "dist/linux-folder/Codex-App-Transfer" ]]; then
    echo "ERROR: dist/linux-folder/Codex-App-Transfer missing" >&2
    exit 1
fi
if [[ ! -f "dist/linux-onefile/Codex-App-Transfer" ]]; then
    echo "ERROR: dist/linux-onefile/Codex-App-Transfer missing" >&2
    exit 1
fi

cp LICENSE.txt dist/linux-folder/Codex-App-Transfer/LICENSE.txt

echo "==> Done. Container produced raw artifacts; host driver will sign + index."
ls -la dist/linux-folder dist/linux-onefile 2>/dev/null || true
