"""Responses API 请求 → OpenAI Chat Completions 请求转换器。

代理网关的核心转换路径：将客户端发来的 Responses API 格式请求
转换为上游 Provider 可接受的 OpenAI Chat Completions 格式。

本模块为 ``backend.api_adapters`` 提供底层转换能力，所有函数均**
不修改原始字典**，返回深拷贝后的新字典（或新构造的字典）。
"""

from __future__ import annotations

import copy
import json
import uuid
from typing import Any

from backend.base_adapter import (
    CHAT_COMPLETIONS_KNOWN_PARAMS,
    convert_developer_to_system,
    convert_reasoning_to_reasoning_effort,
    convert_responses_tool_to_chat_tool,
    convert_text_format_to_response_format,
    filter_unknown_params,
    merge_consecutive_assistant_messages,
    merge_consecutive_user_messages,
    normalize_content_array,
)
from backend.openai_adapter import (
    build_reasoning_effort,
    build_response_format,
    build_tool_choice,
    build_tool_config,
    filter_chat_completion_params,
    handle_openai_audio_params,
    handle_openai_metadata_param,
    handle_openai_modalities,
    handle_openai_prediction_param,
    handle_openai_service_tier,
    handle_openai_store_param,
)
from backend.provider_workarounds import apply_request_workarounds
from backend.response_id_codec import encode_response_id


# --------------------------------------------------------------------------- #
# 主转换入口
# --------------------------------------------------------------------------- #


