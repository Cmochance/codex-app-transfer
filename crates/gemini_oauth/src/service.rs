//! High-level "拿一个有效的 access_token" service —— 给 proxy / admin handler 调。
//!
//! 隐藏 [`TokenStore`] / [`refresh_access_token`] 协调细节,提供单一函数
//! [`ensure_valid_access_token`]:load → check expiry → refresh + persist 必要时 →
//! return access_token。
//!
//! ## Race / 并发
//!
//! 当前实现**没有跨进程 / 跨请求 mutex**。两个并发请求同时进 should_refresh()
//! true 分支 → 各自 refresh 一次,后者覆盖前者。Google `/token` endpoint
//! idempotent,refresh 操作本身不出错;但浪费一次网络往返 + 多写一次磁盘。
//! 影响小,后续若高并发可加 `tokio::sync::Mutex` 包 store。

use thiserror::Error;

use super::flow::{refresh_access_token, FlowError};
use super::token::{OauthToken, TokenError, TokenStore};

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
/// 失败语义:
/// - 文件不存在 → `NotLoggedIn`,调用方应触发 OAuth login flow
/// - 文件存在但 IO / JSON 错 → `Token` 包装(致命,不能用)
/// - refresh 调用失败 → `Refresh` 包装(可重试)
pub async fn ensure_valid_access_token(
    http: &reqwest::Client,
    store: &TokenStore,
) -> Result<String, ServiceError> {
    let token = store.load()?.ok_or(ServiceError::NotLoggedIn)?;
    if !token.should_refresh() {
        return Ok(token.access_token);
    }
    // 过期窗口内 — refresh + 持久化
    tracing::debug!(
        expiry_date = token.expiry_date,
        "OAuth token 过期窗口内,触发 refresh"
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
