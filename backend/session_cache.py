"""本地内存会话缓存（替代 LiteLLM 的 Prisma + Cold Storage）。

用于保存 Responses API 的对话历史，使 ``previous_response_id`` 可以在不访问
数据库的情况下恢复上下文。
"""

from __future__ import annotations

import threading
import time
from typing import Any


class ResponseSessionCache:
    """线程安全的内存会话缓存，支持 TTL 与 LRU 淘汰。

    Attributes:
        max_size: 最大缓存条目数。
        ttl_seconds: 默认存活时间（秒）。
    """

    def __init__(self, max_size: int = 1000, ttl_seconds: int = 3600):
        self.max_size = max(max_size, 1)
        self.ttl_seconds = ttl_seconds

        # 核心存储结构
        # _cache: {response_id: {"messages": [...], "ts": float_timestamp, "access_count": int}}
        self._cache: dict[str, dict[str, Any]] = {}
        self._lock = threading.Lock()

    # ------------------------------------------------------------------ #
    # Public API
    # ------------------------------------------------------------------ #

    def save(self, response_id: str, messages: list[dict]) -> None:
        """保存对话历史并附带当前时间戳。

        Args:
            response_id: 响应对应的 ID（通常是编码后的 ``resp_xxx``）。
            messages: 本次交互产生的消息列表（Chat Completion 格式）。
        """
        if not response_id:
            return

        with self._lock:
            self._evict_expired()
            # 若即将超出上限，先淘汰最旧的条目
            if len(self._cache) >= self.max_size and response_id not in self._cache:
                self._evict_oldest()

            self._cache[response_id] = {
                "messages": list(messages),
                "ts": time.time(),
                "access_count": 0,
            }

    def get(self, response_id: str) -> list[dict] | None:
        """获取指定响应 ID 对应的历史消息。

        若条目已过期，则自动清除并返回 ``None``。

        Args:
            response_id: 响应 ID。

        Returns:
            消息列表；不存在或已过期时返回 ``None``。
        """
        if not response_id:
            return None

        with self._lock:
            entry = self._cache.get(response_id)
            if entry is None:
                return None

            if self._is_expired(entry["ts"]):
                self._cache.pop(response_id, None)
                return None

            entry["access_count"] += 1
            # 返回深拷贝，避免外部修改污染缓存
            return [dict(msg) for msg in entry["messages"]]

    def build_messages_with_history(
        self,
        previous_response_id: str | None,
        current_input: list[dict] | dict | str | None,
    ) -> list[dict]:
        """合并历史消息与当前输入，构造完整的 messages 列表。

        当 ``previous_response_id`` 不为空且能在缓存中命中时，将历史消息
        前置；随后将 ``current_input`` 标准化为 Chat Completion message
        格式并追加到末尾。

        Args:
            previous_response_id: 上一次响应的 ID。
            current_input: 当前用户输入，支持多种形式：
                - ``str``: 直接作为 user message
                - ``dict``: 单条 message
                - ``list[dict]``: 多条 message
                - ``None``: 仅返回历史

        Returns:
            合并后的完整消息列表。
        """
        messages: list[dict] = []

        if previous_response_id:
            history = self.get(previous_response_id)
            if history:
                messages.extend(history)

        if current_input is None:
            return messages

        if isinstance(current_input, str):
            messages.append({"role": "user", "content": current_input})
            return messages

        if isinstance(current_input, dict):
            messages.append(dict(current_input))
            return messages

        if isinstance(current_input, list):
            for item in current_input:
                if isinstance(item, dict):
                    messages.append(dict(item))
                elif isinstance(item, str):
                    messages.append({"role": "user", "content": item})
            return messages

        return messages

    def delete(self, response_id: str) -> bool:
        """主动删除指定响应 ID 的缓存条目。

        Args:
            response_id: 响应 ID。

        Returns:
            是否成功删除。
        """
        with self._lock:
            if response_id in self._cache:
                del self._cache[response_id]
                return True
            return False

    def clear(self) -> None:
        """清空全部缓存。"""
        with self._lock:
            self._cache.clear()

    def stats(self) -> dict[str, int]:
        """返回当前缓存统计信息。

        Returns:
            包含 ``size``（当前条目数）和 ``max_size`` 的字典。
        """
        with self._lock:
            return {
                "size": len(self._cache),
                "max_size": self.max_size,
            }

    # ------------------------------------------------------------------ #
    # Internal helpers
    # ------------------------------------------------------------------ #

    def _is_expired(self, timestamp: float) -> bool:
        """判断给定时间戳是否已超出 TTL。"""
        return time.time() - timestamp > self.ttl_seconds

    def _evict_expired(self) -> int:
        """清除所有已过期条目，返回被清除的数量。"""
        expired_keys = [
            rid for rid, entry in self._cache.items()
            if self._is_expired(entry["ts"])
        ]
        for rid in expired_keys:
            self._cache.pop(rid, None)
        return len(expired_keys)

    def _evict_oldest(self) -> None:
        """按 LRU（最早写入）淘汰一条记录。

        优先淘汰 ``access_count`` 最小的；若相同则选时间戳最早的。
        """
        if not self._cache:
            return

        oldest_rid = min(
            self._cache.keys(),
            key=lambda rid: (
                self._cache[rid]["access_count"],
                self._cache[rid]["ts"],
            ),
        )
        self._cache.pop(oldest_rid, None)


class ToolCallCache:
    """以 ``call_id`` 为 key 缓存上游返回的 tool_call 完整定义。

    Codex CLI 在 ``previous_response_id`` 模式下经常只发增量
    ``function_call_output`` 而省略前面的 ``function_call``。我们在响应阶段
    把每个 tool_call 的 ``{id, type, function: {name, arguments}}`` 存进来,
    后续请求转换时如果发现 tool message 找不到对应的 assistant.tool_calls,
    用此 cache 重建一个最小可用的 tool_call,补到前面的 assistant 消息上,
    避免上游报 ``tool_call_id is not found``。这是 litellm 同款方案。
    """

    def __init__(self, max_size: int = 4096, ttl_seconds: int = 3600):
        self.max_size = max(max_size, 1)
        self.ttl_seconds = ttl_seconds
        self._cache: dict[str, dict[str, Any]] = {}
        self._lock = threading.Lock()

    def set(self, call_id: str, tool_call: dict) -> None:
        if not call_id or not isinstance(tool_call, dict):
            return
        with self._lock:
            if len(self._cache) >= self.max_size and call_id not in self._cache:
                # 简单 LRU：踢掉最旧
                oldest = min(self._cache.keys(), key=lambda k: self._cache[k]["ts"])
                self._cache.pop(oldest, None)
            self._cache[str(call_id)] = {"tool_call": dict(tool_call), "ts": time.time()}

    def get(self, call_id: str) -> dict | None:
        if not call_id:
            return None
        with self._lock:
            entry = self._cache.get(str(call_id))
            if not entry:
                return None
            if time.time() - entry["ts"] > self.ttl_seconds:
                self._cache.pop(str(call_id), None)
                return None
            return dict(entry["tool_call"])

    def clear(self) -> None:
        with self._lock:
            self._cache.clear()


# 全局单例：响应阶段 populate,请求阶段 lookup。
TOOL_CALLS_CACHE = ToolCallCache()
