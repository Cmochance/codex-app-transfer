"""本地代理服务 - 模型名翻译 + 请求转发 + SSE 流式处理"""

import json
import os
import threading
import uuid
from datetime import datetime
from typing import Optional

# 日志文件目录：与 config 同级，按天滚动
LOG_DIR = os.path.expanduser("~/.codex-app-transfer/logs")
# 备份目录：清除日志时不直接删除文件，而是把当前所有 proxy-*.log 移到这里
LOG_BACKUP_DIR = os.path.join(LOG_DIR, "backup")

import httpx
from fastapi import FastAPI, Request, WebSocket
from fastapi.responses import JSONResponse, StreamingResponse

from backend.api_adapters import (
    build_chat_body,
    build_responses_response,
    content_to_text,
    get_streaming_adapter,
    normalize_api_format,
)
from backend.deployment_affinity import check_deployment_affinity
from backend.response_id_codec import encode_response_id
from backend.session_cache import ResponseSessionCache
from backend.model_alias import (
    all_provider_model_entries,
    normalize_model_mappings,
    provider_model_ids as alias_provider_model_ids,
    resolve_model_alias,
    resolve_requested_model_slot,
)

def provider_model_ids(provider: Optional[dict]) -> list:
    """返回当前 provider 暴露给 Codex App 的真实模型 ID。"""
    return alias_provider_model_ids(provider)


def gateway_models_response(
    provider: Optional[dict],
    providers: Optional[list[dict]] = None,
    expose_all: bool = False,
) -> dict:
    """生成 OpenAI /v1/models 风格的模型列表响应。"""
    if expose_all:
        entries = all_provider_model_entries(providers or [])
    else:
        entries = [
            {"name": model_id, "displayName": model_id}
            for model_id in provider_model_ids(provider)
        ]
    data = []
    for item in entries:
        model_id = item["name"]
        row = {
            "id": model_id,
            "object": "model",
            "created": 1704067200,
            "owned_by": provider.get("name", "gateway") if provider else "gateway",
        }
        if item.get("supports1m") is True:
            row["supports1m"] = True
        data.append(row)
    return {
        "object": "list",
        "data": data,
    }


class ProxyStats:
    """代理统计"""

    def __init__(self):
        self._lock = threading.Lock()
        self.total = 0
        self.success = 0
        self.failed = 0
        self.today = 0
        self._date = datetime.now().strftime("%Y-%m-%d")

    def record(self, success: bool):
        with self._lock:
            self.total += 1
            today_str = datetime.now().strftime("%Y-%m-%d")
            if today_str != self._date:
                self.today = 0
                self._date = today_str
            self.today += 1
            if success:
                self.success += 1
            else:
                self.failed += 1

    def to_dict(self):
        with self._lock:
            return {
                "total": self.total,
                "success": self.success,
                "failed": self.failed,
                "today": self.today,
            }


