//! [MOC-89] forward-trace 端到端。gate 开(`CAS_DIAG_TRACE=1`)时一次成功转发应在
//! `~/.codex-app-transfer/forward-trace/<date>.jsonl` 落一行,验证:
//! - leg1(小响应):credential header(authorization)/ JSON body 字段(api_key)脱敏成
//!   `***`,诊断字段(x-custom / max_tokens)保留,客户端仍拿到完整流式响应(tee 不破流式)
//! - leg2(>256KiB 响应):成功路径 tee 把响应体 cap 到 256KiB,但 jsonl 必须如实记真实
//!   全长 `bytes` 与被丢弃的 `truncated_bytes`(回归:不能谎报「未截断」,见 pre-push review)
//!
//! 用临时 HOME 隔离落盘(不污染真机 ~/.codex-app-transfer)。两 leg 合在**一个**测试里:
//! `set_var(HOME)` 是进程级,拆成两个 `#[test]` 会并发改 HOME 互相打架;单测试顺序跑、
//! 共享同一临时 HOME 与同一 jsonl(seq 区分)即可。开头 `set_var`(HOME + CAS_DIAG_TRACE)
//! 安全:此刻无并发线程读这两个 env;gate 的 OnceLock 在首次转发时才读取并缓存。
//!
//! 注:本文件**刻意不用** `mod common;`(`#[ctor]` 设 `CODEX_APP_TRANSFER_HOME`)——
//! forward-trace 测的是 HOME 真实落盘路径(`CAS_DIAG_TRACE` gate),需在测试体内
//! 精确控制 HOME + jsonl 文件路径,隔离粒度细于 common 提供的进程级 tempdir。

use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, response::Response, routing::any, Router};
use codex_app_transfer_proxy::{build_router, StaticResolver};
use codex_app_transfer_registry::Provider;
use indexmap::IndexMap;
use tokio::net::TcpListener;

async fn spawn(router: Router) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service())
            .await
            .unwrap();
    });
    addr
}

fn provider_for(upstream_base: &str) -> Provider {
    Provider {
        id: "test-upstream".into(),
        name: "Test Upstream".into(),
        base_url: upstream_base.into(),
        auth_scheme: "none".into(),
        api_format: "openai_chat".into(),
        api_key: String::new(),
        models: IndexMap::new(),
        extra_headers: IndexMap::new(),
        model_capabilities: IndexMap::new(),
        request_options: IndexMap::new(),
        is_builtin: false,
        sort_index: 0,
        extra: IndexMap::new(),
    }
}

/// 起一个固定回包的上游 mock + 一个代理,返回代理地址。
async fn start_proxy(
    status: u16,
    content_type: &'static str,
    body: String,
) -> std::net::SocketAddr {
    let upstream = Router::new().fallback(any(move || {
        let body = body.clone();
        async move {
            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .header("x-upstream-secret-token", "shhh-should-be-redacted")
                .body(Body::from(body))
                .unwrap()
        }
    }));
    let upstream_addr = spawn(upstream).await;
    let resolver = Arc::new(StaticResolver::new(
        None,
        vec![provider_for(&format!("http://{upstream_addr}"))],
        Some("test-upstream".into()),
    ));
    spawn(build_router(resolver)).await
}

async fn post_chat(proxy: std::net::SocketAddr) -> reqwest::Response {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("http://{proxy}/v1/chat/completions"))
        .header("authorization", "Bearer SECRET-do-not-leak")
        .header("x-custom", "keepme")
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "model": "gpt-5",
                "max_tokens": 4096,
                "api_key": "sk-should-be-redacted",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string(),
        )
        .send()
        .await
        .expect("proxy send")
}

