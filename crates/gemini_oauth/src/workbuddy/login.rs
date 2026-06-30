//! WorkBuddy 账号登录(external-link 轮询式 OAuth)+ token 自动 refresh。
//!
//! 逆向自桌面端 `ExternalLinkAuthenticationProvider`(实测验证):
//!
//! ```text
//! ① POST /v2/plugin/auth/state?platform=workbuddy  (X-No-* 头, body {})
//!    → {code:0, data:{state, authUrl}}
//! ② 浏览器打开 authUrl,用户用腾讯账号登录(Keycloak realm=copilot)
//! ③ 轮询 GET /v2/plugin/auth/token?state=<state>  (X-No-* 头, ~1s)
//!    登录中 → {code:11217, msg:"login ing..."};完成 → {code:0, data:{accessToken,...}}
//! ④ refresh POST /v2/plugin/auth/token/refresh  (X-Refresh-Token + X-Auth-Refresh-Source:plugin)
//! ```
//!
//! 设计参照 `trae`/`zai` 登录模块,但 WorkBuddy 无 PKCE / 无 loopback / 无设备密钥,
//! 是最简单的"服务端 state + 客户端轮询"式,故独立实现轻量 flow。

use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::watch;

use super::token::{unix_now_ms, WorkbuddyCredential, WorkbuddyTokenError};
use super::{user_id_from_jwt, WORKBUDDY_HOST};

/// 登录 / refresh 的 base —— 取正式网关(staging 切换非本期目标)。
fn base_url() -> String {
    format!("https://{WORKBUDDY_HOST}")
}

/// 轮询间隔 / 总超时 —— 对齐桌面端体感(~1s 一轮,5 分钟放弃)。
const POLL_INTERVAL: Duration = Duration::from_millis(1200);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// 业务码:登录尚未完成(继续轮询)。
const CODE_LOGIN_PENDING: i64 = 11217;

#[derive(Debug, Error)]
pub enum WorkbuddyError {
    #[error("home 目录不可用,无法定位 token store: {0}")]
    Token(#[from] WorkbuddyTokenError),
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("上游业务错误 code={code}: {msg}")]
    Business { code: i64, msg: String },
    #[error("响应缺字段: {0}")]
    Parse(&'static str),
    #[error("登录超时(5 分钟未完成)")]
    Timeout,
    #[error("登录被取消")]
    Cancelled,
    #[error("未登录(无本地凭证)")]
    NotLoggedIn,
}

/// WorkBuddy 通用响应封套 `{code, msg, data}`。
#[derive(Deserialize)]
struct Envelope<T> {
    code: i64,
    #[serde(default)]
    msg: String,
    // `Option<T>` 缺失即 None,不加 `#[serde(default)]`(field-level default 会给泛型
    // impl 强加 `T: Default` bound,而 StateData/TokenData 无需 Default)。
    data: Option<T>,
}

#[derive(Deserialize)]
struct StateData {
    state: String,
    #[serde(rename = "authUrl")]
    auth_url: String,
}

/// `/auth/token` + `/auth/token/refresh` 的 data —— camelCase 上游字段。
#[derive(Deserialize)]
struct TokenData {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
    #[serde(rename = "expiresIn", default)]
    expires_in: i64,
    #[serde(rename = "tokenType", default)]
    token_type: String,
    #[serde(default)]
    nickname: Option<String>,
}

/// state / token 轮询都带这组"免鉴权"头(逆向自桌面端:这两步本就在登录前,无 token)。
fn no_auth_headers() -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    for k in [
        "x-no-authorization",
        "x-no-user-id",
        "x-no-enterprise-id",
        "x-no-department-info",
    ] {
        h.insert(HeaderName::from_static(k), HeaderValue::from_static("true"));
    }
    h
}

fn token_to_cred(t: TokenData) -> WorkbuddyCredential {
    let now = unix_now_ms();
    let token_type = if t.token_type.is_empty() {
        "Bearer".to_string()
    } else {
        t.token_type
    };
    let uid = user_id_from_jwt(&t.access_token);
    WorkbuddyCredential {
        expiry_date: now + t.expires_in.max(0) * 1000,
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        token_type,
        obtained_at_ms: now,
        nickname: t.nickname,
        uid,
        // device_id 是**本地生成的账号专属设备指纹**,不来自上游;由 caller(pool::add_account /
        // ensure_valid 续期)按「复用既有 / 新登录新生成」补上。
        device_id: None,
    }
}

