//! `/api/desktop/*` + Codex.app 进程管理 + apply / restore 桌面状态.
//!
//! - 把 `~/.codex/{config.toml,auth.json}` 应用 / 还原
//! - Codex App 进程退出 / 重启(macOS / Windows / Linux)
//! - 桌面健康检查 + active provider 同步
//!

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_codex_integration::{
    get_snapshot_status, has_snapshot, has_stale_active_snapshot, ignore_mcp_credentials_keys,
    list_recovery, list_snapshots, remove_mcp_credentials_keys, repair_residual_pollution,
    restore_codex_snapshot, restore_codex_state, restore_mcp_credentials_keys,
    scan_residual_pollution, CodexPaths, RecoveryItem,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::handlers::common::{err, open_directory};
use crate::admin::handlers::proxy::read_proxy_port;
use crate::admin::registry_io::load as load_registry;

// Re-export core services to preserve public API / downstream integration stability (e.g. called by main.rs)
pub use crate::admin::services::desktop::snapshot::{
    auto_apply_on_startup_if_enabled, mcp_credentials_on_setting_changed,
    mcp_credentials_startup_sync, restore_codex_if_enabled, switch_provider_and_sync,
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
    // follow-up #28 P0 守门:无 Codex 配置快照时**不动 config/auth 文件**。
    // [MOC-197] stale session 快照(被强杀 session 遗留)也算"有快照"——
    // restore_codex_state 内部会兜底还原它;只有 active/ 真空(从未 apply)才 noop。
    if !has_snapshot(&paths) && !has_stale_active_snapshot(&paths) {
        // [MOC-277 followup] 无 Codex 配置快照,但 superpowers 可能已被 reconcile 端点装上(切到
        // Antigravity 后翻开关、尚未 apply 建快照)。此分支里卸载是**唯一**清理动作 → 失败必须
        // surface(否则 UI 谎报清理成功但插件仍在);未装则维持原 noop 文案。
        if !crate::admin::services::superpowers::is_managed_installed() {
            return Json(json!({
                "success": true,
                "restored": false,
                "message": "no snapshot to clear (本应用未对 ~/.codex/ 做过任何修改,无需清除)",
            }))
            .into_response();
        }
        if let Err(e) = crate::admin::services::superpowers::uninstall() {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("卸载 superpowers 约束插件失败: {e}"),
            )
            .into_response();
        }
        return Json(json!({
            "success": true,
            "restored": true,
            "message": "已卸载 superpowers 约束插件(无 Codex 配置快照需还原)",
        }))
        .into_response();
    }
    // [MOC-257 review] 顺序 mirror exit/startup:**先 restore_codex,再 un-stash 真账号**(真账号最终写)。
    // 反序(先 un-stash 再 restore_codex)会让 restore_codex 在 stash 还原出的真账号上 merge 旧快照 managed auth
    // key,快照拍于 ChatGPT 登录前(无 auth_mode)时抹掉真账号 genuine 的 auth_mode=chatgpt → tokens 在但不被认作
    // ChatGPT。改 restore_codex 先(还原 config + strip transfer key),再整文件覆盖回真账号。
    let restored = match restore_codex_state(&paths) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // [MOC-277] 有快照的 clear:restore 成功**之后**再卸 superpowers —— 放 restore 之前会在 restore
    // 失败时留下"superpowers 已删但 Codex 配置仍 Transfer-applied Antigravity"的不一致。restore 是主
    // 清理,此处卸载失败 best-effort 只 log(无快照路径才把卸载失败 surface,见上)。
    if let Err(e) = crate::admin::services::superpowers::uninstall() {
        tracing::warn!("[MOC-277] superpowers uninstall on clear failed: {e}");
    }
    // un-stash 失败 → **abort + surface**,别静默吞:restore_stashed_impl 先删活动再 rename stash,Windows 文件锁/
    // 权限失败会留 auth.json 缺失;真账号未丢(rename 失败=仍在 stash),报错让用户重启自愈。
    if let Err(e) = crate::codex_real_account::restore_stashed_real_auth().await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("还原真账号失败(真账号仍安全在 stash,重启 Codex App Transfer 会自动恢复): {e}"),
        )
        .into_response();
    }
    // 还原后已无解锁态 → 重置「最近生效」+ 关伪造,否则 status 报陈旧 last_applied 档、前端 no-op 点不动。
    crate::codex_real_account::reset_applied_mode();
    codex_app_transfer_proxy::set_fake_account_mode(false);
    Json(json!({"success": true, "restored": restored})).into_response()
}

