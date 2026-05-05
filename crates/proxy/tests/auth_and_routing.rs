//! 端到端验证 B1(多 provider 路由)+ B2(鉴权改写)。
//!
//! 拓扑:
//!     reqwest client ──► [Rust 代理 + StaticResolver]
//!                         ├─► upstream-a(echo mock,所有请求回 JSON 反射)
//!                         └─► upstream-b(echo mock,所有请求回 JSON 反射)
//!
//! upstream mock 把收到的 method / path / headers / body 原样反射为 JSON,
//! 测试拿到代理的响应后即可判定:
//! - 是否打到了正确的上游(用 mock 自身的 marker 头区分)
//! - Authorization / X-Api-Key / extra-headers 是否被代理重写正确
//! - body 中的 model 字段是否被剥掉 `<slug>/` 前缀

use std::sync::Arc;

use axum::{body::Body, extract::Request, response::Response, routing::any, Router};
use codex_app_transfer_proxy::{build_router, proxy_telemetry, StaticResolver};
use codex_app_transfer_registry::Provider;
use indexmap::IndexMap;
use serde_json::json;
use tokio::net::TcpListener;

/// echo-back 上游:把收到的请求镜像成 JSON 返回。`marker` 用来在响应头里
/// 标记是哪个 mock,以便测试断言代理选对了上游。
fn echo_mock(marker: &'static str) -> Router {
    Router::new().fallback(any(move |req: Request| async move {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap_or_default();
        let mut headers_map = serde_json::Map::new();
        for (k, v) in parts.headers.iter() {
            headers_map.insert(
                k.as_str().to_owned(),
                serde_json::Value::String(v.to_str().unwrap_or("").to_owned()),
            );
        }
        let payload = json!({
            "marker": marker,
            "method": parts.method.as_str(),
            "path": parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"),
            "headers": headers_map,
            "body": String::from_utf8_lossy(&bytes),
        });
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("x-mock-marker", marker)
            .body(Body::from(payload.to_string()))
            .unwrap()
    }))
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

fn provider(
    id: &str,
    base: &str,
    api_key: &str,
    auth_scheme: &str,
    extras: &[(&str, &str)],
) -> Provider {
    let mut h = IndexMap::new();
    for (k, v) in extras {
        h.insert((*k).to_owned(), (*v).to_owned());
    }
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: base.into(),
        auth_scheme: auth_scheme.into(),
        api_format: "openai_chat".into(),
        api_key: api_key.into(),
        models: IndexMap::new(),
        extra_headers: h,
        model_capabilities: IndexMap::new(),
        request_options: IndexMap::new(),
        is_builtin: false,
        sort_index: 0,
        extra: IndexMap::new(),
    }
}

struct Stack {
    proxy: std::net::SocketAddr,
    upstream_a: std::net::SocketAddr,
    upstream_b: std::net::SocketAddr,
}

