//! Trae loopback OAuth2 + PKCE flow + ExchangeToken(首次换 token / refresh 续期)。
//!
//! 对齐 Trae 主进程 `OAuthLocalServer`:
//! - loopback `127.0.0.1:0` 随机端口,回调 path `/authorize`(对齐 `hp.AUTHORIZE`)
//! - 回调取 `authCodeInfo`(urlencoded JSON,内含 `AuthCode`),**非**标准 `?code=`
//! - authorize URL 用 PKCE S256 + login_trace_id(Trae 不用 OAuth `state`)
//! - 首次换 token **无签名**;refresh 带设备私钥签名的 `DeviceProof`
//!
//! 复用 gemini parent 的 [`OauthFlowConfig`] / [`FlowError`] loopback 骨架
//! (cancel-aware select + on_auth_url 回调 + auto_open_browser)。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Query, response::Html, routing::get, Router};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::super::flow::{FlowError, OauthFlowConfig};
use super::constants::{
    TraeProviderConfig, AUTH_TYPE, CALLBACK_PATH, EXCHANGE_TOKEN_PATH, LOGIN_CHANNEL, LOGIN_VERSION,
};
use super::crypto::{build_device_proof, DeviceKeyPair, PkcePair};
use super::device::DeviceFingerprint;
use super::TraeError;

/// ExchangeToken 成功后从 `Result` 抽出的关键产物。
#[derive(Clone)]
pub struct TraeTokenResult {
    /// `Result.Token`(JWT)—— x-icube-token / Cloud-IDE-JWT。
    pub token: String,
    /// `Result.RefreshToken`。
    pub refresh_token: String,
    /// access token 过期 UNIX ms(0 = 未知)。
    pub token_expire_at_ms: i64,
    /// refresh token 过期 UNIX ms(0 = 未知)。
    pub refresh_expire_at_ms: i64,
    /// `Result.UserID`。
    pub user_id: Option<String>,
    /// `Result.AIRegion`。
    pub ai_region: Option<String>,
}

impl std::fmt::Debug for TraeTokenResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraeTokenResult")
            .field("token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("token_expire_at_ms", &self.token_expire_at_ms)
            .field("refresh_expire_at_ms", &self.refresh_expire_at_ms)
            .field("user_id", &self.user_id)
            .field("ai_region", &self.ai_region)
            .finish()
    }
}

/// 回调 query —— Trae 用 `authCodeInfo`(JSON);留 `code` 兜底。
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default, rename = "authCodeInfo")]
    auth_code_info: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default, rename = "error_code")]
    error_code: Option<String>,
    #[serde(default, rename = "error_msg")]
    error_msg: Option<String>,
}

/// `authCodeInfo` urldecode 后的 JSON。
#[derive(Debug, Deserialize)]
struct AuthCodeInfo {
    #[serde(rename = "AuthCode")]
    auth_code: Option<String>,
}

#[derive(Debug)]
enum CallbackResult {
    Code(String),
    Denied {
        error: String,
        description: Option<String>,
    },
    Malformed,
}

