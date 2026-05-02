"""FastAPI 应用 - 管理 API + 静态文件服务"""

import json
import os
import subprocess
import sys
import threading
import time
from contextlib import asynccontextmanager
from urllib.parse import urlparse

import httpx
import uvicorn
from pathlib import Path
from typing import Callable, Optional

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from fastapi.staticfiles import StaticFiles

from backend import config as cfg
from backend.config import APP_VERSION
from backend import provider_tools
from backend import registry
from backend import update as updater
from backend.api_adapters import normalize_api_format
from backend.model_alias import provider_model_ids
from backend.proxy import (
    LOG_DIR as PROXY_LOG_DIR,
    build_upstream_url,
    close_http_client,
    create_proxy_app,
    get_upstream_headers,
    stats as proxy_stats,
    log_buffer as proxy_logs,
)

# ── 路径设置 ──
# 前端目录: 项目根目录下的 frontend/
FRONTEND_DIR = Path(__file__).resolve().parent.parent / "frontend"
_update_quit_handler: Optional[Callable[[], None]] = None
_show_window_handler: Optional[Callable[[], None]] = None

# 用户反馈 Worker(Cloudflare),所有反馈走它收集 + 邮件通知
FEEDBACK_WORKER_URL = "https://codex-app-transfer-feedback.alysechencn.workers.dev"


class _FeedbackThrottle:
    """进程内反馈节流。

    设计原则(用户体验优先):
    - **成功提交后冷却 60s** —— 防止用户连点
    - **失败不立即计入冷却** —— 让用户能立刻改完重试
    - **5 分钟内连续 5 次失败** 才触发 60s 冷却 —— 防滥用兜底
    """

    SUCCESS_COOLDOWN_S = 60
    FAILURE_WINDOW_S = 300        # 5 分钟
    FAILURE_LIMIT = 5
    FAILURE_COOLDOWN_S = 60

    def __init__(self):
        self._lock = threading.Lock()
        self._last_success_ts: float = 0.0
        self._failure_ts: list[float] = []
        self._failure_cooldown_until: float = 0.0

    def acquire(self) -> dict:
        with self._lock:
            now = time.time()

            # 1. 成功后 60s 冷却
            since_success = now - self._last_success_ts
            if 0 < since_success < self.SUCCESS_COOLDOWN_S:
                wait = int(self.SUCCESS_COOLDOWN_S - since_success)
                return {"ok": False, "reason": f"刚提交成功,请等 {wait} 秒后再发新反馈"}

            # 2. 累积失败导致的冷却
            if now < self._failure_cooldown_until:
                wait = int(self._failure_cooldown_until - now)
                return {"ok": False, "reason": f"连续提交失败次数过多,请等 {wait} 秒后再试"}

            # 3. 失败窗口清理
            self._failure_ts = [t for t in self._failure_ts if now - t < self.FAILURE_WINDOW_S]
            return {"ok": True}

    def record_success(self):
        with self._lock:
            self._last_success_ts = time.time()
            # 成功一次重置失败计数
            self._failure_ts.clear()
            self._failure_cooldown_until = 0.0

    def record_failure(self):
        with self._lock:
            now = time.time()
            self._failure_ts = [t for t in self._failure_ts if now - t < self.FAILURE_WINDOW_S]
            self._failure_ts.append(now)
            if len(self._failure_ts) >= self.FAILURE_LIMIT:
                self._failure_cooldown_until = now + self.FAILURE_COOLDOWN_S


_feedback_throttle = _FeedbackThrottle()


def _popen_hidden(command: list[str], *, detached: bool = False):
    """启动外部程序时避免 Windows 弹出黑色终端窗口。"""
    kwargs = {"close_fds": True}
    if detached:
        kwargs["stdin"] = subprocess.DEVNULL
        kwargs["stdout"] = subprocess.DEVNULL
        kwargs["stderr"] = subprocess.DEVNULL
    if sys.platform == "win32":
        startupinfo = subprocess.STARTUPINFO()
        startupinfo.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        kwargs["startupinfo"] = startupinfo
        kwargs["creationflags"] = getattr(subprocess, "CREATE_NO_WINDOW", 0) | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    elif detached:
        kwargs["start_new_session"] = True
    return subprocess.Popen(command, **kwargs)


def register_update_quit_handler(handler: Optional[Callable[[], None]]):
    """注册更新安装前用于优雅退出主应用的回调。"""
    global _update_quit_handler
    _update_quit_handler = handler


def register_show_window_handler(handler: Optional[Callable[[], None]]):
    """注册"将主窗口拉到前台"的回调。供单实例锁第二实例触发用。"""
    global _show_window_handler
    _show_window_handler = handler


def _get_update_quit_handler() -> Optional[Callable[[], None]]:
    handler = _update_quit_handler
    return handler if callable(handler) else None


def _schedule_update_quit_for_install(handler: Callable[[], None], delay: float = 0.8) -> bool:
    """延迟一点退出主应用，确保前端先收到安装响应。"""
    if not callable(handler):
        return False

    def _invoke_handler():
        try:
            handler()
        except Exception:
            return

    timer = threading.Timer(delay, _invoke_handler)
    timer.daemon = True
    timer.start()
    return True