async fn build_stack() -> Stack {
    let upstream_a = spawn(echo_mock("upstream-a")).await;
    let upstream_b = spawn(echo_mock("upstream-b")).await;
    let resolver = Arc::new(StaticResolver::new(
        Some("cas_test_gw".into()),
        vec![
            provider(
                "provider-a",
                &format!("http://{upstream_a}"),
                "sk-a-bearer",
                "bearer",
                &[],
            ),
            provider(
                "provider-b",
                &format!("http://{upstream_b}"),
                "sk-b-key",
                "x-api-key",
                &[("User-Agent", "TestAgent/1.0")],
            ),
        ],
        Some("provider-a".into()),
    ));
    let proxy = spawn(build_router(resolver)).await;
    Stack {
        proxy,
        upstream_a,
        upstream_b,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn body_json(resp: reqwest::Response) -> serde_json::Value {
    let bytes = resp.bytes().await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn successful_forward_updates_proxy_telemetry() {
    let before = proxy_telemetry().stats.snapshot();
    let s = build_stack().await;

    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .header("content-type", "application/json")
        .body(r#"{"model":"provider-a/gpt-x"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    let after = proxy_telemetry().stats.snapshot();
    assert!(after.total >= before.total + 1);
    assert!(after.success >= before.success + 1);

    let logs = proxy_telemetry().logs.get_all();
    assert!(logs
        .iter()
        .any(|entry| entry.level == "INFO" && entry.message.contains("请求: POST")));
    assert!(logs
        .iter()
        .any(|entry| entry.level == "SUCCESS" && entry.message == "上游响应 200"));
}

#[tokio::test]
async fn gateway_auth_failure_updates_proxy_telemetry() {
    let before = proxy_telemetry().stats.snapshot();
    let s = build_stack().await;

    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .body(r#"{"model":"any"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 401);

    let after = proxy_telemetry().stats.snapshot();
    assert!(after.total >= before.total + 1);
    assert!(after.failed >= before.failed + 1);

    let logs = proxy_telemetry().logs.get_all();
    assert!(logs
        .iter()
        .any(|entry| entry.level == "ERROR" && entry.message.contains("代理请求失败")));
}

#[tokio::test]
async fn unauthorized_without_gateway_key() {
    let s = build_stack().await;
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .body(r#"{"model":"any"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn unauthorized_with_wrong_gateway_key() {
    let s = build_stack().await;
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .header("authorization", "Bearer wrong")
        .body(r#"{"model":"any"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn slug_routes_to_provider_a_with_bearer_and_strips_slug() {
    let s = build_stack().await;
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .header("content-type", "application/json")
        .body(r#"{"model":"provider-a/gpt-x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("x-mock-marker").unwrap(),
        "upstream-a",
        "应该路由到 upstream-a"
    );
    let v = body_json(resp).await;
    let auth = v["headers"]["authorization"].as_str().unwrap();
    assert_eq!(
        auth, "Bearer sk-a-bearer",
        "B2: authorization 必须重写为 provider-a 的 key"
    );
    let body = v["body"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["model"], "gpt-x", "应该剥掉 `provider-a/` 前缀");
    // 上游不应收到 user-agent(provider-a 没配 extras)
    assert!(
        v["headers"]
            .get("user-agent")
            .map(|x| x.as_str().unwrap_or(""))
            .unwrap_or("")
            != "TestAgent/1.0",
        "provider-a 没配 extras,不应注入 TestAgent"
    );
    assert_eq!(s.upstream_a.port() > 0 && s.upstream_b.port() > 0, true);
}

#[tokio::test]
async fn slug_routes_to_provider_b_with_x_api_key_and_extras() {
    let s = build_stack().await;
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .header("content-type", "application/json")
        .body(r#"{"model":"provider-b/coding"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("x-mock-marker").unwrap(),
        "upstream-b",
        "应该路由到 upstream-b"
    );
    let v = body_json(resp).await;
    // B2: X-Api-Key 路径
    assert_eq!(
        v["headers"]["x-api-key"].as_str().unwrap(),
        "sk-b-key",
        "B2: X-Api-Key 必须等于 provider-b 的 key"
    );
    // B2: extra header 注入
    assert_eq!(
        v["headers"]["user-agent"].as_str().unwrap(),
        "TestAgent/1.0",
        "B2: provider-b.extraHeaders 必须注入 User-Agent"
    );
    // 入站 gateway Authorization 不应泄漏到上游
    assert!(
        v["headers"]
            .get("authorization")
            .map(|x| x.as_str().unwrap_or(""))
            .unwrap_or("")
            != "Bearer cas_test_gw",
        "incoming gateway Authorization 不应原样转发"
    );
    let body = v["body"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["model"], "coding");
}

#[tokio::test]
async fn fallback_to_default_when_no_slug() {
    let s = build_stack().await;
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .header("content-type", "application/json")
        .body(r#"{"model":"plain-model-name"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("x-mock-marker").unwrap(),
        "upstream-a",
        "无 slug 时应 fallback 到 default(provider-a)"
    );
    let v = body_json(resp).await;
    let body = v["body"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        parsed["model"], "plain-model-name",
        "无 slug 时不重写 model"
    );
}

/// Stage 3.1:adapter 负责路径规范化,`/v1/foo` 走 openai_chat 适配器后
/// 上游收到 `/foo`(因为 baseUrl 已含 `/v1`,这样合起来恰好一份)。
#[tokio::test]
async fn adapter_normalizes_v1_prefix_and_keeps_query() {
    let s = build_stack().await;
    let resp = client()
        .get(format!("http://{}/v1/models?deep=1&order=asc", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v = body_json(resp).await;
    assert_eq!(
        v["path"].as_str().unwrap(),
        "/models?deep=1&order=asc",
        "openai_chat adapter 应把入站 /v1/foo 规范化为 /foo,query 保留"
    );
    assert_eq!(v["method"].as_str().unwrap(), "GET");
}

/// 入站不带 /v1 前缀的路径不应被改写。
#[tokio::test]
async fn adapter_passes_through_paths_without_v1_prefix() {
    let s = build_stack().await;
    let resp = client()
        .get(format!("http://{}/models", s.proxy))
        .header("authorization", "Bearer cas_test_gw")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v = body_json(resp).await;
    assert_eq!(v["path"].as_str().unwrap(), "/models");
}
