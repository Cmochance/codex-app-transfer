"""通用转换规则基类。

为 Responses API ↔ Chat Completions API 的转换提供可复用的基础函数，
被 ``responses_adapter.py``、``openai_adapter.py`` 等上层模块导入使用。
"""

from __future__ import annotations

import copy
from typing import Any

# --------------------------------------------------------------------------- #
# 角色转换
# --------------------------------------------------------------------------- #


def convert_developer_to_system(messages: list[dict]) -> list[dict]:
    """将 ``role="developer"`` 转换为 ``role="system"``。

    Chat Completions API 不支持 ``developer`` 角色，而 Responses API
    使用 ``developer`` 替代 ``system``。本函数在向下游 Chat Completions
     provider 发送请求前调用。

    Args:
        messages: 原始消息列表，每个元素为 ``{"role": ..., "content": ...}`` 字典。

    Returns:
        角色转换后的新消息列表（深拷贝，不修改原列表）。
    """
    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        new_msg = dict(msg)
        if new_msg.get("role") == "developer":
            new_msg["role"] = "system"
        result.append(new_msg)
    return result


def convert_system_to_developer(messages: list[dict]) -> list[dict]:
    """将 ``role="system"`` 转换为 ``role="developer"``。

    Responses API 使用 ``developer`` 角色而非 ``system``。本函数在将上游
    Chat Completions 响应转换回 Responses API 格式时，对消息历史做反向映射。

    Args:
        messages: 原始消息列表。

    Returns:
        角色转换后的新消息列表（深拷贝，不修改原列表）。
    """
    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        new_msg = dict(msg)
        if new_msg.get("role") == "system":
            new_msg["role"] = "developer"
        result.append(new_msg)
    return result


# --------------------------------------------------------------------------- #
# 通用内容块转换
# --------------------------------------------------------------------------- #


def normalize_content_block(block: Any) -> dict:
    """将任意内容块规范化为标准 ``{"type": "...", ...}`` 字典。

    支持以下输入：
    - ``str`` → ``{"type": "text", "text": <str>}``
    - ``dict`` 且含 ``type`` → 原样返回（浅拷贝）
    - ``dict`` 不含 ``type`` 但含 ``text`` → 补 ``type: text``
    - ``dict`` 不含 ``type`` 但含 ``image_url`` → 补 ``type: image_url``
    - ``dict`` 不含 ``type`` 但含 ``input_audio`` → 补 ``type: input_audio``
    - 其他 → 包装为 ``{"type": "text", "text": str(block)}``

    Args:
        block: 任意内容块。

    Returns:
        标准化的内容块字典。
    """
    if isinstance(block, str):
        return {"type": "text", "text": block}

    if isinstance(block, dict):
        normalized = dict(block)
        if "type" in normalized:
            return normalized
        if "text" in normalized:
            normalized["type"] = "text"
            return normalized
        if "image_url" in normalized:
            normalized["type"] = "image_url"
            return normalized
        if "input_audio" in normalized:
            normalized["type"] = "input_audio"
            return normalized
        # 兜底：把 dict 序列化为文本
        import json

        return {"type": "text", "text": json.dumps(normalized, ensure_ascii=False)}

    return {"type": "text", "text": str(block)}


def normalize_content_array(content: Any) -> list[dict]:
    """统一将 ``content`` 处理为标准 content block 数组。

    支持以下输入：
    - ``None`` → ``[]``
    - ``str`` → ``[{"type": "text", "text": <str>}]``
    - ``list`` → 逐个调用 ``normalize_content_block``
    - ``dict`` → 包装为单元素列表

    Args:
        content: 任意 content 值。

    Returns:
        标准化的 content block 列表。
    """
    if content is None:
        return []

    if isinstance(content, str):
        return [{"type": "text", "text": content}]

    if isinstance(content, list):
        return [normalize_content_block(item) for item in content]

    if isinstance(content, dict):
        return [normalize_content_block(content)]

    return [{"type": "text", "text": str(content)}]


# --------------------------------------------------------------------------- #
# 通用参数过滤
# --------------------------------------------------------------------------- #

