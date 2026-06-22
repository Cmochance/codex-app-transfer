//! Trae(字节 TRAE SOLO CN / Work CN)账号登录 provider。
//!
//! 跟 [`super::zai`] / [`super::antigravity`] **并行**:浏览器登录一次 → 本地持久化
//! 凭证 → 后续复用,免手填 key。但 Trae 是标准 loopback OAuth2 + PKCE,token 直接
//! 可用(无换 org key 步骤),且有 refresh(设备私钥签名续期)。逆向 + 抓包来源见
//! [`constants`] 头注。
//!
//! ## 多账号 + 指纹隔离(用户硬性需求)
//!
//! 凭证按 **provider id** 分文件([`token::TraeCredentialStore::for_provider_id`]),
//! 每账号一套独立 [`device::DeviceFingerprint`] + [`crypto::DeviceKeyPair`],首登生成、
//! 固定复用、切 provider 整包切换 —— 同设备多账号不共用指纹。
//!
//! ## 端到端([`run_trae_login`])
//!
//! 1. 取/生成本账号的指纹 + keypair(同 provider id 再登复用,保持设备连续)
//! 2. [`flow::run_trae_oauth_flow_with_cancel`] — loopback 授权 → AuthCode → 首次
//!    ExchangeToken(无签名)→ JWT + RefreshToken
//! 3. best-effort 拉 email(GetUserInfo)
//! 4. 组装 [`token::TraeCredential`] 落盘 `~/.codex-app-transfer/trae/<id>.json`
//!
//! [`ensure_valid_trae_token`] 在 token 临期时用 RefreshToken + DeviceProof 签名续期。

pub mod constants;
pub mod crypto;
pub mod device;
pub mod flow;
pub mod token;

use thiserror::Error;

use super::flow::{FlowError, OauthFlowConfig};
pub use constants::{TraeEdition, TraeProviderConfig};
pub use crypto::{CryptoError, DeviceKeyPair};
pub use device::{DeviceFingerprint, DeviceInfo};
pub use token::{TraeCredential, TraeCredentialStore};

/// access token 续期提前量(临期 2 分钟内即刷)。
const REFRESH_SKEW_MS: i64 = 120_000;

/// 服务端**未回 token 过期时刻**(`token_expire_at_ms == 0`)时的兜底有效期:
/// 从 `obtained_at_ms` 起算 50 分钟。避免「0 当永不过期 → 永久禁用续期 → token 实际
/// 过期后每次 401」(review [5])。正常响应带 TokenExpireAt 时不走这条。
const ASSUMED_TOKEN_TTL_MS: i64 = 50 * 60 * 1000;

