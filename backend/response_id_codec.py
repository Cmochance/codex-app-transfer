"""Response ID / Container ID 编解码器。

将 provider、model、request_id 编码进响应 ID，使后续请求可以基于
previous_response_id 自动路由回原始部署（deployment affinity）。
"""

from __future__ import annotations

import base64
import re
from typing import Any

# 用于解析 decode_response_id 解码后的字符串（request_id 可能含分号）
_RESPONSE_ID_PATTERN = re.compile(
    r"litellm:custom_llm_provider:([^;]*);model_id:([^;]*);response_id:(.+)"
)


def encode_response_id(provider: str | None, model: str | None, request_id: str) -> str:
    """将 provider、model、request_id 编码为 ``resp_xxx`` 格式。

    编码格式：
        ``litellm:custom_llm_provider:{provider};model_id:{model};response_id:{request_id}``
        → base64 → ``resp_{base64}``

    Args:
        provider: 提供商标识（如 ``deepseek``、``openai``）。
        model: 模型标识（可为空）。
        request_id: 上游返回的原始响应 ID（必填）。

    Returns:
        形如 ``resp_xxx`` 的编码字符串。
    """
    provider_part = "" if provider is None else provider
    model_part = "" if model is None else model
    assembled = (
        f"litellm:custom_llm_provider:{provider_part};"
        f"model_id:{model_part};"
        f"response_id:{request_id}"
    )
    encoded = base64.b64encode(assembled.encode("utf-8")).decode("utf-8")
    return f"resp_{encoded}"


def decode_response_id(response_id: str) -> dict[str, Any]:
    """解码 ``resp_xxx`` 格式的响应 ID。

    Returns:
        字典，包含 ``custom_llm_provider``、``model_id``、``response_id`` 字段。
        若解码失败，则 ``response_id`` 回退为原始输入，其余字段为 ``None``。
    """
    result: dict[str, Any] = {
        "custom_llm_provider": None,
        "model_id": None,
        "response_id": response_id,
    }
    if not response_id.startswith("resp_"):
        return result

    try:
        cleaned = response_id[len("resp_"):]
        # 恢复可能被截断的 base64 padding
        missing = len(cleaned) % 4
        if missing:
            cleaned += "=" * (4 - missing)
        decoded = base64.b64decode(cleaned.encode("utf-8")).decode("utf-8")

        match = _RESPONSE_ID_PATTERN.match(decoded)
        if match:
            result["custom_llm_provider"] = match.group(1) or None
            result["model_id"] = match.group(2) or None
            result["response_id"] = match.group(3) or response_id
        else:
            result["response_id"] = decoded
    except Exception:
        # 解码异常时安全回退
        pass

    return result


def encode_container_id(
    custom_llm_provider: str | None,
    model_id: str | None,
    container_id: str,
) -> str:
    """将 container_id 编码为 ``cntr_xxx`` 格式。

    编码格式：
        ``litellm:custom_llm_provider:{provider};model_id:{model};container_id:{container_id}``
        → base64 → ``cntr_{base64}``

    Args:
        custom_llm_provider: 提供商标识。
        model_id: 模型标识。
        container_id: 上游返回的原始 container ID。

    Returns:
        形如 ``cntr_xxx`` 的编码字符串。
    """
    provider_part = "" if custom_llm_provider is None else custom_llm_provider
    model_part = "" if model_id is None else model_id
    assembled = (
        f"litellm:custom_llm_provider:{provider_part};"
        f"model_id:{model_part};"
        f"container_id:{container_id}"
    )
    encoded = base64.b64encode(assembled.encode("utf-8")).decode("utf-8")
    return f"cntr_{encoded}"


def decode_container_id(container_id: str) -> dict[str, Any]:
    """解码 ``cntr_xxx`` 格式的容器 ID。

    Returns:
        字典，包含 ``custom_llm_provider``、``model_id``、``response_id``（原始 container_id）。
        若解码失败则安全回退。
    """
    result: dict[str, Any] = {
        "custom_llm_provider": None,
        "model_id": None,
        "response_id": container_id,
    }
    if not container_id.startswith("cntr_"):
        return result

    try:
        cleaned = container_id[len("cntr_"):]
        missing = len(cleaned) % 4
        if missing:
            cleaned += "=" * (4 - missing)
        decoded = base64.b64decode(cleaned.encode("utf-8")).decode("utf-8")

        if not decoded.startswith("litellm:"):
            return result

        # 使用正则提取三个部分，允许 container_id 内部包含分号
        pattern = (
            r"^litellm:custom_llm_provider:([^;]*);"
            r"model_id:([^;]*);"
            r"container_id:(.+)$"
        )
        match = re.match(pattern, decoded)
        if not match:
            return result

        raw_provider = match.group(1)
        raw_model = match.group(2)
        original_container = match.group(3)

        result["custom_llm_provider"] = None if raw_provider in ("", "None") else raw_provider
        result["model_id"] = None if raw_model in ("", "None") else raw_model
        result["response_id"] = original_container
    except Exception:
        pass

    return result


def encode_content_item_id(model_id: str, item_id: str) -> str:
    """为加密内容项（encrypted_content）编码带 affinity 信息的 item ID。

    编码格式：
        ``litellm:model_id:{model_id};item_id:{item_id}`` → base64 → ``encitem_{base64}``

    Args:
        model_id: 部署/模型标识。
        item_id: 上游返回的原始 item ID。

    Returns:
        形如 ``encitem_xxx`` 的编码字符串。
    """
    assembled = f"litellm:model_id:{model_id};item_id:{item_id}"
    encoded = base64.b64encode(assembled.encode("utf-8")).decode("utf-8")
    return f"encitem_{encoded}"


def decode_content_item_id(encoded_id: str) -> dict[str, str] | None:
    """解码 ``encitem_xxx`` 格式的内容项 ID。

    Returns:
        字典，包含 ``model_id`` 和 ``item_id``；若格式不匹配则返回 ``None``。
    """
    if not encoded_id.startswith("encitem_"):
        return None
    try:
        cleaned = encoded_id[len("encitem_"):]
        missing = len(cleaned) % 4
        if missing:
            cleaned += "=" * (4 - missing)
        decoded = base64.b64decode(cleaned.encode("utf-8")).decode("utf-8")
        parts = decoded.split(";", 1)
        if len(parts) < 2:
            return None
        model_id = parts[0].replace("litellm:model_id:", "")
        item_id = parts[1].replace("item_id:", "")
        return {"model_id": model_id, "item_id": item_id}
    except Exception:
        return None
