"""Provider-specific 参数 workaround。

根据上游 Provider 的类型选择性应用修复，避免 400/422 或不兼容行为。
所有函数均**不修改原始字典**，返回深拷贝后的新字典（或原值引用若无需修改）。
"""

from __future__ import annotations

import base64
import copy
import json
import re
from typing import Any

# --------------------------------------------------------------------------- #
# Provider 检测
# --------------------------------------------------------------------------- #


def detect_provider_kind(provider: dict) -> str:
    """根据 provider ``name`` 和 ``baseUrl`` 检测提供商类型。

    识别以下 provider：
    ``deepseek``, ``kimi``, ``zhipu``, ``bailian``, ``siliconflow``,
    ``qiniu``, ``anthropic``, ``gemini``, ``mistral``, ``groq``,
    ``together``, ``openai``, ``unknown``。

    Args:
        provider: provider 配置字典，通常含 ``name``、``baseUrl``、``id``。

    Returns:
        小写的 provider kind 字符串。
    """
    if not isinstance(provider, dict):
        return "unknown"

    name = str(provider.get("name") or "").lower()
    base_url = str(provider.get("baseUrl") or "").lower()
    provider_id = str(provider.get("id") or "").lower()

    # 组合检测文本
    combined = f"{name} {base_url} {provider_id}"

    # DeepSeek
    if "deepseek" in combined:
        return "deepseek"

    # Kimi (月之暗面)
    if "kimi" in combined or "moonshot" in combined:
        return "kimi"

    # 智谱 GLM
    if "zhipu" in combined or "bigmodel" in combined:
        return "zhipu"

    # 阿里云百炼
    if "bailian" in combined or "dashscope" in combined:
        return "bailian"

    # SiliconFlow
    if "siliconflow" in combined:
        return "siliconflow"

    # 七牛云 / Qiniu
    if "qiniu" in combined:
        return "qiniu"

    # Anthropic
    if "anthropic" in combined or "claude" in combined:
        return "anthropic"

    # Gemini / Google
    if "gemini" in combined or "google" in combined:
        return "gemini"

    # Mistral
    if "mistral" in combined:
        return "mistral"

    # Groq
    if "groq" in combined:
        return "groq"

    # Together AI
    if "together" in combined:
        return "together"

    # OpenAI (官方)
    if "openai" in combined and "azure" not in combined:
        return "openai"

    return "unknown"


def is_provider(provider_kind: str, *candidates: str) -> bool:
    """辅助函数：判断 provider_kind 是否匹配任一候选。

    Args:
        provider_kind: 小写的 provider kind 字符串。
        *candidates: 可变长度候选 kind 列表。

    Returns:
        若匹配任一候选返回 ``True``，否则 ``False``。
    """
    return provider_kind in {c.lower() for c in candidates}


# --------------------------------------------------------------------------- #
# Anthropic Workaround
# --------------------------------------------------------------------------- #


