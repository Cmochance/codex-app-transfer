//! `/api/qoder-oauth/*` admin handlers — QoderWork CN(阿里 Qoder 系)账号登录
//! (纯客户端 PKCE device flow 轮询式)登录 / 状态 / 注销 / 取消。
//!
//! 跟 [`super::workbuddy_oauth`] 并行,但**单账号**(非账号池):
//! 1. **轮询式**(非 loopback):`run_qoder_login` 本地生成 PKCE+nonce → 回调里开内置
//!    webview 加载 authUrl(`qoder.com.cn/device/selectAccounts`)→ 轮询
//!    `openapi.qoder.com.cn/api/v1/deviceToken/poll` 拿凭证。用户关窗 = 取消。
//! 2. **单账号落盘**:凭证存 `~/.codex-app-transfer/qoder-oauth.json`(覆盖式,一账号)。
//! 3. **有 refresh**:personal_token 过期前 5min 自动 refresh(阶段二模型出站前调
//!    `ensure_valid_personal_token`)。
//!
//! ## 路由
//! - `GET    /api/qoder-oauth/status`        当前登录态
//! - `POST   /api/qoder-oauth/login`         发起登录(长阻塞至完成/取消)
//! - `DELETE /api/qoder-oauth/login/cancel`  取消 in-flight 登录
//! - `DELETE /api/qoder-oauth/logout`        注销(删凭证)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::qoder::{pool, run_qoder_login, QoderError};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use super::super::state::AdminState;
use super::common::err;
use crate::web_session_quota;

/// 内置登录 webview 窗口 label。
const QODER_LOGIN_WIN: &str = "qoder-oauth-login";

// ── 进程级 cancel slot(独立于 workbuddy / trae / zai / gemini-cli）──────

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

#[derive(Debug, Clone, Copy)]
pub struct CancelOutcome {
    pub cancelled: bool,
    pub cancelled_epoch: Option<u64>,
}

/// 取消 in-flight 登录(UI 关窗 / 新登录抢占 / 显式 cancel / app 退出)。
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

/// 每个 login_handler 完成时经此 channel 广播 epoch;app 退出路径据此等 in-flight 登录跑完。
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

// ── shared HTTP client（独立 pool）──────────────────────────────────

fn shared_qoder_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "QODER_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "qoder reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("qoder HTTP client init failed (TLS/resolver); 见 QODER_HTTP_CLIENT_BUILDER_FAILED 日志"),
    }
}

/// 多账号:所有路由按 `providerId` 隔离账号池(一个 qoder provider = 一个池)。
#[derive(Debug, Deserialize)]
struct ProviderIdQuery {
    #[serde(rename = "providerId", default)]
    provider_id: String,
}

/// 账号级操作(移除 / 切换)额外带目标账号 `uid`。
#[derive(Debug, Deserialize)]
struct AccountQuery {
    #[serde(rename = "providerId", default)]
    provider_id: String,
    #[serde(rename = "uid", default)]
    uid: String,
}

/// trim 后非空才有效;空 = provider 未保存,账号池无处安放。
fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        // status?providerId → 列池内账号(多账号);login?providerId → 加账号入池
        .route("/api/qoder-oauth/status", get(status_handler))
        .route("/api/qoder-oauth/login", post(login_handler))
        .route(
            "/api/qoder-oauth/login/cancel",
            delete(cancel_login_handler),
        )
        // account?providerId&uid → 移除单账号;switch?providerId&uid → 手动切当前服务账号
        .route("/api/qoder-oauth/account", delete(remove_account_handler))
        .route("/api/qoder-oauth/switch", post(switch_handler))
}

async fn status_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    // providerId 空(provider 未保存)→ 空池(前端显「先保存再添加账号」)。
    let Some(provider_id) = nonempty(&q.provider_id) else {
        return Json(json!({ "loggedIn": false, "accounts": [] })).into_response();
    };
    match pool::list_pool(provider_id) {
        Ok(accounts) => {
            let now = codex_app_transfer_gemini_oauth::qoder::token::unix_now_ms();
            let list: Vec<_> = accounts
                .iter()
                .map(|a| {
                    json!({
                        "uid": a.uid,
                        "display": a.display,
                        "nickname": a.nickname,
                        "isActive": a.is_active,
                        "exhausted": a.exhausted_until > now,
                        "exhaustedUntil": a.exhausted_until,
                    })
                })
                .collect();
            Json(json!({ "loggedIn": !list.is_empty(), "accounts": list })).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("qoder status: {e}"),
        )
        .into_response(),
    }
}

