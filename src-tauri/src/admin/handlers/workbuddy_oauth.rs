//! `/api/workbuddy-oauth/*` admin handlers — WorkBuddy(腾讯 CodeBuddy)账号登录
//! (external-link 轮询式 OAuth)登录 / 状态 / 注销 / 取消。
//!
//! 跟 [`super::zai_oauth`] / [`super::trae_oauth`] **并行**,但 WorkBuddy wire 更简单:
//! 1. **单账号**:一个本地 `workbuddy-oauth.json`,无 provider id keying(对齐"一个网关
//!    一套登录态")。前端传的 `providerId` 接受但忽略。
//! 2. **轮询式**(非 loopback):`run_workbuddy_login` 请求 state → 回调里开内置 webview
//!    加载 authUrl → 轮询 `/auth/token` 拿凭证。用户关窗 = 取消。
//! 3. **有 refresh**:access token 过期前 5min 由 `ensure_valid_workbuddy_token` 自动
//!    refresh(`X-Refresh-Token` 头),不像 zai/trae 过期即重登。
//!
//! ## 路由
//! - `GET    /api/workbuddy-oauth/status`
//! - `POST   /api/workbuddy-oauth/login`
//! - `DELETE /api/workbuddy-oauth/login/cancel`
//! - `DELETE /api/workbuddy-oauth/logout`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::workbuddy::{
    run_workbuddy_login, WorkbuddyCredentialStore, WorkbuddyError,
};
use serde_json::json;
use tokio::sync::watch;

use super::super::state::AdminState;
use super::common::err;
use crate::web_session_quota;

/// 内置登录 webview 窗口 label。
const WORKBUDDY_LOGIN_WIN: &str = "workbuddy-oauth-login";

// ── 进程级 cancel slot(独立于 zai / trae / gemini-cli)─────────────────

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
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn cleanup_slot(my_epoch: u64) {
    let mut slot = lock_cancel_slot();
    if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
        slot.take();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CancelOutcome {
    pub cancelled: bool,
    pub cancelled_epoch: Option<u64>,
}

/// 取消 in-flight 登录(UI 关窗 / 新登录抢占 / 显式 cancel / app 退出)。返回是否取消 +
/// 被取消的 epoch,供 app 退出路径 `wait_for_login_epoch_complete` 等该 task 真退出。
pub fn cancel_in_flight_login() -> CancelOutcome {
    let mut guard = lock_cancel_slot();
    if let Some((epoch, sender)) = guard.take() {
        let _ = sender.send(true);
        CancelOutcome {
            cancelled: true,
            cancelled_epoch: Some(epoch),
        }
    } else {
        CancelOutcome {
            cancelled: false,
            cancelled_epoch: None,
        }
    }
}

/// 每个 login_handler 完成(成功/失败/取消)时,经此 channel 广播自己的 epoch;
/// app 退出路径 [`wait_for_login_epoch_complete`] 据此等 in-flight 登录真跑完。
fn login_done_channel() -> &'static (watch::Sender<u64>, watch::Receiver<u64>) {
    static C: OnceLock<(watch::Sender<u64>, watch::Receiver<u64>)> = OnceLock::new();
    C.get_or_init(|| watch::channel(0))
}

