"""流式响应适配器 - 将 OpenAI Chat Completions SSE 流转换为 Responses API SSE 事件流。

本模块提供完整的流式状态机，负责：
1. 将上游 Chat Completions 逐 chunk 翻译为 Responses API 标准 SSE 事件
2. 管理 output item 的生命周期（added → delta → done）
3. 累积 tool call name / arguments，确保 ``.done`` 事件包含完整内容
4. 收集并归一化 usage（含 cached_tokens、audio_tokens、reasoning_tokens 等子字段）
5. 提供 ``StreamChunkBuilder`` 用于从事件序列重建完整响应

设计原则：
- 尽可能全，而非尽可能少 —— 处理所有边界情况（空 chunk、无 choices、仅 usage chunk 等）。
- 不修改原始 chunk 字典。
- 所有事件 dict 兼容 ResponsesAPIStreamingResponse 的 Discriminator 约束。
"""

from __future__ import annotations

import json
import uuid
from typing import Any, Optional

from backend.adapter_utils import _transform_usage_details

# --------------------------------------------------------------------------- #
# 公共辅助函数
# --------------------------------------------------------------------------- #


def normalize_usage_detail(usage: dict[str, Any] | None) -> dict[str, Any]:
    """将上游 usage 对象归一化为 Responses API 标准格式。

    包装 ``backend.adapter_utils._transform_usage_details`` 并提供更宽松的
    空值处理。

    Args:
        usage: 原始 usage 字典或 ``None``。

    Returns:
        包含 ``input_tokens``、``output_tokens``、``total_tokens`` 的标准字典，
        以及可选的 ``cached_tokens``、``audio_tokens``、``reasoning_tokens``。
    """
    return _transform_usage_details(usage)


def normalize_chat_chunk(chunk: dict[str, Any]) -> dict[str, Any]:
    """规范化 Chat Completions chunk 格式。

    确保返回值至少包含以下结构：
    ``{"id": ..., "choices": [{"delta": {...}, "finish_reason": ...}], "usage": ...}``

    对于仅含 usage 的 chunk（``choices`` 为空列表或缺失），保留 usage 并返回
    标准化后的字典。

    Args:
        chunk: 上游原始 chunk 字典。

    Returns:
        规范化后的新字典（浅拷贝，不修改原始 chunk）。
    """
    if not isinstance(chunk, dict):
        return {"id": None, "choices": [], "usage": None}

    normalized: dict[str, Any] = dict(chunk)

    # 确保 choices 为列表
    choices = normalized.get("choices")
    if not isinstance(choices, list):
        normalized["choices"] = []

    # 规范化每个 choice
    for i, choice in enumerate(normalized["choices"]):
        if not isinstance(choice, dict):
            normalized["choices"][i] = {"delta": {}, "finish_reason": None}
            continue
        if "delta" not in choice or not isinstance(choice["delta"], dict):
            choice["delta"] = {}
        if "finish_reason" not in choice:
            choice["finish_reason"] = None

    # 确保 usage 要么是 dict 要么是 None
    usage = normalized.get("usage")
    if usage is not None and not isinstance(usage, dict):
        normalized["usage"] = None

    return normalized


def is_usage_chunk(chunk: dict[str, Any]) -> bool:
    """判断是否为仅包含 usage 的 chunk。

    当 ``stream_options={"include_usage": True}`` 时，OpenAI 会在流末尾发送
    一个 ``choices=[]`` 但含 ``usage`` 的 chunk。

    Args:
        chunk: 原始 chunk 字典。

    Returns:
        如果 chunk 不包含任何有意义的 choice delta，则返回 ``True``。
    """
    if not isinstance(chunk, dict):
        return False

    choices = chunk.get("choices")
    if not isinstance(choices, list) or not choices:
        return True

    # 检查是否存在任何非空 delta
    for choice in choices:
        if not isinstance(choice, dict):
            continue
        delta = choice.get("delta") or {}
        if not isinstance(delta, dict):
            continue
        if any(v for v in delta.values() if v is not None):
            return False
        finish_reason = choice.get("finish_reason")
        if finish_reason is not None:
            return False

    return True