def anthropic_merge_tool_calls(messages: list[dict]) -> list[dict]:
    """合并连续 function_call 为单条 assistant message。

    Anthropic Messages API 要求 ``tool_use`` 内容块必须紧跟在发出
    ``tool_calls`` 的 assistant message 之后，中间不能插入空的 assistant
    message。本函数将相邻的 assistant + tool_calls 合并为一条。

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

        prev = result[-1]
        # 合并 content
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

    return result


def anthropic_rebuild_tool_calls(
    messages: list[dict],
    cached_tools: list[dict] | None = None,
) -> list[dict]:
    """从缓存或消息历史中重建缺失的 tool_calls。

    Anthropic 的流式响应中，某些 chunk 可能不包含完整的 tool_calls 信息。
    本函数遍历消息列表，若 assistant message 缺失 ``tool_calls`` 但后续出现
    ``role="tool"`` 消息，则从 ``cached_tools`` 或历史推断并补全。

    Args:
        messages: 原始消息列表。
        cached_tools: 可选的工具定义缓存，用于补全 ``name`` 等字段。

    Returns:
        重建后的新消息列表（深拷贝）。
    """
    if not messages:
        return []

    result = copy.deepcopy(messages)
    tool_call_names: dict[str, str] = {}
    if cached_tools:
        for tool in cached_tools:
            if isinstance(tool, dict):
                name = tool.get("name") or tool.get("function", {}).get("name")
                if name:
                    tool_call_names[name] = name

    # 第一遍：收集所有 tool_call_id → name 映射
    for msg in result:
        if not isinstance(msg, dict):
            continue
        tcs = msg.get("tool_calls")
        if isinstance(tcs, list):
            for tc in tcs:
                if isinstance(tc, dict):
                    fn = tc.get("function", {}) or {}
                    call_id = tc.get("id")
                    name = fn.get("name") if isinstance(fn, dict) else None
                    if call_id and name:
                        tool_call_names[call_id] = name

    # 第二遍：为含 tool 消息但缺失 tool_calls 的 assistant 补全
    for i, msg in enumerate(result):
        if not isinstance(msg, dict) or msg.get("role") != "assistant":
            continue
        if msg.get("tool_calls"):
            continue

        # 检查后续是否有 tool 消息引用某个 call_id
        subsequent_tool_ids: set[str] = set()
        for later in result[i + 1 :]:
            if isinstance(later, dict) and later.get("role") == "tool":
                tc_id = later.get("tool_call_id")
                if tc_id:
                    subsequent_tool_ids.add(tc_id)

        if subsequent_tool_ids:
            rebuilt_tool_calls: list[dict] = []
            for tc_id in subsequent_tool_ids:
                name = tool_call_names.get(tc_id, "")
                rebuilt_tool_calls.append({
                    "id": tc_id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                })
            if rebuilt_tool_calls:
                msg["tool_calls"] = rebuilt_tool_calls
                if not msg.get("content"):
                    msg["content"] = ""

    return result


def anthropic_strip_unsupported_params(body: dict) -> dict:
    """移除 Anthropic Messages API 不支持的参数。

    Anthropic 不兼容以下 OpenAI 风格参数：
    ``logprobs``, ``top_logprobs``, ``parallel_tool_calls``,
    ``frequency_penalty``, ``presence_penalty``, ``seed``,
    ``response_format``（部分模型支持 json_mode，但非标准）。

    Args:
        body: 原始请求体字典。

    Returns:
        清理后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}

    unsupported = {
        "logprobs",
        "top_logprobs",
        "parallel_tool_calls",
        "frequency_penalty",
        "presence_penalty",
        "seed",
        "service_tier",
        "safety_identifier",
        "tool_choice",  # Anthropic 使用 tools 和 tool_choice 的简化版本，先移除避免冲突
        "user",
        "metadata",
        "audio",
        "modalities",
        "store",
        "prediction",
    }

    cleaned = copy.deepcopy(body)
    for key in unsupported:
        cleaned.pop(key, None)

    # response_format 处理：若存在则降级为提示词注入
    if "response_format" in cleaned:
        rf = cleaned.pop("response_format")
        _inject_json_instruction(cleaned, rf)

    # Anthropic 使用 ``max_tokens`` 而非 ``max_completion_tokens``
    if "max_completion_tokens" in cleaned:
        cleaned["max_tokens"] = cleaned.pop("max_completion_tokens")

    return cleaned


# --------------------------------------------------------------------------- #
# Mistral Workaround
# --------------------------------------------------------------------------- #


def mistral_clean_tool_schema(tools: list[dict]) -> list[dict]:
    """移除 Mistral 不兼容的工具参数字段。

    Mistral API 对 JSON Schema 的兼容性较严格，不接受：
    ``$id``, ``$schema``, ``strict`` 等字段。

    Args:
        tools: Chat Completions 格式的 tools 列表。

    Returns:
        清理后的 tools 列表（深拷贝）。
    """
    if not tools:
        return []

    cleaned: list[dict] = []
    for tool in tools:
        if not isinstance(tool, dict):
            continue
        new_tool = copy.deepcopy(tool)
        fn = new_tool.get("function")
        if isinstance(fn, dict):
            params = fn.get("parameters")
            if isinstance(params, dict):
                for bad_key in ("$id", "$schema", "strict"):
                    params.pop(bad_key, None)
                # 递归清理嵌套 schema
                _recursive_remove_schema_keys(params)
            # Mistral 不支持 strict
            fn.pop("strict", None)
        cleaned.append(new_tool)
    return cleaned


