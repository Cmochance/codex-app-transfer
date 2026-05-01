"""Codex CLI 环境变量配置管理（保留旧注册表函数供参考）"""

import base64
import datetime as _datetime
import json
import os
import shutil
import subprocess
import sys
import tempfile
from typing import Optional

from backend.model_alias import all_provider_model_entries, provider_model_entries

REGISTRY_PATH = r"SOFTWARE\Policies\Claude"
CCDS_MARKER = "cas_managed"

# 预期的配置项（名称 → 默认值, 值类型）
DESKTOP_CONFIG = {
    "inferenceProvider": ("gateway", str),
    "inferenceGatewayApiKey": ("", str),
    "inferenceGatewayAuthScheme": ("bearer", str),
    "inferenceGatewayHeaders": ("[]", str),
    "inferenceModels": ('["sonnet","haiku","opus"]', str),
    "inferenceGatewayBaseUrl": ("http://127.0.0.1:18080", str),
    "isClaudeCodeForDesktopEnabled": (1, int),
}

# ── 辅助函数 ──

def _managed_policy_names(names: list[str]) -> list[str]:
    """返回本工具写入、清除时也应删除的 Claude policy 项。"""
    managed = set(DESKTOP_CONFIG.keys()) | {CCDS_MARKER}
    return [name for name in names if name in managed]


def _desktop_model_items(items: list) -> list:
    """只保留 Codex App 模型列表支持的字段。"""
    cleaned = []
    for item in items:
        if not isinstance(item, dict):
            cleaned.append(item)
            continue
        allowed = {
            "name": item.get("name"),
            "displayName": item.get("displayName"),
        }
        if item.get("supports1m") is True:
            allowed["supports1m"] = True
        cleaned.append({k: v for k, v in allowed.items() if v is not None})
    return cleaned

def _safe_config_value(name: str, value) -> str:
    """返回可展示的配置值，避免把密钥暴露给前端。"""
    lowered = name.lower()
    if "headers" in lowered and value in (None, "", "[]", []):
        return ""
    if any(token in lowered for token in ("key", "token", "secret", "authorization", "headers")):
        return "******" if value else ""
    return str(value)


def serialize_gateway_headers(extra_headers: Optional[dict], api_key: str = "") -> str:
    """把 provider extraHeaders 转为 Claude 3P policy 的额外请求头。"""
    if not isinstance(extra_headers, dict) or not extra_headers:
        return ""
    headers = []
    for name, value in extra_headers.items():
        header_name = str(name or "").strip()
        if not header_name:
            continue
        header_value = str(value or "")
        if "{apiKey}" in header_value:
            header_value = header_value.replace("{apiKey}", api_key or "")
        headers.append(f"{header_name}: {header_value}")
    return json.dumps(headers, ensure_ascii=False, separators=(",", ":")) if headers else ""

def _os_name() -> str:
    """返回 'win', 'mac', 'linux'"""
    if sys.platform == "win32":
        return "win"
    if sys.platform == "darwin":
        return "mac"
    return "linux"


def _not_supported() -> dict:
    """非 Windows 且非 macOS 时的提示"""
    return {"success": False, "message": "Codex CLI 通过环境变量配置，所有平台均支持"}


def provider_inference_models(provider: Optional[dict]) -> list:
    """生成 Codex App gateway 需要的模型列表。

    Codex App 的模型列表通过 /v1/models 返回，
    name 要和 gateway /v1/models 返回的 ID 完全一致。
    """
    fallback = ["sonnet", "haiku", "opus"]
    if not provider:
        return fallback
    result = _desktop_model_items(provider_model_entries(provider, use_alias=False))
    return result or fallback


def all_provider_inference_models(providers: list[dict]) -> list:
    """生成所有 provider 的 Codex App 模型列表。"""
    result = _desktop_model_items(all_provider_model_entries(providers))
    return result or ["sonnet", "haiku", "opus"]


def serialize_inference_models(
    provider: Optional[dict],
    providers: Optional[list[dict]] = None,
    expose_all: bool = False,
) -> str:
    """序列化 inferenceModels，供注册表 / plist 写入。"""
    models = all_provider_inference_models(providers or []) if expose_all else provider_inference_models(provider)
    return json.dumps(
        models,
        ensure_ascii=False,
        separators=(",", ":"),
    )


# ── Windows ──

def _win_get_key(read_only=False):
    import winreg
    try:
        if read_only:
            return winreg.OpenKey(winreg.HKEY_CURRENT_USER, REGISTRY_PATH, 0, winreg.KEY_READ)
        else:
            return winreg.CreateKey(winreg.HKEY_CURRENT_USER, REGISTRY_PATH)
    except (PermissionError, FileNotFoundError, OSError):
        return None


def _b64_utf8(value: str) -> str:
    """把字符串编码成 Base64，避免 PowerShell 参数转义问题。"""
    return base64.b64encode(str(value or "").encode("utf-8")).decode("ascii")


def _ps_single_quote(value: str) -> str:
    """PowerShell 单引号字符串转义。"""
    return "'" + str(value).replace("'", "''") + "'"


def _current_user_sid() -> str:
    """读取当前登录用户 SID，确保提权后仍写回原用户配置。"""
    try:
        result = subprocess.run(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
            ],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except Exception:
        return ""
    if result.returncode != 0:
        return ""
    return result.stdout.strip()