/// 跑完整 Trae loopback OAuth → 拿 AuthCode → 首次 ExchangeToken(无签名)→
/// [`TraeTokenResult`]。
pub async fn run_trae_oauth_flow_with_cancel(
    http: &reqwest::Client,
    config: &TraeProviderConfig,
    fingerprint: &DeviceFingerprint,
    keypair: &DeviceKeyPair,
    flow_config: &OauthFlowConfig,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TraeTokenResult, TraeError> {
    // 1. 动态 loopback 端口
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(FlowError::Bind)?;
    let port = listener.local_addr().map_err(FlowError::Bind)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    tracing::info!(edition = ?config.edition, port, "Trae OAuth loopback server bound");

    // 2. PKCE + authorize URL
    let pkce = super::crypto::generate_pkce()?;
    let login_trace_id = uuid_v4()?;
    let auth_url = build_authorize_url(config, fingerprint, &redirect_uri, &pkce, &login_trace_id);

    // 3. loopback server
    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let app = Router::new().route(
        CALLBACK_PATH,
        get({
            let tx = Arc::clone(&tx);
            move |Query(q): Query<CallbackQuery>| async move {
                let result = interpret_callback(q);
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
            Ok(()) => tracing::warn!("axum::serve 返 Ok — listener 已关,callback 无法到达"),
            Err(e) => {
                let _ = server_err_tx.send(e);
            }
        }
    });

    // 4. 回调 URL 给 UI(open 失败可手动粘贴)
    if let Some(callback) = &flow_config.on_auth_url {
        callback(&auth_url);
    }
    // 5. open 浏览器
    if flow_config.auto_open_browser {
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::warn!(error = %e, "Trae webbrowser::open 失败,等用户手动粘贴 URL");
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
            tracing::error!(error = %server_err, "Trae loopback HTTP server crashed mid-flow");
            return Err(FlowError::Bind(server_err).into());
        }
        _ = cancel_fut => {
            tracing::info!("Trae OAuth flow cancelled by caller; aborting");
            server_handle.abort();
            return Err(FlowError::Cancelled.into());
        }
    };
    server_handle.abort();

    // 7. 取 AuthCode
    let auth_code = match callback {
        CallbackResult::Code(code) => code,
        CallbackResult::Denied { error, description } => {
            return Err(FlowError::Denied { error, description }.into());
        }
        CallbackResult::Malformed => {
            return Err(FlowError::Denied {
                error: "missing_auth_code".into(),
                description: Some("Trae 回调既无 authCodeInfo.AuthCode 也无 error".into()),
            }
            .into());
        }
    };

    // 8. 首次换 token(无签名)
    exchange_auth_code(
        http,
        config,
        fingerprint,
        keypair,
        &auth_code,
        &pkce.verifier,
    )
    .await
}

/// 解析回调 query → [`CallbackResult`]。抽纯函数便于单测。
fn interpret_callback(q: CallbackQuery) -> CallbackResult {
    if let Some(err) = q.error_code {
        return CallbackResult::Denied {
            error: err,
            description: q.error_msg,
        };
    }
    // 优先 authCodeInfo(JSON 内 AuthCode);兜底直接 code 参数
    if let Some(raw) = q.auth_code_info {
        if let Ok(info) = serde_json::from_str::<AuthCodeInfo>(&raw) {
            if let Some(code) = info.auth_code.filter(|c| !c.is_empty()) {
                return CallbackResult::Code(code);
            }
        }
    }
    if let Some(code) = q.code.filter(|c| !c.is_empty()) {
        return CallbackResult::Code(code);
    }
    CallbackResult::Malformed
}

/// 构造 authorize URL(全部 query 参数对齐 Trae `main.js` URL builder)。
pub fn build_authorize_url(
    config: &TraeProviderConfig,
    fingerprint: &DeviceFingerprint,
    redirect_uri: &str,
    pkce: &PkcePair,
    login_trace_id: &str,
) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    ser.append_pair("login_version", LOGIN_VERSION)
        .append_pair("auth_from", config.auth_from)
        .append_pair("login_channel", LOGIN_CHANNEL)
        .append_pair("plugin_version", config.ide_version)
        .append_pair("auth_type", AUTH_TYPE)
        .append_pair("client_id", config.client_id)
        .append_pair("redirect", "0")
        .append_pair("login_trace_id", login_trace_id)
        .append_pair("auth_callback_url", redirect_uri)
        .append_pair("machine_id", &fingerprint.machine_id)
        .append_pair("device_id", &fingerprint.device_id)
        .append_pair("x_device_id", &fingerprint.device_id)
        .append_pair("x_machine_id", &fingerprint.machine_id)
        // 对齐 main.js:x_device_brand 实际取 deviceModel
        .append_pair("x_device_brand", &fingerprint.device_model)
        .append_pair("x_device_type", &fingerprint.os_info)
        .append_pair("x_os_version", &fingerprint.os_version)
        .append_pair("x_env", "")
        .append_pair("x_app_version", config.ide_version)
        .append_pair("x_app_type", config.app_channel)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    if config.hide_saas_login {
        ser.append_pair("hide_saas_login", "true");
    }
    format!(
        "{}{}?{}",
        config.console_host,
        super::constants::AUTHORIZE_PATH,
        ser.finish()
    )
}

