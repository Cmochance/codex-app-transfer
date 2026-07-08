//! grok build(xAI grok CLI / grok-code 编码后端)账号登录 provider。
//!
//! 跟 [`super::workbuddy`] / [`super::trae`] 并行:浏览器授权一次 → 本地持久化凭证 →
//! 后续复用 + 临期自动 refresh,免手填 key、**不依赖本地安装 grok CLI**。
//!
//! ## 鉴权 wire(2026-07-06 抓包 + 二进制静态实证)
//!
//! 标准 **OAuth2 device authorization grant(RFC 8628)**,`accounts.x.ai`:
//! 1. `POST /oauth2/device/code`(form:`client_id` + `scope`)→ `{device_code, user_code,
//!    verification_uri, verification_uri_complete, expires_in, interval}`
//! 2. 用户浏览器打开 `verification_uri_complete` 授权
//! 3. `POST /oauth2/token`(form:`grant_type=urn:ietf:params:oauth:grant-type:device_code`
//!    + `device_code` + `client_id`)轮询 → `{access_token, refresh_token, token_type,
//!    expires_in, id_token}`;授权前返 `{error:"authorization_pending"|"slow_down"}`
//! 4. 临期:`POST /oauth2/token`(`grant_type=refresh_token` + `refresh_token` + `client_id`)
//!
//! access token = auth.x.ai 签发的 OIDC JWT(ES256),6h TTL。打 `cli-chat-proxy.grok.com/v1`
//! 的 Responses wire,`Authorization: Bearer <access_token>` + grok-shell 客户端指纹头(forward.rs)。
//!
//! ## client_id
//!
//! grok CLI 的 client_id 由远端 `login-config` 下发(可轮换),**未硬编码**。本模块 v1 用
//! 抓包实证的 [`DEFAULT_CLIENT_ID`](= 登录账号 JWT 的 aud)。login-config 动态取的确切端点
//! 尚未抓实(登录流被 grok leader 架构挡住,见 MOC-299),[`resolve_client_id`] 预留 seam,
//! 待端点核实后接入(followup);当前恒返回 pin 值。

pub mod token;

use std::sync::OnceLock;

use serde::Deserialize;
use thiserror::Error;

pub use token::{unix_now_ms, GrokBuildCredential, GrokBuildCredentialStore, GrokBuildTokenError};

/// device authorization 端点。
pub const DEVICE_CODE_URL: &str = "https://accounts.x.ai/oauth2/device/code";
/// token 端点(device_code 换 token + refresh_token 续期)。
pub const TOKEN_URL: &str = "https://accounts.x.ai/oauth2/token";
/// RFC 8628 device_code grant_type。
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// 抓包实证的 OAuth client_id(登录账号 JWT 的 aud);login-config 动态取未接入前的兜底。
pub const DEFAULT_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// 登录 scope(JWT scope claim 实证)。
pub const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";

/// grok CLI 客户端指纹(2026-07-06 抓包实证:真实 `grok -p` 对 `/v1/responses` 的头)。
/// 对齐官方客户端身份避免服务端风控判定为非官方客户端。版本随 grok CLI 升级会漂移,
/// 漂移不致命(服务端主要看 identifier);需要时同步这里。
const CLIENT_VERSION: &str = "0.2.87";
const CLIENT_IDENTIFIER: &str = "grok-shell";
const USER_AGENT: &str = "grok-shell/0.2.87 (macos; aarch64)";
const TOKEN_AUTH_MARKER: &str = "xai-grok-cli";
const AUTHENTICATE_RESPONSE_MARKER: &str = "authenticate-response";

