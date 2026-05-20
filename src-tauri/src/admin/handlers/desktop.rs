//! `/api/desktop/*` + Codex.app 进程管理 + apply / restore 桌面状态.
//!
//! - 把 `~/.codex/{config.toml,auth.json}` 应用 / 还原
//! - Codex App 进程退出 / 重启(macOS / Windows / Linux)
//! - 桌面健康检查 + active provider 同步
//!

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_codex_integration::{
    get_snapshot_status, has_snapshot, list_snapshots, restore_codex_snapshot, restore_codex_state,
    CodexPaths,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::handlers::common::err;
use crate::admin::handlers::proxy::read_proxy_port;
use crate::admin::registry_io::load as load_registry;

// Re-export core services to preserve public API / downstream integration stability (e.g. called by main.rs)
pub use crate::admin::services::desktop::process::is_codex_app_running;
pub use crate::admin::services::desktop::snapshot::{
    auto_apply_on_startup_if_enabled, restore_codex_if_enabled, switch_provider_and_sync,
    read_codex_toml_root_string, codex_openai_api_key_present,
    desktop_target_for_active_provider, desktop_health,
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

// ── /api/desktop/* Axum HTTP Handlers ─────────────────────────────────

pub async fn desktop_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let configured = has_snapshot(&paths);
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    let proxy_port = read_proxy_port(&cfg);
    let actual_base_url = snapshot::read_codex_toml_root_string(&paths, "openai_base_url");
    let actual_api_key_present = snapshot::codex_openai_api_key_present(&paths);
    let desktop_target = snapshot::desktop_target_for_active_provider(&cfg);
    let fallback_base_url = desktop_target
        .as_ref()
        .map(|target| target.base_url.clone())
        .unwrap_or_else(|| format!("http://127.0.0.1:{proxy_port}"));
    let api_key_present = actual_api_key_present
        || desktop_target
            .as_ref()
            .map(|target| !target.api_key.is_empty())
            .unwrap_or_else(|| !crate::admin::handlers::proxy::read_gateway_key(&cfg).is_empty());
    let health = snapshot::desktop_health(
        Some(&paths),
        configured,
        actual_base_url.as_deref(),
        actual_api_key_present,
        desktop_target.as_ref(),
    );
    Json(json!({
        "configured": configured,
        "health": health,
        "keys": {
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": actual_base_url.unwrap_or(fallback_base_url),
            "inferenceGatewayApiKey": if api_key_present { "******" } else { "" },
            "inferenceGatewayAuthScheme": "bearer",
            "inferenceModels": snapshot::desktop_inference_models_json(desktop_target.as_ref()),
        },
    }))
    .into_response()
}

pub async fn desktop_configure() -> impl IntoResponse {
    let target_result = crate::admin::registry_io::with_config_write(|cfg| {
        let Some(active) = crate::admin::handlers::providers::active_provider(cfg) else {
            return Err("add a provider first".into());
        };
        let target = snapshot::desktop_config_target_for_provider(cfg, &active, None);
        Ok(crate::admin::registry_io::ConfigMutation::Modified(target))
    });
    let target = match target_result {
        Ok(t) => t,
        Err(e) if e == "add a provider first" => {
            return err(StatusCode::BAD_REQUEST, e).into_response();
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match snapshot::apply_desktop_target(&target) {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("success".into(), Value::Bool(true));
                obj.insert("mode".into(), Value::String(target.mode.to_owned()));
                obj.insert("requiresProxy".into(), Value::Bool(target.requires_proxy));
            }
            Json(result).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn desktop_clear() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // follow-up #28 P0 守门:无快照时**直接 noop 不动文件**。
    if !has_snapshot(&paths) {
        return Json(json!({
            "success": true,
            "restored": false,
            "message": "no snapshot to clear (本应用未对 ~/.codex/ 做过任何修改,无需清除)",
        }))
        .into_response();
    }
    match restore_codex_state(&paths) {
        Ok(restored) => Json(json!({"success": true, "restored": restored})).into_response(),
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
        Ok(restored) => Json(json!({
            "success": true,
            "restored": restored,
            "snapshotId": if snapshot_id.is_empty() { Value::Null } else { Value::String(snapshot_id) },
            "cleanupAll": payload.cleanup_all,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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

pub async fn restart_codex_app() -> impl IntoResponse {
    match process::launch_codex_app_restart(std::env::consts::OS) {
        Ok(_) => {
            // 通知 plugin_unlock daemon 重置 backoff 立刻重新 detect_cdp。
            let service = super::plugin_unlock::get_service().await;
            service.reinject().await;
            Json(json!({"success": true})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
