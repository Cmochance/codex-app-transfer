"""配置管理 - JSON 配置文件读写"""

import json
import os
import secrets
import shutil
import copy
import tempfile
from datetime import datetime
from typing import Optional

from backend.model_alias import normalize_model_mappings

CONFIG_DIR = os.path.expanduser("~/.codex-app-transfer")
CONFIG_FILE = os.path.join(CONFIG_DIR, "config.json")
BACKUP_DIR = os.path.join(CONFIG_DIR, "backups")
LIBRARY_DIR = os.path.join(CONFIG_DIR, "configLibrary")
OLD_CONFIG_DIR = os.path.expanduser("~/.cc-desktop-switch")
DEFAULT_UPDATE_URL = "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json"

APP_VERSION = "1.0.2"

DEFAULT_CONFIG = {
    "version": APP_VERSION,
    "activeProvider": None,
    "gatewayApiKey": None,
    "providers": [],
    "settings": {
        "theme": "default",
        "language": "zh",
        "proxyPort": 18080,
        "adminPort": 18081,
        "autoStart": False,
        "autoApplyOnStart": True,
        "exposeAllProviderModels": False,
        "restoreCodexOnExit": True,
        "updateUrl": DEFAULT_UPDATE_URL,
    },
}

BUILTIN_PRESETS = [
    {
        "id": "deepseek",
        "name": "DeepSeek",
        "baseUrl": "https://api.deepseek.com/v1",
        "docsUrl": "https://api-docs.deepseek.com/",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "models": {
            "sonnet": "deepseek-v4-pro",
            "haiku": "deepseek-v4-flash",
            "opus": "deepseek-v4-pro",
            "default": "deepseek-v4-pro",
        },
        "modelOptions": {
            "deepseek_1m": {
                "label": "解锁 1M 上下文",
                "description": "用于 Claude Code/长上下文场景。开启后 Sonnet、Opus 和默认模型使用 deepseek-v4-pro[1m]。",
                "models": {
                    "sonnet": "deepseek-v4-pro[1m]",
                    "haiku": "deepseek-v4-flash",
                    "opus": "deepseek-v4-pro[1m]",
                    "default": "deepseek-v4-pro[1m]",
                },
                "modelCapabilities": {
                    "deepseek-v4-pro[1m]": {"supports1m": True},
                },
            }
        },
        "requestOptions": {},
        "requestOptionPresets": {
            "deepseek_max_effort": {
                "label": "DeepSeek Max 思维",
                "description": "Low：更快更省，适合简单任务。\nMedium：速度和效果平衡，适合日常使用。\nHigh：更认真思考，适合复杂代码和排错。\n勾选后：本工具会按 DeepSeek Max 转发；未勾选则使用 Claude 当前默认配置。",
                "requestOptions": {
                    "chat": {
                        "thinking": {"type": "enabled"},
                        "reasoning_effort": "max",
                    }
                },
            }
        },
        "extraHeaders": {},
        "isBuiltin": True,
    },
    {
        "id": "kimi",
        "name": "Kimi (月之暗面)",
        "baseUrl": "https://api.moonshot.cn/v1",
        "docsUrl": "https://platform.moonshot.cn/docs",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "models": {
            "sonnet": "kimi-k2.6",
            "haiku": "kimi-k2.6",
            "opus": "kimi-k2.6",
            "default": "kimi-k2.6",
        },
        "isBuiltin": True,
    },
    {
        "id": "kimi-code",
        "name": "Kimi Code",
        "baseUrl": "https://api.kimi.com/coding/v1",
        "docsUrl": "https://platform.moonshot.cn/docs",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "extraHeaders": {
            "User-Agent": "KimiCLI/1.40.0"
        },
        "models": {
            "sonnet": "kimi-for-coding",
            "haiku": "kimi-for-coding",
            "opus": "kimi-for-coding",
            "default": "kimi-for-coding",
        },
        "isBuiltin": True,
    },
    {
        "id": "xiaomi-mimo-payg",
        "name": "Xiaomi MiMo (Pay for Token)",
        "baseUrl": "https://api.xiaomimimo.com/v1",
        "docsUrl": "https://www.mimo-v2.com/zh/docs/quick-start/first-api-call",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "models": {
            "sonnet": "",
            "haiku": "",
            "opus": "",
            "default": "mimo-v2.5-pro",
        },
        "isBuiltin": True,
    },
    {
        "id": "xiaomi-mimo-token-plan",
        "name": "Xiaomi MiMo (Token Plan)",
        "baseUrl": "https://token-plan-cn.xiaomimimo.com/v1",
        "docsUrl": "https://www.mimo-v2.com/zh/docs/quick-start/first-api-call",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "baseUrlOptions": [
            {
                "label": "中国集群",
                "value": "https://token-plan-cn.xiaomimimo.com/v1",
            },
            {
                "label": "新加坡集群",
                "value": "https://token-plan-sgp.xiaomimimo.com/v1",
            },
            {
                "label": "欧洲集群",
                "value": "https://token-plan-ams.xiaomimimo.com/v1",
            },
        ],
        "baseUrlHint": "请使用账号所属地区的 Base URL，若不清楚请访问 https://platform.xiaomimimo.com/console/plan-manage 获取专属Base URL。",
        "models": {
            "sonnet": "",
            "haiku": "",
            "opus": "",
            "default": "mimo-v2.5-pro",
        },
        "isBuiltin": True,
    },
    {
        "id": "zhipu",
        "name": "智谱 GLM",
        "baseUrl": "https://open.bigmodel.cn/api/paas/v4",
        "docsUrl": "https://docs.bigmodel.cn/cn/guide/develop/openai/introduction",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "models": {
            "sonnet": "glm-5.1",
            "haiku": "glm-4.7",
            "opus": "glm-5.1",
            "default": "glm-5.1",
        },
        "isBuiltin": True,
    },
    {
        "id": "bailian",
        "name": "阿里云百炼",
        "baseUrl": "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "docsUrl": "https://help.aliyun.com/zh/model-studio/compatibility-of-openai-with-dashscope",
        "authScheme": "bearer",
        "apiFormat": "openai_chat",
        "models": {
            "sonnet": "qwen3.6-plus",
            "haiku": "qwen3.6-flash",
            "opus": "qwen3.6-max-preview",
            "default": "qwen3.6-plus",
        },
        "modelOptions": {
            "qwen_1m": {
                "label": "开启千问 1M 上下文",
                "description": "阿里云文档确认 qwen3.6-plus / qwen3.6-flash 支持 1M。勾选后会把 1M 能力写入 Codex CLI 配置；不勾选则按普通上下文显示。",
                "modelCapabilities": {
                    "qwen3.6-plus": {"supports1m": True},
                    "qwen3.6-flash": {"supports1m": True},
                },
            }
        },
        "modelCapabilities": {},
        "requestOptions": {},
        "isBuiltin": True,
    },
]


