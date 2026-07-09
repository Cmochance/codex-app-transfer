//! grok build(xAI grok CLI / grok-code 编码后端)账号登录 provider。
//!
//! 跟 [`super::workbuddy`] / [`super::trae`] 并行:浏览器授权一次 → 本地持久化凭证 →
//! 后续复用 + 临期自动 refresh,免手填 key、**不依赖本地安装 grok CLI**。
//!
//! ## 鉴权 wire(2026-07-06 抓包 + 2026-07-09 CF 拦截调研,MOC-300)
//!
//! 标准 **OAuth2 authorization code + PKCE(S256)**,issuer `https://auth.x.ai`:
//! 1. **OIDC discovery** `GET https://auth.x.ai/.well-known/openid-configuration` 取
//!    `authorization_endpoint` / `token_endpoint`(端点漂移兜底见 [`FALLBACK_AUTHORIZE_URL`])。
//! 2. 生成 PKCE(verifier/challenge S256)+ state + nonce,拼 authorize URL,**在浏览器/内置
//!    webview 里导航打开**(关键:授权页导航由真实 webview 发起,自然过 Cloudflare challenge —— 这
//!    正是弃用 device flow 的原因:device flow 的 `POST /oauth2/device/code` 是**无浏览器的裸
//!    HTTP**,被 CF WAF 拦返 block page,见 MOC-300)。
//! 3. 用户授权后重定向到固定 loopback `http://127.0.0.1:56121/callback?code=…&state=…`,本地
//!    loopback server 捕获 `code`(+ 校验 `state`)。redirect_uri **必须与 client 注册值精确一致**。
//! 4. `POST {token_endpoint}`(form:`grant_type=authorization_code` + `code` + `redirect_uri`
//!    + `client_id` + `code_verifier`)换 `{access_token, refresh_token, token_type, expires_in,
//!    id_token}`。此 POST 是纯 API(非 HTML 授权页),CF 不拦(pi-xai-oauth 实证 Node 侧可换)。
//! 5. 临期:`POST {token_endpoint}`(`grant_type=refresh_token` + `refresh_token` + `client_id`)。
//!
//! access token = auth.x.ai 签发的 OIDC JWT(ES256),6h TTL。打 `cli-chat-proxy.grok.com/v1`
//! 的 Responses wire,`Authorization: Bearer <access_token>` + grok-shell 客户端指纹头(forward.rs)。
//! 流程拆两段:[`prepare_grok_build_authorization`](discovery+PKCE+URL,不触网授权)+
//! [`complete_grok_build_login`](校验 state → 换 token → 落盘);loopback server + 开浏览器在
//! Tauri handler(`admin::handlers::grok_build_oauth`)。参考实证:github.com/BlockedPath/pi-xai-oauth。
//!
//! ## client_id
//!
//! grok CLI 的 client_id 由远端 `login-config` 下发(可轮换),**未硬编码**。本模块用抓包实证的
//! [`DEFAULT_CLIENT_ID`](= 登录账号 JWT 的 aud,与 pi-xai-oauth 逆向值精确一致)。[`resolve_client_id`]
//! 预留 seam,待 login-config 端点核实后接入(followup);当前恒返回 pin 值。

pub mod token;

use std::sync::OnceLock;

use serde::Deserialize;
use thiserror::Error;

pub use token::{unix_now_ms, GrokBuildCredential, GrokBuildCredentialStore, GrokBuildTokenError};

/// OIDC discovery 文档(动态解析 authorize / token 端点,抗端点漂移)。
pub const OIDC_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// discovery 不可用时的 authorize 端点兜底(issuer=auth.x.ai)。
pub const FALLBACK_AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
/// discovery 不可用时的 token 端点兜底(authorization_code 换 token + refresh_token 续期);
/// 也是老凭证(无存储 token_endpoint)refresh 的回落端点。
pub const FALLBACK_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// 固定 loopback redirect_uri —— **必须与 xAI OAuth client 注册值精确一致**(pi-xai-oauth 实证
/// 官方 grok CLI 用端口 56121;xAI 对 public client 未必放开任意 loopback 端口,故固定不用随机端口)。
pub const REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
/// loopback callback server 监听端口(与 [`REDIRECT_URI`] 一致)。
pub const LOOPBACK_PORT: u16 = 56121;
/// authorization_code grant_type。
pub const AUTH_CODE_GRANT_TYPE: &str = "authorization_code";
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
    #[error("PKCE/RNG 失败: {0}")]
    Rng(String),
    #[error("回调 state 不匹配(可能的 CSRF / 会话串扰),已中止登录")]
    StateMismatch,
    #[error("授权已过期,请重新发起登录")]
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

