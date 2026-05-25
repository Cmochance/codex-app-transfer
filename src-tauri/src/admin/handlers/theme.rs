//! `/api/desktop/theme/*` — Codex Desktop UI 主题(#264).
//!
//! 跟 [`crate::admin::handlers::plugin_unlock`] 独立 toggle:user 可单独
//! 开 plugin unlock 不开 theme,反之亦然。
//!
//! - GET  /api/desktop/theme/list    → 内置主题列表(id + display_name + has_mascot)
//! - GET  /api/desktop/theme/status  → 当前注入状态(disabled / applying / applied / failed)
//! - POST /api/desktop/theme/apply   → body `{ theme_id: "..." }` 注入指定主题
//! - POST /api/desktop/theme/clear   → 清除主题(回原生 Codex UI)

use axum::{
    extract::Json,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::codex_theme_injector::{
    all_themes, apply_theme, clear_theme, get_status as get_theme_status, load_theme_assets,
};

use super::super::state::AdminState;
use super::common::err;

pub async fn list_handler() -> impl IntoResponse {
    // **附带 bg data URI**(#264 缩略图):前端 grid 卡片直接 `<img src>`,
    // 不需要新 endpoint。响应大小 ~5MB(5 张原图 base64 + JSON 包装),用户
    // 只在第一次进 Theme 页时拉,接受;比加 thumbnail endpoint + 二次解析路径轻。
    let themes: Vec<_> = all_themes()
        .into_iter()
        .map(|m| {
            let bg_data_uri = load_theme_assets(m.id)
                .map(|a| a.bg_data_uri)
                .unwrap_or_default();
            json!({
                "id": m.id,
                "displayNameZh": m.display_name_zh,
                "displayNameEn": m.display_name_en,
                "hasMascot": m.has_mascot,
                "bgDataUri": bg_data_uri,
            })
        })
        .collect();
    Json(json!({ "themes": themes }))
}

pub async fn status_handler() -> impl IntoResponse {
    let status = get_theme_status().await;
    Json(json!({ "status": status }))
}

#[derive(Debug, Deserialize)]
pub struct ApplyPayload {
    pub theme_id: String,
}

pub async fn apply_handler(Json(payload): Json<ApplyPayload>) -> impl IntoResponse {
    match apply_theme(&payload.theme_id).await {
        Ok(()) => Json(json!({
            "success": true,
            "message": format!("主题 {} 已应用 / Theme {} applied", payload.theme_id, payload.theme_id),
        }))
        .into_response(),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn clear_handler() -> impl IntoResponse {
    match clear_theme().await {
        Ok(()) => Json(json!({
            "success": true,
            "message": "主题已清除 / Theme cleared",
        }))
        .into_response(),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::theme::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/desktop/theme/list", get(list_handler))
        .route("/api/desktop/theme/status", get(status_handler))
        .route("/api/desktop/theme/apply", post(apply_handler))
        .route("/api/desktop/theme/clear", post(clear_handler))
}
