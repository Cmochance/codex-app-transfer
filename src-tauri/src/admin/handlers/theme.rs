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
    extract::{Json, Query},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::codex_theme_injector::{
    all_themes, apply_theme, bg_download_progress, clear_theme, curated_from, delete_custom_theme,
    get_status as get_theme_status, load_theme_assets, parse_override, reload_codex_page,
    save_custom_theme,
};
use axum::routing::delete;

use super::super::state::AdminState;
use super::common::err;

pub async fn list_handler() -> impl IntoResponse {
    // **附带 preview data URI**(#264 缩略图):用预渲染的 640px 宽 desktop
    // 主题应用截图(左侧 sidebar GaussianBlur 防隐私),~40KB/张 → 总 ~200KB
    // 响应,比塞原始 bg 全图(~10MB)轻得多。
    // [MOC-272] 一次性读 config 里的 palette overrides,避免 per-theme 重复 load/heal config(/code-review #8)。
    let overrides = crate::admin::registry_io::load().ok().and_then(|c| {
        c.get("settings")
            .and_then(|s| s.get("themePaletteOverrides"))
            .cloned()
    });
    let themes: Vec<_> = all_themes()
        .into_iter()
        .map(|m| {
            let preview_data_uri = load_theme_assets(m.id)
                .map(|a| a.preview_data_uri)
                .unwrap_or_default();
            let ov = overrides
                .as_ref()
                .and_then(|o| o.get(m.id))
                .and_then(parse_override);
            json!({
                "id": m.id,
                "displayNameZh": m.display_name_zh,
                "displayNameEn": m.display_name_en,
                "hasMascot": m.has_mascot,
                "previewDataUri": preview_data_uri,
                // [MOC-272] 有效精选调色板(基底 + 用户 override)→ 前端调色盘编辑器回显当前值。
                "palette": curated_from(&m.palette, ov.as_ref()),
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
pub struct BgProgressQuery {
    pub theme_id: String,
}

/// `GET /api/desktop/theme/bg-progress?theme_id=` — 内置主题背景全图 on-demand 下载进度。
/// 前端在 apply 期间轮询,在该主题缩略图上渲染进度环 + 白半透明蒙版;`downloading:false`
/// = 已缓存 / 未触发 / 下载结束(前端据此撤掉环+蒙版)。
pub async fn bg_progress_handler(Query(q): Query<BgProgressQuery>) -> impl IntoResponse {
    match bg_download_progress(&q.theme_id) {
        Some((downloaded, total)) => Json(json!({
            "downloading": true,
            "downloaded": downloaded,
            "total": total,
        })),
        None => Json(json!({ "downloading": false })),
    }
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

/// CDP `Page.reload` 当前 Codex Desktop page。**v1 无前端 UI 入口**(改主题
/// 走 IIFE remove-then-create 即刻切换,不需要 page reload);保留为
/// 开发 / 测试备用 API(可 `curl -X POST localhost:N/api/desktop/theme/reload`
/// 强制重应用 / verify 注册的 `addScriptToEvaluateOnNewDocument` 是否生效)。
pub async fn reload_handler() -> impl IntoResponse {
    match reload_codex_page().await {
        Ok(()) => Json(json!({
            "success": true,
            "message": "已发送 reload 请求 / Reload requested",
        }))
        .into_response(),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CustomUploadPayload {
    /// `data:image/jpeg;base64,<...>` 或 `data:image/png;base64,<...>`。前端
    /// 用 `FileReader.readAsDataURL` 直接拿到这个格式。
    pub data_uri: String,
}

/// 接 user 上传的图片(JPG / PNG)→ 后端等比 resize(最长边 ≤2048)+ JPEG
/// encode → 写 `~/.codex-app-transfer/themes/custom/bg.jpg` + `preview.jpg`。
/// 前端 `ThemeCropModal` 已按 16:9 裁切;接着前端 list 会拿到 custom 卡片。
pub async fn custom_upload_handler(Json(payload): Json<CustomUploadPayload>) -> impl IntoResponse {
    use base64::{engine::general_purpose, Engine as _};

    let comma = match payload.data_uri.find(',') {
        Some(i) => i,
        None => {
            return err(
                axum::http::StatusCode::BAD_REQUEST,
                "data_uri 格式错误(需 'data:image/...;base64,<bytes>')".to_string(),
            )
            .into_response();
        }
    };
    let b64 = &payload.data_uri[comma + 1..];
    let bytes = match general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => {
            return err(
                axum::http::StatusCode::BAD_REQUEST,
                format!("base64 解码失败: {e}"),
            )
            .into_response();
        }
    };

    match save_custom_theme(&bytes) {
        Ok(()) => Json(json!({
            "success": true,
            "message": "自定义主题已保存 / Custom theme saved",
        }))
        .into_response(),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 删除 user 上传的自定义主题(rm disk)。幂等:文件不存在返 success。
/// **不**碰 settings.codexUiTheme — caller(前端)若当前 selected = custom 需自行
/// 切回默认(carton)避免下次 apply 找不到 asset。
pub async fn custom_delete_handler() -> impl IntoResponse {
    match delete_custom_theme() {
        Ok(()) => Json(json!({
            "success": true,
            "message": "自定义主题已删除 / Custom theme deleted",
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
        .route("/api/desktop/theme/bg-progress", get(bg_progress_handler))
        .route("/api/desktop/theme/apply", post(apply_handler))
        .route("/api/desktop/theme/clear", post(clear_handler))
        .route("/api/desktop/theme/reload", post(reload_handler))
        .route(
            "/api/desktop/theme/custom/upload",
            post(custom_upload_handler),
        )
        .route("/api/desktop/theme/custom", delete(custom_delete_handler))
}