/// OIDC discovery 文档里我们要的两个端点(其余字段忽略)。
#[derive(Debug, Clone, Deserialize)]
struct OidcDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// 一次 authorization-code 登录的本地状态。handler 据 [`Self::authorize_url`] 开浏览器 + 起
/// loopback,拿到回调 `code`/`state` 后连同本结构交给 [`complete_grok_build_login`]。
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    /// 浏览器/webview 要导航打开的 authorize URL(已带 client_id/redirect/scope/state/nonce/PKCE challenge)。
    pub authorize_url: String,
    /// CSRF 防护:回调 `state` 必须与此精确一致,否则 [`GrokBuildError::StateMismatch`]。
    pub state: String,
    /// PKCE `code_verifier`(换 token 时上送)。
    pub verifier: String,
    /// 本次登录 discovery 到的 token 端点(complete 换 token + 后续 refresh 复用)。
    pub token_endpoint: String,
    /// 固定 loopback redirect_uri(换 token 时须原样回送,须与 client 注册值一致)。
    pub redirect_uri: String,
    /// 本次登录的 client_id。
    pub client_id: String,
}

/// token 端点成功响应(authorization_code 换取 / refresh)。
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

/// OIDC discovery 解析 authorize / token 端点;失败(网络 / CF / 缺字段)回落到硬编码兜底。
/// discovery 是纯 JSON GET,CF 不像对 device/code 那样拦(它只拦裸 device authorization POST)。
pub async fn discover_endpoints(http: &reqwest::Client) -> (String, String) {
    match http.get(OIDC_DISCOVERY_URL).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<OidcDiscovery>().await {
            Ok(d) if !d.authorization_endpoint.is_empty() && !d.token_endpoint.is_empty() => {
                return (d.authorization_endpoint, d.token_endpoint);
            }
            Ok(_) => tracing::warn!("grok OIDC discovery 缺 authorize/token 端点,用兜底"),
            Err(e) => tracing::warn!(error = %e, "grok OIDC discovery 解析失败,用兜底"),
        },
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "grok OIDC discovery 非 2xx,用兜底")
        }
        Err(e) => tracing::warn!(error = %e, "grok OIDC discovery 请求失败,用兜底"),
    }
    (
        FALLBACK_AUTHORIZE_URL.to_string(),
        FALLBACK_TOKEN_URL.to_string(),
    )
}