def _run_elevated_powershell(script_text: str) -> tuple[bool, str]:
    """通过 UAC 提权运行临时 PowerShell 脚本。"""
    fd, script_path = tempfile.mkstemp(prefix="cas-desktop-config-", suffix=".ps1")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(script_text)

        command = (
            "$p = Start-Process -FilePath 'powershell.exe' "
            "-ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',"
            f"{_ps_single_quote(script_path)}) "
            "-WindowStyle Hidden -Verb RunAs -Wait -PassThru; exit $p.ExitCode"
        )
        result = subprocess.run(
            ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            capture_output=True,
            text=True,
            timeout=180,
        )
        output = "\n".join(part for part in (result.stdout, result.stderr) if part).strip()
        return result.returncode == 0, output
    except subprocess.TimeoutExpired as exc:
        return False, f"管理员写入超时: {exc}"
    except Exception as exc:
        return False, str(exc)
    finally:
        try:
            os.remove(script_path)
        except OSError:
            pass


def _win_apply_config_elevated(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    """权限不足时通过 UAC 写入当前用户的旧版注册表 policy。（保留函数）"""
    sid = _current_user_sid()
    target_path = f"Registry::HKEY_USERS\\{sid}\\{REGISTRY_PATH}" if sid else r"HKCU:\SOFTWARE\Policies\Claude"
    script = f"""
$ErrorActionPreference = 'Stop'
function DecodeUtf8([string]$Value) {{
    [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Value))
}}
$path = DecodeUtf8 '{_b64_utf8(target_path)}'
if (-not (Test-Path -LiteralPath $path)) {{
    New-Item -Path $path -Force | Out-Null
}}
$baseUrl = DecodeUtf8 '{_b64_utf8(base_url)}'
$gatewayApiKey = DecodeUtf8 '{_b64_utf8(gateway_api_key)}'
$inferenceModels = DecodeUtf8 '{_b64_utf8(inference_models or DESKTOP_CONFIG["inferenceModels"][0])}'
$authScheme = DecodeUtf8 '{_b64_utf8(auth_scheme or "bearer")}'
$gatewayHeaders = DecodeUtf8 '{_b64_utf8(gateway_headers or "[]")}'
New-ItemProperty -LiteralPath $path -Name 'inferenceProvider' -Value 'gateway' -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'inferenceGatewayBaseUrl' -Value $baseUrl -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'inferenceGatewayApiKey' -Value $gatewayApiKey -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'inferenceGatewayAuthScheme' -Value $authScheme -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'inferenceGatewayHeaders' -Value $gatewayHeaders -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'inferenceModels' -Value $inferenceModels -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name 'isClaudeCodeForDesktopEnabled' -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $path -Name '{CCDS_MARKER}' -Value 'true' -PropertyType String -Force | Out-Null
"""
    ok, output = _run_elevated_powershell(script)
    if ok:
        return {"success": True, "message": "已通过管理员权限写入旧版注册表配置（保留函数）"}
    detail = output or "用户取消了管理员授权，或系统拒绝提权"
    return {"success": False, "message": f"需要管理员权限写入旧版注册表配置：{detail}"}


def _win_get_config_status() -> dict:
    import winreg
    key = _win_get_key(read_only=True)
    if key is None:
        return {"configured": False, "keys": {}, "message": "注册表键不存在"}
    result = {"configured": False, "keys": {}, "message": ""}
    try:
        i = 0
        while True:
            name, value, _ = winreg.EnumValue(key, i)
            result["keys"][name] = _safe_config_value(name, value)
            i += 1
    except OSError:
        pass
    finally:
        winreg.CloseKey(key)
    result["configured"] = (
        result["keys"].get("inferenceProvider") == "gateway"
        and result["keys"].get(CCDS_MARKER) == "true"
    )
    return result


def _win_apply_config(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    key = _win_get_key(read_only=False)
    if key is None:
        return _win_apply_config_elevated(base_url, gateway_api_key, inference_models, auth_scheme, gateway_headers)
    import winreg
    try:
        inference_models = inference_models or DESKTOP_CONFIG["inferenceModels"][0]
        values = {
            "inferenceProvider": ("gateway", winreg.REG_SZ),
            "inferenceGatewayBaseUrl": (base_url, winreg.REG_SZ),
            "inferenceGatewayApiKey": (gateway_api_key, winreg.REG_SZ),
            "inferenceGatewayAuthScheme": (auth_scheme or "bearer", winreg.REG_SZ),
            "inferenceGatewayHeaders": (gateway_headers or "[]", winreg.REG_SZ),
            "inferenceModels": (inference_models, winreg.REG_SZ),
            "isClaudeCodeForDesktopEnabled": (1, winreg.REG_DWORD),
            CCDS_MARKER: ("true", winreg.REG_SZ),
        }
        for name, (value, type_) in values.items():
            winreg.SetValueEx(key, name, 0, type_, value)
        return {"success": True, "message": "Desktop 3P 配置已应用"}
    except PermissionError:
        return _win_apply_config_elevated(base_url, gateway_api_key, inference_models, auth_scheme, gateway_headers)
    except Exception as e:
        return {"success": False, "message": f"配置失败: {str(e)}"}
    finally:
        winreg.CloseKey(key)


def _win_clear_config() -> dict:
    import winreg
    # 读取所有键名
    key = _win_get_key(read_only=True)
    if key is None:
        return {"success": True, "message": "注册表键不存在，无需清除"}
    names = []
    try:
        i = 0
        while True:
            name, _, _ = winreg.EnumValue(key, i)
            names.append(name)
            i += 1
    except OSError:
        pass
    finally:
        winreg.CloseKey(key)

    managed = _managed_policy_names(names)
    if not managed:
        return {"success": True, "message": "没有需要清除的配置"}

    key = _win_get_key(read_only=False)
    if key is None:
        return {"success": False, "message": "无法打开注册表"}
    try:
        for name in managed:
            winreg.DeleteValue(key, name)
        return {"success": True, "message": f"已清除 {len(managed)} 项配置"}
    except Exception as e:
        return {"success": False, "message": f"清除失败: {str(e)}"}
    finally:
        winreg.CloseKey(key)


# ── macOS ──

MAC_BUNDLE = "com.anthropic.claudefordesktop"
MAC_PLIST = f"~/Library/Preferences/{MAC_BUNDLE}.plist"
MAC_3P_CONFIG = "~/Library/Application Support/Claude-3p/claude_desktop_config.json"
MAC_3P_CONFIG_LIBRARY = "configLibrary"


def _mac_run(args: list) -> tuple:
    """运行 defaults 命令，返回 (ok, output)"""
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=5)
        output = "\n".join(part.strip() for part in (r.stdout, r.stderr) if part.strip())
        return r.returncode == 0, output
    except (FileNotFoundError, subprocess.TimeoutExpired) as e:
        return False, str(e)


def _mac_get_plist_config_status() -> dict:
    keys = {}
    for name in DESKTOP_CONFIG:
        ok, out = _mac_run(["defaults", "read", MAC_BUNDLE, name])
        if ok:
            keys[name] = _safe_config_value(name, out)
    # 检查标记
    ok, marker = _mac_run(["defaults", "read", MAC_BUNDLE, CCDS_MARKER])
    marked = ok and marker == "true"
    configured = keys.get("inferenceProvider") == "gateway" and marked
    return {"configured": configured, "keys": keys, "message": ""}


def _mac_apply_plist_config(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    try:
        inference_models = inference_models or DESKTOP_CONFIG["inferenceModels"][0]
        expected = {}
        failures = []
        for name in DESKTOP_CONFIG:
            val, typ = DESKTOP_CONFIG[name]
            if name == "inferenceGatewayBaseUrl":
                val = base_url
            if name == "inferenceGatewayApiKey":
                val = gateway_api_key
            if name == "inferenceGatewayAuthScheme":
                val = auth_scheme or DESKTOP_CONFIG["inferenceGatewayAuthScheme"][0]
            if name == "inferenceGatewayHeaders":
                val = gateway_headers or DESKTOP_CONFIG["inferenceGatewayHeaders"][0]
            if name == "inferenceModels":
                val = inference_models
            expected[name] = val
            # 根据 Python 类型选择 defaults 的 -type 参数
            if typ == int:
                ok, out = _mac_run(["defaults", "write", MAC_BUNDLE, name, "-int", str(val)])
            else:
                ok, out = _mac_run(["defaults", "write", MAC_BUNDLE, name, "-string", str(val)])
            if not ok:
                detail = out if "key" not in name.lower() else "defaults write failed"
                failures.append(f"{name}: {detail or 'defaults write failed'}")

        ok, out = _mac_run(["defaults", "write", MAC_BUNDLE, CCDS_MARKER, "-string", "true"])
        if not ok:
            failures.append(f"{CCDS_MARKER}: {out or 'defaults write failed'}")
        expected[CCDS_MARKER] = "true"

        if failures:
            return {"success": False, "message": "macOS 配置写入失败: " + "; ".join(failures)}

        for name, val in expected.items():
            ok, out = _mac_run(["defaults", "read", MAC_BUNDLE, name])
            if not ok:
                failures.append(f"{name}: readback failed")
                continue
            if str(out) != str(val):
                failures.append(f"{name}: readback mismatch")

        if failures:
            return {"success": False, "message": "macOS 配置写入校验失败: " + "; ".join(failures)}
        return {"success": True, "message": "macOS Desktop 3P 配置已应用"}
    except Exception as e:
        return {"success": False, "message": f"macOS 配置失败: {str(e)}"}


def _mac_config_json_path() -> str:
    return os.path.expanduser(MAC_3P_CONFIG)


def _mac_config_library_dir_path() -> str:
    return os.path.join(os.path.dirname(_mac_config_json_path()), MAC_3P_CONFIG_LIBRARY)


def _mac_config_library_meta_path() -> str:
    return os.path.join(_mac_config_library_dir_path(), "_meta.json")


def _mac_config_library_entry_path(entry_id: str) -> str:
    return os.path.join(_mac_config_library_dir_path(), f"{entry_id}.json")


def _mac_read_json_file(path: str) -> tuple[bool, dict, str]:
    if not os.path.exists(path):
        return True, {}, ""
    try:
        with open(path, "r", encoding="utf-8") as handle:
            data = json.load(handle)
        if not isinstance(data, dict):
            return False, {}, "JSON root is not an object"
        return True, data, ""
    except Exception as exc:
        return False, {}, str(exc)


def _mac_write_json_file(path: str, data: dict) -> tuple[bool, str]:
    directory = os.path.dirname(path)
    temp_path = ""
    try:
        os.makedirs(directory, exist_ok=True)
        fd, temp_path = tempfile.mkstemp(prefix=".cas-", suffix=".json", dir=directory)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(data, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        os.replace(temp_path, path)
        return True, ""
    except Exception as exc:
        if temp_path:
            try:
                os.remove(temp_path)
            except OSError:
                pass
        return False, str(exc)


def _mac_read_json_config() -> tuple[bool, dict, str]:
    return _mac_read_json_file(_mac_config_json_path())


def _mac_write_json_config(data: dict) -> tuple[bool, str]:
    return _mac_write_json_file(_mac_config_json_path(), data)


def _mac_json_model_names(inference_models: str) -> list[str]:
    try:
        parsed = json.loads(inference_models or DESKTOP_CONFIG["inferenceModels"][0])
    except (TypeError, ValueError):
        parsed = []
    result = []
    if isinstance(parsed, list):
        for item in parsed:
            if isinstance(item, dict):
                model_name = str(item.get("name") or "").strip()
            else:
                model_name = str(item or "").strip()
            if model_name and model_name not in result:
                result.append(model_name)
    return result or ["sonnet", "haiku", "opus"]


def _mac_json_gateway_headers(gateway_headers: str) -> list[str]:
    try:
        parsed = json.loads(gateway_headers or DESKTOP_CONFIG["inferenceGatewayHeaders"][0])
    except (TypeError, ValueError):
        parsed = []
    if isinstance(parsed, list):
        return [str(item) for item in parsed if str(item or "").strip()]
    if isinstance(parsed, str) and parsed.strip():
        return [parsed.strip()]
    return []


def _mac_json_enterprise_config(
    base_url: str,
    gateway_api_key: str,
    inference_models: str,
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    return {
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceGatewayApiKey": gateway_api_key,
        "inferenceGatewayAuthScheme": auth_scheme or DESKTOP_CONFIG["inferenceGatewayAuthScheme"][0],
        "inferenceGatewayHeaders": _mac_json_gateway_headers(gateway_headers),
        "inferenceModels": _mac_json_model_names(inference_models),
        "isClaudeCodeForDesktopEnabled": True,
    }


def _mac_json_status_keys(enterprise_config: dict) -> dict:
    keys = {}
    for name in DESKTOP_CONFIG:
        if name not in enterprise_config:
            continue
        value = enterprise_config.get(name)
        if name == "inferenceModels" and isinstance(value, list):
            value = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        if name == "inferenceGatewayHeaders" and isinstance(value, list):
            value = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        if name == "isClaudeCodeForDesktopEnabled" and isinstance(value, bool):
            value = int(value)
        keys[name] = _safe_config_value(name, value)
    return keys


def _mac_flat_config_status_keys(config: dict) -> dict:
    keys = _mac_json_status_keys(config)
    aliases = {
        "provider": "inferenceProvider",
        "apiKey": "inferenceGatewayApiKey",
        "authScheme": "inferenceGatewayAuthScheme",
        "baseUrl": "inferenceGatewayBaseUrl",
        "models": "inferenceModels",
    }
    for source, target in aliases.items():
        if target in keys or source not in config:
            continue
        value = config.get(source)
        if source == "models" and isinstance(value, dict):
            value = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        keys[target] = _safe_config_value(target, value)
    return keys


def _mac_get_json_config_status() -> dict:
    path = _mac_config_json_path()
    exists = os.path.exists(path)
    ok, data, message = _mac_read_json_config()
    if not ok:
        return {"configured": False, "keys": {}, "message": message, "exists": exists}
    enterprise_config = data.get("enterpriseConfig")
    if not isinstance(enterprise_config, dict):
        return {"configured": False, "keys": {}, "message": "", "exists": exists}
    keys = _mac_json_status_keys(enterprise_config)
    configured = data.get("deploymentMode") == "3p" and keys.get("inferenceProvider") == "gateway"
    return {"configured": configured, "keys": keys, "message": "", "exists": exists}


def _mac_config_library_entry_paths(include_missing_active: bool = False) -> tuple[bool, list[str], str]:
    library_dir = _mac_config_library_dir_path()
    meta_path = _mac_config_library_meta_path()
    ok, meta, message = _mac_read_json_file(meta_path)
    if not ok:
        return False, [], message

    paths = []
    applied_id = str(meta.get("appliedId") or "").strip()
    if applied_id and "/" not in applied_id and "\\" not in applied_id:
        active_path = _mac_config_library_entry_path(applied_id)
        if include_missing_active or os.path.exists(active_path):
            paths.append(active_path)

    if not paths and os.path.isdir(library_dir):
        for name in sorted(os.listdir(library_dir)):
            if not name.endswith(".json") or name == "_meta.json":
                continue
            paths.append(os.path.join(library_dir, name))

    return True, paths, ""


def _mac_get_library_config_status() -> dict:
    ok, paths, message = _mac_config_library_entry_paths()
    if not ok:
        return {"configured": False, "keys": {}, "message": message, "exists": False}
    if not paths:
        return {"configured": False, "keys": {}, "message": "", "exists": os.path.isdir(_mac_config_library_dir_path())}

    for path in paths:
        ok, data, message = _mac_read_json_file(path)
        if not ok:
            return {"configured": False, "keys": {}, "message": message, "exists": True}
        keys = _mac_flat_config_status_keys(data)
        if keys:
            return {
                "configured": keys.get("inferenceProvider") == "gateway",
                "keys": keys,
                "message": "",
                "exists": True,
            }
    return {"configured": False, "keys": {}, "message": "", "exists": True}


def _mac_apply_library_config(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    ok, paths, message = _mac_config_library_entry_paths(include_missing_active=True)
    if not ok:
        return {"success": False, "message": f"configLibrary 元数据读取失败: {message}"}
    if not paths:
        return {"success": True, "message": "configLibrary 不存在，无需写入"}

    expected = _mac_json_enterprise_config(
        base_url,
        gateway_api_key,
        inference_models or DESKTOP_CONFIG["inferenceModels"][0],
        auth_scheme,
        gateway_headers,
    )
    failures = []
    for path in paths:
        ok, data, message = _mac_read_json_file(path)
        if not ok:
            failures.append(f"{os.path.basename(path)}: read failed: {message}")
            continue
        data.update(expected)
        ok, message = _mac_write_json_file(path, data)
        if not ok:
            failures.append(f"{os.path.basename(path)}: write failed: {message}")
            continue

        ok, saved, message = _mac_read_json_file(path)
        if not ok:
            failures.append(f"{os.path.basename(path)}: readback failed: {message}")
            continue
        for name, value in expected.items():
            if saved.get(name) != value:
                failures.append(f"{os.path.basename(path)}: {name}: readback mismatch")

    if failures:
        return {"success": False, "message": "configLibrary 写入校验失败: " + "; ".join(failures)}
    return {"success": True, "message": "macOS configLibrary 3P 配置已应用"}


def _mac_apply_json_config(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    ok, data, message = _mac_read_json_config()
    if not ok:
        return {"success": False, "message": f"JSON 配置读取失败: {message}"}

    expected = _mac_json_enterprise_config(
        base_url,
        gateway_api_key,
        inference_models or DESKTOP_CONFIG["inferenceModels"][0],
        auth_scheme,
        gateway_headers,
    )
    enterprise_config = data.get("enterpriseConfig")
    if not isinstance(enterprise_config, dict):
        enterprise_config = {}
    enterprise_config.update(expected)
    data["deploymentMode"] = "3p"
    data["enterpriseConfig"] = enterprise_config

    ok, message = _mac_write_json_config(data)
    if not ok:
        return {"success": False, "message": f"JSON 配置写入失败: {message}"}

    ok, saved, message = _mac_read_json_config()
    if not ok:
        return {"success": False, "message": f"JSON 配置读回失败: {message}"}
    saved_enterprise = saved.get("enterpriseConfig")
    if not isinstance(saved_enterprise, dict) or saved.get("deploymentMode") != "3p":
        return {"success": False, "message": "JSON 配置写入校验失败: deploymentMode 或 enterpriseConfig 不正确"}
    failures = []
    for name, value in expected.items():
        if saved_enterprise.get(name) != value:
            failures.append(f"{name}: readback mismatch")
    if failures:
        return {"success": False, "message": "JSON 配置写入校验失败: " + "; ".join(failures)}
    return {"success": True, "message": "macOS JSON 3P 配置已应用"}


def _mac_get_config_status() -> dict:
    plist_status = _mac_get_plist_config_status()
    json_status = _mac_get_json_config_status()
    library_status = _mac_get_library_config_status()
    library_has_runtime_config = bool(library_status.get("keys"))
    json_has_runtime_config = bool(json_status.get("keys"))

    if library_has_runtime_config:
        keys = dict(library_status.get("keys") or {})
        configured = library_status.get("configured", False)
    else:
        keys = dict(plist_status.get("keys") or {})
        for name, value in (json_status.get("keys") or {}).items():
            if name == "inferenceModels" and keys.get("inferenceModels"):
                continue
            keys[name] = value
        configured = json_status.get("configured", False) if json_has_runtime_config else plist_status.get("configured", False)

    return {
        "configured": configured,
        "keys": keys,
        "message": library_status.get("message") or json_status.get("message") or plist_status.get("message", ""),
        "sources": {
            "plist": plist_status.get("configured", False),
            "json": json_status.get("configured", False),
            "configLibrary": library_status.get("configured", False),
        },
    }


def _mac_apply_config(
    base_url: str,
    gateway_api_key: str = "",
    inference_models: str = "",
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    plist_result = _mac_apply_plist_config(base_url, gateway_api_key, inference_models, auth_scheme, gateway_headers)
    json_result = _mac_apply_json_config(base_url, gateway_api_key, inference_models, auth_scheme, gateway_headers)
    library_result = _mac_apply_library_config(base_url, gateway_api_key, inference_models, auth_scheme, gateway_headers)
    if plist_result.get("success") and json_result.get("success") and library_result.get("success"):
        return {"success": True, "message": "macOS Desktop 3P 配置已应用"}

    failures = []
    if not plist_result.get("success"):
        failures.append(f"plist: {plist_result.get('message', '写入失败')}")
    if not json_result.get("success"):
        failures.append(f"json: {json_result.get('message', '写入失败')}")
    if not library_result.get("success"):
        failures.append(f"configLibrary: {library_result.get('message', '写入失败')}")
    return {"success": False, "message": "macOS 配置部分写入失败: " + "; ".join(failures)}


def _mac_clear_plist_config() -> dict:
    managed = list(DESKTOP_CONFIG.keys()) + [CCDS_MARKER]
    count = 0
    for name in managed:
        ok, _ = _mac_run(["defaults", "delete", MAC_BUNDLE, name])
        if ok:
            count += 1
    if count:
        return {"success": True, "message": f"已清除 {count} 项配置"}
    return {"success": True, "message": "没有需要清除的配置"}


def _mac_clear_json_config() -> dict:
    ok, data, message = _mac_read_json_config()
    if not ok:
        return {"success": False, "message": f"JSON 配置读取失败: {message}"}
    if not data:
        return {"success": True, "message": "JSON 配置不存在，无需清除"}

    changed = False
    if "enterpriseConfig" in data:
        data.pop("enterpriseConfig", None)
        changed = True
    if data.get("deploymentMode") != "clear":
        data["deploymentMode"] = "clear"
        changed = True
    if not changed:
        return {"success": True, "message": "JSON 配置无需清除"}

    ok, message = _mac_write_json_config(data)
    if not ok:
        return {"success": False, "message": f"JSON 配置写入失败: {message}"}
    return {"success": True, "message": "JSON 3P 配置已清除"}


def _mac_clear_library_config() -> dict:
    ok, paths, message = _mac_config_library_entry_paths()
    if not ok:
        return {"success": False, "message": f"configLibrary 元数据读取失败: {message}"}
    if not paths:
        return {"success": True, "message": "configLibrary 不存在，无需清除"}

    managed = set(DESKTOP_CONFIG.keys()) | {
        "provider",
        "apiKey",
        "authScheme",
        "baseUrl",
        "models",
    }
    failures = []
    for path in paths:
        ok, data, message = _mac_read_json_file(path)
        if not ok:
            failures.append(f"{os.path.basename(path)}: read failed: {message}")
            continue
        changed = False
        for name in managed:
            if name in data:
                data.pop(name, None)
                changed = True
        if not changed:
            continue
        ok, message = _mac_write_json_file(path, data)
        if not ok:
            failures.append(f"{os.path.basename(path)}: write failed: {message}")

    if failures:
        return {"success": False, "message": "configLibrary 清除失败: " + "; ".join(failures)}
    return {"success": True, "message": "configLibrary 3P 配置已清除"}


def _mac_clear_config() -> dict:
    plist_result = _mac_clear_plist_config()
    json_result = _mac_clear_json_config()
    library_result = _mac_clear_library_config()
    if plist_result.get("success") and json_result.get("success") and library_result.get("success"):
        return {"success": True, "message": "macOS Desktop 3P 配置已清除"}
    failures = []
    if not plist_result.get("success"):
        failures.append(f"plist: {plist_result.get('message', '清除失败')}")
    if not json_result.get("success"):
        failures.append(f"json: {json_result.get('message', '清除失败')}")
    if not library_result.get("success"):
        failures.append(f"configLibrary: {library_result.get('message', '清除失败')}")
    return {"success": False, "message": "macOS 配置部分清除失败: " + "; ".join(failures)}


# ── 统一入口 ──

# ── Codex CLI 配置文件操作（参考 Codex_Account_Switch）──

CODEX_HOME = os.path.expanduser("~/.codex")
CODEX_CONFIG_PATH = os.path.join(CODEX_HOME, "config.toml")
CODEX_AUTH_PATH = os.path.join(CODEX_HOME, "auth.json")


def _read_codex_config() -> str:
    if not os.path.exists(CODEX_CONFIG_PATH):
        return ""
    try:
        with open(CODEX_CONFIG_PATH, "r", encoding="utf-8") as f:
            return f.read()
    except Exception:
        return ""


def _write_codex_config(content: str):
    os.makedirs(CODEX_HOME, exist_ok=True)
    with open(CODEX_CONFIG_PATH, "w", encoding="utf-8") as f:
        f.write(content)


def _sync_codex_toml_value(key: str, value: Optional[str]):
    """同步 ~/.codex/config.toml 中的单个键值（参考 Codex_Account_Switch 的 sync_root_string_value）。
    
    规则：
    1. 删除所有已存在的 root-level `key = ...` 行（以 key 开头且包含 = 的行）
    2. 如果 value 不为 None，在第一个 `[` 节标题之前插入新行；如果没有节，则追加到末尾。
    """
    current = _read_codex_config()
    lines = current.splitlines()
    new_lines = []
    found_first_section = False
    inserted = False
    
    for line in lines:
        stripped = line.strip()
        # 删除旧的 root-level 同名键行
        if stripped.startswith(key) and "=" in stripped:
            continue
        # 在第一个节标题 [ 之前插入新值
        if not inserted and value is not None and stripped.startswith("["):
            new_lines.append(f'{key} = {json.dumps(value)}')
            inserted = True
            found_first_section = True
        new_lines.append(line)
    
    # 如果没有节标题且 value 未插入，追加到末尾
    if not inserted and value is not None:
        new_lines.append(f'{key} = {json.dumps(value)}')
    
    result = "\n".join(new_lines)
    if not result.endswith("\n"):
        result += "\n"
    _write_codex_config(result)


def _read_codex_auth() -> dict:
    if not os.path.exists(CODEX_AUTH_PATH):
        return {}
    try:
        with open(CODEX_AUTH_PATH, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def _write_codex_auth(data: dict):
    os.makedirs(CODEX_HOME, exist_ok=True)
    with open(CODEX_AUTH_PATH, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write("\n")


def _parse_codex_base_url(config_content: str) -> str:
    for line in config_content.splitlines():
        line = line.strip()
        if line.startswith("openai_base_url") and "=" in line:
            _, val = line.split("=", 1)
            val = val.strip()
            if val.startswith('"') and val.endswith('"'):
                return val[1:-1]
            if val.startswith("'") and val.endswith("'"):
                return val[1:-1]
            return val
    return ""


def is_configured() -> bool:
    """检查 Desktop 是否已通过我们的工具配置"""
    status = get_config_status()
    return status.get("configured", False)


def _get_codex_config_status() -> dict:
    """读取 ~/.codex/config.toml 和 ~/.codex/auth.json 获取配置状态。"""
    config_content = _read_codex_config()
    auth = _read_codex_auth()
    base_url = _parse_codex_base_url(config_content)
    api_key = auth.get("OPENAI_API_KEY", "")
    configured = bool(base_url)
    return {
        "configured": configured,
        "keys": {
            "openai_base_url": base_url or "未设置",
            "OPENAI_API_KEY": "******" if api_key else "未设置",
        },
        "message": "Codex CLI 配置文件已写入" if configured else "Codex CLI 配置文件未配置",
    }


def _generate_env_config(
    base_url: str = "http://127.0.0.1:18080",
    gateway_api_key: str = "",
    provider: Optional[dict] = None,
    providers: Optional[list[dict]] = None,
    expose_all: bool = False,
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    """生成 Codex CLI 环境变量配置命令（保留，作为备用参考）。"""
    import platform
    system = platform.system()

    if system == "Windows":
        temp_cmd = f"set OPENAI_BASE_URL={base_url}\nset OPENAI_API_KEY={gateway_api_key}"
        perm_cmd = f"setx OPENAI_BASE_URL {base_url}\nsetx OPENAI_API_KEY {gateway_api_key}"
    else:
        temp_cmd = f"export OPENAI_BASE_URL={base_url}\nexport OPENAI_API_KEY={gateway_api_key}"
        shell_rc = "~/.zshrc" if os.path.exists(os.path.expanduser("~/.zshrc")) else "~/.bashrc"
        perm_cmd = f"echo 'export OPENAI_BASE_URL={base_url}' >> {shell_rc}\necho 'export OPENAI_API_KEY={gateway_api_key}' >> {shell_rc}"

    return {
        "success": True,
        "system": system,
        "commands": {
            "temporary": temp_cmd,
            "permanent": perm_cmd,
        },
        "env": {
            "OPENAI_BASE_URL": base_url,
            "OPENAI_API_KEY": gateway_api_key,
        },
        "message": "Codex CLI 配置已写入 ~/.codex/config.toml 和 ~/.codex/auth.json",
    }


def get_config_status() -> dict:
    """获取当前 Codex CLI 配置文件状态。"""
    return _get_codex_config_status()


def apply_config(
    base_url: str = "http://127.0.0.1:18080",
    gateway_api_key: str = "",
    provider: Optional[dict] = None,
    providers: Optional[list[dict]] = None,
    expose_all: bool = False,
    auth_scheme: str = "bearer",
    gateway_headers: str = "",
) -> dict:
    """将配置写入 Codex CLI 配置文件（~/.codex/config.toml + ~/.codex/auth.json）。

    在写入前会确保已对原始 Codex 配置做过快照（幂等），用于退出 / 点「还原 Codex
    原配置」时回滚。
    """
    # 在第一次 apply 前对原状态做快照；同会话再次 apply 不会覆盖原始备份。
    snapshot_codex_state()

    # 写入 config.toml：openai_base_url
    _sync_codex_toml_value("openai_base_url", base_url or None)

    # 写入 auth.json：OPENAI_API_KEY
    auth = _read_codex_auth()
    if gateway_api_key:
        auth["auth_mode"] = "apikey"
        auth["OPENAI_API_KEY"] = gateway_api_key
    else:
        auth.pop("OPENAI_API_KEY", None)
    _write_codex_auth(auth)

    return _generate_env_config(base_url, gateway_api_key, provider, providers, expose_all, auth_scheme, gateway_headers)


def clear_config() -> dict:
    """对外旧入口：还原至 apply 之前的原始 Codex 配置（智能合并）。

    保留函数名是为了让 /api/desktop/clear 端点和外部调用方无需改动；
    实际行为已升级为 restore_codex_state。
    """
    return restore_codex_state()


# ── apply 前快照 / 退出时智能合并还原 ──
#
# 设计目标：
#   1. 用户首次 apply 时，把 ~/.codex/config.toml 与 ~/.codex/auth.json 整文件备份
#      到 ~/.codex-app-transfer/codex-snapshot/，记下文件存在与否、原始内容；
#   2. 用户点「还原 Codex 原配置」或退出应用时，按 key 级别合并还原：
#        - 我们写过的 key（auth_mode、OPENAI_API_KEY、openai_base_url）改回快照值
#          （快照里没有就删除）；
#        - 其他 key / 行原样保留，不影响用户在我们运行期间手动加的内容；
#   3. 还原成功后删除快照目录，下一次 apply 重新建快照；
#   4. 若没有快照（极端情况：用户从 v1.0.0 升级而来，原状态已被破坏），
#      退化为旧版「pop OPENAI_API_KEY + 删除 openai_base_url 行」。

CAS_SNAPSHOT_DIR = os.path.expanduser("~/.codex-app-transfer/codex-snapshot")
CAS_SNAPSHOT_CONFIG = os.path.join(CAS_SNAPSHOT_DIR, "config.toml")
CAS_SNAPSHOT_AUTH = os.path.join(CAS_SNAPSHOT_DIR, "auth.json")
CAS_SNAPSHOT_MANIFEST = os.path.join(CAS_SNAPSHOT_DIR, "manifest.json")

# 我们 apply 时实际触碰的 key 集合 —— 还原时只动这些,其他 key 全部保留。
_MANAGED_AUTH_KEYS = ("auth_mode", "OPENAI_API_KEY")
_MANAGED_TOML_KEYS = ("openai_base_url",)


def _read_manifest() -> dict:
    if not os.path.exists(CAS_SNAPSHOT_MANIFEST):
        return {}
    try:
        with open(CAS_SNAPSHOT_MANIFEST, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def _write_manifest(data: dict):
    os.makedirs(CAS_SNAPSHOT_DIR, exist_ok=True)
    with open(CAS_SNAPSHOT_MANIFEST, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write("\n")


def has_snapshot() -> bool:
    """判断当前是否存在未还原的 Codex 配置快照。"""
    return os.path.exists(CAS_SNAPSHOT_MANIFEST)


def get_snapshot_status() -> dict:
    """供 UI 展示的快照状态摘要。不含敏感字段值。"""
    manifest = _read_manifest()
    if not manifest:
        return {"hasSnapshot": False}
    return {
        "hasSnapshot": True,
        "snapshotAt": manifest.get("snapshot_at"),
        "configExisted": bool(manifest.get("config_existed")),
        "authExisted": bool(manifest.get("auth_existed")),
        "appVersion": manifest.get("app_version"),
    }


def _read_file_text(path: str) -> Optional[str]:
    if not os.path.exists(path):
        return None
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except Exception:
        return None


def snapshot_codex_state(app_version: Optional[str] = None) -> dict:
    """apply 前调用：若尚未快照,则把 config.toml / auth.json 整文件复制到快照目录。

    幂等：已存在快照则直接返回当前 manifest,绝不覆盖原始快照（保证同会话内多次
    切换 provider、多次 apply 都不会污染最初那份原始备份）。
    """
    if has_snapshot():
        return _read_manifest()

    os.makedirs(CAS_SNAPSHOT_DIR, exist_ok=True)

    config_existed = os.path.exists(CODEX_CONFIG_PATH)
    auth_existed = os.path.exists(CODEX_AUTH_PATH)

    if config_existed:
        shutil.copy2(CODEX_CONFIG_PATH, CAS_SNAPSHOT_CONFIG)
    if auth_existed:
        shutil.copy2(CODEX_AUTH_PATH, CAS_SNAPSHOT_AUTH)
        try:
            os.chmod(CAS_SNAPSHOT_AUTH, 0o600)
        except OSError:
            pass  # Windows 上 chmod 无效；auth 内容已落盘即可

    manifest = {
        "snapshot_at": _datetime.datetime.now().isoformat(timespec="seconds"),
        "config_existed": config_existed,
        "auth_existed": auth_existed,
        "app_version": app_version or "",
    }
    _write_manifest(manifest)
    return manifest


def _snapshot_auth_managed_values() -> dict:
    """从快照读取我们 apply 时会动的 auth.json 字段的原始值。

    返回结构 {key: ("present", value)} 或 {key: ("absent",)}。区分"原本就没有"
    与"原本是空字符串"两种情况。快照不存在时返回空 dict（调用方需先判断）。
    """
    result: dict = {}
    if not os.path.exists(CAS_SNAPSHOT_AUTH):
        for key in _MANAGED_AUTH_KEYS:
            result[key] = ("absent",)
        return result

    try:
        with open(CAS_SNAPSHOT_AUTH, "r", encoding="utf-8") as f:
            data = json.load(f)
    except Exception:
        data = {}

    for key in _MANAGED_AUTH_KEYS:
        if key in data:
            result[key] = ("present", data[key])
        else:
            result[key] = ("absent",)
    return result


def _snapshot_toml_managed_values() -> dict:
    """从快照解析 config.toml 中我们 apply 时会动的根级别 key 的原值。"""
    result: dict = {}
    snapshot_text = _read_file_text(CAS_SNAPSHOT_CONFIG)
    for key in _MANAGED_TOML_KEYS:
        if snapshot_text is None:
            result[key] = ("absent",)
            continue
        # 简单解析：找根级别第一个 `key = ...` 行（不在某个 [section] 里）。
        # 与 _sync_codex_toml_value 写入逻辑对称。
        in_section = False
        found_value: Optional[str] = None
        for line in snapshot_text.splitlines():
            stripped = line.strip()
            if stripped.startswith("[") and stripped.endswith("]"):
                in_section = True
                continue
            if in_section:
                continue
            if stripped.startswith(key) and "=" in stripped:
                _, raw = stripped.split("=", 1)
                raw = raw.strip()
                if raw.startswith('"') and raw.endswith('"'):
                    found_value = raw[1:-1]
                elif raw.startswith("'") and raw.endswith("'"):
                    found_value = raw[1:-1]
                else:
                    found_value = raw
                break
        if found_value is not None:
            result[key] = ("present", found_value)
        else:
            result[key] = ("absent",)
    return result


def _restore_from_snapshot_smart_merge() -> dict:
    """智能合并还原：只把我们写过的 key 改回快照值,其他保持现状。"""
    auth_originals = _snapshot_auth_managed_values()
    toml_originals = _snapshot_toml_managed_values()

    # auth.json：读当前内容,逐 key 还原
    current_auth = _read_codex_auth()
    for key in _MANAGED_AUTH_KEYS:
        spec = auth_originals.get(key, ("absent",))
        if spec[0] == "present":
            current_auth[key] = spec[1]
        else:
            current_auth.pop(key, None)
    # 边界：当前 auth.json 不存在 + 原也不存在 → 不写,保持缺失
    snapshot_had_auth = os.path.exists(CAS_SNAPSHOT_AUTH)
    if current_auth or snapshot_had_auth or os.path.exists(CODEX_AUTH_PATH):
        _write_codex_auth(current_auth)
        try:
            os.chmod(CODEX_AUTH_PATH, 0o600)
        except OSError:
            pass

    # config.toml：复用 _sync_codex_toml_value 行级合并能力
    for key in _MANAGED_TOML_KEYS:
        spec = toml_originals.get(key, ("absent",))
        if spec[0] == "present":
            _sync_codex_toml_value(key, spec[1])
        else:
            _sync_codex_toml_value(key, None)

    # 还原成功后删除快照目录
    try:
        shutil.rmtree(CAS_SNAPSHOT_DIR)
    except Exception:
        pass

    return {
        "success": True,
        "restored": True,
        "message": "已还原 ~/.codex/config.toml 与 ~/.codex/auth.json 至 apply 之前的状态",
    }


def _legacy_clear_without_snapshot() -> dict:
    """旧 clear 行为：用户从 v1.0.0 升级、原状态已无快照可还原时的兜底。"""
    _sync_codex_toml_value("openai_base_url", None)
    auth = _read_codex_auth()
    auth.pop("OPENAI_API_KEY", None)
    _write_codex_auth(auth)
    return {
        "success": True,
        "restored": False,
        "message": "未发现快照,已按旧逻辑清除 openai_base_url / OPENAI_API_KEY（升级前的原始 auth_mode / 自定义 base_url 无法恢复）",
    }


def restore_codex_state() -> dict:
    """对外入口：智能合并还原至 apply 之前的状态；若无快照则退化为旧 clear 行为。"""
    if has_snapshot():
        return _restore_from_snapshot_smart_merge()
    return _legacy_clear_without_snapshot()


def discard_snapshot() -> dict:
    """显式丢弃快照（不执行还原）。预留给未来 UI；当前未在常规路径调用。"""
    if not has_snapshot():
        return {"success": True, "message": "无快照可丢弃"}
    try:
        shutil.rmtree(CAS_SNAPSHOT_DIR)
    except Exception as exc:
        return {"success": False, "message": f"丢弃快照失败：{exc}"}
    return {"success": True, "message": "已丢弃 Codex 配置快照"}
