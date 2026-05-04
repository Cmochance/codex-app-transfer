"""Fixture schema 与 IO 工具.

fixture 是 Python 与 Rust 两个实现之间的契约 —— 任何 Rust 端口必须能在同一份
fixture 上驱动产生相同的 client-facing 输出。schema 定义见 fixtures/_schema.md。
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any


FIXTURES_DIR = Path(__file__).parent / "fixtures"


@dataclass
class FrameRecord:
    """SSE / 流式响应的单帧."""

    data: str
    delay_ms: int = 0


@dataclass
class ClientRequest:
    method: str
    path: str
    headers: dict[str, str] = field(default_factory=dict)
    body_json: Any = None
    body_text: str | None = None


@dataclass
class UpstreamResponse:
    status: int
    headers: dict[str, str] = field(default_factory=dict)
    body_text: str | None = None
    body_json: Any = None
    stream: list[FrameRecord] = field(default_factory=list)


@dataclass
class UpstreamCall:
    """单次上游调用记录."""

    url_pattern: str
    method: str
    response: UpstreamResponse


@dataclass
class ExpectedClientResponse:
    status: int | None = None
    headers_contain: dict[str, str] = field(default_factory=dict)
    body_json: Any = None
    body_text: str | None = None
    body_substrings: list[str] = field(default_factory=list)
    stream_frames: list[FrameRecord] = field(default_factory=list)
    stream_substrings: list[str] = field(default_factory=list)


@dataclass
class Fixture:
    name: str
    description: str
    provider: str
    client_request: ClientRequest
    upstream: list[UpstreamCall]
    expected: ExpectedClientResponse
    notes: str = ""


def _frame_from_dict(d: dict[str, Any]) -> FrameRecord:
    return FrameRecord(data=d["data"], delay_ms=int(d.get("delay_ms", 0)))


def load_fixture(path: Path | str) -> Fixture:
    """从磁盘加载 fixture JSON."""
    path = Path(path)
    raw = json.loads(path.read_text(encoding="utf-8"))
    cr = raw["client_request"]
    upstream_raw = raw.get("upstream", [])
    if isinstance(upstream_raw, dict):
        upstream_raw = [upstream_raw]
    upstream: list[UpstreamCall] = []
    for u in upstream_raw:
        resp = u["response"]
        upstream.append(
            UpstreamCall(
                url_pattern=u["url_pattern"],
                method=u.get("method", "POST"),
                response=UpstreamResponse(
                    status=int(resp.get("status", 200)),
                    headers=dict(resp.get("headers", {})),
                    body_text=resp.get("body_text"),
                    body_json=resp.get("body_json"),
                    stream=[_frame_from_dict(f) for f in resp.get("stream", [])],
                ),
            )
        )
    expected = raw.get("expected", {})
    return Fixture(
        name=raw["name"],
        description=raw.get("description", ""),
        provider=raw.get("provider", ""),
        client_request=ClientRequest(
            method=cr.get("method", "POST"),
            path=cr["path"],
            headers=dict(cr.get("headers", {})),
            body_json=cr.get("body_json"),
            body_text=cr.get("body_text"),
        ),
        upstream=upstream,
        expected=ExpectedClientResponse(
            status=expected.get("status"),
            headers_contain=dict(expected.get("headers_contain", {})),
            body_json=expected.get("body_json"),
            body_text=expected.get("body_text"),
            body_substrings=list(expected.get("body_substrings", [])),
            stream_frames=[_frame_from_dict(f) for f in expected.get("stream_frames", [])],
            stream_substrings=list(expected.get("stream_substrings", [])),
        ),
        notes=raw.get("notes", ""),
    )


def dump_fixture(fix: Fixture, path: Path | str) -> None:
    """把 fixture 写回磁盘(供 recorder 使用)."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    data = asdict(fix)
    # 移除值为空 / None 的可选字段,保持 fixture JSON 简洁
    data = _strip_empty(data)
    path.write_text(
        json.dumps(data, ensure_ascii=False, indent=2, sort_keys=False) + "\n",
        encoding="utf-8",
    )


def _strip_empty(obj: Any) -> Any:
    if isinstance(obj, dict):
        return {
            k: _strip_empty(v)
            for k, v in obj.items()
            if v not in (None, "", [], {})
        }
    if isinstance(obj, list):
        return [_strip_empty(v) for v in obj]
    return obj


def list_fixtures(directory: Path | str = FIXTURES_DIR) -> list[Path]:
    """枚举 fixtures/ 下所有 .json(忽略下划线开头的内部文件)."""
    directory = Path(directory)
    return sorted(p for p in directory.glob("**/*.json") if not p.name.startswith("_"))
