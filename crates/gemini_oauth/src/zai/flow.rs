//! z.ai / bigmodel OAuth code-grant flow(loopback 回调 + JSON 信封 token 交换)。
//!
//! 跟 antigravity `flow.rs` **并行**:loopback callback server + CSRF state +
//! cancel-aware select 的骨架相同,但 ZCode wire 跟 Google 差异大:
//! - authorize 两套样式(z.ai `redirect_uri`+`response_type`+`client_id`;
//!   bigmodel `redirect`+`appId`)—— [`build_zai_authorize_url`]
//! - **动态 loopback 端口**(`127.0.0.1:0`):ZCode 注册的是 `zcode://` deeplink,
//!   我们实测 localhost 任意端口都被 authorize 回跳 + token 交换接受(RFC 8252
//!   loopback),用动态端口最稳、不跟 antigravity 固定 51121 撞
//! - token 交换是 **JSON body**(非 form),`{provider, code, redirect_uri, state}`,
//!   **无** `grant_type`/`client_secret`/PKCE;响应是 `{code,msg,data}` 业务信封
//!   (`code != 0` 即业务错)

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Query, response::Html, routing::get, Router};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::super::flow::{FlowError, OauthFlowConfig};
use super::constants::{ZaiProvider, ZaiProviderConfig};
use super::ZaiError;

/// token 交换成功后从 `{code,msg,data}` 信封里抽出的关键产物。
#[derive(Clone)]
pub struct ZaiTokenExchange {
    /// `data.token` —— ZCode 业务 JWT。
    pub zcode_jwt: String,
    /// `data.<provider>.access_token` —— provider 侧 access_token。
    pub provider_access_token: Option<String>,
    /// `data.expires_in`(秒,可空)。
    pub expires_in: Option<i64>,
    /// `data.user.email`(best-effort,UI 展示用)。
    pub email: Option<String>,
}

/// 手写 `Debug` 脱敏 secret(`zcode_jwt`/`provider_access_token`),防误打日志。
impl std::fmt::Debug for ZaiTokenExchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZaiTokenExchange")
            .field("zcode_jwt", &"<redacted>")
            .field(
                "provider_access_token",
                &self.provider_access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field("email", &self.email)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    /// bigmodel 回跳用 `authCode`(z.ai 用 `code`)—— 两个都接。
    #[serde(default, rename = "authCode")]
    auth_code: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

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
    Malformed,
}