#[tokio::test]
async fn forward_trace_writes_redacted_and_truncation_honest_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    // [MOC-195] 开发者 shell 若残留 override env,优先级高于下面的 HOME,会把
    // registry 派生路径(含 trace 落盘)指到别处,断言报错却看不出真因 —— 先清掉
    std::env::remove_var(codex_app_transfer_registry::HOME_OVERRIDE_ENV);
    std::env::set_var("HOME", tmp.path());
    std::env::set_var("USERPROFILE", tmp.path()); // Windows 解析回退也指向临时目录
    std::env::set_var("CAS_DIAG_TRACE", "1");
    let dir = tmp.path().join(".codex-app-transfer").join("forward-trace");

    // ── leg1:小 SSE 响应,验脱敏 + 流式完整 ──
    let small_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let proxy1 = start_proxy(200, "text/event-stream", small_body.to_string()).await;
    let resp = post_chat(proxy1).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body_text = String::from_utf8_lossy(&resp.bytes().await.expect("read body")).to_string();
    assert!(body_text.contains("[DONE]"), "流式响应不完整: {body_text}");

    let small = wait_for_line(&dir, |v| {
        v["response"]["body"]["bytes"].as_u64() == Some(small_body.len() as u64)
    })
    .await
    .expect("leg1 jsonl 未生成");
    assert_eq!(small["trace_kind"], "forward_protocol");
    let auth = small["inbound"]["headers"]["authorization"]
        .as_str()
        .unwrap_or("");
    assert!(auth.starts_with("***(len="), "authorization 未脱敏: {auth}");
    assert_eq!(
        small["inbound"]["body"]["content"]["api_key"], "***",
        "body.api_key 未脱敏"
    );
    let resp_tok = small["response"]["headers"]["x-upstream-secret-token"]
        .as_str()
        .unwrap_or("");
    assert!(
        resp_tok.starts_with("***(len="),
        "响应头 token 未脱敏: {resp_tok}"
    );
    assert_eq!(small["inbound"]["headers"]["x-custom"], "keepme");
    assert_eq!(
        small["inbound"]["body"]["content"]["max_tokens"], 4096,
        "max_tokens 被误脱敏(不该用 contains(\"token\"))"
    );
    assert_eq!(small["response"]["status"], 200);
    assert_eq!(small["response"]["body"]["truncated_bytes"], 0);

    // ── leg2:>256KiB 响应,验 tee 截断后如实标注 bytes/truncated_bytes(回归) ──
    let big_len = 300 * 1024;
    let big_body = format!("data: {}\n\n", "A".repeat(big_len));
    let proxy2 = start_proxy(200, "text/event-stream", big_body.clone()).await;
    let resp2 = post_chat(proxy2).await;
    assert_eq!(resp2.status().as_u16(), 200);
    let got = resp2.bytes().await.expect("read big body");
    assert_eq!(
        got.len(),
        big_body.len(),
        "客户端应拿到完整未截断响应(tee 只是旁路)"
    );

    let big = wait_for_line(&dir, |v| {
        v["response"]["body"]["bytes"].as_u64() == Some(big_body.len() as u64)
    })
    .await
    .expect("leg2 jsonl 未生成");
    // 关键回归:bytes = 真实全长(非截断后的 256KiB),truncated_bytes = 被丢弃量 > 0
    assert_eq!(big["response"]["body"]["bytes"], big_body.len());
    let truncated = big["response"]["body"]["truncated_bytes"].as_u64().unwrap();
    assert_eq!(
        truncated as usize,
        big_body.len() - 256 * 1024,
        "truncated_bytes 必须如实反映 tee 丢弃量,不能谎报 0"
    );
}

/// 轮询 forward-trace 目录,返回第一条满足 `pred` 的 jsonl 行(解析成 Value)。jsonl 在
/// 成功路径由 `TracedStream::Drop` 写出,与客户端收齐字节之间有微小异步间隙。
async fn wait_for_line<F>(dir: &std::path::Path, pred: F) -> Option<serde_json::Value>
where
    F: Fn(&serde_json::Value) -> bool,
{
    for _ in 0..60 {
        if let Ok(content) = read_jsonl(dir) {
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if pred(&v) {
                        return Some(v);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

fn read_jsonl(dir: &std::path::Path) -> std::io::Result<String> {
    let mut out = String::new();
    for item in std::fs::read_dir(dir)?.flatten() {
        if item.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push_str(&std::fs::read_to_string(item.path())?);
        }
    }
    Ok(out)
}
