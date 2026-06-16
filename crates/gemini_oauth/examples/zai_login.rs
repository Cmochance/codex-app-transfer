//! z.ai / bigmodel **真实落盘**账号登录入口(开发期 CLI,Stage 3 admin UI 前的临时入口)。
//!
//! 跟 `tests/zai_login_e2e.rs` 的区别:**不隔离到临时目录**——凭证存进真实
//! `~/.codex-app-transfer/{zai,bigmodel}-oauth.json`,登录**一次即永久保存**、Stage 2
//! 打模型直接复用。换出的组织 API key 长期有效(无 refresh),所以这就是「最后一次登录」。
//!
//! ## 跑
//! ```bash
//! # 完整登录(开浏览器授权)
//! cargo run -p codex-app-transfer-gemini-oauth --example zai_login -- zai
//! cargo run -p codex-app-transfer-gemini-oauth --example zai_login -- bigmodel
//!
//! # 续传:浏览器授权已成功但换 key 那步失败过 → 用已存的 pending token 重试,
//! # 不重走浏览器(限 token 有效期内,通常约 1 小时)
//! cargo run -p codex-app-transfer-gemini-oauth --example zai_login -- zai --resume
//! ```
//! 终端只打**打码**的 key(完整值不外泄)。

use std::sync::Arc;
use std::time::Duration;

use codex_app_transfer_gemini_oauth::{
    resume_zai_login, run_zai_login, OauthFlowConfig, ZaiCredential, ZaiCredentialStore, ZaiError,
    ZaiProvider,
};

/// 打码:只露前 6 字符 + 总长,完整 secret 不打印。
fn mask(s: &str) -> String {
    let n = s.chars().count();
    let head: String = s.chars().take(6).collect();
    format!("{head}…(len={n})")
}

fn print_result(provider: ZaiProvider, result: Result<ZaiCredential, ZaiError>) -> ! {
    match result {
        Ok(cred) => {
            let path = ZaiCredentialStore::for_provider(provider)
                .map(|s| s.path().display().to_string())
                .unwrap_or_else(|_| "<无法解析路径>".into());
            eprintln!("\n✅ {} 登录成功,凭证已永久落盘", provider.wire_id());
            eprintln!(
                "  email       = {}",
                cred.email.as_deref().unwrap_or("<none>")
            );
            eprintln!("  org_api_key = {}", mask(&cred.org_api_key));
            eprintln!("  落盘路径    = {path}");
            eprintln!("\n这把组织 key 长期有效(无 refresh),Stage 2 打模型直接复用,无需再登录。");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n❌ {} 登录失败: {e}", provider.wire_id());
            eprintln!(
                "若错误出现在『浏览器授权之后』的换 key 步骤,token 已存 pending,\n\
                 修复后可用 `--resume` 不重走浏览器地重试(限 token 有效期内)。"
            );
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("zai") => ZaiProvider::Zai,
        Some("bigmodel") => ZaiProvider::BigModel,
        other => {
            eprintln!(
                "用法: cargo run --example zai_login -- <zai|bigmodel> [--resume](收到: {other:?})"
            );
            std::process::exit(2);
        }
    };
    let resume = args.any(|a| a == "--resume");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("建 http client 失败");

    if resume {
        eprintln!(
            "\n=== {} 续传:用已存 pending token 换 key(不开浏览器)===",
            provider.wire_id()
        );
        print_result(provider, resume_zai_login(&http, provider).await);
    }

    let flow_config = OauthFlowConfig {
        callback_timeout: Duration::from_secs(300),
        auto_open_browser: true,
        on_auth_url: Some(Arc::new(|url: &str| {
            eprintln!("\n👉 若浏览器没自动弹出,手动打开此 URL 登录:\n{url}\n");
        })),
    };

    eprintln!(
        "\n=== {} 真实登录开始,浏览器即将弹出 ===",
        provider.wire_id()
    );
    print_result(
        provider,
        run_zai_login(&http, provider, &flow_config, None).await,
    );
}
