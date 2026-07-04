//! QoderWork CN 账号登录(纯客户端 PKCE device flow)+ token 自动 refresh。
//!
//! 逆向自 `out/main/main.js` 的 `startDeviceFlow` / `pollDeviceToken` /
//! `refreshDeviceToken`(静态实证,未 live 验):
//!
//! ```text
//! ① 本地生成 PKCE(verifier / challenge=base64url(sha256(verifier)))+ nonce(uuid)
//!    + machine_id,拼 authUrl:
//!    https://qoder.com.cn/device/selectAccounts?challenge=&challenge_method=S256
//!      &nonce=&machine_id=&client_id=&redirect_uri=qoder-work-cn://
//! ② 浏览器打开 authUrl,用户登录选账号(服务端把 token 关联到 nonce)
//! ③ 轮询 GET https://openapi.qoder.com.cn/api/v1/deviceToken/poll?nonce=&verifier=&challenge_method=S256
//!    未完成 → 无 token(继续轮询);完成 → { token, refresh_token, expires_at, ... }
//! ④ refresh: POST openapi.qoder.com.cn/api/v1/deviceToken/refresh { refresh_token }
//! ```
//!
//! 与 workbuddy 的"服务端 state + 轮询"不同,QoderWork 是"客户端 PKCE + 轮询"——
//! 发起不请求服务端,nonce/PKCE 本地生成,故独立实现。**poll 的 pending wire
//! 语义(HTTP 码 / body 形态)静态拿不准**,轮询每轮 log 实际响应,首次真机登录后据 log 收敛。

use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;

use super::token::{unix_now_ms, QoderCredential, QoderCredentialStore, QoderTokenError};
use super::{
    qoder_machine_id, user_id_from_jwt, QODER_CLIENT_ID, QODER_OPENAPI_HOST, QODER_REDIRECT_URI,
    QODER_WEBSITE_HOST,
};

