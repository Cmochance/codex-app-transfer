//! [MOC-234] responses↔responses 1:1 passthrough 的端到端验证。
//!
//! 拓扑:reqwest client ─► [本 proxy axum + StaticResolver(apiFormat=responses)] ─► [mock 上游]
//!
//! 双轮对话(turn2 用 turn1 的 response_id 作 previous_response_id),验证两件事:
//! 1. **通信正常**:proxy 把 `/responses` 请求体 1:1 转发给原生上游(语义零改写),
//!    并把上游 SSE 响应 1:1 回灌客户端。
//! 2. **context 分类准确**:开启 breakdown 后,proxy 用**独立观测累积器**沿
//!    `previous_response_id` 链重建全历史,responses 原生 breakdown 把各来源精确归桶
//!    并按 `prompt_cache_key` 落盘 —— turn2 的明细应反映「instructions + 全部历史消息 + tools」。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{body::Bytes, extract::State, http::header::CONTENT_TYPE, routing::post, Router};
use codex_app_transfer_adapters::responses::{load_context_breakdown, set_breakdown_enabled};
use codex_app_transfer_proxy::{build_router, StaticResolver};
use codex_app_transfer_registry::Provider;
use indexmap::IndexMap;
use serde_json::{json, Value};
use tokio::net::TcpListener;

mod common;

const CONV_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

#[derive(Default)]
struct MockState {
    /// 上游逐轮收到的请求体(供 1:1 断言)。
    received: Mutex<Vec<Vec<u8>>>,
    /// 轮次计数 → 决定本轮回什么 response_id / assistant 文本。
    turn: AtomicUsize,
}

/// mock 原生 Responses 上游:捕获请求体,回一段含 `response.completed` 的 SSE。
async fn mock_upstream(
    State(state): State<Arc<MockState>>,
    body: Bytes,
) -> impl axum::response::IntoResponse {
    state.received.lock().unwrap().push(body.to_vec());
    let n = state.turn.fetch_add(1, Ordering::SeqCst) + 1;
    let resp_id = format!("r{n}");
    let assistant_text = format!("reply-{n}");
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": resp_id,
            "object": "response",
            "status": "completed",
            "output": [
                {"type":"message","role":"assistant","content":[{"type":"output_text","text": assistant_text}]}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        }
    });
    let sse = format!(
        "event: response.created\ndata: {}\n\nevent: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"type":"response.created","response":{"id": format!("r{n}")}}),
        completed
    );
    ([(CONTENT_TYPE, "text/event-stream")], sse)
}

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

