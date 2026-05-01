#!/usr/bin/env python3
"""
工具调用全流程测试

测试覆盖：
1. Non-streaming：client 发 tools → upstream 返回 tool_calls → proxy 转成 function_call
2. Streaming：client 发 tools → upstream SSE 返回 tool_calls delta → proxy 流式 event
3. 完整往返：tool request → function_call → function_call_output → final message
"""

import asyncio
import json
import sys
import time

import uvicorn
from fastapi import FastAPI, Request
from fastapi.responses import StreamingResponse

# --------------------------------------------------------------------------- #
# 颜色输出
# --------------------------------------------------------------------------- #
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


def fail(msg: str):
    global failed
    failed += 1
    print(f"  {RED}✗{RESET} {msg}")
    raise AssertionError(msg)


def info(msg: str):
    print(f"  {YELLOW}ℹ{RESET} {msg}")


# --------------------------------------------------------------------------- #
# Mock 上游（支持工具调用响应）
# --------------------------------------------------------------------------- #

def _make_mock_app():
    app = FastAPI()

    @app.post("/chat/completions")
    async def mock_chat(request: Request):
        body = await request.json()
        stream = body.get("stream", False)
        messages = body.get("messages", [])
        model = body.get("model", "gpt-4")

        # 检测是否是 tool result 回合（最后一条 message 的 role == tool）
        last_role = messages[-1].get("role") if messages else None
        is_tool_result_round = last_role in ("tool", "function")

        # 检测是否包含 tools 参数
        has_tools = bool(body.get("tools"))

        if stream:
            return _streaming_response(model, has_tools, is_tool_result_round)
        return _non_streaming_response(model, has_tools, is_tool_result_round)

    return app


def _non_streaming_response(model: str, has_tools: bool, is_tool_result_round: bool):
    """返回普通 JSON 响应。"""
    if is_tool_result_round:
        # tool result 之后，返回最终消息
        return {
            "id": "chatcmpl-final",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "The weather in Beijing is sunny, 25°C."},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 25, "completion_tokens": 10, "total_tokens": 35},
        }

    if has_tools:
        # 返回 tool_calls（模型决定调用工具）
        return {
            "id": "chatcmpl-tool",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": json.dumps({"location": "Beijing"}),
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 15, "completion_tokens": 8, "total_tokens": 23},
        }

    # 普通对话
    return {
        "id": "chatcmpl-normal",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello! How can I help you?"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16},
    }