/// token 交换响应信封 `{code, msg, data}`。
#[derive(Debug, Deserialize)]
struct TokenEnvelope {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<TokenData>,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    zai: Option<ProviderTok>,
    #[serde(default)]
    bigmodel: Option<ProviderTok>,
    #[serde(default)]
    user: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ProviderTok {
    #[serde(default)]
    access_token: Option<String>,
    /// ZCode 后端有 camelCase 变体(`accessToken`)—— 兜一层(snake_case 优先)。
    #[serde(default, rename = "accessToken")]
    access_token_camel: Option<String>,
}

/// 跑完整 z.ai / bigmodel OAuth code grant(loopback 回调 → code → JSON token 交换)。
/// 返回 token 交换产物(zcode_jwt + provider access_token),换组织 key 由
/// [`super::coding_plan`] 接力。
pub async fn run_zai_oauth_flow_with_cancel(
    http: &reqwest::Client,
    config: &ZaiProviderConfig,
    flow_config: &OauthFlowConfig,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<ZaiTokenExchange, ZaiError> {
    // 1. 动态 loopback 端口(RFC 8252;localhost 任意端口都被 ZCode 后端接受)
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(FlowError::Bind)?;
    let port = listener.local_addr().map_err(FlowError::Bind)?.port();
    let redirect_uri = format!("http://localhost:{port}/oauth-callback");
    tracing::info!(
        provider = config.provider.wire_id(),
        port,
        "z.ai OAuth loopback server bound"
    );

    // 2. CSRF state + authorize URL(按 provider 选样式)
    let state = random_state_token()?;
    let auth_url = build_zai_authorize_url(config, &redirect_uri, &state);

    // 3. 起 loopback server,callback 经 oneshot 回传
    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let app = Router::new().route(
        "/oauth-callback",
        get({
            let tx = Arc::clone(&tx);
            move |Query(q): Query<CallbackQuery>| async move {
                // z.ai=code,bigmodel=authCode,取先到的
                let code = q.code.or(q.auth_code);
                let result = match (code, q.error, q.state) {
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
    let (server_err_tx, mut server_err_rx) = oneshot::channel::<std::io::Error>();
    let server_handle = tokio::spawn(async move {
        match axum::serve(listener, app).await {
            Ok(()) => {
                tracing::warn!("axum::serve 返 Ok 异常 — listener 已关闭,后续 callback 无法到达")
            }
            Err(e) => {
                let _ = server_err_tx.send(e);
            }
        }
    });

    // 4. 先把 URL 回调给 UI(open 失败也能手动粘贴)
    if let Some(callback) = &flow_config.on_auth_url {
        callback(&auth_url);
    }
    // 5. 尝试 open browser
    if flow_config.auto_open_browser {
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::warn!(error = %e, "z.ai webbrowser::open 失败,等用户手动粘贴 URL");
        }
    }

    // 6. 等 callback / timeout / server 崩 / cancel
    let cancel_fut = async {
        match cancel.as_mut() {
            Some(rx) => {
                if *rx.borrow() {
                    return;
                }
                loop {
                    if rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    if *rx.borrow() {
                        return;
                    }
                }
            }
            None => std::future::pending::<()>().await,
        }
    };
    let callback = tokio::select! {
        result = rx => result.map_err(|_| FlowError::Timeout(flow_config.callback_timeout))?,
        _ = tokio::time::sleep(flow_config.callback_timeout) => {
            server_handle.abort();
            return Err(FlowError::Timeout(flow_config.callback_timeout).into());
        }
        Ok(server_err) = &mut server_err_rx => {
            tracing::error!(error = %server_err, "z.ai loopback HTTP server crashed mid-flow");
            return Err(FlowError::Bind(server_err).into());
        }
        _ = cancel_fut => {
            tracing::info!("z.ai OAuth flow cancelled by caller; aborting");
            server_handle.abort();
            return Err(FlowError::Cancelled.into());
        }
    };
    server_handle.abort();

    // 7. 校验 state + 取 code
    let code = match callback {
        CallbackResult::Code {
            code,
            state: returned_state,
        } => {
            if returned_state != state {
                tracing::error!("z.ai OAuth state mismatch");
                return Err(FlowError::StateMismatch.into());
            }
            code
        }
        CallbackResult::Denied { error, description } => {
            return Err(FlowError::Denied { error, description }.into());
        }
        CallbackResult::Malformed => {
            return Err(FlowError::Denied {
                error: "missing_code_and_state".into(),
                description: Some("ZCode callback 既无 code/authCode 也无 error".into()),
            }
            .into());
        }
    };

    // 8. JSON 换 token
    exchange_code_for_token(http, config, &code, &redirect_uri, &state).await
}

/// 构造 authorize URL —— 两套样式(ZCode 两个 adapter 的 `buildAuthorizeUrl`)。
pub fn build_zai_authorize_url(
    config: &ZaiProviderConfig,
    redirect_uri: &str,
    state: &str,
) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    match config.provider {
        ZaiProvider::Zai => {
            ser.append_pair("redirect_uri", redirect_uri)
                .append_pair("response_type", "code")
                .append_pair("client_id", config.app_id)
                .append_pair("state", state);
        }
        ZaiProvider::BigModel => {
            ser.append_pair("redirect", redirect_uri)
                .append_pair("appId", config.app_id)
                .append_pair("state", state);
        }
    }
    format!("{}?{}", config.authorize_url, ser.finish())
}

/// POST `token_url`(JSON)换 token,解 `{code,msg,data}` 信封。
async fn exchange_code_for_token(
    http: &reqwest::Client,
    config: &ZaiProviderConfig,
    code: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<ZaiTokenExchange, ZaiError> {
    let body = serde_json::json!({
        "provider": config.provider.wire_id(),
        "code": code,
        "redirect_uri": redirect_uri,
        "state": state,
    });
    let resp = http.post(config.token_url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(ZaiError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    parse_token_envelope(config.provider, &text)
}

/// 把 token 交换响应文本解成 [`ZaiTokenExchange`]。`code != 0` 当业务错;
/// `data.token` 必须存在。抽成纯函数便于单测(不依赖网络)。
pub(crate) fn parse_token_envelope(
    provider: ZaiProvider,
    text: &str,
) -> Result<ZaiTokenExchange, ZaiError> {
    let env: TokenEnvelope =
        serde_json::from_str(text).map_err(|e| ZaiError::Parse(e.to_string()))?;
    // ZCode: code !== undefined && code !== 0 → 业务错
    if let Some(code) = env.code {
        if code != 0 {
            return Err(ZaiError::Business {
                code,
                msg: env.msg.unwrap_or_default(),
            });
        }
    }
    let data = env.data.ok_or(ZaiError::MissingField("data"))?;
    let zcode_jwt = data
        .token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or(ZaiError::MissingField("data.token"))?;
    let provider_tok = match provider {
        ZaiProvider::Zai => data.zai,
        ZaiProvider::BigModel => data.bigmodel,
    };
    let provider_access_token = provider_tok
        .and_then(|p| p.access_token.or(p.access_token_camel))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let email = data
        .user
        .as_ref()
        .and_then(|u| u.get("email"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string());
    Ok(ZaiTokenExchange {
        zcode_jwt,
        provider_access_token,
        expires_in: data.expires_in,
        email,
    })
}

fn random_state_token() -> Result<String, FlowError> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| FlowError::Rng(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 当前 UNIX ms-epoch(凭证 `obtained_at_ms` 用)。
pub(crate) fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const CALLBACK_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Codex App Transfer — GLM Authorized</title>
<style>
body { font-family: -apple-system, system-ui, sans-serif; max-width: 600px; margin: 60px auto; padding: 0 20px; color: #333; }
h1 { color: #4caf50; }
p { line-height: 1.6; }
</style>
</head>
<body>
<h1>✓ GLM authorization complete</h1>
<p>You can close this window and return to <strong>Codex App Transfer</strong>.</p>
<p>授权完成,请关闭此窗口返回 <strong>Codex App Transfer</strong>。</p>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zai_authorize_url_uses_oauth_style_params() {
        let url = build_zai_authorize_url(
            &super::super::constants::ZAI_CONFIG,
            "http://localhost:5/oauth-callback",
            "st8",
        );
        assert!(url.starts_with("https://chat.z.ai/api/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client_P8X5CMWmlaRO9gyO-KSqtg"));
        assert!(url.contains("redirect_uri=http"));
        assert!(url.contains("state=st8"));
        // z.ai 样式不带 bigmodel 的 appId/redirect 参数名
        assert!(!url.contains("appId="));
    }

    #[test]
    fn bigmodel_authorize_url_uses_login_page_style_params() {
        let url = build_zai_authorize_url(
            &super::super::constants::BIGMODEL_CONFIG,
            "http://localhost:7/oauth-callback",
            "st9",
        );
        assert!(url.starts_with("https://bigmodel.cn/login?"));
        assert!(url.contains("appId=zcode"));
        assert!(url.contains("redirect=http"));
        assert!(url.contains("state=st9"));
        // bigmodel 不用 OAuth 标准 response_type/client_id
        assert!(!url.contains("response_type="));
        assert!(!url.contains("client_id="));
    }

    #[test]
    fn parse_envelope_rejects_nonzero_business_code() {
        let body = r#"{"code":40001,"msg":"invalid code","data":null}"#;
        let err = parse_token_envelope(ZaiProvider::Zai, body).unwrap_err();
        match err {
            ZaiError::Business { code, msg } => {
                assert_eq!(code, 40001);
                assert_eq!(msg, "invalid code");
            }
            other => panic!("应为 Business 错,实际 {other:?}"),
        }
    }

    #[test]
    fn parse_envelope_extracts_zai_token_and_email() {
        let body = r#"{"code":0,"msg":"ok","data":{
            "token":" ey.zcode.jwt ",
            "expires_in":3600,
            "zai":{"access_token":"zai-at-123"},
            "user":{"email":"u@z.ai"}
        }}"#;
        let out = parse_token_envelope(ZaiProvider::Zai, body).unwrap();
        assert_eq!(out.zcode_jwt, "ey.zcode.jwt", "token 应 trim");
        assert_eq!(out.provider_access_token.as_deref(), Some("zai-at-123"));
        assert_eq!(out.expires_in, Some(3600));
        assert_eq!(out.email.as_deref(), Some("u@z.ai"));
    }

    #[test]
    fn parse_envelope_extracts_bigmodel_provider_token() {
        let body = r#"{"code":0,"data":{"token":"jwt","bigmodel":{"access_token":"bm-at","refresh_token":"rt"}}}"#;
        let out = parse_token_envelope(ZaiProvider::BigModel, body).unwrap();
        assert_eq!(out.zcode_jwt, "jwt");
        assert_eq!(out.provider_access_token.as_deref(), Some("bm-at"));
        // bigmodel 路径下不会误取 zai 字段
    }

    #[test]
    fn parse_envelope_missing_token_errors() {
        let body = r#"{"code":0,"data":{"zai":{"access_token":"x"}}}"#;
        let err = parse_token_envelope(ZaiProvider::Zai, body).unwrap_err();
        assert!(
            matches!(err, ZaiError::MissingField("data.token")),
            "实际 {err:?}"
        );
    }

    #[test]
    fn parse_envelope_treats_absent_code_as_success() {
        // ZCode 只在 code 既存在又非 0 时报错;缺省 code 视为成功
        let body = r#"{"data":{"token":"jwt"}}"#;
        let out = parse_token_envelope(ZaiProvider::Zai, body).unwrap();
        assert_eq!(out.zcode_jwt, "jwt");
    }
}
