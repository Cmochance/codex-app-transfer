//! `/api/codex/mcp/*` — MCP tab 三 sub-resource 统一入口:
//! - `/servers/*` — `[mcp_servers.*]` 结构化 CRUD
//! - `/plugins/*` — `~/.codex/plugins/cache/<market>/<plugin>/` 已安装 plugin 管理
//! - `/marketplace/*` — registry source 管理 + 索引聚合 + tar.gz 安装

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::{codex_plugins, marketplace, mcp_servers};
use super::common::err;

// ── Servers ──

#[derive(Debug, Deserialize, Default)]
pub struct DeleteServerInput {
    pub name: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RestoreInput {
    pub index: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawWriteInput {
    pub content: String,
}

pub async fn list_servers() -> impl IntoResponse {
    match mcp_servers::list_servers() {
        Ok(servers) => Json(json!({"success": true, "servers": servers})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn upsert_server(Json(spec): Json<mcp_servers::McpServerSpec>) -> impl IntoResponse {
    match mcp_servers::upsert_server(&spec) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn delete_server(Json(input): Json<DeleteServerInput>) -> impl IntoResponse {
    match mcp_servers::delete_server(&input.name) {
        Ok(removed) => Json(json!({"success": true, "removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn backup_servers() -> impl IntoResponse {
    match mcp_servers::snapshot_current() {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn restore_servers(Json(input): Json<RestoreInput>) -> impl IntoResponse {
    match mcp_servers::restore_from_history(input.index) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn history_servers() -> impl IntoResponse {
    let history = mcp_servers::read_history();
    let payload: Vec<_> = history
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            json!({
                "index": i,
                "appliedContent": entry.applied_content,
                "timestamp": entry.timestamp,
            })
        })
        .collect();
    Json(json!({"success": true, "history": payload})).into_response()
}

pub async fn raw_get_config() -> impl IntoResponse {
    match mcp_servers::read_raw() {
        Ok(content) => Json(json!({"success": true, "content": content})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn raw_write_config(Json(input): Json<RawWriteInput>) -> impl IntoResponse {
    match mcp_servers::write_raw(&input.content) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

// ── Plugins ──

#[derive(Debug, Deserialize, Default)]
pub struct PluginKeyInput {
    pub key: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PluginToggleInput {
    pub key: String,
    pub enabled: bool,
}

pub async fn list_plugins() -> impl IntoResponse {
    match codex_plugins::list_installed() {
        Ok(plugins) => Json(json!({"success": true, "plugins": plugins})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn toggle_plugin(Json(input): Json<PluginToggleInput>) -> impl IntoResponse {
    match codex_plugins::set_enabled(&input.key, input.enabled) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn uninstall_plugin(Json(input): Json<PluginKeyInput>) -> impl IntoResponse {
    match codex_plugins::uninstall(&input.key) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn install_plugin(Json(input): Json<codex_plugins::InstallInput>) -> impl IntoResponse {
    match codex_plugins::install_tarball(&input).await {
        Ok(entry) => Json(json!({"success": true, "entry": entry})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

// ── Marketplace ──

#[derive(Debug, Deserialize, Default)]
pub struct AddSourceInput {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RemoveSourceInput {
    pub id: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ToggleSourceInput {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct IndexQuery {
    #[serde(default)]
    pub force_refresh: bool,
}

pub async fn list_sources() -> impl IntoResponse {
    match marketplace::list_sources() {
        Ok(sources) => Json(json!({"success": true, "sources": sources})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn add_source(Json(input): Json<AddSourceInput>) -> impl IntoResponse {
    match marketplace::add_source(input.name, input.url) {
        Ok(src) => Json(json!({"success": true, "source": src})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn remove_source(Json(input): Json<RemoveSourceInput>) -> impl IntoResponse {
    match marketplace::remove_source(&input.id) {
        Ok(removed) => Json(json!({"success": true, "removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn toggle_source(Json(input): Json<ToggleSourceInput>) -> impl IntoResponse {
    match marketplace::toggle_source(&input.id, input.enabled) {
        Ok(updated) => Json(json!({"success": true, "updated": updated})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn marketplace_index(Query(q): Query<IndexQuery>) -> impl IntoResponse {
    match marketplace::index(q.force_refresh).await {
        Ok(idx) => Json(json!({"success": true, "index": idx})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