class LogBuffer:
    """环形日志缓冲区 + 按天滚动的文件日志"""

    def __init__(self, max_size=200):
        self._logs = []
        self._max_size = max_size
        self._file_lock = threading.Lock()

    def _file_path_for(self, now: datetime) -> str:
        return os.path.join(LOG_DIR, f"proxy-{now.strftime('%Y-%m-%d')}.log")

    def _append_to_file(self, now: datetime, level: str, message: str) -> None:
        try:
            os.makedirs(LOG_DIR, exist_ok=True)
            line = f"{now.strftime('%Y-%m-%d %H:%M:%S')}\t{level}\t{message}\n"
            with self._file_lock:
                with open(self._file_path_for(now), "a", encoding="utf-8") as f:
                    f.write(line)
        except OSError:
            # 写文件失败不应影响内存缓冲
            pass

    def add(self, level: str, message: str):
        now = datetime.now()
        self._logs.append({
            "time": now.strftime("%H:%M:%S"),
            "level": level,
            "message": message,
        })
        if len(self._logs) > self._max_size:
            self._logs = self._logs[-self._max_size:]
        self._append_to_file(now, level, message)

    def get_all(self):
        return list(self._logs)

    def clear(self):
        """清空内存缓冲并备份磁盘上的日志文件。

        不直接删除任何 ``proxy-*.log``，而是给文件名末尾追加备份时间戳后
        移动到 ``LOG_BACKUP_DIR``。下一次 ``add()`` 会自动创建新的当日日志。
        """
        self._logs = []
        self._archive_logs()

    def _archive_logs(self) -> None:
        """把 ``LOG_DIR`` 根目录下所有 ``proxy-*.log`` 文件搬到 ``LOG_BACKUP_DIR``。"""
        if not os.path.isdir(LOG_DIR):
            return
        try:
            os.makedirs(LOG_BACKUP_DIR, exist_ok=True)
        except OSError:
            return
        tag = datetime.now().strftime("%Y%m%d-%H%M%S")
        with self._file_lock:
            try:
                entries = os.listdir(LOG_DIR)
            except OSError:
                return
            for name in entries:
                if not (name.startswith("proxy-") and name.endswith(".log")):
                    continue
                src = os.path.join(LOG_DIR, name)
                if not os.path.isfile(src):
                    continue
                base = name[: -len(".log")]
                dst = os.path.join(LOG_BACKUP_DIR, f"{base}_{tag}.log")
                counter = 1
                while os.path.exists(dst):
                    dst = os.path.join(
                        LOG_BACKUP_DIR, f"{base}_{tag}_{counter}.log"
                    )
                    counter += 1
                try:
                    os.replace(src, dst)
                except OSError:
                    # 单个文件移动失败不阻塞其他文件
                    continue


# 全局单例
stats = ProxyStats()
log_buffer = LogBuffer()
_session_cache = ResponseSessionCache(max_size=1000, ttl_seconds=3600)

# 全局 HTTP 客户端（连接池复用）
_http_client: httpx.AsyncClient | None = None


async def get_http_client() -> httpx.AsyncClient:
    """获取全局复用的 httpx.AsyncClient（带连接池）。"""
    global _http_client
    if _http_client is None or _http_client.is_closed:
        _http_client = httpx.AsyncClient(
            timeout=httpx.Timeout(120.0, connect=30.0),
            limits=httpx.Limits(max_connections=100, max_keepalive_connections=20),
        )
    return _http_client


async def close_http_client() -> None:
    """关闭全局 HTTP 客户端（应在应用生命周期结束时调用）。"""
    global _http_client
    if _http_client and not _http_client.is_closed:
        await _http_client.aclose()
        _http_client = None


def map_model(original_model: str, provider: Optional[dict]) -> str:
    """映射模型名：将 OpenAI 模型名映射为上游真实模型名。"""
    if not provider or not original_model:
        return original_model

    models_config = normalize_model_mappings(provider.get("models", {}))
    if not models_config:
        return original_model

    # 把 OpenAI 模型名映射到槽位
    mapped_slot = resolve_requested_model_slot(original_model)
    if mapped_slot:
        slot_model = models_config.get(mapped_slot, "").strip()
        if slot_model:
            return slot_model
        # 降级到 default
        return models_config.get("default") or original_model

    # 非 OpenAI 模型名，直接透传
    return original_model


def build_upstream_url(base_url: str, api_format: str) -> str:
    """根据用户填写的 Base URL 生成最终请求地址。

    用户可能填写基础地址，也可能直接粘贴完整 endpoint；这里统一处理，
    避免重复拼接 /v1/responses 或 /chat/completions。
    """
    clean = str(base_url or "").strip().rstrip("/")
    api_format = normalize_api_format(api_format)
    lower = clean.lower()
    if api_format == "openai_chat":
        if lower.endswith("/chat/completions"):
            return clean
        return f"{clean}/chat/completions"
    # responses 分支（原 anthropic 分支）
    if lower.endswith("/v1/responses"):
        return clean
    if lower.endswith("/v1"):
        return f"{clean}/responses"
    return f"{clean}/v1/responses"