/// 拼 authorize URL(query:response_type/client_id/redirect_uri/scope/state/nonce/PKCE challenge)。
/// 纯函数,便于单测;`code_challenge_method` 固定 S256。
fn build_authorize_url(
    authorize_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    code_challenge: &str,
) -> Result<String, GrokBuildError> {
    let mut url = url::Url::parse(authorize_endpoint)
        .map_err(|e| GrokBuildError::Parse(format!("authorize 端点非法 URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// **第 1 段**:发起 authorization-code + PKCE 登录 —— resolve client_id → OIDC discovery →
/// 生成 PKCE/state/nonce → 拼 authorize URL。**不触网授权**(授权在浏览器里发生),故不被 CF 拦。
/// 返回 [`AuthorizationRequest`] 供 handler 开浏览器 + 起 loopback callback server。
pub async fn prepare_grok_build_authorization(
    http: &reqwest::Client,
    redirect_uri: &str,
) -> Result<AuthorizationRequest, GrokBuildError> {
    let client_id = resolve_client_id(http).await;
    let (authorize_endpoint, token_endpoint) = discover_endpoints(http).await;
    let pkce = crate::pkce::generate().map_err(GrokBuildError::Rng)?;
    let state = crate::workbuddy::uuid_v4();
    let nonce = crate::workbuddy::uuid_v4();
    let authorize_url = build_authorize_url(
        &authorize_endpoint,
        &client_id,
        redirect_uri,
        &state,
        &nonce,
        &pkce.challenge,
    )?;
    Ok(AuthorizationRequest {
        authorize_url,
        state,
        verifier: pkce.verifier,
        token_endpoint,
        redirect_uri: redirect_uri.to_string(),
        client_id,
    })
}

/// **第 2 段**:回调拿到 `code` + `state` 后完成登录 —— 校验 state(CSRF)→ POST token 端点
/// (`grant_type=authorization_code` + PKCE `code_verifier`)换 token → 落盘 [`GrokBuildCredential`]
/// (含本次 `token_endpoint` 供 refresh 复用)。成功即已 save,返回凭证。
pub async fn complete_grok_build_login(
    http: &reqwest::Client,
    req: &AuthorizationRequest,
    code: &str,
    returned_state: &str,
) -> Result<GrokBuildCredential, GrokBuildError> {
    if returned_state != req.state {
        return Err(GrokBuildError::StateMismatch);
    }
    let resp = http
        .post(&req.token_endpoint)
        .form(&[
            ("grant_type", AUTH_CODE_GRANT_TYPE),
            ("code", code),
            ("redirect_uri", req.redirect_uri.as_str()),
            ("client_id", req.client_id.as_str()),
            ("code_verifier", req.verifier.as_str()),
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
    let tok: TokenResponse =
        serde_json::from_str(&body).map_err(|e| GrokBuildError::Parse(e.to_string()))?;
    let cred = credential_from_token(tok, &req.client_id, Some(req.token_endpoint.clone()))?;
    let store = GrokBuildCredentialStore::single()?;
    store.save(&cred)?;
    Ok(cred)
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
    // 老凭证(登录时无 discovery / 早于本改动)无 token_endpoint → 回落兜底端点。
    let token_endpoint = cred
        .token_endpoint
        .clone()
        .unwrap_or_else(|| FALLBACK_TOKEN_URL.to_string());
    tracing::info!("grok build access token 临期,续期中");
    let refreshed =
        match refresh_token_request(http, &token_endpoint, &client_id, &cred.refresh_token).await {
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
        token_endpoint: Some(token_endpoint),
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

/// `POST {token_endpoint}`(`grant_type=refresh_token`)。端点随凭证传入(discovery 结果 / 兜底)。
async fn refresh_token_request(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, GrokBuildError> {
    let resp = http
        .post(token_endpoint)
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

/// TokenResponse → 落盘凭证(算 expiry、best-effort 从 id_token 取 email/sub)。`token_endpoint`
/// 随凭证存,供后续 refresh 复用同一端点(登录经 discovery 解析)。
fn credential_from_token(
    tok: TokenResponse,
    client_id: &str,
    token_endpoint: Option<String>,
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
        token_endpoint,
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
    fn authorize_url_has_pkce_and_encoded_params() {
        let u = build_authorize_url(
            FALLBACK_AUTHORIZE_URL,
            DEFAULT_CLIENT_ID,
            REDIRECT_URI,
            "st-123",
            "nonce-abc",
            "chal-xyz",
        )
        .unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some(DEFAULT_CLIENT_ID)
        );
        // redirect_uri 原样(url crate 解码后应等于常量,验证编码 roundtrip 正确)。
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some("chal-xyz")
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(q.get("state").map(String::as_str), Some("st-123"));
        assert_eq!(q.get("scope").map(String::as_str), Some(SCOPES));
    }

    #[tokio::test]
    async fn complete_rejects_state_mismatch_before_network() {
        // state 不匹配必须在任何换 token 网络请求**之前**拦下(CSRF 防护),故无需真实端点即可测。
        let http = reqwest::Client::new();
        let req = AuthorizationRequest {
            authorize_url: "https://auth.x.ai/oauth2/authorize?x=1".into(),
            state: "expected-state".into(),
            verifier: "v".into(),
            token_endpoint: "https://auth.x.ai/oauth2/token".into(),
            redirect_uri: REDIRECT_URI.into(),
            client_id: DEFAULT_CLIENT_ID.into(),
        };
        let err = complete_grok_build_login(&http, &req, "code123", "attacker-state")
            .await
            .unwrap_err();
        assert!(matches!(err, GrokBuildError::StateMismatch));
    }
}
