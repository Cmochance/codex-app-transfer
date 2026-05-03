"""OpenAI Chat Completions 响应 → Responses API 响应转换器。

代理网关的反向转换路径：将上游 Provider 返回的 Chat Completions
格式响应转换为客户端期望的 Responses API 格式。

本模块为 ``backend.api_adapters`` 提供底层转换能力，所有函数均**
不修改原始字典**，返回深拷贝后的新字典（或新构造的字典）。
"""

from __future__ import annotations

import copy
import json
import uuid
from datetime import datetime, timezone
from typing import Any

from backend.adapter_utils import _transform_usage_details
from backend.base_adapter import (
    convert_chat_tool_to_responses_tool,
    convert_reasoning_effort_to_reasoning,
    convert_response_format_to_text_format,
    convert_system_to_developer,
)
from backend.provider_workarounds import apply_response_workarounds
from backend.response_id_codec import encode_response_id


# --------------------------------------------------------------------------- #
# 主转换入口
# --------------------------------------------------------------------------- #


def convert_chat_to_responses(
    chat_response: dict,
    model: str,
    provider: dict | None = None,
    request_body: dict | None = None,
) -> dict:
    """将 OpenAI Chat Completions 响应转换为 Responses API 响应。

    转换流程：
    1. 对原始响应应用 ``apply_response_workarounds``（provider 修正、
       缺失字段补全等）。
    2. 提取 ``choices[0].message``，解析 ``content``、``reasoning_content``、
       ``tool_calls``、``finish_reason``。
    3. 将 ``finish_reason`` 映射为 Responses API ``status``。
    4. 构建 ``output`` items（``reasoning`` → ``message`` → ``function_call``）。
    5. 归一化 ``usage`` 字段（``_transform_usage_details``）。
    6. 生成 ``created_at`` ISO 时间戳。
    7. 编码或保留 ``response_id``。
    8. 反向解析 ``response_format`` → ``text.format``、
       ``reasoning_effort`` → ``reasoning``。

    Args:
        chat_response: 上游返回的 Chat Completions 响应字典。
        model: 模型名称（用于填充 ``model`` 字段）。
        provider: 上游 Provider 配置字典，用于应用 response workaround。
        request_body: 原始 Responses API 请求体（用于恢复 ``text.format``
            等请求上下文）。

    Returns:
        Responses API 格式的响应字典。
    """
    if not isinstance(chat_response, dict):
        return {"error": {"message": "Invalid upstream response"}}

    # 深拷贝并应用 provider workaround
    chat_resp = copy.deepcopy(chat_response)
    chat_resp = apply_response_workarounds(chat_resp, provider or {})

    choices = chat_resp.get("choices", [])
    if not choices or not isinstance(choices, list):
        return {"error": {"message": "No choices in upstream response"}}

    choice = choices[0]
    if not isinstance(choice, dict):
        return {"error": {"message": "Invalid choice in upstream response"}}

    message = choice.get("message", {}) or {}
    if not isinstance(message, dict):
        message = {}

    finish_reason = choice.get("finish_reason")
    status = _map_finish_reason_to_status(finish_reason)

    # ------------------------------------------------------------------ #
    # 构建 output items
    # ------------------------------------------------------------------ #
    output = build_output_items_from_message(message, status)

    # ------------------------------------------------------------------ #
    # usage
    # ------------------------------------------------------------------ #
    usage = _transform_usage_details(chat_resp.get("usage"))

    # ------------------------------------------------------------------ #
    # created_at
    # ------------------------------------------------------------------ #
    created_at = _normalize_created_at(chat_resp.get("created"))

    # ------------------------------------------------------------------ #
    # response_id
    # ------------------------------------------------------------------ #
    upstream_id = chat_resp.get("id")
    if upstream_id and isinstance(upstream_id, str):
        # 若上游 ID 已经是 resp_ 开头，可能已经被编码过，直接保留
        if upstream_id.startswith("resp_"):
            response_id = upstream_id
        else:
            # 编码 provider 信息
            provider_name = provider.get("name") if isinstance(provider, dict) else None
            response_id = encode_response_id(provider_name, model, upstream_id)
    else:
        provider_name = provider.get("name") if isinstance(provider, dict) else None
        raw_id = f"chatcmpl_{uuid.uuid4().hex[:16]}"
        response_id = encode_response_id(provider_name, model, raw_id)

    # ------------------------------------------------------------------ #
    # 组装 Responses API 响应
    # ------------------------------------------------------------------ #
    result: dict[str, Any] = {
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "model": chat_resp.get("model") or model,
        "status": status,
        "output": output,
        "usage": usage,
    }

    # system_fingerprint
    system_fingerprint = chat_resp.get("system_fingerprint")
    if system_fingerprint is not None:
        result["system_fingerprint"] = system_fingerprint

    # ------------------------------------------------------------------ #
    # 反向解析：从原始请求恢复 text.format
    # ------------------------------------------------------------------ #
    if request_body and isinstance(request_body, dict):
        text = request_body.get("text")
        if isinstance(text, dict) and "format" in text:
            result["text"] = {"format": text["format"]}

    # ------------------------------------------------------------------ #
    # 反向解析：response_format → text.format
    # ------------------------------------------------------------------ #
    response_format = chat_resp.get("response_format")
    if response_format is not None and isinstance(response_format, dict):
        text_format = convert_response_format_to_text_format(response_format)
        if text_format:
            result["text"] = {"format": text_format}

    # ------------------------------------------------------------------ #
    # 反向解析：reasoning_effort → reasoning
    # ------------------------------------------------------------------ #
    reasoning_effort = chat_resp.get("reasoning_effort")
    if reasoning_effort is not None:
        result["reasoning"] = convert_reasoning_effort_to_reasoning(reasoning_effort)

    # ------------------------------------------------------------------ #
    # 其他可选字段透传
    # ------------------------------------------------------------------ #
    for key in ("metadata", "user", "parallel_tool_calls"):
        if request_body and isinstance(request_body, dict) and key in request_body:
            result[key] = request_body[key]

    return result


