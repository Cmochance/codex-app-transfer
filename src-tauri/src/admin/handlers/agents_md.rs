//! `/api/codex/agents-md/*` — Codex CLI 全局 AGENTS.md 受管块管理.
//!
//! 6 endpoints (借鉴 AiMaMi:src-tauri/src/commands/custom_instructions.rs):
//! - GET `/status` — 当前受管块状态 + history 数量 + 上次 apply
//! - POST `/preview` — body { content: String } → 返写盘前完整 file 内容(diff 用)
//! - POST `/apply` — body { content: String } → 真写盘 + 推 history snapshot
//! - POST `/rollback` — body { index: usize } → 还原 history[index]
//! - POST `/clear` — 删 marker + managed 段, 还原到 app 介入前
//! - GET `/history` — 列 history snapshot (最多 HISTORY_LIMIT 条)

use std::path::PathBuf;

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::managed_block::{
    ManagedBlock, ManagedBlockError, MarkdownManagedBlock,
};
use super::common::err;

/// 解析 `~/` 路径(支持 macOS / Linux / Windows USERPROFILE)
fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

/// 构造 AGENTS.md 受管块实例。
///
/// target = `~/.codex/AGENTS.md`
/// history = `~/.codex-app-transfer/managed-history/agents.json`
/// (history 放本工具自己的数据目录,避免污染 Codex 目录)
fn build_block() -> Result<MarkdownManagedBlock, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(MarkdownManagedBlock {
        block_type: "agents",
        target: home.join(".codex").join("AGENTS.md"),
        history: home
            .join(".codex-app-transfer")
            .join("managed-history")
            .join("agents.json"),
    })
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApplyInput {
    pub content: String,
    pub expected_outer_signature: Option<String>,
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
    match block.apply(&input.content, input.expected_outer_signature.as_deref()) {
        Ok(()) => match block.status_json() {
            Ok(v) => Json(json!({"success": true, "status": v})).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        Err(e) => {
            let status = match e {
                ManagedBlockError::ProtectedCollision(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            err(status, e.to_string()).into_response()
        }
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