/// 稳定的 `x-grok-agent-id` —— 真实客户端一台机器一个稳定 agent id(grok CLI 存
/// `~/.grok/agent_id`)。首次生成 v4 UUID 持久化到 `~/.codex-app-transfer/grok-build-agent-id`,
/// 之后复用;读写失败退化成进程内稳定值。
pub fn agent_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = codex_app_transfer_registry::paths::resolve_home()
                .map(|h| h.join(".codex-app-transfer").join("grok-build-agent-id"));
            if let Some(p) = &path {
                if let Ok(s) = std::fs::read_to_string(p) {
                    let t = s.trim();
                    if !t.is_empty() {
                        return t.to_string();
                    }
                }
            }
            let id = crate::workbuddy::uuid_v4();
            if let Some(p) = &path {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(p, &id);
            }
            id
        })
        .clone()
}

/// 进程内稳定的 `x-grok-session-id` / `x-grok-conv-id`(真实客户端一个对话复用同 id;
/// Codex 会话无法精确对齐 grok 会话,取进程级稳定值即可,比每请求换更像真实客户端)。
fn session_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(crate::workbuddy::uuid_v4).clone()
}

fn conv_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(crate::workbuddy::uuid_v4).clone()
}

/// grok-shell 客户端指纹头集合(固定身份 + 会话/请求标识)。`Authorization: Bearer` 由
/// `inject_auth` 注,`content-type` / `accept` 由 Codex 透传 body 自带。`model_id` = 本次
/// rewrite 后的上游模型(填 `x-grok-model-override`)。`x-grok-req-id` 每请求新 UUID,
/// `x-grok-session-id` / `x-grok-conv-id` / `x-grok-agent-id` 稳定。返回 (name,value) 供
/// call site reqwest 直接塞(与 workbuddy_source_headers 同形)。
pub fn client_headers(model_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("user-agent", USER_AGENT.to_string()),
        ("x-xai-token-auth", TOKEN_AUTH_MARKER.to_string()),
        (
            "x-authenticateresponse",
            AUTHENTICATE_RESPONSE_MARKER.to_string(),
        ),
        ("x-grok-client-identifier", CLIENT_IDENTIFIER.to_string()),
        ("x-grok-client-version", CLIENT_VERSION.to_string()),
        ("x-grok-model-override", model_id.to_string()),
        ("x-grok-conv-id", conv_id()),
        ("x-grok-session-id", session_id()),
        ("x-grok-agent-id", agent_id()),
        ("x-grok-req-id", crate::workbuddy::uuid_v4()),
    ]
}

/// 本次请求出站要注入的 grok 指纹头名集合(小写)—— 用于 strip 入站同名头,防 reqwest
/// `header()` 的 append 语义出现双值。与 [`client_headers`] 的 name 保持一致。
pub fn is_grok_build_owned_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "user-agent"
            | "x-xai-token-auth"
            | "x-authenticateresponse"
            | "x-grok-client-identifier"
            | "x-grok-client-version"
            | "x-grok-model-override"
            | "x-grok-conv-id"
            | "x-grok-session-id"
            | "x-grok-agent-id"
            | "x-grok-req-id"
    )
}

/// access token 续期提前量(临期 5 分钟内即刷,与 token store REFRESH_BUFFER 对齐)。
const REFRESH_SKEW_MS: i64 = 300_000;
/// 服务端未回 `expires_in` 时的兜底 TTL:1 小时(auth.x.ai 实测 6h,取更保守值免过期后每请求 401)。
const ASSUMED_TOKEN_TTL_MS: i64 = 60 * 60 * 1000;
/// device 授权轮询在服务端 `slow_down` 时的 interval 增量(RFC 8628 §3.5)。
const SLOW_DOWN_STEP_SECS: i64 = 5;