def ensure_config_dir():
    """确保配置目录存在"""
    os.makedirs(CONFIG_DIR, exist_ok=True)


def ensure_backup_dir():
    """确保配置备份目录存在"""
    ensure_config_dir()
    os.makedirs(BACKUP_DIR, exist_ok=True)


def load_config() -> dict:
    """加载配置文件；若新版不存在但旧版存在，自动迁移。"""
    ensure_config_dir()

    # 自动迁移旧配置
    if not os.path.exists(CONFIG_FILE) and os.path.exists(os.path.join(OLD_CONFIG_DIR, "config.json")):
        try:
            old_file = os.path.join(OLD_CONFIG_DIR, "config.json")
            with open(old_file, "r", encoding="utf-8") as f:
                old_config = json.load(f)
            # 更新 version 和 gatewayApiKey 前缀
            old_config["version"] = DEFAULT_CONFIG["version"]
            old_key = old_config.get("gatewayApiKey", "")
            if old_key.startswith("ccds_"):
                old_config["gatewayApiKey"] = "cas_" + old_key[5:]
            # apiFormat 向后兼容：旧 anthropic 映射为 responses
            for provider in old_config.get("providers", []):
                fmt = (provider.get("apiFormat") or "").lower()
                if fmt in ("anthropic", "claude", "messages"):
                    provider["apiFormat"] = "responses"
            with open(CONFIG_FILE, "w", encoding="utf-8") as f:
                json.dump(old_config, f, ensure_ascii=False, indent=2)
        except Exception:
            pass

    if not os.path.exists(CONFIG_FILE):
        return copy.deepcopy(DEFAULT_CONFIG)
    try:
        with open(CONFIG_FILE, "r", encoding="utf-8") as f:
            config = json.load(f)
    except (json.JSONDecodeError, IOError):
        return copy.deepcopy(DEFAULT_CONFIG)

    # 自动把旧版 monolithic providers 迁移到 Library
    providers = config.get("providers", [])
    if providers and not os.path.isdir(LIBRARY_DIR):
        try:
            for provider in providers:
                provider_id = provider.get("id")
                if provider_id:
                    write_library_provider(provider_id, provider)
            meta = {"providerIds": [p["id"] for p in providers]}
            if config.get("activeProvider"):
                meta["appliedId"] = config["activeProvider"]
            write_library_meta(meta)
        except Exception:
            pass

    return config