# --------------------------------------------------------------------------- #
# Messages → Input items 反向映射
# --------------------------------------------------------------------------- #


def convert_messages_to_input_items(messages: list[dict]) -> list[dict]:
    """将 Chat Completions ``messages`` 列表转换为 Responses API ``input`` items。

    角色映射规则：

    - ``system`` / ``developer`` → ``{"type": "message", "role": "system", ...}``
    - ``user`` → ``{"type": "message", "role": "user", ...}``
      （content 可为字符串或 content block 数组）
    - ``assistant``（不含 ``tool_calls``）→
      ``{"type": "message", "role": "assistant", ...}``
    - ``assistant``（含 ``tool_calls``）→ assistant message +
      每条 tool_call 转为 ``function_call`` item
    - ``tool`` → ``{"type": "function_call_output", "call_id": ..., "output": ...}``
    - ``function``（legacy）→ ``{"type": "function_call_output", ...}``
    - 未知 role → 兜底为 ``{"type": "message", "role": <role>, ...}``

    Args:
        messages: Chat Completions 格式的 message 字典列表。

    Returns:
        Responses API 格式的 input items 列表。
    """
    if not messages or not isinstance(messages, list):
        return []

    input_items: list[dict] = []

    for msg in messages:
        if not isinstance(msg, dict):
            continue

        role = msg.get("role", "")
        content = msg.get("content", "")

        # system / developer → message (role=system)
        if role in ("system", "developer"):
            input_items.append({
                "type": "message",
                "role": "system",
                "content": content or "",
            })
            continue

        # user → message (role=user)
        if role == "user":
            input_items.append({
                "type": "message",
                "role": "user",
                "content": content or "",
            })
            continue

        # assistant → message + optional function_call items
        if role == "assistant":
            tool_calls = msg.get("tool_calls")
            if tool_calls and isinstance(tool_calls, list):
                # assistant message 本身（content 可能为空）
                input_items.append({
                    "type": "message",
                    "role": "assistant",
                    "content": content or "",
                })
                # 每个 tool_call 转为 function_call item
                for tc in tool_calls:
                    if isinstance(tc, dict):
                        fn = tc.get("function", {}) or {}
                        if not isinstance(fn, dict):
                            fn = {}
                        call_id = tc.get("id", "")
                        input_items.append({
                            "type": "function_call",
                            "id": call_id,
                            "call_id": call_id,
                            "name": fn.get("name", ""),
                            "arguments": fn.get("arguments", ""),
                        })
            else:
                input_items.append({
                    "type": "message",
                    "role": "assistant",
                    "content": content or "",
                })
            continue

        # tool → function_call_output
        if role == "tool":
            tool_call_id = msg.get("tool_call_id", "")
            tool_content = content or ""
            if not isinstance(tool_content, str):
                try:
                    tool_content = json.dumps(tool_content, ensure_ascii=False)
                except (TypeError, ValueError):
                    tool_content = str(tool_content)
            input_items.append({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": tool_content,
            })
            continue

        # function (legacy) → function_call_output
        if role == "function":
            input_items.append({
                "type": "function_call_output",
                "call_id": msg.get("tool_call_id") or msg.get("name", ""),
                "output": str(content or ""),
            })
            continue

        # 未知 role：兜底为 message
        input_items.append({
            "type": "message",
            "role": role or "user",
            "content": content or "",
        })

    return input_items


