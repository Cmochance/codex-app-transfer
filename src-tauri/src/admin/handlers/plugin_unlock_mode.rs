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
        // [MOC-257 review] 当前**实际生效**三态 = 最近成功 apply 的模式;**未 apply 过 → null**(不回退
        // resolve)。① 报 last-applied 而非 resolve:外部 codex login 让 resolve 升级但未 apply 时,报
        // resolve 会显示 Real 却仍 fabricate。② null(而非 resolve)用于「启动跳过 apply」(autoApplyOnStart
        // =false / 无 provider):此时 ~/.codex 是 restore 后的原态、未应用任何解锁档,报某档=已生效会让前端
        // re-select 同档 no-op、永远点不动;报 null → SegmentedControl 不高亮,点任一档都会 apply。
        "mode": codex_real_account::last_applied_mode(),
        // resolve 推导出的「应当生效」模式(与 mode 不同 = 有待 apply 的升级,如外部登录后)。
        "resolved": codex_real_account::resolve_plugin_unlock_mode(),
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
    let provider_routing = crate::admin::services::desktop::snapshot::provider_routing_is_active();
    // synthetic / real 需要有 active provider 才能把 chatgpt_base_url 引到 proxy(否则 relay 起不来)。
    if provider_routing
        && matches!(mode, PluginUnlockMode::Synthetic | PluginUnlockMode::Real)
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
    // [MOC-257 review] **persist 先于 apply,apply 失败回滚 persist**:两轮 review 的两面 —— persist 后于
    // apply 则 persist 失败时 auth/proxy 已变、UI/持久回旧不一致;persist 先于 apply 则 apply 失败时持久
    // 仍是新值。取「先 persist(失败 → 啥都没动直接报错)+ apply 失败时回滚 persist 到旧意图」同时满足两者。
    let prev = super::settings::read_plugin_unlock_mode();
    if !super::settings::set_plugin_unlock_mode(&req.mode) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "写入插件解锁模式失败(配置文件不可写?),请检查权限 / 磁盘后重试".to_owned(),
        )
        .into_response();
    }
    // 官方路由下只记录第三方模式偏好，等用户下一次启用 Provider 时再应用。这里尤其不能
    // 执行 off（会清掉官方 auth.json）或 synthetic/real（会把官方请求重新指回 proxy）。
    if !provider_routing {
        return Json(json!({
            "success": true,
            "mode": req.mode,
            "effective": null,
            "deferred": true,
            "message": "当前使用官方 Codex；插件解锁设置已保存，将在启用第三方 Provider 后生效",
            "hasRealAccount": codex_real_account::has_real_account(),
            "realAccountUsable": codex_real_account::real_account_usable(),
            "activeIsSynthetic": codex_real_account::active_is_synthetic(),
        }))
        .into_response();
    }
    // effective 直接由请求模式 + 账号可用性算(real 失效 → 降级 synthetic)。
    let effective = match mode {
        PluginUnlockMode::Real if !codex_real_account::real_account_usable() => {
            PluginUnlockMode::Synthetic
        }
        m => m,
    };
    if let Err(e) =
        crate::admin::services::desktop::snapshot::apply_plugin_unlock_mode(&state, effective).await
    {
        // 回滚持久意图到切换前(无键则清回默认),避免下次启动 reconcile 到 UI 没显示的状态。
        match prev {
            Some(p) => {
                let _ = super::settings::set_plugin_unlock_mode(&p);
            }
            None => {
                let _ = super::settings::clear_plugin_unlock_mode();
            }
        }
        // [MOC-257 review] auth/proxy 的回滚**不在这里做**:apply_plugin_unlock_mode 已**事务化自清理**
        // (synthetic/real 分支失败时还原 pre-apply 活动字节 + re-stash + 关伪造),活动态已回到 apply 前。
        // set 端点只需回滚自己写的持久键(上面)即可;再叠一层 recovery(重 apply last_applied / un-stash)
        // 会在 apply 自回滚之上再动(如重复 un-stash 真账号),反而制造不一致。
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
