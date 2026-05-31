//! `/api/desktop/real-account/*` — 真实 ChatGPT 账号 plugin 模式 HTTP API(MOC-104)。
//!
//! 前端用这组 API 判断本机是否已有可用的真实 chatgpt 登录态,决定是否提示登录
//! /可直接走真实账号模式。本增量只暴露**只读检测**;登录(spawn `codex login`)、
//! token 刷新、强制启用等写操作在后续增量加。
//!
//! - GET /api/desktop/real-account/status → 检测真实 chatgpt 账号状态

use axum::{response::IntoResponse, routing::get, Json, Router};
use serde_json::json;

use crate::codex_real_account::{self, AuthSource};

use super::super::state::AdminState;

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

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::real_account::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new().route("/api/desktop/real-account/status", get(status_handler))
}