def _content_to_text(content) -> str:
    """把文本块转换为 OpenAI 兼容接口常见的字符串 content。"""
    return content_to_text(content)


async def _responses_to_openai_body(body: dict, stream: bool, provider: dict | None = None) -> dict:
    """将 Codex App 发来的 Responses API 请求转换为 OpenAI Chat Completions。"""
    return await build_chat_body(body=body, stream=stream, provider=provider, session_cache=_session_cache)


def get_upstream_headers(provider: dict) -> dict:
    """获取上游请求的认证头"""
    auth_scheme = provider.get("authScheme", "bearer")
    api_key = provider.get("apiKey", "")

    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
    }

    # Responses API 不需要 anthropic-version 头
    # if normalize_api_format(provider.get("apiFormat", "responses")) == "responses":
    #     pass  # 保留扩展点，供将来需要特殊头时使用

    if api_key:
        if auth_scheme == "x-api-key":
            headers["x-api-key"] = api_key
        else:
            headers["Authorization"] = f"Bearer {api_key}"

    # 合并提供商自定义的额外请求头（如 DeepSeek 需要同时发 x-api-key）
    extra = provider.get("extraHeaders", {})
    if isinstance(extra, dict):
        for k, v in extra.items():
            # 支持 {apiKey} 模板变量
            headers[k] = v.replace("{apiKey}", api_key) if isinstance(v, str) else v

    # 强制覆盖 Kimi Code 的 User-Agent（应用内强制，不受用户残留配置影响）
    provider_id = str(provider.get("id") or "")
    base_url = str(provider.get("baseUrl") or "").lower()
    if provider_id == "kimi-code" or "api.kimi.com/coding" in base_url:
        headers["User-Agent"] = "KimiCLI/1.40.0"

    return headers


def _provider_kind(provider: dict) -> str:
    """用名称和 URL 粗略判断提供商，用于处理厂商私有参数。"""
    probe = f"{provider.get('name', '')} {provider.get('baseUrl', '')}".lower()
    if "deepseek" in probe:
        return "deepseek"
    if "moonshot" in probe or "kimi" in probe:
        return "kimi"
    if "bigmodel" in probe or "zhipu" in probe or "glm" in probe:
        return "zhipu"
    if "dashscope" in probe or "bailian" in probe or "aliyun" in probe:
        return "bailian"
    if "siliconflow" in probe:
        return "siliconflow"
    if "qnaigc" in probe or "qiniu" in probe:
        return "qiniu"
    return "unknown"


def _deep_merge(target: dict, source: dict) -> dict:
    """递归合并少量请求选项，保留已有请求体字段。"""
    merged = dict(target)
    for key, value in source.items():
        if isinstance(value, dict) and isinstance(merged.get(key), dict):
            merged[key] = _deep_merge(merged[key], value)
        else:
            merged[key] = value
    return merged


def _responses_request_options(provider: dict) -> dict:
    options = provider.get("requestOptions") or {}
    if not isinstance(options, dict):
        return {}
    responses_options = options.get("responses", options)
    return responses_options if isinstance(responses_options, dict) else {}


def apply_responses_request_options(upstream_body: dict, provider: dict) -> dict:
    """按 provider 差异处理 Responses API 请求里的参数。

    DeepSeek 的兼容接口支持 reasoning 等字段。
    其它提供商的兼容层对这些字段支持不一致，因此默认延续旧行为：
    不主动透传 request-level reasoning，避免上游 400。
    """
    kind = _provider_kind(provider)
    options = _responses_request_options(provider)

    if kind != "deepseek":
        upstream_body.pop("reasoning", None)
        return upstream_body

    if options:
        upstream_body = _deep_merge(upstream_body, options)

    return upstream_body


