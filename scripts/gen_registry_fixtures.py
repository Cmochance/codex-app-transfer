#!/usr/bin/env python3
"""生成 Python 侧的 registry fixture(供 Rust crate 做字节级对照测试).

执行::

    . .venv/bin/activate && python scripts/gen_registry_fixtures.py

这会把当前 Python 实现产出的 JSON 写到
``tests/replay/fixtures/registry/`` 下,Rust 的 ``crates/registry`` 集成测试
``python_compat.rs`` 会读这些文件,确认 ``serde_json`` 双向通过后产出的
字节序列与 Python 一致.

不依赖真实 API key、不读用户本机配置;输入完全合成.
"""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path

# 让脚本在仓库根执行时能 import backend.*
ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from backend.config import BUILTIN_PRESETS, DEFAULT_CONFIG  # noqa: E402
from backend.model_alias import normalize_model_mappings  # noqa: E402


FIXTURE_DIR = ROOT / "tests" / "replay" / "fixtures" / "registry"


def dump_main_config(path: Path, payload: dict) -> None:
    """与 backend/config.py save_config 等价:无末尾换行."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=2)


def dump_library_entry(path: Path, payload: dict) -> None:
    """与 backend/config.py _write_json_file 等价:末尾带 `\\n`."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=2)
        f.write("\n")


def main() -> None:
    FIXTURE_DIR.mkdir(parents=True, exist_ok=True)

    # 1. DEFAULT_CONFIG 全空 —— 一台机器初次启动时落地的样子
    dump_main_config(FIXTURE_DIR / "default_config.json", DEFAULT_CONFIG)

    # 2. DEFAULT_CONFIG + 1 个合成 provider(覆盖必备字段,含中文 name).
    #    用合成数据,不依赖任何真实 token / 私有 baseUrl.
    config_with_provider = copy.deepcopy(DEFAULT_CONFIG)
    config_with_provider["activeProvider"] = "fixture-provider"
    config_with_provider["gatewayApiKey"] = "cas_<redacted>"
    config_with_provider["providers"] = [
        {
            "id": "fixture-provider",
            "name": "Fixture · 合成 Provider",
            "baseUrl": "https://fixture.invalid/v1",
            "authScheme": "bearer",
            "apiFormat": "openai_chat",
            "apiKey": "<redacted>",
            "models": normalize_model_mappings(
                {
                    "default": "fixture-default",
                    "gpt_5_5": "fixture-gpt-5.5",
                }
            ),
            "extraHeaders": {},
            "modelCapabilities": {},
            "requestOptions": {},
            "isBuiltin": False,
            "sortIndex": 0,
        }
    ]
    dump_main_config(FIXTURE_DIR / "with_provider.json", config_with_provider)

    # 3. BUILTIN_PRESETS —— Rust 侧应该 1:1 复刻这份数据
    dump_main_config(FIXTURE_DIR / "builtin_presets.json", BUILTIN_PRESETS)

    # 4. 一份 Library 条目(末尾带换行)
    library_entry = {
        "id": "library-fixture",
        "name": "Library Fixture",
        "baseUrl": "https://library.invalid/v1",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "apiKey": "<redacted>",
        "models": normalize_model_mappings({"default": "library-default"}),
        "extraHeaders": {},
        "modelCapabilities": {},
        "requestOptions": {},
        "isBuiltin": False,
        "sortIndex": 0,
    }
    dump_library_entry(FIXTURE_DIR / "library_entry.json", library_entry)

    print(f"wrote 4 fixtures into {FIXTURE_DIR.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
