#!/usr/bin/env python3
"""Bundle build artifacts into release/ with sha256 + signature + latest.json.

Cross-platform replacement for the asset bundling part of New-Release.ps1.
Runs on macOS (after macos/build-macos.sh) and inside the Linux / Windows
builder containers (after PyInstaller + makensis).

Designed for incremental, per-platform invocation: re-running with --include
windows replaces only the Windows-* files in release/ and keeps macOS / Linux
files untouched. latest.json is regenerated from whatever is currently in
release/ (i.e. it always reflects the latest signed artifacts on disk).

Signature scheme: RSA-3072 PKCS#1 v1.5 + SHA-256, key in PEM.
Verifier: use the Python cryptography snippet in README.md.
"""
from __future__ import annotations

import argparse
import base64
import datetime as _dt
import hashlib
import json
import os
import re
import shutil
import sys
import tarfile
import zipfile
from pathlib import Path

try:
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import padding, rsa
except ImportError:
    sys.stderr.write(
        "Missing dependency: cryptography. Install with `pip install cryptography`.\n"
    )
    sys.exit(2)

PROJECT_NAME = "Codex App Transfer"
ASSET_PREFIX = "Codex-App-Transfer"
PUBLIC_KEY_BASENAME = f"{ASSET_PREFIX}-release-public.pem"

# Per-platform patterns and the platforms[] key in latest.json.
# Each pattern is matched against the basename in release/.
PLATFORM_PATTERNS: dict[str, list[tuple[str, str]]] = {
    "windows": [
        (r"-Windows-Portable\.zip$", "windows-x64"),
        (r"-Windows-x64\.exe$", "windows-x64"),
        (r"-Windows-Setup\.exe$", "windows-x64"),
    ],
    "macos": [
        (r"-macOS-arm64\.(?:pkg|dmg)$", "macos-arm64"),
        (r"-macOS-x64\.(?:pkg|dmg)$", "macos-x64"),
    ],
    "linux": [
        (r"-Linux-x86_64\.tar\.gz$", "linux-x86_64"),
        (r"-Linux-x86_64$", "linux-x86_64"),
    ],
}


def project_root() -> Path:
    return Path(__file__).resolve().parent.parent


def get_or_create_key(key_dir: Path, release_dir: Path) -> rsa.RSAPrivateKey:
    key_dir.mkdir(parents=True, exist_ok=True)
    private_path = key_dir / "release-private-key.pem"
    public_path = key_dir / "release-public-key.pem"

    if private_path.exists():
        private_key = serialization.load_pem_private_key(
            private_path.read_bytes(), password=None
        )
    else:
        private_key = rsa.generate_private_key(public_exponent=65537, key_size=3072)
        private_path.write_bytes(
            private_key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.PKCS8,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )
        public_path.write_bytes(
            private_key.public_key().public_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PublicFormat.SubjectPublicKeyInfo,
            )
        )
        print(f"Created local release signing key: {private_path}")

    release_dir.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(public_path, release_dir / PUBLIC_KEY_BASENAME)
    return private_key


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def sign_file(path: Path, private_key: rsa.RSAPrivateKey) -> Path:
    sig = private_key.sign(path.read_bytes(), padding.PKCS1v15(), hashes.SHA256())
    sig_path = path.with_name(path.name + ".sig")
    sig_path.write_text(base64.b64encode(sig).decode("ascii"))
    return sig_path


def write_sha256(path: Path) -> Path:
    digest = sha256_of(path)
    sha_path = path.with_name(path.name + ".sha256")
    sha_path.write_text(f"{digest}  {path.name}\n")
    return sha_path


def asset_url(repo: str | None, version: str, filename: str) -> str:
    if repo:
        return f"https://github.com/{repo}/releases/download/v{version}/{filename}"
    return filename


def sign_and_index(
    path: Path, private_key: rsa.RSAPrivateKey, repo: str | None, version: str
) -> dict:
    write_sha256(path)
    sign_file(path, private_key)
    return {
        "name": path.name,
        "url": asset_url(repo, version, path.name),
        "signature": path.name + ".sig",
        "sha256": sha256_of(path),
        "size": path.stat().st_size,
    }