async def convert_responses_to_chat_body(
    body: dict,
    provider: dict | None = None,
    stream: bool = False,
    session_cache: Any = None,
) -> dict:
    """将 Responses API 请求体转换为 OpenAI Chat Completions 请求体。

    转换流程：
    1. 深拷贝原始请求体，避免副作用。
    2. 构建 ``messages``（含 ``previous_response_id`` 历史恢复、
       ``instructions`` → system、``input`` → user/assistant/tool）。
    3. 合并连续用户消息与 assistant 消息。
    4. 非 OpenAI 官方 provider 时将 ``developer`` 角色降级为 ``system``。
    5. 转换 ``tools`` / ``tool_choice``、``text.format`` → ``response_format``、
       ``reasoning`` → ``reasoning_effort``、``max_output_tokens`` → ``max_tokens``。
    6. 透传 ``temperature``、``top_p``、``user``、``metadata``、
       ``parallel_tool_calls``、``modalities``、``audio`` 等标准字段。
    7. 过滤参数白名单（``filter_chat_completion_params``）。
    8. 应用 provider-specific request workaround。

    Args:
        body: Responses API 请求体字典。
        provider: 上游 Provider 配置字典（含 ``name``、``baseUrl`` 等），
            用于应用 provider-specific workaround。
        stream: 是否流式请求。为 ``True`` 时会注入
            ``stream_options: {"include_usage": True}``。
        session_cache: ``ResponseSessionCache`` 实例。若提供且请求中包含
            ``previous_response_id``，则从缓存恢复历史消息。

    Returns:
        OpenAI Chat Completions 格式的请求体字典。
    """
    if not isinstance(body, dict):
        body = {}

    # 深拷贝，确保不修改原始 dict
    original = copy.deepcopy(body)

    # ------------------------------------------------------------------ #
    # 1. 构建 messages
    # ------------------------------------------------------------------ #
    messages = _build_messages_from_input(original, session_cache)

    # 合并连续用户消息（图片+文本+文件可能产生多条连续 user）
    messages = merge_consecutive_user_messages(messages)

    # 合并连续 assistant 消息（content + tool_calls 可能分离）
    messages = merge_consecutive_assistant_messages(messages)

    # 修复 tool message 的 tool_call_id：Codex CLI 的历史里偶尔会出现
    # function_call_output 的 call_id 为空(历史压缩 / 序列化丢字段等),
    # 直接送给 Kimi/上游会得到 `400 tool_call_id  is not found`(空字符串
    # 拼到 error 模板里产生双空格)。按位置从紧邻的前一条 assistant.tool_calls
    # 里补 ID;无法配对的孤儿 tool message 直接丢弃避免上游 400。
    messages = _repair_tool_call_ids(messages)

    # Thinking + tool-call loops must replay assistant reasoning_content.
    # 思考模式 + 工具调用循环必须回传 assistant 的 reasoning_content。
    #
    # Codex may send full Responses history instead of previous_response_id,
    # and DeepSeek thinking can be enabled by provider requestOptions rather
    # than a request-level `reasoning` field. Repair after all message merging
    # and tool-call pairing has settled.
    #
    # Codex 可能直接发送完整 Responses 历史，而不是 previous_response_id；
    # DeepSeek thinking 也可能由 provider.requestOptions 开启，而不是请求体里的
    # `reasoning` 字段。这里等消息合并和 tool_call 配对完成后再统一修复。
    _ensure_thinking_tool_call_reasoning(messages, original, provider)

    # 非 OpenAI 官方 provider 时，developer → system
    if provider and isinstance(provider, dict):
        provider_name = str(provider.get("name") or "").lower()
        # OpenAI 官方（不含 azure）保留 developer；其余全部降级为 system
        is_openai_official = (
            "openai" in provider_name and "azure" not in provider_name
        )
        if not is_openai_official:
            messages = convert_developer_to_system(messages)
    else:
        # 无 provider 信息时，默认降级为 system（最广兼容）
        messages = convert_developer_to_system(messages)

    # ------------------------------------------------------------------ #
    # 2. 组装 Chat Completions 请求体
    # ------------------------------------------------------------------ #
    result: dict[str, Any] = {}

    # model
    if "model" in original:
        result["model"] = original["model"]

    # messages
    if messages:
        result["messages"] = messages

    # tools
    tools = original.get("tools")
    if tools and isinstance(tools, list):
        chat_tools = []
        for tool in tools:
            if isinstance(tool, dict):
                converted = convert_responses_tool_to_chat_tool(tool)
                if converted:
                    chat_tools.append(converted)
        if chat_tools:
            result["tools"] = chat_tools

    # tool_choice
    tool_choice = original.get("tool_choice")
    if tool_choice is not None:
        result["tool_choice"] = build_tool_choice(tool_choice)

    # text.format → response_format
    text = original.get("text")
    if isinstance(text, dict):
        response_format = build_response_format(text)
        if response_format:
            result["response_format"] = response_format

    # reasoning → reasoning_effort
    reasoning = original.get("reasoning")
    if reasoning is not None:
        reasoning_effort = build_reasoning_effort(reasoning)
        if reasoning_effort is not None:
            result["reasoning_effort"] = reasoning_effort

    # max_output_tokens → max_tokens
    if "max_output_tokens" in original:
        result["max_tokens"] = original["max_output_tokens"]

    # ------------------------------------------------------------------ #
    # 3. 特殊参数处理（store / metadata / prediction / service_tier /
    #    modalities / audio）
    # ------------------------------------------------------------------ #
    _body_special = copy.deepcopy(original)
    handle_openai_store_param(_body_special)
    handle_openai_metadata_param(_body_special)
    handle_openai_prediction_param(_body_special)
    handle_openai_service_tier(_body_special)
    handle_openai_modalities(_body_special)
    handle_openai_audio_params(_body_special)

    for key in (
        "store",
        "metadata",
        "prediction",
        "service_tier",
        "modalities",
        "audio",
    ):
        if key in _body_special:
            result[key] = _body_special[key]

    # ------------------------------------------------------------------ #
    # 4. 透传其他标准字段
    # ------------------------------------------------------------------ #
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
        "response_format",      # 允许直接透传（兼容层）
        "reasoning_effort",     # 允许直接透传
        "max_completion_tokens",  # 某些 provider 使用此参数
        "safety_identifier",
        "safety_settings",
        "context",
        "truncate",
        "prompt_truncation",
        "extra_headers",
        "extra_query",
        "extra_body",
        "timeout",
    ):
        if key in original and key not in result:
            result[key] = original[key]

    # ------------------------------------------------------------------ #
    # 5. stream & stream_options
    # ------------------------------------------------------------------ #
    result["stream"] = stream
    if stream:
        result["stream_options"] = {"include_usage": True}

    # ------------------------------------------------------------------ #
    # 6. 参数白名单过滤
    # ------------------------------------------------------------------ #
    result = filter_chat_completion_params(result, strict=False)

    # ------------------------------------------------------------------ #
    # 7. Provider workaround
    # ------------------------------------------------------------------ #
    if provider and isinstance(provider, dict):
        result = await apply_request_workarounds(result, provider)

    return result


# --------------------------------------------------------------------------- #
# Messages 构建
# --------------------------------------------------------------------------- #


def _provider_looks_like(provider: dict | None, needle: str) -> bool:
    if not isinstance(provider, dict):
        return False
    haystack = " ".join(
        str(provider.get(key) or "").lower()
        for key in ("id", "name", "baseUrl")
    )
    return needle.lower() in haystack