def save_config(config: dict):
    """保存配置文件"""
    ensure_config_dir()
    # 原子写入：先写临时文件，再重命名
    tmp_file = CONFIG_FILE + ".tmp"
    with open(tmp_file, "w", encoding="utf-8") as f:
        json.dump(config, f, ensure_ascii=False, indent=2)
    shutil.move(tmp_file, CONFIG_FILE)


def _sync_apiformat_from_builtin(provider: dict) -> None:
    """若 provider 的 baseUrl 与某个内置预设完全一致(包含 baseUrlOptions),
    把 apiFormat 同步为预设值。

    背景:v1.0.0 时 Kimi 默认 apiFormat 是 "responses",v1.0.1 起改成 "openai_chat"。
    从 1.0.0 升级来的用户 saved Kimi provider 仍停在旧 "responses",导致编辑页
    默认勾错 + 代理走错协议 + 测速失败。预设变更不会自动迁移已保存数据,本函数
    在每次 _normalize_provider 调用时按 baseUrl 同步,保证读取出来的 provider
    永远跟当前预设的 apiFormat 一致。

    用户自定义 URL(跟任何预设都不匹配)的 provider 不会被改动。
    """
    base_url = str(provider.get("baseUrl") or "").strip().rstrip("/")
    if not base_url:
        return
    for preset in BUILTIN_PRESETS:
        candidates = {str(preset.get("baseUrl") or "").rstrip("/")}
        for opt in preset.get("baseUrlOptions") or []:
            if isinstance(opt, dict) and opt.get("value"):
                candidates.add(str(opt["value"]).rstrip("/"))
        if base_url in candidates:
            preset_format = preset.get("apiFormat")
            if preset_format and provider.get("apiFormat") != preset_format:
                provider["apiFormat"] = preset_format
            return


def _normalize_provider(provider: dict) -> dict:
    """补齐 provider 必要字段，导入旧配置时保持兼容。"""
    normalized = dict(provider)
    provider_id = str(normalized.get("id") or "")
    safe_id = "".join(ch for ch in provider_id if ch.isalnum() or ch in {"-", "_"})[:64]
    normalized["id"] = safe_id or secrets.token_hex(4)
    normalized.setdefault("name", "Unnamed Provider")
    normalized.setdefault("baseUrl", "")
    normalized.setdefault("authScheme", "bearer")
    normalized.setdefault("apiFormat", "responses")
    normalized.setdefault("apiKey", "")
    normalized.setdefault("extraHeaders", {})
    normalized.setdefault("modelCapabilities", {})
    normalized.setdefault("requestOptions", {})
    normalized.setdefault("isBuiltin", False)
    normalized.setdefault("sortIndex", 0)
    normalized["models"] = normalize_model_mappings(normalized.get("models"))
    # 自动跟内置预设的 apiFormat 对齐(仅当 baseUrl 完全匹配某预设时)
    _sync_apiformat_from_builtin(normalized)
    return normalized


def normalize_config(config: dict) -> dict:
    """把外部导入的配置整理成当前版本可读取的结构。"""
    if not isinstance(config, dict):
        raise ValueError("配置文件必须是 JSON 对象")

    source = config.get("config") if isinstance(config.get("config"), dict) else config
    normalized = copy.deepcopy(DEFAULT_CONFIG)
    normalized.update({k: v for k, v in source.items() if k in normalized})
    normalized["version"] = source.get("version", DEFAULT_CONFIG["version"])

    settings = dict(DEFAULT_CONFIG["settings"])
    imported_settings = source.get("settings", {})
    if isinstance(imported_settings, dict):
        settings.update(imported_settings)
    normalized["settings"] = settings

    providers = source.get("providers", [])
    if not isinstance(providers, list):
        raise ValueError("providers 必须是数组")
    normalized_providers = []
    seen_ids = set()
    for provider in providers:
        if not isinstance(provider, dict):
            continue
        normalized_provider = _normalize_provider(provider)
        if normalized_provider["id"] in seen_ids:
            normalized_provider["id"] = f"{normalized_provider['id']}-{secrets.token_hex(2)}"
        seen_ids.add(normalized_provider["id"])
        normalized_providers.append(normalized_provider)
    normalized["providers"] = normalized_providers

    provider_ids = {p["id"] for p in normalized["providers"]}
    active_provider = source.get("activeProvider")
    if active_provider in provider_ids:
        normalized["activeProvider"] = active_provider
    else:
        normalized["activeProvider"] = normalized["providers"][0]["id"] if normalized["providers"] else None

    if source.get("gatewayApiKey"):
        normalized["gatewayApiKey"] = source["gatewayApiKey"]

    return normalized