CHAT_COMPLETIONS_KNOWN_PARAMS: set[str] = {
    "model",
    "messages",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "max_tokens",
    "max_completion_tokens",
    "n",
    "presence_penalty",
    "response_format",
    "seed",
    "service_tier",
    "safety_identifier",
    "stop",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "function_call",
    "functions",
    "user",
    "metadata",
    "reasoning_effort",
    "prediction",
    "audio",
    "modalities",
    "store",
}

RESPONSES_API_KNOWN_PARAMS: set[str] = {
    "model",
    "input",
    "instructions",
    "max_output_tokens",
    "metadata",
    "parallel_tool_calls",
    "previous_response_id",
    "reasoning",
    "store",
    "stream",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
    "truncation",
    "user",
    "include",
    "background",
    "service_tier",
    "safety_identifier",
    "prompt",
    "max_tool_calls",
    "prompt_cache_key",
    "prompt_cache_retention",
    "stream_options",
    "top_logprobs",
    "partial_images",
    "context_management",
}


def filter_unknown_params(body: dict, known_params: set[str]) -> dict:
    """过滤掉 body 中不在已知参数集合里的键，避免上游返回 400。

    Args:
        body: 原始请求体字典。
        known_params: 允许透传的参数名集合。

    Returns:
        过滤后的新字典（浅拷贝）。
    """
    if not isinstance(body, dict):
        return {}
    return {k: v for k, v in body.items() if k in known_params}


def extract_known_params(body: dict, known_params: set[str]) -> dict:
    """从 body 中提取已知参数（语义与 ``filter_unknown_params`` 相同，便于阅读）。

    Args:
        body: 原始请求体字典。
        known_params: 已知参数名集合。

    Returns:
        仅包含已知参数的新字典。
    """
    return filter_unknown_params(body, known_params)


# --------------------------------------------------------------------------- #
# 通用消息合并
# --------------------------------------------------------------------------- #


def merge_consecutive_assistant_messages(messages: list[dict]) -> list[dict]:
    """合并连续的 assistant message（特别是含 tool_calls 的）。

    某些上游（如 Anthropic）要求 tool_use 必须紧跟在发出 tool_calls 的
    assistant message 之后；如果中间出现了空的 assistant message，可能
    导致格式错误。本函数将相邻的 ``role="assistant"`` 消息合并为一条，
    优先保留非空 ``content``，合并 ``tool_calls`` 列表。

    Args:
        messages: 原始消息列表。

    Returns:
        合并后的新消息列表（深拷贝）。
    """
    if not messages:
        return []

    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue

        role = msg.get("role")
        if role != "assistant" or not result or result[-1].get("role") != "assistant":
            result.append(copy.deepcopy(msg))
            continue

        # 合并到前一条 assistant message
        prev = result[-1]
        # 合并 content：保留非空内容
        content = msg.get("content")
        if content:
            prev_content = prev.get("content")
            if prev_content and isinstance(prev_content, str) and isinstance(content, str):
                prev["content"] = prev_content + "\n" + content
            else:
                prev["content"] = content

        # 合并 tool_calls
        tool_calls = msg.get("tool_calls")
        if tool_calls and isinstance(tool_calls, list):
            prev_tool_calls = prev.setdefault("tool_calls", [])
            if isinstance(prev_tool_calls, list):
                prev_tool_calls.extend(copy.deepcopy(tool_calls))

        # 合并其他可追加字段（保留原值 + 新值去重）
        for key in ("reasoning_content",):
            if key in msg:
                prev[key] = msg[key]

    return result


def merge_consecutive_user_messages(messages: list[dict]) -> list[dict]:
    """合并连续的用户消息。

    当 Responses API 的 input 列表展开后，可能产生多条连续的 ``role="user"``
    消息（例如图片 + 文本 + 文件）。本函数将它们合并为一条，content 转为数组
    或拼接字符串。

    Args:
        messages: 原始消息列表。

    Returns:
        合并后的新消息列表（深拷贝）。
    """
    if not messages:
        return []

    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue

        role = msg.get("role")
        if role != "user" or not result or result[-1].get("role") != "user":
            result.append(copy.deepcopy(msg))
            continue

        prev = result[-1]
        content = msg.get("content")
        prev_content = prev.get("content")

        # 如果任一 content 是 list，统一转为 list 合并
        if isinstance(prev_content, list) or isinstance(content, list):
            prev_arr = normalize_content_array(prev_content)
            curr_arr = normalize_content_array(content)
            prev["content"] = prev_arr + curr_arr
        elif isinstance(prev_content, str) and isinstance(content, str):
            prev["content"] = prev_content + "\n" + content
        elif content:
            prev["content"] = content

    return result


