"""Fixture 回放器.

把 Fixture 描述的上游调用挂到 respx 上,然后把 client_request 投递给目标 ASGI
应用,捕获其 client-facing 输出并与 expected 比对。
"""

from __future__ import annotations

import json
import re
from typing import Any

import httpx
import respx

from .fixture import Fixture, UpstreamResponse


def _build_upstream_response(resp: UpstreamResponse) -> httpx.Response:
    """把 fixture 中描述的上游响应转为 httpx.Response."""
    headers = dict(resp.headers)
    if resp.stream:
        body = "".join(frame.data for frame in resp.stream).encode("utf-8")
        headers.setdefault("content-type", "text/event-stream")
        return httpx.Response(status_code=resp.status, headers=headers, content=body)
    if resp.body_json is not None:
        headers.setdefault("content-type", "application/json")
        return httpx.Response(
            status_code=resp.status,
            headers=headers,
            content=json.dumps(resp.body_json).encode("utf-8"),
        )
    if resp.body_text is not None:
        return httpx.Response(
            status_code=resp.status, headers=headers, content=resp.body_text.encode("utf-8")
        )
    return httpx.Response(status_code=resp.status, headers=headers, content=b"")


def build_respx_mock(fix: Fixture) -> respx.MockRouter:
    """根据 fixture 构造一个 respx router(进入 with 块后即生效).

    使用方式::

        with build_respx_mock(fix) as mock:
            ... # 这里所有 httpx 出站都会被 mock 拦截

    `url_pattern` 为正则;若以 `re:` 开头会去掉前缀。
    """
    router = respx.mock(assert_all_called=False, assert_all_mocked=True)
    for upstream in fix.upstream:
        pattern = upstream.url_pattern
        if pattern.startswith("re:"):
            pattern = pattern[3:]
        # respx 接受字符串(精确匹配)或编译后的 regex
        try:
            url_arg: Any = re.compile(pattern)
        except re.error:
            url_arg = pattern
        method = upstream.method.lower()
        route = router.route(method=method.upper(), url=url_arg)
        route.return_value = _build_upstream_response(upstream.response)
    return router


async def replay_against_app(app: Any, fix: Fixture) -> httpx.Response:
    """把 fix.client_request 通过 ASGI app 投递,返回响应供测试断言."""
    transport = httpx.ASGITransport(app=app)
    cr = fix.client_request
    request_kwargs: dict[str, Any] = {"headers": cr.headers}
    if cr.body_json is not None:
        request_kwargs["json"] = cr.body_json
    elif cr.body_text is not None:
        request_kwargs["content"] = cr.body_text
    async with httpx.AsyncClient(transport=transport, base_url="http://app.local") as client:
        with build_respx_mock(fix):
            return await client.request(cr.method, cr.path, **request_kwargs)


def assert_response_matches(resp: httpx.Response, fix: Fixture) -> None:
    """按 fixture.expected 校验响应。失败抛 AssertionError(pytest 友好)."""
    exp = fix.expected
    if exp.status is not None:
        assert resp.status_code == exp.status, (
            f"[{fix.name}] status {resp.status_code} != expected {exp.status}"
        )
    for k, v in exp.headers_contain.items():
        actual = resp.headers.get(k, "")
        assert v in actual, f"[{fix.name}] header {k!r}={actual!r} 不含 {v!r}"
    body_text = resp.text
    if exp.body_text is not None:
        assert body_text == exp.body_text, f"[{fix.name}] body_text 不一致"
    if exp.body_json is not None:
        assert resp.json() == exp.body_json, f"[{fix.name}] body_json 不一致"
    for needle in exp.body_substrings + exp.stream_substrings:
        assert needle in body_text, f"[{fix.name}] body 缺少子串 {needle!r}"
    if exp.stream_frames:
        joined = "".join(f.data for f in exp.stream_frames)
        assert joined == body_text, (
            f"[{fix.name}] stream_frames 拼接后与响应体不一致(逐帧 diff 留给 Rust 端口)"
        )