def _launch_update_installer(installer_path: str, platform: str) -> bool:
    """启动安装器；macOS 上优先等待当前应用退出后再打开安装包。"""
    quit_handler = _get_update_quit_handler() if platform.startswith("macos-") else None
    if platform.startswith("macos-") and quit_handler:
        command = updater.install_after_quit_command(installer_path, platform, os.getpid())
        _popen_hidden(command, detached=True)
        return _schedule_update_quit_for_install(quit_handler)

    command = updater.install_command(installer_path, platform)
    _popen_hidden(command)
    return False


def _public_provider(provider: Optional[dict]) -> Optional[dict]:
    """返回给前端展示的 provider，避免泄露 API Key。"""
    if not provider:
        return None
    public = dict(provider)
    if "apiKey" in public:
        public["hasApiKey"] = bool(public.get("apiKey"))
        public.pop("apiKey", None)
    public.pop("extraHeaders", None)
    return public


def _provider_not_found():
    return JSONResponse(
        status_code=404,
        content={"success": False, "message": "提供商不存在"},
    )


def _parse_inference_models(raw_value: str) -> list:
    """解析 Desktop managed policy 中的 inferenceModels。"""
    try:
        parsed = json.loads(raw_value or "[]")
    except (TypeError, ValueError):
        return []
    return parsed if isinstance(parsed, list) else []


def desktop_config_target_for_provider(provider: Optional[dict], settings: Optional[dict] = None) -> dict:
    """生成 Codex App 配置目标。

    Responses 兼容 provider 直接写真实地址和真实 Key；OpenAI Chat 等需要转换
    的实验接口才保留本地转发模式。
    """
    settings = settings or cfg.get_settings()
    api_format = normalize_api_format((provider or {}).get("apiFormat", "responses"))
    requires_proxy = api_format != "responses" or not provider
    if requires_proxy:
        proxy_port = settings.get("proxyPort", 18080)
        return {
            "baseUrl": f"http://127.0.0.1:{proxy_port}",
            "apiKey": cfg.get_or_create_gateway_api_key(),
            "authScheme": "bearer",
            "gatewayHeaders": "",
            "provider": provider,
            "providers": None,
            "exposeAll": False,
            "requiresProxy": True,
            "mode": "local_proxy",
        }
    api_key = provider.get("apiKey") or ""
    return {
        "baseUrl": str(provider.get("baseUrl") or "").rstrip("/"),
        "apiKey": api_key,
        "authScheme": provider.get("authScheme") or "bearer",
        "gatewayHeaders": registry.serialize_gateway_headers(provider.get("extraHeaders"), api_key),
        "provider": provider,
        "providers": None,
        "exposeAll": False,
        "requiresProxy": False,
        "mode": "direct_provider",
    }


def _desktop_health(
    desktop_status: dict,
    proxy_port: int,
    provider: Optional[dict],
    providers: Optional[list[dict]] = None,
    expose_all: bool = False,
) -> dict:
    """判断 Codex App 配置是否仍指向本工具当前 provider。"""
    keys = desktop_status.get("keys") or {}
    settings = dict(cfg.get_settings())
    settings["proxyPort"] = proxy_port
    target = desktop_config_target_for_provider(provider, settings)
    expected_base_url = str(target.get("baseUrl") or "").rstrip("/")
    actual_base_url = str(keys.get("inferenceGatewayBaseUrl") or "").rstrip("/")
    issues = []

    if actual_base_url and actual_base_url != expected_base_url:
        issues.append({
            "code": "gateway_base_url_mismatch",
            "message": "Codex CLI 仍指向旧地址，请重新一键生成 Codex CLI 配置。",
        })

    if desktop_status.get("configured") is False and keys:
        issues.append({
            "code": "not_managed_by_cas",
            "message": "当前桌面版配置不是由本工具最新版本写入。",
        })

    inference_models = _parse_inference_models(str(keys.get("inferenceModels") or ""))
    target_models = (
        registry.provider_inference_models(provider)
    )
    one_million_models = [
        str(item.get("name"))
        for item in target_models
        if isinstance(item, dict) and item.get("supports1m") is True and item.get("name")
    ]
    one_million_ready = True
    if one_million_models:
        one_million_ready = False
        for item in inference_models:
            if (
                isinstance(item, dict)
                and item.get("name") in one_million_models
                and item.get("supports1m") is True
            ):
                one_million_ready = True
                break
        if not one_million_ready:
            issues.append({
                "code": "one_million_not_written",
                "message": "1M 上下文模型尚未写入 Codex CLI 配置，请重新一键生成配置并重启终端。",
            })

    return {
        "needsApply": bool(issues),
        "oneMillionReady": one_million_ready,
        "expectedBaseUrl": expected_base_url,
        "actualBaseUrl": actual_base_url,
        "mode": target.get("mode"),
        "requiresProxy": bool(target.get("requiresProxy")),
        "issues": issues,
    }


