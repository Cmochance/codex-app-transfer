//! `/api/desktop/real-account/*` — 真实 ChatGPT 账号 plugin 模式 HTTP API(MOC-104)。
//!
//! 前端用这组 API 管理真实 chatgpt 账号:
//! - GET  /api/desktop/real-account/status        → 检测 + 登录流程状态
//! - POST /api/desktop/real-account/login         → 启动官方 codex login(非阻塞)
//! - POST /api/desktop/real-account/login/cancel  → 取消进行中的登录
//! - POST /api/desktop/real-account/import        → 从文件导入(body=auth.json 内容,持久 + 生效)
//! - POST /api/desktop/real-account/pin-current   → 持久保留当前真实账号(登录成功后前端自动调)
//! - POST /api/desktop/real-account/forget        → 清除真实账号(删持久镜像,退出长期生效)

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
/// 从文件**路径**导入:body = `{ "source_path": "<绝对路径>" }`(前端用 Tauri dialog
/// 选文件、把绝对路径传进来 —— file input 在 macOS webview 拿不到路径)。后端读该路径
/// 文件、校验是可用 chatgpt → 写持久镜像快照 + **记录源路径** + 恢复到活动(先备份)。
///
/// [MOC-104 分流] 导入**不刷新** token —— transfer 与源头 Codex 共享 single-use
/// refresh_token,任何一方多刷一次都会触发 `refresh_token_reused` 把账号烧死。导入只
/// 校验 + 落盘 + 记源路径;token 保鲜交给源头(活源:记录的路径那边 Codex 刷新,启动
/// reconcile 从源跟随;静态文件:用快照)。`import_auth` 按本地 JWT exp 判过期设 relogin,
/// 这里读出来回给前端:过期就提示重新导出 / 登录,而不是默默拿过期账号去 401。
#[derive(serde::Deserialize)]
pub struct ImportRequest {
    /// 导入源文件的绝对路径(前端 Tauri dialog.open 返回)。
    pub source_path: String,
}

pub async fn import_handler(Json(req): Json<ImportRequest>) -> impl IntoResponse {
    if let Err(e) = codex_real_account::import_auth(req.source_path).await {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    let status = codex_real_account::detect();
    Json(json!({
        "success": true,
        "message": "已导入并持久保留真实账号",
        "relogin_required": status.relogin_required,
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