def _provider_chat_thinking_enabled(provider: dict | None) -> bool:
    """Whether provider config forces Chat Completions thinking mode on.

    判断 provider 配置是否强制开启 Chat Completions thinking 模式。
    """
    if not isinstance(provider, dict):
        return False
    options = provider.get("requestOptions") or {}
    if not isinstance(options, dict):
        return False
    chat_options = options.get("chat", options)
    if not isinstance(chat_options, dict):
        return False

    thinking = chat_options.get("thinking")
    if isinstance(thinking, dict):
        thinking_type = str(thinking.get("type") or "").lower()
        if thinking_type and thinking_type != "disabled":
            return True
    elif thinking:
        return True

    return bool(chat_options.get("reasoning_effort"))


def _request_thinking_enabled(body: dict, provider: dict | None) -> bool:
    if isinstance(body, dict) and body.get("reasoning") is not None:
        return True
    # DeepSeek V4 preset enables thinking in provider.requestOptions.chat.
    if _provider_looks_like(provider, "deepseek") and _provider_chat_thinking_enabled(provider):
        return True
    return False


def _ensure_thinking_tool_call_reasoning(
    messages: list[dict],
    body: dict,
    provider: dict | None,
) -> None:
    """Repair DeepSeek-style thinking history for tool-call continuations.

    修复 DeepSeek 风格 thinking 模式下工具调用续轮的历史消息。

    DeepSeek V4 thinking mode returns `reasoning_content` alongside `content`.
    Normal fresh user turns should not replay old reasoning, but tool-call
    continuations are stricter: once history contains a tool-call loop, DeepSeek
    validates assistant history for reasoning_content. When Codex history has
    only an opaque reasoning item, a single-space placeholder satisfies the API
    without inventing visible content.

    DeepSeek V4 thinking 模式会在 `content` 之外返回 `reasoning_content`。
    普通新用户轮不应回放旧推理，但工具调用续轮更严格：一旦历史里包含
    tool-call loop，DeepSeek 会校验 assistant 历史是否带 reasoning_content。
    当 Codex 历史里只有不透明 reasoning item 时，用单个空格占位即可满足
    API 要求，同时不会编造可见内容。
    """
    if not _request_thinking_enabled(body, provider):
        return
    has_tool_loop = any(
        isinstance(msg, dict)
        and (
            msg.get("role") == "tool"
            or (
                msg.get("role") == "assistant"
                and isinstance(msg.get("tool_calls"), list)
                and msg.get("tool_calls")
            )
        )
        for msg in messages
    )
    if not has_tool_loop:
        return

    for msg in messages:
        if not isinstance(msg, dict):
            continue
        if msg.get("role") != "assistant":
            continue
        if not (isinstance(msg.get("tool_calls"), list) and msg.get("tool_calls")):
            continue
        if not str(msg.get("reasoning_content") or "").strip():
            msg["reasoning_content"] = " "


def _build_messages_from_input(
    body: dict,
    session_cache: Any = None,
) -> list[dict]:
    """从 Responses API 请求体构建完整的 Chat Completions ``messages`` 数组。

    处理逻辑：
    1. ``instructions`` → 单条 ``role="system"`` message。
    2. 若存在 ``previous_response_id`` 且提供了 ``session_cache``：
       调用 ``session_cache.build_messages_with_history()`` 合并历史与当前 input。
    3. 否则直接展开 ``input`` 为 message 列表。

    Args:
        body: Responses API 请求体（深拷贝后的字典）。
        session_cache: ``ResponseSessionCache`` 实例或 ``None``。

    Returns:
        Chat Completions 格式的 message 字典列表。
    """
    messages: list[dict] = []

    # instructions → system message
    instructions = body.get("instructions")
    if instructions is not None:
        msg = _build_instructions_message(instructions)
        if msg:
            messages.append(msg)

    previous_response_id = body.get("previous_response_id")
    input_param = body.get("input")

    # 解码 previous_response_id：我们在响应阶段把上游的 chatcmpl-xxx 用
    # response_id_codec 编码成 ``resp_<base64>`` 发给 Codex（litellm 同款格式
    # 用于 deployment affinity）；Codex 把 encoded 形式作为 previous_response_id
    # 发回时,必须先解码回原始 chatcmpl-xxx 才能跟 session_cache 的 key 对上。
    # 不做这一步会导致 100% cache miss,messages 只剩当前 input,模型彻底失忆
    # 看到一堆孤儿 tool 又被 _repair_tool_call_ids 默默插入占位 assistant,
    # 最终上游收到一个无 user message 的合成对话,回复 "I'm here..."。
    # 等价于 litellm session_handler.py:284-291 的 _decode_responses_api_response_id 步骤。
    if previous_response_id:
        try:
            from backend.response_id_codec import decode_response_id
            previous_response_id = (
                decode_response_id(previous_response_id).get("response_id")
                or previous_response_id
            )
        except Exception:
            pass

    if previous_response_id and session_cache is not None:
        # 使用 session_cache 合并历史消息与当前 input
        current_messages = _convert_input_param_to_messages(input_param)
        # build_messages_with_history 返回的是新列表（内部已做浅拷贝）
        merged = session_cache.build_messages_with_history(
            previous_response_id, current_messages
        )
        # 若历史包含 system message，避免与 instructions 重复
        has_system = any(
            m.get("role") in ("system", "developer") for m in merged
        )
        if has_system and messages and messages[0].get("role") == "system":
            messages = messages[1:]  # 移除重复的 instructions system
        messages.extend(merged)
    else:
        # 无 previous_response_id 或无 session_cache：直接处理 input
        input_messages = _convert_input_param_to_messages(input_param)
        messages.extend(input_messages)

    return messages


