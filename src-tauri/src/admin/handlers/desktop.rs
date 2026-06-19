//! `/api/desktop/*` + Codex.app 进程管理 + apply / restore 桌面状态.
//!
//! - 把 `~/.codex/{config.toml,auth.json}` 应用 / 还原
//! - Codex App 进程退出 / 重启(macOS / Windows / Linux)
//! - 桌面健康检查 + active provider 同步
//!

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_codex_integration::{
    discard_mcp_mirror, get_snapshot_status, has_snapshot, has_stale_active_snapshot,
    list_snapshots, repair_residual_pollution, restore_codex_snapshot, restore_codex_state,
    restore_mcp_credentials_from_mirror, scan_residual_pollution, CodexPaths,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::handlers::common::{err, open_directory};
use crate::admin::handlers::proxy::read_proxy_port;
use crate::admin::registry_io::load as load_registry;

// Re-export core services to preserve public API / downstream integration stability (e.g. called by main.rs)
pub use crate::admin::services::desktop::process::is_codex_app_running;
pub use crate::admin::services::desktop::snapshot::{
    auto_apply_on_startup_if_enabled, codex_openai_api_key_present, desktop_health,
    desktop_target_for_active_provider, mcp_credentials_on_setting_changed,
    mcp_credentials_startup_sync, read_codex_toml_root_string, restore_codex_if_enabled,
    switch_provider_and_sync,
};

use crate::admin::services::desktop::{process, snapshot};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopRestoreRequest {
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub cleanup_all: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResidualRepairRequest {
    /// `true` 时只返回 strip 计划不写盘,供 UI 预览。
    #[serde(default)]
    pub dry_run: bool,
}

/// 已知 transfer proxy 端口的历史默认值。当前 settings.proxyPort 与该常量都会
/// 参与 signature 匹配,覆盖"用户改过 port 后老 snapshot 仍保留旧 port"的场景。
const TRANSFER_PROXY_PORT_LEGACY_DEFAULT: u16 = 18080;

pub fn known_transfer_proxy_ports_for_startup() -> Vec<u16> {
    known_transfer_proxy_ports()
}

fn known_transfer_proxy_ports() -> Vec<u16> {
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    let current = read_proxy_port(&cfg);
    let mut ports = vec![current];
    if current != TRANSFER_PROXY_PORT_LEGACY_DEFAULT {
        ports.push(TRANSFER_PROXY_PORT_LEGACY_DEFAULT);
    }
    ports
}

// ── /api/desktop/* Axum HTTP Handlers ─────────────────────────────────

pub async fn desktop_clear() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // follow-up #28 P0 守门:无快照时**直接 noop 不动文件**。
    // [MOC-197] stale session 快照(被强杀 session 遗留)也算"有快照"——
    // restore_codex_state 内部会兜底还原它;只有 active/ 真空(从未 apply)才 noop。
    if !has_snapshot(&paths) && !has_stale_active_snapshot(&paths) {
        return Json(json!({
            "success": true,
            "restored": false,
            "message": "no snapshot to clear (本应用未对 ~/.codex/ 做过任何修改,无需清除)",
        }))
        .into_response();
    }
    match restore_codex_state(&paths) {
        Ok(restored) => {
            // [MOC-257 review] 还原原配置后已无解锁态(合成/真 auth + relay 都清了)→ 重置「最近生效」标记
            // + 关伪造,否则 status 仍报陈旧 last_applied 档、前端 no-op 点不动。
            crate::codex_real_account::reset_applied_mode();
            codex_app_transfer_proxy::set_fake_account_mode(false);
            Json(json!({"success": true, "restored": restored})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn desktop_snapshots() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(json!({
        "snapshots": list_snapshots(&paths),
    }))
    .into_response()
}

pub async fn desktop_restore(Json(payload): Json<DesktopRestoreRequest>) -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let snapshot_id = payload.snapshot_id.unwrap_or_default();
    match restore_codex_snapshot(&paths, &snapshot_id, payload.cleanup_all) {
        Ok(restored) => {
            // [MOC-257 review] 同 desktop_clear:还原快照后无解锁态 → 重置「最近生效」+ 关伪造。
            crate::codex_real_account::reset_applied_mode();
            codex_app_transfer_proxy::set_fake_account_mode(false);
            Json(json!({
                "success": true,
                "restored": restored,
                "snapshotId": if snapshot_id.is_empty() { Value::Null } else { Value::String(snapshot_id) },
                "cleanupAll": payload.cleanup_all,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /api/desktop/scan-residual` — #268 完整性自检.
///
/// 扫描 `~/.codex/config.toml` + active/recovery snapshots,返回所有含
/// transfer apply 残留字段的文件清单。详见
/// [`codex_app_transfer_codex_integration::residual`].
pub async fn desktop_scan_residual() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let ports = known_transfer_proxy_ports();
    match scan_residual_pollution(&paths, &ports) {
        Ok(report) => Json(report).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `POST /api/desktop/repair-residual` — 针对性 strip transfer 残留字段.
///
/// body: `{ "dryRun": bool }`(默认 `false`)。`dryRun=true` 时只返回 strip
/// 计划不写盘,UI 用来弹 preview。
pub async fn desktop_repair_residual(
    Json(payload): Json<ResidualRepairRequest>,
) -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let ports = known_transfer_proxy_ports();
    let report = match scan_residual_pollution(&paths, &ports) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match repair_residual_pollution(&report, payload.dry_run) {
        Ok(repair) => Json(json!({
            "success": true,
            "scan": report,
            "repair": repair,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-62:用户在"MCP 凭据文件丢失,从备份恢复?"确认里点**恢复** → 把镜像写回 live。
/// 仅当 live 仍缺失 / 空时才写(不覆盖已重新授权的 live);返回写回条数。
pub async fn mcp_credentials_restore() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match restore_mcp_credentials_from_mirror(&paths) {
        Ok(restored) => Json(json!({"success": true, "restored": restored})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-62:用户点**忽略** → 删镜像,接受"凭据已不在",停止每次启动重复弹确认(非破坏:
/// live 不动,日后重新授权会重新生成镜像)。
pub async fn mcp_credentials_discard() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match discard_mcp_mirror(&paths) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-62:前端 load 时轮询 —— 是否有可恢复的 MCP 凭据备份(>0 → 弹恢复确认)。
/// 比一次性 startup event 可靠(避免 event 在 listener 注册前 emit 丢失)。只读。
pub async fn mcp_credentials_status() -> impl IntoResponse {
    Json(json!({"restoreAvailable": snapshot::mcp_credentials_restore_status()})).into_response()
}

pub async fn desktop_snapshot_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let status = get_snapshot_status(&paths);
    Json(json!({
        "hasSnapshot": status.has_snapshot,
        "snapshotAt": status.snapshot_at,
        "configExisted": status.config_existed,
        "authExisted": status.auth_existed,
        "appVersion": status.app_version,
        "restorableCount": status.restorable_count,
        "recoveryCount": status.recovery_count,
    }))
    .into_response()
}

pub async fn restart_codex_app(State(state): State<crate::admin::AdminState>) -> impl IntoResponse {
    let desktop_sync = snapshot::sync_desktop_for_active_provider(&state).await;
    if desktop_sync.get("attempted").and_then(|v| v.as_bool()) == Some(true)
        && desktop_sync.get("success").and_then(|v| v.as_bool()) != Some(true)
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            desktop_sync
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Codex 配置同步失败"),
        )
        .into_response();
    }
    match process::launch_codex_app_restart(std::env::consts::OS) {
        Ok(_) => {
            // 通知 plugin_unlock daemon 重置 backoff 立刻重新 detect_cdp。
            let service = super::plugin_unlock::get_service().await;
            service.reinject().await;
            Json(json!({"success": true, "desktopSync": desktop_sync})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /api/desktop/open-snapshot-dir — 在系统文件管理器打开 Codex 原配置快照目录
/// (`~/.codex-app-transfer/codex-snapshots/active/`,内含各次 pre-apply 快照的
/// config.toml / auth.json / manifest.json),方便用户查找备份的原始配置。
pub async fn open_snapshot_dir() -> impl IntoResponse {
    let dir = match CodexPaths::from_home_env() {
        Ok(p) => p.active_snapshots_dir,
        Err(_) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "无法定位快照目录").into_response()
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建快照目录失败: {e}"),
        )
        .into_response();
    }
    match open_directory(&dir) {
        Ok(_) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