# --------------------------------------------------------------------------- #
# Output items 构建
# --------------------------------------------------------------------------- #


def build_output_items_from_message(message: dict, status: str) -> list[dict]:
    """从 Chat Completions assistant ``message`` 构建 Responses API ``output`` items。

    构建顺序（与 OpenAI 官方 Responses API 保持一致）：
    1. ``reasoning_content`` → ``reasoning`` item（若有）
    2. ``tool_calls`` → 若干 ``function_call`` items（若有）
    3. ``content`` → ``message`` item with ``output_text`` blocks（始终追加）

    Args:
        message: Chat Completions 的 assistant message 字典。
        status: 响应状态（``completed`` / ``incomplete`` / ``in_progress``）。

    Returns:
        Responses API 格式的 output items 列表。
    """
    if not isinstance(message, dict):
        return []

    output: list[dict] = []

    # 1. reasoning_content → reasoning item
    reasoning_content = message.get("reasoning_content")
    if reasoning_content and isinstance(reasoning_content, str) and reasoning_content.strip():
        output.append(_build_reasoning_item(reasoning_content, status))

    # 2. tool_calls → function_call items
    tool_calls = message.get("tool_calls")
    if tool_calls and isinstance(tool_calls, list):
        for tc in tool_calls:
            if isinstance(tc, dict):
                output.append(_build_function_call_item(tc, status))

    # 3. content → message item with output_text blocks
    content = message.get("content", "")
    content_blocks = _extract_content_blocks(content)

    output.append({
        "type": "message",
        "id": f"msg_{uuid.uuid4().hex[:12]}",
        "status": status,
        "role": "assistant",
        "content": content_blocks,
    })

    return output


# --------------------------------------------------------------------------- #
# 辅助函数
# --------------------------------------------------------------------------- #


