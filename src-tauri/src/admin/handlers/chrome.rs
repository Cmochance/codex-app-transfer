//! `/api/chrome/*` — headless 抓取后端 (MOC-144) 的 Chrome 探测/按需下载。
//!
//! 前端"联网工具"设置选 `headless` 时:先 `GET /api/chrome/detect` 看系统有没有 Chrome;
//! 没有则弹窗让用户确认,确认后 `POST /api/chrome/ensure` 触发按需下载 chrome-headless-shell。
//!
//! - `GET  /api/chrome/detect` → `{ detected: bool, path?: string }`
//! - `POST /api/chrome/ensure` → `{ success: bool, path?: string, message?: string }`

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_http::headless;
use serde_json::json;

/// 探测系统已装的 Chrome/Edge/Chromium(**不下载**)。命中返回路径。
pub async fn detect() -> impl IntoResponse {
    match headless::detect_system_chrome() {
        Some(path) => Json(json!({
            "detected": true,
            "path": path.to_string_lossy(),
        }))
        .into_response(),
        None => Json(json!({ "detected": false })).into_response(),
    }
}

/// 确保 chrome-headless-shell 就绪(系统无 Chrome 时按需下载 ~86MB,复用)。
///
/// 注:首次会阻塞下载(~20s);前端应在确认弹窗后带 loading 态调用。仅在 detect 未命中
/// + 用户确认下载后才调,所以这里直接走按需下载(不再探测系统)。
pub async fn ensure() -> impl IntoResponse {
    match headless::ensure_chrome_headless_shell().await {
        Ok(path) => Json(json!({
            "success": true,
            "path": path.to_string_lossy(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": e.to_string() })),
        )
            .into_response(),
    }
}
