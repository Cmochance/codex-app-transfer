//! `/api/desktop/real-account/*` — 真实 ChatGPT 账号 plugin 模式 HTTP API(MOC-104)。
//!
//! 前端用这组 API 判断本机是否已有可用的真实 chatgpt 登录态、并按需刷新其 token。
//! 本增量暴露:
//! - GET  /api/desktop/real-account/status   → 只读检测真实 chatgpt 账号状态
//! - POST /api/desktop/real-account/refresh  → 刷新真实账号 token(将过期才刷)
//!
//! 登录(spawn `codex login`)、强制启用按钮等在后续增量加。

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
        (true, AuthSource::Backup) => "transfer 备份里有真实 ChatGPT 账号(活动文件已被改写,可恢复)",
        _ => "未检测到真实 ChatGPT 登录态",
    };
    Json(json!({
        "success": true,
        "message": message,
        "status": status,
    }))
}

/// POST /api/desktop/real-account/refresh
///
/// 刷新真实 chatgpt 账号(官方活动或 transfer 备份里那份)的 token —— access_token
/// 将过期才真刷,否则报 still_valid。非破坏:只更新 token 字段 + last_refresh。
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

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::real_account::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/desktop/real-account/status", get(status_handler))
        .route("/api/desktop/real-account/refresh", post(refresh_handler))
}
