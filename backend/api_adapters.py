"""第三方 API 格式适配层 — Phase 3 集成薄包装。

本模块保留所有历史公共 API，内部实现已迁移至专业子模块：
- responses_adapter   : Responses API → Chat Completions 请求转换
- chat_responses_adapter: Chat Completions → Responses API 响应转换
- streaming_adapter   : SSE 流式状态机
- base_adapter        : 通用转换规则
- provider_workarounds: Provider-specific 参数修复
"""

from __future__ import annotations

import json
from typing import Any

# ── 从子模块导入核心转换函数 ───────────────────────────────────────────
from backend.base_adapter import (
    convert_developer_to_system,
    convert_system_to_developer,
    convert_text_format_to_response_format,
    convert_response_format_to_text_format,
    convert_reasoning_to_reasoning_effort,
    convert_reasoning_effort_to_reasoning,
    filter_unknown_params,
    extract_known_params,
    merge_consecutive_assistant_messages,
    merge_consecutive_user_messages,
    normalize_content_block,
    normalize_content_array,
    normalize_tool_parameters,
    convert_responses_tool_to_chat_tool,
    convert_chat_tool_to_responses_tool,
    CHAT_COMPLETIONS_KNOWN_PARAMS,
    RESPONSES_API_KNOWN_PARAMS,
)
from backend.chat_responses_adapter import (
    convert_chat_to_responses as _convert_chat_to_responses,
    convert_messages_to_input_items,
    build_output_items_from_message,
    rebuild_responses_output_from_chat_delta,
)
from backend.provider_workarounds import (
    apply_provider_workarounds,
    apply_request_workarounds,
    apply_response_workarounds,
    detect_provider_kind,
    is_provider,
)
from backend.responses_adapter import (
    convert_responses_to_chat_body as _convert_responses_to_chat_body,
    convert_input_item_to_message,
    encode_provider_response_id,
)
from backend.streaming_adapter import (
    StreamingAdapter,
    StreamChunkBuilder,
    create_error_event,
    create_completed_event,
    normalize_chat_chunk,
    is_usage_chunk,
)

# 向后兼容：旧名称别名
ResponsesStreamConverter = StreamingAdapter


# ── 历史工具函数（保留供 proxy.py / 外部调用）───────────────────────────

def normalize_api_format(value: str) -> str:
    """统一历史 apiFormat 值，保留 responses 主线；向后兼容旧 anthropic 值。"""
    normalized = str(value or "responses").strip().lower().replace("-", "_")
    if normalized in {"openai", "openai_chat", "chat_completions"}:
        return "openai_chat"
    if normalized in {"responses", "openai_responses"}:
        return "responses"
    # 向后兼容：旧的 anthropic 值映射为 responses
    if normalized in {"anthropic", "claude", "messages"}:
        return "responses"
    return normalized or "responses"


def content_to_text(content: Any) -> str:
    """把常见内容块转换为 OpenAI Chat 可接受的文本。"""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, str):
                parts.append(item)
            elif isinstance(item, dict):
                if isinstance(item.get("text"), str):
                    parts.append(item["text"])
                elif isinstance(item.get("content"), str):
                    parts.append(item["content"])
                elif isinstance(item.get("content"), list):
                    text = content_to_text(item["content"])
                    if text:
                        parts.append(text)
        return "\n".join(part for part in parts if part)
    return str(content)


def _tool_result_content(block: dict) -> str:
    content = block.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        text = content_to_text(content)
        return text if text else json.dumps(content, ensure_ascii=False)
    if content is None:
        return ""
    if isinstance(content, (dict, list)):
        return json.dumps(content, ensure_ascii=False)
    return str(content)


def _normalize_usage(usage: Any) -> dict:
    """标准化 usage 字典，确保包含 input_tokens / output_tokens。

    内部委托 ``adapter_utils._transform_usage_details`` 以支持更多明细字段
    （cached_tokens、audio_tokens、reasoning_tokens 等）。
    """
    from backend.adapter_utils import _transform_usage_details

    return _transform_usage_details(usage)


