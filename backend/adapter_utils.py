"""Responses API ↔ Chat Completions 转换过程中的通用工具函数。

涵盖 usage 归一化、response_format 转换、tool_choice 规范化、
MCP 输入模式标准化、本地文件转 base64 data URI 等能力。
"""

from __future__ import annotations

import base64
import json
import mimetypes
import os
import re
from io import IOBase
from pathlib import Path
from typing import Any

# --------------------------------------------------------------------------- #
# Usage 转换
# --------------------------------------------------------------------------- #


def _transform_usage_details(usage: dict[str, Any] | None) -> dict[str, Any]:
    """将上游 usage 对象转换为标准化字典。

    同时兼容 OpenAI Chat Completions 和 Responses API 的 usage 格式，
    输出统一包含 ``input_tokens``、``output_tokens``、``total_tokens``
    以及可选的明细字段 ``cached_tokens``、``audio_tokens``、
    ``reasoning_tokens``、``completion_tokens_details``。

    Args:
        usage: 原始 usage 字典或 ``None``。

    Returns:
        标准化后的 usage 字典。
    """
    if not usage:
        return {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
        }

    def _token_int(value: Any) -> int:
        try:
            return int(value or 0)
        except (TypeError, ValueError):
            return 0

    # 兼容两种字段命名
    input_tokens = _token_int(
        usage.get("prompt_tokens")
        if usage.get("prompt_tokens") is not None
        else usage.get("input_tokens")
    )
    output_tokens = _token_int(
        usage.get("completion_tokens")
        if usage.get("completion_tokens") is not None
        else usage.get("output_tokens")
    )
    total_tokens = _token_int(usage.get("total_tokens"))
    if total_tokens == 0 and (input_tokens or output_tokens):
        total_tokens = input_tokens + output_tokens

    result: dict[str, Any] = {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
    }

    # prompt_tokens_details（输入明细）
    prompt_details = usage.get("prompt_tokens_details") or usage.get("input_tokens_details")
    if isinstance(prompt_details, dict):
        cached = prompt_details.get("cached_tokens")
        audio = prompt_details.get("audio_tokens")
        if cached is not None:
            result["cached_tokens"] = _token_int(cached)
        if audio is not None:
            result["audio_tokens"] = _token_int(audio)

    # completion_tokens_details（输出明细）
    completion_details = usage.get("completion_tokens_details")
    if isinstance(completion_details, dict):
        reasoning = completion_details.get("reasoning_tokens")
        if reasoning is not None:
            result["reasoning_tokens"] = _token_int(reasoning)
        result["completion_tokens_details"] = {
            k: _token_int(v)
            for k, v in completion_details.items()
            if v is not None
        }
    elif "reasoning_tokens" in usage:
        # 某些上游直接把 reasoning_tokens 放在 usage 顶层
        result["reasoning_tokens"] = _token_int(usage["reasoning_tokens"])

    return result


# --------------------------------------------------------------------------- #
# response_format / text.format 转换
# --------------------------------------------------------------------------- #


def _transform_text_format_to_response_format(
    text_param: dict[str, Any] | None,
) -> dict[str, Any] | None:
    """将 Responses API 的 ``text.format`` 参数转换为 Chat Completions 的
    ``response_format``。

    支持：
    - ``json_schema`` → 标准 OpenAI json_schema 结构
    - ``json_object`` → ``{"type": "json_object"}``
    - ``text`` → ``None``（无需强制格式）

    Args:
        text_param: ``text`` 参数字典，通常来自请求体的 ``text`` 字段。

    Returns:
        Chat Completions 可用的 ``response_format`` 字典，或 ``None``。
    """
    if not text_param or not isinstance(text_param, dict):
        return None

    fmt = text_param.get("format")
    if not fmt or not isinstance(fmt, dict):
        return None

    fmt_type = fmt.get("type")
    if fmt_type == "json_schema":
        return {
            "type": "json_schema",
            "json_schema": {
                "name": fmt.get("name") or "response_schema",
                "schema": fmt.get("schema") or {},
                "strict": fmt.get("strict", False),
            },
        }
    if fmt_type == "json_object":
        return {"type": "json_object"}
    if fmt_type == "text":
        return None

    # 未知类型：若包含 schema 则尝试按 json_schema 处理
    if "schema" in fmt:
        return {
            "type": "json_schema",
            "json_schema": {
                "name": fmt.get("name") or "response_schema",
                "schema": fmt.get("schema") or {},
                "strict": fmt.get("strict", False),
            },
        }

    return None


# --------------------------------------------------------------------------- #
# tool_choice 规范化
# --------------------------------------------------------------------------- #


def _normalize_tool_choice(tool_choice: Any) -> Any:
    """将各种 ``tool_choice`` 格式规范化为 Chat Completions 可接受的值。

    支持：
    - ``str``: ``auto``、``none``、``required`` 直接透传
    - ``dict``:
      - OpenAI 标准格式（含 ``function.name``）直接透传
      - 简写格式（如 ``{"type": "auto"}``）转为字符串
      - ``{"type": "function", "function": {"name": "xxx"}}`` 透传
    - 其他类型原样返回

    Args:
        tool_choice: 原始 tool_choice 值。

    Returns:
        规范化后的 tool_choice。
    """
    if tool_choice is None:
        return None

    if isinstance(tool_choice, str):
        return tool_choice

    if isinstance(tool_choice, dict):
        tc_type = tool_choice.get("type")

        # 标准 OpenAI dict 格式：含 function.name 直接透传
        if tool_choice.get("function") and isinstance(tool_choice["function"], dict):
            if tool_choice["function"].get("name"):
                return tool_choice

        # 简写格式转换
        if tc_type == "auto":
            return "auto"
        if tc_type == "none":
            return "none"
        if tc_type in ("required", "tool", "any"):
            return "required"
        if tc_type == "function":
            # 如果没有 name，降级为 required
            return tool_choice if tool_choice.get("function") else "required"

    return tool_choice