# --------------------------------------------------------------------------- #
# 工具格式转换
# --------------------------------------------------------------------------- #


def convert_responses_tool_to_chat_tool(tool: dict) -> dict:
    """将 Responses API 的 tool 定义转换为 Chat Completions 格式。

    Responses API tool 格式示例::

        {
            "type": "function",
            "name": "get_weather",
            "description": "...",
            "parameters": {...},
            "strict": false
        }

    Chat Completions tool 格式::

        {
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "...",
                "parameters": {...},
                "strict": false
            }
        }

    Args:
        tool: Responses API 格式的 tool 字典。

    Returns:
        Chat Completions 格式的 tool 字典。
    """
    if not isinstance(tool, dict):
        return tool

    ttype = tool.get("type")

    if ttype == "function":
        parameters = dict(tool.get("parameters", {}) or {})
        if not parameters.get("type"):
            parameters["type"] = "object"
        return {
            "type": "function",
            "function": {
                "name": tool.get("name", ""),
                "description": tool.get("description", ""),
                "parameters": parameters,
                "strict": tool.get("strict", False),
            },
        }

    if ttype == "custom":
        # Responses API 的 custom tool 无 JSON schema（用 grammar/text 自由输出），
        # Chat 端点不认。降级成接受单个字符串 input 的 function，保持 Codex 工具仍可调用。
        return {
            "type": "function",
            "function": {
                "name": tool.get("name", ""),
                "description": tool.get("description", ""),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "Free-form input passed verbatim to the tool.",
                        }
                    },
                    "required": ["input"],
                },
                "strict": False,
            },
        }

    # 其余 Responses-API 专属类型（local_shell / web_search* / file_search /
    # computer_use* / code_interpreter / image_generation / mcp / tool_search* 等）
    # Chat 端点都不认，丢弃。调用方按 falsy 过滤。
    return {}


def convert_chat_tool_to_responses_tool(tool: dict) -> dict:
    """将 Chat Completions 的 tool 定义转换为 Responses API 格式。

    反向执行 ``convert_responses_tool_to_chat_tool``。

    Args:
        tool: Chat Completions 格式的 tool 字典。

    Returns:
        Responses API 格式的 tool 字典。
    """
    if not isinstance(tool, dict):
        return tool

    fn = tool.get("function")
    if not isinstance(fn, dict):
        # 没有 function 字段则原样返回
        return dict(tool)

    result: dict[str, Any] = {
        "type": "function",
        "name": fn.get("name", ""),
        "description": fn.get("description", ""),
        "parameters": fn.get("parameters", {}),
    }
    if "strict" in fn:
        result["strict"] = fn["strict"]
    return result


def normalize_tool_parameters(parameters: dict) -> dict:
    """确保 tool ``parameters`` 字典符合 OpenAI 规范。

    OpenAI 要求 function parameters 必须包含 ``type: "object"`` 和
    ``properties``（可为空），并推荐 ``additionalProperties: false``。

    Args:
        parameters: 原始参数字典。

    Returns:
        规范化后的新字典（深拷贝）。
    """
    if not parameters or not isinstance(parameters, dict):
        return {"type": "object", "properties": {}, "additionalProperties": False}

    normalized = copy.deepcopy(parameters)

    if "type" not in normalized:
        normalized["type"] = "object"

    if "properties" not in normalized:
        normalized["properties"] = {}

    if "additionalProperties" not in normalized:
        normalized["additionalProperties"] = False

    return normalized


# --------------------------------------------------------------------------- #
# 响应格式转换
# --------------------------------------------------------------------------- #