def create_error_event(message: str, code: str | None = None) -> dict[str, Any]:
    """创建标准 error SSE 事件。

    Args:
        message: 错误描述文本。
        code: 错误码（可选）。

    Returns:
        符合 ``ErrorEvent`` 结构的事件字典。
    """
    error_obj: dict[str, Any] = {
        "type": "invalid_request_error",
        "code": code or "unknown_error",
        "message": message,
        "param": None,
    }
    return {
        "type": "error",
        "sequence_number": 0,
        "error": error_obj,
    }


def create_completed_event(response: dict[str, Any]) -> dict[str, Any]:
    """创建 ``response.completed`` SSE 事件。

    Args:
        response: 完整的 Responses API 响应字典（应包含 ``id``、``status``、
            ``model``、``output``、``usage`` 等字段）。

    Returns:
        符合 ``ResponseCompletedEvent`` 结构的事件字典。
    """
    return {
        "type": "response.completed",
        "response": response,
    }


# --------------------------------------------------------------------------- #
# StreamingAdapter — 流式状态机
# --------------------------------------------------------------------------- #


class StreamingAdapter:
    """流式状态机，处理 Chat Completions → Responses API 的流式转换。

    对于每个上游 Chat Completions chunk，``process_chunk`` 返回一个或多个
    Responses API 标准事件。状态机负责：

    - 首次 chunk 时发送 ``response.created`` + ``response.in_progress``
    - 首次出现某类内容时发送 ``response.output_item.added`` +
      ``response.content_part.added``
    - 内容增量时发送对应的 ``.delta`` 事件
    - 收到 ``finish_reason`` 时发送 ``.done`` 系列事件 +
      ``response.completed``
    - 收集 usage（含明细子字段）并在 completed 事件中注入
    """

    def __init__(self, model: str, provider_kind: str = "unknown") -> None:
        self.model = model
        self.provider_kind = provider_kind

        # --- ID 跟踪 ---------------------------------------------------------
        self.response_id: str | None = None
        self.message_item_id: str | None = None
        self.reasoning_item_id: str | None = None
        self.tool_call_ids: dict[int, str] = {}

        # --- output_index 映射 ------------------------------------------------
        self.tool_output_indices: dict[str, int] = {}
        self.message_output_index: int | None = None
        self.reasoning_output_index: int | None = None
        self.next_output_index: int = 0

        # --- 内容累积 ---------------------------------------------------------
        self.accumulated_text: str = ""
        self.accumulated_reasoning: str = ""
        self.tool_args: dict[str, str] = {}
        self.tool_names: dict[str, str] = {}

        # --- 状态标志 ---------------------------------------------------------
        self.sent_created: bool = False
        self.sent_message_item: bool = False
        self.sent_reasoning_item: bool = False
        self.added_tool_calls: set[str] = set()
        self.finished: bool = False

        # --- usage & sequence -------------------------------------------------
        self.usage: dict[str, Any] | None = None
        self.sequence_number: int = 0

        # --- provider 特有字段透传 -------------------------------------------
        self.provider_specific_fields: dict[str, Any] = {}

    # --- 内部辅助方法 -------------------------------------------------------

    def _next_seq(self) -> int:
        """获取下一个序列号并自增。"""
        self.sequence_number += 1
        return self.sequence_number

    def _get_response_id(self, chunk: dict[str, Any]) -> str:
        """从 chunk 提取或生成 response_id。"""
        if self.response_id is None:
            self.response_id = chunk.get("id") or f"resp_{uuid.uuid4().hex[:12]}"
        return self.response_id

    def _get_message_output_index(self) -> int:
        """为 message item 分配 output_index（首次调用时分配）。"""
        if self.message_output_index is None:
            self.message_output_index = self.next_output_index
            self.next_output_index += 1
        return self.message_output_index

    def _get_reasoning_output_index(self) -> int:
        """为 reasoning item 分配 output_index（首次调用时分配）。"""
        if self.reasoning_output_index is None:
            self.reasoning_output_index = self.next_output_index
            self.next_output_index += 1
        return self.reasoning_output_index

    def _get_tool_output_index(self, call_id: str) -> int:
        """为指定 tool call 分配 output_index（首次调用时分配）。"""
        if call_id not in self.tool_output_indices:
            self.tool_output_indices[call_id] = self.next_output_index
            self.next_output_index += 1
        return self.tool_output_indices[call_id]

    def _make_event(self, event_type: str, **fields: Any) -> dict[str, Any]:
        """构造带 sequence_number 的标准事件字典。"""
        event: dict[str, Any] = {"type": event_type, "sequence_number": self._next_seq()}
        event.update(fields)
        return event

    def _collect_provider_fields(self, chunk: dict[str, Any]) -> None:
        """收集 provider 特有的顶层字段（如 system_fingerprint 等）用于透传。"""
        for key in ("system_fingerprint", "service_tier", "model"):
            if key in chunk and chunk[key] is not None:
                self.provider_specific_fields[key] = chunk[key]

    # --- 生命周期事件 -------------------------------------------------------

    def _emit_created_and_in_progress(self) -> list[dict[str, Any]]:
        """发送 ``response.created`` 和 ``response.in_progress`` 事件。"""
        if self.sent_created:
            return []
        self.sent_created = True

        resp_stub = {
            "id": self.response_id,
            "object": "response",
            "status": "in_progress",
            "model": self.model,
        }
        return [
            self._make_event("response.created", response=resp_stub),
            self._make_event("response.in_progress", response=resp_stub),
        ]

    # --- Message (text) -----------------------------------------------------

    def _handle_message_added(self) -> list[dict[str, Any]]:
        """首次出现文本内容时，发送 message 的 added 事件。"""
        if self.sent_message_item:
            return []
        self.sent_message_item = True
        self.message_item_id = f"msg_{uuid.uuid4().hex[:12]}"
        output_index = self._get_message_output_index()

        return [
            self._make_event(
                "response.output_item.added",
                output_index=output_index,
                item={
                    "type": "message",
                    "id": self.message_item_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            ),
            self._make_event(
                "response.content_part.added",
                item_id=self.message_item_id,
                output_index=output_index,
                content_index=0,
                part={"type": "output_text", "text": "", "annotations": []},
            ),
        ]

    def _handle_text_delta(self, text: str) -> list[dict[str, Any]]:
        """处理文本增量，返回 output_text.delta 事件。"""
        if not text:
            return []
        self.accumulated_text += str(text)
        return [
            self._make_event(
                "response.output_text.delta",
                item_id=self.message_item_id,
                output_index=self.message_output_index,
                content_index=0,
                delta=str(text),
            )
        ]

    def _emit_message_done(self) -> list[dict[str, Any]]:
        """发送 message 相关的 done 事件（content_part.done + output_item.done）。"""
        if not self.sent_message_item or self.message_item_id is None:
            return []

        events: list[dict[str, Any]] = []
        output_index = self.message_output_index or 0

        # content_part.done
        events.append(
            self._make_event(
                "response.content_part.done",
                item_id=self.message_item_id,
                output_index=output_index,
                content_index=0,
                part={
                    "type": "output_text",
                    "text": self.accumulated_text,
                    "annotations": [],
                },
            )
        )

        # output_item.done
        content_blocks: list[dict[str, Any]] = []
        if self.accumulated_text:
            content_blocks.append(
                {"type": "output_text", "text": self.accumulated_text, "annotations": []}
            )
        events.append(
            self._make_event(
                "response.output_item.done",
                output_index=output_index,
                item={
                    "type": "message",
                    "id": self.message_item_id,
                    "status": "completed",
                    "role": "assistant",
                    "content": content_blocks,
                },
            )
        )
        return events

    # --- Reasoning ----------------------------------------------------------

    def _handle_reasoning_added(self) -> list[dict[str, Any]]:
        """首次出现 reasoning 内容时，发送 reasoning 的 added 事件。

        协议关键点：reasoning item 的 part 起点必须发
        ``response.reasoning_summary_part.added`` + ``summary_index``,
        与后续的 ``reasoning_summary_text.delta`` 配对。若错发成通用的
        ``response.content_part.added`` + ``content_index``,Codex CLI
        会显示「Thinking...」但永远等不到对应的 summary part 起点,
        delta 事件没有可挂载的 part,UI 卡死不展示思维内容。
        """
        if self.sent_reasoning_item:
            return []
        self.sent_reasoning_item = True
        self.reasoning_item_id = f"rs_{uuid.uuid4().hex[:12]}"
        output_index = self._get_reasoning_output_index()

        return [
            self._make_event(
                "response.output_item.added",
                output_index=output_index,
                item={
                    "type": "reasoning",
                    "id": self.reasoning_item_id,
                    "status": "in_progress",
                },
            ),
            self._make_event(
                "response.reasoning_summary_part.added",
                item_id=self.reasoning_item_id,
                output_index=output_index,
                summary_index=0,
                part={"type": "summary_text", "text": ""},
            ),
        ]

    def _handle_reasoning_delta(self, reasoning: str) -> list[dict[str, Any]]:
        """处理 reasoning 增量，返回 reasoning_summary_text.delta 事件。"""
        if not reasoning:
            return []
        self.accumulated_reasoning += str(reasoning)
        return [
            self._make_event(
                "response.reasoning_summary_text.delta",
                item_id=self.reasoning_item_id,
                output_index=self.reasoning_output_index,
                summary_index=0,
                delta=str(reasoning),
            )
        ]

    def _emit_reasoning_done(self) -> list[dict[str, Any]]:
        """发送 reasoning 相关的 done 事件。"""
        if not self.sent_reasoning_item or self.reasoning_item_id is None:
            return []

        events: list[dict[str, Any]] = []
        output_index = self.reasoning_output_index or 0

        # reasoning_summary_part.done —— 跟 _handle_reasoning_added 里的
        # reasoning_summary_part.added 配对收尾。事件名 / summary_index /
        # part.type 必须一致才能让 Codex CLI 正确闭合 thinking UI。
        events.append(
            self._make_event(
                "response.reasoning_summary_part.done",
                item_id=self.reasoning_item_id,
                output_index=output_index,
                summary_index=0,
                part={
                    "type": "summary_text",
                    "text": self.accumulated_reasoning,
                },
            )
        )

        # reasoning_summary_text.done（扩展事件，提供完整文本）
        events.append(
            self._make_event(
                "response.reasoning_summary_text.done",
                item_id=self.reasoning_item_id,
                output_index=output_index,
                summary_index=0,
                text=self.accumulated_reasoning,
            )
        )

        # output_item.done
        events.append(
            self._make_event(
                "response.output_item.done",
                output_index=output_index,
                item={
                    "type": "reasoning",
                    "id": self.reasoning_item_id,
                    "status": "completed",
                    "summary": [
                        {
                            "type": "reasoning_summary_text",
                            "text": self.accumulated_reasoning,
                        }
                    ],
                },
            )
        )
        return events

    # --- Tool Calls ---------------------------------------------------------

    def _resolve_tool_call_id(self, tc_index: int, tc_id: str | None) -> str:
        """解析并缓存 tool call ID。

        若 chunk 中提供了 ``id``，则使用之；否则生成一个稳定的伪 ID。
        """
        if tc_index in self.tool_call_ids:
            return self.tool_call_ids[tc_index]

        if tc_id:
            self.tool_call_ids[tc_index] = tc_id
            return tc_id

        generated = f"fc_{uuid.uuid4().hex[:12]}"
        self.tool_call_ids[tc_index] = generated
        return generated

    def _handle_tool_call_added(
        self, call_id: str, name: str
    ) -> list[dict[str, Any]]:
        """首次出现某 tool call 时，发送 function_call 的 added 事件。"""
        if call_id in self.added_tool_calls:
            return []
        self.added_tool_calls.add(call_id)
        self.tool_names[call_id] = name or ""
        output_index = self._get_tool_output_index(call_id)

        return [
            self._make_event(
                "response.output_item.added",
                output_index=output_index,
                item={
                    "type": "function_call",
                    "id": call_id,
                    "call_id": call_id,
                    "name": name or "",
                    "arguments": "",
                    "status": "in_progress",
                },
            )
        ]

    def _handle_tool_call_delta(
        self, call_id: str, name: str | None, args_delta: str | None
    ) -> list[dict[str, Any]]:
        """处理 tool call 增量，返回 function_call_arguments.delta 事件。"""
        events: list[dict[str, Any]] = []
        output_index = self._get_tool_output_index(call_id)

        # 累积 name（某些 provider 会在后续 chunk 中补发 name）
        if name:
            self.tool_names[call_id] = name

        # 累积 arguments
        if args_delta:
            self.tool_args[call_id] = self.tool_args.get(call_id, "") + str(args_delta)
            events.append(
                self._make_event(
                    "response.function_call_arguments.delta",
                    item_id=call_id,
                    output_index=output_index,
                    delta=str(args_delta),
                )
            )

        return events

    def _emit_tool_call_done(self, call_id: str) -> list[dict[str, Any]]:
        """发送单个 tool call 的 done 事件。"""
        events: list[dict[str, Any]] = []
        output_index = self.tool_output_indices.get(call_id, 0)
        final_args = self.tool_args.get(call_id, "")
        final_name = self.tool_names.get(call_id, "")

        # function_call_arguments.done
        events.append(
            self._make_event(
                "response.function_call_arguments.done",
                item_id=call_id,
                output_index=output_index,
                arguments=final_args,
            )
        )

        # output_item.done
        events.append(
            self._make_event(
                "response.output_item.done",
                output_index=output_index,
                item={
                    "type": "function_call",
                    "id": call_id,
                    "call_id": call_id,
                    "name": final_name,
                    "arguments": final_args,
                    "status": "completed",
                },
            )
        )
        return events

    def _emit_all_tool_calls_done(self) -> list[dict[str, Any]]:
        """发送所有已累积 tool call 的 done 事件。"""
        events: list[dict[str, Any]] = []
        for call_id in self.tool_output_indices:
            events.extend(self._emit_tool_call_done(call_id))
        return events

    # --- 主入口 --------------------------------------------------------------

    def process_chunk(self, chunk: dict[str, Any]) -> list[dict[str, Any]]:
        """处理单个 Chat Completions chunk，返回 Responses API 事件列表。

        处理流程：
        1. 提取 ``id``、``choices``、``usage``、``finish_reason``
        2. 首次调用时发送 ``response.created`` + ``response.in_progress``
        3. 按类型分发 delta（content / reasoning_content / tool_calls）
        4. 若出现 ``finish_reason``，发送对应 ``.done`` 事件 +
           ``response.completed``

        Args:
            chunk: 上游 Chat Completions chunk 字典。

        Returns:
            Responses API 事件字典列表（可能为空）。
        """
        if self.finished:
            return []

        events: list[dict[str, Any]] = []

        if not isinstance(chunk, dict):
            return events

        # 提取并保存 usage（可能出现在独立 chunk 中）
        raw_usage = chunk.get("usage")
        if isinstance(raw_usage, dict):
            self.usage = raw_usage

        # 收集 provider 特有字段
        self._collect_provider_fields(chunk)

        choices = chunk.get("choices")
        if not isinstance(choices, list) or not choices:
            # 仅 usage chunk，不生成事件（usage 在 completed 时统一注入）
            return events

        choice = choices[0]
        if not isinstance(choice, dict):
            return events

        delta = choice.get("delta") or {}
        if not isinstance(delta, dict):
            delta = {}

        finish_reason = choice.get("finish_reason")
        response_id = self._get_response_id(chunk)

        # --- 首次事件 ---------------------------------------------------------
        if not self.sent_created:
            events.extend(self._emit_created_and_in_progress())

        # --- 文本 content -----------------------------------------------------
        content = delta.get("content")
        if content is not None and content != "":
            events.extend(self._handle_message_added())
            events.extend(self._handle_text_delta(str(content)))

        # --- reasoning_content ------------------------------------------------
        reasoning_content = delta.get("reasoning_content")
        if reasoning_content is not None and reasoning_content != "":
            events.extend(self._handle_reasoning_added())
            events.extend(self._handle_reasoning_delta(str(reasoning_content)))

        # --- tool_calls -------------------------------------------------------
        tool_calls = delta.get("tool_calls")
        if tool_calls and isinstance(tool_calls, list):
            for tc in tool_calls:
                if not isinstance(tc, dict):
                    continue
                tc_index = tc.get("index", 0)
                if not isinstance(tc_index, int):
                    try:
                        tc_index = int(tc_index)
                    except (TypeError, ValueError):
                        tc_index = 0

                call_id = self._resolve_tool_call_id(tc_index, tc.get("id"))
                fn = tc.get("function") or {}
                if not isinstance(fn, dict):
                    fn = {}
                fn_name = fn.get("name", "")
                fn_args = fn.get("arguments", "")

                # 首次出现该 tool_call 时发送 added
                events.extend(self._handle_tool_call_added(call_id, fn_name))

                # 发送 delta
                events.extend(
                    self._handle_tool_call_delta(call_id, fn_name or None, fn_args or None)
                )

        # --- finish_reason：结束处理 ------------------------------------------
        if finish_reason is not None:
            self.finished = True

            # message done
            events.extend(self._emit_message_done())

            # reasoning done
            events.extend(self._emit_reasoning_done())

            # tool calls done
            events.extend(self._emit_all_tool_calls_done())

            # response.completed
            completed_response = self.build_final_response(finish_reason=finish_reason)
            events.append(
                self._make_event("response.completed", response=completed_response)
            )

        return events

    def build_final_response(
        self, finish_reason: str | None = None
    ) -> dict[str, Any]:
        """从累积状态构建最终响应（用于 ``response.completed`` 事件）。

        按照 output_index 顺序组装 ``output`` 列表，确保 message、reasoning、
        tool_call 的顺序与流式事件中的 ``output_index`` 一致。

        Args:
            finish_reason: 上游 finish_reason，用于推断最终 ``status``。

        Returns:
            完整的 Responses API 响应字典。
        """
        status = "incomplete" if finish_reason == "length" else "completed"

        output: list[dict[str, Any]] = []

        # 按 output_index 排序构建 output 列表
        indexed_items: dict[int, dict[str, Any]] = {}

        if self.sent_message_item and self.message_item_id is not None:
            idx = self.message_output_index or 0
            content_blocks: list[dict[str, Any]] = []
            if self.accumulated_text:
                content_blocks.append(
                    {"type": "output_text", "text": self.accumulated_text, "annotations": []}
                )
            indexed_items[idx] = {
                "type": "message",
                "id": self.message_item_id,
                "status": status,
                "role": "assistant",
                "content": content_blocks,
            }

        if self.sent_reasoning_item and self.reasoning_item_id is not None:
            idx = self.reasoning_output_index or 0
            indexed_items[idx] = {
                "type": "reasoning",
                "id": self.reasoning_item_id,
                "status": status,
                "summary": [
                    {
                        "type": "reasoning_summary_text",
                        "text": self.accumulated_reasoning,
                    }
                ],
            }

        for call_id in self.tool_output_indices:
            idx = self.tool_output_indices[call_id]
            indexed_items[idx] = {
                "type": "function_call",
                "id": call_id,
                "call_id": call_id,
                "name": self.tool_names.get(call_id, ""),
                "arguments": self.tool_args.get(call_id, ""),
                "status": status,
            }

        # 按索引排序并追加到 output
        for idx in sorted(indexed_items):
            output.append(indexed_items[idx])

        response: dict[str, Any] = {
            "id": self.response_id or f"resp_{uuid.uuid4().hex[:12]}",
            "object": "response",
            "status": status,
            "model": self.model,
            "output": output,
        }

        # 注入 usage
        if self.usage:
            response["usage"] = normalize_usage_detail(self.usage)
        else:
            response["usage"] = {
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0,
            }

        # 透传 provider 特有字段
        if self.provider_specific_fields:
            for key, value in self.provider_specific_fields.items():
                if key not in ("model",):
                    response[key] = value

        return response

    def is_finished(self) -> bool:
        """检查是否已处理完成（收到 ``finish_reason``）。"""
        return self.finished


# --------------------------------------------------------------------------- #
# StreamChunkBuilder — 从事件序列重建完整响应
# --------------------------------------------------------------------------- #


class StreamChunkBuilder:
    """从 Responses API 流事件序列构建完整响应。

    用于验证、调试或客户端侧重建完整响应体。
    """

    @staticmethod
    def build_response_from_events(
        events: list[dict[str, Any]], model: str
    ) -> dict[str, Any]:
        """从事件列表构建完整 Responses API 响应。

        遍历所有事件，聚合：
        - ``output_text.delta`` → 完整文本
        - ``reasoning_summary_text.delta`` → 完整 reasoning
        - ``function_call_arguments.delta`` → 完整参数
        - ``output_item.added/done`` → 完整 output items
        - ``response.completed`` 中的 usage / status

        Args:
            events: Responses API 事件字典列表。
            model: 模型标识（用于填充最终响应）。

        Returns:
            完整的 Responses API 响应字典。
        """
        if not events:
            return {
                "id": f"resp_{uuid.uuid4().hex[:12]}",
                "object": "response",
                "status": "completed",
                "model": model,
                "output": [],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "total_tokens": 0,
                },
            }

        # 累积容器
        text_by_item: dict[str, str] = {}
        reasoning_by_item: dict[str, str] = {}
        args_by_item: dict[str, str] = {}
        name_by_item: dict[str, str] = {}
        item_types: dict[str, str] = {}
        item_status: dict[str, str] = {}
        item_output_index: dict[str, int] = {}
        completed_response: dict[str, Any] | None = None
        response_id: str = ""

        for event in events:
            if not isinstance(event, dict):
                continue
            event_type = event.get("type", "")

            # 提取顶层 response_id
            if event_type in ("response.created", "response.in_progress", "response.completed"):
                resp = event.get("response") or {}
                if isinstance(resp, dict) and resp.get("id"):
                    response_id = resp["id"]

            # output_item.added
            if event_type == "response.output_item.added":
                item = event.get("item") or {}
                if isinstance(item, dict) and item.get("id"):
                    item_id = item["id"]
                    item_types[item_id] = item.get("type", "")
                    item_status[item_id] = item.get("status", "in_progress")
                    item_output_index[item_id] = event.get("output_index", 0)
                    if item.get("type") == "function_call":
                        name_by_item[item_id] = item.get("name", "")

            # output_text.delta
            if event_type == "response.output_text.delta":
                item_id = event.get("item_id", "")
                delta = event.get("delta", "")
                text_by_item[item_id] = text_by_item.get(item_id, "") + str(delta)

            # reasoning_summary_text.delta
            if event_type == "response.reasoning_summary_text.delta":
                item_id = event.get("item_id", "")
                delta = event.get("delta", "")
                reasoning_by_item[item_id] = reasoning_by_item.get(item_id, "") + str(delta)

            # function_call_arguments.delta
            if event_type == "response.function_call_arguments.delta":
                item_id = event.get("item_id", "")
                delta = event.get("delta", "")
                args_by_item[item_id] = args_by_item.get(item_id, "") + str(delta)

            # output_item.done（提取最终状态、name、arguments）
            if event_type == "response.output_item.done":
                item = event.get("item") or {}
                if isinstance(item, dict) and item.get("id"):
                    item_id = item["id"]
                    item_status[item_id] = item.get("status", "completed")
                    if item.get("type") == "function_call":
                        name_by_item[item_id] = item.get("name", "")
                        args_by_item[item_id] = item.get("arguments", "")

            # response.completed（保存最终响应骨架）
            if event_type == "response.completed":
                resp = event.get("response")
                if isinstance(resp, dict):
                    completed_response = dict(resp)

        # 组装 output 列表
        output: list[dict[str, Any]] = []
        indexed_output: dict[int, dict[str, Any]] = {}

        for item_id, item_type in item_types.items():
            idx = item_output_index.get(item_id, 0)
            status = item_status.get(item_id, "completed")

            if item_type == "message":
                text = text_by_item.get(item_id, "")
                content_blocks: list[dict[str, Any]] = []
                if text:
                    content_blocks.append(
                        {"type": "output_text", "text": text, "annotations": []}
                    )
                indexed_output[idx] = {
                    "type": "message",
                    "id": item_id,
                    "status": status,
                    "role": "assistant",
                    "content": content_blocks,
                }

            elif item_type == "reasoning":
                text = reasoning_by_item.get(item_id, "")
                indexed_output[idx] = {
                    "type": "reasoning",
                    "id": item_id,
                    "status": status,
                    "summary": [
                        {"type": "reasoning_summary_text", "text": text}
                    ],
                }

            elif item_type == "function_call":
                indexed_output[idx] = {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": item_id,
                    "name": name_by_item.get(item_id, ""),
                    "arguments": args_by_item.get(item_id, ""),
                    "status": status,
                }

        for idx in sorted(indexed_output):
            output.append(indexed_output[idx])

        # 构建最终响应
        result: dict[str, Any] = {
            "id": response_id or f"resp_{uuid.uuid4().hex[:12]}",
            "object": "response",
            "status": "completed",
            "model": model,
            "output": output,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0,
            },
        }

        if completed_response:
            result["status"] = completed_response.get("status", "completed")
            if "usage" in completed_response:
                result["usage"] = completed_response["usage"]
            for key in ("system_fingerprint", "service_tier"):
                if key in completed_response:
                    result[key] = completed_response[key]

        return result

    @staticmethod
    def extract_usage_from_events(events: list[dict[str, Any]]) -> dict[str, Any]:
        """从事件序列提取 usage 信息。

        优先使用 ``response.completed`` 事件中的 ``usage``；
        如果没有，则查找任何事件中的 ``usage`` 字段并归一化。

        Args:
            events: Responses API 事件字典列表。

        Returns:
            标准化 usage 字典。
        """
        # 优先查找 response.completed 中的 usage
        for event in events:
            if not isinstance(event, dict):
                continue
            if event.get("type") == "response.completed":
                resp = event.get("response") or {}
                if isinstance(resp, dict) and "usage" in resp:
                    usage = resp["usage"]
                    if isinstance(usage, dict):
                        return normalize_usage_detail(usage)

        # 回退：查找任何事件中的 usage 字段
        for event in events:
            if not isinstance(event, dict):
                continue
            usage = event.get("usage")
            if isinstance(usage, dict):
                return normalize_usage_detail(usage)

        # 最终回退
        return {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
        }


# --------------------------------------------------------------------------- #
# 兼容层：保留旧名称 ResponsesStreamConverter（指向 StreamingAdapter）
# --------------------------------------------------------------------------- #

ResponsesStreamConverter = StreamingAdapter
"""向后兼容别名。``ResponsesStreamConverter`` 即 ``StreamingAdapter``。"""
