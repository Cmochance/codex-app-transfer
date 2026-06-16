//! z.ai / bigmodel 账号登录**真登录 e2e**(交互式,默认 `#[ignore]`)。
//!
//! 这两条不是常规单测:它们会**打开浏览器**让你真实登录 GLM 账号,然后跑完整
//! [`run_zai_login`](codex_app_transfer_gemini_oauth::run_zai_login) ——
//! loopback 回调 → JSON token 交换 → (z.ai)业务 token → 换组织 API key → 落盘。
//! 是 Stage 1 收敛前的「真登录 e2e」闸(见 memory `feedback_e2e_test_before_convergence`)。
//!
//! ## 怎么跑
//! ```bash
//! # z.ai(国际账号)
//! cargo test -p codex-app-transfer-gemini-oauth --test zai_login_e2e \
//!   e2e_login_zai -- --ignored --nocapture
//! # bigmodel(智谱国内账号)
//! cargo test -p codex-app-transfer-gemini-oauth --test zai_login_e2e \
//!   e2e_login_bigmodel -- --ignored --nocapture
//! ```
//! 浏览器弹出后用对应账号登录授权;终端打印**打码后**的组织 key + 落盘路径。
//!
//! ## 安全
//! - 凭证写到**临时目录**(`CODEX_APP_TRANSFER_HOME` 指向 tempdir),**不碰**真实
//!   `~/.codex-app-transfer/`,跑完临时目录随进程退出清掉。
//! - 终端只打印**打码**的 key(前缀 + 长度),完整值不外泄、不入库。

use std::sync::Arc;
use std::time::Duration;

use codex_app_transfer_gemini_oauth::{run_zai_login, OauthFlowConfig, ZaiProvider};

/// 打码:只露前 6 字符 + 总长,完整 secret 不打印。
fn mask(s: &str) -> String {
    let n = s.chars().count();
    let head: String = s.chars().take(6).collect();
    format!("{head}…(len={n})")
}

async fn drive_login(provider: ZaiProvider) {
    // 隔离持久化目录到 tempdir,绝不碰真实 ~/.codex-app-transfer
    let tmp = tempfile::TempDir::new().expect("建临时 HOME 失败");
    std::env::set_var("CODEX_APP_TRANSFER_HOME", tmp.path());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("建 http client 失败");

    // 打开浏览器 + 终端也打印 URL(open 失败可手动粘贴)
    let flow_config = OauthFlowConfig {
        callback_timeout: Duration::from_secs(300),
        auto_open_browser: true,
        on_auth_url: Some(Arc::new(|url: &str| {
            eprintln!("\n👉 若浏览器没自动弹出,手动打开此 URL 登录:\n{url}\n");
        })),
    };

    eprintln!("\n=== z.ai e2e: {} 登录开始,浏览器即将弹出 ===", provider.wire_id());
    let result = run_zai_login(&http, provider, &flow_config, None).await;

    match result {
        Ok(cred) => {
            eprintln!("\n✅ {} 登录成功", provider.wire_id());
            eprintln!("  email           = {}", cred.email.as_deref().unwrap_or("<none>"));
            eprintln!("  org_api_key     = {}", mask(&cred.org_api_key));
            eprintln!("  zcode_jwt       = {}", mask(&cred.zcode_jwt));
            eprintln!("  落盘路径(临时)  = {}", tmp.path().join(".codex-app-transfer").join(provider.token_filename()).display());
            assert!(!cred.org_api_key.is_empty(), "组织 key 不能为空");
            // 组织 key 形如 <apiKey>.<secretKey>(z.ai 必有点号;bigmodel 可只 apiKey)
            if provider == ZaiProvider::Zai {
                assert!(cred.org_api_key.contains('.'), "z.ai 组织 key 应为 <apiKey>.<secretKey>");
            }
        }
        Err(e) => panic!("❌ {} 登录失败: {e}", provider.wire_id()),
    }
}

#[tokio::test]
#[ignore = "交互式真登录,需浏览器手动授权;手动跑 -- --ignored --nocapture"]
async fn e2e_login_zai() {
    drive_login(ZaiProvider::Zai).await;
}

#[tokio::test]
#[ignore = "交互式真登录,需浏览器手动授权;手动跑 -- --ignored --nocapture"]
async fn e2e_login_bigmodel() {
    drive_login(ZaiProvider::BigModel).await;
}