def _normalize_tool_choice(tool_choice: Any) -> Any:
    """规范化 tool_choice 为 Chat Completions 可接受的格式。"""
    if tool_choice is None:
        return None
    if isinstance(tool_choice, str):
        return tool_choice
    if isinstance(tool_choice, dict):
        tc_type = tool_choice.get("type")
        # OpenAI 标准格式：含 function.name 直接透传
        if tool_choice.get("function") and tool_choice.get("function", {}).get("name"):
            return tool_choice
        # Cursor IDE 等简化格式
        if tc_type == "auto":
            return "auto"
        elif tc_type == "none":
            return "none"
        elif tc_type in ("required", "tool", "any"):
            return "required"
        elif tc_type == "function":
            return "required"
    return tool_choice


def _transform_input_item_to_message(item: dict) -> list[dict]:
    """将单个 Responses API input item 转换为一个或多个 Chat Completion messages。

    向后兼容：内部直接调用 responses_adapter.convert_input_item_to_message。
    """
    return convert_input_item_to_message(item)


# ── Phase 3: 新版转换入口（薄包装，保持向后兼容）────────────────────────

def responses_to_openai_chat_body(body: dict, stream: bool) -> dict:
    """将 Responses API 请求转换为 OpenAI Chat Completions 请求。

    向后兼容：保留原有 (body, stream) 签名。内部调用新模块的完整转换逻辑，
    但 provider 与 session_cache 均传 None，以保持与旧代码一致的默认行为。
    """
    return _convert_responses_to_chat_body(
        body=body,
        provider=None,
        stream=stream,
        session_cache=None,
    )


def openai_chat_to_responses(openai_resp: dict, model: str) -> dict:
    """将 OpenAI Chat Completions 响应转换为 Responses API 响应。

    向后兼容：保留原有 (openai_resp, model) 签名。内部调用新模块的完整转换逻辑，
    provider 与 request_body 均传 None。
    """
    return _convert_chat_to_responses(
        chat_response=openai_resp,
        model=model,
        provider=None,
        request_body=None,
    )


# ── Phase 3: 新增公共 API（供 proxy.py 使用）────────────────────────────

async def build_chat_body(
    body: dict,
    stream: bool = False,
    provider: dict | None = None,
    session_cache: Any = None,
) -> dict:
    """完整版 Responses → Chat Completions 请求转换。

    Args:
        body: Responses API 请求体。
        stream: 是否流式请求。
        provider: 上游 Provider 配置，用于应用 provider-specific workaround。
        session_cache: ResponseSessionCache 实例，用于 previous_response_id 历史恢复。

    Returns:
        OpenAI Chat Completions 请求体 dict。
    """
    return await _convert_responses_to_chat_body(
        body=body,
        provider=provider,
        stream=stream,
        session_cache=session_cache,
    )


def build_responses_response(
    chat_response: dict,
    model: str,
    provider: dict | None = None,
    request_body: dict | None = None,
) -> dict:
    """完整版 Chat Completions → Responses API 响应转换。

    Args:
        chat_response: Chat Completions 响应体。
        model: 模型名。
        provider: 上游 Provider 配置。
        request_body: 原始 Responses API 请求体，用于上下文恢复。

    Returns:
        Responses API 响应体 dict。
    """
    return _convert_chat_to_responses(
        chat_response=chat_response,
        model=model,
        provider=provider,
        request_body=request_body,
    )


def get_streaming_adapter(model: str, provider_kind: str = "unknown") -> StreamingAdapter:
    """获取流式转换适配器实例。

    Args:
        model: 模型名。
        provider_kind: Provider 类型标识（如 deepseek, kimi 等），用于 provider-specific 处理。

    Returns:
        StreamingAdapter 实例。
    """
    return StreamingAdapter(model=model, provider_kind=provider_kind)
