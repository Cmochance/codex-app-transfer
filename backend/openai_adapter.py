"""OpenAI 参数透传/过滤规则。

提供 Chat Completions API 与 Responses API 之间的参数白名单/黑名单管理、
参数过滤、特殊字段处理（store / metadata / prediction / service_tier /
modalities / audio）、response_format 与 reasoning 配置双向转换、
tool 配置双向转换、message 列表双向转换，以及统一的请求/响应构建入口。

不依赖 ``openai`` 或 ``litellm`` 包，所有逻辑基于纯字典操作。
"""

from __future__ import annotations

import copy
import json
import uuid
from datetime import datetime, timezone
from typing import Any, Iterator

from backend.adapter_utils import (
    _normalize_mcp_input_schema,
    _normalize_tool_choice,
    _transform_text_format_to_response_format,
    _transform_usage_details,
)

# --------------------------------------------------------------------------- #
# 参数白名单 / 黑名单定义
# --------------------------------------------------------------------------- #

OPENAI_CHAT_COMPLETIONS_PARAMS: frozenset[str] = frozenset(
    {
        # 核心参数
        "model",
        "messages",
        # 生成控制
        "frequency_penalty",
        "presence_penalty",
        "max_tokens",
        "max_completion_tokens",
        "n",
        "seed",
        "stop",
        "temperature",
        "top_p",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        # 流式
        "stream",
        "stream_options",
        # 工具
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        # 函数（legacy，但仍被部分 provider 支持）
        "functions",
        "function_call",
        # 响应格式
        "response_format",
        # 用户标识
        "user",
        # 服务级别
        "service_tier",
        # 推理（o1 / o3）
        "reasoning_effort",
        # 输出格式
        "modalities",
        "audio",
        # 预测内容
        "prediction",
        # 元数据
        "metadata",
        # 存储
        "store",
        # 其他官方参数
        "extra_headers",
        "extra_query",
        "extra_body",
        "timeout",
        # 部分 provider 扩展
        "safety_identifier",
        "safety_settings",
        "context",
        "truncate",
        "prompt_truncation",
    }
)

OPENAI_RESPONSES_API_PARAMS: frozenset[str] = frozenset(
    {
        # 核心参数
        "model",
        "input",
        "instructions",
        # 生成控制
        "max_output_tokens",
        "temperature",
        "top_p",
        "seed",
        "stop",
        # 工具
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        # 响应格式
        "text",
        "response_format",  # 部分兼容层允许直接传
        # 推理
        "reasoning",
        "reasoning_effort",
        # 输出格式
        "modalities",
        "audio",
        # 流式
        "stream",
        "stream_options",
        # 元数据与存储
        "metadata",
        "store",
        # 用户标识
        "user",
        # 服务级别
        "service_tier",
        # 上下文管理
        "previous_response_id",
        "truncation",
        "include",
        # 其他
        "background",
        "prompt",
        "max_tool_calls",
        "prompt_cache_key",
        "prompt_cache_retention",
        "partial_images",
        "context_management",
        # 未文档化但广泛使用
        "prediction",
        # 额外参数透传
        "extra_headers",
        "extra_query",
        "extra_body",
        "timeout",
    }
)

# 未文档化但被广泛使用的参数（OpenAI 官方 SDK 或生态中常见）
OPENAI_UNDOCUMENTED_PARAMS: frozenset[str] = frozenset(
    {
        "prediction",
        "store",
        "metadata",
        "modalities",
        "audio",
        "reasoning_effort",
        "service_tier",
        "safety_identifier",
        "extra_headers",
        "extra_query",
        "extra_body",
        "timeout",
    }
)

# --------------------------------------------------------------------------- #
# 参数过滤函数
# --------------------------------------------------------------------------- #


def strip_unsupported_params(body: dict, supported_params: set[str]) -> dict:
    """通用参数剥离：仅保留 ``supported_params`` 中列出的键。

    Args:
        body: 原始请求体字典。
        supported_params: 允许保留的键集合。

    Returns:
        过滤后的新字典。原字典不会被修改。
    """
    if not isinstance(body, dict):
        return {}
    return {k: v for k, v in body.items() if k in supported_params}


def filter_chat_completion_params(body: dict, strict: bool = False) -> dict:
    """过滤出 OpenAI Chat Completions API 接受的参数。

    当 ``strict=True`` 时，仅保留 ``OPENAI_CHAT_COMPLETIONS_PARAMS`` 白名单中的键；
    当 ``strict=False`` 时，额外允许 ``OPENAI_UNDOCUMENTED_PARAMS`` 中的键透传。

    Args:
        body: 原始请求体字典。
        strict: 是否启用严格模式（默认关闭以兼容更多上游）。

    Returns:
        过滤后的参数字典。
    """
    allowed = set(OPENAI_CHAT_COMPLETIONS_PARAMS)
    if not strict:
        allowed |= OPENAI_UNDOCUMENTED_PARAMS
    return strip_unsupported_params(body, allowed)


