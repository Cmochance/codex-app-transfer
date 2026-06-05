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

/// [MOC-178 codex P2] 开真实账号模式的共用收尾:写持久 flag=true + apply relay,并校验 relay
/// 真生效。direct provider(relay gate 拒 → auth 被 rewrite 回 apikey)/ sync 失败(proxy 起不来)
/// 导致活动留不住 chatgpt 时,回滚 flag + 把活动切回 apikey(clearing + deactivate 兜底),返 Err。
/// enable / import / pin 共用,避免某路径漏检查(import/pin 曾只 set flag=true 不校验,direct 下
/// 会 set flag=true 但 relay 不生效 → 状态不一致)。`Ok(())` = relay 真开了。
async fn finalize_enable_real_account(state: &AdminState) -> Result<(), String> {
    // [MOC-178 codex P2] 写 flag 失败(config 文件不可写:权限改了 / 配置盘满,但 auth.json 还可写)
    // → abort,不 apply relay。否则会 apply relay 把活动写成 chatgpt 但 flag 仍旧值 → 前端/startup
    // 据旧 flag 当 mode off,活动却是 chatgpt,状态不一致。
    if !super::settings::set_real_account_mode_enabled(true) {
        // [MOC-178 codex P2] abort 前先切活动回 apikey —— activate(enable)/import 已把活动写成
        // chatgpt,若只 return Err 不切,flag false 但活动 chatgpt + plugins exposed 状态不一致。
        if codex_real_account::active_is_real_chatgpt_now() {
            let _ = codex_real_account::deactivate_real_account().await;
        }
        return Err("写入真实账号模式开关失败(配置文件不可写?),请检查权限 / 磁盘后重试".to_owned());
    }
    let synced =
        crate::admin::services::desktop::snapshot::sync_desktop_for_active_provider(state).await;
    let sync_ok = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if sync_ok && codex_real_account::active_is_real_chatgpt_now() {
        return Ok(());
    }
    // relay 没真生效(direct / proxy 起不来)→ 回滚 flag + 切活动回 apikey(clearing 走 force_apikey,
    // 即便 proxy 起不来也切;再 deactivate 兜底覆盖无 provider),避免「flag/UI 说开但 relay 没起」。
    // [本地审查 silent-failure HIGH] 回滚 flag 写失败要留痕:supports_relay=true 但 proxy 临时挂时
    // flag 残留 true → toggle 误显 on,且下次 startup 的 !supports_relay 收敛不触发(provider 本身
    // 支持 relay),reconcile 正常分支可能又恢复 chatgpt 横跳;无日志会让此类报障查不到根因。
    if !super::settings::set_real_account_mode_enabled(false) {
        tracing::error!(
            "[RealAccount] finalize 回滚:flag 回写 false 失败(config 不可写),flag 残留 true → \
             UI toggle 可能误显 on 但 relay 未起;依赖下次 startup reconcile 再纠"
        );
    }
    let _ =
        crate::admin::services::desktop::snapshot::sync_desktop_clearing_real_account(state).await;
    if codex_real_account::active_is_real_chatgpt_now() {
        let _ = codex_real_account::deactivate_real_account().await;
    }
    Err("当前 provider 不支持真实账号 relay(如 direct 模式),或系统代理未能启动。请切到 local_proxy 类 provider / 检查系统代理后重试".to_owned())
}

pub async fn import_handler(
    axum::extract::State(state): axum::extract::State<AdminState>,
    Json(req): Json<ImportRequest>,
) -> impl IntoResponse {
    if let Err(e) = codex_real_account::import_auth(req.source_path).await {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    // [MOC-178 codex P2] import_auth 已写活动 chatgpt + 镜像;走共用收尾(set flag + apply relay +
    // 校验回滚),避免 direct provider 下 set flag=true 但 relay 不生效的状态不一致(原来无条件
    // set flag=true)。enabled=false 表示导入成功但当前 provider 开不了 relay,凭据仍保留。
    let enabled = finalize_enable_real_account(&state).await.is_ok();
    let status = codex_real_account::detect();
    Json(json!({
        "success": true,
        "enabled": enabled,
        "message": if enabled {
            "已导入并开启真实账号模式"
        } else {
            "已导入真实账号;当前 provider 不支持 relay(如 direct),未开启真实账号模式,可切 local_proxy provider 后再开"
        },
        "relogin_required": status.relogin_required,
    }))
    .into_response()
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
    Json(json!({ "success": true, "message": "已钉住当前真实账号(持久保留)" })).into_response()
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
    let _synced =
        crate::admin::services::desktop::snapshot::sync_desktop_clearing_real_account(&state).await;
    // [MOC-178 codex P2] sync 依赖 active provider config;无 provider(默认 activeProvider null)
    // / apply 失败时 sync 切不了、活动仍 chatgpt → 直接切活动 auth apikey 兜底(不依赖 provider)。
    if codex_real_account::active_is_real_chatgpt_now() {
        let _ = codex_real_account::deactivate_real_account().await;
    }
    // [MOC-178 codex P2] switchedToApikey = 活动**确实**已非 chatgpt(plugins 真关了),直接看结果
    // 而非 sync 的 success(那个对「活动本就 apikey」会误报)。
    let switched = !codex_real_account::active_is_real_chatgpt_now();
    Json(json!({
        // [MOC-178 codex P2] success **恒 true**(forget 主操作 = 删镜像 + 关 flag 已成功),api() 不
        // throw → 前端 partial-failure handling(置 realAccountForgotten/modeEnabled/清 force/refresh)
        // 照常执行。切 apikey 是否成功由 `switchedToApikey` 单独标志,前端据它 warning 暴露(sync +
        // deactivate 兜底都失败 = IO error:磁盘满 / 写权限拒 → 活动仍 chatgpt、plugins 未关)。把切
        // apikey 失败塞进 success:false 会让 api() 抛错、跳过 handling、UI stale —— 故用非 throw envelope。
        "success": true,
        "removed": removed,
        "switchedToApikey": switched,
        "message": if switched {
            "已清除真实账号(切回 apikey 模式,tokens 保留,退出可恢复)"
        } else {
            "已清除镜像,但切 apikey 失败(磁盘 / 权限?)—— Plugins 可能未关,请重试或重启 Codex"
        },
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
    // [MOC-178 codex P2] 共用收尾:set flag + apply relay + 校验回滚(direct / proxy 失败回滚
    // flag + 切活动回 apikey)。逻辑见 finalize_enable_real_account。
    if let Err(msg) = finalize_enable_real_account(&state).await {
        return err(StatusCode::BAD_REQUEST, msg).into_response();
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