#[derive(Debug, Error)]
pub enum GrokBuildError {
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("grok 端返非 2xx: HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("OAuth 错误 (error={error}): {description}")]
    OAuth { error: String, description: String },
    #[error("响应 JSON 解析失败: {0}")]
    Parse(String),
    #[error("响应缺少必需字段: {0}")]
    MissingField(&'static str),
    #[error("设备授权已过期,请重新发起登录")]
    DeviceCodeExpired,
    #[error("用户拒绝了授权")]
    AccessDenied,
    #[error("登录已取消")]
    Cancelled,
    #[error("凭证持久化失败: {0}")]
    Token(#[from] GrokBuildTokenError),
    #[error("未登录(无凭证)")]
    NotLoggedIn,
}

/// `POST /oauth2/device/code` 的响应(RFC 8628)。UI 用 `user_code` +
/// `verification_uri_complete` 引导用户授权,后台用 `device_code` + `interval` 轮询。
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: i64,
    #[serde(default = "default_interval")]
    pub interval: i64,
}

fn default_interval() -> i64 {
    5
}

/// token 端点成功响应(device_code 换取 / refresh)。
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

/// token 端点错误响应(RFC 6749 §5.2 / RFC 8628 §3.5)。
#[derive(Debug, Clone, Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// 取本次登录用的 OAuth client_id。
///
/// **seam**:v1 恒返回 [`DEFAULT_CLIENT_ID`]。login-config 动态取(抗 client_id 轮换)的确切
/// 端点尚未抓实(MOC-299),核实后在此接 `cli-chat-proxy.grok.com/v1/login-config` 探测,失败
/// 仍兜底 pin 值。签名保留 `http` + async 以便后续接入不改调用方。
pub async fn resolve_client_id(_http: &reqwest::Client) -> String {
    DEFAULT_CLIENT_ID.to_string()
}

/// 第 1 步:发起 device authorization,拿 `user_code` + 授权 URL + `device_code`。
pub async fn start_device_authorization(
    http: &reqwest::Client,
    client_id: &str,
) -> Result<DeviceAuthResponse, GrokBuildError> {
    let resp = http
        .post(DEVICE_CODE_URL)
        .form(&[("client_id", client_id), ("scope", SCOPES)])
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        // device/code 阶段的 4xx 一般是 client_id/scope 配置错,直接失败(非轮询态)。
        if let Ok(err) = serde_json::from_str::<OAuthErrorBody>(&body) {
            return Err(GrokBuildError::OAuth {
                error: err.error,
                description: err.error_description.unwrap_or_default(),
            });
        }
        return Err(GrokBuildError::Status {
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_str::<DeviceAuthResponse>(&body)
        .map_err(|e| GrokBuildError::Parse(e.to_string()))
}

/// 第 2 步:轮询 token 端点直到用户授权完成 / 拒绝 / 超时 / 取消。成功即**落盘** [`GrokBuildCredential`]。
///
/// `authorization_pending` → 继续按 `interval` 轮询;`slow_down` → interval += 5s;
/// `access_denied` → [`GrokBuildError::AccessDenied`];`expired_token`/`expired` →
/// [`GrokBuildError::DeviceCodeExpired`]。`cancel` 被置 true 立即中止(不落盘)。
pub async fn poll_for_token(
    http: &reqwest::Client,
    client_id: &str,
    device: &DeviceAuthResponse,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<GrokBuildCredential, GrokBuildError> {
    let deadline_ms = unix_now_ms() + device.expires_in.max(1) * 1000;
    let mut interval_secs = device.interval.max(1);
    loop {
        if is_cancelled(&cancel) {
            return Err(GrokBuildError::Cancelled);
        }
        // 先等 interval(device/code 刚返回,用户还没授权,首轮也应等)。可被 cancel 唤醒。
        if let Some(rx) = cancel.as_mut() {
            let sleep = tokio::time::sleep(std::time::Duration::from_secs(interval_secs as u64));
            tokio::select! {
                _ = sleep => {}
                _ = rx.changed() => {
                    if *rx.borrow() { return Err(GrokBuildError::Cancelled); }
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs as u64)).await;
        }
        if unix_now_ms() >= deadline_ms {
            return Err(GrokBuildError::DeviceCodeExpired);
        }

        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", device.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;

        if status.is_success() {
            let tok: TokenResponse =
                serde_json::from_str(&body).map_err(|e| GrokBuildError::Parse(e.to_string()))?;
            let cred = credential_from_token(tok, client_id)?;
            let store = GrokBuildCredentialStore::single()?;
            store.save(&cred)?;
            return Ok(cred);
        }

        // 非 2xx:解析 OAuth error,区分「继续轮询」与「终止」。
        let err =
            serde_json::from_str::<OAuthErrorBody>(&body).map_err(|_| GrokBuildError::Status {
                status: status.as_u16(),
                body: body.clone(),
            })?;
        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                interval_secs += SLOW_DOWN_STEP_SECS;
                continue;
            }
            "access_denied" => return Err(GrokBuildError::AccessDenied),
            "expired_token" | "expired" => return Err(GrokBuildError::DeviceCodeExpired),
            other => {
                return Err(GrokBuildError::OAuth {
                    error: other.to_string(),
                    description: err.error_description.unwrap_or_default(),
                })
            }
        }
    }
}

/// 跑完整 device 登录:resolve client_id → `device/code` → 回调交出 `DeviceAuthResponse`
/// (UI 据 `user_code` + `verification_uri_complete` 引导用户浏览器授权)→ 轮询 token →
/// 成功**已落盘** [`GrokBuildCredential`] 并返回。与 [`super::qoder::run_qoder_login`] 同形
/// (callback 交出授权入口,cancel 贯穿全程,返回凭证)。
pub async fn run_grok_build_login(
    http: &reqwest::Client,
    on_device_auth: impl FnOnce(&DeviceAuthResponse),
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<GrokBuildCredential, GrokBuildError> {
    let client_id = resolve_client_id(http).await;
    let device = start_device_authorization(http, &client_id).await?;
    on_device_auth(&device);
    poll_for_token(http, &client_id, &device, cancel).await
}

/// 取一个**有效**的 access token:加载凭证,临期 / 已过期则用 refresh_token 续期并落盘,
/// 返回最新凭证。proxy forward 每请求前调它拿当前 token。
///
/// refresh 明确被拒(4xx / invalid_grant)→ 删凭证返 [`GrokBuildError::NotLoggedIn`],UI 重登;
/// 瞬时错(网络 / 5xx / 429)→ 沿用旧凭证下个周期再试(不删)。
pub async fn ensure_valid_grok_build_token(
    http: &reqwest::Client,
) -> Result<GrokBuildCredential, GrokBuildError> {
    let store = GrokBuildCredentialStore::single()?;
    let cred = store.load()?.ok_or(GrokBuildError::NotLoggedIn)?;

    let effective_expire = if cred.expiry_date > 0 {
        cred.expiry_date
    } else {
        cred.obtained_at_ms + ASSUMED_TOKEN_TTL_MS
    };
    if unix_now_ms() + REFRESH_SKEW_MS < effective_expire {
        return Ok(cred);
    }

    let client_id = cred
        .client_id
        .clone()
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    tracing::info!("grok build access token 临期,续期中");
    let refreshed = match refresh_token_request(http, &client_id, &cred.refresh_token).await {
        Ok(r) => r,
        Err(e) if is_transient_refresh_error(&e) => {
            tracing::warn!(error = %e, "grok build 续期瞬时失败,沿用旧凭证(下个周期重试)");
            return Ok(cred);
        }
        Err(e) => {
            tracing::warn!(error = %e, "grok build 续期被拒(鉴权),删凭证 + 需重新登录");
            let _ = store.delete();
            return Err(GrokBuildError::NotLoggedIn);
        }
    };

    // refresh 响应可能不回新 refresh_token → 保留旧的;不回 expires_in → 用兜底 TTL。
    let now = unix_now_ms();
    let updated = GrokBuildCredential {
        access_token: refreshed.access_token,
        refresh_token: refreshed
            .refresh_token
            .filter(|s| !s.is_empty())
            .unwrap_or(cred.refresh_token),
        token_type: refreshed.token_type.unwrap_or(cred.token_type),
        expiry_date: now + refreshed.expires_in.unwrap_or(ASSUMED_TOKEN_TTL_MS / 1000) * 1000,
        obtained_at_ms: now,
        client_id: Some(client_id),
        email: cred.email,
        user_id: cred.user_id,
    };
    store.save(&updated)?;
    Ok(updated)
}

/// 删凭证(logout)。
pub fn logout() -> Result<(), GrokBuildError> {
    GrokBuildCredentialStore::single()?.delete()?;
    Ok(())
}

/// `POST /oauth2/token`(`grant_type=refresh_token`)。
async fn refresh_token_request(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, GrokBuildError> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        if let Ok(err) = serde_json::from_str::<OAuthErrorBody>(&body) {
            return Err(GrokBuildError::OAuth {
                error: err.error,
                description: err.error_description.unwrap_or_default(),
            });
        }
        return Err(GrokBuildError::Status {
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_str::<TokenResponse>(&body).map_err(|e| GrokBuildError::Parse(e.to_string()))
}

/// TokenResponse → 落盘凭证(算 expiry、best-effort 从 id_token 取 email/sub)。
fn credential_from_token(
    tok: TokenResponse,
    client_id: &str,
) -> Result<GrokBuildCredential, GrokBuildError> {
    if tok.access_token.is_empty() {
        return Err(GrokBuildError::MissingField("access_token"));
    }
    let now = unix_now_ms();
    let (email, user_id) = tok
        .id_token
        .as_deref()
        .or(Some(tok.access_token.as_str()))
        .map(jwt_email_and_sub)
        .unwrap_or((None, None));
    Ok(GrokBuildCredential {
        access_token: tok.access_token,
        refresh_token: tok.refresh_token.unwrap_or_default(),
        token_type: tok.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expiry_date: now + tok.expires_in.unwrap_or(ASSUMED_TOKEN_TTL_MS / 1000) * 1000,
        obtained_at_ms: now,
        client_id: Some(client_id.to_string()),
        email,
        user_id,
    })
}

/// best-effort 解 JWT payload 取 `email` / `sub`(仅 UI 展示,失败返 (None,None))。
fn jwt_email_and_sub(jwt: &str) -> (Option<String>, Option<String>) {
    use base64::Engine;
    let Some(payload_b64) = jwt.split('.').nth(1) else {
        return (None, None);
    };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return (None, None);
    };
    let email = v.get("email").and_then(|s| s.as_str()).map(str::to_owned);
    let sub = v.get("sub").and_then(|s| s.as_str()).map(str::to_owned);
    (email, sub)
}

/// refresh 失败是否瞬时(可沿用旧凭证重试,**不删凭证**)。
///
/// [AI review P2] **不透明 HTTP 错误(含 4xx)一律判瞬时**:`accounts.x.ai` 会回 Cloudflare/edge
/// 临时拦(非 JSON 4xx body),`refresh_token_request` 已先 parse `OAuthErrorBody`,parse 不出才落
/// `Status`——即 `Status` 恒是「上游没给可解析 OAuth 错」的不透明失败,不能当撤销强制登出。真正的
/// refresh token 撤销由上游回**可解析的 OAuth `invalid_grant`**(→ `OAuth` 变体)。故:
/// - `Http` / `Parse` / **任意 `Status`(含 4xx)** = 瞬时,沿用旧凭证、下周期重试;
/// - `OAuth{invalid_grant/invalid_client/unauthorized_client}` = 明确 token/client 失效 → 删凭证重登;
/// - 其余 `OAuth`(temporarily_unavailable 等临时/配置类)= 瞬时,不删。
fn is_transient_refresh_error(e: &GrokBuildError) -> bool {
    match e {
        GrokBuildError::Http(_) | GrokBuildError::Parse(_) => true,
        GrokBuildError::Status { .. } => true,
        GrokBuildError::OAuth { error, .. } => !matches!(
            error.as_str(),
            "invalid_grant" | "invalid_client" | "unauthorized_client"
        ),
        _ => false,
    }
}

fn is_cancelled(cancel: &Option<tokio::sync::watch::Receiver<bool>>) -> bool {
    cancel.as_ref().map(|rx| *rx.borrow()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_refresh_error_classification() {
        assert!(is_transient_refresh_error(&GrokBuildError::Parse(
            "x".into()
        )));
        assert!(is_transient_refresh_error(&GrokBuildError::Status {
            status: 503,
            body: String::new()
        }));
        assert!(is_transient_refresh_error(&GrokBuildError::Status {
            status: 429,
            body: String::new()
        }));
        // [AI review P2] 不透明 4xx(CF/edge 临时拦)现判**瞬时**、不删凭证。
        assert!(is_transient_refresh_error(&GrokBuildError::Status {
            status: 400,
            body: "<html>cloudflare</html>".into()
        }));
        // 可解析 OAuth 撤销错 → 非瞬时(删凭证重登)。
        assert!(!is_transient_refresh_error(&GrokBuildError::OAuth {
            error: "invalid_grant".into(),
            description: String::new()
        }));
        // 其余 OAuth(临时/配置类)→ 瞬时,不删。
        assert!(is_transient_refresh_error(&GrokBuildError::OAuth {
            error: "temporarily_unavailable".into(),
            description: String::new()
        }));
        assert!(!is_transient_refresh_error(&GrokBuildError::NotLoggedIn));
    }

    #[test]
    fn jwt_claims_extraction() {
        // {"sub":"u-1","email":"a@b.com"} 的 base64url(无 padding)payload。
        let jwt = "h.eyJzdWIiOiJ1LTEiLCJlbWFpbCI6ImFAYi5jb20ifQ.sig";
        let (email, sub) = jwt_email_and_sub(jwt);
        assert_eq!(email.as_deref(), Some("a@b.com"));
        assert_eq!(sub.as_deref(), Some("u-1"));
        assert_eq!(jwt_email_and_sub("garbage"), (None, None));
    }

    #[test]
    fn client_headers_include_identity_and_model_override() {
        let hs = client_headers("grok-build");
        let map: std::collections::HashMap<&str, String> =
            hs.iter().map(|(k, v)| (*k, v.clone())).collect();
        // x-grok-model-override = 传入的上游模型(MOC-299:防上游选错模型)。
        assert_eq!(
            map.get("x-grok-model-override").map(String::as_str),
            Some("grok-build")
        );
        // 固定身份指纹(避免服务端风控判定为非官方客户端)。
        assert_eq!(
            map.get("user-agent").map(String::as_str),
            Some("grok-shell/0.2.87 (macos; aarch64)")
        );
        assert_eq!(
            map.get("x-xai-token-auth").map(String::as_str),
            Some("xai-grok-cli")
        );
        assert_eq!(
            map.get("x-grok-client-identifier").map(String::as_str),
            Some("grok-shell")
        );
        // 会话/请求标识存在。
        assert!(map.contains_key("x-grok-req-id"));
        assert!(map.contains_key("x-grok-session-id"));
        assert!(map.contains_key("x-grok-agent-id"));
        // strip 名集必须覆盖所有注入头(否则入站同名会 append 成双值)。
        for (name, _) in &hs {
            assert!(
                is_grok_build_owned_header(name),
                "{name} 被 client_headers 注入但未被 is_grok_build_owned_header 认为 owned"
            );
        }
    }

    #[test]
    fn device_auth_response_parses_minimal() {
        let json = r#"{"device_code":"dc","user_code":"ABCD-1234","verification_uri":"https://x.ai/device","expires_in":600}"#;
        let d: DeviceAuthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(d.user_code, "ABCD-1234");
        assert_eq!(d.interval, 5, "缺 interval 默认 5s");
        assert!(d.verification_uri_complete.is_none());
    }
}