def filter_responses_api_params(body: dict, strict: bool = False) -> dict:
    """过滤出 OpenAI Responses API 接受的参数。

    当 ``strict=True`` 时，仅保留 ``OPENAI_RESPONSES_API_PARAMS`` 白名单中的键；
    当 ``strict=False`` 时，额外允许 ``OPENAI_UNDOCUMENTED_PARAMS`` 中的键透传。

    Args:
        body: 原始请求体字典。
        strict: 是否启用严格模式。

    Returns:
        过滤后的参数字典。
    """
    allowed = set(OPENAI_RESPONSES_API_PARAMS)
    if not strict:
        allowed |= OPENAI_UNDOCUMENTED_PARAMS
    return strip_unsupported_params(body, allowed)


# --------------------------------------------------------------------------- #
# OpenAI 特定参数处理
# --------------------------------------------------------------------------- #


def handle_openai_store_param(body: dict) -> dict:
    """处理 ``store`` 参数。

    OpenAI 的 ``store`` 参数控制是否将请求/响应存储到服务器端日志中。
    本函数仅做校验：若值为布尔值则保留，否则移除。

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    if "store" in body and not isinstance(body["store"], bool):
        del body["store"]
    return body


def handle_openai_metadata_param(body: dict) -> dict:
    """处理 ``metadata`` 参数。

    OpenAI 限制 ``metadata`` 必须是一个最多包含 16 个键的字典，
    且键长度不超过 64 字符，值长度不超过 512 字符，值必须为 ``str`` 类型。

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    metadata = body.get("metadata")
    if not isinstance(metadata, dict):
        if "metadata" in body:
            del body["metadata"]
        return body

    cleaned: dict[str, str] = {}
    for idx, (k, v) in enumerate(metadata.items()):
        if idx >= 16:
            break
        key = str(k)[:64]
        if isinstance(v, str):
            cleaned[key] = v[:512]
        else:
            # 尝试序列化为字符串
            try:
                cleaned[key] = json.dumps(v, ensure_ascii=False)[:512]
            except (TypeError, ValueError):
                cleaned[key] = str(v)[:512]

    if cleaned:
        body["metadata"] = cleaned
    else:
        del body["metadata"]
    return body


def handle_openai_prediction_param(body: dict) -> dict:
    """处理 ``prediction`` 参数（内容预测 / Prompt Caching 预热）。

    OpenAI 的 ``prediction`` 参数格式为：
    ``{"type": "content", "content": "..."}``

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    prediction = body.get("prediction")
    if not isinstance(prediction, dict):
        if "prediction" in body:
            del body["prediction"]
        return body

    ptype = prediction.get("type")
    if ptype != "content":
        # 未知类型：若包含 content 字段则降级为 content 类型使用
        if "content" in prediction:
            body["prediction"] = {
                "type": "content",
                "content": str(prediction["content"]),
            }
        else:
            del body["prediction"]
        return body

    content = prediction.get("content")
    if not isinstance(content, str):
        try:
            content = json.dumps(content, ensure_ascii=False)
        except (TypeError, ValueError):
            content = str(content)

    body["prediction"] = {"type": "content", "content": content}
    return body


def handle_openai_service_tier(body: dict) -> dict:
    """处理 ``service_tier`` 参数。

    支持的值：``"auto"``、``"default"``、``"flex"``、``"scale"``、
    ``"priority"`` 等。非字符串或空值将被移除。

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    tier = body.get("service_tier")
    if tier is not None and not isinstance(tier, str):
        del body["service_tier"]
    elif isinstance(tier, str) and not tier.strip():
        del body["service_tier"]
    return body