def create_backup(reason: str = "manual") -> dict:
    """备份当前配置文件，返回备份文件元数据。"""
    ensure_backup_dir()
    if not os.path.exists(CONFIG_FILE):
        save_config(load_config())

    safe_reason = "".join(ch for ch in str(reason or "manual").lower() if ch.isalnum() or ch in {"-", "_"})[:32]
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    filename = f"config-{timestamp}-{safe_reason or 'manual'}-{secrets.token_hex(2)}.json"
    target = os.path.join(BACKUP_DIR, filename)
    shutil.copy2(CONFIG_FILE, target)
    stat = os.stat(target)
    return {
        "name": filename,
        "size": stat.st_size,
        "createdAt": datetime.fromtimestamp(stat.st_mtime).isoformat(timespec="seconds"),
    }


def list_backups() -> list:
    """列出配置备份。"""
    ensure_backup_dir()
    backups = []
    for name in os.listdir(BACKUP_DIR):
        if not name.endswith(".json"):
            continue
        path = os.path.join(BACKUP_DIR, name)
        if not os.path.isfile(path):
            continue
        stat = os.stat(path)
        backups.append({
            "name": name,
            "size": stat.st_size,
            "createdAt": datetime.fromtimestamp(stat.st_mtime).isoformat(timespec="seconds"),
        })
    return sorted(backups, key=lambda item: item["createdAt"], reverse=True)


def export_config() -> dict:
    """导出完整配置。包含 API Key，仅供用户本机保存。"""
    return {
        "format": "codex-app-transfer.config",
        "exportedAt": datetime.now().isoformat(timespec="seconds"),
        "config": load_config(),
    }


def import_config(data: dict) -> dict:
    """导入配置。导入前自动备份当前配置。"""
    backup = create_backup("before-import")
    normalized = normalize_config(data)
    save_config(normalized)
    return {"config": normalized, "backup": backup}


def get_or_create_gateway_api_key() -> str:
    """获取本地 gateway 认证密钥，没有则生成一个。

    这个密钥写入 Codex CLI 的环境变量配置，用于满足 gateway 模式的
    必填凭据要求。它不是上游提供商 API Key。
    """
    config = load_config()
    key = config.get("gatewayApiKey")
    if not key:
        key = "cas_" + secrets.token_urlsafe(32)
        config["gatewayApiKey"] = key
        save_config(config)
    return key


def get_gateway_api_key() -> Optional[str]:
    """读取本地 gateway 认证密钥，不存在时不自动创建。"""
    return load_config().get("gatewayApiKey")


def get_providers() -> list:
    """获取所有提供商列表（Library 优先）"""
    migrate_legacy_providers()
    return get_merged_providers()


def get_provider(provider_id: str) -> Optional[dict]:
    """按 ID 获取提供商（Library 优先）"""
    migrate_legacy_providers()
    return get_merged_provider(provider_id)


def get_active_provider() -> Optional[dict]:
    """获取当前激活的提供商（Library 优先）"""
    migrate_legacy_providers()
    config = load_config()
    active_id = config.get("activeProvider")
    if not active_id:
        providers = get_merged_providers()
        return providers[0] if providers else None
    return get_merged_provider(active_id)


