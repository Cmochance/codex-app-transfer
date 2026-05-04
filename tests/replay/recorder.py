"""Fixture 录制器.

通过自定义 httpx Transport 截获真实出站请求与响应,转写为 Fixture JSON。
设计目标:在现有 Python 仓库中,用真实 API key 跑一次,把响应固化下来,后续
回放和 Rust 端口都不再需要联网。

**安全提醒**:录制下来的 fixture 会包含真实响应内容;请在提交前手动检查并
脱敏 Authorization / API Key 等敏感头,再写入 fixtures/ 目录。
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

import httpx

from .fixture import (
    ClientRequest,
    ExpectedClientResponse,
    Fixture,
    FrameRecord,
    UpstreamCall,
    UpstreamResponse,
)


REDACTED = "<redacted>"
SENSITIVE_HEADERS = {
    # 凭据
    "authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "set-cookie",
    # 账号 / 项目 / 用户标识(虽非凭据,公网仓库泄漏会暴露用户)
    "openai-organization",
    "openai-project",
    "x-organization",
    "x-organization-id",
    "x-user-id",
    "x-account-id",
    # Moonshot / Kimi 系列
    "msh-org-id",
    "msh-project-id",
    "msh-uid",
    "msh-request-id",
    "msh-trace-id",
    # 追踪/请求 ID(部分含 IP / 路由路径,统一脱敏)
    "x-request-id",
    "request-id",
    "cf-ray",
    "x-trace-id",
}


def _sanitize_headers(headers: dict[str, str]) -> dict[str, str]:
    out: dict[str, str] = {}
    for k, v in headers.items():
        if k.lower() in SENSITIVE_HEADERS:
            out[k] = REDACTED
        else:
            out[k] = v
    return out


@dataclass
class _Capture:
    upstream: list[UpstreamCall] = field(default_factory=list)


class RecordingTransport(httpx.AsyncBaseTransport):
    """包一层 httpx Transport,记录所有出站请求与响应."""

    def __init__(self, inner: httpx.AsyncBaseTransport, capture: _Capture):
        self._inner = inner
        self._capture = capture

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        resp = await self._inner.handle_async_request(request)
        body = await resp.aread()
        # 重建一个可复用的 response,因为 aread 之后 stream 已耗尽
        rebuilt = httpx.Response(
            status_code=resp.status_code,
            headers=resp.headers,
            content=body,
            request=request,
        )
        ct = resp.headers.get("content-type", "")
        upstream_resp = UpstreamResponse(
            status=resp.status_code,
            headers=_sanitize_headers(dict(resp.headers)),
        )
        if "text/event-stream" in ct:
            # 简单按 \n\n 切帧;真实场景 frame 边界更复杂,recorder 阶段够用
            text = body.decode("utf-8", errors="replace")
            frames = [FrameRecord(data=chunk + "\n\n") for chunk in text.split("\n\n") if chunk]
            upstream_resp.stream = frames
        elif "application/json" in ct:
            try:
                upstream_resp.body_json = json.loads(body.decode("utf-8"))
            except Exception:
                upstream_resp.body_text = body.decode("utf-8", errors="replace")
        else:
            upstream_resp.body_text = body.decode("utf-8", errors="replace")
        self._capture.upstream.append(
            UpstreamCall(
                url_pattern=str(request.url),
                method=request.method,
                response=upstream_resp,
            )
        )
        return rebuilt

    async def aclose(self) -> None:
        await self._inner.aclose()


def make_recording_client(real_transport: httpx.AsyncBaseTransport | None = None) -> tuple[httpx.AsyncClient, _Capture]:
    """构造一个会记录所有出站的 httpx.AsyncClient。

    使用方式::

        client, cap = make_recording_client()
        # 在被测代码里把 client 注入(替换业务里的 httpx.AsyncClient)
        ... 触发请求 ...
        upstream = cap.upstream  # 录到的所有上游调用
    """
    cap = _Capture()
    inner = real_transport or httpx.AsyncHTTPTransport()
    client = httpx.AsyncClient(transport=RecordingTransport(inner, cap))
    return client, cap


def fixture_from_capture(
    *,
    name: str,
    provider: str,
    client_request: ClientRequest,
    capture: _Capture,
    expected: ExpectedClientResponse,
    description: str = "",
    notes: str = "",
) -> Fixture:
    """把一次录制的 capture 组合成 Fixture。"""
    return Fixture(
        name=name,
        description=description,
        provider=provider,
        client_request=ClientRequest(
            method=client_request.method,
            path=client_request.path,
            headers=_sanitize_headers(client_request.headers),
            body_json=client_request.body_json,
            body_text=client_request.body_text,
        ),
        upstream=list(capture.upstream),
        expected=expected,
        notes=notes,
    )


def redact_fixture_inplace(fix: Fixture, extra_keys: list[str] | None = None) -> None:
    """二次脱敏:把 fixture 中所有敏感头清掉,可附加 extra_keys."""
    keys = set(SENSITIVE_HEADERS)
    if extra_keys:
        keys |= {k.lower() for k in extra_keys}
    fix.client_request.headers = {
        k: (REDACTED if k.lower() in keys else v) for k, v in fix.client_request.headers.items()
    }
    for u in fix.upstream:
        u.response.headers = {
            k: (REDACTED if k.lower() in keys else v) for k, v in u.response.headers.items()
        }