def _build_instructions_message(instructions: Any) -> dict | None:
    """将 ``instructions`` 转换为单条 system message。

    支持：
    - ``str`` → 直接作为 content
    - ``dict`` → 提取 ``text`` / ``content`` 字段，或整体 JSON 序列化
    - 其他 → ``str()`` 转换

    Args:
        instructions: Responses API 的 ``instructions`` 值。

    Returns:
        Chat Completions message 字典，或 ``None``（当 instructions 为空时）。
    """
    if instructions is None:
        return None

    if isinstance(instructions, str):
        if not instructions.strip():
            return None
        return {"role": "system", "content": instructions}

    if isinstance(instructions, dict):
        text = instructions.get("text") or instructions.get("content")
        if text and isinstance(text, str):
            return {"role": "system", "content": text}
        # 兜底：将整个 dict 序列化为 JSON 字符串
        return {
            "role": "system",
            "content": json.dumps(instructions, ensure_ascii=False),
        }

    # 其他类型（int / float / list 等）
    content = str(instructions)
    if not content.strip():
        return None
    return {"role": "system", "content": content}


def _repair_tool_call_ids(messages: list[dict]) -> list[dict]:
    """确保 tool messages 都能对上前面 assistant 的 tool_calls,避免上游 400。

    Codex CLI 在 ``previous_response_id`` 模式下经常只发 ``function_call_output``
    而省略前面的 ``function_call``。converter 翻译之后我们手里的 messages 长这样:

        [..., user, tool(tcid=X)]    ← 缺 assistant.tool_calls=[X] 这一条!

    上游 Kimi / DeepSeek 严格校验这种配对,看不到对应 tool_call 就拒收
    ``400 tool_call_id is not found``。

    本 pass 做两件事:
    1. tool message 的 ``tool_call_id`` 为空时,按位置从前面紧邻 assistant 的
       ``tool_calls[].id`` 补上;
    2. tool message 的 ``tool_call_id`` 非空但前面 assistant 没有对应 tool_call
       (典型 Codex CLI 增量场景)时,从 ``TOOL_CALLS_CACHE`` 取回 tool_call 完整
       定义,补到前面 assistant 的 tool_calls 列表里;cache 没命中就插入一条
       占位 assistant 消息,保证 tool 不变孤儿。

    这是 litellm ``_ensure_tool_results_have_corresponding_tool_calls`` 的同款
    思路:不要求 client / session_cache 完美保持上下文,而是用 tool_call 级别
    的小 cache + 智能重建,容错性最高。
    """
    if not isinstance(messages, list):
        return messages

    # 延迟 import 避免循环依赖（session_cache 不依赖 responses_adapter）
    try:
        from backend.session_cache import TOOL_CALLS_CACHE
    except Exception:
        TOOL_CALLS_CACHE = None

    repaired: list[dict] = []
    available_call_ids: list[str] = []  # 来自最近 assistant 的待消费 tool_call IDs
    consumed: set[str] = set()
    last_assistant_idx: int | None = None  # 指向 repaired 里最近一条 assistant 的下标

    def _ensure_assistant_has_tool_call(asst_msg: dict, call_id: str) -> bool:
        """如果 asst_msg.tool_calls 里没有 call_id,从 TOOL_CALLS_CACHE 取回补上。

        返回是否已确保 call_id 在 asst_msg.tool_calls 里。
        """
        existing = asst_msg.get("tool_calls")
        if not isinstance(existing, list):
            existing = []
            asst_msg["tool_calls"] = existing
        if any(isinstance(tc, dict) and tc.get("id") == call_id for tc in existing):
            return True
        # cache 兜底
        if TOOL_CALLS_CACHE is not None:
            cached = TOOL_CALLS_CACHE.get(call_id)
            if cached and isinstance(cached, dict):
                existing.append(cached)
                # content 字段是 chat completion 标准要求,空也得给
                asst_msg.setdefault("content", "")
                return True
        # 没缓存也补一个最小 stub,虽然 name 是空的可能上游不接受,
        # 但比让上游看到孤儿 tool 直接 400 强一些
        existing.append({
            "id": call_id,
            "type": "function",
            "function": {"name": "", "arguments": "{}"},
        })
        asst_msg.setdefault("content", "")
        return True

    for msg in messages:
        if not isinstance(msg, dict):
            repaired.append(msg)
            continue

        role = msg.get("role")

        if role == "assistant":
            tool_calls = msg.get("tool_calls")
            if isinstance(tool_calls, list) and tool_calls:
                available_call_ids = [
                    str(tc.get("id") or "")
                    for tc in tool_calls
                    if isinstance(tc, dict) and (tc.get("id") or "")
                ]
                consumed = set()
            repaired.append(msg)
            last_assistant_idx = len(repaired) - 1
            continue

        if role == "tool":
            tcid = str(msg.get("tool_call_id") or "").strip()

            # 路径 A: 空 tool_call_id → 按位置补
            if not tcid:
                unused = [aid for aid in available_call_ids if aid and aid not in consumed]
                if unused:
                    fixed = dict(msg)
                    fixed["tool_call_id"] = unused[0]
                    consumed.add(unused[0])
                    repaired.append(fixed)
                # 真没法救的孤儿,丢弃
                continue

            # 路径 B: 非空 tool_call_id → 检查前面 assistant 是否有对应 tool_call
            if tcid not in available_call_ids:
                # 缺 assistant.tool_calls[tcid],尝试从 cache 重建
                target_assistant: dict | None = None
                if last_assistant_idx is not None:
                    candidate = repaired[last_assistant_idx]
                    # 只有当紧邻前一条是 assistant(可能含/不含 tool_calls)时才补
                    if isinstance(candidate, dict) and candidate.get("role") == "assistant":
                        target_assistant = candidate
                if target_assistant is None:
                    # 前面没有任何 assistant 可挂载,插一条占位 assistant
                    placeholder = {"role": "assistant", "content": "", "tool_calls": []}
                    repaired.append(placeholder)
                    last_assistant_idx = len(repaired) - 1
                    target_assistant = placeholder
                _ensure_assistant_has_tool_call(target_assistant, tcid)
                # 把刚补的 ID 加入 available 防止其他 tool 重新触发补充
                if tcid not in available_call_ids:
                    available_call_ids.append(tcid)

            consumed.add(tcid)
            repaired.append(msg)
            continue

        # 其他 role: user / system / developer 打断 tool_call 上下文
        if role in ("user", "system", "developer"):
            available_call_ids = []
            consumed = set()
            last_assistant_idx = None
        repaired.append(msg)

    return repaired