# --------------------------------------------------------------------------- #
# MCP inputSchema 标准化
# --------------------------------------------------------------------------- #


def _normalize_mcp_input_schema(schema: dict[str, Any] | None) -> dict[str, Any]:
    """将 MCP 工具的 ``inputSchema`` 标准化为 OpenAI function 参数格式。

    OpenAI 要求 function parameters 必须：
    - ``type: "object"``
    - 包含 ``properties``（可为空）
    - 推荐 ``additionalProperties: false``

    Args:
        schema: MCP 返回的原始 inputSchema。

    Returns:
        标准化后的参数字典。
    """
    if not schema or not isinstance(schema, dict):
        return {"type": "object", "properties": {}, "additionalProperties": False}

    normalized = dict(schema)

    if "type" not in normalized:
        normalized["type"] = "object"

    if "properties" not in normalized:
        normalized["properties"] = {}

    if "additionalProperties" not in normalized:
        normalized["additionalProperties"] = False

    return normalized


# --------------------------------------------------------------------------- #
# 文件 / MIME 处理
# --------------------------------------------------------------------------- #

# 扩展名到 MIME 类型的显式映射（覆盖系统 guess_type 不可靠的情况）
_MIME_TYPE_MAP: dict[str, str] = {
    ".pdf": "application/pdf",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".tiff": "image/tiff",
    ".tif": "image/tiff",
    ".bmp": "image/bmp",
    ".svg": "image/svg+xml",
    ".txt": "text/plain",
    ".html": "text/html",
    ".htm": "text/html",
    ".json": "application/json",
    ".xml": "application/xml",
    ".md": "text/markdown",
    ".csv": "text/csv",
}

# 严格的 MIME 类型格式校验正则
_MIME_PATTERN = re.compile(r"^[\w.+-]+/[\w.+-]+$")


def get_mime_type(filename: str) -> str:
    """根据文件名推断 MIME 类型。

    优先使用内置扩展名映射表，其次调用 ``mimetypes.guess_type``，
    最终回退为 ``application/octet-stream``。

    Args:
        filename: 文件名或完整路径。

    Returns:
        MIME 类型字符串。
    """
    ext = os.path.splitext(str(filename))[1].lower()
    mime = _MIME_TYPE_MAP.get(ext)
    if mime:
        return mime
    guessed, _ = mimetypes.guess_type(str(filename))
    return guessed or "application/octet-stream"


def convert_file_document_to_url_document(
    file_input: dict[str, Any],
) -> dict[str, str]:
    """将本地文件类型的 document 转换为 base64 data URI 格式。

    支持的 ``file_input`` 格式：
    - ``{"type": "file", "file": "/path/to/file.pdf"}``
    - ``{"type": "file", "file": pathlib.Path("/path/to/file.pdf")}``
    - ``{"type": "file", "file": b"raw bytes"}``
    - ``{"type": "file", "file": <file-like object>}``

    额外字段：
    - ``mime_type``: 显式指定 MIME 类型，覆盖自动推断。

    Returns:
        转换后的字典：
        - 图片类型 → ``{"type": "image_url", "image_url": "data:image/...;base64,..."}``
        - 其他类型 → ``{"type": "document_url", "document_url": "data:...;base64,..."}``

    Raises:
        ValueError: 输入格式不支持或文件为空。
        FileNotFoundError: 文件路径不存在。
    """
    if not isinstance(file_input, dict):
        raise ValueError(
            f"file_input must be a dict, got {type(file_input).__name__}"
        )

    raw_file = file_input.get("file")
    if raw_file is None:
        raise ValueError(
            "document with type='file' must include a 'file' field containing "
            "a file path (str), pathlib.Path, file-like object, or bytes"
        )

    file_bytes: bytes
    mime_type: str = "application/octet-stream"
    file_name: str | None = None

    # 1. 字符串路径
    if isinstance(raw_file, (str, Path)):
        file_path = str(raw_file)
        if not os.path.isfile(file_path):
            raise FileNotFoundError(f"File not found: {file_path}")
        mime_type = get_mime_type(file_path)
        file_name = os.path.basename(file_path)
        with open(file_path, "rb") as f:
            file_bytes = f.read()

    # 2. 原始 bytes
    elif isinstance(raw_file, bytes):
        file_bytes = raw_file

    # 3. 类文件对象
    elif isinstance(raw_file, IOBase) or hasattr(raw_file, "read"):
        if hasattr(raw_file, "name"):
            file_name = getattr(raw_file, "name", None)
            if file_name:
                mime_type = get_mime_type(file_name)
        read_result = raw_file.read()
        if isinstance(read_result, str):
            file_bytes = read_result.encode("utf-8")
        else:
            file_bytes = read_result

    else:
        raise ValueError(
            f"Unsupported file input type: {type(raw_file).__name__}. "
            "Expected str (file path), pathlib.Path, bytes, or a file-like object."
        )

    if not file_bytes:
        raise ValueError("File is empty or could not be read")

    # 允许调用方显式覆盖 MIME 类型
    if "mime_type" in file_input:
        mime_type = str(file_input["mime_type"])

    if not _MIME_PATTERN.match(mime_type):
        raise ValueError(f"Invalid MIME type: {mime_type}")

    base64_data = base64.b64encode(file_bytes).decode("utf-8")
    data_uri = f"data:{mime_type};base64,{base64_data}"

    if mime_type.startswith("image/"):
        return {"type": "image_url", "image_url": data_uri}

    return {"type": "document_url", "document_url": data_uri}
