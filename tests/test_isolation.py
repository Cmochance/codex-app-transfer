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
import sys
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

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


# ============================================================================
# 1. 模块导入测试
# ============================================================================
print("\n" + "=" * 60)
print("1. 模块导入测试")
print("=" * 60)

MODULES = [
    "backend.api_types",
    "backend.api_response_types",
    "backend.session_cache",
    "backend.response_id_codec",
    "backend.deployment_affinity",
    "backend.adapter_utils",
    "backend.base_adapter",
    "backend.provider_workarounds",
    "backend.openai_adapter",
    "backend.native_streaming_adapter",
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
    from backend.main import create_admin_app

    admin_app = create_admin_app()
    admin_port = 28081

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
    proxy_port = 28082

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

    mock_port = 29090
    mock_config = uvicorn.Config(mock_app, host="127.0.0.1", port=mock_port, log_level="error")
    mock_server = uvicorn.Server(mock_config)
    mock_task = asyncio.create_task(mock_server.serve())
    await asyncio.sleep(0.3)

    # 重置全局 HTTP 客户端（避免之前测试的残留状态）
    import backend.proxy as _proxy_mod
    if _proxy_mod._http_client:
        await _proxy_mod._http_client.aclose()
        _proxy_mod._http_client = None

    # 配置一个指向 mock 的 provider
    from backend import config as cfg
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

    try:
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
        mock_server.should_exit = True
        await mock_task


asyncio.run(test_end_to_end())


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