def _extract_reasoning_text(item: dict) -> str:
    """从 Responses API 的 ``reasoning`` item 中提取可读文本。

    优先级：``summary[].text`` → ``content[].text``。``encrypted_content`` 是
    不透明字符串，无法作为 ``reasoning_content`` 使用。
    """
    parts: list[str] = []
    summaries = item.get("summary")
    if isinstance(summaries, list):
        for s in summaries:
            if isinstance(s, dict):
                t = s.get("text")
                if isinstance(t, str) and t.strip():
                    parts.append(t)
    if not parts:
        content = item.get("content")
        if isinstance(content, list):
            for blk in content:
                if isinstance(blk, dict):
                    t = blk.get("text")
                    if isinstance(t, str) and t.strip():
                        parts.append(t)
    return "\n".join(parts)


def _convert_input_param_to_messages(input_param: Any) -> list[dict]:
    """将 Responses API 的 ``input`` 参数转换为 Chat Completions messages。

    支持：
    - ``str`` → 单条 user message
    - ``dict``（单条 item）→ 调用 ``convert_input_item_to_message``
    - ``list[dict]`` → 逐条转换并合并连续 assistant message

    Reasoning 处理：连续 ``reasoning`` items 的文本被缓存，附加到紧随其后的
    第一条 ``assistant`` 消息的 ``reasoning_content`` 字段。这是 Kimi/DeepSeek
    等开启 thinking 的上游所要求的格式（assistant tool_call 消息必须带
    ``reasoning_content``）。被中断（遇到 user/tool 消息）时缓存清空。

    Args:
        input_param: Responses API 请求中的 ``input`` 字段。

    Returns:
        Chat Completions message 字典列表。
    """
    items = _extract_input_items(input_param)
    messages: list[dict] = []
    pending_reasoning_parts: list[str] = []
    has_pending_reasoning = False

    def take_pending() -> tuple[bool, str]:
        nonlocal pending_reasoning_parts, has_pending_reasoning
        had = has_pending_reasoning
        text = "\n".join(pending_reasoning_parts)
        pending_reasoning_parts = []
        has_pending_reasoning = False
        return had, text

    for item in items:
        # reasoning item: 不产生独立 message，缓存文本等下一条 assistant 消化
        if isinstance(item, dict) and item.get("type") == "reasoning":
            text = _extract_reasoning_text(item)
            if text:
                pending_reasoning_parts.append(text)
            has_pending_reasoning = True
            continue

        item_messages = convert_input_item_to_message(item)
        for msg in item_messages:
            msg = dict(msg)
            if msg.get("role") == "assistant":
                had, text = take_pending()
                if had and not str(msg.get("reasoning_content") or "").strip():
                    # text 为空时用单空格,避免 Kimi/DeepSeek thinking 模式 400
                    msg["reasoning_content"] = text if text.strip() else " "
            else:
                # 非 assistant 消息（user/tool/system）打断 reasoning 上下文
                take_pending()

            # 合并连续 assistant message（特别是 content + tool_calls 分离的情况）
            if (
                msg.get("role") == "assistant"
                and messages
                and messages[-1].get("role") == "assistant"
            ):
                prev = messages[-1]
                # 合并 content
                new_content = msg.get("content")
                if new_content:
                    prev_content = prev.get("content")
                    if prev_content and isinstance(prev_content, str) and isinstance(new_content, str):
                        prev["content"] = prev_content + "\n" + new_content
                    else:
                        prev["content"] = new_content
                # 合并 tool_calls
                new_tool_calls = msg.get("tool_calls")
                if new_tool_calls and isinstance(new_tool_calls, list):
                    existing = prev.setdefault("tool_calls", [])
                    if isinstance(existing, list):
                        existing.extend(copy.deepcopy(new_tool_calls))
                # 合并 reasoning_content
                if "reasoning_content" in msg:
                    rc = msg["reasoning_content"]
                    prev_rc = prev.get("reasoning_content")
                    if prev_rc and rc:
                        prev["reasoning_content"] = prev_rc + "\n" + rc
                    elif rc or "reasoning_content" not in prev:
                        prev["reasoning_content"] = rc
                if not prev.get("content"):
                    prev["content"] = ""
            else:
                messages.append(copy.deepcopy(msg))

    # 收尾：若结尾仍有未消化的 reasoning（理论上不会，因为 input 总是以 user/tool 结束），
    # 直接丢弃，避免污染请求。
    take_pending()

    return messages