def _sync_desktop_for_active_provider() -> dict:
    """默认 provider 切换后，同步本工具管理的 Codex CLI 模型列表 + 按需启停转发。"""
    provider = cfg.get_active_provider()
    if not provider:
        return {"attempted": False, "success": False, "message": "没有默认提供商"}

    settings = cfg.get_settings()
    target = desktop_config_target_for_provider(provider, settings)

    # 转发服务状态跟新 provider 对齐：需要转发就起,不需要就停
    if target.get("requiresProxy"):
        _start_proxy_server(settings.get("proxyPort", 18080))
    else:
        if _proxy_running:
            _stop_proxy_server()

    result = registry.apply_config(
        target["baseUrl"],
        gateway_api_key=target["apiKey"],
        provider=target["provider"],
        providers=target["providers"],
        expose_all=target["exposeAll"],
        auth_scheme=target["authScheme"],
        gateway_headers=target["gatewayHeaders"],
    )
    return {
        "attempted": True,
        "mode": target["mode"],
        "requiresProxy": target["requiresProxy"],
        **result,
    }


def auto_apply_active_provider_on_startup() -> dict:
    """启动时按 active provider 写入 ~/.codex/ 并按需起转发服务。

    供 main.py 启动序列调用。任何异常都吞掉、记日志,不阻塞应用启动。

    返回 {applied, requiresProxy, proxyStarted, message} 供日志输出。
    """
    try:
        provider = cfg.get_active_provider()
        if not provider:
            return {"applied": False, "requiresProxy": False, "proxyStarted": False,
                    "message": "no active provider; skip"}

        settings = cfg.get_settings()
        target = desktop_config_target_for_provider(provider, settings)
        proxy_started = False
        if target.get("requiresProxy"):
            proxy_started = _start_proxy_server(settings.get("proxyPort", 18080))

        registry.apply_config(
            target["baseUrl"],
            gateway_api_key=target["apiKey"],
            provider=target["provider"],
            providers=target["providers"],
            expose_all=target["exposeAll"],
            auth_scheme=target["authScheme"],
            gateway_headers=target["gatewayHeaders"],
        )
        return {
            "applied": True,
            "requiresProxy": bool(target.get("requiresProxy")),
            "proxyStarted": bool(proxy_started),
            "message": f"applied {provider.get('name', provider.get('id'))}",
        }
    except Exception as exc:  # 启动阶段任何错误都不阻塞应用打开
        return {"applied": False, "requiresProxy": False, "proxyStarted": False,
                "message": f"failed: {exc}"}


def maybe_stop_proxy_for_provider(provider: Optional[dict]) -> bool:
    """切 provider 后,如果新 provider 不需要转发,把 proxy stop 掉避免空跑。

    返回是否真的执行了 stop。
    """
    if not provider:
        return False
    settings = cfg.get_settings()
    target = desktop_config_target_for_provider(provider, settings)
    if target.get("requiresProxy"):
        return False
    if not _proxy_running:
        return False
    _stop_proxy_server()
    return True


def stop_proxy_if_running() -> bool:
    """供 main.py atexit 钩子调用。已停 / 未启动时是 no-op。"""
    if not _proxy_running:
        return False
    _stop_proxy_server()
    return True


def _provider_test_model(provider: dict) -> str:
    for model in provider_model_ids(provider):
        if model:
            return model
    return "claude-sonnet-4-6"


def _provider_test_body(provider: dict, api_format: str) -> dict:
    model = _provider_test_model(provider)
    if api_format == "openai_chat":
        return {
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 8,
            "stream": False,
        }
    return {
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 8,
    }


def _is_kimi_provider(provider: dict) -> bool:
    """粗略识别 Kimi provider，用于给出更具体的排错提示。"""
    probe = f"{provider.get('name', '')} {provider.get('baseUrl', '')}".lower()
    return "kimi" in probe or "moonshot" in probe