def _normalize_usage(usage) -> dict:
    """保证 usage 至少包含 input_tokens / output_tokens。"""
    def token_int(value) -> int:
        try:
            return int(value or 0)
        except (TypeError, ValueError):
            return 0

    normalized = dict(usage) if isinstance(usage, dict) else {}
    input_tokens = (
        normalized.get("input_tokens")
        if normalized.get("input_tokens") is not None
        else normalized.get("prompt_tokens")
    )
    output_tokens = (
        normalized.get("output_tokens")
        if normalized.get("output_tokens") is not None
        else normalized.get("completion_tokens")
    )
    normalized["input_tokens"] = token_int(input_tokens)
    normalized["output_tokens"] = token_int(output_tokens)
    return normalized


def _normalize_content(content) -> list:
    """把常见上游 content 变体整理成 Responses API content block。"""
    if isinstance(content, list):
        return content
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if content is None:
        return []
    return [{"type": "text", "text": str(content)}]


def _normalize_responses_message(message: dict, model: str) -> dict:
    """补齐 Responses API 响应常用字段。"""
    normalized = dict(message) if isinstance(message, dict) else {}
    normalized.setdefault("id", f"msg_{uuid.uuid4().hex[:12]}")
    normalized.setdefault("type", "message")
    normalized.setdefault("object", "response")
    normalized.setdefault("role", "assistant")
    normalized["model"] = normalized.get("model") or model
    normalized["content"] = _normalize_content(normalized.get("content"))
    normalized["usage"] = _normalize_usage(normalized.get("usage"))
    return normalized


def _normalize_responses_response(upstream_data: dict, model: str) -> dict:
    """规范 Responses API 兼容响应，确保必要字段存在。"""
    if not isinstance(upstream_data, dict) or upstream_data.get("error"):
        return upstream_data
    if upstream_data.get("object") == "response" or "output" in upstream_data:
        return _normalize_responses_message(upstream_data, model)
    return upstream_data


def _normalize_responses_sse_event(event: dict, model: str) -> dict:
    """规范 Responses API 兼容 SSE 事件中的 usage 字段。"""
    if not isinstance(event, dict):
        return event
    normalized = dict(event)
    event_type = normalized.get("type")
    if event_type == "response.created":
        normalized["response"] = _normalize_responses_message(
            normalized.get("message") or {},
            model,
        )
    elif event_type == "response.completed":
        normalized["usage"] = _normalize_usage(normalized.get("usage"))
    elif "usage" in normalized:
        normalized["usage"] = _normalize_usage(normalized.get("usage"))
    return normalized


async def forward_request(
    body: dict,
    provider: dict,
    request_id: str,
) -> dict:
    """转发请求到上游 API（非流式）"""
    api_format = normalize_api_format(provider.get("apiFormat", "responses"))

    if api_format == "openai_chat":
        upstream_url = build_upstream_url(provider.get("baseUrl", ""), api_format)
        upstream_body = await _responses_to_openai_body(body, stream=False, provider=provider)
    else:
        # Responses API 格式直接透传
        upstream_url = build_upstream_url(provider.get("baseUrl", ""), api_format)

        # 移除流式标记（我们单独处理流式）
        upstream_body = dict(body)
        upstream_body.pop("stream", None)
        upstream_body = apply_responses_request_options(upstream_body, provider)

    headers = get_upstream_headers(provider)

    log_buffer.add("INFO", f"转发请求 → {upstream_url}")
    log_buffer.add("INFO", f"模型: {body.get('model', '')} → {upstream_body.get('model', '')}")

    try:
        client = await get_http_client()
        resp = await client.post(
            upstream_url,
            json=upstream_body,
            headers=headers,
        )

        stats.record(resp.is_success)
        log_buffer.add(
            "SUCCESS" if resp.is_success else "ERROR",
            f"响应 {resp.status_code} ({round(resp.elapsed.total_seconds(), 2)}s)",
        )

        if not resp.is_success:
            return {
                "error": {
                    "type": "upstream_error",
                    "status": resp.status_code,
                    "message": resp.text[:500] or "上游 API 返回错误",
                }
            }

        try:
            upstream_data = resp.json()
        except json.JSONDecodeError:
            stats.failed += 1
            stats.success = max(0, stats.success - 1)
            log_buffer.add("ERROR", "上游 API 返回了非 JSON 响应")
            return {
                "error": {
                    "type": "invalid_upstream_response",
                    "message": "上游 API 返回了非 JSON 响应",
                }
            }

        if api_format == "openai_chat":
            # OpenAI Chat → Responses API 格式转换
            result = _openai_to_responses(
                upstream_data,
                body.get("model", ""),
                provider=provider,
                request_body=body,
            )
            # 保存会话历史用于后续 previous_response_id
            if isinstance(result, dict) and not result.get("error") and result.get("id"):
                _session_cache.save(result["id"], body.get("input", []))
            return result
        return _normalize_responses_response(upstream_data, body.get("model", ""))

    except httpx.TimeoutException:
        stats.record(False)
        log_buffer.add("ERROR", "请求超时")
        return {"error": {"type": "timeout", "message": "上游 API 请求超时"}}
    except Exception as e:
        stats.record(False)
        message = f"{e.__class__.__name__}: {str(e)}".rstrip()
        log_buffer.add("ERROR", f"请求失败: {message}")
        return {"error": {"type": "connection_error", "message": message}}


