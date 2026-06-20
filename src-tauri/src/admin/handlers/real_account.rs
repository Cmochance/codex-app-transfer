//! `/api/desktop/real-account/*` — 真实 ChatGPT 账号 plugin 模式 HTTP API(MOC-104)。
//!
//! 前端用这组 API 管理真实 chatgpt 账号:
//! - GET  /api/desktop/real-account/status        → 检测 + 登录流程状态
//! - POST /api/desktop/real-account/login         → 启动官方 codex login(非阻塞)
//! - POST /api/desktop/real-account/login/cancel  → 取消进行中的登录
//! - POST /api/desktop/real-account/pin-current   → 持久保留当前真实账号(登录成功后前端自动调)

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
        // [MOC-178] 真实账号模式持久开关(用户意图)+ 活动是否真 chatgpt(relay 此刻是否真生效)。
        // 前端据 mode_enabled 派生 toggle(不再用 logged_in),据 active_is_chatgpt 判 relay 实效。
        "mode_enabled": super::settings::read_real_account_mode_enabled(),
        "active_is_chatgpt": codex_real_account::active_is_real_chatgpt_now(),
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

/// POST /api/desktop/real-account/pin-current
///
/// 钉住当前检测到的真实账号(官方活动 auth.json)进持久镜像。
pub async fn pin_current_handler() -> impl IntoResponse {
    if let Err(e) = codex_real_account::pin_current_account().await {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    // [MOC-178 codex P2] pin 由前端 auto-pin **自动**调用(activeReal + 无镜像,仅打开 UI 就触发),
    // 前提是活动已 chatgpt。故**只 save 镜像**,绝不走 finalize 的 apply relay / 回滚 / deactivate
    // —— 否则 proxy 起不来时仅打开 UI 就把用户正在用的活动 chatgpt 切 apikey(回归)。
    // flag:**只在 provider 支持 relay**(有 active provider + 走 proxy)时开;direct(不代理)**或无
    // provider**(默认 activeProvider null,没法 apply relay)→ 只 save 镜像不开 mode,避免「flag on 但
    // 无法 relay、plugins locked」。同 startup reconcile 的收敛,纠正 runtime 切走后 flag 残留。
    let supports_relay =
        crate::admin::services::desktop::snapshot::active_provider_supports_relay();
    let _ = super::settings::set_real_account_mode_enabled(supports_relay);
    // [MOC-178 codex P2] 返回 enabled = 是否真开了 relay(supports_relay)。前端 auto-pin 据它决定
    // 是否清 force CDP 档 —— direct/无 provider 下 pin 只 save 镜像、relay 没开,force 可能是唯一
    // unlock path,不能因 pin succeed 就清。
    Json(json!({ "success": true, "enabled": supports_relay, "message": "已钉住当前真实账号(持久保留)" }))
        .into_response()
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
        .route(
            "/api/desktop/real-account/pin-current",
            post(pin_current_handler),
        )
}
