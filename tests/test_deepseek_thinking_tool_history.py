import asyncio

from backend.responses_adapter import convert_responses_to_chat_body


DEEPSEEK_V4_THINKING_PROVIDER = {
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


async def _provider_level_deepseek_thinking_replays_tool_reasoning():
    """Provider-level thinking should repair tool-call history.

    Provider 配置层开启 thinking 时，也应该修复工具调用历史。
    """
    body = {
        "model": "deepseek-v4-pro",
        "input": [
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "shell",
                "arguments": "{\"cmd\":\"pwd\"}",
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "/tmp",
            },
            {"type": "message", "role": "user", "content": "continue"},
        ],
    }

    chat_body = await convert_responses_to_chat_body(
        body,
        provider=DEEPSEEK_V4_THINKING_PROVIDER,
        stream=True,
    )

    assistant = next(
        message
        for message in chat_body["messages"]
        if message.get("role") == "assistant" and message.get("tool_calls")
    )
    assert assistant["reasoning_content"] == " "


async def _role_content_input_without_type_is_normalized():
    """Codex may send Responses input items as role/content dicts.

    Codex 可能发送没有 type 字段的 role/content 格式输入。
    """
    body = {
        "model": "deepseek-v4-pro",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello"},
                ],
            },
        ],
    }

    chat_body = await convert_responses_to_chat_body(
        body,
        provider=DEEPSEEK_V4_THINKING_PROVIDER,
    )

    assert chat_body["messages"] == [
        {
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }
    ]


def test_provider_level_deepseek_thinking_replays_tool_reasoning_sync():
    asyncio.run(_provider_level_deepseek_thinking_replays_tool_reasoning())


def test_role_content_input_without_type_is_normalized_sync():
    asyncio.run(_role_content_input_without_type_is_normalized())