/// 首次换 token:`POST {api_host}/trae/api/v3/oauth/ExchangeToken`
/// body `{ClientID, AuthCode, CodeVerifier, DeviceInfo, IDEVersion}`(**无签名**)。
pub async fn exchange_auth_code(
    http: &reqwest::Client,
    config: &TraeProviderConfig,
    fingerprint: &DeviceFingerprint,
    keypair: &DeviceKeyPair,
    auth_code: &str,
    code_verifier: &str,
) -> Result<TraeTokenResult, TraeError> {
    let device_info = fingerprint.to_device_info(config, &keypair.public_spki_pem);
    let body = serde_json::json!({
        "ClientID": config.client_id,
        "AuthCode": auth_code,
        "CodeVerifier": code_verifier,
        "DeviceInfo": device_info,
        "IDEVersion": config.ide_version,
    });
    post_exchange(http, config, &body).await
}

/// refresh 续期:同端点,body 加 `ClientSecret:""` + 设备私钥签名的 `DeviceProof`。
pub async fn refresh_token(
    http: &reqwest::Client,
    config: &TraeProviderConfig,
    fingerprint: &DeviceFingerprint,
    keypair: &DeviceKeyPair,
    refresh_token: &str,
) -> Result<TraeTokenResult, TraeError> {
    let device_info = fingerprint.to_device_info(config, &keypair.public_spki_pem);
    let proof = build_device_proof(
        &keypair.private_pkcs8_pem,
        "POST",
        EXCHANGE_TOKEN_PATH,
        config.client_id,
        refresh_token,
        unix_now_secs(),
    )?;
    let body = serde_json::json!({
        "ClientID": config.client_id,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": device_info,
        "DeviceProof": proof,
        "IDEVersion": config.ide_version,
    });
    post_exchange(http, config, &body).await
}

/// 共用:POST ExchangeToken + 解 `{Result, ResponseMetadata}` 信封。
async fn post_exchange(
    http: &reqwest::Client,
    config: &TraeProviderConfig,
    body: &serde_json::Value,
) -> Result<TraeTokenResult, TraeError> {
    let url = format!("{}{}", config.api_host, EXCHANGE_TOKEN_PATH);
    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(TraeError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    parse_token_result(&text)
}

/// 解 ExchangeToken 响应 `{Result:{...}, ResponseMetadata:{Error:{Code}}}`。纯函数。
pub(crate) fn parse_token_result(text: &str) -> Result<TraeTokenResult, TraeError> {
    let env: serde_json::Value =
        serde_json::from_str(text).map_err(|e| TraeError::Parse(e.to_string()))?;
    // 业务错:ResponseMetadata.Error.Code 非空
    if let Some(code) = env
        .get("ResponseMetadata")
        .and_then(|m| m.get("Error"))
        .and_then(|e| e.get("Code"))
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        let msg = env
            .get("ResponseMetadata")
            .and_then(|m| m.get("Error"))
            .and_then(|e| e.get("Message"))
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        return Err(TraeError::Business {
            code: code.to_string(),
            msg,
        });
    }
    let result = env.get("Result").ok_or(TraeError::MissingField("Result"))?;
    // 实测 ExchangeToken Result 不带 user_id(只 Token/RefreshToken/expiry),user_id 改由
    // 额度端点取(见 mod.rs::fetch_account_user_id)。保留 debug 级 key 日志便于将来排障。
    if let Some(obj) = result.as_object() {
        let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        tracing::debug!(result_keys = ?keys, "[Trae] ExchangeToken Result keys");
    }
    let token = result
        .get("Token")
        .and_then(|t| t.as_str())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or(TraeError::MissingField("Result.Token"))?;
    let refresh_token = first_str(result, &["RefreshToken", "refresh_token"]).unwrap_or_default();
    let token_expire_at_ms = first_field(result, &["TokenExpireAt", "token_expire_at"])
        .map(to_ms)
        .unwrap_or(0);
    let refresh_expire_at_ms = first_field(result, &["RefreshExpireAt", "refresh_expire_at"])
        .map(to_ms)
        .unwrap_or(0);
    // 字段名容错:实测 ExchangeToken Result 不带 UserID(可能在别名 / GetUserInfo),多名兜底。
    let user_id = ["UserID", "UserId", "user_id", "Uid", "uid"]
        .iter()
        .find_map(|k| result.get(*k).and_then(json_to_string))
        .filter(|s| !s.is_empty());
    let ai_region = first_str(result, &["AIRegion", "AiRegion", "ai_region", "region"]);
    Ok(TraeTokenResult {
        token,
        refresh_token,
        token_expire_at_ms,
        refresh_expire_at_ms,
        user_id,
        ai_region,
    })
}

