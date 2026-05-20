//! `/api/codex/mcp-toml/*` — Codex CLI `~/.codex/config.toml` 中 `[mcp_servers.*]` 段
//! 的受管块管理(借鉴 borawong/AiMaMi:src-tauri/src/core/mcp.rs).
//!
//! 6 endpoints 跟 agents_md.rs 完全对称,差异只在 target_path + block_type + marker
//! style (TOML 走行注释 `#` 而非 HTML 注释 `<!-- -->`)。

use std::path::PathBuf;

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::managed_block::{ManagedBlock, TomlManagedBlock};
use super::common::err;

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

fn build_block() -> Result<TomlManagedBlock, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(TomlManagedBlock {
        block_type: "mcp",
        target: home.join(".codex").join("config.toml"),
        history: home
            .join(".codex-app-transfer")
            .join("managed-history")
            .join("mcp.json"),
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct ApplyInput {
    pub content: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RollbackInput {
    pub index: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct PreviewQuery {
    pub content: Option<String>,
}

pub async fn status() -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match block.status_json() {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn preview(body: Option<Json<ApplyInput>>) -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let content = body.map(|j| j.0.content).unwrap_or_default();
    match block.preview(&content) {
        Ok(rendered) => Json(json!({
            "success": true,
            "rendered": rendered,
            "newManaged": content,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn apply(Json(input): Json<ApplyInput>) -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match block.apply(&input.content) {
        Ok(()) => match block.status_json() {
            Ok(v) => Json(json!({"success": true, "status": v})).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn rollback(Json(input): Json<RollbackInput>) -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match block.rollback(input.index) {
        Ok(()) => match block.status_json() {
            Ok(v) => Json(json!({"success": true, "status": v})).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn clear() -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match block.clear() {
        Ok(()) => match block.status_json() {
            Ok(v) => Json(json!({"success": true, "status": v})).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn history(_q: Query<PreviewQuery>) -> impl IntoResponse {
    let block = match build_block() {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let hist = block.read_history().unwrap_or_default();
    let payload: Vec<_> = hist
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            json!({
                "index": i,
                "managedContent": entry.managed_content,
                "appliedContent": entry.applied_content,
                "timestamp": entry.timestamp,
            })
        })
        .collect();
    Json(json!({
        "success": true,
        "history": payload,
    }))
    .into_response()
}