def mistral_strip_unsupported_params(body: dict) -> dict:
    """移除 Mistral API 不支持的参数。

    Mistral 不兼容：``logprobs``, ``top_logprobs``, ``parallel_tool_calls``,
    ``seed``, ``response_format``（新版已支持，但旧版可能报错，保守移除）。

    Args:
        body: 原始请求体字典。

    Returns:
        清理后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}

    unsupported = {
        "logprobs",
        "top_logprobs",
        "parallel_tool_calls",
        "seed",
        "service_tier",
        "safety_identifier",
        "audio",
        "modalities",
        "store",
        "prediction",
    }

    cleaned = copy.deepcopy(body)
    for key in unsupported:
        cleaned.pop(key, None)

    if "response_format" in cleaned:
        rf = cleaned.pop("response_format")
        _inject_json_instruction(cleaned, rf)

    return cleaned


# --------------------------------------------------------------------------- #
# DeepSeek Workaround
# --------------------------------------------------------------------------- #


def deepseek_convert_reasoning(body: dict) -> dict:
    """转换 DeepSeek 的 reasoning 参数格式。

    DeepSeek 的 reasoning 参数使用 ``{"type": "enabled"}`` 而非标准的
    ``reasoning_effort`` 字符串。本函数将 ``reasoning_effort`` 转换为
    DeepSeek 兼容的 ``reasoning`` 字典格式。

    Args:
        body: 原始请求体字典。

    Returns:
        转换后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}

    cleaned = copy.deepcopy(body)
    reasoning_effort = cleaned.pop("reasoning_effort", None)

    if reasoning_effort:
        # DeepSeek 使用 thinking 字段，格式为 {"type": "enabled"}
        cleaned["reasoning"] = {"type": "enabled"}
        # 某些场景下也支持 budget_tokens
        if isinstance(reasoning_effort, (int, float)):
            cleaned["reasoning"]["budget_tokens"] = int(reasoning_effort)

    return cleaned


def deepseek_strip_unsupported_response_format(body: dict) -> dict:
    """剥掉 DeepSeek 不支持的 ``response_format`` 类型。

    DeepSeek 官方仅支持 ``{"type": "json_object"}``,
    不支持 OpenAI 较新的 ``{"type": "json_schema", ...}`` strict structured output
    （上游会返回 400 ``This response_format type is unavailable now``）。

    本函数遇到 ``json_schema`` 类型时直接移除整个字段。不降级为
    ``json_object`` 是因为 DeepSeek 的 ``json_object`` 模式要求 prompt 中
    出现 "json" 字样, Codex 的 prompt 不一定满足, 强行降级反而二次报错。

    参考: https://api-docs.deepseek.com/guides/json_mode
    """
    if not isinstance(body, dict):
        return body
    rf = body.get("response_format")
    if isinstance(rf, dict) and rf.get("type") == "json_schema":
        body = copy.deepcopy(body)
        body.pop("response_format", None)
    return body


