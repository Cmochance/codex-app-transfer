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
        // 当前**生效**三态(持久键优先,缺失则按真账号推导;real 不可用会降级 synthetic)。
        "mode": codex_real_account::resolve_plugin_unlock_mode(),
        // 持久值(用户是否手动设过);null = 未设、走默认推导。
        "persisted": super::settings::read_plugin_unlock_mode(),
        // 本地是否有真账号(活动或 stash,含失效的)。
        "hasRealAccount": codex_real_account::has_real_account(),
        // 真账号是否**实际可用**(非空 + 未过期 + 未撤销)—— 前端据此显示「真账号已失效已降级」。
        "realAccountUsable": codex_real_account::real_account_usable(),
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
    // [MOC-257] real + 本地**完全无真账号**(活动 + stash 都没有)→ 不切模式,回 needsLogin 让前端
    // 弹窗提示「请先在 Codex 登录 ChatGPT 账号」。区别于「有账号但失效」(那走下面降级,不拦)。
    if matches!(mode, PluginUnlockMode::Real) && !codex_real_account::has_real_account() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "needsLogin": true,
                "message": "未检测到 ChatGPT 账号;请先在 Codex 登录 ChatGPT 账号后再用真实账号",
            })),
        )
            .into_response();
    }
    // 先落持久意图(off 也持久:重启仍 stash 走 auth.json)。[MOC-257 review] 持久化失败(config 只读 /
    // 磁盘满)必须报错,不能 apply 完报成功 —— 否则下次启动 reconcile 读到旧/缺失的持久值、与 UI 不一致。
    if !super::settings::set_plugin_unlock_mode(&req.mode) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "写入插件解锁模式失败(配置文件不可写?),请检查权限 / 磁盘后重试".to_owned(),
        )
        .into_response();
    }
    // [MOC-257] apply **生效**模式而非请求模式:real 但账号失效(过期/撤销)→ resolve 降级到 synthetic
    // (用户要求);持久仍 real,账号恢复可用后自动升回。off/synthetic 生效=请求。
    let effective = codex_real_account::resolve_plugin_unlock_mode();
    if let Err(e) =
        crate::admin::services::desktop::snapshot::apply_plugin_unlock_mode(&state, effective).await
    {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    let degraded =
        matches!(mode, PluginUnlockMode::Real) && effective == PluginUnlockMode::Synthetic;
    Json(json!({
        "success": true,
        "mode": req.mode,            // 用户意图(持久)
        "effective": effective,      // 实际生效(real 失效会降级 synthetic)
        "degraded": degraded,
        "message": if degraded {
            "真实账号已失效(过期 / 服务端撤销),已降级为模拟账号;请在 Codex 重新登录后再切真实账号"
        } else {
            ""
        },
        "hasRealAccount": codex_real_account::has_real_account(),
        "realAccountUsable": codex_real_account::real_account_usable(),
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
