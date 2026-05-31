//! `/api/desktop/real-account/*` — 真实 ChatGPT 账号 plugin 模式 HTTP API(MOC-104)。
//!
//! 前端用这组 API 管理真实 chatgpt 账号:
//! - GET  /api/desktop/real-account/status        → 检测 + 登录流程状态
//! - POST /api/desktop/real-account/refresh       → 刷新真实账号 token(将过期才刷)
//! - POST /api/desktop/real-account/login         → 启动官方 codex login(非阻塞)
//! - POST /api/desktop/real-account/login/cancel  → 取消进行中的登录
//! - POST /api/desktop/real-account/import        → 从文件导入(body=auth.json 内容,持久 + 生效)
//! - POST /api/desktop/real-account/pin-current   → 持久保留当前真实账号(登录成功后前端自动调)
//! - POST /api/desktop/real-account/forget        → 清除真实账号(删持久镜像,退出长期生效)

use std::time::Duration;

use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::codex_real_account::{self, AuthSource};

use super::super::state::AdminState;
use super::common::err;

/// GET /api/desktop/real-account/status
pub async fn status_handler() -> impl IntoResponse {
    let status = codex_real_account::detect();
    let message = match (status.logged_in, status.source) {
        (true, AuthSource::Official) => "已登录真实 ChatGPT 账号(官方 auth.json)",
        (true, AuthSource::Imported) => "已导入真实 ChatGPT 账号(持久保留,活动文件失效时自动恢复)",
        _ => "未检测到真实 ChatGPT 登录态",
    };
    Json(json!({
        "success": true,
        "message": message,
        "status": status,
        "login": codex_real_account::login_status(),
    }))
}

/// POST /api/desktop/real-account/refresh
///
/// 刷新真实 chatgpt 账号(官方活动 auth.json 或导入的持久镜像)的 token ——
/// access_token 将过期才真刷,否则报 still_valid。非破坏:只更新 token 字段 + last_refresh。
pub async fn refresh_handler() -> impl IntoResponse {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("构建 HTTP client 失败: {e}"),
            )
            .into_response()
        }
    };
    match codex_real_account::refresh_if_needed(&client).await {
        Ok(outcome) => Json(json!({ "success": true, "outcome": outcome })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /api/desktop/real-account/login
///
/// 启动官方 `codex login`(非阻塞,会弹浏览器做 ChatGPT OAuth)。立即返回;前端轮
/// 询 `status` 的 `login` 字段看进度(running → succeeded/failed/cancelled)。
pub async fn login_handler() -> impl IntoResponse {
    match codex_real_account::start_login() {
        Ok(()) => {
            Json(json!({ "success": true, "message": "已启动 codex login,请在浏览器完成授权" }))
                .into_response()
        }
        Err(e) => err(StatusCode::CONFLICT, e).into_response(),
    }
}

/// POST /api/desktop/real-account/login/cancel
pub async fn login_cancel_handler() -> impl IntoResponse {
    let cancelled = codex_real_account::cancel_login();
    Json(json!({
        "success": true,
        "cancelled": cancelled,
        "message": if cancelled { "已取消登录" } else { "当前没有进行中的登录" },
    }))
}

/// POST /api/desktop/real-account/import
///
/// 从文件导入:body 是 chatgpt auth.json 的 JSON 内容。校验后写进 transfer 持久
/// 镜像(长期保留、不受 ~/.codex 文件变动影响)+ 恢复到活动文件(先备份)。
///
/// [review #3] 导入成功后立即按需刷新一次 token —— 用户导入的很可能是过期旧文件,
/// 不刷的话活动文件里就是个 access_token 已过期的账号,plugin 模式拿去用直接 401。
/// 刷新是 best-effort:导入(持久镜像 + 活动文件)已落盘成功,刷不动只在 outcome
/// 里告诉前端(如 relogin_required),不把"导入成功"翻成失败。
pub async fn import_handler(Json(input): Json<serde_json::Value>) -> impl IntoResponse {
    if let Err(e) = codex_real_account::import_auth(input).await {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    let refresh = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(client) => match codex_real_account::refresh_if_needed(&client).await {
            Ok(outcome) => json!(outcome),
            Err(e) => {
                tracing::warn!("[RealAccount] 导入后刷新 token 失败(best-effort): {e}");
                json!({ "error": e })
            }
        },
        Err(e) => {
            tracing::warn!("[RealAccount] 导入后构建 HTTP client 失败(best-effort): {e}");
            json!({ "error": format!("构建 HTTP client 失败: {e}") })
        }
    };
    Json(json!({
        "success": true,
        "message": "已导入并持久保留真实账号",
        "refresh": refresh,
    }))
    .into_response()
}

/// POST /api/desktop/real-account/pin-current
///
/// 钉住当前检测到的真实账号(官方活动 auth.json)进持久镜像。
pub async fn pin_current_handler() -> impl IntoResponse {
    match codex_real_account::pin_current_account().await {
        Ok(()) => Json(json!({ "success": true, "message": "已钉住当前真实账号(持久保留)" }))
            .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// POST /api/desktop/real-account/forget
///
/// 忘记导入的真实账号(删持久镜像)= 退出"长期生效",启动不再自动恢复。
pub async fn forget_handler() -> impl IntoResponse {
    match codex_real_account::forget_imported().await {
        Ok(removed) => Json(json!({
            "success": true,
            "removed": removed,
            "message": if removed { "已忘记导入的真实账号" } else { "没有导入的真实账号" },
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::real_account::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/desktop/real-account/status", get(status_handler))
        .route("/api/desktop/real-account/refresh", post(refresh_handler))
        .route("/api/desktop/real-account/login", post(login_handler))
        .route(
            "/api/desktop/real-account/login/cancel",
            post(login_cancel_handler),
        )
        .route("/api/desktop/real-account/import", post(import_handler))
        .route(
            "/api/desktop/real-account/pin-current",
            post(pin_current_handler),
        )
        .route("/api/desktop/real-account/forget", post(forget_handler))
}
