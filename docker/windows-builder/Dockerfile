# Wine + Windows Python + NSIS image for cross-building Codex App Transfer
# from macOS / Linux. Uses Wine to run a Windows-native Python interpreter so
# PyInstaller produces real Windows binaries.
#
# Caveats: pywebview / pystray Windows binaries built via Wine sometimes have
# WebView2 / tray-icon glitches on real Windows. Always smoke-test the resulting
# .exe on a real Windows machine before publishing a release.

FROM tobix/pywine:3.12

ENV DEBIAN_FRONTEND=noninteractive

# NSIS for installer.nsi (native Linux makensis is fine).
# zip / unzip kept for ad-hoc debugging.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        nsis nsis-pluginapi \
        zip unzip ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Pre-install Windows Python deps to cache them in the image layer.
# Build context is the project root, so paths below are relative to it.
COPY requirements.txt /tmp/requirements.txt
RUN wine python -m pip install --upgrade pip wheel \
 && wine python -m pip install -r /tmp/requirements.txt

WORKDIR /workspace
COPY docker/windows-builder/build.sh /usr/local/bin/build-windows
RUN chmod +x /usr/local/bin/build-windows

ENTRYPOINT ["/usr/local/bin/build-windows"]