# --------------------------------------------------------------------------- #
# Input item 转换
# --------------------------------------------------------------------------- #


def _extract_input_items(input_param: Any) -> list[dict]:
    """从 Responses API 的 ``input`` 参数中提取标准化 input item 列表。

    支持：
    - ``None`` → ``[]``
    - ``str`` → ``[{"type": "message", "role": "user", "content": <str>}]``
    - ``dict`` → 单元素列表（需含 ``type`` 字段；若无则包装为 message）
    - ``list`` → 逐元素处理（str 包装为 message，dict 直接保留）
    - 其他 → 包装为字符串 message

    Args:
        input_param: 原始 ``input`` 参数。

    Returns:
        标准化的 input item 字典列表。
    """
    if input_param is None:
        return []

    if isinstance(input_param, str):
        return [{"type": "message", "role": "user", "content": input_param}]

    if isinstance(input_param, dict):
        # 单条 item：若已含 type 则保留，否则包装为 message
        if "type" in input_param:
            return [dict(input_param)]
        return [{"type": "message", "role": "user", "content": dict(input_param)}]

    if isinstance(input_param, list):
        items: list[dict] = []
        for elem in input_param:
            if isinstance(elem, dict):
                if "type" in elem:
                    items.append(dict(elem))
                else:
                    items.append({
                        "type": "message",
                        "role": elem.get("role", "user"),
                        "content": elem.get("content", dict(elem)),
                    })
            elif isinstance(elem, str):
                items.append({"type": "message", "role": "user", "content": elem})
            else:
                items.append({"type": "message", "role": "user", "content": str(elem)})
        return items

    # 兜底：其他类型包装为字符串
    return [{"type": "message", "role": "user", "content": str(input_param)}]


