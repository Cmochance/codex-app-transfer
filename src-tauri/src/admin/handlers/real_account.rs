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
    // [MOC-178 codex P2] 导入真实账号 = 用户主动建立真实账号 → 开真实账号模式持久 flag。否则
    // clear(flag=false)→ import 后 flag 仍 false,下次启动 ForceDisable 把刚导入的账号又切回
    // apikey(撤销导入)、UI toggle 也错显 off。
    let _ = super::settings::set_real_account_mode_enabled(true);
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
        Ok(()) => {
            // [MOC-178 codex P2] 钉住真实账号 = 开真实账号模式持久 flag(同 import,见上)。
            let _ = super::settings::set_real_account_mode_enabled(true);
            Json(json!({ "success": true, "message": "已钉住当前真实账号(持久保留)" }))
                .into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// POST /api/desktop/real-account/forget
///
/// 忘记导入的真实账号(删持久镜像)= 退出"长期生效",启动不再自动恢复。
pub async fn forget_handler(
    axum::extract::State(state): axum::extract::State<AdminState>,
) -> impl IntoResponse {
    let removed = match codex_real_account::forget_imported().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // [MOC-178] 落「用户主动关真实账号模式」持久标志(不被退出 restore 撤销)——重启后
    // reconcile 据此收敛回 apikey、不自动开。这是「关闭持久」的真相源。
    let _ = super::settings::set_real_account_mode_enabled(false);
    // [MOC-178] 删镜像后 apply 当前 provider 强制切 apikey:停用真实账号(toggle 关 + Codex
    // 原生不显示 plugins),但**保留 tokens** → 退出 restore 能写回 chatgpt + tokens 完整恢复。
    // (对比直接删活动 auth.json:那会丢 tokens、restore 恢复不回,残缺。)
    let synced =
        crate::admin::services::desktop::snapshot::sync_desktop_clearing_real_account(&state).await;
    let switched = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Json(json!({
        "success": true,
        "removed": removed,
        "switchedToApikey": switched,
        "message": "已清除真实账号(切回 apikey 模式,tokens 保留,退出可恢复)",
    }))
    .into_response()
}

/// POST /api/desktop/real-account/enable
///
/// [MOC-178] 开真实账号模式:校验有可用 token → 写持久 flag=true + 把活动写回 chatgpt +
/// apply relay(Codex 原生显示 plugins)。账号有有效 token(哪怕活动当前是 apikey)就能开。
pub async fn enable_handler(
    axum::extract::State(state): axum::extract::State<AdminState>,
) -> impl IntoResponse {
    // 账号可用性(新口径认 token,清除切 apikey 后 tokens 还在也算有)。
    if !codex_real_account::detect().logged_in {
        return err(
            StatusCode::BAD_REQUEST,
            "无可用真实账号(需先登录 / 导入)".to_owned(),
        )
        .into_response();
    }
    match codex_real_account::activate_real_account().await {
        Ok(true) => {}
        Ok(false) => {
            return err(
                StatusCode::BAD_REQUEST,
                "账号 token 不可用(可能已过期,需重新登录)".to_owned(),
            )
            .into_response()
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
    let _ = super::settings::set_real_account_mode_enabled(true);
    // 活动已写回 chatgpt,apply relay 的 gate 通过 → Codex 原生显示 plugins,不启 daemon。
    let synced =
        crate::admin::services::desktop::snapshot::sync_desktop_for_active_provider(&state).await;
    // [MOC-178 codex P2 ×2] 开真实账号模式要 relay 真生效才算成功,两种失败都回滚 flag + 报错,
    // 否则「flag 说开但 relay 没起」状态不一致:
    // ① sync 失败(success:false)—— local_proxy 的 proxy 起不来(端口冲突等)在 apply 前 return,
    //    此时活动仍是 activate 写的 chatgpt(`active_is_real_chatgpt_now` 仍 true,单查它漏判);
    // ② sync 后活动不再 chatgpt —— direct provider 的 relay gate(mode != "direct")把 auth
    //    rewrite 回 apikey。
    let sync_ok = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !sync_ok || !codex_real_account::active_is_real_chatgpt_now() {
        let _ = super::settings::set_real_account_mode_enabled(false);
        return err(
            StatusCode::BAD_REQUEST,
            "开启真实账号模式失败:当前 provider 不支持 relay(如 direct 模式),或系统代理未能启动。请检查 provider / 系统代理后重试".to_owned(),
        )
        .into_response();
    }
    Json(json!({
        "success": true,
        "enabled": true,
        "applied": true,
        "message": "已开启真实账号模式",
    }))
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
        .route("/api/desktop/real-account/import", post(import_handler))
        .route(
            "/api/desktop/real-account/pin-current",
            post(pin_current_handler),
        )
        .route("/api/desktop/real-account/forget", post(forget_handler))
        .route("/api/desktop/real-account/enable", post(enable_handler))
}