async def _test_provider_connection(provider: dict) -> dict:
    """测试 provider 是否能真实访问上游接口。"""
    api_format = normalize_api_format(provider.get("apiFormat", "responses"))
    base_url = build_upstream_url(provider.get("baseUrl", ""), api_format)
    parsed = urlparse(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        return {
            "success": False,
            "message": "API 地址无效",
        }

    headers = get_upstream_headers(provider)
    headers.pop("Content-Type", None)
    started = time.perf_counter()

    try:
        timeout = httpx.Timeout(8.0, connect=5.0)
        async with httpx.AsyncClient(timeout=timeout, follow_redirects=False) as client:
            response = await client.head(base_url, headers=headers)
            if response.status_code in {404, 405}:
                response = await client.get(base_url, headers=headers)
            if response.status_code in {404, 405} and provider.get("apiKey"):
                response = await client.post(
                    base_url,
                    headers=get_upstream_headers(provider),
                    json=_provider_test_body(provider, api_format),
                )
    except httpx.RequestError as exc:
        latency_ms = round((time.perf_counter() - started) * 1000)
        return {
            "success": True,
            "ok": False,
            "latencyMs": latency_ms,
            "message": f"连接失败：{exc.__class__.__name__}",
        }

    latency_ms = round((time.perf_counter() - started) * 1000)
    status_code = response.status_code
    reachable = status_code < 500
    if 200 <= status_code < 300:
        message = f"连接正常，{latency_ms} ms"
    elif status_code in {401, 403}:
        reachable = False
        if _is_kimi_provider(provider):
            message = (
                f"Kimi 认证失败，HTTP {status_code}。Kimi Platform Key 请使用 "
                f"https://api.moonshot.cn/v1；Kimi Code 会员 Key 请使用 "
                f"https://api.kimi.com/coding，{latency_ms} ms"
            )
        else:
            message = f"认证失败，HTTP {status_code}，请检查 API Key 和 API 地址是否匹配，{latency_ms} ms"
    elif status_code in {404, 405}:
        reachable = False
        message = f"接口不可用，HTTP {status_code}，请检查 API 地址是否填到了兼容 Codex 的接口，{latency_ms} ms"
    else:
        message = f"地址可达，HTTP {status_code}，{latency_ms} ms"

    return {
        "success": True,
        "ok": reachable,
        "latencyMs": latency_ms,
        "statusCode": status_code,
        "message": message,
    }


def _provider_compatibility(provider: dict) -> dict:
    """返回 provider 第三方接口兼容性摘要，不发起网络请求。"""
    api_format = normalize_api_format(provider.get("apiFormat", "responses"))
    if api_format == "responses":
        return {
            "id": provider.get("id"),
            "name": provider.get("name"),
            "apiFormat": api_format,
            "level": "stable",
            "message": "Responses 兼容接口，适合 Codex App 主流程。",
            "checks": {
                "models": True,
                "text": True,
                "stream": True,
                "tools": True,
                "streamingTools": True,
            },
        }
    if api_format == "openai_chat":
        return {
            "id": provider.get("id"),
            "name": provider.get("name"),
            "apiFormat": api_format,
            "level": "experimental",
            "message": "OpenAI Chat 实验适配：文本和非流式工具调用可测试，流式工具调用暂不作为稳定能力。",
            "checks": {
                "models": True,
                "text": True,
                "stream": True,
                "tools": True,
                "streamingTools": False,
            },
        }
    return {
        "id": provider.get("id"),
        "name": provider.get("name"),
        "apiFormat": api_format,
        "level": "unsupported",
        "message": f"{api_format} 暂未适配。",
        "checks": {
            "models": False,
            "text": False,
            "stream": False,
            "tools": False,
            "streamingTools": False,
        },
    }


def create_admin_app() -> FastAPI:
    """创建管理后台 FastAPI 应用"""

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        yield
        await close_http_client()

    app = FastAPI(title="Codex App Transfer Admin", version=APP_VERSION, lifespan=lifespan)

    @app.middleware("http")
    async def require_app_header_for_writes(request: Request, call_next):
        """阻止普通网页表单跨站触发本地写操作。"""
        sensitive_read = (
            request.url.path == "/api/config/export"
            or (
                request.url.path.startswith("/api/providers/")
                and request.url.path.endswith("/secret")
            )
        )
        if (
            request.url.path.startswith("/api/")
            and (
                request.method not in {"GET", "HEAD", "OPTIONS"}
                or sensitive_read
            )
            and request.headers.get("x-cas-request") != "1"
        ):
            return JSONResponse(
                status_code=403,
                content={"success": False, "message": "Invalid local request"},
            )
        return await call_next(request)

    # ── 状态 API ──
    @app.get("/api/status")
    async def get_status():
        """获取全局状态"""
        providers = cfg.get_providers()
        active = cfg.get_active_provider()
        desktop_status = registry.get_config_status()
        settings = cfg.get_settings()
        proxy_port = settings.get("proxyPort", 18080)
        expose_all = False
        target = desktop_config_target_for_provider(active, settings)

        return {
            "desktopConfigured": desktop_status.get("configured", False),
            "proxyRunning": _proxy_running,
            "proxyPort": proxy_port,
            "desktopMode": target["mode"],
            "desktopRequiresProxy": target["requiresProxy"],
            "activeProvider": _public_provider(active),
            "activeProviderId": active["id"] if active else None,
            "providerCount": len(providers),
            "desktopHealth": _desktop_health(desktop_status, proxy_port, active, providers, expose_all),
            "exposeAllProviderModels": expose_all,
        }

    # ── 单实例握手 API ──
    # 第二实例启动时探测 admin 端口拿到 instance-info 即可识别"已有实例在跑";
    # 调 instance-show-window 让旧实例把主窗口拉到前台,然后第二实例 sys.exit(0)。
    @app.get("/api/instance-info")
    async def get_instance_info():
        return {
            "app": "codex-app-transfer",
            "version": APP_VERSION,
            "pid": os.getpid(),
        }

    @app.post("/api/instance-show-window")
    async def instance_show_window():
        handler = _show_window_handler
        if not callable(handler):
            return {"shown": False, "reason": "no-handler"}
        try:
            handler()
        except Exception as exc:
            return {"shown": False, "error": str(exc)}
        return {"shown": True}

    # ── 提供商 API ──
    @app.get("/api/providers")
    async def list_providers():
        """获取所有提供商"""
        providers = [_public_provider(p) for p in cfg.get_providers()]
        active_id = cfg.load_config().get("activeProvider")
        return {
            "providers": providers,
            "activeId": active_id,
        }

    @app.put("/api/providers/reorder")
    async def reorder_providers(request: Request):
        """保存拖动后的 provider 顺序。"""
        data = await request.json()
        provider_ids = data.get("providerIds", [])
        if not isinstance(provider_ids, list) or not all(isinstance(item, str) for item in provider_ids):
            return JSONResponse(
                status_code=400,
                content={"success": False, "message": "providerIds 必须是字符串数组"},
            )
        if cfg.reorder_providers(provider_ids):
            return {"success": True, "providers": [_public_provider(p) for p in cfg.get_providers()]}
        return JSONResponse(
            status_code=400,
            content={"success": False, "message": "provider 排序保存失败"},
        )

    @app.get("/api/providers/{provider_id}/secret")
    async def get_provider_secret(provider_id: str):
        """读取已保存的 Provider API Key，仅允许本机前端调用。"""
        provider = cfg.get_provider(provider_id)
        if not provider:
            return _provider_not_found()
        return {"apiKey": provider.get("apiKey", "")}

    @app.post("/api/providers/{provider_id}/draft")
    async def save_provider_draft(provider_id: str, request: Request):
        """实时保存 provider 草稿到 Library。"""
        data = await request.json()
        cfg.write_library_provider(provider_id, data)
        return {"success": True, "message": "草稿已保存"}

    @app.post("/api/providers/{provider_id}/activate")
    async def activate_provider(provider_id: str):
        """将 Library 中的 provider 激活并同步到 config.json 和 Codex CLI。"""
        library_provider = cfg.read_library_provider(provider_id)
        if not library_provider:
            return JSONResponse(
                status_code=404,
                content={"success": False, "message": "提供商草稿不存在"},
            )
        config = cfg.load_config()
        config_providers = config.get("providers", [])
        existing_provider = None
        existing_index = None
        for i, p in enumerate(config_providers):
            if p.get("id") == provider_id:
                existing_provider = p
                existing_index = i
                break
        provider = dict(library_provider)
        if existing_provider and not provider.get("apiKey"):
            provider["apiKey"] = existing_provider.get("apiKey", "")
        if existing_index is not None:
            config_providers[existing_index] = provider
        else:
            config_providers.append(provider)
        config["providers"] = config_providers
        config["activeProvider"] = provider_id
        cfg.save_config(config)
        try:
            desktop_sync = _sync_desktop_for_active_provider()
        except Exception as exc:
            desktop_sync = {
                "attempted": True,
                "success": False,
                "message": f"桌面版模型同步失败: {exc}",
            }
        return {
            "success": True,
            "message": "提供商已激活",
            "desktopSync": desktop_sync,
        }

    @app.post("/api/providers")
    async def create_provider(request: Request):
        """添加提供商"""
        data = await request.json()
        provider = cfg.add_provider(data)
        return {"success": True, "provider": _public_provider(provider)}

    @app.put("/api/providers/{provider_id}")
    async def edit_provider(provider_id: str, request: Request):
        """编辑提供商"""
        data = await request.json()
        result = cfg.update_provider(provider_id, data)
        if result:
            return {"success": True, "provider": _public_provider(result)}
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    @app.delete("/api/providers/{provider_id}")
    async def remove_provider(provider_id: str):
        """删除提供商"""
        if cfg.delete_provider(provider_id):
            return {"success": True, "message": "已删除"}
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    @app.put("/api/providers/{provider_id}/default")
    async def set_default_provider(provider_id: str):
        """设为默认"""
        if cfg.set_active_provider(provider_id):
            try:
                desktop_sync = _sync_desktop_for_active_provider()
            except Exception as exc:
                desktop_sync = {
                    "attempted": True,
                    "success": False,
                    "message": f"桌面版模型同步失败: {exc}",
                }
            return {
                "success": True,
                "message": "默认提供商已更新",
                "desktopSync": desktop_sync,
            }
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    @app.post("/api/providers/test")
    async def test_provider_payload(request: Request):
        """测试表单中尚未保存的 provider 连接。"""
        data = await request.json()
        return await _test_provider_connection(data)

    @app.post("/api/providers/models/available")
    async def get_available_models_from_payload(request: Request):
        """自动获取表单中尚未保存的 provider 模型列表。"""
        data = await request.json()
        result = await provider_tools.fetch_provider_models(data)
        if result.get("success"):
            return result
        return JSONResponse(status_code=400, content=result)

    @app.post("/api/providers/{provider_id}/test")
    async def test_saved_provider(provider_id: str):
        """测试已保存 provider 的连接延迟（优先读取 Library）。"""
        provider = cfg.get_merged_provider(provider_id)
        if provider:
            return await _test_provider_connection(provider)
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    @app.post("/api/providers/{provider_id}/usage")
    async def query_provider_usage(provider_id: str):
        """查询提供商余额/用量。"""
        provider = cfg.get_provider(provider_id)
        if not provider:
            return _provider_not_found()
        return await provider_tools.query_provider_usage(provider)

    @app.get("/api/providers/compatibility")
    async def provider_compatibility_report():
        """查看已保存 provider 的第三方接口兼容性摘要。"""
        providers = [_provider_compatibility(provider) for provider in cfg.get_providers()]
        return {
            "success": True,
            "providers": providers,
            "experimentalCount": len([item for item in providers if item["level"] == "experimental"]),
        }

    # ── 模型映射 API ──
    @app.get("/api/providers/{provider_id}/models")
    async def get_models(provider_id: str):
        """获取模型映射"""
        providers = cfg.get_providers()
        for p in providers:
            if p["id"] == provider_id:
                return {"models": p.get("models", {})}
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    @app.get("/api/providers/{provider_id}/models/available")
    async def get_available_models(provider_id: str):
        """自动获取 provider 支持的模型列表。"""
        provider = cfg.get_provider(provider_id)
        if not provider:
            return _provider_not_found()
        result = await provider_tools.fetch_provider_models(provider)
        if result.get("success"):
            return result
        return JSONResponse(status_code=400, content=result)

    @app.post("/api/providers/{provider_id}/models/autofill")
    async def autofill_models(provider_id: str):
        """自动获取模型列表并写入推荐模型映射。"""
        provider = cfg.get_provider(provider_id)
        if not provider:
            return _provider_not_found()
        result = await provider_tools.fetch_provider_models(provider)
        if not result.get("success"):
            return JSONResponse(status_code=400, content=result)
        if cfg.update_models(provider_id, result.get("suggested", {})):
            return {
                "success": True,
                "models": result.get("models", []),
                "suggested": result.get("suggested", {}),
                "endpoint": result.get("endpoint"),
                "message": "模型映射已自动填充",
            }
        return _provider_not_found()

    @app.put("/api/providers/{provider_id}/models")
    async def save_models(provider_id: str, request: Request):
        """保存模型映射"""
        data = await request.json()
        if cfg.update_models(provider_id, data.get("models", {})):
            return {"success": True, "message": "模型映射已保存"}
        return JSONResponse(
            status_code=404,
            content={"success": False, "message": "提供商不存在"},
        )

    # ── 配置备份 / 导入导出 API ──
    @app.post("/api/config/backup")
    async def create_config_backup():
        """手动创建配置备份。"""
        return {"success": True, "backup": cfg.create_backup("manual")}

    @app.get("/api/config/backups")
    async def list_config_backups():
        """列出配置备份。"""
        return {"backups": cfg.list_backups()}

    @app.get("/api/config/export")
    async def export_config():
        """导出完整配置。会包含 API Key，仅供用户本机下载保存。"""
        return cfg.export_config()

    @app.post("/api/config/import")
    async def import_config(request: Request):
        """导入完整配置。导入前自动备份当前配置。"""
        try:
            data = await request.json()
            result = cfg.import_config(data)
        except ValueError as exc:
            return JSONResponse(
                status_code=400,
                content={"success": False, "message": str(exc)},
            )
        return {
            "success": True,
            "message": "配置已导入",
            "backup": result["backup"],
        }

    # ── 用户反馈 API ──
    @app.post("/api/feedback")
    async def submit_feedback(request: Request):
        """转发用户反馈到 Cloudflare Worker (codex-app-transfer-feedback)。

        - 客户端用 JSON 提交(避开 pywebview WebKit 对 FormData 的 bug)
        - 服务端拼装 multipart/form-data 转发给 Worker
        - 服务端按需自动附加诊断信息(应用版本 / OS / active provider / 最近 200 行 proxy 日志)
        - 节流:成功一次 60s 冷却,失败 5 次内不限速,5 次后 60s 冷却
        """
        # 节流
        ok_msg = _feedback_throttle.acquire()
        if not ok_msg["ok"]:
            return JSONResponse(status_code=429, content={"success": False, "message": ok_msg["reason"]})

        try:
            data = await request.json()
        except Exception:
            return JSONResponse(status_code=400, content={"success": False, "message": "请求体非 JSON"})

        title = str(data.get("title") or "").strip()
        body_text = str(data.get("body") or "").strip()
        include_diag = bool(data.get("include_diagnostics", True))
        client_attachments = data.get("attachments") or []

        if not body_text:
            return JSONResponse(status_code=400, content={"success": False, "message": "请填写描述"})

        # 自动附加诊断信息(脱敏:不含 API Key / 不含 base URL)
        meta = {"app_version": APP_VERSION}
        if include_diag:
            import platform as _platform
            try:
                active = cfg.get_active_provider() or {}
                meta.update({
                    "os": _platform.system(),
                    "arch": _platform.machine(),
                    "active_provider_name": active.get("name", ""),
                    "include_diagnostics": True,
                })
            except Exception:
                pass

        # 构造 multipart 转发到 Worker
        import base64 as _b64
        files: list = []
        files.append(("meta", (None, json.dumps(meta, ensure_ascii=False), "application/json")))
        files.append(("title", (None, title, "text/plain")))
        files.append(("body", (None, body_text, "text/plain")))

        # 用户上传的附件(base64 → 二进制)
        shot_idx = log_idx = 0
        if isinstance(client_attachments, list):
            for att in client_attachments:
                if not isinstance(att, dict):
                    continue
                try:
                    raw = _b64.b64decode((att.get("content_b64") or "").encode("ascii"), validate=False)
                except Exception:
                    continue
                if not raw or len(raw) > 5 * 1024 * 1024:
                    continue
                kind = str(att.get("kind") or "log")
                name = str(att.get("name") or f"{kind}-{int(time.time())}.bin")
                content_type = str(att.get("content_type") or "application/octet-stream")
                if kind == "screenshot":
                    field = f"screenshot{shot_idx}"; shot_idx += 1
                else:
                    field = f"log{log_idx}"; log_idx += 1
                files.append((field, (name, raw, content_type)))

        # 自动附加最近 200 行 proxy 日志
        if include_diag:
            try:
                from datetime import date as _date
                from pathlib import Path as _Path
                log_path = _Path.home() / ".codex-app-transfer" / "logs" / f"proxy-{_date.today().isoformat()}.log"
                if log_path.exists():
                    lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
                    tail = "\n".join(lines[-200:])
                    if tail.strip():
                        files.append((
                            "log_proxy_tail",
                            (f"proxy-tail-{_date.today().isoformat()}.log", tail.encode("utf-8"), "text/plain"),
                        ))
            except Exception:
                pass

        # 转发到 Worker(超时 30s)
        try:
            async with httpx.AsyncClient(timeout=30.0) as client:
                resp = await client.post(FEEDBACK_WORKER_URL, files=files)
            data = resp.json() if resp.headers.get("content-type", "").startswith("application/json") else {}
        except Exception as exc:
            _feedback_throttle.record_failure()
            return JSONResponse(status_code=502, content={"success": False, "message": f"反馈服务暂不可用:{exc}"})

        if not resp.is_success or not data.get("ok"):
            _feedback_throttle.record_failure()
            return JSONResponse(
                status_code=resp.status_code if resp.status_code >= 400 else 502,
                content={"success": False, "message": data.get("error") or data.get("message") or "上游错误"},
            )

        _feedback_throttle.record_success()
        return {
            "success": True,
            "id": data.get("id", ""),
            "message": f"反馈已收到 (ID: {data.get('id', '')})",
            "email_sent": bool(data.get("email_sent")),
        }

    # ── Desktop 集成 API ──
    @app.get("/api/desktop/status")
    async def get_desktop_status():
        """获取 Codex CLI 环境变量配置状态"""
        status = registry.get_config_status()
        # 映射 keys -> config，保持前端兼容
        status["config"] = status.pop("keys", {})
        settings = cfg.get_settings()
        proxy_port = settings.get("proxyPort", 18080)
        status["health"] = _desktop_health(
            status,
            proxy_port,
            cfg.get_active_provider(),
            cfg.get_providers(),
            False,
        )
        return status

    @app.post("/api/desktop/configure")
    async def apply_desktop_config(request: Request):
        """生成 Codex CLI 环境变量配置"""
        data = await request.json() if request.headers.get("content-type") == "application/json" else {}
        active_provider = cfg.get_active_provider()
        settings = cfg.get_settings()
        if data.get("port"):
            settings = dict(settings)
            settings["proxyPort"] = int(data["port"])
        target = desktop_config_target_for_provider(active_provider, settings)
        result = registry.apply_config(
            target["baseUrl"],
            gateway_api_key=target["apiKey"],
            provider=target["provider"],
            providers=target["providers"],
            expose_all=target["exposeAll"],
            auth_scheme=target["authScheme"],
            gateway_headers=target["gatewayHeaders"],
        )
        return {**result, "mode": target["mode"], "requiresProxy": target["requiresProxy"]}

    @app.post("/api/desktop/clear")
    async def clear_desktop_config():
        """还原 ~/.codex/ 至 apply 之前的状态（智能合并；无快照时退化为旧 clear）"""
        return registry.restore_codex_state()

    @app.get("/api/desktop/snapshot-status")
    async def get_desktop_snapshot_status():
        """读取当前是否存在未还原的 Codex 原配置快照,供 Settings 页展示。"""
        return {"success": True, **registry.get_snapshot_status()}

    # ── 代理 API ──
    @app.get("/api/version")
    async def get_version():
        """获取应用版本号"""
        return {"version": APP_VERSION}

    @app.get("/api/proxy/status")
    async def get_proxy_status():
        """获取代理状态"""
        return {
            "running": _proxy_running,
            "port": cfg.get_settings().get("proxyPort", 18080),
            "stats": proxy_stats.to_dict(),
        }

    @app.post("/api/proxy/start")
    async def start_proxy(request: Request):
        """启动代理"""
        global _proxy_running
        data = await request.json() if request.headers.get("content-type") == "application/json" else {}
        requested_port = data.get("port")
        if requested_port is not None:
            try:
                port = int(requested_port)
            except (TypeError, ValueError):
                return JSONResponse(
                    status_code=400,
                    content={"success": False, "message": "Invalid port number"},
                )
            if not (1024 <= port <= 65535):
                return JSONResponse(
                    status_code=400,
                    content={"success": False, "message": "Port must be between 1024 and 65535"},
                )
            cfg.update_settings({"proxyPort": port})

        if _proxy_running:
            return {"success": True, "message": "代理已在运行中"}

        port = cfg.get_settings().get("proxyPort", 18080)
        success = _start_proxy_server(port)
        if success:
            return {"success": True, "message": f"代理已启动，端口: {port}"}
        return JSONResponse(
            status_code=500,
            content={"success": False, "message": "代理启动失败"},
        )

    @app.post("/api/proxy/stop")
    async def stop_proxy():
        """停止代理"""
        global _proxy_running
        if not _proxy_running:
            return {"success": True, "message": "代理未在运行"}

        _stop_proxy_server()
        return {"success": True, "message": "代理已停止"}

    @app.get("/api/proxy/logs")
    async def get_proxy_logs():
        """获取代理日志"""
        return {"logs": proxy_logs.get_all()}

    @app.post("/api/proxy/logs/clear")
    async def clear_proxy_logs():
        """清除代理日志"""
        proxy_logs.clear()
        return {"success": True}

    @app.post("/api/proxy/logs/open-dir")
    async def open_proxy_log_dir():
        """在系统资源管理器中打开日志目录"""
        try:
            os.makedirs(PROXY_LOG_DIR, exist_ok=True)
        except OSError as exc:
            return JSONResponse(
                status_code=500,
                content={"success": False, "message": f"无法创建日志目录: {exc}"},
            )

        try:
            if sys.platform == "darwin":
                _popen_hidden(["open", PROXY_LOG_DIR], detached=True)
            elif sys.platform == "win32":
                # Windows 下 explorer 可直接打开目录
                _popen_hidden(["explorer", PROXY_LOG_DIR], detached=True)
            else:
                _popen_hidden(["xdg-open", PROXY_LOG_DIR], detached=True)
        except (OSError, FileNotFoundError) as exc:
            return JSONResponse(
                status_code=500,
                content={"success": False, "message": f"无法打开日志目录: {exc}"},
            )
        return {"success": True, "path": PROXY_LOG_DIR}

    # ── 设置 API ──
    @app.get("/api/settings")
    async def get_settings():
        """获取设置"""
        return cfg.get_settings()

    @app.put("/api/settings")
    async def save_settings(request: Request):
        """保存设置"""
        data = await request.json()
        settings = cfg.update_settings(data)
        return {"success": True, "settings": settings}

    @app.get("/api/update/check")
    async def check_update(url: Optional[str] = None, current: Optional[str] = None, platform: Optional[str] = None):
        """检查最新版本，不自动下载或安装。"""
        settings = cfg.get_settings()
        update_url = url or settings.get("updateUrl") or cfg.DEFAULT_UPDATE_URL
        if not update_url:
            return JSONResponse(
                status_code=400,
                content={"success": False, "message": "请先配置 latest.json 更新地址"},
            )
        try:
            return await updater.check_update(
                url=update_url,
                current_version=current or cfg.DEFAULT_CONFIG.get("version", "1.0.0"),
                platform=platform or updater.current_platform(),
            )
        except updater.UpdateCheckError as exc:
            return JSONResponse(
                status_code=400,
                content={"success": False, "message": str(exc)},
            )

    @app.post("/api/update/install")
    async def download_and_install_update(request: Request):
        """下载最新安装包并启动安装器。"""
        data = await request.json() if request.headers.get("content-type") == "application/json" else {}
        settings = cfg.get_settings()
        update_url = data.get("url") or settings.get("updateUrl") or cfg.DEFAULT_UPDATE_URL
        platform = data.get("platform") or updater.current_platform()
        try:
            result = await updater.download_update(
                url=update_url,
                current_version=data.get("current") or cfg.DEFAULT_CONFIG.get("version", "1.0.0"),
                platform=platform,
            )
            if not result.get("updateAvailable"):
                return result
            installer_path = result.get("installerPath")
            if not installer_path:
                raise updater.UpdateCheckError("下载安装包失败")
            resolved_platform = result.get("platform") or platform
            quit_requested = _launch_update_installer(installer_path, resolved_platform)
            is_macos = resolved_platform.startswith("macos-")
            return {
                **result,
                "success": True,
                "installerStarted": True,
                "quitRequested": quit_requested,
                "message": (
                    (
                        "更新包已下载，应用即将退出并启动安装器。"
                        if quit_requested
                        else "更新包已下载并打开。请先退出当前应用，再按 macOS 提示完成安装。"
                    )
                    if is_macos
                    else "安装包已下载并启动。安装器会沿用旧安装目录，并在安装前关闭正在运行的 Codex App Transfer。"
                ),
            }
        except updater.UpdateCheckError as exc:
            return JSONResponse(
                status_code=400,
                content={"success": False, "message": str(exc)},
            )
        except OSError as exc:
            return JSONResponse(
                status_code=500,
                content={"success": False, "message": f"启动安装器失败: {exc}"},
            )

    # ── 预设 API ──
    @app.get("/api/presets")
    async def get_presets():
        """获取内置预设"""
        return {"presets": cfg.get_presets()}

    # ── 挂载前端静态文件 ──
    # 必须放在 API 路由之后，否则 "/" 挂载会先匹配 /api/* 并返回静态 404。
    if FRONTEND_DIR.exists():
        frontend_static = StaticFiles(directory=str(FRONTEND_DIR), html=True)
        app.mount("/", frontend_static, name="frontend")

    return app


# ── 代理服务器管理 ──
_proxy_running = False
_proxy_thread: Optional[threading.Thread] = None
_proxy_server = None


def _start_proxy_server(port: int) -> bool:
    """在新线程中启动代理服务器"""
    global _proxy_running, _proxy_thread, _proxy_server

    if _proxy_running:
        return True

    proxy_app = create_proxy_app()

    config = uvicorn.Config(
        proxy_app,
        host="127.0.0.1",
        port=port,
        log_level="warning",
        access_log=False,
        log_config=None,
    )
    _proxy_server = uvicorn.Server(config)

    def run():
        global _proxy_running
        _proxy_running = True
        _proxy_server.run()
        _proxy_running = False

    _proxy_thread = threading.Thread(target=run, daemon=True)
    _proxy_thread.start()
    return True


def _stop_proxy_server():
    """停止代理服务器"""
    global _proxy_running, _proxy_server
    if _proxy_server:
        _proxy_server.should_exit = True
    _proxy_running = False
