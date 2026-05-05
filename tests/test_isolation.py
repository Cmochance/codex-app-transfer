#!/usr/bin/env python3
"""Codex App Transfer 隔离测试脚本

测试范围：
1. 模块导入
2. 核心转换函数（无需 HTTP）
3. Admin API 启动 + 端点测试
4. Proxy API 启动 + 端点测试
5. 本地 mock 上游 + 端到端请求转发
"""

import asyncio
import json
import os
import socket
import sys
import tempfile
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import httpx
import uvicorn
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse

# ── 颜色输出 ──
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
RESET = "\033[0m"

passed = 0
failed = 0


def ok(msg: str):
    global passed
    passed += 1
    print(f"  {GREEN}✓{RESET} {msg}")


def err(msg: str):
    global failed
    failed += 1
    print(f"  {RED}✗{RESET} {msg}")


def info(msg: str):
    print(f"  {YELLOW}ℹ{RESET} {msg}")


def free_port() -> int:
    """Ask the OS for an unused localhost port.

    向操作系统申请一个当前空闲的 localhost 端口。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


# ============================================================================
# 1. 模块导入测试
# ============================================================================
print("\n" + "=" * 60)
print("1. 模块导入测试")
print("=" * 60)

MODULES = [
    "backend.session_cache",
    "backend.response_id_codec",
    "backend.deployment_affinity",
    "backend.adapter_utils",
    "backend.base_adapter",
    "backend.provider_workarounds",
    "backend.openai_adapter",
    "backend.responses_adapter",
    "backend.chat_responses_adapter",
    "backend.streaming_adapter",
    "backend.api_adapters",
    "backend.proxy",
    "backend.main",
    "backend.config",
    "backend.model_alias",
    "backend.provider_tools",
    "backend.update",
    "backend.registry",
]

for mod in MODULES:
    try:
        __import__(mod)
        ok(f"import {mod}")
    except Exception as e:
        err(f"import {mod}: {e}")


# ============================================================================
# 2. 核心转换函数单元测试（无需 HTTP）
# ============================================================================
print("\n" + "=" * 60)
print("2. 核心转换函数单元测试")
print("=" * 60)


def test_normalize_api_format():
    from backend.api_adapters import normalize_api_format
    assert normalize_api_format("openai_chat") == "openai_chat"
    assert normalize_api_format("responses") == "responses"
    assert normalize_api_format("anthropic") == "responses"
    assert normalize_api_format("claude") == "responses"
    assert normalize_api_format("") == "responses"
    ok("normalize_api_format")


def test_response_id_codec():
    from backend.response_id_codec import encode_response_id, decode_response_id
    encoded = encode_response_id("deepseek", "deepseek-chat", "req_123")
    assert encoded.startswith("resp_")
    decoded = decode_response_id(encoded)
    assert decoded.get("custom_llm_provider") == "deepseek"
    assert decoded.get("model_id") == "deepseek-chat"
    assert decoded.get("response_id") == "req_123"
    ok("response_id_codec roundtrip")


def test_session_cache():
    from backend.session_cache import ResponseSessionCache
    cache = ResponseSessionCache(max_size=10, ttl_seconds=60)
    cache.save("resp_test", [{"role": "user", "content": "hello"}])
    hist = cache.get("resp_test")
    assert hist is not None and len(hist) == 1
    assert cache.get("resp_nonexistent") is None
    ok("session_cache save/get")


def test_deployment_affinity():
    from backend.deployment_affinity import check_deployment_affinity
    providers = [
        {"id": "p1", "name": "DeepSeek"},
        {"id": "p2", "name": "Kimi"},
    ]
    # no previous_response_id
    r = check_deployment_affinity({}, {"id": "p1", "name": "DeepSeek"}, providers)
    assert r["ok"] is True
    ok("deployment_affinity no previous_id")

    # mismatch
    from backend.response_id_codec import encode_response_id
    resp_id = encode_response_id("kimi", "kimi-chat", "req_456")
    r = check_deployment_affinity(
        {"previous_response_id": resp_id},
        {"id": "p1", "name": "DeepSeek"},
        providers,
    )
    assert r["ok"] is False
    assert r["suggested_provider"]["name"] == "Kimi"
    ok("deployment_affinity mismatch detection")


def test_base_adapter():
    from backend.base_adapter import (
        convert_developer_to_system,
        merge_consecutive_assistant_messages,
    )
    msgs = [{"role": "developer", "content": "sys"}, {"role": "user", "content": "hi"}]
    result = convert_developer_to_system(msgs)
    assert result[0]["role"] == "system"
    ok("convert_developer_to_system")

    msgs2 = [
        {"role": "assistant", "content": "hi"},
        {"role": "assistant", "tool_calls": [{"id": "tc1"}]},
    ]
    merged = merge_consecutive_assistant_messages(msgs2)
    assert len(merged) == 1
    assert merged[0].get("tool_calls")
    ok("merge_consecutive_assistant_messages")


def test_provider_workarounds():
    from backend.provider_workarounds import detect_provider_kind
    assert detect_provider_kind({"name": "DeepSeek", "baseUrl": "https://deepseek.com"}) == "deepseek"
    assert detect_provider_kind({"name": "Kimi", "baseUrl": ""}) == "kimi"
    assert detect_provider_kind({"name": "Unknown", "baseUrl": ""}) == "unknown"
    ok("detect_provider_kind")


def test_streaming_adapter():
    from backend.streaming_adapter import StreamingAdapter
    adapter = StreamingAdapter("gpt-4", provider_kind="unknown")
    # first chunk with content
    chunk = {
        "id": "chatcmpl-test",
        "choices": [{"delta": {"content": "Hello"}, "finish_reason": None}],
    }
    events = adapter.process_chunk(chunk)
    types = [e["type"] for e in events]
    assert "response.created" in types
    assert "response.in_progress" in types
    assert "response.output_text.delta" in types
    ok("streaming_adapter first chunk")

    # finish chunk
    chunk2 = {
        "id": "chatcmpl-test",
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5},
    }
    events2 = adapter.process_chunk(chunk2)
    types2 = [e["type"] for e in events2]
    assert "response.completed" in types2
    ok("streaming_adapter finish chunk")


async def test_async_conversions():
    from backend.responses_adapter import convert_responses_to_chat_body
    from backend.chat_responses_adapter import convert_chat_to_responses

    body = {
        "model": "gpt-4",
        "input": "hello",
        "stream": False,
    }
    chat_body = await convert_responses_to_chat_body(body)
    assert chat_body["model"] == "gpt-4"
    assert chat_body["messages"][0]["role"] == "user"
    assert chat_body["messages"][0]["content"] == "hello"
    ok("responses_adapter convert_responses_to_chat_body")

    chat_resp = {
        "id": "chatcmpl-123",
        "model": "gpt-4",
        "choices": [{"message": {"content": "Hi there"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2},
    }
    resp = convert_chat_to_responses(chat_resp, "gpt-4")
    assert resp["object"] == "response"
    assert resp["status"] == "completed"
    assert any(item.get("type") == "message" for item in resp["output"])
    ok("chat_responses_adapter convert_chat_to_responses")

    # ─── streaming reasoning 事件协议（Kimi thinking 不展开 bug 修复）─────────
    # 验证：reasoning_content delta 应触发 reasoning_summary_part.added/done +
    # summary_index,而不是通用的 content_part.added/done + content_index。
    # Codex CLI 严格按事件名匹配 reasoning summary part,通用的 content_part
    # 会让 thinking UI 卡住不展开。
    from backend.streaming_adapter import StreamingAdapter
    sa = StreamingAdapter("kimi-for-coding", "kimi")
    chunk1 = {
        "id": "chatcmpl-test-r",
        "choices": [{"index": 0, "delta": {"reasoning_content": "Let me think..."}, "finish_reason": None}],
    }
    events1 = sa.process_chunk(chunk1)
    types1 = [e["type"] for e in events1]
    assert "response.reasoning_summary_part.added" in types1, \
        f"reasoning 起点应发 reasoning_summary_part.added, got: {types1}"
    assert "response.content_part.added" not in [
        e["type"] for e in events1
        if isinstance(e.get("item"), dict) and e["item"].get("type") == "reasoning"
    ], "reasoning item 不应再发通用 content_part.added"
    summary_part = next(e for e in events1 if e["type"] == "response.reasoning_summary_part.added")
    assert summary_part.get("summary_index") == 0
    assert summary_part.get("part", {}).get("type") == "summary_text"
    assert "response.reasoning_summary_text.delta" in types1
    ok("streaming reasoning: 首个 reasoning_content 触发 reasoning_summary_part.added")

    chunk2 = {
        "id": "chatcmpl-test-r",
        "choices": [{"index": 0, "delta": {"content": "Final answer"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 10},
    }
    events2 = sa.process_chunk(chunk2)
    types2 = [e["type"] for e in events2]
    assert "response.reasoning_summary_part.done" in types2, \
        f"reasoning 收尾应发 reasoning_summary_part.done, got: {types2}"
    done_part = next(e for e in events2 if e["type"] == "response.reasoning_summary_part.done")
    assert done_part.get("summary_index") == 0
    assert done_part.get("part", {}).get("type") == "summary_text"
    assert done_part.get("part", {}).get("text") == "Let me think..."
    ok("streaming reasoning: 收尾发 reasoning_summary_part.done 含完整 text")

    # ─── tool_call_id 修复 pass (Kimi 'tool_call_id is not found' 400 修复) ────
    # 场景 A: function_call_output 的 call_id 字段为空 → 按位置从前面 assistant
    #         的 tool_calls 取 ID 补上
    body_tool_a = {
        "model": "kimi-for-coding",
        "input": [
            {"type": "message", "role": "user", "content": "run ls"},
            {"type": "function_call", "id": "call_xyz1", "call_id": "call_xyz1",
             "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
            # 模拟 Codex 历史压缩后 call_id 丢失
            {"type": "function_call_output", "output": "file.txt"},
            {"type": "message", "role": "user", "content": "ok"},
        ],
    }
    chat_tool_a = await convert_responses_to_chat_body(body_tool_a)
    tool_msg_a = next(m for m in chat_tool_a["messages"] if m.get("role") == "tool")
    assert tool_msg_a["tool_call_id"] == "call_xyz1", \
        f"空 tool_call_id 应按位置补成 call_xyz1, got: {tool_msg_a['tool_call_id']!r}"
    ok("tool_call_id 修复: 空字段按位置从前面 assistant.tool_calls 补 ID")

    # 场景 B: 多个 tool_calls + 部分 tool message 缺 ID → 按顺序逐个配对
    body_tool_b = {
        "model": "kimi-for-coding",
        "input": [
            {"type": "message", "role": "user", "content": "do two things"},
            {"type": "function_call", "id": "call_a", "call_id": "call_a",
             "name": "shell", "arguments": "{\"cmd\":\"a\"}"},
            {"type": "function_call", "id": "call_b", "call_id": "call_b",
             "name": "shell", "arguments": "{\"cmd\":\"b\"}"},
            {"type": "function_call_output", "call_id": "call_a", "output": "A"},
            {"type": "function_call_output", "output": "B"},  # 缺 ID
            {"type": "message", "role": "user", "content": "next"},
        ],
    }
    chat_tool_b = await convert_responses_to_chat_body(body_tool_b)
    tool_msgs_b = [m for m in chat_tool_b["messages"] if m.get("role") == "tool"]
    assert len(tool_msgs_b) == 2
    assert tool_msgs_b[0]["tool_call_id"] == "call_a"
    assert tool_msgs_b[1]["tool_call_id"] == "call_b", \
        f"第二条 tool 应按位置补成 call_b, got: {tool_msgs_b[1]['tool_call_id']!r}"
    ok("tool_call_id 修复: 多 tool_calls 按位置逐个配对")

    # 场景 C: 孤儿 tool message (前面没有 assistant.tool_calls) → 丢弃
    body_tool_c = {
        "model": "kimi-for-coding",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            # 模拟 Codex 历史压缩把 function_call 删了但留下 output
            {"type": "function_call_output", "output": "orphan"},
            {"type": "message", "role": "user", "content": "still here"},
        ],
    }
    chat_tool_c = await convert_responses_to_chat_body(body_tool_c)
    tool_msgs_c = [m for m in chat_tool_c["messages"] if m.get("role") == "tool"]
    assert len(tool_msgs_c) == 0, f"孤儿 tool message 应被丢弃, got: {tool_msgs_c}"
    ok("tool_call_id 修复: 无可配对 assistant 的孤儿 tool message 被丢弃")

    # 场景 D: 正常情况 → 行为保持不变(回归保护)
    body_tool_d = {
        "model": "kimi-for-coding",
        "input": [
            {"type": "function_call", "id": "call_x", "call_id": "call_x",
             "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_x", "output": "ok"},
            {"type": "message", "role": "user", "content": "next"},
        ],
    }
    chat_tool_d = await convert_responses_to_chat_body(body_tool_d)
    tool_msg_d = next(m for m in chat_tool_d["messages"] if m.get("role") == "tool")
    assert tool_msg_d["tool_call_id"] == "call_x"
    ok("tool_call_id 修复: 正常 call_id 透传无副作用")

    # ─── reasoning_content 占位符回归 (Kimi/DeepSeek thinking 模式 400 修复) ────
    # 场景 A: reasoning item 只有 encrypted_content,无 summary text
    #         → assistant tool_call 消息应得到非空 reasoning_content
    body_a = {
        "model": "kimi-for-coding",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "reasoning", "encrypted_content": "opaque_blob_abc123", "summary": []},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1",
             "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "file.txt"},
            {"type": "message", "role": "user", "content": "next step?"},
        ],
    }
    chat_a = await convert_responses_to_chat_body(body_a)
    asst_a = next(m for m in chat_a["messages"] if m.get("role") == "assistant" and m.get("tool_calls"))
    rc_a = asst_a.get("reasoning_content")
    assert rc_a is not None and str(rc_a).strip() == "" and len(str(rc_a)) > 0, \
        f"encrypted-only 应得到非空但空白占位符, got: {rc_a!r}"
    ok("reasoning_content fix: encrypted_content-only 历史得到非空占位")

    # 场景 B: 历史中没有 reasoning item,但 assistant 有 tool_calls + 请求体含 reasoning
    #         → safety net 应填非空占位
    body_b = {
        "model": "kimi-for-coding",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "function_call", "id": "fc_1", "call_id": "call_1",
             "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "file.txt"},
            {"type": "message", "role": "user", "content": "next?"},
        ],
    }
    chat_b = await convert_responses_to_chat_body(body_b)
    asst_b = next(m for m in chat_b["messages"] if m.get("role") == "assistant" and m.get("tool_calls"))
    rc_b = asst_b.get("reasoning_content")
    assert rc_b is not None and len(str(rc_b)) > 0, \
        f"safety net 应保证 tool_call assistant 消息有非空 reasoning_content, got: {rc_b!r}"
    ok("reasoning_content fix: 缺 reasoning item 时 safety net 填非空")

    # 场景 C: reasoning item 有真实 summary text → 真实文本透传,不被覆盖
    body_c = {
        "model": "kimi-for-coding",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "reasoning", "summary": [
                {"type": "summary_text", "text": "Analyzing the user's request..."},
            ]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1",
             "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "file.txt"},
            {"type": "message", "role": "user", "content": "good"},
        ],
    }
    chat_c = await convert_responses_to_chat_body(body_c)
    asst_c = next(m for m in chat_c["messages"] if m.get("role") == "assistant" and m.get("tool_calls"))
    rc_c = asst_c.get("reasoning_content")
    assert rc_c == "Analyzing the user's request...", \
        f"真实 summary 文本应原样保留, got: {rc_c!r}"
    ok("reasoning_content fix: 真实 summary text 不被空格占位覆盖")

    # 场景 D: DeepSeek V4 provider preset 开启 thinking,但请求体没有 reasoning
    #         → tool-call 续轮仍应补 reasoning_content。
    # Scenario D: DeepSeek V4 provider preset enables thinking, but the request
    #             body has no reasoning field. Tool-call continuations should
    #             still replay non-empty reasoning_content.
    deepseek_provider = {
        "id": "deepseek",
        "name": "DeepSeek V4 Pro",
        "baseUrl": "https://api.deepseek.com/v1",
        "requestOptions": {
            "chat": {
                "thinking": {"type": "enabled"},
                "reasoning_effort": "max",
            }
        },
    }
    body_d = {
        "model": "deepseek-v4-pro",
        "input": [
            {"type": "function_call", "id": "fc_1", "call_id": "call_1",
             "name": "shell", "arguments": "{\"cmd\":\"pwd\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "/tmp"},
            {"type": "message", "role": "user", "content": "continue"},
        ],
    }
    chat_d = await convert_responses_to_chat_body(body_d, provider=deepseek_provider)
    asst_d = next(m for m in chat_d["messages"] if m.get("role") == "assistant" and m.get("tool_calls"))
    rc_d = asst_d.get("reasoning_content")
    assert rc_d is not None and len(str(rc_d)) > 0, \
        f"provider-level thinking 应补非空 reasoning_content, got: {rc_d!r}"
    ok("reasoning_content fix: DeepSeek provider-level thinking 续轮补非空")

    # 场景 E: Codex Responses input 可直接是 role/content dict,无 type 字段。
    # Scenario E: Codex Responses input may use role/content dicts without type.
    body_e = {
        "model": "deepseek-v4-pro",
        "input": [
            {"role": "user", "content": [
                {"type": "input_text", "text": "hello"},
            ]},
        ],
    }
    chat_e = await convert_responses_to_chat_body(body_e, provider=deepseek_provider)
    assert chat_e["messages"][0]["role"] == "user"
    assert chat_e["messages"][0]["content"] == [{"type": "text", "text": "hello"}]
    ok("responses input: role/content dict without type is normalized")


# run sync tests
test_normalize_api_format()
test_response_id_codec()
test_session_cache()
test_deployment_affinity()
test_base_adapter()
test_provider_workarounds()
test_streaming_adapter()

# run async tests
asyncio.run(test_async_conversions())


# ============================================================================
# 3. Admin API 启动 + 端点测试
# ============================================================================
print("\n" + "=" * 60)
print("3. Admin API 启动 + 端点测试")
print("=" * 60)


async def test_admin_api():
    from backend import config as cfg
    from backend.main import create_admin_app

    old_cfg_dir = cfg.CONFIG_DIR
    old_cfg_file = cfg.CONFIG_FILE
    old_lib_dir = cfg.LIBRARY_DIR

    try:
        with tempfile.TemporaryDirectory() as tmp:
            fake_app_dir = os.path.join(tmp, ".codex-app-transfer")
            cfg.CONFIG_DIR = fake_app_dir
            cfg.CONFIG_FILE = os.path.join(fake_app_dir, "config.json")
            cfg.LIBRARY_DIR = os.path.join(fake_app_dir, "configLibrary")
            cfg.save_config({
                "version": cfg.APP_VERSION,
                "activeProvider": None,
                "providers": [],
                "settings": cfg.DEFAULT_CONFIG["settings"].copy(),
            })

            admin_app = create_admin_app()
            admin_port = free_port()

            config = uvicorn.Config(admin_app, host="127.0.0.1", port=admin_port, log_level="error")
            server = uvicorn.Server(config)
            task = asyncio.create_task(server.serve())
            await asyncio.sleep(0.5)

            try:
                async with httpx.AsyncClient() as client:
                    # /api/status
                    r = await client.get(f"http://127.0.0.1:{admin_port}/api/status")
                    assert r.status_code == 200
                    assert "proxyRunning" in r.json()
                    ok(f"Admin /api/status → {r.status_code}")

                    # /api/version
                    r = await client.get(f"http://127.0.0.1:{admin_port}/api/version")
                    assert r.status_code == 200
                    assert "version" in r.json()
                    info(f"Admin /api/version → {r.json()}")
                    ok("Admin /api/version")
            finally:
                server.should_exit = True
                await task
    finally:
        cfg.CONFIG_DIR = old_cfg_dir
        cfg.CONFIG_FILE = old_cfg_file
        cfg.LIBRARY_DIR = old_lib_dir


asyncio.run(test_admin_api())


# ============================================================================
# 4. Proxy API 启动 + 端点测试
# ============================================================================
print("\n" + "=" * 60)
print("4. Proxy API 启动 + 端点测试")
print("=" * 60)


async def test_proxy_api():
    from backend.proxy import create_proxy_app

    proxy_app = create_proxy_app()
    proxy_port = free_port()

    config = uvicorn.Config(proxy_app, host="127.0.0.1", port=proxy_port, log_level="error")
    server = uvicorn.Server(config)
    task = asyncio.create_task(server.serve())
    await asyncio.sleep(0.5)

    try:
        async with httpx.AsyncClient() as client:
            # /health
            r = await client.get(f"http://127.0.0.1:{proxy_port}/health")
            assert r.status_code == 200
            assert r.json()["status"] == "ok"
            ok(f"Proxy /health → {r.status_code}")

            # /v1/models (no auth → 401)
            r = await client.get(f"http://127.0.0.1:{proxy_port}/v1/models")
            assert r.status_code == 401
            ok(f"Proxy /v1/models (no auth) → {r.status_code}")
    finally:
        server.should_exit = True
        await task


asyncio.run(test_proxy_api())


# ============================================================================
# 5. 本地 mock 上游 + 端到端请求转发测试
# ============================================================================
print("\n" + "=" * 60)
print("5. 端到端请求转发测试（mock 上游）")
print("=" * 60)


async def test_end_to_end():
    from backend import config as cfg

    old_cfg_dir = cfg.CONFIG_DIR
    old_cfg_file = cfg.CONFIG_FILE
    old_lib_dir = cfg.LIBRARY_DIR

    # 启动 mock 上游
    mock_app = FastAPI()

    @mock_app.post("/chat/completions")
    async def mock_chat(request: Request):
        body = await request.json()
        if body.get("stream"):
            from fastapi.responses import StreamingResponse
            import json
            def sse_chunks():
                chunks = [
                    {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": body.get("model", "gpt-4"), "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                    {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": body.get("model", "gpt-4"), "choices": [{"index": 0, "delta": {"content": "Mock"}, "finish_reason": None}]},
                    {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": body.get("model", "gpt-4"), "choices": [{"index": 0, "delta": {"content": " response"}, "finish_reason": None}]},
                    {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": body.get("model", "gpt-4"), "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
                for chunk in chunks:
                    yield f"data: {json.dumps(chunk)}\n\n"
                yield "data: [DONE]\n\n"
            return StreamingResponse(sse_chunks(), media_type="text/event-stream")
        return {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": body.get("model", "gpt-4"),
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Mock response"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        }

    mock_port = free_port()
    mock_config = uvicorn.Config(mock_app, host="127.0.0.1", port=mock_port, log_level="error")
    mock_server = uvicorn.Server(mock_config)
    mock_task = asyncio.create_task(mock_server.serve())
    await asyncio.sleep(0.3)

    # 重置全局 HTTP 客户端（避免之前测试的残留状态）
    import backend.proxy as _proxy_mod
    if _proxy_mod._http_client:
        await _proxy_mod._http_client.aclose()
        _proxy_mod._http_client = None

    fake_config_tmp = tempfile.TemporaryDirectory()
    fake_app_dir = os.path.join(fake_config_tmp.name, ".codex-app-transfer")
    cfg.CONFIG_DIR = fake_app_dir
    cfg.CONFIG_FILE = os.path.join(fake_app_dir, "config.json")
    cfg.LIBRARY_DIR = os.path.join(fake_app_dir, "configLibrary")

    try:
        # 配置一个指向 mock 的 provider
        cfg.save_config({
            "version": cfg.APP_VERSION,
            "activeProvider": "mock-provider",
            "gatewayApiKey": "test-key",
            "providers": [{
                "id": "mock-provider",
                "name": "Mock",
                "baseUrl": f"http://127.0.0.1:{mock_port}",
                "apiFormat": "openai_chat",
                "apiKey": "mock-key",
                "models": {"default": "mock-model"},
            }],
            "settings": cfg.DEFAULT_CONFIG["settings"].copy(),
        })

        # 直接测试核心转发函数（避免两个 uvicorn 服务器在同一事件循环中竞争）
        from backend.proxy import forward_request

        # non-streaming
        result = await forward_request(
            {"model": "mock-model", "input": "Hello", "stream": False},
            {
                "id": "mock-provider",
                "name": "Mock",
                "baseUrl": f"http://127.0.0.1:{mock_port}",
                "apiFormat": "openai_chat",
                "apiKey": "mock-key",
                "models": {"default": "mock-model"},
            },
            "test-req",
        )
        assert result.get("object") == "response"
        assert result.get("status") == "completed"
        assert any(item.get("type") == "message" for item in result.get("output", []))
        ok(f"E2E non-streaming → status={result['status']}")

        # streaming — 直接迭代 generator
        from backend.proxy import forward_request_stream
        events = []
        async for chunk in forward_request_stream(
            {"model": "mock-model", "input": "Hello", "stream": True},
            {
                "id": "mock-provider",
                "name": "Mock",
                "baseUrl": f"http://127.0.0.1:{mock_port}",
                "apiFormat": "openai_chat",
                "apiKey": "mock-key",
                "models": {"default": "mock-model"},
            },
            "test-req",
        ):
            if chunk.startswith("data: "):
                data_str = chunk[6:]
                if data_str.strip() and data_str != "[DONE]":
                    try:
                        events.append(json.loads(data_str))
                    except json.JSONDecodeError:
                        pass
        types = [e.get("type") for e in events]
        info(f"streaming event types: {types}")
        assert "response.created" in types, f"missing response.created in {types}"
        assert "response.completed" in types, f"missing response.completed in {types}"
        ok(f"E2E streaming → events={len(events)}")

    finally:
        cfg.CONFIG_DIR = old_cfg_dir
        cfg.CONFIG_FILE = old_cfg_file
        cfg.LIBRARY_DIR = old_lib_dir
        fake_config_tmp.cleanup()
        mock_server.should_exit = True
        await mock_task


asyncio.run(test_end_to_end())


# ============================================================================
# 6. Codex 配置快照 / 智能合并还原 round-trip
# ============================================================================
print("\n" + "=" * 60)
print("6. Codex 配置快照 / 智能合并还原 round-trip")
print("=" * 60)


def test_codex_snapshot_restore():
    import json as _json
    import tempfile
    from backend import registry as _registry

    # 准备一个临时 fake $HOME，重定向 ~/.codex/ 与 ~/.codex-app-transfer/codex-snapshot/
    with tempfile.TemporaryDirectory() as tmp:
        fake_codex = os.path.join(tmp, ".codex")
        fake_snap = os.path.join(tmp, ".codex-app-transfer", "codex-snapshot")
        os.makedirs(fake_codex, exist_ok=True)

        original_home = _registry.CODEX_HOME
        original_config = _registry.CODEX_CONFIG_PATH
        original_auth = _registry.CODEX_AUTH_PATH
        original_snap_dir = _registry.CAS_SNAPSHOT_DIR
        original_snap_cfg = _registry.CAS_SNAPSHOT_CONFIG
        original_snap_auth = _registry.CAS_SNAPSHOT_AUTH
        original_snap_manifest = _registry.CAS_SNAPSHOT_MANIFEST

        _registry.CODEX_HOME = fake_codex
        _registry.CODEX_CONFIG_PATH = os.path.join(fake_codex, "config.toml")
        _registry.CODEX_AUTH_PATH = os.path.join(fake_codex, "auth.json")
        _registry.CAS_SNAPSHOT_DIR = fake_snap
        _registry.CAS_SNAPSHOT_CONFIG = os.path.join(fake_snap, "config.toml")
        _registry.CAS_SNAPSHOT_AUTH = os.path.join(fake_snap, "auth.json")
        _registry.CAS_SNAPSHOT_MANIFEST = os.path.join(fake_snap, "manifest.json")

        try:
            # 1) 模拟用户原状态：ChatGPT 登录,有 model_reasoning_effort
            with open(_registry.CODEX_CONFIG_PATH, "w") as f:
                f.write('model_reasoning_effort = "xhigh"\npersonality = "pragmatic"\n')
            with open(_registry.CODEX_AUTH_PATH, "w") as f:
                _json.dump({"auth_mode": "chatgpt", "tokens": {"id": "abc"}, "last_refresh": "t0"}, f)

            assert not _registry.has_snapshot()
            ok("snapshot pre-state: no snapshot")

            # 2) apply：写入网关 base_url + apikey
            _registry.apply_config(
                base_url="http://127.0.0.1:18080",
                gateway_api_key="sk-our-gateway",
            )

            assert _registry.has_snapshot(), "apply 后应已建立快照"
            ok("apply triggers snapshot")

            applied_auth = _json.load(open(_registry.CODEX_AUTH_PATH))
            assert applied_auth["auth_mode"] == "apikey"
            assert applied_auth["OPENAI_API_KEY"] == "sk-our-gateway"
            assert applied_auth["tokens"] == {"id": "abc"}, "tokens 应保留"
            ok("apply 写入 auth.json 字段正确")

            applied_cfg = open(_registry.CODEX_CONFIG_PATH).read()
            assert 'openai_base_url = "http://127.0.0.1:18080"' in applied_cfg
            assert 'model_reasoning_effort = "xhigh"' in applied_cfg
            ok("apply 写入 config.toml 字段正确,其他行保留")

            # 3) 用户在 apply 之后手动加了一个 auth 字段和一个 toml 字段
            modified_auth = _json.load(open(_registry.CODEX_AUTH_PATH))
            modified_auth["my_extra_field"] = "user-data"
            with open(_registry.CODEX_AUTH_PATH, "w") as f:
                _json.dump(modified_auth, f)
            with open(_registry.CODEX_CONFIG_PATH, "a") as f:
                f.write('user_added_key = "preserved"\n')

            # 4) 第二次 apply：快照不应被覆盖（关键幂等性）
            _registry.apply_config(
                base_url="http://127.0.0.1:19999",
                gateway_api_key="sk-second-apply",
            )
            snap_auth = _json.load(open(_registry.CAS_SNAPSHOT_AUTH))
            assert snap_auth["auth_mode"] == "chatgpt", "快照 auth_mode 必须保持原值"
            ok("二次 apply 不覆盖原始快照")

            # 5) 还原：智能合并
            result = _registry.restore_codex_state()
            assert result["restored"] is True
            assert not _registry.has_snapshot(), "还原后应清除快照"
            ok("restore_codex_state 智能合并 + 清快照")

            restored_auth = _json.load(open(_registry.CODEX_AUTH_PATH))
            assert restored_auth["auth_mode"] == "chatgpt", f"auth_mode 应还原, got {restored_auth}"
            assert "OPENAI_API_KEY" not in restored_auth, "OPENAI_API_KEY 应被移除（原本就没有）"
            assert restored_auth["tokens"] == {"id": "abc"}, "tokens 应保留"
            assert restored_auth["my_extra_field"] == "user-data", "用户运行期间加的字段应保留"
            ok("auth.json 智能合并：我们的字段还原 + 用户字段保留")

            restored_cfg = open(_registry.CODEX_CONFIG_PATH).read()
            assert 'openai_base_url' not in restored_cfg, "openai_base_url 应移除（原本就没有）"
            assert 'model_reasoning_effort = "xhigh"' in restored_cfg, "原 toml 行应保留"
            assert 'user_added_key = "preserved"' in restored_cfg, "用户运行期间加的 toml 行应保留"
            ok("config.toml 智能合并：我们的行还原 + 用户行保留")

            # 6) legacy fallback：没有快照时 restore 退化为旧 clear
            with open(_registry.CODEX_AUTH_PATH, "w") as f:
                _json.dump({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-stale"}, f)
            with open(_registry.CODEX_CONFIG_PATH, "w") as f:
                f.write('openai_base_url = "http://stale"\nother = "x"\n')
            assert not _registry.has_snapshot()
            result = _registry.restore_codex_state()
            assert result["restored"] is False, "无快照时 restored=False"
            stale_auth = _json.load(open(_registry.CODEX_AUTH_PATH))
            assert "OPENAI_API_KEY" not in stale_auth
            stale_cfg = open(_registry.CODEX_CONFIG_PATH).read()
            assert 'openai_base_url' not in stale_cfg
            assert 'other = "x"' in stale_cfg
            ok("无快照时 fallback 到旧 clear 行为")

        finally:
            _registry.CODEX_HOME = original_home
            _registry.CODEX_CONFIG_PATH = original_config
            _registry.CODEX_AUTH_PATH = original_auth
            _registry.CAS_SNAPSHOT_DIR = original_snap_dir
            _registry.CAS_SNAPSHOT_CONFIG = original_snap_cfg
            _registry.CAS_SNAPSHOT_AUTH = original_snap_auth
            _registry.CAS_SNAPSHOT_MANIFEST = original_snap_manifest


test_codex_snapshot_restore()


# ============================================================================
# 7. 启动时按 active provider 自动 apply（含 requiresProxy 启动 proxy）
# ============================================================================
print("\n" + "=" * 60)
print("7. 启动时自动 apply active provider")
print("=" * 60)


def test_startup_auto_apply():
    import json as _json
    import tempfile
    from backend import registry as _registry
    from backend import config as _cfg
    import backend.main as _bm

    # 完全隔离环境：CONFIG_DIR / CODEX_HOME / 快照目录都重定向到 tempdir
    with tempfile.TemporaryDirectory() as tmp:
        fake_app_dir = os.path.join(tmp, ".codex-app-transfer")
        fake_codex = os.path.join(tmp, ".codex")
        fake_snap = os.path.join(fake_app_dir, "codex-snapshot")
        fake_lib = os.path.join(fake_app_dir, "library")
        os.makedirs(fake_codex, exist_ok=True)
        os.makedirs(fake_app_dir, exist_ok=True)
        os.makedirs(fake_lib, exist_ok=True)

        old_cfg_dir = _cfg.CONFIG_DIR
        old_cfg_path = _cfg.CONFIG_FILE
        old_lib_dir = _cfg.LIBRARY_DIR
        old_codex = (
            _registry.CODEX_HOME, _registry.CODEX_CONFIG_PATH, _registry.CODEX_AUTH_PATH,
            _registry.CAS_SNAPSHOT_DIR, _registry.CAS_SNAPSHOT_CONFIG,
            _registry.CAS_SNAPSHOT_AUTH, _registry.CAS_SNAPSHOT_MANIFEST,
        )

        _cfg.CONFIG_DIR = fake_app_dir
        _cfg.CONFIG_FILE = os.path.join(fake_app_dir, "config.json")
        _cfg.LIBRARY_DIR = fake_lib

        _registry.CODEX_HOME = fake_codex
        _registry.CODEX_CONFIG_PATH = os.path.join(fake_codex, "config.toml")
        _registry.CODEX_AUTH_PATH = os.path.join(fake_codex, "auth.json")
        _registry.CAS_SNAPSHOT_DIR = fake_snap
        _registry.CAS_SNAPSHOT_CONFIG = os.path.join(fake_snap, "config.toml")
        _registry.CAS_SNAPSHOT_AUTH = os.path.join(fake_snap, "auth.json")
        _registry.CAS_SNAPSHOT_MANIFEST = os.path.join(fake_snap, "manifest.json")

        try:
            with open(_registry.CODEX_AUTH_PATH, "w") as f:
                _json.dump({"auth_mode": "chatgpt", "tokens": {"id": "abc"}}, f)

            # 1) 真正空 provider 列表 → 跳过,不建快照
            _cfg.save_config({
                "version": _cfg.APP_VERSION,
                "activeProvider": None,
                "providers": [],
                "settings": _cfg.DEFAULT_CONFIG["settings"].copy(),
            })
            result = _bm.auto_apply_active_provider_on_startup()
            assert result["applied"] is False, f"无 provider 时应跳过, got: {result}"
            assert not _registry.has_snapshot()
            ok("startup auto-apply: 无 provider 时正确跳过")

            # 2) Responses 兼容 provider → apply 但不起 proxy
            _cfg.save_config({
                "version": _cfg.APP_VERSION,
                "activeProvider": "kimi-test",
                "providers": [{
                    "id": "kimi-test", "name": "Kimi Test",
                    "baseUrl": "https://api.moonshot.cn/anthropic",
                    "apiFormat": "responses",
                    "apiKey": "sk-kimi",
                    "models": {"sonnet": "kimi-k1", "haiku": "kimi-k1", "opus": "kimi-k1"},
                }],
                "settings": _cfg.DEFAULT_CONFIG["settings"].copy(),
            })
            if _bm._proxy_running:
                _bm._stop_proxy_server()

            result = _bm.auto_apply_active_provider_on_startup()
            assert result["applied"] is True
            assert result["requiresProxy"] is False
            assert result["proxyStarted"] is False
            assert _registry.has_snapshot()
            ok("startup auto-apply: responses 路径写入但不起 proxy")

            applied_cfg = open(_registry.CODEX_CONFIG_PATH).read()
            assert 'api.moonshot.cn' in applied_cfg
            ok("startup auto-apply: responses 路径写入真实 baseUrl 直连地址")

            # 3) OpenAI Chat 类需要转发 → apply 同时起 proxy
            _registry.restore_codex_state()
            with open(_registry.CODEX_AUTH_PATH, "w") as f:
                _json.dump({"auth_mode": "chatgpt", "tokens": {"id": "abc"}}, f)

            _cfg.save_config({
                "version": _cfg.APP_VERSION,
                "activeProvider": "ds-test",
                "providers": [{
                    "id": "ds-test", "name": "DeepSeek Test",
                    "baseUrl": "https://api.deepseek.com/v1",
                    "apiFormat": "openai_chat",
                    "apiKey": "sk-deepseek",
                    "models": {"sonnet": "deepseek-v4-pro", "haiku": "deepseek-v4-flash", "opus": "deepseek-v4-pro"},
                }],
                "settings": dict(_cfg.DEFAULT_CONFIG["settings"], proxyPort=29291),
            })
            if _bm._proxy_running:
                _bm._stop_proxy_server()

            result = _bm.auto_apply_active_provider_on_startup()
            assert result["applied"] is True
            assert result["requiresProxy"] is True
            assert result["proxyStarted"] is True, f"应已启动 proxy: {result}"
            ok("startup auto-apply: openai_chat 路径触发 proxy 启动")

            applied_cfg2 = open(_registry.CODEX_CONFIG_PATH).read()
            assert '127.0.0.1' in applied_cfg2
            ok("startup auto-apply: openai_chat 路径写入网关地址")

            # 4) maybe_stop_proxy_for_provider：切到不需转发的 provider 应能识别
            kimi_provider = {
                "id": "kimi-test", "name": "Kimi Test",
                "baseUrl": "https://api.moonshot.cn/anthropic",
                "apiFormat": "responses",
            }
            # 注意 _proxy_running 状态依赖于线程,这里只验证函数能正确判断 requiresProxy=False
            stopped = _bm.maybe_stop_proxy_for_provider(kimi_provider)
            ok(f"maybe_stop_proxy_for_provider on responses provider → stopped={stopped}")

            # 收尾
            if _bm._proxy_running:
                _bm._stop_proxy_server()
        finally:
            _cfg.CONFIG_DIR = old_cfg_dir
            _cfg.CONFIG_FILE = old_cfg_path
            _cfg.LIBRARY_DIR = old_lib_dir
            (_registry.CODEX_HOME, _registry.CODEX_CONFIG_PATH, _registry.CODEX_AUTH_PATH,
             _registry.CAS_SNAPSHOT_DIR, _registry.CAS_SNAPSHOT_CONFIG,
             _registry.CAS_SNAPSHOT_AUTH, _registry.CAS_SNAPSHOT_MANIFEST) = old_codex


test_startup_auto_apply()


# ============================================================================
# 总结
# ============================================================================
print("\n" + "=" * 60)
print("测试总结")
print("=" * 60)
print(f"  通过: {GREEN}{passed}{RESET}")
print(f"  失败: {RED}{failed}{RESET}")
if failed == 0:
    print(f"\n  {GREEN}🎉 全部测试通过！{RESET}")
else:
    print(f"\n  {RED}⚠️ 有 {failed} 项测试失败{RESET}")

sys.exit(0 if failed == 0 else 1)