/// [MOC-277] GET /api/desktop/superpowers/status — 设置页据此算开关 effective 默认 + 展示检测态
/// (用户已自装 superpowers → 默认关,避免双装)。
pub async fn superpowers_status() -> impl IntoResponse {
    use crate::admin::services::superpowers;
    let explicit = load_registry()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get(superpowers::SETTING_KEY))
        .and_then(|v| v.as_bool());
    let foreign = superpowers::user_has_foreign_superpowers();
    Json(json!({
        "effectiveEnabled": explicit.unwrap_or(!foreign),
        "explicit": explicit,
        "foreignDetected": foreign,
        "managedInstalled": superpowers::is_managed_installed(),
        "version": superpowers::vendored_version(),
    }))
    .into_response()
}

/// [MOC-277] POST /api/desktop/superpowers/reconcile — 翻开关后即时按当前 active provider + 新设置
/// 装/卸内置 superpowers(否则要等下次 apply / Codex relaunch)。返回收敛后的 status。
pub async fn superpowers_reconcile() -> impl IntoResponse {
    snapshot::reconcile_superpowers_from_config();
    superpowers_status().await
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
    // [MOC-257 review] 同 desktop_clear:顺序 mirror exit/startup —— 先 restore_codex,再 un-stash 真账号(真
    // 账号最终写,保 genuine auth_mode 不被旧快照 merge 抹掉)。
    let restored = match restore_codex_snapshot(&paths, &snapshot_id, payload.cleanup_all) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // [MOC-277] 快照还原(有快照时前端走这条而非 /clear)→ 同样卸载我方挂载的 superpowers
    // (只动受管 market,不碰用户自装),否则还原回 pristine 后约束插件仍残留。
    if let Err(e) = crate::admin::services::superpowers::uninstall() {
        tracing::warn!("[MOC-277] superpowers uninstall on snapshot restore failed: {e}");
    }
    // un-stash 失败 → abort + surface(真账号仍安全在 stash,重启自愈)。
    if let Err(e) = crate::codex_real_account::restore_stashed_real_auth().await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("还原真账号失败(真账号仍安全在 stash,重启 Codex App Transfer 会自动恢复): {e}"),
        )
        .into_response();
    }
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

/// MOC-261 一-4:逐条恢复操作的入参 —— 要处理的 server_key 列表。
#[derive(Deserialize)]
pub struct McpRecoveryKeys {
    #[serde(default)]
    pub keys: Vec<String>,
}

/// MOC-62 / 一-4:**选择性恢复** —— 把 body 里的 server_key 从镜像写回 live(不覆盖 live 已有),
/// 并从恢复状态清除。返回真正写回条数。
pub async fn mcp_credentials_restore(Json(req): Json<McpRecoveryKeys>) -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match restore_mcp_credentials_keys(&paths, &req.keys) {
        Ok(restored) => Json(json!({"success": true, "restored": restored})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-261 一-4:**选择性移除** —— 从镜像 + 恢复状态删除 body 里的 server_key(用户「不要这些备份」)。
/// 不动 live;镜像清空则删文件。返回删除条数。
pub async fn mcp_credentials_remove(Json(req): Json<McpRecoveryKeys>) -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match remove_mcp_credentials_keys(&paths, &req.keys) {
        Ok(removed) => Json(json!({"success": true, "removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-261 一-4:**标记忽略** —— body 里的 server_key 设为已忽略(留备份 + 列表,不再触发自动弹窗)。
pub async fn mcp_credentials_ignore(Json(req): Json<McpRecoveryKeys>) -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match ignore_mcp_credentials_keys(&paths, &req.keys) {
        Ok(ignored) => Json(json!({"success": true, "ignored": ignored})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// MOC-62 / 一-4:前端 load 时轮询 —— 返回逐条待处理恢复项(server_key + 是否已忽略)+ 待处理
/// (未忽略)条数。`pending>0` → 自动弹窗;设置入口据 `entries` 显示状态。只读(ensure 副作用仅
/// 写恢复状态文件)。
pub async fn mcp_credentials_status() -> impl IntoResponse {
    // 保险箱开关关 → 不提示恢复(尊重用户关闭意图,与旧 mcp_credentials_restore_status gate 一致)。
    let enabled = load_registry()
        .ok()
        .and_then(|c| {
            c.get("settings")
                .and_then(|s| s.get("mcpCredentialsPortableStore"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true);
    if !enabled {
        return Json(json!({"pending": 0, "restoreAvailable": 0, "entries": []})).into_response();
    }
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // 恢复态不可信(读/写失败)→ 500,前端 refresh 的 catch 会静默不展示弹窗 / 入口,
    // 避免在状态无法持久化时让用户执行恢复而丢未处理备份(silent-failure 防线)。
    let items: Vec<RecoveryItem> = match list_recovery(&paths) {
        Ok(items) => items,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let pending = items.iter().filter(|i| !i.ignored).count();
    let entries: Vec<Value> = items
        .iter()
        .map(|i| json!({"key": i.key, "ignored": i.ignored}))
        .collect();
    Json(json!({
        "pending": pending,
        // 向后兼容:restoreAvailable 仍 = 待处理(未忽略)条数。
        "restoreAvailable": pending,
        "entries": entries,
    }))
    .into_response()
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