def _override_response_fields(event: dict, original_model: str, provider_name: str | None) -> dict:
    """把 openai_chat 上游回来的 streaming 事件里的 ``response.id`` / ``response.model``
    改回 Codex 客户端原始请求时使用的值。

    Codex CLI 强校验 ``response.id`` 必须是 ``resp_…``、``response.model`` 必须等于
    它发出去的模型名；否则会判定 stream 异常，最终报 "closed before response.completed"。
    """
    if not isinstance(event, dict):
        return event
    inner = event.get("response")
    if not isinstance(inner, dict):
        return event
    new_inner = dict(inner)
    raw_id = new_inner.get("id")
    if isinstance(raw_id, str) and not raw_id.startswith("resp_"):
        try:
            new_inner["id"] = encode_response_id(provider_name, original_model or new_inner.get("model"), raw_id)
        except Exception:
            pass
    if original_model:
        new_inner["model"] = original_model
    new_event = dict(event)
    new_event["response"] = new_inner
    return new_event


async def forward_request_stream(
    body: dict,
    provider: dict,
    request_id: str,
    original_model: str | None = None,
):
    """转发流式请求到上游 API（SSE）"""
    api_format = normalize_api_format(provider.get("apiFormat", "responses"))
    if not original_model:
        original_model = body.get("model", "")
    provider_name = provider.get("name") if isinstance(provider, dict) else None

    if api_format == "openai_chat":
        upstream_url = build_upstream_url(provider.get("baseUrl", ""), api_format)
        upstream_body = await _responses_to_openai_body(body, stream=True, provider=provider)
    else:
        upstream_url = build_upstream_url(provider.get("baseUrl", ""), api_format)
        upstream_body = dict(body)
        upstream_body = apply_responses_request_options(upstream_body, provider)
        # 确保流式开启
        upstream_body["stream"] = True

    headers = get_upstream_headers(provider)

    log_buffer.add("INFO", f"流式请求 → {upstream_url}")

    try:
        client = await get_http_client()
        async with client.stream(
            "POST",
            upstream_url,
            json=upstream_body,
            headers=headers,
        ) as resp:

                log_buffer.add(
                    "SUCCESS" if resp.is_success else "ERROR",
                    f"流式连接 {resp.status_code}",
                )

                if not resp.is_success:
                    stats.record(False)
                    error_text = (await resp.aread()).decode("utf-8", errors="replace")[:500]
                    log_buffer.add(
                        "ERROR",
                        f"上游 {resp.status_code} body: {error_text}" if error_text else f"上游 {resp.status_code} body: <空>",
                    )
                    error_event = {
                        "type": "error",
                        "error": {
                            "type": "upstream_error",
                            "status": resp.status_code,
                            "message": error_text or "上游 API 返回错误",
                        },
                    }
                    yield f"event: error\ndata: {json.dumps(error_event, ensure_ascii=False)}\n\n"
                    return

                if api_format == "openai_chat":
                    converter = get_streaming_adapter(body.get("model", ""), _provider_kind(provider))
                    async for line in resp.aiter_lines():
                        if not line.strip():
                            continue
                        # SSE 规范允许 data: 后有 0 或 1 个空格（部分上游如 Kimi 直接贴 JSON）。
                        if not line.startswith("data:"):
                            continue
                        data_str = line[len("data:"):].lstrip()
                        if not data_str or data_str == "[DONE]":
                            continue
                        try:
                            openai_chunk = json.loads(data_str)
                        except json.JSONDecodeError:
                            continue
                        for responses_event in converter.process_chunk(openai_chunk):
                            normalized_event = _override_response_fields(
                                responses_event, original_model, provider_name
                            )
                            yield f"data: {json.dumps(normalized_event)}\n\n"
                else:
                    async for line in resp.aiter_lines():
                        if line.startswith("data:"):
                            data_str = line[len("data:"):].strip()
                            if data_str and data_str != "[DONE]":
                                try:
                                    event = json.loads(data_str)
                                    event = _normalize_responses_sse_event(event, body.get("model", ""))
                                    yield f"data: {json.dumps(event, ensure_ascii=False)}\n"
                                    continue
                                except json.JSONDecodeError:
                                    pass
                        yield line + "\n"

                stats.record(True)
                log_buffer.add("SUCCESS", f"流式完成")

    except Exception as e:
        stats.record(False)
        message = f"{e.__class__.__name__}: {str(e)}".rstrip()
        log_buffer.add("ERROR", f"流式请求失败: {message}")
        error_event = {
            "type": "error",
            "error": {"message": message},
        }
        yield f"data: {json.dumps(error_event)}\n\n"