def handle_openai_modalities(body: dict) -> dict:
    """处理 ``modalities`` 参数。

    支持的值列表元素应为 ``"text"`` 或 ``"audio"``。
    非法值将被过滤；若过滤后为空列表则移除该参数。

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    modalities = body.get("modalities")
    if modalities is None:
        return body
    if not isinstance(modalities, list):
        del body["modalities"]
        return body

    allowed = {"text", "audio", "image"}
    cleaned = [m for m in modalities if isinstance(m, str) and m in allowed]
    if cleaned:
        body["modalities"] = cleaned
    else:
        del body["modalities"]
    return body


def handle_openai_audio_params(body: dict) -> dict:
    """处理 ``audio`` 参数。

    ``audio`` 参数应为字典，通常包含 ``voice``、``format`` 等字段。
    本函数仅做基础校验，保留字典格式。

    Args:
        body: 原始请求体字典（会被原地修改）。

    Returns:
        处理后的 ``body`` 字典。
    """
    audio = body.get("audio")
    if audio is None:
        return body
    if not isinstance(audio, dict):
        del body["audio"]
        return body
    # 保留原始字典，但确保至少包含 voice 字段提示（不做强制修改）
    return body


# --------------------------------------------------------------------------- #
# Response Format 完整处理
# --------------------------------------------------------------------------- #


def build_response_format(text_config: dict | None) -> dict | None:
    """从 Responses API 的 ``text.format`` 构建 Chat Completions 的 ``response_format``。

    支持：
    - ``json_schema`` → 标准 OpenAI json_schema 结构
    - ``json_object`` → ``{"type": "json_object"}``
    - ``text`` → ``None``（无需强制格式）
    - ``text.allowed_engines`` 等扩展字段会被忽略（Chat Completions 不支持）

    Args:
        text_config: ``text`` 参数字典，通常来自请求体的 ``text`` 字段。

    Returns:
        Chat Completions 可用的 ``response_format`` 字典，或 ``None``。
    """
    return _transform_text_format_to_response_format(text_config)


def parse_response_format(response_format: dict | None) -> dict:
    """将 Chat Completions 的 ``response_format`` 反向解析为 Responses API 的 ``text.format``。

    支持：
    - ``json_schema`` → ``{"type": "json_schema", "schema": ..., "name": ..., "strict": ...}``
    - ``json_object`` → ``{"type": "json_object"}``
    - ``text`` 或 ``None`` → ``{"type": "text"}``

    Args:
        response_format: Chat Completions 的 ``response_format`` 字典或 ``None``。

    Returns:
        Responses API 的 ``text.format`` 字典。
    """
    if not response_format or not isinstance(response_format, dict):
        return {"type": "text"}

    fmt_type = response_format.get("type")
    if fmt_type == "json_schema":
        json_schema = response_format.get("json_schema", {})
        if not isinstance(json_schema, dict):
            json_schema = {}
        return {
            "type": "json_schema",
            "name": json_schema.get("name", "response_schema"),
            "schema": json_schema.get("schema", {}),
            "strict": json_schema.get("strict", False),
        }
    if fmt_type == "json_object":
        return {"type": "json_object"}
    if fmt_type == "text":
        return {"type": "text"}

    # 未知类型：若包含 schema 则尝试按 json_schema 处理
    if "schema" in response_format:
        return {
            "type": "json_schema",
            "name": response_format.get("name", "response_schema"),
            "schema": response_format["schema"],
            "strict": response_format.get("strict", False),
        }

    return {"type": "text"}


# --------------------------------------------------------------------------- #
# Reasoning 参数完整处理
# --------------------------------------------------------------------------- #


_CHAT_REASONING_EFFORT_ALLOWED = {"minimal", "low", "medium", "high"}
_CHAT_REASONING_EFFORT_REMAP = {
    "xhigh": "high",   # Codex CLI 专用；Chat 端只到 high
    "max": "high",
    "highest": "high",
    "none": None,      # Kimi 等上游会拒，直接 drop
    "off": None,
    "auto": None,
}


def _normalize_chat_reasoning_effort(value: Any) -> Any:
    """把任意 effort 字符串归一化为 Chat 端可接受的值，未知值返回 None 以便调用方丢弃。"""
    if not isinstance(value, str):
        return value
    lowered = value.strip().lower()
    if lowered in _CHAT_REASONING_EFFORT_ALLOWED:
        return lowered
    if lowered in _CHAT_REASONING_EFFORT_REMAP:
        return _CHAT_REASONING_EFFORT_REMAP[lowered]
    return None


def build_reasoning_effort(reasoning_config: dict | str | None) -> Any:
    """从 Responses API 的 ``reasoning`` 配置构建 Chat Completions 的 ``reasoning_effort``。

    处理规则：
    - ``str`` → 归一化到 Chat 允许的 ``minimal/low/medium/high`` 之一，
      未知或不被普遍接受的值（``xhigh``/``none``/``auto`` 等）会被映射或丢弃
    - ``dict`` 含 ``effort`` 键 → 提取 ``effort`` 后做同样归一化
    - ``dict`` 含 ``summary`` 键 → 返回完整字典（含 ``summary`` 的 provider 特定格式）
    - ``None`` → ``None``

    Args:
        reasoning_config: Responses API 请求中的 ``reasoning`` 字段。

    Returns:
        Chat Completions 可接受的 ``reasoning_effort`` 值（字符串、字典或 ``None``）。
    """
    if reasoning_config is None:
        return None
    if isinstance(reasoning_config, str):
        return _normalize_chat_reasoning_effort(reasoning_config)
    if isinstance(reasoning_config, dict):
        if "effort" in reasoning_config:
            return _normalize_chat_reasoning_effort(reasoning_config["effort"])
        if "summary" in reasoning_config:
            # 保留完整字典以便上游支持 summary 的 provider 使用
            return reasoning_config
    return reasoning_config


def parse_reasoning_effort(reasoning_effort: Any) -> dict:
    """将 Chat Completions 的 ``reasoning_effort`` 反向解析为 Responses API 的 ``reasoning`` 配置。

    处理规则：
    - ``str``（如 ``"medium"``）→ ``{"effort": "medium"}``
    - ``dict`` 含 ``summary`` 键 → 原样返回（保留 ``summary`` 字段）
    - ``dict`` 不含 ``summary`` 但含 ``effort`` → 原样返回
    - ``None`` → ``{}``

    Args:
        reasoning_effort: Chat Completions 响应中的 ``reasoning_effort`` 值。

    Returns:
        Responses API 的 ``reasoning`` 字典。
    """
    if reasoning_effort is None:
        return {}
    if isinstance(reasoning_effort, str):
        return {"effort": reasoning_effort}
    if isinstance(reasoning_effort, dict):
        # 若已经是带 summary 的完整格式，直接透传
        if "summary" in reasoning_effort:
            return reasoning_effort
        if "effort" in reasoning_effort:
            return reasoning_effort
        # 兜底：将字典整体作为 effort 值（极少见）
        return {"effort": reasoning_effort}
    return {"effort": str(reasoning_effort)}


# --------------------------------------------------------------------------- #
# Tool 配置完整处理
# --------------------------------------------------------------------------- #


def build_tool_config(tools: list[dict] | None) -> list[dict] | None:
    """将 Responses API 的 ``tools`` 列表构建为 Chat Completions 的 ``tools`` 列表。

    处理规则：
    - ``type: "function"`` 的 tool → 转换为 OpenAI Chat Completions 标准格式
      （含 ``function.name``、``function.description``、``function.parameters``、
      ``function.strict``）
    - ``type: "mcp"`` 的 tool → 尝试将 ``inputSchema`` 标准化为 ``parameters``
    - 其他类型 → 尽可能透传

    Args:
        tools: Responses API 请求中的 ``tools`` 列表。

    Returns:
        Chat Completions 可用的 ``tools`` 列表，或 ``None``。
    """
    if not tools or not isinstance(tools, list):
        return None

    chat_tools: list[dict] = []
    for tool in tools:
        if not isinstance(tool, dict):
            continue
        tool_type = tool.get("type")

        if tool_type == "function":
            parameters = dict(tool.get("parameters", {}) or {})
            if not parameters.get("type"):
                parameters["type"] = "object"
            chat_tools.append(
                {
                    "type": "function",
                    "function": {
                        "name": tool.get("name", ""),
                        "description": tool.get("description", ""),
                        "parameters": parameters,
                        "strict": tool.get("strict", False),
                    },
                }
            )
        elif tool_type == "mcp":
            # MCP tool：尝试提取 server_label 作为 name，inputSchema 标准化为 parameters
            name = tool.get("server_label", "mcp_tool")
            schema = tool.get("inputSchema") or tool.get("parameters")
            parameters = _normalize_mcp_input_schema(schema)
            chat_tools.append(
                {
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": tool.get("server_description", ""),
                        "parameters": parameters,
                        "strict": False,
                    },
                }
            )
        else:
            # 非 function 类型（如 file_search、web_search、computer_use_preview 等）
            # Chat Completions 不支持，直接透传（上游 provider 可能支持）
            chat_tools.append(tool)

    return chat_tools if chat_tools else None


def build_tool_choice(tool_choice: Any) -> Any:
    """构建 Chat Completions 的 ``tool_choice``。

    本质上是 ``backend.adapter_utils._normalize_tool_choice`` 的别名，
    但额外处理 Responses API 中的 ``{"type": "function", "function": {"name": "..."}}``
    和 ``{"type": "mcp", "server_label": "..."}`` 格式。

    Args:
        tool_choice: 原始 ``tool_choice`` 值。

    Returns:
        规范化后的 ``tool_choice``。
    """
    if isinstance(tool_choice, dict):
        tc_type = tool_choice.get("type")
        if tc_type == "mcp":
            # MCP tool_choice 降级为 required（Chat Completions 无原生 MCP 支持）
            return "required"
        # function 含 name 的标准格式已在 _normalize_tool_choice 中处理
    return _normalize_tool_choice(tool_choice)


def parse_tool_config(chat_tools: list[dict] | None) -> list[dict]:
    """将 Chat Completions 的 ``tools`` 列表反向解析为 Responses API 的 ``tools`` 列表。

    处理规则：
    - ``type: "function"`` 的 tool → 转换为 Responses API 的 ``{"type": "function", ...}``
    - 其他类型 → 原样透传

    Args:
        chat_tools: Chat Completions 响应中的 ``tools`` 列表。

    Returns:
        Responses API 可用的 ``tools`` 列表。
    """
    if not chat_tools or not isinstance(chat_tools, list):
        return []

    parsed: list[dict] = []
    for tool in chat_tools:
        if not isinstance(tool, dict):
            continue
        tool_type = tool.get("type")
        if tool_type == "function":
            fn = tool.get("function", {}) or {}
            if not isinstance(fn, dict):
                fn = {}
            parsed.append(
                {
                    "type": "function",
                    "name": fn.get("name", ""),
                    "description": fn.get("description", ""),
                    "parameters": fn.get("parameters", {}),
                    "strict": fn.get("strict", False),
                }
            )
        else:
            parsed.append(tool)
    return parsed


def parse_tool_choice(chat_tool_choice: Any) -> Any:
    """将 Chat Completions 的 ``tool_choice`` 反向解析为 Responses API 的 ``tool_choice``。

    处理规则：
    - ``"auto"`` / ``"none"`` / ``"required"`` → 直接透传字符串
    - ``{"type": "function", "function": {"name": "..."}}`` → 原样透传字典
    - ``None`` → ``"auto"``（Responses API 默认值）

    Args:
        chat_tool_choice: Chat Completions 的 ``tool_choice`` 值。

    Returns:
        Responses API 可用的 ``tool_choice`` 值。
    """
    if chat_tool_choice is None:
        return "auto"
    if isinstance(chat_tool_choice, str):
        return chat_tool_choice
    if isinstance(chat_tool_choice, dict):
        # 若含 function.name 则保留完整字典
        if chat_tool_choice.get("function") and isinstance(
            chat_tool_choice["function"], dict
        ):
            if chat_tool_choice["function"].get("name"):
                return chat_tool_choice
        # 简写格式转换回字符串
        tc_type = chat_tool_choice.get("type")
        if tc_type == "auto":
            return "auto"
        if tc_type == "none":
            return "none"
        if tc_type in ("required", "tool", "any"):
            return "required"
    return chat_tool_choice


# --------------------------------------------------------------------------- #
# Message 转换完整处理
# --------------------------------------------------------------------------- #


def _transform_input_item_to_message(item: dict) -> list[dict]:
    """将单个 Responses API input item 转换为一个或多个 Chat Completion messages。

    这是 ``backend.api_adapters`` 中同名函数的增强版本，支持更多 input item 类型。
    """
    item_type = item.get("type")

    if item_type == "message":
        role = item.get("role", "user")
        content = item.get("content", "")
        return [{"role": role, "content": content}]

    if item_type == "function_call":
        call_id = item.get("call_id") or item.get("id", "")
        name = item.get("name", "")
        arguments = item.get("arguments", "")
        return [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }
                ],
            }
        ]

    if item_type == "function_call_output":
        call_id = item.get("call_id") or item.get("id", "")
        output = item.get("output", "")
        if not isinstance(output, str):
            output = json.dumps(output, ensure_ascii=False)
        return [{"role": "tool", "tool_call_id": call_id, "content": output}]

    if item_type == "input_image":
        image_url = item.get("image_url") or item.get("url", "")
        detail = item.get("detail", "auto")
        return [
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {"url": image_url, "detail": detail},
                    }
                ],
            }
        ]

    if item_type == "input_file":
        file_id = item.get("file_id") or item.get("id", "")
        # Chat Completions 无原生 file block，降级为文本提示
        return [{"role": "user", "content": f"[File: {file_id}]"}]

    if item_type == "input_audio":
        data = item.get("data", "")
        fmt = item.get("format", "wav")
        return [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_audio",
                        "input_audio": {"data": data, "format": fmt},
                    }
                ],
            }
        ]

    if item_type == "reasoning":
        # reasoning item 在 Chat Completions 中无原生对应，降级为 system 消息
        summaries = item.get("summary", [])
        text_parts: list[str] = []
        for s in summaries:
            if isinstance(s, dict) and s.get("text"):
                text_parts.append(s["text"])
        if text_parts:
            return [{"role": "system", "content": "[Reasoning] " + "\n".join(text_parts)}]
        return []

    if item_type in (
        "file_search_call",
        "web_search_call",
        "computer_call",
        "code_interpreter_call",
        "image_generation_call",
    ):
        # 工具调用输出在 input 历史中出现时，降级为文本提示
        return [{"role": "user", "content": f"[{item_type}]"}]

    # 未知类型：透传 content
    content = item.get("content")
    if content is not None:
        role = item.get("role", "user")
        return [{"role": role, "content": content}]

    return []


def build_messages(body: dict) -> list[dict]:
    """从 Responses API 请求体构建 Chat Completions 的 ``messages`` 列表。

    处理规则：
    - ``instructions`` → ``system`` 消息
    - ``input: str`` → ``user`` 消息
    - ``input: list[dict]`` → 逐个转换为 message（支持合并连续 assistant message）

    Args:
        body: Responses API 请求体字典。

    Returns:
        Chat Completions 可用的 ``messages`` 列表。
    """
    messages: list[dict] = []

    # instructions -> system message
    instructions = body.get("instructions")
    if instructions:
        if isinstance(instructions, str):
            messages.append({"role": "system", "content": instructions})
        else:
            messages.append({"role": "system", "content": str(instructions)})

    # input -> messages
    input_param = body.get("input")
    if isinstance(input_param, str):
        messages.append({"role": "user", "content": input_param})
    elif isinstance(input_param, list):
        for item in input_param:
            if isinstance(item, dict):
                item_messages = _transform_input_item_to_message(item)
                for msg in item_messages:
                    # 合并连续 assistant message（含 tool_calls）
                    if (
                        msg.get("role") == "assistant"
                        and messages
                        and messages[-1].get("role") == "assistant"
                        and msg.get("tool_calls")
                    ):
                        existing = messages[-1].setdefault("tool_calls", [])
                        if isinstance(existing, list):
                            existing.extend(msg["tool_calls"])
                        if not messages[-1].get("content"):
                            messages[-1]["content"] = ""
                    else:
                        messages.append(msg)

    return messages


def parse_messages(messages: list[dict]) -> list[dict]:
    """将 Chat Completions 的 ``messages`` 列表解析为 Responses API 的 ``input`` items。

    处理规则：
    - ``system`` / ``developer`` → ``{"type": "message", "role": "system", ...}``
    - ``user`` → ``{"type": "message", "role": "user", ...}``
    - ``assistant``（不含 tool_calls）→ ``{"type": "message", "role": "assistant", ...}``
    - ``assistant``（含 tool_calls）→ assistant message + function_call items
    - ``tool`` → ``{"type": "function_call_output", ...}``
    - ``function``（legacy）→ ``{"type": "function_call_output", ...}``

    Args:
        messages: Chat Completions 的 ``messages`` 列表。

    Returns:
        Responses API 的 ``input`` items 列表。
    """
    if not messages or not isinstance(messages, list):
        return []

    input_items: list[dict] = []

    for msg in messages:
        if not isinstance(msg, dict):
            continue
        role = msg.get("role", "")
        content = msg.get("content", "")

        if role in ("system", "developer"):
            input_items.append(
                {
                    "type": "message",
                    "role": "system",
                    "content": content or "",
                }
            )
            continue

        if role == "user":
            input_items.append(
                {
                    "type": "message",
                    "role": "user",
                    "content": content or "",
                }
            )
            continue

        if role == "assistant":
            tool_calls = msg.get("tool_calls")
            if tool_calls and isinstance(tool_calls, list):
                # assistant message 本身（内容可能为空）
                input_items.append(
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": content or "",
                    }
                )
                # 每个 tool_call 转为 function_call item
                for tc in tool_calls:
                    if isinstance(tc, dict):
                        fn = tc.get("function", {}) or {}
                        if not isinstance(fn, dict):
                            fn = {}
                        input_items.append(
                            {
                                "type": "function_call",
                                "id": tc.get("id", ""),
                                "call_id": tc.get("id", ""),
                                "name": fn.get("name", ""),
                                "arguments": fn.get("arguments", ""),
                            }
                        )
            else:
                input_items.append(
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": content or "",
                    }
                )
            continue

        if role == "tool":
            tool_call_id = msg.get("tool_call_id", "")
            tool_content = content or ""
            if not isinstance(tool_content, str):
                tool_content = json.dumps(tool_content, ensure_ascii=False)
            input_items.append(
                {
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": tool_content,
                }
            )
            continue

        if role == "function":
            # legacy function role
            input_items.append(
                {
                    "type": "function_call_output",
                    "call_id": msg.get("tool_call_id", msg.get("name", "")),
                    "output": str(content or ""),
                }
            )
            continue

        # 未知 role：透传为 message
        input_items.append(
            {
                "type": "message",
                "role": role or "user",
                "content": content or "",
            }
        )

    return input_items


# --------------------------------------------------------------------------- #
# 统一构建/解析入口
# --------------------------------------------------------------------------- #


def build_openai_chat_body(responses_body: dict) -> dict:
    """从 Responses API 请求构建完整的 OpenAI Chat Completions 请求。

    本函数整合了参数过滤、字段转换、特殊参数处理等全部能力：
    1. 提取 ``model``、``messages``（含 instructions → system、input → user/assistant/tool）
    2. 转换 ``tools`` / ``tool_choice``
    3. 转换 ``text.format`` → ``response_format``
    4. 转换 ``reasoning`` → ``reasoning_effort``
    5. 映射 ``max_output_tokens`` → ``max_tokens``
    6. 处理 ``store``、``metadata``、``prediction``、``service_tier``、
       ``modalities``、``audio``
    7. 透传 ``temperature``、``top_p``、``user``、``parallel_tool_calls`` 等标准字段
    8. 强制设置 ``stream_options: {"include_usage": True}`` 当 ``stream=True`` 时

    Args:
        responses_body: Responses API 请求体字典。

    Returns:
        OpenAI Chat Completions 请求体字典。
    """
    if not isinstance(responses_body, dict):
        responses_body = {}

    # 深拷贝，避免修改原始请求体
    body = copy.deepcopy(responses_body)

    result: dict[str, Any] = {}

    # model
    if "model" in body:
        result["model"] = body["model"]

    # messages
    result["messages"] = build_messages(body)

    # tools
    tools = build_tool_config(body.get("tools"))
    if tools:
        result["tools"] = tools

    # tool_choice
    tool_choice = body.get("tool_choice")
    if tool_choice is not None:
        result["tool_choice"] = build_tool_choice(tool_choice)

    # text.format -> response_format
    text = body.get("text")
    if isinstance(text, dict):
        response_format = build_response_format(text)
        if response_format:
            result["response_format"] = response_format

    # reasoning -> reasoning_effort
    reasoning = body.get("reasoning")
    if reasoning is not None:
        reasoning_effort = build_reasoning_effort(reasoning)
        if reasoning_effort is not None:
            result["reasoning_effort"] = reasoning_effort

    # max_output_tokens -> max_tokens
    if "max_output_tokens" in body:
        result["max_tokens"] = body["max_output_tokens"]

    # 处理特殊参数
    handle_openai_store_param(body)
    handle_openai_metadata_param(body)
    handle_openai_prediction_param(body)
    handle_openai_service_tier(body)
    handle_openai_modalities(body)
    handle_openai_audio_params(body)

    # 透传特殊参数（若在 body 中且未被移除）
    for key in (
        "store",
        "metadata",
        "prediction",
        "service_tier",
        "modalities",
        "audio",
    ):
        if key in body:
            result[key] = body[key]

    # 透传标准字段
    for key in (
        "temperature",
        "top_p",
        "user",
        "parallel_tool_calls",
        "seed",
        "stop",
        "frequency_penalty",
        "presence_penalty",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "n",
        "response_format",  # 允许直接透传（兼容层）
        "reasoning_effort",  # 允许直接透传
    ):
        if key in body and key not in result:
            result[key] = body[key]

    # stream
    stream = bool(body.get("stream", False))
    result["stream"] = stream

    # stream_options: include_usage for usage in streaming
    if stream:
        result["stream_options"] = {"include_usage": True}

    # 过滤参数，确保仅发送 Chat Completions 可接受的字段
    return filter_chat_completion_params(result, strict=False)


def parse_openai_chat_response(chat_response: dict, model: str) -> dict:
    """将 OpenAI Chat Completions 响应解析为 Responses API 响应。

    处理规则：
    1. 提取 ``choices[0].message`` 中的 ``content``、``tool_calls``、
       ``reasoning_content``
    2. 将 ``content`` 转为 ``output_text`` item
    3. 将 ``reasoning_content`` 转为 ``reasoning`` item
    4. 将 ``tool_calls`` 转为 ``function_call`` items
    5. 归一化 ``usage`` 字段
    6. 生成 ``created_at`` ISO 时间戳
    7. 根据 ``finish_reason`` 判断 ``status``（``completed`` / ``incomplete``）

    Args:
        chat_response: OpenAI Chat Completions 响应字典。
        model: 模型名称（用于填充 ``model`` 字段）。

    Returns:
        Responses API 响应字典。
    """
    if not isinstance(chat_response, dict):
        return {"error": {"message": "Invalid upstream response"}}

    choices = chat_response.get("choices", [])
    if not choices or not isinstance(choices, list):
        return {"error": {"message": "No choices in upstream response"}}

    choice = choices[0]
    if not isinstance(choice, dict):
        return {"error": {"message": "Invalid choice in upstream response"}}

    message = choice.get("message", {}) or {}
    if not isinstance(message, dict):
        message = {}

    finish_reason = choice.get("finish_reason")
    status = "incomplete" if finish_reason == "length" else "completed"

    output: list[dict] = []

    # reasoning_content -> reasoning item
    reasoning_content = message.get("reasoning_content")
    if reasoning_content:
        output.append(
            {
                "type": "reasoning",
                "id": f"rs_{uuid.uuid4().hex[:12]}",
                "status": status,
                "summary": [
                    {
                        "type": "reasoning_summary_text",
                        "text": reasoning_content,
                    }
                ],
            }
        )

    # content -> message item
    content = message.get("content", "")
    content_blocks: list[dict] = []
    if content:
        content_blocks.append(
            {
                "type": "output_text",
                "text": content,
                "annotations": [],
            }
        )

    # tool_calls -> function_call items
    tool_calls = message.get("tool_calls")
    if tool_calls and isinstance(tool_calls, list):
        for tc in tool_calls:
            if isinstance(tc, dict):
                fn = tc.get("function", {}) or {}
                if not isinstance(fn, dict):
                    fn = {}
                output.append(
                    {
                        "type": "function_call",
                        "id": tc.get("id", f"fc_{uuid.uuid4().hex[:12]}"),
                        "call_id": tc.get("id", f"fc_{uuid.uuid4().hex[:12]}"),
                        "name": fn.get("name", ""),
                        "arguments": fn.get("arguments", ""),
                        "status": status,
                    }
                )

    # 添加 message item
    output.append(
        {
            "type": "message",
            "id": f"msg_{uuid.uuid4().hex[:12]}",
            "status": status,
            "role": "assistant",
            "content": content_blocks,
        }
    )

    # usage
    usage = _transform_usage_details(chat_response.get("usage"))

    # created_at
    created = chat_response.get("created")
    if isinstance(created, (int, float)):
        created_at = datetime.fromtimestamp(created, tz=timezone.utc).isoformat()
    else:
        created_at = datetime.now(timezone.utc).isoformat()

    # response_format / text 反向解析（若原始请求有 response_format）
    text_config = None
    resp_fmt = chat_response.get("response_format")
    if resp_fmt:
        text_config = parse_response_format(resp_fmt)

    result: dict[str, Any] = {
        "id": chat_response.get("id", f"resp_{uuid.uuid4().hex[:12]}"),
        "object": "response",
        "created_at": created_at,
        "model": chat_response.get("model", model),
        "status": status,
        "output": output,
        "usage": usage,
        "system_fingerprint": chat_response.get("system_fingerprint"),
    }

    if text_config:
        result["text"] = {"format": text_config}

    # 反向解析 reasoning_effort -> reasoning
    reasoning_effort = chat_response.get("reasoning_effort")
    if reasoning_effort is not None:
        result["reasoning"] = parse_reasoning_effort(reasoning_effort)

    return result


# --------------------------------------------------------------------------- #
# 额外辅助函数
# --------------------------------------------------------------------------- #


def sanitize_body_for_logging(body: dict) -> dict:
    """清洗请求体中可能包含敏感信息的字段，用于日志记录。

    移除/脱敏的字段：
    - ``input`` 中的 base64 图片/音频数据（替换为 ``"<base64_redacted>"``）
    - ``audio`` 中的敏感 token

    Args:
        body: 原始请求体字典。

    Returns:
        清洗后的字典（不修改原字典）。
    """
    if not isinstance(body, dict):
        return {}

    sanitized = copy.deepcopy(body)
    input_param = sanitized.get("input")
    if isinstance(input_param, list):
        for item in input_param:
            if isinstance(item, dict):
                # 脱敏 input_audio data
                if item.get("type") == "input_audio":
                    input_audio = item.get("input_audio", {})
                    if isinstance(input_audio, dict) and "data" in input_audio:
                        input_audio["data"] = "<base64_redacted>"
                # 脱敏 image_url
                if item.get("type") == "input_image":
                    image_url = item.get("image_url", "")
                    if isinstance(image_url, str) and image_url.startswith("data:"):
                        item["image_url"] = "<base64_redacted>"
    return sanitized


def estimate_token_count(text: str | None) -> int:
    """粗略估计文本的 token 数量（按 1 token ≈ 4 字符计算）。

    Args:
        text: 输入文本或 ``None``。

    Returns:
        估计的 token 数量。
    """
    if not text:
        return 0
    # 简单启发式：英文约 4 字符/token，中文约 1.5 字符/token
    # 这里使用保守估计 3.5 字符/token
    return max(1, int(len(text) / 3.5))


def validate_messages(messages: list[dict]) -> list[str]:
    """校验 Chat Completions message 列表的基本有效性。

    检查项：
    - 是否为空列表
    - 每个 message 是否包含 ``role`` 字段
    - ``role`` 是否为已知值
    - ``tool`` role 是否包含 ``tool_call_id``

    Args:
        messages: Chat Completions message 列表。

    Returns:
        错误信息列表（为空表示校验通过）。
    """
    errors: list[str] = []
    if not messages:
        errors.append("messages list is empty")
        return errors

    known_roles = {"system", "user", "assistant", "tool", "function", "developer"}
    for idx, msg in enumerate(messages):
        if not isinstance(msg, dict):
            errors.append(f"message[{idx}] is not a dict")
            continue
        role = msg.get("role")
        if role is None:
            errors.append(f"message[{idx}] missing 'role'")
            continue
        if role not in known_roles:
            errors.append(f"message[{idx}] has unknown role '{role}'")
        if role == "tool" and not msg.get("tool_call_id"):
            errors.append(f"message[{idx}] (role=tool) missing 'tool_call_id'")

    return errors
