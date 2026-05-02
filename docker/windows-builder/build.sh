#!/usr/bin/env bash
# Runs INSIDE the windows-builder container. Driven by scripts/build-windows-on-mac.sh.
#
# Steps:
#   1. Refresh Windows Python deps via Wine (host source mounted at /workspace)
#   2. PyInstaller folder build  -> dist/Codex-App-Transfer/
#   3. PyInstaller onefile build -> dist/Codex-App-Transfer.exe
#   4. NSIS  -> Codex-App-Transfer-Setup-<version>.exe
#   5. Bundle release assets via scripts/release_assets.py --include windows

set -euo pipefail

cd /workspace

VERSION="${CCDS_VERSION:-1.0.0}"
SKIP_INSTALLER="${CCDS_SKIP_INSTALLER:-0}"

echo "==> Refreshing Windows Python deps via Wine"
wine python -m pip install --upgrade pip wheel
wine python -m pip install -r requirements.txt

echo "==> Cleaning previous Windows artifacts"
rm -rf dist/Codex-App-Transfer dist/Codex-App-Transfer.exe \
       Codex-App-Transfer-Setup-*.exe \
       build/build build/build-onefile 2>/dev/null || true

# Optional local-only pre-build hook (kept out of public repo).
if [[ -f scripts/_local_prebuild.sh ]]; then
    source scripts/_local_prebuild.sh "wine python" best-effort
fi

echo "==> PyInstaller (folder mode)"
unset CCDS_ONEFILE
wine python -m PyInstaller --noconfirm --clean build.spec

echo "==> PyInstaller (onefile mode)"
CCDS_ONEFILE=1 wine python -m PyInstaller --noconfirm --clean build.spec

if [[ ! -d "dist/Codex-App-Transfer" ]]; then
    echo "ERROR: dist/Codex-App-Transfer folder build missing" >&2
    exit 1
fi
if [[ ! -f "dist/Codex-App-Transfer.exe" ]]; then
    echo "ERROR: dist/Codex-App-Transfer.exe onefile build missing" >&2
    exit 1
fi

cp LICENSE.txt "dist/Codex-App-Transfer/LICENSE.txt"

if [[ "$SKIP_INSTALLER" != "1" ]]; then
    echo "==> NSIS installer (PRODUCT_VERSION=$VERSION)"
    # 单一版本源在 backend/config.py;installer.nsi 已改成 !ifndef PRODUCT_VERSION,
    # 这里通过 -D 注入 $VERSION,nsi 文件不再保留版本副本。
    makensis -DPRODUCT_VERSION="$VERSION" installer.nsi

    # 硬约束:默认就是要打 Setup,产物缺失要立刻报错而不是静默放过
    setup_exe="Codex-App-Transfer-Setup-${VERSION}.exe"
    if [[ ! -f "$setup_exe" ]]; then
        echo "ERROR: NSIS reported success but $setup_exe is missing." >&2
        echo "       Check installer.nsi syntax / OutFile / disk space." >&2
        exit 1
    fi
    echo "==> Setup installer ok: $setup_exe ($(stat -c %s "$setup_exe" 2>/dev/null || stat -f %z "$setup_exe") bytes)"
else
    echo "==> Skipping NSIS installer (CCDS_SKIP_INSTALLER=1)"
fi

echo "==> Done. Container produced raw artifacts; host driver will sign + index."
ls -la dist/ 2>/dev/null | head -20 || true