def add_provider(provider: dict) -> dict:
    """添加提供商"""
    config = load_config()
    providers = config.get("providers", [])

    # 生成唯一 ID
    import uuid
    provider = _normalize_provider(provider)
    existing_ids = {p.get("id") for p in providers}
    candidate_id = provider.get("id") or str(uuid.uuid4())[:8]
    while candidate_id in existing_ids:
        candidate_id = f"{provider.get('id') or 'provider'}-{secrets.token_hex(2)}"
    provider["id"] = candidate_id
    provider["sortIndex"] = len(providers)

    providers.append(provider)
    config["providers"] = providers

    # 如果是第一个提供商，自动设为默认
    if len(providers) == 1:
        config["activeProvider"] = provider["id"]

    save_config(config)
    return provider


def update_provider(provider_id: str, data: dict) -> Optional[dict]:
    """更新提供商"""
    config = load_config()
    for i, p in enumerate(config.get("providers", [])):
        if p["id"] == provider_id:
            updated = dict(p)
            updated.update(data)
            updated["id"] = provider_id
            is_builtin = p.get("isBuiltin", False)
            updated["isBuiltin"] = is_builtin

            # 内置 provider 的 baseUrl 不允许用户修改
            if is_builtin:
                updated["baseUrl"] = p.get("baseUrl", "")

            # 编辑表单中 API Key 留空表示“不修改”，避免误清空已保存密钥。
            if not data.get("apiKey"):
                updated["apiKey"] = p.get("apiKey", "")

            # preset 的额外认证头也要保留，例如 DeepSeek 的 x-api-key。
            if "extraHeaders" not in data or data.get("extraHeaders") in (None, {}):
                updated["extraHeaders"] = p.get("extraHeaders", {})

            if "modelCapabilities" not in data:
                updated["modelCapabilities"] = p.get("modelCapabilities", {})

            if "requestOptions" not in data:
                updated["requestOptions"] = p.get("requestOptions", {})

            if "models" in data and isinstance(data["models"], dict):
                merged_models = dict(p.get("models", {}))
                merged_models.update(data["models"])
                updated["models"] = normalize_model_mappings(merged_models)

            config["providers"][i] = updated
            save_config(config)
            return updated
    return None


def delete_provider(provider_id: str) -> bool:
    """删除提供商"""
    config = load_config()
    original_len = len(config.get("providers", []))
    config["providers"] = [p for p in config.get("providers", []) if p["id"] != provider_id]

    if len(config["providers"]) == original_len:
        return False

    # 如果删除的是当前激活的，切换到第一个可用的
    if config.get("activeProvider") == provider_id:
        config["activeProvider"] = config["providers"][0]["id"] if config["providers"] else None

    for index, provider in enumerate(config["providers"]):
        provider["sortIndex"] = index

    save_config(config)
    return True


def set_active_provider(provider_id: str) -> bool:
    """设置默认提供商"""
    config = load_config()
    for p in config.get("providers", []):
        if p["id"] == provider_id:
            config["activeProvider"] = provider_id
            save_config(config)
            return True
    return False


def update_models(provider_id: str, models: dict) -> bool:
    """更新模型映射"""
    config = load_config()
    for p in config.get("providers", []):
        if p["id"] == provider_id:
            p["models"] = normalize_model_mappings(models)
            save_config(config)
            return True
    return False


def reorder_providers(provider_ids: list[str]) -> bool:
    """按照前端拖动后的 ID 顺序保存 providers。"""
    config = load_config()
    providers = config.get("providers", [])
    by_id = {provider.get("id"): provider for provider in providers}
    ordered = []
    seen = set()
    for provider_id in provider_ids:
        provider = by_id.get(provider_id)
        if provider and provider_id not in seen:
            ordered.append(provider)
            seen.add(provider_id)
    ordered.extend(provider for provider in providers if provider.get("id") not in seen)
    if len(ordered) != len(providers):
        return False
    for index, provider in enumerate(ordered):
        provider["sortIndex"] = index
    config["providers"] = ordered
    save_config(config)
    return True


def get_settings() -> dict:
    """获取设置"""
    config = load_config()
    settings = dict(DEFAULT_CONFIG["settings"])
    settings.update(config.get("settings", {}))
    # 统一模型菜单暂不开放，避免不同厂商能力混在一起导致 1M / 思维深度失效。
    settings["exposeAllProviderModels"] = False
    if not settings.get("updateUrl"):
        settings["updateUrl"] = DEFAULT_UPDATE_URL
    return settings


def update_settings(settings: dict) -> dict:
    """更新设置"""
    config = load_config()
    current = dict(DEFAULT_CONFIG["settings"])
    current.update(config.get("settings", {}))
    current.update(settings)
    current["exposeAllProviderModels"] = False
    if not current.get("updateUrl"):
        current["updateUrl"] = DEFAULT_UPDATE_URL
    config["settings"] = current
    save_config(config)
    return current


