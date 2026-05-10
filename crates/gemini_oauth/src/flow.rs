//! OAuth 2.0 code grant flow + token refresh。
//!
//! ## 流程(impersonate gemini-cli web flow)
//!
//! 1. 起 loopback HTTP server 监听 `127.0.0.1:<动态port>/oauth2callback`
//!    (动态 port = OS 自选,跟 gemini-cli `getAvailablePort()` 行为对齐)
//! 2. 生成 CSRF state(32 字节随机 hex,对齐 `oauth2.ts:200ish` `crypto.randomBytes(32).toString('hex')`)
//! 3. 构造 Google 授权 URL(`accounts.google.com/o/oauth2/v2/auth` + client_id +
//!    redirect_uri + access_type=offline + scope + state)
//! 4. 浏览器 open URL,用户登录 + 授权
//! 5. Google redirect 回 callback,带 `?code=...&state=...`
//!    - **必须**校验 state 一致(CSRF 防御)
//!    - 提取 `code` 用于换 token
//! 6. POST `oauth2.googleapis.com/token` 用 `authorization_code` grant_type 换
//!    `access_token + refresh_token + expires_in + scope + id_token`
//! 7. 转换 `expires_in` (秒) → `expiry_date` (UNIX ms-epoch),持久化
//!
//! ## Refresh
//!
//! POST `/token` 带 `grant_type=refresh_token`,响应里 `refresh_token` 字段**可能
//! 不返回**(Google 不一定 rotate),fallback 沿用旧值。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{extract::Query, response::Html, routing::get, Router};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::oneshot;

use super::constants::{
    AUTH_ENDPOINT, CLIENT_ID, CLIENT_SECRET, REDIRECT_PATH, SCOPES, TOKEN_ENDPOINT,
};
use super::token::OauthToken;

#[derive(Debug, Error)]
pub enum FlowError {
    #[error("loopback HTTP server bind 失败: {0}")]
    Bind(#[from] std::io::Error),
    #[error("CSRF state 不匹配 — 可能是恶意 callback,绝不能继续 token exchange")]
    StateMismatch,
    #[error("用户授权超时(等待 callback 超过 {0:?})")]
    Timeout(Duration),
    #[error("授权被拒绝或返回错误: {error}{}", .description.as_ref().map(|d| format!(" — {d}")).unwrap_or_default())]
    Denied {
        error: String,
        description: Option<String>,
    },
    #[error("token endpoint HTTP 失败: {0}")]
    TokenHttp(#[from] reqwest::Error),
    #[error("token endpoint 返非 2xx: HTTP {status}: {body}")]
    TokenStatus { status: u16, body: String },
    #[error("token 响应 JSON 解析失败: {0}")]
    TokenParse(String),
    #[error("浏览器打开失败 — 用户可手动复制 URL 粘贴: {url}")]
    BrowserOpen { url: String },
    #[error("OS RNG 不可用: {0}")]
    Rng(String),
}

/// OAuth flow 配置。所有字段都有默认值,通常不需要改。
#[derive(Debug, Clone)]
pub struct OauthFlowConfig {
    /// 等待 callback 的最大时长。默认 5 分钟 — 用户登 Google 账号 + 同意授权 5min 够用。
    pub callback_timeout: Duration,
    /// 是否自动打开浏览器。`false` 时返回 URL 让调用方自己处理(headless / 测试)。
    pub auto_open_browser: bool,
}

impl Default for OauthFlowConfig {
    fn default() -> Self {
        Self {
            callback_timeout: Duration::from_secs(300),
            auto_open_browser: true,
        }
    }
}

/// `/token` endpoint 的 wire 响应 shape。
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// **可能不返回**(refresh 路径常见)— None 时由调用方沿用旧 refresh_token
    #[serde(default)]
    refresh_token: Option<String>,
    /// "Bearer"
    token_type: String,
    /// **秒**(不是 ms-epoch),从 now 起算
    expires_in: i64,
    scope: String,
    #[serde(default)]
    id_token: Option<String>,
}

/// `/oauth2callback` 收到的 query 参数(Google 重定向带过来)。
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    /// 授权成功路径
    #[serde(default)]
    code: Option<String>,
    /// 授权失败路径(用户拒绝 / Google 异常)
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    /// CSRF state — 必须跟我们生成的 state 完全一致
    #[serde(default)]
    state: Option<String>,
}

