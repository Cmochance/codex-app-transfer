//! `/api/grok-build-oauth/*` admin handlers — grok build(xAI grok CLI 编码后端)账号登录
//! (OAuth2 device authorization grant,RFC 8628)登录 / 状态 / 注销 / 取消。
//!
//! 跟 [`super::qoder_oauth`] 并行但**单账号**(非账号池,对齐 antigravity 单账号):
//! 1. **device flow**:`run_grok_build_login` → `accounts.x.ai/oauth2/device/code` 拿
//!    `user_code` + `verification_uri_complete` → 回调开内置 webview 加载授权页(用户授权)
//!    → 轮询 `oauth2/token` 拿凭证。用户关窗 = 取消。
//! 2. **单账号落盘**:凭证存 `~/.codex-app-transfer/grok-build-oauth.json`(覆盖式,一账号)。
//! 3. **有 refresh**:access token 过期前 5min 自动 refresh(出站前 `ensure_valid_grok_build_token`)。
//!
//! ## 路由
//! - `GET    /api/grok-build-oauth/status`        当前登录态(email / 过期时刻)
//! - `POST   /api/grok-build-oauth/login`         发起登录(长阻塞至完成/取消)
//! - `DELETE /api/grok-build-oauth/login/cancel`  取消 in-flight 登录
//! - `DELETE /api/grok-build-oauth/logout`        注销(删凭证)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::{
    grok_build_logout, run_grok_build_login, GrokBuildCredentialStore, GrokBuildError,
};
use serde_json::json;
use tokio::sync::watch;

use super::super::state::AdminState;
use super::common::err;
use crate::web_session_quota;

/// 内置登录 webview 窗口 label。
const GROK_BUILD_LOGIN_WIN: &str = "grok-build-oauth-login";

// ── 进程级 cancel slot(独立于 qoder / workbuddy / trae / zai / gemini-cli)──────

fn cancel_slot() -> &'static Mutex<Option<(u64, watch::Sender<bool>)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, watch::Sender<bool>)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn lock_cancel_slot() -> std::sync::MutexGuard<'static, Option<(u64, watch::Sender<bool>)>> {
    cancel_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn next_epoch() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn cleanup_slot(my_epoch: u64) {
    let mut slot = lock_cancel_slot();
    if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
        slot.take();
    }
}

/// 取消 in-flight 登录(UI 关窗 / 新登录抢占 / 显式 cancel / app 退出)。返回是否有在飞登录被取消。
pub fn cancel_in_flight_login() -> bool {
    let mut guard = lock_cancel_slot();
    if let Some((_, sender)) = guard.take() {
        let _ = sender.send(true);
        true
    } else {
        false
    }
}

// ── shared HTTP client(独立 pool)──────────────────────────────────

fn shared_grok_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "GROK_BUILD_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "grok build reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("grok build HTTP client init failed (TLS/resolver); 见 GROK_BUILD_HTTP_CLIENT_BUILDER_FAILED 日志"),
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/grok-build-oauth/status", get(status_handler))
        .route("/api/grok-build-oauth/login", post(login_handler))
        .route(
            "/api/grok-build-oauth/login/cancel",
            delete(cancel_login_handler),
        )
        .route("/api/grok-build-oauth/logout", delete(logout_handler))
}

async fn status_handler() -> impl IntoResponse {
    let store = match GrokBuildCredentialStore::single() {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("grok build status: {e}"),
            )
            .into_response()
        }
    };
    match store.load() {
        Ok(Some(cred)) => Json(json!({
            "loggedIn": true,
            "email": cred.email,
            "userId": cred.user_id,
            "expiryDate": cred.expiry_date,
        }))
        .into_response(),
        Ok(None) => Json(json!({ "loggedIn": false })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("grok build status load: {e}"),
        )
        .into_response(),
    }
}

async fn cancel_login_handler() -> impl IntoResponse {
    let cancelled = cancel_in_flight_login();
    web_session_quota::close_external_login_window(GROK_BUILD_LOGIN_WIN);
    if cancelled {
        tracing::info!("grok build OAuth login cancelled by user request");
    }
    Json(json!({ "cancelled": cancelled })).into_response()
}

async fn logout_handler() -> impl IntoResponse {
    match grok_build_logout() {
        Ok(()) => Json(json!({ "loggedIn": false })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("grok build logout failed: {e}"),
        )
        .into_response(),
    }
}

async fn login_handler() -> impl IntoResponse {
    let my_epoch = next_epoch();
    let http = match shared_grok_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    // 注册 cancel sender + 抢占语义(新登录抢占任何 in-flight)。
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight grok build OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // on_device_auth:拿到 user_code + 授权 URL 后开内置 webview 加载授权页
    // (verification_uri_complete 已内嵌 user_code,用户直接确认授权)。
    let on_device_auth = |device: &codex_app_transfer_gemini_oauth::DeviceAuthResponse| {
        let url = device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone());
        tracing::info!(
            user_code = %device.user_code,
            verification_uri = %url,
            "grok build device 授权已发起 — 内置 webview 打开授权页"
        );
        tauri::async_runtime::spawn(async move {
            if let Err(e) = web_session_quota::open_external_login_window(
                GROK_BUILD_LOGIN_WIN,
                "Grok Build 登录",
                &url,
                (520.0, 720.0),
            )
            .await
            {
                tracing::warn!(error = %e, "[GrokBuild] 打开内置登录窗失败");
            }
        });
    };

    // 用户手动关登录窗 = 取消(否则轮询会傻等到 device_code 过期)。
    let login_done = Arc::new(AtomicBool::new(false));
    {
        let done = login_done.clone();
        tauri::async_runtime::spawn(async move {
            let mut seen_open = false;
            loop {
                tokio::time::sleep(Duration::from_millis(800)).await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                if web_session_quota::external_login_window_open(GROK_BUILD_LOGIN_WIN) {
                    seen_open = true;
                } else if seen_open {
                    tracing::info!("[GrokBuild] 登录窗被用户关闭 → 取消登录");
                    cancel_in_flight_login();
                    break;
                }
            }
        });
    }

    let result = run_grok_build_login(http, on_device_auth, Some(cancel_rx)).await;
    login_done.store(true, Ordering::Relaxed);
    cleanup_slot(my_epoch);
    web_session_quota::close_external_login_window(GROK_BUILD_LOGIN_WIN);

    match result {
        Ok(cred) => Json(json!({
            "loggedIn": true,
            "email": cred.email,
            "userId": cred.user_id,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(GrokBuildError::Cancelled) => {
            tracing::info!("grok build OAuth login cancelled — 不落盘");
            Json(json!({ "loggedIn": false, "cancelled": true })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "grok build OAuth login 失败");
            Json(json!({ "loggedIn": false, "error": e.to_string() })).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_compile() {
        let _ = routes();
    }

    #[test]
    fn cancel_with_no_in_flight_returns_false() {
        let _ = lock_cancel_slot().take();
        assert!(!cancel_in_flight_login());
    }
}
