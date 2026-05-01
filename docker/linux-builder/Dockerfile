# Linux x86_64 builder for Codex App Transfer.
# Produces a PyInstaller folder + onefile build inside a controlled Ubuntu
# image with the GTK/WebKit system libs pywebview needs at runtime.

FROM --platform=linux/amd64 ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# System deps:
#   pywebview Linux backend wants libgtk-3 + libwebkit2gtk-4.0 (Qt is also an
#   option, but GTK is what works most cleanly with pyinstaller bundling).
#   pystray Linux uses libappindicator3 + xorg/x11 stubs.
#   build-essential / pkg-config / libgirepository1.0-dev are needed because
#   pip will compile PyGObject from source for newer Pythons.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        python3 python3-venv python3-pip python3-dev \
        build-essential pkg-config \
        libgirepository1.0-dev libgtk-3-dev libwebkit2gtk-4.0-dev \
        libcairo2-dev libffi-dev libssl-dev \
        gir1.2-webkit2-4.0 gir1.2-gtk-3.0 \
        libayatana-appindicator3-dev \
        zip tar binutils ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Install Python deps (skip Darwin-only pyobjc-* lines via marker filtering;
# requirements.txt already gates them on sys_platform == "darwin").
COPY requirements.txt /tmp/requirements.txt
RUN python3 -m pip install --no-cache-dir --upgrade pip wheel \
 && python3 -m pip install --no-cache-dir -r /tmp/requirements.txt

WORKDIR /workspace
COPY docker/linux-builder/build.sh /usr/local/bin/build-linux
RUN chmod +x /usr/local/bin/build-linux

ENTRYPOINT ["/usr/local/bin/build-linux"]