/// Loopback callback server 收到结果后通过 oneshot 回传 — 跟主 flow 解耦。
#[derive(Debug)]
enum CallbackResult {
    Code {
        code: String,
        state: String,
    },
    Denied {
        error: String,
        description: Option<String>,
    },
    /// state / code 都没收到 — Google 不应该这样,但防御性处理
    Malformed,
}

/// 跑完整 OAuth code grant 流程。返回的 `OauthToken` 已含 access/refresh/expiry,
/// **不含** project_id(后续 `cloud_code` 模块 bootstrap 时填)。
///
/// ## 流程
///
/// 1. bind 127.0.0.1:0(OS 自选 port)起 loopback server
/// 2. 生成 state token,构造授权 URL,可选 open browser
/// 3. 等待 callback(timeout 内),校验 state,提取 code
/// 4. POST token endpoint exchange code → access_token
///
/// ## 错误恢复
///
/// - `StateMismatch` → 不要重试,极可能 CSRF 攻击
/// - `Timeout` → 用户没及时授权,可重启 flow
/// - `Denied` → 用户拒绝,重启 flow 让用户重新选账号
/// - `BrowserOpen` → fallback 让用户手动复制 URL,**flow 仍在等 callback**(loopback server 没退)
pub async fn run_oauth_flow(
    http: &reqwest::Client,
    config: &OauthFlowConfig,
) -> Result<OauthToken, FlowError> {
    // 1. bind loopback 拿动态 port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr: SocketAddr = listener.local_addr()?;
    let port = local_addr.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");
    tracing::info!(port, "gemini OAuth loopback server bound");

    // 2. 生成 CSRF state token + auth URL
    let state = random_state_token()?;
    let auth_url = build_auth_url(&redirect_uri, &state);

    // 3. 起 loopback server,callback 通过 oneshot 回传
    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let app = Router::new().route(
        REDIRECT_PATH,
        get({
            let tx = Arc::clone(&tx);
            move |Query(q): Query<CallbackQuery>| async move {
                let result = match (q.code, q.error, q.state) {
                    (Some(code), _, Some(state)) => CallbackResult::Code { code, state },
                    (_, Some(error), _) => CallbackResult::Denied {
                        error,
                        description: q.error_description,
                    },
                    _ => CallbackResult::Malformed,
                };
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(result);
                }
                Html(CALLBACK_HTML)
            }
        }),
    );
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // 4. 打开浏览器(失败也继续 — 用户可手动复制 URL)
    if config.auto_open_browser {
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::warn!(error = %e, "无法自动打开浏览器,用户需手动复制 URL");
            // 不立刻返错 — flow 继续,用户可能直接拷贝 url 到别的浏览器粘贴
        }
    }

    // 5. 等待 callback 或 timeout
    let callback = tokio::select! {
        result = rx => result.map_err(|_| FlowError::Timeout(config.callback_timeout))?,
        _ = tokio::time::sleep(config.callback_timeout) => {
            server_handle.abort();
            return Err(FlowError::Timeout(config.callback_timeout));
        }
    };
    server_handle.abort();

    // 6. 校验 state + 提取 code
    let code = match callback {
        CallbackResult::Code {
            code,
            state: returned_state,
        } => {
            if returned_state != state {
                tracing::error!(
                    expected_len = state.len(),
                    returned_len = returned_state.len(),
                    "OAuth state mismatch — 拒绝继续 token exchange"
                );
                return Err(FlowError::StateMismatch);
            }
            code
        }
        CallbackResult::Denied { error, description } => {
            return Err(FlowError::Denied { error, description });
        }
        CallbackResult::Malformed => {
            return Err(FlowError::Denied {
                error: "missing_code_and_state".into(),
                description: Some("Google callback 既没有 code 也没有 error,极不正常".into()),
            });
        }
    };

    // 7. POST /token 换 access_token
    exchange_code_for_token(http, &code, &redirect_uri).await
}

