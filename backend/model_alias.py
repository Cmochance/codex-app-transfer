"""模型别名和多 provider 路由工具。"""

from __future__ import annotations

import copy
import re
from typing import Optional


MODEL_SLOTS = (
    {
        "key": "default",
        "legacy": ("default",),
    },
    {
        "key": "gpt_5_5",
        "legacy": (),
        "openai_id": "gpt-5.5",
    },
    {
        "key": "gpt_5_4",
        "legacy": (),
        "openai_id": "gpt-5.4",
    },
    {
        "key": "gpt_5_4_mini",
        "legacy": (),
        "openai_id": "gpt-5.4-mini",
    },
    {
        "key": "gpt_5_3_codex",
        "legacy": (),
        "openai_id": "gpt-5.3-codex",
    },
    {
        "key": "gpt_5_2",
        "legacy": (),
        "openai_id": "gpt-5.2",
    },
)
MODEL_ORDER = tuple(item["key"] for item in MODEL_SLOTS)
DEFAULT_MODEL_KEY = "default"
LEGACY_MODEL_KEYS = ("default",)

OPENAI_ID_TO_SLOT = {
    slot["openai_id"]: slot["key"]
    for slot in MODEL_SLOTS
    if slot.get("openai_id")
}

SLOT_TO_OPENAI_ID = {
    slot["key"]: slot["openai_id"]
    for slot in MODEL_SLOTS
    if slot.get("openai_id")
}


def empty_model_mappings() -> dict:
    return {key: "" for key in MODEL_ORDER}


def normalize_model_mappings(models: Optional[dict]) -> dict:
    """把旧四槽位和新多槽位统一成当前结构。"""
    normalized = empty_model_mappings()
    if not isinstance(models, dict):
        return normalized

    source = copy.deepcopy(models)
    default = str(source.get("default") or "").strip()
    normalized["default"] = default

    for slot in MODEL_SLOTS:
        key = slot["key"]
        if key == DEFAULT_MODEL_KEY:
            continue
        for candidate in (key, *slot["legacy"]):
            value = str(source.get(candidate) or "").strip()
            if value:
                normalized[key] = value
                break
    return normalized


def provider_model_ids(provider: Optional[dict]) -> list[str]:
    """返回 provider 暴露给 Codex CLI 的 OpenAI 模型 ID 列表。

    只有配置了映射（或 default 有映射可降级）的槽位才会返回对应的
    OpenAI 模型名，避免 Codex CLI 显示无法使用的模型。
    """
    if not provider:
        return []
    models = normalize_model_mappings(provider.get("models") or {})
    default_model = str(models.get("default") or "").strip()
    ordered: list[str] = []
    for slot in MODEL_SLOTS:
        key = slot["key"]
        if key == DEFAULT_MODEL_KEY:
            continue
        openai_id = slot.get("openai_id")
        if not openai_id:
            continue
        slot_model = str(models.get(key) or "").strip()
        if slot_model or default_model:
            ordered.append(openai_id)
    return ordered


def provider_slug(provider: dict) -> str:
    """生成用于 Claude 模型菜单的稳定 provider 前缀。"""
    source = str(provider.get("id") or provider.get("name") or "provider").lower()
    slug = re.sub(r"[^a-z0-9_-]+", "-", source).strip("-_")
    return slug[:56] or "provider"


def model_alias(provider: dict, model_id: str) -> str:
    """把 provider 和真实模型 ID 组合成菜单别名。"""
    return f"{provider_slug(provider)}/{model_id}"


_DEFAULT_1M_MODELS_BY_KIND: dict[str, tuple[str, ...]] = {
    # DeepSeek V4 全系（pro / flash）默认 1M 上下文。
    "deepseek": ("deepseek-v4-",),
    # 阿里云百炼 Qwen 3.6 系列（plus / flash / max-preview 等）默认 1M。
    "bailian": ("qwen3.6-",),
}


