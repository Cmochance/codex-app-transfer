# -*- mode: python ; coding: utf-8 -*-
"""
CC Desktop Switch - PyInstaller 构建配置

使用方法：
    pyinstaller build.spec                    # 文件夹模式（启动快）
    set CCDS_ONEFILE=1 && pyinstaller build.spec  # 单文件 exe（便携）
    set CCDS_CONSOLE=1 && pyinstaller build.spec  # 调试时显示控制台

输出：
    dist/CC-Desktop-Switch/        ← 文件夹模式
    dist/CC-Desktop-Switch.exe     ← 单文件模式（加 --onefile）
"""

import os
from pathlib import Path
from PyInstaller.utils.hooks import collect_all, collect_data_files, collect_submodules, copy_metadata

ROOT = Path(SPECPATH)
FRONTEND = ROOT / "frontend"
ONEFILE = os.environ.get("CCDS_ONEFILE") == "1"
CONSOLE = os.environ.get("CCDS_CONSOLE") == "1"
ICON_FILE = FRONTEND / "assets" / "app-icon.png"
ICON = str(ICON_FILE) if ICON_FILE.exists() else None

WEBVIEW_HIDDENIMPORTS = collect_submodules("webview")
WEBVIEW_DATAS = collect_data_files("webview") + copy_metadata("pywebview")


def safe_collect_submodules(package):
    try:
        return collect_submodules(package)
    except Exception:
        return []


def safe_copy_metadata(package):
    try:
        return copy_metadata(package)
    except Exception:
        return []


PYSTRAY_HIDDENIMPORTS = safe_collect_submodules("pystray")
PYSTRAY_DATAS = safe_copy_metadata("pystray") + safe_copy_metadata("Pillow")

# PyInstaller 的运行时 hook pyi_rth_pkgres.py 启动时一定会 import pkg_resources。
# 新版 setuptools 把 jaraco.text / more_itertools / platformdirs / zipp / packaging
# 等 vendored 到 pkg_resources/_vendor/ 下，必须整体收集，否则 Windows 启动会报
# ModuleNotFoundError: No module named 'jaraco.text'。
PKG_RES_DATAS, PKG_RES_BINARIES, PKG_RES_HIDDENIMPORTS = collect_all("pkg_resources")

block_cipher = None

a = Analysis(
    ["main.py"],
    pathex=[str(ROOT)],
    binaries=[] + PKG_RES_BINARIES,
    datas=[
        (str(FRONTEND), "frontend"),
        (str(ROOT / "LICENSE.txt"), "."),
    ] + WEBVIEW_DATAS + PYSTRAY_DATAS + PKG_RES_DATAS,
    hiddenimports=[
        "backend", "backend.main", "backend.config",
        "backend.registry", "backend.proxy", "backend.update",
        "backend.model_alias", "backend.provider_tools",
        # Adapter modules — must be in hiddenimports for PyInstaller to bundle them.
        "backend.responses_adapter",
        "backend.chat_responses_adapter",
        "backend.streaming_adapter",
        "backend.openai_adapter",
        "backend.base_adapter",
        "backend.adapter_utils",
        "backend.api_adapters",
        "backend.provider_workarounds",
        "backend.response_id_codec",
        "backend.session_cache",
        "backend.deployment_affinity",
    ] + WEBVIEW_HIDDENIMPORTS + PYSTRAY_HIDDENIMPORTS + PKG_RES_HIDDENIMPORTS,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        "tkinter", "matplotlib", "numpy", "pandas",
        "scipy", "pip",
        "cryptography", "zmq", "notebook", "IPython",
        "PyQt5", "PySide2", "PySide6",
    ],
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

if ONEFILE:
    exe = EXE(
        pyz,
        a.scripts,
        a.binaries,
        a.zipfiles,
        a.datas,
        [],
        name="Codex-App-Transfer",
        debug=False,
        bootloader_ignore_signals=False,
        strip=False,
        upx=True,
        upx_exclude=[],
        runtime_tmpdir=None,
        console=CONSOLE,
        disable_windowed_traceback=False,
        argv_emulation=False,
        target_arch=None,
        codesign_identity=None,
        entitlements_file=None,
        icon=ICON,
    )
else:
    exe = EXE(
        pyz,
        a.scripts,
        [],
        exclude_binaries=True,
        name="Codex-App-Transfer",
        debug=False,
        bootloader_ignore_signals=False,
        strip=False,
        upx=True,
        upx_exclude=[],
        runtime_tmpdir=None,
        console=CONSOLE,
        disable_windowed_traceback=False,
        argv_emulation=False,
        target_arch=None,
        codesign_identity=None,
        entitlements_file=None,
        icon=ICON,
    )
    COLLECT(
        exe,
        a.binaries,
        a.zipfiles,
        a.datas,
        strip=False,
        upx=True,
        upx_exclude=[],
        name="Codex-App-Transfer",
    )
    app = BUNDLE(
        exe,
        a.binaries,
        a.zipfiles,
        a.datas,
        name="Codex App Transfer.app",
        icon=ICON,
        bundle_identifier="io.github.lonr6.codexappswitch",
    )
