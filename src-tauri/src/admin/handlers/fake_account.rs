//! `/api/desktop/fake-account/*` — 模拟(伪造)账号 plugin 模式 HTTP API(MOC-257)。
//!
//! 无真实 ChatGPT 账号时的「强制解锁」新档,取代不可靠的 CDP `setAuthMethod` 注入:
//! - GET  /api/desktop/fake-account/status   → 当前是否开 + 活动是否合成账号
//! - POST /api/desktop/fake-account/enable    → 写合规伪造 auth.json + apply relay + 开 proxy 伪造
//! - POST /api/desktop/fake-account/disable   → 切回 apikey + 关 proxy 伪造 + strip chatgpt_base_url
//!
//! 机制:合成 auth.json(`auth_mode=chatgpt` + 合成 JWT)让 Codex 原生显示 Plugins、原生发
//! `/backend-api/*`;proxy 在 `FAKE_ACCOUNT_MODE` 开时把这些请求逐条伪造 200(`fake_account` 模块),
//! 不透传真 chatgpt.com。relay 装配(写 `chatgpt_base_url`→proxy)复用真实账号模式那套。
//!
//! **与真实账号模式互斥**:开伪造前要求无真实账号在用;`activate_fake_account` 本身也拒绝覆盖真账号。

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::codex_real_account;

use super::super::state::AdminState;
use super::common::err;

/// GET /api/desktop/fake-account/status
pub async fn status_handler() -> impl IntoResponse {
    let status = codex_real_account::detect();
    Json(json!({
        "success": true,
        "mode_enabled": super::settings::read_fake_account_mode_enabled(),
        // 活动当前是否合成账号(伪造 relay 此刻是否真生效)。
        "active_is_synthetic": codex_real_account::active_is_synthetic(),
        "status": status,
    }))
}