def _responses_block_to_chat_block(block: Any) -> dict | None:
    """把 Responses API 的 content block 翻译成 OpenAI Chat 接受的形式。

    Responses 用 ``input_text`` / ``output_text`` / ``input_image`` 等专属
    type，Chat 只认 ``text`` / ``image_url`` / ``input_audio`` / ``refusal``。
    严格的上游（如 Kimi For Coding）会拒掉未知 type，必须在此处归一化。
    """
    if isinstance(block, str):
        return {"type": "text", "text": block}
    if not isinstance(block, dict):
        return {"type": "text", "text": str(block)}

    btype = block.get("type")
    if btype in ("input_text", "output_text", "text"):
        text = block.get("text", "")
        return {"type": "text", "text": text if isinstance(text, str) else str(text)}
    if btype == "input_image":
        url = block.get("image_url") or block.get("url") or ""
        if isinstance(url, dict):
            return {"type": "image_url", "image_url": dict(url)}
        return {
            "type": "image_url",
            "image_url": {"url": url, "detail": block.get("detail", "auto")},
        }
    if btype == "image_url":
        return dict(block)
    if btype == "input_audio":
        audio = block.get("input_audio") or {
            "data": block.get("data", ""),
            "format": block.get("format", "wav"),
        }
        return {"type": "input_audio", "input_audio": dict(audio)}
    if btype == "refusal":
        return {"type": "refusal", "refusal": block.get("refusal", "")}
    if btype == "input_file":
        # 简化降级：保留文件名/ID 做文本提示，避免上游因未知 type 直接 400
        marker = block.get("filename") or block.get("file_id") or "input_file"
        return {"type": "text", "text": f"[input_file: {marker}]"}
    if btype is None and "text" in block:
        return {"type": "text", "text": str(block.get("text", ""))}
    # 兜底：未知 type 序列化为文本，保证上游一定能解析
    try:
        return {"type": "text", "text": json.dumps(block, ensure_ascii=False)}
    except (TypeError, ValueError):
        return {"type": "text", "text": str(block)}


def _normalize_message_content_for_chat(content: Any) -> Any:
    """把 message item 的 content 字段标准化为 Chat Completions 可接受的形式。"""
    if content is None or isinstance(content, str):
        return content
    if isinstance(content, list):
        return [
            blk for blk in (_responses_block_to_chat_block(b) for b in content) if blk
        ]
    if isinstance(content, dict):
        blk = _responses_block_to_chat_block(content)
        return [blk] if blk else ""
    return str(content)


def convert_input_item_to_message(item: dict) -> list[dict]:
    """将单个 Responses API input item 转换为一个或多个 Chat Completion messages。

    完整支持以下 input item 类型：

    - ``message`` → 对应 ``role`` 的 message（user / assistant / system / developer）
    - ``function_call`` → ``role="assistant"`` message with ``tool_calls``
    - ``function_call_output`` → ``role="tool"`` message
    - ``input_image`` → ``role="user"`` message with ``image_url`` content block
    - ``input_file`` → ``role="user"`` message（降级为文本提示或 data URI）
    - ``input_audio`` → ``role="user"`` message with ``input_audio`` content block
    - ``input_video`` → ``role="user"`` message（降级为 ``image_url`` 或文本）
    - ``reasoning`` → ``role="system"`` message（降级，用于历史回传）
    - 内置 call 类型（``file_search_call``、``web_search_call``、
      ``computer_call``、``code_interpreter_call``、``image_generation_call``）
      → 降级为 user message 文本提示
    - 未知类型 → 若含 ``content`` 则按 message 透传，否则返回空列表

    Args:
        item: Responses API input item 字典。

    Returns:
        Chat Completions 格式的 message 字典列表。每个 item 可能产生多条
        message（例如 ``function_call`` 产生 assistant + 后续可能需要 tool
        消息，但此处仅产生 assistant message）。
    """
    if not isinstance(item, dict):
        return []

    item_type = item.get("type")

    # ----------------------- message ----------------------- #
    if item_type == "message":
        role = item.get("role", "user")
        content = _normalize_message_content_for_chat(item.get("content", ""))
        return [{"role": role, "content": content}]

    # ----------------------- function_call ----------------------- #
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
                        "id": call_id or f"call_{uuid.uuid4().hex[:12]}",
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }
                ],
            }
        ]

    # ----------------------- function_call_output ----------------------- #
    if item_type == "function_call_output":
        # Codex CLI 有时把链接 ID 放在 call_id / tool_call_id / id 三种字段名下,
        # 真实情况(尤其历史压缩后)三个字段都可能为空。这里逐个 fallback,
        # 真正的孤儿在后续的 _repair_tool_call_ids 修复 pass 里处理。
        call_id = (
            item.get("call_id")
            or item.get("tool_call_id")
            or item.get("id", "")
        )
        output = item.get("output", "")
        if not isinstance(output, str):
            try:
                output = json.dumps(output, ensure_ascii=False)
            except (TypeError, ValueError):
                output = str(output)
        return [{"role": "tool", "tool_call_id": call_id, "content": output}]

    # ----------------------- input_image ----------------------- #
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

    # ----------------------- input_file ----------------------- #
    if item_type == "input_file":
        return _convert_file_item_to_message(item)

    # ----------------------- input_audio ----------------------- #
    if item_type == "input_audio":
        data = item.get("data", "")
        fmt = item.get("format", "wav")
        mime_type = item.get("mime_type") or f"audio/{fmt}"
        return [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_audio",
                        "input_audio": {"data": data, "format": fmt, "mime_type": mime_type},
                    }
                ],
            }
        ]

    # ----------------------- input_video ----------------------- #
    if item_type == "input_video":
        video_url = item.get("video_url") or item.get("url", "")
        if video_url:
            # 降级为 image_url（部分 provider 支持视频 URL）
            return [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": {"url": video_url, "detail": "auto"},
                        }
                    ],
                }
            ]
        return [{"role": "user", "content": "[Video input]"}]

    # ----------------------- reasoning ----------------------- #
    if item_type == "reasoning":
        summaries = item.get("summary", [])
        text_parts: list[str] = []
        if isinstance(summaries, list):
            for s in summaries:
                if isinstance(s, dict) and s.get("text"):
                    text_parts.append(str(s["text"]))
        if text_parts:
            return [{"role": "system", "content": "[Reasoning] " + "\n".join(text_parts)}]
        return []

    # ----------------------- 内置 call 类型（降级） ----------------------- #
    if item_type in (
        "file_search_call",
        "web_search_call",
        "computer_call",
        "code_interpreter_call",
        "image_generation_call",
    ):
        # 这些工具调用在 input 历史中出现时，降级为文本提示
        return [{"role": "user", "content": f"[{item_type}]"}]

    # ----------------------- 未知类型兜底 ----------------------- #
    content = item.get("content")
    if content is not None:
        role = item.get("role", "user")
        return [{"role": role, "content": _normalize_message_content_for_chat(content)}]

    return []