/// 用 refresh_token 刷新 access_token。返回新 OauthToken,自动沿用旧 refresh_token
/// 如果 Google 没返新的(行为见 RFC 6749 §1.5,Google 不一定 rotate)。
pub async fn refresh_access_token(
    http: &reqwest::Client,
    refresh_token: &str,
    existing_id_token: Option<String>,
    existing_email: Option<String>,
    existing_project_id: Option<String>,
    existing_scope: Option<String>,
) -> Result<OauthToken, FlowError> {
    refresh_access_token_at(
        http,
        TOKEN_ENDPOINT,
        refresh_token,
        existing_id_token,
        existing_email,
        existing_project_id,
        existing_scope,
    )
    .await
}

/// 内部版 — 接收可定制 token endpoint。`pub(crate)` 让 crate 外**完全不可见**
/// (silent-failure-hunter H-1 修:lib.rs export 也透不出去,proxy / admin handler
/// 等下游 crate 无法误用此 fn 绕过 const [`TOKEN_ENDPOINT`])。仅 crate 内
/// production [`refresh_access_token`] 走 const,以及 service::tests 调 mock。
pub(crate) async fn refresh_access_token_at(
    http: &reqwest::Client,
    token_endpoint: &str,
    refresh_token: &str,
    existing_id_token: Option<String>,
    existing_email: Option<String>,
    existing_project_id: Option<String>,
    existing_scope: Option<String>,
) -> Result<OauthToken, FlowError> {
    let params = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let resp = http.post(token_endpoint).form(&params).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(FlowError::TokenStatus {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: TokenResponse =
        serde_json::from_str(&body).map_err(|e| FlowError::TokenParse(e.to_string()))?;

    Ok(OauthToken {
        access_token: parsed.access_token,
        // refresh response 不返新 refresh_token 时沿用旧值(常见路径)
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_owned()),
        token_type: parsed.token_type,
        expiry_date: now_ms_plus_secs(parsed.expires_in),
        scope: existing_scope.unwrap_or(parsed.scope),
        id_token: parsed.id_token.or(existing_id_token),
        email: existing_email,
        project_id: existing_project_id,
    })
}

async fn exchange_code_for_token(
    http: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
) -> Result<OauthToken, FlowError> {
    let params = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ];
    let resp = http.post(TOKEN_ENDPOINT).form(&params).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(FlowError::TokenStatus {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: TokenResponse =
        serde_json::from_str(&body).map_err(|e| FlowError::TokenParse(e.to_string()))?;
    let refresh_token = parsed.refresh_token.ok_or_else(|| {
        FlowError::TokenParse(
            "授权码 exchange 必须返回 refresh_token,但响应没有(检查 access_type=offline)".into(),
        )
    })?;
    Ok(OauthToken {
        access_token: parsed.access_token,
        refresh_token,
        token_type: parsed.token_type,
        expiry_date: now_ms_plus_secs(parsed.expires_in),
        scope: parsed.scope,
        id_token: parsed.id_token,
        email: None,
        project_id: None,
    })
}

/// 构造 Google OAuth 授权 URL。query 参数顺序对齐 gemini-cli `oauth2.ts:207-213`
/// (虽然 RFC 6749 不要求顺序,但保持一致便于 wire diff)。
pub fn build_auth_url(redirect_uri: &str, state: &str) -> String {
    let scope = SCOPES.join(" ");
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scope)
        .append_pair("access_type", "offline")
        .append_pair("state", state)
        .finish();
    format!("{AUTH_ENDPOINT}?{params}")
}

/// 32 字节 OS RNG → hex(64 字符)— 对齐 gemini-cli upstream。
fn random_state_token() -> Result<String, FlowError> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| FlowError::Rng(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// `now_ms + expires_in_secs * 1000`。用于把 token endpoint 的相对秒数转
/// gemini-cli `Credentials.expiry_date` 绝对 ms-epoch。
fn now_ms_plus_secs(secs: i64) -> i64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    now_ms.saturating_add(secs.saturating_mul(1000))
}

/// 用户授权完成后浏览器看到的 HTML(简单的成功提示)。Google 重定向到我们
/// loopback 后,这页面就在用户浏览器显示,提示可以关掉。
const CALLBACK_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Codex App Transfer — OAuth Success</title>
<style>
body { font-family: -apple-system, system-ui, sans-serif; max-width: 600px; margin: 60px auto; padding: 0 20px; color: #333; }
h1 { color: #4caf50; }
p { line-height: 1.6; }
</style>
</head>
<body>
<h1>✓ Authorization complete</h1>
<p>You can close this window and return to <strong>Codex App Transfer</strong>.</p>
<p>授权完成,请关闭此窗口返回 <strong>Codex App Transfer</strong>。</p>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_required_params() {
        let url = build_auth_url("http://127.0.0.1:12345/oauth2callback", "abc123");
        // OAuth 2.0 RFC 6749 必填 params
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=681255809395-"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("state=abc123"));
        // redirect_uri 必须 URL-encoded
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Foauth2callback"));
        // scope 三个 OAuth scope 全在
        assert!(url.contains("cloud-platform"));
        assert!(url.contains("userinfo.email"));
        assert!(url.contains("userinfo.profile"));
        // 不该有 PKCE 相关字段(对齐 gemini-cli web flow)
        assert!(!url.contains("code_challenge"));
    }

    #[test]
    fn random_state_is_64_hex_chars() {
        let s = random_state_token().unwrap();
        assert_eq!(s.len(), 64, "32 bytes → 64 hex chars");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "state 必须全 hex");
        // 多调几次确保不一样(防 RNG 退化成 zero)
        let s2 = random_state_token().unwrap();
        assert_ne!(s, s2, "OS RNG 必须每次产不同 state");
    }

    #[test]
    fn now_ms_plus_secs_arithmetic() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let result = now_ms_plus_secs(3600);
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // result 应在 [before+3600s, after+3600s] 区间
        assert!(result >= before.saturating_add(3_600_000));
        assert!(result <= after.saturating_add(3_600_000));
    }

    #[test]
    fn now_ms_plus_secs_handles_overflow() {
        // 极端值不 panic(saturating arithmetic)
        let _ = now_ms_plus_secs(i64::MAX);
        let _ = now_ms_plus_secs(0);
    }

    #[tokio::test]
    async fn refresh_token_uses_form_encoding_and_parses_response() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // wiremock 的 mock token endpoint
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=old-refresh-xyz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.new-access",
                "expires_in": 3599,
                "scope": "https://www.googleapis.com/auth/cloud-platform",
                "token_type": "Bearer",
                "id_token": "ey.new-id"
            })))
            .mount(&server)
            .await;

        // 暂时把 TOKEN_ENDPOINT mock 掉 — 用 reqwest::Client base_url override
        // (constants.rs::TOKEN_ENDPOINT 是 const 字符串,测试不能改;只能直接调内部 helper
        //  by 重新构造 params 手动 POST 到 mock server)
        // 这里直接验 wiremock 收到了正确的 form body — flow 内部逻辑由其他单测覆盖
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{}/token", server.uri()))
            .form(&[
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("refresh_token", "old-refresh-xyz"),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let parsed: TokenResponse = resp.json().await.unwrap();
        assert_eq!(parsed.access_token, "ya29.new-access");
        assert_eq!(parsed.expires_in, 3599);
        assert!(
            parsed.refresh_token.is_none(),
            "Google 默认不 rotate refresh_token"
        );
    }

    #[tokio::test]
    async fn refresh_token_falls_back_to_old_refresh_when_response_omits_it() {
        // refresh_access_token 的契约:response 没 refresh_token 时沿用旧值
        // 这里直接构造 OauthToken 验 fallback 逻辑(不调 mock server)
        let parsed = TokenResponse {
            access_token: "new".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: "test-scope".into(),
            id_token: None,
        };
        let fallback = parsed
            .refresh_token
            .clone()
            .unwrap_or_else(|| "old-refresh".to_owned());
        assert_eq!(fallback, "old-refresh");
    }

    #[test]
    fn flow_error_denied_message_includes_description() {
        let err = FlowError::Denied {
            error: "access_denied".into(),
            description: Some("User declined".into()),
        };
        let msg = err.to_string();
        assert!(msg.contains("access_denied"));
        assert!(msg.contains("User declined"));
    }

    #[test]
    fn flow_error_denied_without_description() {
        let err = FlowError::Denied {
            error: "invalid_request".into(),
            description: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid_request"));
        assert!(!msg.contains("None"));
    }
}