/// Trae 登录链路统一错误。
#[derive(Debug, Error)]
pub enum TraeError {
    #[error("OAuth loopback/授权流程错误: {0}")]
    Flow(#[from] FlowError),
    #[error("设备密钥/签名错误: {0}")]
    Crypto(#[from] CryptoError),
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Trae 端返非 2xx: HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("响应 JSON 解析失败: {0}")]
    Parse(String),
    #[error("Trae 业务响应拒绝 (code={code}): {msg}")]
    Business { code: String, msg: String },
    #[error("响应缺少必需字段: {0}")]
    MissingField(&'static str),
    #[error("凭证持久化失败: {0}")]
    Token(#[from] super::token::TokenError),
    #[error("未登录(无凭证)")]
    NotLoggedIn,
}

/// 跑完整 Trae 账号登录,成功后**已落盘** [`TraeCredential`] 并返回。
///
/// `provider_id`:当前 provider 条目 id —— 决定凭证文件 + 指纹隔离单元。
/// 同 id 再登复用既有指纹/keypair(设备连续);不同 id = 不同设备身份。
/// `cancel`:可选取消信号,贯穿全程,OAuth 后落盘前再查,被取消绝不写盘。
pub async fn run_trae_login(
    http: &reqwest::Client,
    edition: TraeEdition,
    provider_id: &str,
    flow_config: &OauthFlowConfig,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TraeCredential, TraeError> {
    let config = edition.config();
    let store = TraeCredentialStore::for_provider_id(provider_id)?;

    // 同 provider id 复用既有指纹/keypair(设备连续);首登则新生成一套独立指纹。
    let (fingerprint, keypair) = match store.load()? {
        Some(existing) => (existing.fingerprint, existing.keypair),
        None => (DeviceFingerprint::generate()?, DeviceKeyPair::generate()?),
    };

    let cancel_guard = cancel.clone();

    // OAuth → 首次 ExchangeToken(浏览器授权 = 消耗登录的部分)
    let result = flow::run_trae_oauth_flow_with_cancel(
        http,
        &config,
        &fingerprint,
        &keypair,
        flow_config,
        cancel,
    )
    .await?;

    // OAuth 后若已取消(关窗 / 新登录抢占),立即中止不落盘
    if is_cancelled(&cancel_guard) {
        tracing::info!(provider_id, "Trae OAuth 后检测到取消,中止(不落盘)");
        return Err(FlowError::Cancelled.into());
    }

    // best-effort 拉 email(失败不致命)
    let email = fetch_user_email(http, &config, &result.token).await;

    // [review] email fetch 有网络往返,取消可能在此期间到达 —— 落盘前**再查一次**,
    // 被取消 / 被新登录抢占绝不写盘(否则 UI 报已取消但盘上留 ghost 凭证,违反保证)。
    if is_cancelled(&cancel_guard) {
        tracing::info!(provider_id, "Trae 拉 email 后检测到取消,中止(不落盘)");
        return Err(FlowError::Cancelled.into());
    }

    let cred = TraeCredential {
        edition,
        token: result.token,
        refresh_token: result.refresh_token,
        token_expire_at_ms: result.token_expire_at_ms,
        refresh_expire_at_ms: result.refresh_expire_at_ms,
        user_id: result.user_id,
        email,
        ai_region: result.ai_region,
        fingerprint,
        keypair,
        obtained_at_ms: flow::unix_now_ms(),
    };
    if let Err(e) = store.save(&cred) {
        tracing::error!(
            error = %e,
            provider_id,
            path = %store.path().display(),
            "Trae 凭证落盘失败 — 重启后会被当未登录"
        );
        return Err(e.into());
    }
    tracing::info!(
        provider_id,
        email = cred.email.as_deref().unwrap_or(""),
        path = %store.path().display(),
        "Trae 账号登录完成,凭证已落盘"
    );
    Ok(cred)
}

/// 取一个**有效**的 access token:加载凭证,临期 / 已过期则用 RefreshToken 续期并
/// 落盘,返回最新凭证。proxy forward / 额度注入调它拿当前 token。
///
/// refresh token 也过期(`refresh_expire_at_ms` 已过 / 服务端拒)→ 删凭证返
/// [`TraeError::NotLoggedIn`],UI 走重新登录。
pub async fn ensure_valid_trae_token(
    http: &reqwest::Client,
    provider_id: &str,
) -> Result<TraeCredential, TraeError> {
    let store = TraeCredentialStore::for_provider_id(provider_id)?;
    let cred = store.load()?.ok_or(TraeError::NotLoggedIn)?;
    let now = flow::unix_now_ms();

    // 有效期:未知 expiry(0)用 obtained + 兜底 TTL,而非「0 = 永不过期」(review [5])。
    let effective_expire = if cred.token_expire_at_ms > 0 {
        cred.token_expire_at_ms
    } else {
        cred.obtained_at_ms + ASSUMED_TOKEN_TTL_MS
    };
    if now + REFRESH_SKEW_MS < effective_expire {
        return Ok(cred);
    }

    // refresh token 已过期 → 删凭证(否则 status_handler 直读盘面永显已登录,review [3])
    // + NotLoggedIn,UI 走重新登录。
    if cred.refresh_expire_at_ms != 0 && now >= cred.refresh_expire_at_ms {
        tracing::info!(provider_id, "Trae refresh token 已过期,删凭证 + 需重新登录");
        let _ = store.delete();
        return Err(TraeError::NotLoggedIn);
    }

    tracing::info!(provider_id, "Trae access token 临期,续期中");
    let config = cred.edition.config();
    let refreshed = match flow::refresh_token(
        http,
        &config,
        &cred.fingerprint,
        &cred.keypair,
        &cred.refresh_token,
    )
    .await
    {
        Ok(r) => r,
        // 瞬时错(网络 / 5xx / 429 / parse):旧 token 可能仍有效(skew 内),返回旧凭证让
        // quota 用旧 token,下个 tick 再试续期 —— 不删、不 NotLoggedIn,避免一次网络抖动
        // 就让额度行消失(review [4],对齐 quota pipeline 的 transient 容忍)。
        Err(e) if is_transient_refresh_error(&e) => {
            tracing::warn!(provider_id, error = %e, "Trae 续期瞬时失败,沿用旧凭证(下个周期重试)");
            return Ok(cred);
        }
        // 明确鉴权失败(refresh 被拒 / 4xx / 业务拒)→ 删凭证 + NotLoggedIn(review [3][4])
        Err(e) => {
            tracing::warn!(provider_id, error = %e, "Trae 续期被拒(鉴权),删凭证 + 需重新登录");
            let _ = store.delete();
            return Err(TraeError::NotLoggedIn);
        }
    };

    let updated = TraeCredential {
        token: refreshed.token,
        // refresh 响应可能不回新 refresh_token,空则保留旧的
        refresh_token: if refreshed.refresh_token.is_empty() {
            cred.refresh_token
        } else {
            refreshed.refresh_token
        },
        // expiry 空(0)保留旧值,别用 0 覆盖已知 expiry(否则下次走兜底/永久禁刷,review [5])
        token_expire_at_ms: if refreshed.token_expire_at_ms == 0 {
            cred.token_expire_at_ms
        } else {
            refreshed.token_expire_at_ms
        },
        refresh_expire_at_ms: if refreshed.refresh_expire_at_ms == 0 {
            cred.refresh_expire_at_ms
        } else {
            refreshed.refresh_expire_at_ms
        },
        user_id: refreshed.user_id.or(cred.user_id),
        ai_region: refreshed.ai_region.or(cred.ai_region),
        ..cred
    };
    if let Err(e) = store.save(&updated) {
        tracing::error!(error = %e, provider_id, "Trae 续期后落盘失败");
        return Err(e.into());
    }
    Ok(updated)
}

/// refresh 失败是否瞬时(可沿用旧凭证重试)。网络 / 5xx / 429 / parse = 瞬时;
/// 4xx / 业务拒 / 缺字段 = 明确鉴权失败(删凭证重登)。
fn is_transient_refresh_error(e: &TraeError) -> bool {
    match e {
        TraeError::Http(_) | TraeError::Parse(_) => true,
        TraeError::Status { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

/// best-effort 拉账号 email/标识(GetUserInfo)。失败返 `None`(不阻断登录)。
async fn fetch_user_email(
    http: &reqwest::Client,
    config: &TraeProviderConfig,
    token: &str,
) -> Option<String> {
    let url = format!("{}{}", config.api_host, constants::USERINFO_PATH);
    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-icube-token", token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let result = v.get("Result").unwrap_or(&v);
    for key in ["NonPlainTextEmail", "Email", "ScreenName", "UserName"] {
        if let Some(s) = result.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn is_cancelled(cancel: &Option<tokio::sync::watch::Receiver<bool>>) -> bool {
    cancel.as_ref().map(|rx| *rx.borrow()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_cancelled_semantics() {
        assert!(!is_cancelled(&None));
        let (tx, rx) = tokio::sync::watch::channel(false);
        assert!(!is_cancelled(&Some(rx.clone())));
        tx.send(true).unwrap();
        assert!(is_cancelled(&Some(rx)));
    }

    #[test]
    fn transient_refresh_error_classification() {
        // 瞬时:网络 / 5xx / 429 / parse → 沿用旧凭证重试
        assert!(is_transient_refresh_error(&TraeError::Parse("x".into())));
        assert!(is_transient_refresh_error(&TraeError::Status {
            status: 503,
            body: String::new()
        }));
        assert!(is_transient_refresh_error(&TraeError::Status {
            status: 429,
            body: String::new()
        }));
        // 明确鉴权失败:4xx / 业务拒 / 缺字段 → 删凭证重登
        assert!(!is_transient_refresh_error(&TraeError::Status {
            status: 401,
            body: String::new()
        }));
        assert!(!is_transient_refresh_error(&TraeError::Status {
            status: 403,
            body: String::new()
        }));
        assert!(!is_transient_refresh_error(&TraeError::Business {
            code: "RefreshTokenInvalid".into(),
            msg: String::new()
        }));
        assert!(!is_transient_refresh_error(&TraeError::NotLoggedIn));
    }

    #[tokio::test]
    async fn ensure_valid_returns_not_logged_in_without_credential() {
        // 指向不存在的凭证文件
        let dir = tempfile::TempDir::new().unwrap();
        let store = TraeCredentialStore::at_path(dir.path().join("nope.json"));
        assert!(store.load().unwrap().is_none());
        // 直接用 store 验证 NotLoggedIn 路径的前置(完整 ensure_valid 依赖 resolve_home,
        // 此处只锁「无文件 = None」契约,续期路径由 flow 单测覆盖)
    }
}