def deepseek_inject_request_options(body: dict, options: dict) -> dict:
    """向 DeepSeek 请求体注入特定选项。

    例如注入 ``chat_format``、``do_sample`` 等 vendor-specific 字段。
    ``options`` 中的键值对会直接合并到请求体顶层。

    Args:
        body: 原始请求体字典。
        options: 需要注入的选项字典。

    Returns:
        合并后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}
    if not options or not isinstance(options, dict):
        return copy.deepcopy(body)

    merged = copy.deepcopy(body)
    for key, value in options.items():
        if key not in merged:
            merged[key] = value
    return merged


# --------------------------------------------------------------------------- #
# Gemini Workaround
# --------------------------------------------------------------------------- #


async def gemini_convert_image_urls(messages: list[dict]) -> list[dict]:
    """将消息中的 HTTP 图片 URL 转换为 base64 data URI。

    Gemini API（通过 OpenAI 兼容层）通常不接受远程图片 URL，需要把图片下载
    后转为 base64 内嵌格式。本函数遍历所有消息 content 块，对 ``type: image_url``
    且 URL 为 http/https 开头的块执行下载并替换。

    Args:
        messages: 原始消息列表。

    Returns:
        转换后的新消息列表（深拷贝）。若下载失败则保留原 URL。
    """
    if not messages:
        return []

    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        new_msg = copy.deepcopy(msg)
        content = new_msg.get("content")

        if isinstance(content, list):
            new_content: list[dict] = []
            for block in content:
                if not isinstance(block, dict):
                    new_content.append(block)
                    continue
                if block.get("type") == "image_url":
                    image_url_obj = block.get("image_url")
                    if isinstance(image_url_obj, dict):
                        url = str(image_url_obj.get("url") or "")
                        if url.startswith(("http://", "https://")):
                            data_uri = await _fetch_image_as_data_uri(url)
                            if data_uri:
                                new_block = copy.deepcopy(block)
                                new_block["image_url"] = {
                                    **image_url_obj,
                                    "url": data_uri,
                                }
                                new_content.append(new_block)
                            else:
                                new_content.append(block)
                        else:
                            new_content.append(block)
                    else:
                        new_content.append(block)
                else:
                    new_content.append(block)
            new_msg["content"] = new_content

        result.append(new_msg)
    return result


def gemini_handle_multimodal_content(messages: list[dict]) -> list[dict]:
    """处理 Gemini 多模态 content 块格式兼容问题。

    Gemini 对 ``image_url``、``input_audio``、``input_video`` 等块有特定
    要求：
    - 图片：仅接受 base64 data URI 或 Google Cloud Storage URI
    - 音频/视频：需要 ``mime_type`` 和 ``data`` 字段

    本函数规范化 content 块，补充缺失的 ``mime_type``，确保格式符合
    Gemini 预期。

    Args:
        messages: 原始消息列表。

    Returns:
        规范化后的新消息列表（深拷贝）。
    """
    if not messages:
        return []

    result: list[dict] = []
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        new_msg = copy.deepcopy(msg)
        content = new_msg.get("content")

        if isinstance(content, list):
            new_content: list[dict] = []
            for block in content:
                if not isinstance(block, dict):
                    new_content.append(block)
                    continue

                block_type = block.get("type")
                if block_type == "image_url":
                    image_url_obj = block.get("image_url")
                    if isinstance(image_url_obj, dict):
                        url = str(image_url_obj.get("url") or "")
                        if url.startswith("data:"):
                            mime = _extract_mime_from_data_uri(url) or "image/png"
                            new_content.append({
                                "type": "image_url",
                                "image_url": {"url": url, "mime_type": mime},
                            })
                        else:
                            new_content.append(block)
                    else:
                        new_content.append(block)
                elif block_type == "input_audio":
                    audio_obj = block.get("input_audio")
                    if isinstance(audio_obj, dict):
                        mime = audio_obj.get("mime_type") or audio_obj.get("format") or "audio/wav"
                        new_content.append({
                            "type": "input_audio",
                            "input_audio": {
                                "data": audio_obj.get("data", ""),
                                "mime_type": mime,
                            },
                        })
                    else:
                        new_content.append(block)
                else:
                    new_content.append(block)
            new_msg["content"] = new_content

        result.append(new_msg)
    return result


# --------------------------------------------------------------------------- #
# Groq Workaround
# --------------------------------------------------------------------------- #


def groq_fake_streaming_inject(body: dict) -> dict:
    """当 streaming + response_format 不兼容时注入 fake streaming。

    Groq 的部分模型在 ``stream=True`` 且同时指定 ``response_format`` 时会报错。
    本函数检测该冲突组合，将 ``stream`` 强制设为 ``False``，由上层代理模拟
    流式输出。

    Args:
        body: 原始请求体字典。

    Returns:
        调整后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}

    cleaned = copy.deepcopy(body)
    stream = cleaned.get("stream")
    has_response_format = "response_format" in cleaned

    if stream and has_response_format:
        cleaned["stream"] = False
        # 标记为 fake streaming，供上层识别
        cleaned["_fake_streaming"] = True

    return cleaned