async fn cancel_login_handler() -> impl IntoResponse {
    let outcome = cancel_in_flight_login();
    web_session_quota::close_external_login_window(QODER_LOGIN_WIN);
    if outcome.cancelled {
        tracing::info!("qoder OAuth login cancelled by user request");
    }
    Json(json!({ "cancelled": outcome.cancelled })).into_response()
}

async fn remove_account_handler(Query(q): Query<AccountQuery>) -> impl IntoResponse {
    let (Some(provider_id), Some(uid)) = (nonempty(&q.provider_id), nonempty(&q.uid)) else {
        return err(StatusCode::BAD_REQUEST, "providerId / uid 必填".to_string()).into_response();
    };
    match pool::remove_account(provider_id, uid) {
        Ok(()) => Json(json!({ "removed": true })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("qoder remove account failed: {e}"),
        )
        .into_response(),
    }
}

async fn switch_handler(Query(q): Query<AccountQuery>) -> impl IntoResponse {
    let (Some(provider_id), Some(uid)) = (nonempty(&q.provider_id), nonempty(&q.uid)) else {
        return err(StatusCode::BAD_REQUEST, "providerId / uid 必填".to_string()).into_response();
    };
    match pool::set_active(provider_id, uid) {
        Ok(()) => Json(json!({ "active": uid })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("qoder switch failed: {e}"),
        )
        .into_response(),
    }
}

async fn login_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    let Some(provider_id) = nonempty(&q.provider_id) else {
        return err(
            StatusCode::BAD_REQUEST,
            "providerId 必填(先保存 provider 再添加账号)".to_string(),
        )
        .into_response();
    };
    let provider_id = provider_id.to_string();
    let my_epoch = next_epoch();
    let _done_guard = LoginDoneGuard { epoch: my_epoch };
    let http = match shared_qoder_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    // 注册 cancel sender + 抢占语义（新登录抢占任何 in-flight）。
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight qoder OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // on_auth_url：拿到 authUrl 后开内置 webview 登录窗（对齐 workbuddy/trae）。
    let on_auth_url = |url: &str| {
        tracing::info!(
            auth_url = url,
            "qoder OAuth authUrl 已生成 — 内置 webview 打开"
        );
        let url = url.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = web_session_quota::open_external_login_window(
                QODER_LOGIN_WIN,
                "QoderWork 登录",
                &url,
                (520.0, 720.0),
            )
            .await
            {
                tracing::warn!(error = %e, "[Qoder] 打开内置登录窗失败");
            }
        });
    };

    // 用户手动关登录窗 = 取消（否则轮询会傻等满 5min）。
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
                if web_session_quota::external_login_window_open(QODER_LOGIN_WIN) {
                    seen_open = true;
                } else if seen_open {
                    tracing::info!("[Qoder] 登录窗被用户关闭 → 取消登录");
                    cancel_in_flight_login();
                    break;
                }
            }
        });
    }

    let result = run_qoder_login(http, on_auth_url, Some(cancel_rx)).await;
    login_done.store(true, Ordering::Relaxed);
    cleanup_slot(my_epoch);
    web_session_quota::close_external_login_window(QODER_LOGIN_WIN);

    match result {
        Ok(cred) => {
            let nickname = cred.nickname.clone();
            let obtained_at = cred.obtained_at_ms;
            // 加账号入池:分配/复用本账号 machine_id + 写 <provider_id>/<uid>.json + 更新池。
            match pool::add_account(&provider_id, cred) {
                Ok(uid) => Json(json!({
                    "loggedIn": true,
                    "nickname": nickname,
                    "userId": uid,
                    "obtainedAt": obtained_at,
                }))
                .into_response(),
                Err(e) => err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("qoder add account failed: {e}"),
                )
                .into_response(),
            }
        }
        Err(QoderError::Cancelled) => {
            tracing::info!("qoder OAuth login cancelled — 不落盘");
            Json(json!({ "loggedIn": false, "cancelled": true })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "qoder OAuth login 失败");
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
        assert!(!cancel_in_flight_login().cancelled);
    }
}