def _extract_content_blocks(content: Any) -> list[dict]:
    """将 message ``content`` 提取为 Responses API 的 content blocks。

    支持格式：
    - ``None`` → ``[]``
    - ``str``（非空）→ ``[{"type": "output_text", "text": <str>, "annotations": []}]``
    - ``list[dict]`` → 逐个规范化：
      - ``type="text"`` → 转为 ``output_text``
      - ``type="output_text"`` / ``type="refusal"`` → 原样保留
      - 未知类型 → 降级为 ``output_text``（尝试提取 ``text`` / ``content``）
    - 其他 → 降级为字符串 ``output_text``

    Args:
        content: Chat Completions message 中的 ``content`` 字段。

    Returns:
        Responses API 格式的 content block 列表。
    """
    if content is None:
        return []

    if isinstance(content, str):
        if not content.strip():
            return []
        return [{
            "type": "output_text",
            "text": content,
            "annotations": [],
        }]

    if isinstance(content, list):
        blocks: list[dict] = []
        for block in content:
            if isinstance(block, dict):
                block_type = block.get("type")
                if block_type == "text":
                    blocks.append({
                        "type": "output_text",
                        "text": block.get("text", ""),
                        "annotations": block.get("annotations", []),
                    })
                elif block_type == "output_text":
                    blocks.append(block)
                elif block_type == "refusal":
                    blocks.append(block)
                elif block_type == "image_url":
                    # 图片 content block 在 output 中无直接对应，降级为 output_text
                    image_url_obj = block.get("image_url", {})
                    url = image_url_obj.get("url", "") if isinstance(image_url_obj, dict) else str(image_url_obj)
                    blocks.append({
                        "type": "output_text",
                        "text": f"[Image: {url}]",
                        "annotations": [],
                    })
                else:
                    # 未知类型：尝试提取 text / content
                    text = block.get("text") or block.get("content")
                    if text and isinstance(text, str):
                        blocks.append({
                            "type": "output_text",
                            "text": text,
                            "annotations": [],
                        })
                    else:
                        blocks.append({
                            "type": "output_text",
                            "text": json.dumps(block, ensure_ascii=False),
                            "annotations": [],
                        })
            elif isinstance(block, str):
                blocks.append({
                    "type": "output_text",
                    "text": block,
                    "annotations": [],
                })
        return blocks

    # 兜底：其他类型转为字符串
    return [{
        "type": "output_text",
        "text": str(content),
        "annotations": [],
    }]