/// POST /api/desktop/fake-account/enable
///
/// 写合规伪造 auth.json + 落持久 flag + 开 proxy 伪造 + apply relay。失败回滚到关闭态。
pub async fn enable_handler(State(state): State<AdminState>) -> impl IntoResponse {
    // 互斥:已有真实账号模式在用 → 拒绝(应直接用真实账号,无需伪造)。
    if super::settings::read_real_account_mode_enabled() == Some(true) {
        return err(
            StatusCode::CONFLICT,
            "真实账号模式已开启,无需模拟账号;如要用模拟账号请先关闭真实账号模式".to_owned(),
        )
        .into_response();
    }
    // relay 必须有 active provider 才能把 chatgpt_base_url 引到 proxy(否则 /backend-api 直连
    // chatgpt.com、伪造拦不到)。无 provider → 拒绝、引导先配 provider。
    if !crate::admin::services::desktop::snapshot::active_provider_supports_relay() {
        return err(
            StatusCode::BAD_REQUEST,
            "当前无可用 provider,无法开启模拟账号 relay;请先在「Provider」配置并激活一个 provider"
                .to_owned(),
        )
        .into_response();
    }
    // 写合成 auth.json(若活动是真实 chatgpt 会被 activate_fake_account 拒绝,防误覆盖真账号)。
    if let Err(e) = codex_real_account::activate_fake_account().await {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    if let Err(msg) = finalize_enable_fake_account(&state).await {
        return err(StatusCode::BAD_REQUEST, msg).into_response();
    }
    Json(json!({
        "success": true,
        "enabled": true,
        "message": "已开启模拟账号模式(Codex 原生显示 Plugins,账号/插件请求由本机伪造)",
    }))
    .into_response()
}

/// POST /api/desktop/fake-account/disable
///
/// 关模拟账号模式:落 flag=false + 切活动回 apikey + apply 当前 provider(写回 apikey/gateway
/// key + strip chatgpt_base_url)+ 关 proxy 伪造。即便活动非合成也幂等收敛。
///
/// 顺序刻意:① 先落 flag ② deactivate 切活动回 apikey ③ apply 回填真 key ④ **最后**关 proxy
/// 伪造 —— 活动还是合成(chatgpt)期间保持伪造,避免「伪造已关但活动仍 chatgpt」窗口里
/// `/backend-api` 透传假 token 被上游 401。如实反映 apply 结果:无 provider 时 sync 不调 apply、
/// 活动停在「apikey 但无 key」残缺态,**不报成功切回**,告知用户需配 provider(对齐「禁止把失败
/// 伪装成成功」硬规则)。
pub async fn disable_handler(State(state): State<AdminState>) -> impl IntoResponse {
    // [MOC-257 bot P2] flag 落 false 失败要如实报告:吞掉的话磁盘仍 fakeAccountModeEnabled=true,
    // 下次启动 reconcile 会据 stale flag 重建合成账号 → 用户点的「关闭」并不持久,恰是磁盘/权限
    // 失败这种最该提示的场景。捕获结果,失败时 error 留痕 + 进入下方非 clean 分支告知用户。
    let flag_persisted = super::settings::set_fake_account_mode_enabled(false);
    if !flag_persisted {
        tracing::error!(
            "[FakeAccount] disable:flag 回写 false 失败(config 不可写),磁盘仍 fakeAccountModeEnabled=true → 下次启动可能重建合成账号"
        );
    }
    // 切活动回干净 apikey(只动合成账号,真账号不碰)。
    let _ = codex_real_account::deactivate_fake_account().await;
    // apply 当前 provider 强制 apikey:写真正的 apikey/gateway key + strip chatgpt_base_url。
    let synced =
        crate::admin::services::desktop::snapshot::sync_desktop_clearing_real_account(&state).await;
    let sync_ok = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 活动已非合成后再关 proxy 伪造(见上方顺序说明)。
    codex_app_transfer_proxy::set_fake_account_mode(false);
    let still_synthetic = codex_real_account::active_is_synthetic();
    // clean = flag 持久化了 + 不再是合成账号 + apply 成功写回了 apikey/gateway key。
    let clean = flag_persisted && !still_synthetic && sync_ok;
    Json(json!({
        "success": true,
        "switchedToApikey": !still_synthetic,
        "flagPersisted": flag_persisted,
        "message": if clean {
            "已关闭模拟账号模式(切回 apikey)"
        } else if !flag_persisted {
            "切换已生效但开关未能写入磁盘(权限 / 磁盘满?)—— 重启后可能恢复模拟账号,请检查后重试"
        } else if !still_synthetic {
            "已关闭开关并清除合成账号,但未能重配 provider(无可用 provider?)—— 请配置并激活一个 provider 后重启 Codex"
        } else {
            "已关闭开关,但活动仍是合成账号(磁盘 / 权限?)—— 请重试或重启 Codex"
        },
    }))
    .into_response()
}

/// 开模拟账号的共用收尾:落 flag=true + 开 proxy 伪造 + apply relay,并校验 relay 真生效。
/// 任一步未达成 → 回滚(关 flag、关 proxy 伪造、切回 apikey),返 Err。
async fn finalize_enable_fake_account(state: &AdminState) -> Result<(), String> {
    if !super::settings::set_fake_account_mode_enabled(true) {
        rollback_fake(state).await;
        return Err("写入模拟账号模式开关失败(配置文件不可写?),请检查权限 / 磁盘后重试".to_owned());
    }
    // 先开 proxy 伪造,再 apply —— apply 写 chatgpt_base_url 后 Codex 一旦发 /backend-api 即由 proxy 伪造,
    // 不会有「relay 已通但伪造未开 → 透传假 token 被 401」的窗口。
    codex_app_transfer_proxy::set_fake_account_mode(true);
    let synced =
        crate::admin::services::desktop::snapshot::sync_desktop_for_active_provider(state).await;
    let sync_ok = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 合成账号 auth_mode=chatgpt → active_is_real_chatgpt_now() 为真 = relay 已生效。
    if sync_ok && codex_real_account::active_is_real_chatgpt_now() {
        return Ok(());
    }
    rollback_fake(state).await;
    Err(
        "apply relay 失败(proxy 未起 / provider 异常),已回滚;请检查系统代理 / provider 后重试"
            .to_owned(),
    )
}

/// 回滚到关闭态:关 flag、切活动回 apikey、apply force_apikey、最后关 proxy 伪造(同 disable
/// 顺序,避免「伪造已关但活动仍合成」的 401 窗口)。best-effort,但失败留痕(回滚里二次失败会
/// 留下 flag/auth.json/proxy 不一致,需可诊断)。
async fn rollback_fake(state: &AdminState) {
    if !super::settings::set_fake_account_mode_enabled(false) {
        tracing::error!(
            "[FakeAccount] 回滚:flag 回写 false 失败(config 不可写),flag 残留 true → UI 可能误显 on"
        );
    }
    // deactivate 失败留痕:activate 已写了合成 auth.json,这里没切回去 → 活动残留合成账号(且 flag
    // 已关、不会再 reconcile 它),Codex 会对 /backend-api 透传假 token 撞 401,需手动检查。
    if let Err(e) = codex_real_account::deactivate_fake_account().await {
        tracing::error!(
            "[FakeAccount] 回滚:deactivate 失败,活动 auth.json 可能残留合成账号需手动检查: {e}"
        );
    }
    let _ =
        crate::admin::services::desktop::snapshot::sync_desktop_clearing_real_account(state).await;
    codex_app_transfer_proxy::set_fake_account_mode(false);
}

/// 组装路由 — 在 `admin/mod.rs` 调 `.merge(handlers::fake_account::routes())` 挂载。
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/desktop/fake-account/status", get(status_handler))
        .route("/api/desktop/fake-account/enable", post(enable_handler))
        .route("/api/desktop/fake-account/disable", post(disable_handler))
}
