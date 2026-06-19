//! `/api/desktop/plugin-unlock/*` — 三态插件解锁选择器 (MOC-257)。
//!
//! 统一旧的「自动解锁(CDP,已废弃)+ 模拟账号 + 真实账号」三个开关为单一三态:
//! - **off**:不解锁。真账号整文件 stash 走、确保 `~/.codex` 无 auth.json;退出/切回时还原。
//! - **synthetic**:写合规合成 auth.json,proxy 截断 `/backend-api` 逐条伪造,Codex 原生显示 Plugins。
//! - **real**:用真实 chatgpt 账号(从 stash 还原 / 现有活动),relay 透传真 chatgpt.com。
//!
//! 非 off 一律 apply relay(写 `chatgpt_base_url`→proxy);synthetic/real 由 proxy `FAKE_ACCOUNT_MODE`
//! atomic 区分伪造 vs 透传。持久键 `pluginUnlockMode`;键缺失按「有真账号→real / 无→synthetic」推导。

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::codex_real_account::{self, PluginUnlockMode};

use super::super::state::AdminState;
use super::common::err;

/// GET /api/desktop/plugin-unlock/status
pub async fn status_handler() -> impl IntoResponse {
    Json(json!({
        "success": true,
        // 当前**生效**三态(持久键优先,缺失则按真账号推导)。serde snake_case: off/synthetic/real。
        "mode": codex_real_account::resolve_plugin_unlock_mode(),
        // 持久值(用户是否手动设过);null = 未设、走默认推导。
        "persisted": super::settings::read_plugin_unlock_mode(),
        "hasRealAccount": codex_real_account::has_real_account(),
        "activeIsSynthetic": codex_real_account::active_is_synthetic(),
    }))
}

#[derive(serde::Deserialize)]
pub struct SetModeRequest {
    /// "off" | "synthetic" | "real"
    pub mode: String,
}

/// POST /api/desktop/plugin-unlock/set
pub async fn set_handler(
    State(state): State<AdminState>,
    Json(req): Json<SetModeRequest>,
) -> impl IntoResponse {
    let mode = match req.mode.as_str() {
        "off" => PluginUnlockMode::Off,
        "synthetic" => PluginUnlockMode::Synthetic,
        "real" => PluginUnlockMode::Real,
        other => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("mode 必须是 off / synthetic / real(收到 {other})"),
            )
            .into_response();
        }
    };
    // synthetic / real 需要有 active provider 才能把 chatgpt_base_url 引到 proxy(否则 relay 起不来)。
    if matches!(mode, PluginUnlockMode::Synthetic | PluginUnlockMode::Real)
        && !crate::admin::services::desktop::snapshot::active_provider_supports_relay()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "当前无可用 provider,无法开启 relay;请先在「Provider」配置并激活一个 provider"
                .to_owned(),
        )
        .into_response();
    }
    // real 需要确有真账号(活动或 stash);无则引导登录/导入或改用模拟账号。
    if matches!(mode, PluginUnlockMode::Real) && !codex_real_account::has_real_account() {
        return err(
            StatusCode::BAD_REQUEST,
            "未检测到真实 ChatGPT 账号(需先登录 / 导入),或改用「模拟账号」".to_owned(),
        )
        .into_response();
    }
    // 先落持久意图(off 也持久:重启仍 stash 走 auth.json),再 apply。
    let _ = super::settings::set_plugin_unlock_mode(&req.mode);
    if let Err(e) =
        crate::admin::services::desktop::snapshot::apply_plugin_unlock_mode(&state, mode).await
    {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    Json(json!({
        "success": true,
        "mode": req.mode,
        "hasRealAccount": codex_real_account::has_real_account(),
        "activeIsSynthetic": codex_real_account::active_is_synthetic(),
    }))
    .into_response()
}

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::plugin_unlock_mode::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/desktop/plugin-unlock/status", get(status_handler))
        .route("/api/desktop/plugin-unlock/set", post(set_handler))
}
