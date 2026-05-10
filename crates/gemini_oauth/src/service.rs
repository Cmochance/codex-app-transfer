//! High-level "拿一个有效的 access_token" service —— 给 proxy / admin handler 调。
//!
//! 隐藏 [`TokenStore`] / [`refresh_access_token`] 协调细节,提供单一函数
//! [`ensure_valid_access_token`]:load → check expiry → single-flight refresh +
//! persist 必要时 → return access_token。
//!
//! ## Single-flight refresh(2026-05-11 critical 修)
//!
//! 原版无 mutex,两个并发请求都进 should_refresh() true 分支时各自调 refresh,
//! Google 默认**不 rotate refresh_token**但在某些 edge case(refresh_token 已被
//! 用过一次后再用)会返 `invalid_grant`,**永久 brick** 这个 token,用户必须
//! 重新 OAuth 登录。
//!
//! 修法:用进程级 `tokio::sync::Mutex` 把 load → refresh → save 整个 sequence
//! 原子化。第二个并发请求拿到锁后会**重新 load**,看到第一次 refresh 的新
//! token + should_refresh()=false → 直接返新 access_token,**完全跳过自己的
//! refresh 调用**,根治 invalid_grant brick + 浪费 RTT。

use std::sync::OnceLock;

use thiserror::Error;
use tokio::sync::Mutex;

use super::flow::{refresh_access_token, FlowError};
use super::token::{OauthToken, TokenError, TokenStore};

/// 进程级 refresh mutex —— 同一进程内任何 `ensure_valid_access_token` 并发调用
/// 都串行进 critical section。Mutex 不绑定到具体 store path 上(全局 single-flight)
/// 因为本项目只用一份 token store(`~/.codex-app-transfer/gemini-oauth.json`)。
fn refresh_mutex() -> &'static Mutex<()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("用户未登录(token 文件不存在或已被清除)— 请触发 OAuth login flow")]
    NotLoggedIn,
    #[error("token store 错误: {0}")]
    Token(#[from] TokenError),
    #[error("token refresh 失败: {0}")]
    Refresh(#[from] FlowError),
}

/// 加载持久化 token,过期前 60s 自动 refresh + 持久化,返回当前可用 access_token。
///
/// **Single-flight refresh**(2026-05-11):并发请求由 [`refresh_mutex`] 串行化,
/// 第二个请求拿到锁后**重新 load** token,如果第一次已经 refresh 过(should_refresh=
/// false)→ 直接返新 access_token,**跳过自己的 refresh 调用**,防 Google
/// `invalid_grant` 永久 brick token。
///
/// 失败语义:
/// - 文件不存在 → `NotLoggedIn`,调用方应触发 OAuth login flow
/// - 文件存在但 IO / JSON 错 → `Token` 包装(致命,不能用)
/// - refresh 调用失败 → `Refresh` 包装(`invalid_grant` 等;调用方应映射到 401 +
///   `refresh_token_revoked` code 提示用户重登)
pub async fn ensure_valid_access_token(
    http: &reqwest::Client,
    store: &TokenStore,
) -> Result<String, ServiceError> {
    // 第一次 load(无锁):大多数情况 token 没过期,直接返避免 mutex contention
    let token = store.load()?.ok_or(ServiceError::NotLoggedIn)?;
    if !token.should_refresh() {
        return Ok(token.access_token);
    }

    // 过期窗口内 — 进 single-flight critical section
    let _guard = refresh_mutex().lock().await;

    // 拿到锁后**重新 load** —— 若上一并发请求已 refresh 过,这里直接复用新 token
    let token = store.load()?.ok_or(ServiceError::NotLoggedIn)?;
    if !token.should_refresh() {
        tracing::debug!("single-flight: 并发请求已 refresh,复用新 token 跳过自己的 refresh 调用");
        return Ok(token.access_token);
    }

    tracing::debug!(
        expiry_date = token.expiry_date,
        "OAuth token 过期窗口内,触发 refresh(single-flight critical section)"
    );
    let refreshed = refresh_access_token(
        http,
        &token.refresh_token,
        token.id_token.clone(),
        token.email.clone(),
        token.project_id.clone(),
        Some(token.scope.clone()),
    )
    .await?;
    store.save(&refreshed)?;
    Ok(refreshed.access_token)
}

/// 把 OAuth flow 拿到的 token 持久化 — 包装 `store.save`,加 `tracing` 日志。
/// 通常 admin handler OAuth login 完成 + cloud_code bootstrap 写完 project_id 后
/// 调用一次落盘。
pub fn persist_token(store: &TokenStore, token: &OauthToken) -> Result<(), TokenError> {
    store.save(token)?;
    tracing::info!(
        email = token.email.as_deref().unwrap_or(""),
        project_id = token.project_id.as_deref().unwrap_or(""),
        "OAuth token 持久化完成"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn unix_now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn fresh_token(expiry_offset_secs: i64) -> OauthToken {
        OauthToken {
            access_token: "ya29.fresh-access".into(),
            refresh_token: "1//refresh-1".into(),
            token_type: "Bearer".into(),
            expiry_date: unix_now_ms() + expiry_offset_secs * 1000,
            scope: "test-scope".into(),
            id_token: Some("ey.id".into()),
            email: Some("u@example.com".into()),
            project_id: Some("proj-99".into()),
        }
    }

    #[tokio::test]
    async fn returns_existing_access_token_when_not_expiring() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_path(dir.path().join("token.json"));
        let token = fresh_token(3600); // 1 小时后过期 — 不该 refresh
        store.save(&token).unwrap();

        let http = reqwest::Client::new();
        let result = ensure_valid_access_token(&http, &store).await.unwrap();
        assert_eq!(result, "ya29.fresh-access");

        // 没改文件 — refresh 没跑
        let reloaded = store.load().unwrap().unwrap();
        assert_eq!(reloaded.access_token, "ya29.fresh-access");
    }

    #[tokio::test]
    async fn returns_not_logged_in_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_path(dir.path().join("nonexistent.json"));

        let http = reqwest::Client::new();
        let err = ensure_valid_access_token(&http, &store).await.unwrap_err();
        assert!(matches!(err, ServiceError::NotLoggedIn));
    }

    #[tokio::test]
    async fn persist_token_logs_metadata() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_path(dir.path().join("token.json"));
        let token = fresh_token(3600);

        persist_token(&store, &token).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.email.as_deref(), Some("u@example.com"));
        assert_eq!(loaded.project_id.as_deref(), Some("proj-99"));
    }
}