def _build_reasoning_item(reasoning_content: str, status: str) -> dict:
    """构建 Responses API ``reasoning`` output item。

    Args:
        reasoning_content: 推理内容文本（非空字符串）。
        status: 响应状态字符串。

    Returns:
        Responses API 格式的 reasoning item 字典。
    """
    return {
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


def _build_function_call_item(tool_call: dict, status: str) -> dict:
    """从 Chat Completions ``tool_call`` 构建 Responses API ``function_call`` item。

    Args:
        tool_call: Chat Completions 的 tool_call 字典，通常包含
            ``id``、``type``、``function``（含 ``name``、``arguments``）。
        status: 响应状态字符串。

    Returns:
        Responses API 格式的 function_call item 字典。
    """
    if not isinstance(tool_call, dict):
        return {
            "type": "function_call",
            "id": f"fc_{uuid.uuid4().hex[:12]}",
            "call_id": f"fc_{uuid.uuid4().hex[:12]}",
            "name": "",
            "arguments": "",
            "status": status,
        }

    fn = tool_call.get("function", {}) or {}
    if not isinstance(fn, dict):
        fn = {}

    call_id = tool_call.get("id", "")
    if not call_id:
        call_id = f"fc_{uuid.uuid4().hex[:12]}"

    return {
        "type": "function_call",
        "id": call_id,
        "call_id": call_id,
        "name": fn.get("name", ""),
        "arguments": fn.get("arguments", ""),
        "status": status,
    }


def _normalize_created_at(created: Any) -> str:
    """标准化创建时间为 ISO 8601 格式字符串（带 UTC 时区）。

    支持输入：
    - ``int`` / ``float`` Unix 时间戳 → ``datetime.fromtimestamp(..., tz=UTC).isoformat()``
    - ``str`` → 原样返回（假设调用方已确保格式正确）
    - 其他 / 解析失败 → ``datetime.now(timezone.utc).isoformat()``

    Args:
        created: 原始 ``created`` 值。

    Returns:
        ISO 8601 格式的时间字符串。
    """
    if isinstance(created, (int, float)):
        try:
            return datetime.fromtimestamp(created, tz=timezone.utc).isoformat()
        except (OSError, ValueError, OverflowError):
            pass

    if isinstance(created, str) and created.strip():
        return created.strip()

    return datetime.now(timezone.utc).isoformat()


def _map_finish_reason_to_status(finish_reason: Any) -> str:
    """将 Chat Completions 的 ``finish_reason`` 映射为 Responses API ``status``。

    映射规则：

    - ``"stop"`` → ``"completed"``
    - ``"length"`` → ``"incomplete"``
    - ``"tool_calls"`` → ``"completed"``
    - ``"content_filter"`` → ``"incomplete"``
    - ``"function_call"``（legacy）→ ``"completed"``
    - ``None`` → ``"in_progress"``
    - 其他未知值 → ``"completed"``（保守兜底）

    Args:
        finish_reason: Chat Completions choice 的 ``finish_reason`` 值。

    Returns:
        Responses API 的 ``status`` 字符串。
    """
    if finish_reason == "stop":
        return "completed"
    if finish_reason == "length":
        return "incomplete"
    if finish_reason == "tool_calls":
        return "completed"
    if finish_reason == "content_filter":
        return "incomplete"
    if finish_reason == "function_call":
        return "completed"
    if finish_reason is None:
        return "in_progress"
    # 未知值保守兜底
    return "completed"


# --------------------------------------------------------------------------- #
# 额外工具函数（供上层调用）
# --------------------------------------------------------------------------- #


def extract_tool_calls_from_message(message: dict) -> list[dict]:
    """从 Chat Completions assistant message 中提取标准化 tool_calls 列表。

    对缺失字段做安全填充，确保每个 tool_call 至少包含
    ``id``、``type``、``function.name``、``function.arguments``。

    Args:
        message: Chat Completions 的 message 字典。

    Returns:
        标准化的 tool_call 字典列表。
    """
    if not isinstance(message, dict):
        return []

    tool_calls = message.get("tool_calls")
    if not tool_calls or not isinstance(tool_calls, list):
        return []

    result: list[dict] = []
    for tc in tool_calls:
        if not isinstance(tc, dict):
            continue
        fn = tc.get("function", {}) or {}
        if not isinstance(fn, dict):
            fn = {}
        call_id = tc.get("id", "")
        if not call_id:
            call_id = f"call_{uuid.uuid4().hex[:12]}"
        result.append({
            "id": call_id,
            "type": tc.get("type") or "function",
            "function": {
                "name": fn.get("name", ""),
                "arguments": fn.get("arguments", ""),
            },
        })
    return result


def rebuild_responses_output_from_chat_delta(
    delta: dict,
    accumulated: dict,
) -> dict:
    """基于 Chat Completions streaming delta 增量重建 Responses API output 片段。

    本函数用于非流式场景下的增量聚合，或供流式转换器内部使用。

    Args:
        delta: Chat Completions delta 字典（含 ``content``、
            ``reasoning_content``、``tool_calls`` 等）。
        accumulated: 当前已聚合的状态字典（会被原地修改）。

    Returns:
        更新后的 ``accumulated`` 字典。
    """
    if not isinstance(delta, dict):
        return accumulated
    if not isinstance(accumulated, dict):
        accumulated = {}

    # 聚合 text
    content = delta.get("content")
    if content and isinstance(content, str):
        accumulated["content"] = accumulated.get("content", "") + content

    # 聚合 reasoning
    reasoning = delta.get("reasoning_content")
    if reasoning and isinstance(reasoning, str):
        accumulated["reasoning_content"] = accumulated.get("reasoning_content", "") + reasoning

    # 聚合 tool_calls
    tool_calls = delta.get("tool_calls")
    if tool_calls and isinstance(tool_calls, list):
        existing = accumulated.setdefault("tool_calls", [])
        if isinstance(existing, list):
            for tc in tool_calls:
                if isinstance(tc, dict):
                    idx = tc.get("index", 0)
                    # 尝试合并到已有 index 位置
                    while len(existing) <= idx:
                        existing.append({})
                    existing_tc = existing[idx]
                    if isinstance(existing_tc, dict):
                        call_id = tc.get("id")
                        if call_id:
                            existing_tc["id"] = call_id
                        fn = tc.get("function", {}) or {}
                        if isinstance(fn, dict):
                            existing_fn = existing_tc.setdefault("function", {})
                            if isinstance(existing_fn, dict):
                                if fn.get("name"):
                                    existing_fn["name"] = fn["name"]
                                if fn.get("arguments"):
                                    existing_fn["arguments"] = existing_fn.get("arguments", "") + fn["arguments"]
                    else:
                        existing[idx] = tc

    return accumulated