def clean_platform(release_dir: Path, platform_name: str) -> None:
    """Remove existing release/ files for a single platform (binaries + .sha256 + .sig)."""
    if not release_dir.exists():
        return
    for entry in list(release_dir.iterdir()):
        if not entry.is_file():
            continue
        base = entry.name
        for trail in (".sha256", ".sig"):
            if base.endswith(trail):
                base = base[: -len(trail)]
                break
        for pattern, _platform_key in PLATFORM_PATTERNS[platform_name]:
            if re.search(pattern, base):
                entry.unlink()
                break


def collect_windows(root: Path, release_dir: Path, version: str) -> list[Path]:
    dist = root / "dist"
    folder = dist / ASSET_PREFIX
    onefile = dist / f"{ASSET_PREFIX}.exe"
    setup = root / f"{ASSET_PREFIX}-Setup-{version}.exe"

    out: list[Path] = []

    if folder.is_dir():
        portable = release_dir / f"{ASSET_PREFIX}-v{version}-Windows-Portable.zip"
        if portable.exists():
            portable.unlink()
        with zipfile.ZipFile(portable, "w", zipfile.ZIP_DEFLATED) as zf:
            for p in folder.rglob("*"):
                if p.is_file():
                    zf.write(p, p.relative_to(folder))
        out.append(portable)

    if onefile.is_file():
        target = release_dir / f"{ASSET_PREFIX}-v{version}-Windows-x64.exe"
        shutil.copyfile(onefile, target)
        out.append(target)

    if setup.is_file():
        target = release_dir / f"{ASSET_PREFIX}-v{version}-Windows-Setup.exe"
        shutil.copyfile(setup, target)
        out.append(target)
    elif folder.is_dir() or onefile.is_file():
        # 有 portable / onefile 但没 Setup 时,Setup 缺失绝大概率是 build 没跑全;
        # 报错而不是静默(老版本就是这里漏了,导致 1.0.1 release 没 Setup 包)。
        if os.environ.get("CCDS_SKIP_INSTALLER") != "1":
            raise SystemExit(
                f"ERROR: Windows Portable/onefile present but Setup installer missing.\n"
                f"       Expected: {setup}\n"
                f"       Re-run windows builder; if you really mean to skip, set CCDS_SKIP_INSTALLER=1."
            )

    return out


def collect_mac(root: Path, release_dir: Path, version: str) -> list[Path]:
    mac_dist = root / "dist" / "mac"
    if not mac_dist.is_dir():
        return []

    out: list[Path] = []
    for arch in ("arm64", "x64"):
        for ext in ("pkg", "dmg"):
            src = mac_dist / f"{ASSET_PREFIX}-v{version}-macOS-{arch}.{ext}"
            if src.is_file():
                target = release_dir / src.name
                shutil.copyfile(src, target)
                out.append(target)
    return out


def collect_linux(root: Path, release_dir: Path, version: str) -> list[Path]:
    folder = root / "dist" / "linux-folder" / ASSET_PREFIX
    onefile = root / "dist" / "linux-onefile" / ASSET_PREFIX

    out: list[Path] = []

    if folder.is_dir():
        tarball = release_dir / f"{ASSET_PREFIX}-v{version}-Linux-x86_64.tar.gz"
        if tarball.exists():
            tarball.unlink()
        with tarfile.open(tarball, "w:gz") as tar:
            tar.add(folder, arcname=ASSET_PREFIX)
        out.append(tarball)

    if onefile.is_file():
        target = release_dir / f"{ASSET_PREFIX}-v{version}-Linux-x86_64"
        shutil.copyfile(onefile, target)
        target.chmod(0o755)
        out.append(target)

    return out


