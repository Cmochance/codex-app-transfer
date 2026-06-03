//! 网络集成测试: 真实打到 Cloudflare 强保域, 验证 `wreq` + `Emulation::Chrome120` 真的能过。
//!
//! 运行: `cargo test -p codex-app-transfer-http --test cf_bypass -- --include-ignored --nocapture`
//! 默认 `#[ignore]` 避免 CI 无网络环境挂掉; 本地手动跑拿真实数据。

use codex_app_transfer_http::{should_impersonate, ImpersonatingClient};

/// 拉 chatgpt.com 首页 — 之前直接 403, 我们期望拿非 403 响应
#[tokio::test]
#[ignore = "需要网络 + 出向 chatgpt.com"]
async fn chatgpt_home_returns_non_cf_challenge() {
    let client = ImpersonatingClient::chrome_120();
    let resp = client
        .get("https://chatgpt.com/")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    let bytes = resp.bytes().await.expect("body");
    let body_text = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
    eprintln!("status={} body[0..200]={:?}", status, body_text);
    // 关键断言: 不是 403 (CF challenge)
    assert_ne!(
        status.as_u16(),
        403,
        "expected non-403 from chatgpt.com, got 403 (CF JS challenge) — impersonation broken"
    );
    // 进一步: body 不应是 cf-challenge HTML
    let lower = body_text.to_ascii_lowercase();
    assert!(
        !lower.contains("cf-chl"),
        "body still looks like Cloudflare challenge page"
    );
}

/// 拉 help.openai.com 的 codex 集合页 — 之前 403, 期望 200
#[tokio::test]
#[ignore = "需要网络 + 出向 help.openai.com"]
async fn help_openai_codex_collection_returns_200() {
    let client = ImpersonatingClient::chrome_120();
    let resp = client
        .get("https://help.openai.com/en/collections/14937394-codex")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    eprintln!("status={}", status);
    assert_eq!(status.as_u16(), 200, "expected 200, got {}", status);
}

/// 路由函数 sanity check
#[test]
fn router_maps_cf_hosts_to_impersonate() {
    assert!(should_impersonate("chatgpt.com"));
    assert!(should_impersonate("help.openai.com"));
    assert!(should_impersonate("api.openai.com"));
    assert!(!should_impersonate("github.com"));
    assert!(!should_impersonate("status.openai.com"));
    assert!(!should_impersonate("localhost"));
}