def convert_text_format_to_response_format(text_format: dict) -> dict | None:
    """将 Responses API 的 ``text.format`` 转换为 Chat Completions 的 ``response_format``。

    支持：
    - ``json_schema`` → 标准 ``{"type": "json_schema", "json_schema": {...}}``
    - ``json_object`` → ``{"type": "json_object"}``
    - ``text`` → ``None``（无需强制格式）

    Args:
        text_format: ``text.format`` 子字典。

    Returns:
        Chat Completions 可用的 ``response_format`` 字典，或 ``None``。
    """
    if not text_format or not isinstance(text_format, dict):
        return None

    fmt_type = text_format.get("type")
    if fmt_type == "json_schema":
        return {
            "type": "json_schema",
            "json_schema": {
                "name": text_format.get("name") or "response_schema",
                "schema": text_format.get("schema") or {},
                "strict": text_format.get("strict", False),
            },
        }
    if fmt_type == "json_object":
        return {"type": "json_object"}
    if fmt_type == "text":
        return None

    # 未知类型：若包含 schema 则尝试按 json_schema 处理
    if "schema" in text_format:
        return {
            "type": "json_schema",
            "json_schema": {
                "name": text_format.get("name") or "response_schema",
                "schema": text_format.get("schema") or {},
                "strict": text_format.get("strict", False),
            },
        }

    return None


def convert_response_format_to_text_format(response_format: dict) -> dict | None:
    """将 Chat Completions 的 ``response_format`` 转换为 Responses API 的 ``text.format``。

    反向执行 ``convert_text_format_to_response_format``。

    Args:
        response_format: Chat Completions 的 ``response_format`` 字典。

    Returns:
        Responses API 可用的 ``text.format`` 子字典，或 ``None``。
    """
    if not response_format or not isinstance(response_format, dict):
        return None

    fmt_type = response_format.get("type")
    if fmt_type == "json_schema":
        json_schema = response_format.get("json_schema") or {}
        if isinstance(json_schema, dict):
            return {
                "type": "json_schema",
                "name": json_schema.get("name") or "response_schema",
                "schema": json_schema.get("schema") or {},
                "strict": json_schema.get("strict", False),
            }
        return {"type": "json_schema"}
    if fmt_type == "json_object":
        return {"type": "json_object"}
    if fmt_type == "text":
        return {"type": "text"}

    return None


# --------------------------------------------------------------------------- #
# Reasoning 参数转换
# --------------------------------------------------------------------------- #


def convert_reasoning_to_reasoning_effort(reasoning: Any) -> Any:
    """将 Responses API 的 ``reasoning`` 参数转换为 Chat Completions 的 ``reasoning_effort``。

    Responses API 格式::

        {"effort": "high", "summary": "auto"}

    Chat Completions 格式::

        "high"  # 仅保留 effort 字符串

    处理规则：
    - ``reasoning`` 为 ``dict`` 且含 ``effort`` → 返回 ``effort`` 字符串
    - ``reasoning`` 为 ``dict`` 且含 ``summary`` → 返回原 ``reasoning`` 字典
    - ``reasoning`` 为 ``str`` → 原样返回
    - 其他 → 原样返回

    Args:
        reasoning: Responses API 的 ``reasoning`` 值。

    Returns:
        Chat Completions 可用的 ``reasoning_effort`` 值。
    """
    if isinstance(reasoning, dict):
        if "effort" in reasoning:
            return reasoning["effort"]
        if "summary" in reasoning:
            return reasoning
    return reasoning


def convert_reasoning_effort_to_reasoning(reasoning_effort: Any) -> dict:
    """将 Chat Completions 的 ``reasoning_effort`` 转换为 Responses API 的 ``reasoning``。

    反向执行 ``convert_reasoning_to_reasoning_effort``。

    Args:
        reasoning_effort: Chat Completions 的 ``reasoning_effort`` 值。

    Returns:
        Responses API 格式的 ``reasoning`` 字典。
    """
    if isinstance(reasoning_effort, str):
        return {"effort": reasoning_effort}
    if isinstance(reasoning_effort, dict):
        return reasoning_effort
    return {}