def _provider_default_supports_1m(provider: dict, model_id: str) -> bool:
    """已知 provider 在某些模型上默认提供 1M 上下文。"""
    try:
        from backend.provider_workarounds import detect_provider_kind  # 局部导入避免循环
    except ImportError:
        return False
    kind = detect_provider_kind(provider)
    prefixes = _DEFAULT_1M_MODELS_BY_KIND.get(kind)
    if not prefixes:
        return False
    lower = str(model_id or "").lower()
    return any(lower.startswith(prefix) for prefix in prefixes)


def model_supports_1m(provider: dict, model_id: str) -> bool:
    """判断模型是否应声明 supports1m。

    判定顺序：
    1. 模型名结尾的 ``[1m]`` 修饰后缀（旧版兼容，迁移期内仍承认）。
    2. ``provider.modelCapabilities[model].supports1m`` 用户手动声明。
    3. 已知 provider + 模型族默认 1M（见 ``_DEFAULT_1M_MODELS_BY_KIND``）。
    """
    if "[1m]" in str(model_id or "").lower():
        return True
    capabilities = provider.get("modelCapabilities") or {}
    if isinstance(capabilities, dict):
        model_capability = capabilities.get(model_id)
        if isinstance(model_capability, dict) and model_capability.get("supports1m") is True:
            return True
    return _provider_default_supports_1m(provider, model_id)


_OUTBOUND_MODEL_SUFFIX_RE = re.compile(r"\[[^\[\]]*\]\s*$")


def outbound_model_id(model_id: str) -> str:
    """剥掉模型名结尾的 ``[xxx]`` 内部修饰后缀，得到上游真实模型名。

    比如内部用 ``deepseek-v4-pro[1m]`` 标记 1M 上下文变体,
    但 DeepSeek 上游只认 ``deepseek-v4-pro``。
    """
    if not isinstance(model_id, str):
        return model_id
    return _OUTBOUND_MODEL_SUFFIX_RE.sub("", model_id).strip()


def provider_model_entries(provider: Optional[dict], use_alias: bool = False) -> list[dict]:
    """生成 inferenceModels / /v1/models 共用的模型条目。"""
    if not provider:
        return []
    entries: list[dict] = []
    provider_name = str(provider.get("name") or provider.get("id") or "Provider")
    for model_id in provider_model_ids(provider):
        name = model_alias(provider, model_id) if use_alias else model_id
        item = {
            "name": name,
            "displayName": f"{provider_name} / {model_id}" if use_alias else model_id,
            "sourceModel": model_id,
            "providerId": provider.get("id"),
        }
        if model_supports_1m(provider, model_id):
            item["supports1m"] = True
        entries.append(item)
    return entries


def all_provider_model_entries(providers: list[dict]) -> list[dict]:
    """生成所有 provider 的去重别名模型条目。"""
    entries: list[dict] = []
    seen: set[str] = set()
    for provider in providers:
        if not provider:
            continue
        for item in provider_model_entries(provider, use_alias=True):
            name = item["name"]
            if name in seen:
                continue
            seen.add(name)
            entries.append(item)
    return entries


def resolve_model_alias(providers: list[dict], requested_model: str) -> tuple[Optional[dict], str, bool]:
    """把 provider/model 别名解析为具体 provider 和真实模型 ID。"""
    requested = str(requested_model or "")
    if "/" not in requested:
        return None, requested, False
    slug, model_id = requested.split("/", 1)
    if not slug or not model_id:
        return None, requested, False
    for provider in providers:
        if provider_slug(provider) == slug:
            if model_id in provider_model_ids(provider):
                return provider, model_id, True
            # 允许用户手动写入未出现在映射里的模型 ID。
            return provider, model_id, True
    return None, requested, False


def resolve_requested_model_slot(requested_model: str) -> Optional[str]:
    """把 OpenAI 请求模型名解析为当前映射槽位。"""
    requested = str(requested_model or "").strip().lower()
    if not requested:
        return None
    return OPENAI_ID_TO_SLOT.get(requested)
