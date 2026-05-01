# macOS Packaging

This directory owns the macOS packaging layer for Codex App Transfer.

The shared application code stays in the repository root:

- `main.py`
- `backend/`
- `frontend/`

Do not copy shared runtime code into this directory. macOS-specific files here should be limited to packaging, signing, notarization, DMG creation, and release notes that only apply to macOS.

## Local Build

Run from the repository root on macOS:

```bash
./macos/build-macos.sh
```

The script installs Python dependencies, creates an `.icns` icon from `frontend/assets/app-icon.png`, builds the macOS app, creates a macOS installer package, and creates a drag-and-drop DMG.

All macOS build outputs are written to:

```text
dist/mac/
```

`<arch>` is detected from `uname -m` and is usually `arm64` or `x64`.

The output directory contains:

- `Codex App Transfer.app`
- `Codex-App-Transfer-v<version>-macOS-<arch>.pkg`
- `Codex-App-Transfer-v<version>-macOS-<arch>.dmg`

The DMG keeps the standard drag-and-drop layout: the `.app` bundle plus an Applications shortcut. If a user copies that app onto an existing app at the same Applications path, Finder handles the replacement prompt. The PKG installer installs to `/Applications/Codex App Transfer.app` and removes the previous app at that official install location before writing the new one.

The script sets `PYINSTALLER_CONFIG_DIR` to `.tmp/pyinstaller/` by default so PyInstaller cache writes stay inside the repository during local or sandboxed builds.

The macOS icon preparation step removes the baked checkerboard background from the shared PNG before creating the `.icns` file.

The version defaults to `APP_VERSION` in `main.py`. Set `CCDS_VERSION` only when intentionally overriding the packaged version.

## Update Assets

The in-app updater uses platform keys such as `macos-arm64` and `macos-x64` from `latest.json`. For macOS install actions, the app prefers a `.pkg` asset and falls back to a `.dmg` asset. The Windows release script stages matching macOS assets from `dist/mac/` into the release directory and includes them in `latest.json`.

## Runtime Behavior

The macOS runtime keeps the app as a normal Dock application. Closing the window hides it, and clicking the Dock icon restores the main window.

For Anthropic-compatible providers, the app writes the selected provider URL, API key, auth scheme, extra gateway headers, and model list directly into Claude Desktop's macOS 3P configuration. After applying the configuration and fully restarting Claude Desktop, Claude Desktop can keep using that provider even if Codex App Transfer is quit.

The local proxy is still used for experimental OpenAI, new-api, and reverse-proxy style providers that need request conversion. In that mode, keep Codex App Transfer running; closing the window only hides the app so the proxy can continue serving Claude Desktop through `127.0.0.1:<proxyPort>`. Quit with `Cmd+Q`, the Dock Quit command, or the app's Window menu when the proxy should stop.

## Optional Signing

For local unsigned testing, no environment variables are required.

For Developer ID signing, set:

```bash
export MACOS_CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
./macos/build-macos.sh
```

The script uses `macos/entitlements.plist` for hardened runtime signing.

For installer package signing, set:

```bash
export MACOS_INSTALLER_SIGN_IDENTITY="Developer ID Installer: Your Name (TEAMID)"
./macos/build-macos.sh
```

## Optional Notarization

Create a notarytool keychain profile outside the repository, then set:

```bash
export MACOS_CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export MACOS_NOTARY_KEYCHAIN_PROFILE="codex-app-transfer-notary"
./macos/build-macos.sh
```

The script submits the DMG with `xcrun notarytool`, waits for the result, and staples the returned ticket when notarization succeeds.

## Manual Smoke Test

After building on macOS:

```bash
open "dist/mac/Codex App Transfer.app"
```

Then verify:

- The desktop window opens.
- `http://127.0.0.1:18081/api/status` responds.
- Applying an Anthropic-compatible provider writes the macOS plist, root Claude-3p JSON config, and active `configLibrary` entry.
- Anthropic-compatible providers report direct-provider mode and do not require the proxy after Claude Desktop restarts.
- Experimental OpenAI/new-api providers start the proxy on port `18080` and keep working when the window is hidden.

## Notes

- PyInstaller is not a cross-compiler. Build macOS assets on macOS.
- Prefer the onedir `.app` bundle for macOS distribution. Onefile app bundles add startup overhead and are a poor fit for signed distribution.
- Public distribution should use Developer ID signing and Apple notarization.
- The first macOS package intentionally disables the `pystray` tray icon. Its AppKit backend calls `NSApplication.run`, which must stay on the main thread while pywebview owns the macOS UI loop. Window close is handled by the Dock app lifecycle instead of a tray icon.