/// 轮询间隔 / 总超时(对齐桌面端体感)。
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum QoderError {
    #[error("home 目录不可用,无法定位 token store: {0}")]
    Token(#[from] QoderTokenError),
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("上游业务错误 status={status}: {msg}")]
    Business { status: u16, msg: String },
    #[error("响应缺字段: {0}")]
    Parse(&'static str),
    #[error("PKCE/RNG 失败: {0}")]
    Rng(String),
    #[error("登录超时(5 分钟未完成)")]
    Timeout,
    #[error("登录被取消")]
    Cancelled,
    #[error("未登录(无本地凭证)")]
    NotLoggedIn,
}

fn openapi_base() -> String {
    format!("https://{QODER_OPENAPI_HOST}")
}

/// `/deviceToken/poll` + `/refresh` 的 token 载荷(camelCase 与 snake 混用,宽松解析)。
#[derive(Deserialize, Default)]
struct TokenPayload {
    #[serde(default)]
    token: String,
    #[serde(default)]
    refresh_token: String,
    /// 绝对过期时刻(可能是 string 或 number,秒或毫秒);优先用 expires_in。
    #[serde(default)]
    expires_at: Option<serde_json::Value>,
    /// 相对过期(秒)。
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token_expires_at: Option<serde_json::Value>,
    #[serde(default)]
    nickname: Option<String>,
}

/// 把 `expires_at`(string|number,秒或 ms)/ `expires_in`(秒)归一成绝对 unix ms。
fn resolve_expiry_ms(expires_at: &Option<serde_json::Value>, expires_in: Option<i64>) -> i64 {
    if let Some(secs) = expires_in {
        return unix_now_ms() + secs.max(0) * 1000;
    }
    let Some(v) = expires_at else {
        return 0;
    };
    let n = match v {
        serde_json::Value::Number(num) => num.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    // 启发式:> 10^12 视为已是毫秒,否则秒 → ×1000。
    if n > 1_000_000_000_000 {
        n
    } else {
        n * 1000
    }
}

fn payload_to_cred(t: TokenPayload, machine_id: String) -> QoderCredential {
    let now = unix_now_ms();
    let uid = user_id_from_jwt(&t.token);
    QoderCredential {
        expiry_date: resolve_expiry_ms(&t.expires_at, t.expires_in),
        refresh_expiry_date: resolve_expiry_ms(&t.refresh_token_expires_at, None),
        personal_token: t.token,
        refresh_token: t.refresh_token,
        obtained_at_ms: now,
        machine_id: Some(machine_id),
        nickname: t.nickname,
        uid,
        // org 在 run_qoder_login 出站前经 /userinfo 补(refresh 保留旧值,见 pool.rs ensure_valid)。
        organization_id: None,
        organization_tags: None,
    }
}

/// 生成 PKCE 对:`verifier = base64url(48 随机字节)`,`challenge = base64url(sha256(verifier))`。
fn generate_pkce() -> Result<(String, String), QoderError> {
    let mut vbytes = [0u8; 48];
    getrandom::getrandom(&mut vbytes).map_err(|e| QoderError::Rng(e.to_string()))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vbytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// 拼 device flow 的 authUrl(`qoder.com.cn/device/selectAccounts`,query 自动 URL 编码)。
fn build_auth_url(challenge: &str, nonce: &str, machine_id: &str) -> Result<String, QoderError> {
    let mut url = reqwest::Url::parse(&format!(
        "https://{QODER_WEBSITE_HOST}/device/selectAccounts"
    ))
    .map_err(|_| QoderError::Parse("authUrl base"))?;
    url.query_pairs_mut()
        .append_pair("challenge", challenge)
        .append_pair("challenge_method", "S256")
        .append_pair("nonce", nonce)
        .append_pair("machine_id", machine_id)
        .append_pair("client_id", QODER_CLIENT_ID)
        .append_pair("redirect_uri", QODER_REDIRECT_URI);
    Ok(url.to_string())
}

/// 跑完整账号登录:本地生成 PKCE/nonce → 回调 `on_auth_url`(call site 开 webview)→
/// 轮询 deviceToken/poll。返回凭证(由 call site 落盘)。`cancel` 置 true 中断。
pub async fn run_qoder_login(
    http: &reqwest::Client,
    on_auth_url: impl Fn(&str),
    mut cancel: Option<watch::Receiver<bool>>,
) -> Result<QoderCredential, QoderError> {
    let (verifier, challenge) = generate_pkce()?;
    let nonce = super::uuid_v4();
    // per-account 设备隔离:每次登录**新生成** machine_id(而非全局共用),让池内各账号有独立
    // `Cosy-MachineId` → 网关不把多账号看作同一设备(降低多号被风控关联/连坐 ban)。该 id 绑进
    // authUrl(device token 由 PKCE 与之绑定)→ payload_to_cred 落盘 → 签名全程用同一个,三处一致。
    // 全局 `qoder_machine_id()` 仅留作 refresh/migrate 兜底(见 pool.rs / :335)。
    let machine_id = super::uuid_v4();

    // ① 拼 authUrl,交 call site 打开
    let auth_url = build_auth_url(&challenge, &nonce, &machine_id)?;
    on_auth_url(&auth_url);

    // ② 轮询 token
    let poll_url = format!("{}/api/v1/deviceToken/poll", openapi_base());
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    loop {
        if is_cancelled(&mut cancel) {
            return Err(QoderError::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(QoderError::Timeout);
        }
        let resp = http
            .get(&poll_url)
            .query(&[
                ("nonce", nonce.as_str()),
                ("verifier", verifier.as_str()),
                ("challenge_method", "S256"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // 每轮 log 真实响应(截断 + 不含 token 明文)供首次真机登录后收敛 pending 语义。
        tracing::info!(
            http_status = status.as_u16(),
            body_snippet = %redact_snippet(&body),
            "[Qoder] deviceToken/poll 轮询响应"
        );

        if status.is_success() {
            if let Ok(payload) = serde_json::from_str::<TokenPayload>(&body) {
                if !payload.token.is_empty() {
                    let mut cred = payload_to_cred(payload, machine_id);
                    // device token 不透明(`dt-` 前缀,非 JWT),`user_id_from_jwt` 恒 None →
                    // uid 解不出。而账号池按 uid keying,缺 uid 则 `add_account` 直接
                    // `PoolError::Account("uid")` 失败(多账号池对所有账号不可用)。故出站前
                    // 走 `GET /userinfo` 补齐 uid(顺带回填 nickname);userinfo 拿不到则登录失败。
                    if cred.uid.is_none() {
                        let info = fetch_user_info(http, &cred.personal_token).await?;
                        cred.uid = Some(info.uid);
                        if cred.nickname.is_none() {
                            cred.nickname = info.nickname;
                        }
                        // org 一并缓存(uid/org 都是账号静态属性)→ 出站签名复用,免每请求打
                        // 账号级 /userinfo(见 forward.rs send_qoder_cosy)。个人号 org_id 为空串。
                        cred.organization_id = Some(info.organization_id);
                        cred.organization_tags = Some(info.organization_tags);
                    }
                    return Ok(cred);
                }
            }
            // 2xx 但无 token = 尚未登录完成,继续轮询。
        } else if matches!(status.as_u16(), 401 | 403) {
            // 明确的鉴权/禁止 = 硬错误(bad client_id / 参数),不再空转。
            return Err(QoderError::Business {
                status: status.as_u16(),
                msg: redact_snippet(&body),
            });
        }
        // 其它非 2xx(404/425/429/5xx 等)按"尚未就绪 / 瞬态"继续轮询,直到超时。

        if wait_or_cancel(POLL_INTERVAL, &mut cancel).await {
            return Err(QoderError::Cancelled);
        }
    }
}

/// 用 refresh token 换一组新 device token(`POST /deviceToken/refresh { refresh_token }`)。
pub async fn refresh_qoder_token(
    http: &reqwest::Client,
    refresh_token: &str,
    machine_id: String,
) -> Result<QoderCredential, QoderError> {
    let resp = http
        .post(format!("{}/api/v1/deviceToken/refresh", openapi_base()))
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(QoderError::Business {
            status: status.as_u16(),
            msg: redact_snippet(&body),
        });
    }
    let payload: TokenPayload =
        serde_json::from_str(&body).map_err(|_| QoderError::Parse("refresh payload"))?;
    if payload.token.is_empty() {
        return Err(QoderError::Parse("refresh: missing token"));
    }
    Ok(payload_to_cred(payload, machine_id))
}

/// QoderWork 用户信息 —— 阶段二 WASM 签名需要的 `uid` + 组织字段。
/// 来自 `GET openapi.qoder.com.cn/api/v1/userinfo`(已真机实测 200,MOC-297)。
#[derive(Debug, Clone, Default)]
pub struct QoderUserInfo {
    /// 用户唯一 id(响应 `id`/`user_id`/`uid`,喂 WASM 作 `Cosy-User`)。
    pub uid: String,
    /// 组织 id(个人账号为空串)。
    pub organization_id: String,
    /// 组织标签(WASM `user_info.organization_tags` 必须是数组)。
    pub organization_tags: Vec<String>,
    /// 昵称(`name`),UI 展示用。
    pub nickname: Option<String>,
}

/// 拉取用户信息(`GET /api/v1/userinfo` + `Authorization: Bearer <device_token>`)。
/// 阶段二出站前调,把 `uid`/`org` 喂给 [`codex_app_transfer_qoder_auth`] 生成签名。
pub async fn fetch_user_info(
    http: &reqwest::Client,
    device_token: &str,
) -> Result<QoderUserInfo, QoderError> {
    let resp = http
        .get(format!("{}/api/v1/userinfo", openapi_base()))
        .header("Accept", "application/json")
        .bearer_auth(device_token)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(QoderError::Business {
            status: status.as_u16(),
            msg: redact_snippet(&body),
        });
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| QoderError::Parse("userinfo payload"))?;
    let uid = ["id", "user_id", "uid"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.is_empty())
        .ok_or(QoderError::Parse("userinfo: missing uid"))?
        .to_string();
    let organization_id = v
        .get("organization_id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let organization_tags = v
        .get("organization_tags")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let nickname = v.get("name").and_then(|x| x.as_str()).map(String::from);
    Ok(QoderUserInfo {
        uid,
        organization_id,
        organization_tags,
        nickname,
    })
}

/// 进程级 single-flight refresh 锁(防并发 refresh 互相作废轮换的 refresh_token)。
fn refresh_mutex() -> &'static tokio::sync::Mutex<()> {
    static MUTEX: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 请求前确保有效 personal_token:load → 过期则 refresh + 落盘 → 返回 personal_token。
/// 无本地凭证 → `NotLoggedIn`。(阶段二换 jobToken 前调。)
pub async fn ensure_valid_personal_token(
    http: &reqwest::Client,
    store: &QoderCredentialStore,
) -> Result<String, QoderError> {
    let cred = store.load()?.ok_or(QoderError::NotLoggedIn)?;
    if !cred.should_refresh() {
        return Ok(cred.personal_token);
    }
    let _guard = refresh_mutex().lock().await;
    // 拿锁后重 load:并发请求可能已 refresh 过。
    let cred = store.load()?.ok_or(QoderError::NotLoggedIn)?;
    if !cred.should_refresh() {
        return Ok(cred.personal_token);
    }
    let machine_id = cred.machine_id.clone().unwrap_or_else(qoder_machine_id);
    let mut fresh = refresh_qoder_token(http, &cred.refresh_token, machine_id).await?;
    if fresh.nickname.is_none() {
        fresh.nickname = cred.nickname.clone();
    }
    store.save(&fresh)?;
    Ok(fresh.personal_token)
}

/// body 截断到 200 字符 + 抹掉可能的长 token 串(粗粒度:形似 JWT / 长 base64 的段替换)。
fn redact_snippet(body: &str) -> String {
    let t: String = body.chars().take(200).collect();
    // 简单脱敏:把 "token":"..." 的值替换掉(pending 响应通常没有,错误响应可能回显)。
    let re_keys = ["token", "refresh_token", "personal_token", "access_token"];
    let mut out = t;
    for k in re_keys {
        if let Some(idx) = out.find(&format!("\"{k}\"")) {
            // 找到 key 后把其后的值截断标记(不精确解析,仅防长串泄漏)。
            let tail = &out[idx..];
            if tail.len() > 40 {
                out = format!("{}…<redacted>", &out[..idx + k.len() + 4]);
                break;
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = generate_pkce().unwrap();
        // challenge = base64url(sha256(verifier))
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expect);
        assert!(!verifier.contains('='), "base64url 无 padding");
        assert!(
            !challenge.contains('+') && !challenge.contains('/'),
            "url-safe"
        );
    }

    #[test]
    fn auth_url_has_all_params() {
        let url = build_auth_url("chal", "non", "mid").unwrap();
        assert!(url.starts_with("https://qoder.com.cn/device/selectAccounts?"));
        assert!(url.contains("challenge=chal"));
        assert!(url.contains("challenge_method=S256"));
        assert!(url.contains("nonce=non"));
        assert!(url.contains("machine_id=mid"));
        assert!(url.contains(&format!("client_id={QODER_CLIENT_ID}")));
        // redirect_uri 被 URL 编码(qoder-work-cn%3A%2F%2F)
        assert!(url.contains("redirect_uri=qoder-work-cn%3A%2F%2F"));
    }

    #[test]
    fn resolve_expiry_prefers_expires_in() {
        let now = unix_now_ms();
        let e = resolve_expiry_ms(&Some(serde_json::json!(0)), Some(3600));
        assert!(e >= now + 3_600_000 - 1000 && e <= now + 3_600_000 + 1000);
    }

    #[test]
    fn resolve_expiry_seconds_string() {
        // 绝对秒级时间戳字符串 → ×1000
        let e = resolve_expiry_ms(&Some(serde_json::json!("1900000000")), None);
        assert_eq!(e, 1_900_000_000_000);
    }

    #[test]
    fn resolve_expiry_ms_passthrough() {
        let e = resolve_expiry_ms(&Some(serde_json::json!(1_900_000_000_000i64)), None);
        assert_eq!(e, 1_900_000_000_000);
    }
}