def _streaming_response(model: str, has_tools: bool, is_tool_result_round: bool):
    """返回 SSE 流式响应。"""
    def sse_chunks():
        if is_tool_result_round:
            chunks = [
                {"id": "chatcmpl-final", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                {"id": "chatcmpl-final", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"content": "The"}, "finish_reason": None}]},
                {"id": "chatcmpl-final", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"content": " weather"}, "finish_reason": None}]},
                {"id": "chatcmpl-final", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"content": " in Beijing is sunny."}, "finish_reason": None}]},
                {"id": "chatcmpl-final", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        elif has_tools:
            # tool_calls 分多个 chunk 返回（模拟真实流式行为）
            chunks = [
                # chunk 1: role + tool_call start (name)
                {"id": "chatcmpl-tool", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_abc123", "type": "function", "function": {"name": "get_weather", "arguments": ""}}]}, "finish_reason": None}]},
                # chunk 2: arguments delta
                {"id": "chatcmpl-tool", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": '{"location": '}}]}, "finish_reason": None}]},
                # chunk 3: arguments delta continued
                {"id": "chatcmpl-tool", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": '"Beijing"}'}}]}, "finish_reason": None}]},
                # chunk 4: finish
                {"id": "chatcmpl-tool", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {"id": "chatcmpl-normal", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                {"id": "chatcmpl-normal", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {"content": "Hello!"}, "finish_reason": None}]},
                {"id": "chatcmpl-normal", "object": "chat.completion.chunk", "created": int(time.time()), "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]

        for chunk in chunks:
            yield f"data: {json.dumps(chunk)}\n\n"
        yield "data: [DONE]\n\n"

    return StreamingResponse(sse_chunks(), media_type="text/event-stream")


# --------------------------------------------------------------------------- #
# 测试主体
# --------------------------------------------------------------------------- #

async def test_tool_calls():
    mock_app = _make_mock_app()
    mock_port = 29091
    mock_config = uvicorn.Config(mock_app, host="127.0.0.1", port=mock_port, log_level="error")
    mock_server = uvicorn.Server(mock_config)
    mock_task = asyncio.create_task(mock_server.serve())
    await asyncio.sleep(0.3)

    # 重置全局 HTTP 客户端
    import backend.proxy as _proxy_mod
    if _proxy_mod._http_client:
        await _proxy_mod._http_client.aclose()
        _proxy_mod._http_client = None

    provider = {
        "id": "mock-provider",
        "name": "Mock",
        "baseUrl": f"http://127.0.0.1:{mock_port}",
        "apiFormat": "openai_chat",
        "apiKey": "mock-key",
        "models": {"default": "mock-model"},
    }

    tools = [{
        "type": "function",
        "name": "get_weather",
        "description": "Get current weather for a location",
        "parameters": {
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"],
        },
    }]

    try:
        from backend.proxy import forward_request, forward_request_stream

        # =====================================================================
        # Test 1: Non-streaming tool call
        # =====================================================================
        print("\n  Test 1: Non-streaming tool call")
        result = await forward_request(
            {
                "model": "mock-model",
                "input": "What's the weather in Beijing?",
                "tools": tools,
                "stream": False,
            },
            provider,
            "test-req-1",
        )
        assert result.get("object") == "response", f"expected object=response, got {result.get('object')}"
        assert result.get("status") == "completed", f"expected status=completed, got {result.get('status')}"

        output = result.get("output", [])
        func_items = [item for item in output if item.get("type") == "function_call"]
        assert len(func_items) == 1, f"expected 1 function_call, got {len(func_items)}"
        func_item = func_items[0]
        assert func_item.get("name") == "get_weather", f"expected name=get_weather, got {func_item.get('name')}"
        args = json.loads(func_item.get("arguments", "{}"))
        assert args.get("location") == "Beijing", f"expected location=Beijing, got {args}"
        ok(f"Non-streaming tool call → name={func_item['name']}, args={args}")

        # =====================================================================
        # Test 2: Streaming tool call
        # =====================================================================
        print("\n  Test 2: Streaming tool call")
        events = []
        async for chunk in forward_request_stream(
            {
                "model": "mock-model",
                "input": "What's the weather in Beijing?",
                "tools": tools,
                "stream": True,
            },
            provider,
            "test-req-2",
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

        assert "response.created" in types, f"missing response.created"
        assert "response.completed" in types, f"missing response.completed"

        # 检查 function_call 相关事件
        added_events = [e for e in events if e.get("type") == "response.output_item.added" and e.get("item", {}).get("type") == "function_call"]
        assert len(added_events) >= 1, f"missing function_call added event"

        delta_events = [e for e in events if e.get("type") == "response.function_call_arguments.delta"]
        assert len(delta_events) >= 1, f"missing function_call_arguments.delta events"

        done_events = [e for e in events if e.get("type") == "response.function_call_arguments.done"]
        assert len(done_events) >= 1, f"missing function_call_arguments.done event"

        # 验证最终 arguments
        final_done = done_events[-1]
        final_args = json.loads(final_done.get("arguments", "{}"))
        assert final_args.get("location") == "Beijing", f"expected location=Beijing, got {final_args}"

        ok(f"Streaming tool call → {len(events)} events, function_call done with args={final_args}")

        # =====================================================================
        # Test 3: Full roundtrip (tool result → final message)
        # =====================================================================
        print("\n  Test 3: Full roundtrip with tool result")

        # Step 3a: initial request → get function_call
        step1 = await forward_request(
            {
                "model": "mock-model",
                "input": "What's the weather in Beijing?",
                "tools": tools,
                "stream": False,
            },
            provider,
            "test-req-3a",
        )
        func_items = [item for item in step1.get("output", []) if item.get("type") == "function_call"]
        call_id = func_items[0].get("call_id", "")
        ok(f"Step 1: got function_call with call_id={call_id}")

        # Step 3b: send tool result (function_call_output)
        step2 = await forward_request(
            {
                "model": "mock-model",
                "input": [
                    {"role": "user", "content": "What's the weather in Beijing?"},
                    {
                        "type": "function_call",
                        "call_id": call_id,
                        "name": "get_weather",
                        "arguments": json.dumps({"location": "Beijing"}),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": json.dumps({"temperature": 25, "condition": "sunny"}),
                    },
                ],
                "tools": tools,
                "stream": False,
            },
            provider,
            "test-req-3b",
        )

        output2 = step2.get("output", [])
        msg_items = [item for item in output2 if item.get("type") == "message"]
        assert len(msg_items) >= 1, f"expected at least 1 message, got {output2}"
        msg_content = msg_items[-1].get("content", [])
        content_text = ""
        if isinstance(msg_content, list):
            content_text = "".join(b.get("text", "") for b in msg_content if isinstance(b, dict))
        else:
            content_text = str(msg_content)
        assert "Beijing" in content_text or "weather" in content_text.lower() or "sunny" in content_text.lower(), \
            f"expected final message about Beijing weather, got: {content_text}"
        ok(f"Step 2: final message → {content_text[:50]}...")

        # =====================================================================
        # Test 4: Streaming roundtrip (tool result → final message stream)
        # =====================================================================
        print("\n  Test 4: Streaming roundtrip with tool result")
        stream_events = []
        async for chunk in forward_request_stream(
            {
                "model": "mock-model",
                "input": [
                    {"role": "user", "content": "What's the weather in Beijing?"},
                    {
                        "type": "function_call",
                        "call_id": call_id,
                        "name": "get_weather",
                        "arguments": json.dumps({"location": "Beijing"}),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": json.dumps({"temperature": 25, "condition": "sunny"}),
                    },
                ],
                "tools": tools,
                "stream": True,
            },
            provider,
            "test-req-4",
        ):
            if chunk.startswith("data: "):
                data_str = chunk[6:]
                if data_str.strip() and data_str != "[DONE]":
                    try:
                        stream_events.append(json.loads(data_str))
                    except json.JSONDecodeError:
                        pass

        st_types = [e.get("type") for e in stream_events]
        info(f"roundtrip stream types: {st_types}")
        assert "response.created" in st_types
        assert "response.completed" in st_types

        # 收集最终文本
        from backend.streaming_adapter import StreamChunkBuilder
        final_resp = StreamChunkBuilder.build_response_from_events(stream_events, "mock-model")
        final_output = final_resp.get("output", [])
        final_msgs = [item for item in final_output if item.get("type") == "message"]
        assert len(final_msgs) >= 1, f"expected message in final response, got {final_output}"
        ok(f"Streaming roundtrip → {len(stream_events)} events, final message present")

    finally:
        mock_server.should_exit = True
        await mock_task


# --------------------------------------------------------------------------- #
# 运行
# --------------------------------------------------------------------------- #
print("=" * 60)
print("工具调用全流程测试")
print("=" * 60)

asyncio.run(test_tool_calls())

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