/// 跑完整账号登录:请求 state → 回调 `on_auth_url`(由 call site 开浏览器/webview)→
/// 轮询 token。返回凭证(由 call site 落盘)。`cancel` 置 true 中断轮询。
pub async fn run_workbuddy_login(
    http: &reqwest::Client,
    on_auth_url: impl Fn(&str),
    mut cancel: Option<watch::Receiver<bool>>,
) -> Result<WorkbuddyCredential, WorkbuddyError> {
    // ① 请求 state
    let base = base_url();
    let resp: Envelope<StateData> = http
        .post(format!("{base}/v2/plugin/auth/state?platform=workbuddy"))
        .headers(no_auth_headers())
        .json(&serde_json::json!({}))
        .send()
        .await?
        .json()
        .await?;
    if resp.code != 0 {
        return Err(WorkbuddyError::Business {
            code: resp.code,
            msg: resp.msg,
        });
    }
    let state = resp.data.ok_or(WorkbuddyError::Parse("data"))?;

    // ② 交给 call site 打开 authUrl
    on_auth_url(&state.auth_url);

    // ③ 轮询 token
    let token_url = format!("{base}/v2/plugin/auth/token?state={}", state.state);
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    loop {
        if is_cancelled(&mut cancel) {
            return Err(WorkbuddyError::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(WorkbuddyError::Timeout);
        }
        let env: Envelope<TokenData> = http
            .get(&token_url)
            .headers(no_auth_headers())
            .send()
            .await?
            .json()
            .await?;
        match env.code {
            0 => {
                let data = env.data.ok_or(WorkbuddyError::Parse("token data"))?;
                return Ok(token_to_cred(data));
            }
            CODE_LOGIN_PENDING => {
                // 继续轮询(可被 cancel 打断)
                if wait_or_cancel(POLL_INTERVAL, &mut cancel).await {
                    return Err(WorkbuddyError::Cancelled);
                }
            }
            other => {
                return Err(WorkbuddyError::Business {
                    code: other,
                    msg: env.msg,
                })
            }
        }
    }
}

/// 用 refresh token 换一组新 token(`X-Refresh-Token` 头 + `X-Auth-Refresh-Source: plugin`)。
pub async fn refresh_workbuddy_token(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<WorkbuddyCredential, WorkbuddyError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(refresh_token) {
        headers.insert(HeaderName::from_static("x-refresh-token"), v);
    }
    headers.insert(
        HeaderName::from_static("x-auth-refresh-source"),
        HeaderValue::from_static("plugin"),
    );
    let env: Envelope<TokenData> = http
        .post(format!("{}/v2/plugin/auth/token/refresh", base_url()))
        .headers(headers)
        .json(&serde_json::json!({}))
        .send()
        .await?
        .json()
        .await?;
    if env.code != 0 {
        return Err(WorkbuddyError::Business {
            code: env.code,
            msg: env.msg,
        });
    }
    let data = env.data.ok_or(WorkbuddyError::Parse("refresh data"))?;
    Ok(token_to_cred(data))
}

/// 进程级 single-flight refresh 锁 —— 防多个 Codex 请求在 token 进入 refresh 窗口后
/// 各自并发 refresh:并发会抢同一 `workbuddy-oauth.json.tmp` temp 文件(互相 unlink/rename
/// 致一方失败)+ 上游 refresh 每次轮换 refresh token(并发调用互相使对方的旧 refresh token
/// 失效)。对齐 gemini `service::ensure_valid_access_token` 的单飞模式(codex review P2)。
fn refresh_mutex() -> &'static tokio::sync::Mutex<()> {
    static MUTEX: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 请求前确保有效 access token:load → 过期则 refresh + 落盘 → 返回 access token。
/// 无本地凭证 → `NotLoggedIn`(forward 据此映射 needs_login,提示用户重登)。
pub async fn ensure_valid_workbuddy_token(
    http: &reqwest::Client,
    store: &super::token::WorkbuddyCredentialStore,
) -> Result<String, WorkbuddyError> {
    // 第一次 load(无锁):大多数情况 token 没过期,直接返避免 mutex contention。
    let cred = store.load()?.ok_or(WorkbuddyError::NotLoggedIn)?;
    if !cred.should_refresh() {
        return Ok(cred.access_token);
    }
    // 过期窗口内 — 进 single-flight critical section。
    let _guard = refresh_mutex().lock().await;
    // 拿到锁后**重新 load**:若并发请求已 refresh 过,这里直接复用新 token,跳过自己的 refresh。
    let cred = store.load()?.ok_or(WorkbuddyError::NotLoggedIn)?;
    if !cred.should_refresh() {
        return Ok(cred.access_token);
    }
    let mut fresh = refresh_workbuddy_token(http, &cred.refresh_token).await?;
    // 续期只换 token —— 本地账号专属 device_id 必须保留(否则 refresh 后设备指纹丢失、风控隔离失效);
    // nickname 上游 refresh 不一定回带,也沿用旧值。
    fresh.device_id = cred.device_id.clone();
    if fresh.nickname.is_none() {
        fresh.nickname = cred.nickname.clone();
    }
    store.save(&fresh)?;
    Ok(fresh.access_token)
}

fn is_cancelled(cancel: &mut Option<watch::Receiver<bool>>) -> bool {
    cancel.as_mut().is_some_and(|rx| *rx.borrow())
}

/// sleep `dur`,期间若 cancel 变 true 提前返回 true(已取消)。
async fn wait_or_cancel(dur: Duration, cancel: &mut Option<watch::Receiver<bool>>) -> bool {
    match cancel.as_mut() {
        Some(rx) => tokio::select! {
            _ = tokio::time::sleep(dur) => *rx.borrow(),
            r = rx.changed() => r.is_ok() && *rx.borrow(),
        },
        None => {
            tokio::time::sleep(dur).await;
            false
        }
    }
}
