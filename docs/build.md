# Building Codex App Transfer

This project ships installers for macOS arm64, Linux x86_64, and Windows x64.
The build pipeline runs entirely on a developer's macOS machine — macOS uses
the host's PyInstaller, Linux and Windows run inside Docker containers that
mount the project as a workspace.

## Prerequisites

- macOS host (Apple Silicon or Intel; only arm64 macOS PKG/DMG is produced —
  Intel mac builds would need an x86_64 macOS to run on)
- Python 3.11–3.14 with project deps installed; the repo's `.venv` is
  authoritative — see "venv setup" below
- For Linux + Windows targets: Docker Desktop or [OrbStack](https://orbstack.dev/)
  (Apple Silicon: OrbStack is lighter and faster than Docker Desktop)
- ~5 GB free disk for the two builder images plus build artifacts

## venv setup (one-time)

```bash
uv venv .venv                                  # or: python3 -m venv .venv
.venv/bin/python -m ensurepip --upgrade        # uv venvs ship without pip
uv pip install --python .venv/bin/python -r requirements.txt cryptography
```

`cryptography` is needed by `scripts/release_assets.py` to sign artifacts.
It's intentionally not in `requirements.txt` (excluded from the bundled app).

## One-shot release

```bash
make release VERSION=1.0.0
```

Runs `mac-release` → `linux-release` → `win-release` → final `release-bundle`
that regenerates `latest.json` from whatever is in `release/`.

Optional environment variables:

| Variable | Effect |
|---|---|
| `CCDS_REPO=owner/repo` | Embed `https://github.com/owner/repo/...` URLs in `latest.json` |
| `MACOS_CODESIGN_IDENTITY="Developer ID Application: ..."` | Apple Developer ID sign macOS bundle |
| `CCDS_SKIP_INSTALLER=1` | Skip NSIS in Windows build (faster iteration) |
| `WIN_IMAGE_TAG`, `LINUX_IMAGE_TAG` | Override Docker image tags |

## Per-platform details

### macOS

```bash
make mac-release VERSION=1.0.0
```

Calls `macos/build-macos.sh` (PyInstaller + `productbuild` + `hdiutil`),
then `release_assets.py --include macos`. Outputs:

- `dist/mac/Codex App Transfer.app`
- `release/Codex-App-Transfer-v1.0.0-macOS-arm64.pkg`
- `release/Codex-App-Transfer-v1.0.0-macOS-arm64.dmg`

Without `MACOS_CODESIGN_IDENTITY`, the app is ad-hoc signed only; first
launch requires right-click → Open to bypass Gatekeeper.

### Linux x86_64

```bash
make linux-release VERSION=1.0.0
```

Container: `docker/linux-builder/Dockerfile` (Ubuntu 22.04 + Python 3.10 +
GTK3 + WebKit2GTK 4.0 + libayatana-appindicator3 + PyInstaller deps). The
container builds *both* a folder mode and an onefile mode binary using
separate `--distpath` directories so they don't collide (Linux executables
have no extension to disambiguate). The host then bundles them.

Outputs:

- `release/Codex-App-Transfer-v1.0.0-Linux-x86_64.tar.gz` (folder build,
  tarred so file modes survive)
- `release/Codex-App-Transfer-v1.0.0-Linux-x86_64` (onefile, +x)

The bundled binary still requires GTK3 + WebKit2GTK 4.0 +
libayatana-appindicator3 *on the user's Linux box*. On Debian/Ubuntu:

```bash
sudo apt-get install libgtk-3-0 libwebkit2gtk-4.0-37 libayatana-appindicator3-1
```

### Windows x64

```bash
make win-release VERSION=1.0.0
```

Container: `docker/windows-builder/Dockerfile` (`tobix/pywine:3.12` + Linux
`makensis`). PyInstaller runs under Wine so the resulting `.exe` is a real
PE32+ binary; NSIS runs natively on Linux and produces the Setup installer.

Outputs:

- `release/Codex-App-Transfer-v1.0.0-Windows-Portable.zip` (folder build zipped)
- `release/Codex-App-Transfer-v1.0.0-Windows-x64.exe` (single-file)
- `release/Codex-App-Transfer-v1.0.0-Windows-Setup.exe` (NSIS installer)

#### Known Wine pitfalls

PyInstaller-via-Wine produces a real Windows binary, but the build environment
isn't quite real Windows. Things that have bitten us before:

- **WebView2 missing at runtime**: pywebview's Edge backend tries to load
  `Microsoft.Web.WebView2.*` DLLs. In our spec these come along via
  `collect_data_files("webview")`, but if pywebview adds backends or moves
  files between releases, the bundled `.exe` may launch a blank window.
- **Tray icon transparency / DPI**: pystray under Wine sometimes draws the
  tray icon with a black background on real Windows.
- **NSIS taskkill on update**: `installer.nsi` runs
  `taskkill /IM Codex-App-Transfer.exe /T /F` before overwrite. Wine's
  makensis builds it fine; verify the upgrade path on a real Windows install.

**Always smoke-test the Windows binary on a real Windows 10/11 machine
before publishing.** If something is broken, fall back to the Windows-native
build path below.

#### Windows-native fallback

On a real Windows machine:

```powershell
build.bat
# or:
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\New-Release.ps1 `
    -Version 1.0.0 -Build -TryInstaller -Repository Cmochance/codex-app-transfer
python scripts\release_assets.py --version 1.0.0 --include windows
```

Needs Python 3.11–3.13, NSIS 3.x (`makensis` on PATH), and
`pip install -r requirements.txt cryptography` already done.
With an Authenticode certificate, add
`-CodeSign -CodeSigningCertificateBase64 ... -CodeSigningCertificatePassword ...`.

## Verifying signatures

Public key: `release/Codex-App-Transfer-release-public.pem`. Scheme:
RSA-3072 PKCS#1 v1.5 over the raw file bytes, hashed with SHA-256, signature
stored as base64 in `<file>.sig`.

PowerShell (works on Windows 10+ with built-in PowerShell 5.1+):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Test-ReleaseSignature.ps1 `
    -File release\Codex-App-Transfer-v1.0.0-Windows-Setup.exe
```

Python (any platform with `cryptography` installed):

```bash
.venv/bin/python -c "
from pathlib import Path
import base64
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding
pub = serialization.load_pem_public_key(Path('release/Codex-App-Transfer-release-public.pem').read_bytes())
asset = 'release/Codex-App-Transfer-v1.0.0-Windows-Setup.exe'
sig = base64.b64decode(Path(asset+'.sig').read_text())
pub.verify(sig, Path(asset).read_bytes(), padding.PKCS1v15(), hashes.SHA256())
print('OK')
"
```

## Where things live

```
.
├── Makefile                          # mac-release / linux-release / win-release / release
├── build.spec                        # PyInstaller, used by all three platforms
├── installer.nsi                     # NSIS, runs in win-builder container
├── docker/
│   ├── linux-builder/
│   │   ├── Dockerfile
│   │   └── build.sh                  # entrypoint inside container
│   └── windows-builder/
│       ├── Dockerfile
│       └── build.sh
├── macos/
│   ├── build-macos.sh                # macOS host-side driver
│   └── build-macos.spec
└── scripts/
    ├── build-linux-on-mac.sh         # macOS → docker driver for Linux
    ├── build-windows-on-mac.sh       # macOS → docker driver for Windows
    ├── release_assets.py             # cross-platform sha256 + sig + latest.json
    └── Test-ReleaseSignature.ps1     # PowerShell verifier
```
