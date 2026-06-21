//! `/api/chrome/*` — headless 抓取后端 (MOC-144) 的 Chrome 就绪检查/按需下载。
//!
//! 前端"联网工具"设置选 `headless` 时:`GET /api/chrome/ready` 看是否就绪(系统已装 Chrome 或已下载的
//! 内置 shell);未就绪则弹窗让用户确认,确认后 `POST /api/chrome/ensure` 触发按需下载 chrome-headless-shell。
//!
//! - `GET  /api/chrome/ready`  → `{ ready: bool }`
//! - `POST /api/chrome/ensure` → `{ success: bool, path?: string, message?: string }`
//!
//! [MOC-261 二-6] 旧独立探针 `GET /api/chrome/detect`(只查系统 Chrome 文件存在)已删:前端零引用、
//! 冗余 —— 系统 Chrome 探测已内建在 `ready`(`chrome_ready_without_download`)+ MCP webfetch 里。

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_http::headless;
use serde_json::json;

/// readiness gate(设置门控用,对齐 web_search MOC-190):已下载内置 shell **或** 系统 Chrome
/// `--version` 自检通过 → ready,且**都不触发下载**。比裸查文件存在更准(忽略已下载的 shell、
/// 不做自检会让 stale/坏 Chrome 被误判命中)。
pub async fn ready() -> impl IntoResponse {
    Json(json!({ "ready": headless::chrome_ready_without_download().await })).into_response()
}

/// 确保 chrome-headless-shell 就绪(系统无 Chrome 时按需下载 ~86MB,复用)。
///
/// 注:首次会阻塞下载(~20s);前端应在确认弹窗后带 loading 态调用(在 `ready` 未就绪时)。
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