def get_presets() -> list:
    """获取内置预设列表"""
    return BUILTIN_PRESETS


# ── Library (configLibrary) ──

def _library_dir() -> str:
    return LIBRARY_DIR


def _library_entry_path(provider_id: str) -> str:
    return os.path.join(LIBRARY_DIR, f"{provider_id}.json")


def _library_meta_path() -> str:
    return os.path.join(LIBRARY_DIR, "_meta.json")


def _read_json_file(path: str) -> tuple[bool, dict, str]:
    if not os.path.exists(path):
        return True, {}, ""
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        if not isinstance(data, dict):
            return False, {}, "JSON root is not an object"
        return True, data, ""
    except Exception as exc:
        return False, {}, str(exc)


def _write_json_file(path: str, data: dict) -> tuple[bool, str]:
    directory = os.path.dirname(path)
    temp_path = ""
    try:
        os.makedirs(directory, exist_ok=True)
        fd, temp_path = tempfile.mkstemp(prefix=".cas-", suffix=".json", dir=directory)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
            f.write("\n")
        os.replace(temp_path, path)
        return True, ""
    except Exception as exc:
        if temp_path:
            try:
                os.remove(temp_path)
            except OSError:
                pass
        return False, str(exc)


def _list_library_provider_ids() -> list[str]:
    if not os.path.isdir(LIBRARY_DIR):
        return []
    ids = []
    for name in sorted(os.listdir(LIBRARY_DIR)):
        if name.endswith(".json") and name != "_meta.json":
            ids.append(name[:-5])
    return ids


def read_library_provider(provider_id: str) -> Optional[dict]:
    ok, data, _ = _read_json_file(_library_entry_path(provider_id))
    if not ok or not data:
        return None
    normalized = _normalize_provider(data)
    normalized["id"] = provider_id
    return normalized


def write_library_provider(provider_id: str, data: dict):
    normalized = _normalize_provider(data)
    normalized["id"] = provider_id
    _write_json_file(_library_entry_path(provider_id), normalized)


def clear_library_provider(provider_id: str):
    path = _library_entry_path(provider_id)
    if os.path.exists(path):
        os.remove(path)


def read_library_meta() -> dict:
    ok, data, _ = _read_json_file(_library_meta_path())
    return data if ok and isinstance(data, dict) else {}


def write_library_meta(data: dict):
    _write_json_file(_library_meta_path(), data)


def get_library_providers() -> list[dict]:
    providers = []
    for provider_id in _list_library_provider_ids():
        provider = read_library_provider(provider_id)
        if provider:
            providers.append(provider)
    return providers


def get_merged_provider(provider_id: str) -> Optional[dict]:
    library = read_library_provider(provider_id)
    config = get_provider_from_config(provider_id)
    if library and config:
        merged = dict(config)
        merged.update(library)
        if not library.get("apiKey"):
            merged["apiKey"] = config.get("apiKey", "")
        return merged
    return library or config


def get_merged_providers() -> list[dict]:
    config = load_config()
    config_providers = {p["id"]: p for p in config.get("providers", [])}
    library_ids = _list_library_provider_ids()
    all_ids = list(dict.fromkeys(list(config_providers.keys()) + library_ids))
    providers = []
    for provider_id in all_ids:
        provider = get_merged_provider(provider_id)
        if provider:
            providers.append(provider)
    providers.sort(key=lambda p: p.get("sortIndex", 0))
    return providers


def get_provider_from_config(provider_id: str) -> Optional[dict]:
    for provider in load_config().get("providers", []):
        if provider.get("id") == provider_id:
            return provider
    return None


def migrate_legacy_providers():
    config = load_config()
    providers = config.get("providers", [])
    if not providers:
        return
    migrated = False
    for provider in providers:
        provider_id = provider.get("id")
        if not provider_id:
            continue
        if not os.path.exists(_library_entry_path(provider_id)):
            write_library_provider(provider_id, provider)
            migrated = True
    if migrated:
        meta = read_library_meta()
        if not meta.get("providerIds"):
            meta["providerIds"] = [p["id"] for p in providers]
        if not meta.get("appliedId") and config.get("activeProvider"):
            meta["appliedId"] = config["activeProvider"]
        write_library_meta(meta)