def existing_assets_for_platform(
    release_dir: Path, version: str, platform_name: str
) -> list[Path]:
    """Find already-signed release/ files matching patterns for a platform.

    Used when generating latest.json after a partial run, so platforms that
    weren't rebuilt this invocation are still included.
    """
    if not release_dir.exists():
        return []
    found: list[Path] = []
    for entry in release_dir.iterdir():
        if not entry.is_file() or entry.name.endswith((".sha256", ".sig")):
            continue
        if entry.name == PUBLIC_KEY_BASENAME or entry.name.startswith("latest.json"):
            continue
        if not entry.name.startswith(f"{ASSET_PREFIX}-v{version}-"):
            continue
        for pattern, _platform_key in PLATFORM_PATTERNS[platform_name]:
            if re.search(pattern, entry.name):
                found.append(entry)
                break
    return found


def asset_dict_from_existing(
    path: Path, repo: str | None, version: str
) -> dict | None:
    sig_path = path.with_name(path.name + ".sig")
    if not sig_path.exists():
        return None
    return {
        "name": path.name,
        "url": asset_url(repo, version, path.name),
        "signature": path.name + ".sig",
        "sha256": sha256_of(path),
        "size": path.stat().st_size,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument(
        "--include",
        nargs="+",
        default=["windows", "macos", "linux"],
        choices=["windows", "macos", "linux"],
        help="Which platforms' artifacts to (re)scan and sign (default: all three).",
    )
    parser.add_argument(
        "--output-dir",
        default="release",
        help="Output directory under project root (default: release).",
    )
    parser.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY"),
        help="owner/repo for asset URLs in latest.json (optional).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    root = project_root()
    release_dir = root / args.output_dir
    key_dir = root / ".release-signing"

    release_dir.mkdir(parents=True, exist_ok=True)
    private_key = get_or_create_key(key_dir, release_dir)

    include = set(args.include)
    platforms: dict[str, list[dict]] = {}

    # Process --include platforms: clean their files in release/, copy/zip from
    # dist/, sign, and record the asset dict.
    for platform_name, collector in (
        ("windows", collect_windows),
        ("macos", collect_mac),
        ("linux", collect_linux),
    ):
        if platform_name not in include:
            continue
        clean_platform(release_dir, platform_name)
        files = collector(root, release_dir, args.version)
        for f in files:
            asset = sign_and_index(f, private_key, args.repo, args.version)
            for pattern, platform_key in PLATFORM_PATTERNS[platform_name]:
                if re.search(pattern, f.name):
                    platforms.setdefault(platform_key, []).append(asset)
                    break

    # Pick up platforms that weren't rebuilt this run by reading the already-signed
    # files left in release/ from previous invocations.
    for platform_name in PLATFORM_PATTERNS:
        if platform_name in include:
            continue
        for f in existing_assets_for_platform(release_dir, args.version, platform_name):
            asset = asset_dict_from_existing(f, args.repo, args.version)
            if asset is None:
                continue
            for pattern, platform_key in PLATFORM_PATTERNS[platform_name]:
                if re.search(pattern, f.name):
                    platforms.setdefault(platform_key, []).append(asset)
                    break

    # Stable ordering inside each platform's assets list (by filename).
    sorted_platforms: dict[str, dict] = {}
    for key in sorted(platforms):
        sorted_platforms[key] = {
            "assets": sorted(platforms[key], key=lambda a: a["name"])
        }

    latest = {
        "name": PROJECT_NAME,
        "version": args.version,
        "pub_date": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "notes": f"Release for {PROJECT_NAME} v{args.version}.",
        "update_protocol": 1,
        "minimum_supported_version": "1.0.0",
        "platforms": sorted_platforms,
        "signature": {
            "algorithm": "RSA-PKCS1-V15-SHA256",
            "public_key": PUBLIC_KEY_BASENAME,
            "format": "base64 raw signature over file bytes",
        },
    }

    latest_path = release_dir / "latest.json"
    latest_path.write_text(json.dumps(latest, indent=2, ensure_ascii=False))
    write_sha256(latest_path)
    sign_file(latest_path, private_key)

    print("\nRelease assets in", release_dir)
    for entry in sorted(release_dir.iterdir()):
        if entry.is_file():
            print(f"  {entry.name}  ({entry.stat().st_size:,} bytes)")

    if not sorted_platforms:
        print("\nWARNING: no platform artifacts found. Build first.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
