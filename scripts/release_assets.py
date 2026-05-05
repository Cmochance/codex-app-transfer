#!/usr/bin/env python3
"""Sign release artifacts (sha256 + .sig) and emit latest.json.

Phase 2 起读取 GitHub Actions release.yml 已 rename 完的产物
(dist-incoming/Codex-App-Transfer-v<V>-<plat>.<ext>), 不再调 PyInstaller /
NSIS / 老 Docker 路径自己组装产物。

支持 incremental, per-platform 调用: --include windows 只刷 release/ 中
Windows-* 文件, 其他平台不动。latest.json 总是按 release/ 目录里的
当前所有已签 asset 重新生成。

签名: RSA-3072 PKCS#1 v1.5 + SHA-256, key 在 PEM。Verifier:

    python -c "
    import base64, sys
    from pathlib import Path
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import padding
    pub = serialization.load_pem_public_key(
        Path('release/Codex-App-Transfer-release-public.pem').read_bytes())
    asset = sys.argv[1]
    sig = base64.b64decode(Path(asset + '.sig').read_text())
    pub.verify(sig, Path(asset).read_bytes(), padding.PKCS1v15(), hashes.SHA256())
    print('OK')
    "
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

# Phase 2 后的 PLATFORM_PATTERNS:
# - macOS:  .pkg 退役, 只保 .dmg (Tauri bundler 不直出 .pkg)
# - Linux:  .tar.gz / 无后缀 onefile 退役, 替换为 .deb (推荐) + .AppImage (兜底)
# - Windows: Portable.zip / x64.exe (PyInstaller onefile) 退役,
#            替换为 -Setup.exe (NSIS) + .msi (WiX)
PLATFORM_PATTERNS: dict[str, list[tuple[str, str]]] = {
    "windows": [
        (r"-Windows-x64-Setup\.exe$", "windows-x64"),
        (r"-Windows-x64\.msi$", "windows-x64"),
    ],
    "macos": [
        (r"-macOS-arm64\.dmg$", "macos-arm64"),
        (r"-macOS-x64\.dmg$", "macos-x64"),
    ],
    "linux": [
        (r"-Linux-x86_64\.deb$", "linux-x86_64"),
        (r"-Linux-x86_64\.AppImage$", "linux-x86_64"),
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


def collect_from_incoming(
    incoming_dir: Path,
    release_dir: Path,
    version: str,
    platform_name: str,
) -> list[Path]:
    """Copy already-renamed artifacts from incoming_dir → release_dir for one platform.

    release.yml 的 build job 把 Tauri 默认产物 cp 到 staging/, upload-artifact
    后 release-bundle job 用 download-artifact 落到 dist-incoming/。文件名
    在 build job rename 阶段已经是 ASSET_PREFIX-vV-PLAT.EXT, 这里只做
    "按 platform 过滤 + cp 到 release/"。
    """
    if not incoming_dir.is_dir():
        return []
    out: list[Path] = []
    for entry in sorted(incoming_dir.iterdir()):
        if not entry.is_file():
            continue
        if not entry.name.startswith(f"{ASSET_PREFIX}-v{version}-"):
            continue
        for pattern, _platform_key in PLATFORM_PATTERNS[platform_name]:
            if re.search(pattern, entry.name):
                target = release_dir / entry.name
                if target.exists():
                    target.unlink()
                shutil.copyfile(entry, target)
                out.append(target)
                break
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
        "--incoming-dir",
        default="dist-incoming",
        help="Directory containing already-renamed artifacts from release.yml "
        "build job (default: dist-incoming).",
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
    incoming_dir = root / args.incoming_dir
    key_dir = root / ".release-signing"

    release_dir.mkdir(parents=True, exist_ok=True)
    private_key = get_or_create_key(key_dir, release_dir)

    include = set(args.include)
    platforms: dict[str, list[dict]] = {}

    # 处理 --include 平台: 清 release/ 中该平台旧文件 → 从 incoming_dir cp →
    # 签名 + 索引到 latest.json。
    for platform_name in ("windows", "macos", "linux"):
        if platform_name not in include:
            continue
        clean_platform(release_dir, platform_name)
        files = collect_from_incoming(
            incoming_dir, release_dir, args.version, platform_name
        )
        for f in files:
            asset = sign_and_index(f, private_key, args.repo, args.version)
            for pattern, platform_key in PLATFORM_PATTERNS[platform_name]:
                if re.search(pattern, f.name):
                    platforms.setdefault(platform_key, []).append(asset)
                    break

    # 没在 --include 里的平台: 从 release/ 上次跑已签的文件读出, 让 latest.json
    # 仍然完整描述全部 3 平台 (incremental release 场景)。
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