def _openai_to_responses(
    openai_resp: dict,
    model: str,
    provider: dict | None = None,
    request_body: dict | None = None,
) -> dict:
    """将 OpenAI Chat Completions 响应格式转换为 Responses API 格式。"""
    return build_responses_response(
        chat_response=openai_resp,
        model=model,
        provider=provider,
        request_body=request_body,
    )


# ========== FastAPI 应用 ==========

from backend.config import get_active_provider, get_gateway_api_key, get_providers, get_settings


def create_proxy_app() -> FastAPI:
    """创建代理 FastAPI 应用"""
    app = FastAPI(title="Codex App Transfer Proxy", version="1.0.0")

    def upstream_error_status(result: dict) -> int:
        """把上游错误转换成 HTTP 错误状态，避免桌面端按成功响应解析。"""
        error = result.get("error") if isinstance(result, dict) else None
        status = error.get("status") if isinstance(error, dict) else None
        try:
            status_code = int(status)
        except (TypeError, ValueError):
            return 502
        return status_code if 400 <= status_code <= 599 else 502

    def gateway_auth_failed(request: Request) -> bool:
        gateway_api_key = get_gateway_api_key()
        if not gateway_api_key:
            return True
        auth_header = request.headers.get("authorization", "")
        bearer_token = auth_header.removeprefix("Bearer ").strip()
        x_api_key = request.headers.get("x-api-key", "").strip()
        return gateway_api_key not in {bearer_token, x_api_key}

    def gateway_auth_error() -> JSONResponse:
        log_buffer.add("ERROR", "本地 gateway 认证失败")
        return JSONResponse(
            status_code=401,
            content={"error": {"message": "Invalid gateway API key"}},
        )

    @app.get("/health")
    @app.get("/status")
    async def health():
        return {"status": "ok", "stats": stats.to_dict()}

    @app.api_route("/v1/models", methods=["GET", "OPTIONS"])
    @app.api_route("/claude/v1/models", methods=["GET", "OPTIONS"])
    async def handle_models(request: Request):
        if request.method == "OPTIONS":
            return JSONResponse(
                content={},
                headers={
                    "Access-Control-Allow-Origin": "*",
                    "Access-Control-Allow-Methods": "GET, OPTIONS",
                    "Access-Control-Allow-Headers": "*",
                },
            )
        if gateway_auth_failed(request):
            return gateway_auth_error()
        settings = get_settings()
        expose_all = bool(settings.get("exposeAllProviderModels"))
        provider = get_active_provider()
        return gateway_models_response(
            provider,
            providers=get_providers() if expose_all else None,
            expose_all=expose_all,
        )

    @app.api_route("/v1/responses", methods=["POST", "OPTIONS"])
    @app.api_route("/openai/v1/responses", methods=["POST", "OPTIONS"])
    # Codex CLI 0.126+ 直接拼 /responses（无 /v1/）作为 HTTP fallback transport，
    # 兼容这种行为。
    @app.api_route("/responses", methods=["POST", "OPTIONS"])
    # 保留旧路由作为兼容别名
    @app.api_route("/v1/messages", methods=["POST", "OPTIONS"])
    @app.api_route("/claude/v1/messages", methods=["POST", "OPTIONS"])
    async def handle_responses(request: Request):
        if request.method == "OPTIONS":
            return JSONResponse(
                content={},
                headers={
                    "Access-Control-Allow-Origin": "*",
                    "Access-Control-Allow-Methods": "POST, OPTIONS",
                    "Access-Control-Allow-Headers": "*",
                },
            )

        request_id = request.headers.get("x-request-id", uuid.uuid4().hex[:12])
        try:
            body = await request.json()
        except json.JSONDecodeError:
            log_buffer.add("ERROR", "请求体 JSON 解析失败")
            return JSONResponse(
                status_code=400,
                content={"error": {"message": "Invalid JSON body"}},
            )

        if gateway_auth_failed(request):
            return gateway_auth_error()

        settings = get_settings()
        expose_all = bool(settings.get("exposeAllProviderModels"))
        providers = get_providers() if expose_all else []
        # 获取当前激活的提供商；全量模型模式下允许模型别名路由到其它 provider。
        provider = get_active_provider()
        alias_provider, alias_model, alias_hit = resolve_model_alias(providers, body.get("model", "")) if expose_all else (None, "", False)
        if alias_hit and alias_provider:
            provider = alias_provider

        # Deployment Affinity 检查（previous_response_id 路由粘性）
        previous_response_id = body.get("previous_response_id")
        if previous_response_id and provider:
            affinity = check_deployment_affinity(body, provider, providers)
            if not affinity.get("ok"):
                suggested = affinity.get("suggested_provider")
                suggested_name = suggested.get("name", "unknown") if isinstance(suggested, dict) else "unknown"
                log_buffer.add(
                    "WARN",
                    f"previous_response_id 路由粘性不匹配: "
                    f"当前={provider.get('name')}, 建议={suggested_name}"
                )
                # 如果有建议的 provider 且可用，自动切换
                if suggested and isinstance(suggested, dict) and suggested.get("apiKey"):
                    provider = suggested
                    log_buffer.add("INFO", f"自动切换到建议的 Provider: {suggested_name}")

        if not provider or not provider.get("apiKey"):
            log_buffer.add("ERROR", "没有配置有效的提供商")
            return JSONResponse(
                status_code=400,
                content={"error": {"message": "No active provider configured"}},
            )

        # 模型名翻译
        original_model = body.get("model", "")
        mapped_model = alias_model if alias_hit else map_model(original_model, provider)
        body["model"] = mapped_model

        log_buffer.add("INFO", f"请求: POST /v1/responses")
        log_buffer.add("INFO", f"模型映射: {original_model} → {mapped_model}")

        # 判断是否流式
        is_stream = body.get("stream", False)

        if is_stream:
            return StreamingResponse(
                forward_request_stream(body, provider, request_id, original_model=original_model),
                media_type="text/event-stream",
                headers={
                    "Cache-Control": "no-cache",
                    "Connection": "keep-alive",
                    "Access-Control-Allow-Origin": "*",
                },
            )
        else:
            result = await forward_request(body, provider, request_id)
            if isinstance(result, dict) and result.get("error"):
                return JSONResponse(
                    status_code=upstream_error_status(result),
                    content=result,
                )
            return JSONResponse(content=result)

    @app.websocket("/responses")
    @app.websocket("/v1/responses")
    @app.websocket("/openai/v1/responses")
    async def handle_responses_websocket(websocket: WebSocket):
        """WebSocket 版本的 responses 端点（Codex CLI 实时模式）。

        接收 Codex CLI 的 WebSocket 消息，复用现有的 HTTP 转发逻辑，
        将上游 SSE 流逐条解析后通过 WebSocket 发回。
        """
        request_id = uuid.uuid4().hex[:12]
        try:
            # 网关认证（复用 HTTP 逻辑，从 websocket.headers 读取）
            gateway_api_key = get_gateway_api_key()
            if gateway_api_key:
                auth_header = websocket.headers.get("authorization", "")
                bearer_token = auth_header.removeprefix("Bearer ").strip()
                x_api_key = websocket.headers.get("x-api-key", "").strip()
                if gateway_api_key not in {bearer_token, x_api_key}:
                    await websocket.close(code=1008)
                    return

            await websocket.accept()

            while True:
                try:
                    message = await websocket.receive_text()
                except Exception:
                    break

                try:
                    msg_obj = json.loads(message)
                except json.JSONDecodeError:
                    await websocket.send_json({"type": "error", "error": {"message": "Invalid JSON"}})
                    continue

                if msg_obj.get("type") != "response.create":
                    continue

                # 提取请求体（支持 nested / flat 两种格式）
                nested = msg_obj.get("response")
                body = (
                    nested
                    if isinstance(nested, dict) and nested
                    else {k: v for k, v in msg_obj.items() if k != "type"}
                )

                # 复用现有的 provider 解析逻辑
                settings = get_settings()
                expose_all = bool(settings.get("exposeAllProviderModels"))
                providers = get_providers() if expose_all else []
                provider = get_active_provider()
                alias_provider, alias_model, alias_hit = (
                    resolve_model_alias(providers, body.get("model", ""))
                    if expose_all else (None, "", False)
                )
                if alias_hit and alias_provider:
                    provider = alias_provider

                # Deployment Affinity
                previous_response_id = body.get("previous_response_id")
                if previous_response_id and provider:
                    affinity = check_deployment_affinity(body, provider, providers)
                    if not affinity.get("ok"):
                        suggested = affinity.get("suggested_provider")
                        if suggested and isinstance(suggested, dict) and suggested.get("apiKey"):
                            provider = suggested

                if not provider or not provider.get("apiKey"):
                    await websocket.send_json({"type": "error", "error": {"message": "No active provider configured"}})
                    continue

                # 模型映射
                original_model = body.get("model", "")
                mapped_model = alias_model if alias_hit else map_model(original_model, provider)
                body["model"] = mapped_model

                log_buffer.add("INFO", f"WS 请求: model={original_model} → {mapped_model}")

                is_stream = body.get("stream", True)
                if is_stream:
                    async for sse_event in forward_request_stream(body, provider, request_id, original_model=original_model):
                        for line in sse_event.split("\n"):
                            line = line.strip()
                            if line.startswith("data: "):
                                data_str = line[6:].strip()
                                if data_str and data_str != "[DONE]":
                                    try:
                                        await websocket.send_text(data_str)
                                    except Exception:
                                        break
                else:
                    result = await forward_request(body, provider, request_id)
                    if isinstance(result, dict) and result.get("error"):
                        await websocket.send_json({"type": "error", "error": result["error"]})
                    else:
                        await websocket.send_text(json.dumps(result))

        except Exception as exc:
            log_buffer.add("ERROR", f"WebSocket 错误: {exc}")
        finally:
            try:
                await websocket.close()
            except Exception:
                pass

    return app