def _convert_file_item_to_message(item: dict) -> list[dict]:
    """将 ``input_file`` item 降级为 Chat Completions message。

    降级策略（按优先级）：
    1. 若包含 ``file_data``（base64 字符串），构造 data URI 并转为
       ``image_url`` content block（图片类型）或通用 ``image_url`` block。
    2. 若包含 ``file_id``，降级为文本提示 ``[File: <filename> (id=...)]``。
    3. 若包含 ``filename``，降级为文本提示 ``[File: <filename>]``。
    4. 完全无信息时，降级为 ``[File]``。

    Args:
        item: ``type="input_file"`` 的 input item 字典。

    Returns:
        Chat Completions 格式的 message 列表（通常为单元素列表）。
    """
    if not isinstance(item, dict):
        return []

    file_id = item.get("file_id") or item.get("id", "")
    file_data = item.get("file_data")
    filename = item.get("filename", "unknown")
    mime_type = item.get("mime_type", "application/octet-stream")

    # 优先使用 file_data（base64 内嵌）构造 data URI
    if file_data and isinstance(file_data, str):
        data_uri = f"data:{mime_type};base64,{file_data}"
        return [
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {"url": data_uri, "detail": "auto"},
                    }
                ],
            }
        ]

    # 次优：使用 file_id 或 filename 降级为文本提示
    if file_id and filename and filename != "unknown":
        return [{"role": "user", "content": f"[File: {filename} (id={file_id})]"}]
    if file_id:
        return [{"role": "user", "content": f"[File id={file_id}]"}]
    if filename and filename != "unknown":
        return [{"role": "user", "content": f"[File: {filename}]"}]

    # 兜底
    return [{"role": "user", "content": "[File]"}]


# --------------------------------------------------------------------------- #
# Response ID 编码（供上层在发送请求前预生成，用于后续 affinity）
# --------------------------------------------------------------------------- #


def encode_provider_response_id(
    provider: dict | None,
    model: str | None,
    upstream_request_id: str | None = None,
) -> str:
    """编码包含 provider 信息的 response_id，用于 deployment affinity。

    若未提供 ``upstream_request_id``，则自动生成一个唯一 ID。

    Args:
        provider: Provider 配置字典。
        model: 模型名称。
        upstream_request_id: 上游原始请求/响应 ID。

    Returns:
        形如 ``resp_xxx`` 的编码字符串。
    """
    provider_name = provider.get("name") if isinstance(provider, dict) else None
    req_id = upstream_request_id or f"req_{uuid.uuid4().hex[:16]}"
    return encode_response_id(provider_name, model, req_id)