/// app 退出时等当前 in-flight 登录跑完(避免 OAuth 流程被硬切留半截 ghost 凭证)。
pub async fn wait_for_login_epoch_complete(target_epoch: u64) {
    let mut rx = login_done_channel().1.clone();
    loop {
        if *rx.borrow() >= target_epoch {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// login_handler 持有它;返回(含 early return / panic)时 Drop 广播本次 epoch 已完成。
struct LoginDoneGuard {
    epoch: u64,
}
impl Drop for LoginDoneGuard {
    fn drop(&mut self) {
        let (tx, _) = login_done_channel();
        let my = self.epoch;
        let _ = tx.send_if_modified(|cur| {
            if my > *cur {
                *cur = my;
                true
            } else {
                false
            }
        });
    }
}

// ── shared HTTP client(独立 pool)──────────────────────────────────

fn shared_workbuddy_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // 单次 HTTP 上限;登录整体超时由 run_workbuddy_login 内部 5min 轮询窗口控。
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "WORKBUDDY_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "workbuddy reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("workbuddy HTTP client init failed (TLS/resolver); 见 WORKBUDDY_HTTP_CLIENT_BUILDER_FAILED 日志"),
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/workbuddy-oauth/status", get(status_handler))
        .route("/api/workbuddy-oauth/login", post(login_handler))
        .route(
            "/api/workbuddy-oauth/login/cancel",
            delete(cancel_login_handler),
        )
        .route("/api/workbuddy-oauth/logout", delete(logout_handler))
}

async fn cancel_login_handler() -> impl IntoResponse {
    let outcome = cancel_in_flight_login();
    web_session_quota::close_external_login_window(WORKBUDDY_LOGIN_WIN);
    if outcome.cancelled {
        tracing::info!("workbuddy OAuth login cancelled by user request");
    }
    Json(json!({ "cancelled": outcome.cancelled })).into_response()
}

async fn status_handler() -> impl IntoResponse {
    let store = match WorkbuddyCredentialStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("home directory unavailable: {e}"),
            )
            .into_response()
        }
    };
    match store.load() {
        Ok(None) => Json(json!({ "loggedIn": false })).into_response(),
        Ok(Some(cred)) => Json(json!({
            "loggedIn": true,
            "nickname": cred.nickname,
            "userId": cred.uid,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("workbuddy credential store load: {e}"),
        )
        .into_response(),
    }
}

async fn login_handler() -> impl IntoResponse {
    let my_epoch = next_epoch();
    // Drop 时广播本次 epoch 完成,让 app 退出路径的 wait_for_login_epoch_complete 能等到。
    let _done_guard = LoginDoneGuard { epoch: my_epoch };
    let http = match shared_workbuddy_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    // 注册 cancel sender + 抢占语义(新登录抢占任何 in-flight)。
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight workbuddy OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // on_auth_url:拿到 authUrl 后开内置 webview 登录窗(不开外部浏览器,对齐 trae/MiMo)。
    let on_auth_url = |url: &str| {
        tracing::info!(
            auth_url = url,
            "workbuddy OAuth authUrl 已生成 — 内置 webview 打开"
        );
        let url = url.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = web_session_quota::open_external_login_window(
                WORKBUDDY_LOGIN_WIN,
                "WorkBuddy 登录",
                &url,
                (520.0, 720.0),
            )
            .await
            {
                tracing::warn!(error = %e, "[WorkBuddy] 打开内置登录窗失败");
            }
        });
    };

    // 用户手动关登录窗 = 取消:登录结束前轮询窗口,曾开过又关掉则触发 cancel(对齐 Trae)。
    // 否则关窗后 run_workbuddy_login 会傻等满 LOGIN_TIMEOUT(5min),POST /login + UI「登录中」
    // 一直卡住(codex review P2)。
    let login_done = Arc::new(AtomicBool::new(false));
    {
        let done = login_done.clone();
        tauri::async_runtime::spawn(async move {
            let mut seen_open = false;
            loop {
                tokio::time::sleep(Duration::from_millis(800)).await;
                if done.load(Ordering::Relaxed) {
                    break; // 登录已结束(成功/失败/超时),停止监视
                }
                if web_session_quota::external_login_window_open(WORKBUDDY_LOGIN_WIN) {
                    seen_open = true;
                } else if seen_open {
                    tracing::info!("[WorkBuddy] 登录窗被用户关闭 → 取消登录");
                    cancel_in_flight_login();
                    break;
                }
            }
        });
    }

    let result = run_workbuddy_login(http, on_auth_url, Some(cancel_rx)).await;
    login_done.store(true, Ordering::Relaxed);
    cleanup_slot(my_epoch);
    web_session_quota::close_external_login_window(WORKBUDDY_LOGIN_WIN);

    match result {
        Ok(cred) => {
            // 落盘到 workbuddy-oauth.json(forward.rs 请求时 load + 自动 refresh)。
            let store = match WorkbuddyCredentialStore::from_home_env() {
                Ok(s) => s,
                Err(e) => {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("home directory unavailable: {e}"),
                    )
                    .into_response()
                }
            };
            if let Err(e) = store.save(&cred) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("workbuddy credential save failed: {e}"),
                )
                .into_response();
            }
            Json(json!({
                "loggedIn": true,
                "nickname": cred.nickname,
                "userId": cred.uid,
                "obtainedAt": cred.obtained_at_ms,
            }))
            .into_response()
        }
        Err(WorkbuddyError::Cancelled) => {
            tracing::info!("workbuddy OAuth login cancelled — 不落盘");
            Json(json!({ "loggedIn": false, "cancelled": true })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "workbuddy OAuth login 失败");
            Json(json!({ "loggedIn": false, "error": e.to_string() })).into_response()
        }
    }
}

async fn logout_handler() -> impl IntoResponse {
    web_session_quota::close_external_login_window(WORKBUDDY_LOGIN_WIN);
    let store = match WorkbuddyCredentialStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("home directory unavailable: {e}"),
            )
            .into_response()
        }
    };
    if let Err(e) = store.delete() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("workbuddy credential delete failed: {e}"),
        )
        .into_response();
    }
    Json(json!({ "loggedIn": false })).into_response()
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
        assert!(!cancel_in_flight_login().cancelled);
    }
}
