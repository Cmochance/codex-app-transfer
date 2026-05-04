"""Smoke 测试:只验证录制回放基建本身,不依赖真实 API key。

跑法::

    . .venv/bin/activate && pytest tests/test_replay_smoke.py -v
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import httpx
import pytest

from tests.replay import (
    Fixture,
    dump_fixture,
    list_fixtures,
    load_fixture,
)
from tests.replay.fixture import (
    ClientRequest,
    ExpectedClientResponse,
    FrameRecord,
    UpstreamCall,
    UpstreamResponse,
)
from tests.replay.player import build_respx_mock, assert_response_matches


EXAMPLE_FIXTURE = (
    Path(__file__).parent
    / "replay"
    / "fixtures"
    / "_example_openai_chat_streaming.json"
)


def test_example_fixture_exists():
    assert EXAMPLE_FIXTURE.is_file(), "样例 fixture 必须存在以验证 schema"


def test_load_dump_roundtrip(tmp_path: Path):
    fix = load_fixture(EXAMPLE_FIXTURE)
    assert fix.name == "_example_openai_chat_streaming"
    assert fix.provider == "openai"
    assert len(fix.upstream) == 1
    assert fix.upstream[0].response.stream, "样例必须包含 SSE 流"

    out = tmp_path / "round.json"
    dump_fixture(fix, out)
    fix2 = load_fixture(out)
    assert fix2.name == fix.name
    assert fix2.provider == fix.provider
    assert len(fix2.upstream) == len(fix.upstream)
    assert (
        fix2.upstream[0].response.stream[0].data
        == fix.upstream[0].response.stream[0].data
    )
    assert fix2.expected.stream_substrings == fix.expected.stream_substrings


def test_list_fixtures_skips_underscore_files():
    """fixtures/ 下以 _ 开头的样例不应被业务 fixture 列表收录."""
    found = list_fixtures()
    for p in found:
        assert not p.name.startswith("_"), f"内部样例不该被列入: {p}"


def test_player_replays_streaming_upstream():
    """构造一个最小 fixture,验证 player 能把 SSE 帧拼接给 mock 上游."""
    fix = Fixture(
        name="inline_smoke_streaming",
        description="纯内存 fixture,验证 build_respx_mock 拼帧",
        provider="test",
        client_request=ClientRequest(
            method="POST",
            path="/v1/chat/completions",
            headers={"content-type": "application/json"},
            body_json={"x": 1},
        ),
        upstream=[
            UpstreamCall(
                url_pattern=r"https://upstream\.test/echo",
                method="POST",
                response=UpstreamResponse(
                    status=200,
                    headers={"content-type": "text/event-stream"},
                    stream=[
                        FrameRecord(data="data: hello\n\n"),
                        FrameRecord(data="data: [DONE]\n\n"),
                    ],
                ),
            )
        ],
        expected=ExpectedClientResponse(
            status=200,
            headers_contain={"content-type": "text/event-stream"},
            stream_substrings=["hello", "[DONE]"],
        ),
    )

    async def run() -> httpx.Response:
        with build_respx_mock(fix):
            async with httpx.AsyncClient() as client:
                return await client.post(
                    "https://upstream.test/echo", json={"x": 1}
                )

    resp = asyncio.run(run())
    assert resp.status_code == 200
    assert "text/event-stream" in resp.headers.get("content-type", "")
    body = resp.text
    assert "hello" in body
    assert "[DONE]" in body
    # 走一遍 expected 校验路径
    assert_response_matches(resp, fix)


def test_strict_redaction_in_example():
    """样例 fixture 不能含真实凭据."""
    raw = json.loads(EXAMPLE_FIXTURE.read_text(encoding="utf-8"))
    auth = raw["client_request"]["headers"].get("authorization", "")
    assert auth == "<redacted>", "样例 fixture 必须保持脱敏"