fn responses_provider(upstream_base: &str) -> Provider {
    Provider {
        id: "test-upstream".into(),
        name: "Test Upstream".into(),
        base_url: upstream_base.into(),
        auth_scheme: "none".into(),
        api_format: "responses".into(),
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

fn turn_body(user_text: &str, prev_id: Option<&str>) -> Value {
    let mut b = json!({
        "model": "gpt-5.5",
        "stream": true,
        "instructions": "You are Codex.",
        "prompt_cache_key": CONV_ID,
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text": user_text}]}
        ],
        "tools": [
            {"type":"function","name":"shell","description":"run a shell command","parameters":{"type":"object"}}
        ]
    });
    if let Some(p) = prev_id {
        b["previous_response_id"] = json!(p);
    }
    b
}

/// 轮询读 breakdown 落盘文件,直到 `messages` 桶条目数达 `want_messages` 或超时。
async fn wait_breakdown_messages(want_messages: u64) -> Option<Value> {
    for _ in 0..40 {
        if let Some(bd) = load_context_breakdown(CONV_ID) {
            let m = bd
                .get("categories")
                .and_then(Value::as_array)
                .and_then(|cs| {
                    cs.iter()
                        .find(|c| c.get("key").and_then(Value::as_str) == Some("messages"))
                })
                .and_then(|c| c.get("items").and_then(Value::as_u64))
                .unwrap_or(0);
            if m >= want_messages {
                return Some(bd);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

fn category_items(bd: &Value, key: &str) -> u64 {
    bd.get("categories")
        .and_then(Value::as_array)
        .and_then(|cs| {
            cs.iter()
                .find(|c| c.get("key").and_then(Value::as_str) == Some(key))
        })
        .and_then(|c| c.get("items").and_then(Value::as_u64))
        .unwrap_or(0)
}

#[tokio::test]
async fn responses_passthrough_e2e_communication_and_breakdown() {
    set_breakdown_enabled(true); // 开启观测整合(默认关)

    let mock = Arc::new(MockState::default());
    let upstream_addr = spawn(
        Router::new()
            .route("/responses", post(mock_upstream))
            .with_state(mock.clone()),
    )
    .await;
    let upstream_base = format!("http://{upstream_addr}");

    let resolver = Arc::new(StaticResolver::new(
        None,
        vec![responses_provider(&upstream_base)],
        Some("test-upstream".into()),
    ));
    let proxy_addr = spawn(build_router(resolver)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // ── Turn 1:无 previous_response_id ──
    let t1 = turn_body("alpha question", None);
    let r1 = client
        .post(format!("http://{proxy_addr}/responses"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&t1).unwrap())
        .send()
        .await
        .expect("turn1 send");
    assert!(r1.status().is_success(), "turn1 status: {}", r1.status());
    let r1_text = r1.text().await.unwrap();
    // 通信:上游 SSE 1:1 回灌(含 mock 的 response_id + assistant 文本)。
    assert!(
        r1_text.contains("\"id\":\"r1\""),
        "turn1 应回灌上游 response.id"
    );
    assert!(
        r1_text.contains("reply-1"),
        "turn1 应回灌上游 assistant 文本"
    );
    assert!(
        r1_text.contains("response.completed"),
        "turn1 应原样保留 response.completed 事件"
    );

    // 通信:上游收到的请求体与客户端所发**语义 1:1**(passthrough 不改写字段)。
    {
        let recv = mock.received.lock().unwrap();
        let upstream_seen: Value = serde_json::from_slice(&recv[0]).unwrap();
        assert_eq!(
            upstream_seen, t1,
            "turn1 请求体必须 1:1 透传给上游(零字段改写)"
        );
    }

    // 等 turn1 的 breakdown 落盘(全历史 = instructions + user alpha + tools;messages=1)。
    let bd1 = wait_breakdown_messages(1)
        .await
        .expect("turn1 breakdown 应落盘");
    assert_eq!(
        category_items(&bd1, "system_prompt"),
        1,
        "instructions → system_prompt"
    );
    assert_eq!(category_items(&bd1, "messages"), 1, "仅 user alpha");
    assert_eq!(category_items(&bd1, "tools"), 1, "shell 工具定义");

    // ── Turn 2:previous_response_id = turn1 的 r1 ──
    let t2 = turn_body("beta question", Some("r1"));
    let r2 = client
        .post(format!("http://{proxy_addr}/responses"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&t2).unwrap())
        .send()
        .await
        .expect("turn2 send");
    assert!(r2.status().is_success());
    let r2_text = r2.text().await.unwrap();
    assert!(
        r2_text.contains("reply-2"),
        "turn2 应回灌第二轮 assistant 文本"
    );

    {
        let recv = mock.received.lock().unwrap();
        let upstream_seen: Value = serde_json::from_slice(&recv[1]).unwrap();
        assert_eq!(
            upstream_seen, t2,
            "turn2 请求体必须 1:1 透传(含 previous_response_id 原样)"
        );
    }

    // context 分类准确:turn2 沿 r1 链重建全历史 →
    //   system_prompt: instructions(1)
    //   messages: user alpha + assistant reply-1 + user beta = 3
    //   tools: shell(1)
    let bd2 = wait_breakdown_messages(3)
        .await
        .expect("turn2 全历史 breakdown 应落盘");
    assert_eq!(
        category_items(&bd2, "system_prompt"),
        1,
        "instructions 仍归 system_prompt"
    );
    assert_eq!(
        category_items(&bd2, "messages"),
        3,
        "全历史:user alpha + assistant reply-1(观测镜像捕获) + user beta"
    );
    assert_eq!(category_items(&bd2, "tools"), 1, "tools 定义计入");
    // 总数自洽 = 各类之和,且 > turn1(历史增长)。
    let sum: u64 = bd2
        .get("categories")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|c| c.get("tokens").and_then(Value::as_u64).unwrap_or(0))
        .sum();
    assert_eq!(bd2.get("total_tokens").and_then(Value::as_u64), Some(sum));
    assert!(
        bd2.get("total_tokens").and_then(Value::as_u64).unwrap()
            > bd1.get("total_tokens").and_then(Value::as_u64).unwrap(),
        "全历史 token 应多于首轮"
    );

    set_breakdown_enabled(false); // 复位,避免影响同 binary 其它用例
}

/// orphan-修复 mock 上游:校验每个 function_call_output 是否有同 call_id 的 function_call
/// 在 input 内(模拟 new-api 类 store:false 反代的窄校验)。
/// - input 无 function_call_output(纯对话轮)→ 200,output 带一个 function_call(call_FIX)
///   让 proxy 的 tool-call 缓存记下;
/// - input 有 function_call_output 但缺配对 function_call → 400 orphan(referencing 该 call_id);
/// - input 有配对(proxy 拼接后)→ 200。
async fn mock_orphan_upstream(
    State(state): State<Arc<MockState>>,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.received.lock().unwrap().push(body.to_vec());
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let input = v
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let present: std::collections::HashSet<String> = input
        .iter()
        .filter(|it| it.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|it| it.get("call_id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let orphan = input
        .iter()
        .filter(|it| it.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|it| it.get("call_id").and_then(Value::as_str))
        .find(|cid| !present.contains(*cid));
    if let Some(cid) = orphan {
        // 窄校验失败 → 400 orphan(裸 JSON,标 event-stream,复刻真机)
        let err = format!(
            "{{\"error\":{{\"message\":\"No tool call found for function call output with call_id {cid}.\",\"type\":\"invalid_request_error\",\"param\":\"input\"}}}}"
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            [(CONTENT_TYPE, "text/event-stream")],
            err,
        )
            .into_response();
    }
    // 200:纯对话轮回一个 function_call 供缓存记录;否则回普通 message。
    let has_output = input
        .iter()
        .any(|it| it.get("type").and_then(Value::as_str) == Some("function_call_output"));
    let out_item = if has_output {
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]})
    } else {
        json!({"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}","call_id":"call_FIX"})
    };
    let completed = json!({
        "type":"response.completed",
        "response":{"id":"r_orphan","object":"response","status":"completed","output":[out_item]}
    });
    let sse = format!("event: response.completed\ndata: {completed}\n\ndata: [DONE]\n\n");
    ([(CONTENT_TYPE, "text/event-stream")], sse).into_response()
}

#[tokio::test]
async fn orphan_function_call_400_is_repaired_and_retried_to_success() {
    // [MOC-234] 真机复现的降级:store:false 反代续轮发 function_call_output 找不到自己产生的
    // function_call → 400。proxy 应:① 从缓存(上一轮响应记录的 function_call)拼回 input
    // + 去 previous_response_id;② 透明重发;③ 客户端拿到 200(而非 400)。
    let mock = Arc::new(MockState::default());
    let upstream_addr = spawn(
        Router::new()
            .route("/responses", post(mock_orphan_upstream))
            .with_state(mock.clone()),
    )
    .await;
    let resolver = Arc::new(StaticResolver::new(
        None,
        vec![responses_provider(&format!("http://{upstream_addr}"))],
        Some("test-upstream".into()),
    ));
    let proxy_addr = spawn(build_router(resolver)).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Turn 1:纯对话轮 → 上游回 function_call(call_FIX),proxy tee 记进降级缓存。
    let t1 = json!({
        "model":"gpt-5.5","stream":true,"store":false,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"run ls"}]}]
    });
    let r1 = client
        .post(format!("http://{proxy_addr}/responses"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&t1).unwrap())
        .send()
        .await
        .unwrap();
    assert!(r1.status().is_success());
    let r1_text = r1.text().await.unwrap();
    assert!(
        r1_text.contains("call_FIX"),
        "turn1 应回灌 function_call 供缓存记录"
    );

    // Turn 2:orphan —— 只发 call_FIX 的 output + previous_response_id,不带 function_call。
    let t2 = json!({
        "model":"gpt-5.5","stream":true,"store":false,
        "previous_response_id":"r_orphan",
        "input":[{"type":"function_call_output","call_id":"call_FIX","output":"file1\nfile2"}]
    });
    let r2 = client
        .post(format!("http://{proxy_addr}/responses"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&t2).unwrap())
        .send()
        .await
        .unwrap();
    // 关键:客户端应拿到 200(proxy 拼接 function_call 重发成功),而非 400/ response.failed。
    assert!(
        r2.status().is_success(),
        "orphan 续轮应被 proxy 修复重试成功,status={}",
        r2.status()
    );
    let r2_text = r2.text().await.unwrap();
    assert!(
        r2_text.contains("response.completed"),
        "重试成功应回灌 response.completed,而非 response.failed: {r2_text}"
    );
    assert!(
        !r2_text.contains("response.failed"),
        "修复成功不应出现 response.failed: {r2_text}"
    );

    // 上游应收到 turn2 两次:第一次 orphan(400)+ 修复后重发(200)。
    let recv = mock.received.lock().unwrap();
    // recv[0]=turn1, recv[1]=turn2 orphan, recv[2]=turn2 repaired
    assert!(
        recv.len() >= 3,
        "应有 turn1 + turn2-orphan + turn2-repaired 三次上游请求,实际 {}",
        recv.len()
    );
    let repaired: Value = serde_json::from_slice(recv.last().unwrap()).unwrap();
    let rin = repaired["input"].as_array().unwrap();
    assert!(
        rin.iter()
            .any(|it| it["type"] == "function_call" && it["call_id"] == "call_FIX"),
        "重发请求 input 必须含拼回的 function_call: {repaired}"
    );
    assert!(
        repaired.get("previous_response_id").is_none(),
        "修复重发应去掉 previous_response_id"
    );
}