/// 把 JSON 数 / 字符串时间戳容错转成 UNIX ms。数字 < 1e12 视为秒。
fn to_ms(v: &serde_json::Value) -> i64 {
    if let Some(n) = v.as_i64() {
        return if n > 0 && n < 1_000_000_000_000 {
            n * 1000
        } else {
            n
        };
    }
    if let Some(s) = v.as_str() {
        if let Ok(n) = s.parse::<i64>() {
            return if n > 0 && n < 1_000_000_000_000 {
                n * 1000
            } else {
                n
            };
        }
    }
    0
}

/// UserID 可能是数字或字符串。
fn json_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 多名兜底:返回第一个存在的 key 对应的 value。
fn first_field<'a>(obj: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|k| obj.get(*k))
}

/// 多名兜底取非空字符串。
fn first_str(obj: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn uuid_v4() -> Result<String, super::crypto::CryptoError> {
    // 复用 device 模块的格式;这里只需一个随机 trace id。
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| super::crypto::CryptoError::Rng(e.to_string()))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

/// 当前 UNIX 秒(DeviceProof timestamp 用)。
pub(crate) fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前 UNIX ms(凭证 obtained_at_ms 用)。
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
<title>Codex App Transfer — Trae Authorized</title>
<style>
body { font-family: -apple-system, system-ui, sans-serif; max-width: 600px; margin: 60px auto; padding: 0 20px; color: #333; }
h1 { color: #4caf50; }
p { line-height: 1.6; }
</style>
</head>
<body>
<h1>✓ Trae authorization complete</h1>
<p>You can close this window and return to <strong>Codex App Transfer</strong>.</p>
<p>授权完成,请关闭此窗口返回 <strong>Codex App Transfer</strong>。</p>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trae::constants::TRAE_CN_CONFIG;

    fn fp() -> DeviceFingerprint {
        DeviceFingerprint::generate().unwrap()
    }

    #[test]
    fn authorize_url_has_all_required_params() {
        let f = fp();
        let pkce = super::super::crypto::generate_pkce().unwrap();
        let url = build_authorize_url(
            &TRAE_CN_CONFIG,
            &f,
            "http://127.0.0.1:5555/authorize",
            &pkce,
            "trace-123",
        );
        assert!(url.starts_with("https://www.trae.cn/authorization?"));
        assert!(url.contains("auth_type=local"));
        assert!(url.contains("client_id=en1oxy7wnw8j9n"));
        assert!(url.contains("auth_from=solo"));
        assert!(url.contains("login_channel=native_ide"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("hide_saas_login=true"));
        assert!(url.contains(&format!("device_id={}", f.device_id)));
        // auth_callback_url 被 url-encode
        assert!(url.contains("auth_callback_url=http%3A%2F%2F127.0.0.1%3A5555%2Fauthorize"));
    }

    #[test]
    fn interpret_callback_extracts_authcode_from_json() {
        let q = CallbackQuery {
            auth_code_info: Some(r#"{"AuthCode":"ac-xyz","Foo":1}"#.to_string()),
            code: None,
            error_code: None,
            error_msg: None,
        };
        match interpret_callback(q) {
            CallbackResult::Code(c) => assert_eq!(c, "ac-xyz"),
            other => panic!("应取到 AuthCode,实际 {other:?}"),
        }
    }

    #[test]
    fn interpret_callback_falls_back_to_code_param() {
        let q = CallbackQuery {
            auth_code_info: None,
            code: Some("plain-code".to_string()),
            error_code: None,
            error_msg: None,
        };
        assert!(matches!(interpret_callback(q), CallbackResult::Code(c) if c == "plain-code"));
    }

    #[test]
    fn interpret_callback_denied_on_error() {
        let q = CallbackQuery {
            auth_code_info: None,
            code: None,
            error_code: Some("access_denied".to_string()),
            error_msg: Some("user cancelled".to_string()),
        };
        match interpret_callback(q) {
            CallbackResult::Denied { error, description } => {
                assert_eq!(error, "access_denied");
                assert_eq!(description.as_deref(), Some("user cancelled"));
            }
            other => panic!("应为 Denied,实际 {other:?}"),
        }
    }

    #[test]
    fn interpret_callback_malformed_when_empty() {
        let q = CallbackQuery {
            auth_code_info: Some("not json".to_string()),
            code: None,
            error_code: None,
            error_msg: None,
        };
        assert!(matches!(interpret_callback(q), CallbackResult::Malformed));
    }

    #[test]
    fn parse_result_extracts_tokens() {
        let body = r#"{"Result":{
            "Token":" ey.jwt.tok ","RefreshToken":"rt-1",
            "TokenExpireAt":1700000000,"RefreshExpireAt":1800000000,
            "UserID":2767898365400680,"AIRegion":"cn"
        }}"#;
        let r = parse_token_result(body).unwrap();
        assert_eq!(r.token, "ey.jwt.tok", "Token 应 trim");
        assert_eq!(r.refresh_token, "rt-1");
        assert_eq!(r.token_expire_at_ms, 1_700_000_000_000, "秒应转 ms");
        assert_eq!(r.user_id.as_deref(), Some("2767898365400680"));
        assert_eq!(r.ai_region.as_deref(), Some("cn"));
    }

    #[test]
    fn parse_result_rejects_business_error() {
        let body = r#"{"ResponseMetadata":{"Error":{"Code":"TokenExpired","Message":"x"}}}"#;
        match parse_token_result(body).unwrap_err() {
            TraeError::Business { code, msg } => {
                assert_eq!(code, "TokenExpired");
                assert_eq!(msg, "x");
            }
            other => panic!("应为 Business,实际 {other:?}"),
        }
    }

    #[test]
    fn parse_result_missing_token_errors() {
        let body = r#"{"Result":{"RefreshToken":"rt"}}"#;
        assert!(matches!(
            parse_token_result(body).unwrap_err(),
            TraeError::MissingField("Result.Token")
        ));
    }

    #[test]
    fn to_ms_heuristic() {
        assert_eq!(
            to_ms(&serde_json::json!(1_700_000_000i64)),
            1_700_000_000_000
        );
        assert_eq!(
            to_ms(&serde_json::json!(1_700_000_000_000i64)),
            1_700_000_000_000
        );
        assert_eq!(to_ms(&serde_json::json!("1700000000")), 1_700_000_000_000);
        assert_eq!(to_ms(&serde_json::json!(0)), 0);
    }
}
