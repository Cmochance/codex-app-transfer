#!/usr/bin/env python3
"""
Kimi Code 真实上游测试
使用用户配置 (~/.codex-app-transfer/config.json) 中的 kimi-code provider 做真实请求。
同时测试多个可能的 baseUrl 变体和 User-Agent。
"""

import asyncio
import json
import sys

sys.path.insert(0, "/Users/alysechen/alysechen/github/codex-app-transfer")

from backend.config import load_config

cfg = load_config()
user_provider = None
for p in cfg.get("providers", []):
    if p.get("id") == "af8696bc" or p.get("name") == "Kimi Code":
        user_provider = p
        break

if not user_provider:
    print("ERROR: 未找到用户配置的 kimi-code provider")
    sys.exit(1)

print("=" * 60)
print("Kimi Code 真实上游测试")
print("=" * 60)
print(f"Provider ID: {user_provider.get('id')}")
print(f"Name: {user_provider.get('name')}")
print(f"Base URL: {user_provider.get('baseUrl')}")
print(f"API Format: {user_provider.get('apiFormat')}")
print(f"API Key: {user_provider.get('apiKey', '')[:12]}...")
print()


async def probe_url(base_url: str, provider: dict, extra_headers: dict | None = None):
    """直接探测一个 URL，看返回什么状态码。"""
    import httpx
    from backend.proxy import build_upstream_url, get_upstream_headers

    api_format = provider.get("apiFormat", "openai_chat")
    upstream_url = build_upstream_url(base_url, api_format)
    headers = get_upstream_headers(provider)
    if extra_headers:
        headers.update(extra_headers)

    print(f"  [Probe] {upstream_url}")
    if extra_headers:
        print(f"    Extra headers: {extra_headers}")
    try:
        async with httpx.AsyncClient(timeout=15.0, follow_redirects=False) as client:
            resp = await client.head(upstream_url, headers={k: v for k, v in headers.items() if k.lower() != "content-type"})
            print(f"    HEAD → {resp.status_code}")
            if resp.status_code in {404, 405}:
                resp = await client.get(upstream_url, headers={k: v for k, v in headers.items() if k.lower() != "content-type"})
                print(f"    GET  → {resp.status_code}")
            if resp.status_code in {404, 405}:
                test_body = {"model": provider.get("models", {}).get("default", "kimi-for-coding"), "messages": [{"role": "user", "content": "ping"}], "max_tokens": 8, "stream": False}
                resp = await client.post(upstream_url, headers=headers, json=test_body)
                print(f"    POST → {resp.status_code}, body: {resp.text[:300]}")
            return resp.status_code, resp.text[:500]
    except Exception as e:
        print(f"    ERROR: {e.__class__.__name__}: {e}")
        return None, str(e)


async def test_variants():
    """测试多种 baseUrl 变体"""
    print("[Test 0] 探测不同 baseUrl 变体")
    for v in ["https://api.kimi.com/coding", "https://api.kimi.com/coding/v1"]:
        print()
        status, text = await probe_url(v, user_provider)
        print(f"    Result: status={status}")
    print()


async def test_user_agents():
    """测试不同 User-Agent 对 /v1 端点的影响"""
    print("[Test 0b] 探测不同 User-Agent 对 https://api.kimi.com/coding/v1 的影响")
    uas = [
        {},
        {"User-Agent": "Claude-Code/1.0"},
        {"User-Agent": "Kimi-CLI/1.0"},
        {"User-Agent": "Roo-Code/1.0"},
        {"User-Agent": "Kilo-Code/1.0"},
        {"User-Agent": "OpenAI/v1"},
    ]
    for ua in uas:
        print()
        status, text = await probe_url("https://api.kimi.com/coding/v1", user_provider, extra_headers=ua)
        print(f"    Result: status={status}")
    print()


async def main():
    await test_variants()
    await test_user_agents()

    print("=" * 60)
    print("测试完成")
    print("=" * 60)


asyncio.run(main())