def groq_strip_unsupported_params(body: dict) -> dict:
    """移除 Groq API 不支持的参数。

    Groq 不兼容：``logprobs``, ``top_logprobs``, ``parallel_tool_calls``,
    ``seed``（部分模型支持），``audio``, ``modalities``, ``store``。

    Args:
        body: 原始请求体字典。

    Returns:
        清理后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}

    unsupported = {
        "logprobs",
        "top_logprobs",
        "parallel_tool_calls",
        "audio",
        "modalities",
        "store",
        "prediction",
        "service_tier",
        "safety_identifier",
    }

    cleaned = copy.deepcopy(body)
    for key in unsupported:
        cleaned.pop(key, None)

    return cleaned


# --------------------------------------------------------------------------- #
# Together AI Workaround
# --------------------------------------------------------------------------- #


def together_filter_capabilities(body: dict, model_capabilities: dict) -> dict:
    """根据 Together AI 模型能力动态移除不支持参数。

    ``model_capabilities`` 是一个模型 ID → 能力字典的映射，能力键示例：
    ``supports_tool_calls``, ``supports_json_mode``, ``supports_streaming``。
    若某能力为 ``False``，则移除对应参数。

    Args:
        body: 原始请求体字典。
        model_capabilities: 模型能力描述字典，形如
            ``{"model-id": {"supports_tool_calls": False}}``。

    Returns:
        过滤后的新字典（深拷贝）。
    """
    if not isinstance(body, dict):
        return {}
    if not model_capabilities or not isinstance(model_capabilities, dict):
        return copy.deepcopy(body)

    model = str(body.get("model") or "")
    caps = model_capabilities.get(model) or {}
    if not isinstance(caps, dict):
        return copy.deepcopy(body)

    cleaned = copy.deepcopy(body)

    if caps.get("supports_tool_calls") is False:
        cleaned.pop("tools", None)
        cleaned.pop("tool_choice", None)
        cleaned.pop("parallel_tool_calls", None)

    if caps.get("supports_json_mode") is False:
        rf = cleaned.pop("response_format", None)
        if rf:
            _inject_json_instruction(cleaned, rf)

    if caps.get("supports_streaming") is False:
        cleaned.pop("stream", None)
        cleaned.pop("stream_options", None)

    if caps.get("supports_reasoning") is False:
        cleaned.pop("reasoning_effort", None)
        cleaned.pop("reasoning", None)

    return cleaned


# --------------------------------------------------------------------------- #
# 通用入口函数
# --------------------------------------------------------------------------- #


async def apply_request_workarounds(body: dict, provider: dict) -> dict:
    """对**请求体**应用所有与 provider 相关的 workaround。

    根据 ``detect_provider_kind(provider)`` 的结果，按顺序调用对应 workaround
    函数。不修改原始 ``body``，返回深拷贝后的新字典。

    Args:
        body: 即将发送给上游 provider 的请求体。
        provider: provider 配置字典。

    Returns:
        修复后的请求体字典。
    """
    if not isinstance(body, dict):
        return {}

    kind = detect_provider_kind(provider)
    result = copy.deepcopy(body)

    # --- Anthropic ---
    if is_provider(kind, "anthropic"):
        result = anthropic_strip_unsupported_params(result)
        messages = result.get("messages")
        if isinstance(messages, list):
            messages = anthropic_merge_tool_calls(messages)
            messages = anthropic_rebuild_tool_calls(messages)
            result["messages"] = messages

    # --- Mistral ---
    elif is_provider(kind, "mistral"):
        result = mistral_strip_unsupported_params(result)
        tools = result.get("tools")
        if isinstance(tools, list):
            result["tools"] = mistral_clean_tool_schema(tools)

    # --- DeepSeek ---
    elif is_provider(kind, "deepseek"):
        result = deepseek_convert_reasoning(result)
        result = deepseek_strip_unsupported_response_format(result)

    # --- Gemini ---
    elif is_provider(kind, "gemini"):
        messages = result.get("messages")
        if isinstance(messages, list):
            messages = await gemini_convert_image_urls(messages)
            messages = gemini_handle_multimodal_content(messages)
            result["messages"] = messages

    # --- Groq ---
    elif is_provider(kind, "groq"):
        result = groq_fake_streaming_inject(result)
        result = groq_strip_unsupported_params(result)

    # --- Together AI ---
    elif is_provider(kind, "together"):
        model_capabilities = provider.get("modelCapabilities") or {}
        result = together_filter_capabilities(result, model_capabilities)

    # 通用：清理空列表/字典参数，避免上游误解析
    result = _strip_empty_values(result)
    return result


def apply_response_workarounds(response: dict, provider: dict) -> dict:
    """对**响应体**应用所有与 provider 相关的 workaround。

    在将上游 provider 的 Chat Completions 响应转换回 Responses API 格式前，
    先对原始响应做必要的字段修正（如缺失字段补全、格式对齐等）。

    Args:
        response: 上游 provider 返回的原始响应字典。
        provider: provider 配置字典。

    Returns:
        修复后的响应字典。
    """
    if not isinstance(response, dict):
        return {}

    kind = detect_provider_kind(provider)
    result = copy.deepcopy(response)

    # DeepSeek: 若返回 reasoning_content 字段，确保格式正确
    if is_provider(kind, "deepseek"):
        choices = result.get("choices")
        if isinstance(choices, list) and choices:
            choice = choices[0]
            if isinstance(choice, dict):
                message = choice.get("message", {}) or {}
                if isinstance(message, dict) and message.get("reasoning_content"):
                    # 确保 reasoning_content 被正确放置
                    pass

    # Gemini: 修正 finish_reason 等字段
    if is_provider(kind, "gemini"):
        choices = result.get("choices")
        if isinstance(choices, list) and choices:
            choice = choices[0]
            if isinstance(choice, dict):
                finish_reason = choice.get("finish_reason")
                if finish_reason == "STOP":
                    choice["finish_reason"] = "stop"
                elif finish_reason == "MAX_TOKENS":
                    choice["finish_reason"] = "length"

    # 通用：补全缺失的 usage 字段（部分 provider 不返回 usage）
    if "usage" not in result:
        result["usage"] = {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        }

    return result


async def apply_provider_workarounds(
    body: dict,
    provider: dict,
    direction: str = "request",
) -> dict:
    """统一入口：根据 provider kind 和方向应用所有相关 workaround。

    Args:
        body: 请求体或响应体字典。
        provider: provider 配置字典。
        direction: ``"request"`` 或 ``"response"``，决定应用哪一组 workaround。

    Returns:
        修复后的新字典。
    """
    if direction == "response":
        return apply_response_workarounds(body, provider)
    return await apply_request_workarounds(body, provider)


# --------------------------------------------------------------------------- #
# 内部辅助函数
# --------------------------------------------------------------------------- #


def _inject_json_instruction(body: dict, response_format: Any) -> None:
    """将 response_format 降级为 system/user 消息中的 JSON 格式提示。

    对不支持 ``response_format`` 的 provider，在 messages 中注入提示词，
    要求模型输出 JSON。直接修改 ``body`` 字典（原地修改）。
    """
    if not isinstance(body, dict):
        return

    instruction = ""
    if isinstance(response_format, dict):
        fmt_type = response_format.get("type")
        if fmt_type == "json_object":
            instruction = "Please respond with valid JSON only."
        elif fmt_type == "json_schema":
            schema = response_format.get("json_schema", {}).get("schema", {})
            instruction = (
                "Please respond with valid JSON conforming to the following schema:\n"
                f"{json.dumps(schema, indent=2, ensure_ascii=False)}"
            )

    if not instruction:
        return

    messages = body.get("messages")
    if not isinstance(messages, list):
        return

    # 优先追加到 system/developer 消息，否则插入新 system 消息
    for msg in messages:
        if isinstance(msg, dict) and msg.get("role") in ("system", "developer"):
            content = msg.get("content") or ""
            if isinstance(content, str):
                msg["content"] = content + "\n\n" + instruction
            break
    else:
        messages.insert(0, {"role": "system", "content": instruction})


def _recursive_remove_schema_keys(schema: dict) -> None:
    """递归移除 JSON Schema 中的不兼容键（原地修改）。"""
    if not isinstance(schema, dict):
        return
    for bad_key in ("$id", "$schema", "strict"):
        schema.pop(bad_key, None)
    for value in schema.values():
        if isinstance(value, dict):
            _recursive_remove_schema_keys(value)
        elif isinstance(value, list):
            for item in value:
                if isinstance(item, dict):
                    _recursive_remove_schema_keys(item)


async def _fetch_image_as_data_uri(url: str) -> str | None:
    """下载远程图片并返回 base64 data URI（异步版本）。

    Returns:
        data URI 字符串，或 ``None`` 表示下载失败。
    """
    try:
        import httpx

        async with httpx.AsyncClient(timeout=30.0, follow_redirects=True) as client:
            resp = await client.get(url)
            resp.raise_for_status()
            mime = resp.headers.get("content-type", "image/png")
            if ";" in mime:
                mime = mime.split(";")[0].strip()
            b64 = base64.b64encode(resp.content).decode("utf-8")
            return f"data:{mime};base64,{b64}"
    except Exception:
        return None


def _extract_mime_from_data_uri(data_uri: str) -> str | None:
    """从 data URI 中提取 MIME 类型。"""
    match = re.match(r"data:([^;]+);base64,", data_uri)
    return match.group(1) if match else None


def _strip_empty_values(body: dict) -> dict:
    """移除请求体中值为空列表/空字典/空字符串的顶层键（原地修改并返回）。"""
    if not isinstance(body, dict):
        return body
    empty_keys = [
        k
        for k, v in body.items()
        if v == [] or v == {} or (isinstance(v, str) and not v.strip())
    ]
    for k in empty_keys:
        body.pop(k, None)
    return body
