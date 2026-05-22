//! `/api/codex/memories-md/*` — Codex CLI `~/.codex/memories/MEMORY.md` 受管块管理.
//!
//! 跟 agents_md.rs 6 endpoints 完全对称, 差异只在 target_path + block_type。
//! MEMORY.md 是 209k+ 层次化 markdown(`# Task Group → ## Task → ### subsections`),
//! 跟 Claude Code MEMORY.md 索引模式一致 — app 通过 marker 区物理隔离,
//! 永远不动用户长期积累的 209k 用户区。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::managed_block::{
    HistoryEntry, ManagedBlock, ManagedBlockError, MarkdownManagedBlock,
};
use super::super::services::memories_md_paths;
use super::common::err;

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

/// target = `~/.codex/memories/MEMORY.md` (层次化 markdown 索引)
/// history = `~/.codex-app-transfer/managed-history/memories.json`
fn build_block() -> Result<MarkdownManagedBlock, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(MarkdownManagedBlock {
        block_type: "memories",
        target: home.join(".codex").join("memories").join("MEMORY.md"),
        history: home
            .join(".codex-app-transfer")
            .join("managed-history")
            .join("memories.json"),
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

#[derive(Debug, Deserialize, Default)]
pub struct HashQuery {
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AddPathInput {
    pub path: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RemovePathInput {
    pub hash: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawWriteInput {
    pub content: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn resolve_target_and_history(hash: Option<&str>) -> Result<(PathBuf, PathBuf), String> {
    let h = hash.ok_or_else(|| "memories require ?hash=<>".to_owned())?;
    if h.is_empty() {
        return Err("memories require ?hash=<>".to_owned());
    }
    let target = memories_md_paths::resolve_path_by_hash(h)?;
    let path_hash = memories_md_paths::path_hash(&target);
    let history = memories_md_paths::history_file_for(&path_hash)?;
    Ok((target, history))
}

fn read_history_raw(history_path: &PathBuf) -> Vec<HistoryEntry> {
    if !history_path.exists() {
        return Vec::new();
    }
    let raw = match fs::read_to_string(history_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn write_history_raw(history_path: &PathBuf, mut history: Vec<HistoryEntry>) -> Result<(), String> {
    const LIMIT: usize = 10;
    if history.len() > LIMIT {
        let drop = history.len() - LIMIT;
        history.drain(..drop);
    }
    if let Some(parent) = history_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir history parent: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(&history).map_err(|e| format!("serialize: {e}"))?;
    let tmp = history_path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, history_path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

fn snapshot_current_to_history(target: &PathBuf, history_path: &PathBuf) -> Result<(), String> {
    let content = if target.exists() {
        fs::read_to_string(target).map_err(|e| format!("read target: {e}"))?
    } else {
        String::new()
    };
    let mut history = read_history_raw(history_path);
    if let Some(pos) = history.iter().position(|e| e.applied_content == content) {
        history.remove(pos);
    }
    history.push(HistoryEntry {
        managed_content: String::new(),
        applied_content: content,
        timestamp: now_unix(),
    });
    write_history_raw(history_path, history)
}

pub async fn list_paths() -> impl IntoResponse {
    match memories_md_paths::list_all_entries() {
        Ok(entries) => Json(json!({"success": true, "entries": entries})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn add_path(Json(input): Json<AddPathInput>) -> impl IntoResponse {
    match memories_md_paths::add_path(&input.path) {
        Ok(entry) => Json(json!({"success": true, "entry": entry})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn remove_path(Json(input): Json<RemovePathInput>) -> impl IntoResponse {
    match memories_md_paths::remove_by_hash(&input.hash) {
        Ok(removed) => Json(json!({"success": true, "removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn raw_get(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let (target, _hist) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    if !target.exists() {
        return Json(json!({
            "success": true,
            "exists": false,
            "content": "",
            "targetPath": target.display().to_string(),
        }))
        .into_response();
    }
    let content = match fs::read_to_string(&target) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read failed: {e}"),
            )
            .into_response()
        }
    };
    Json(json!({
        "success": true,
        "exists": true,
        "content": content,
        "targetPath": target.display().to_string(),
    }))
    .into_response()
}

pub async fn raw_write(
    Query(q): Query<HashQuery>,
    Json(input): Json<RawWriteInput>,
) -> impl IntoResponse {
    let (target, history_path) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    if let Err(e) = snapshot_current_to_history(&target, &history_path) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(parent) = target.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")).into_response();
        }
    }
    let tmp = target.with_extension("md.tmp");
    if let Err(e) = fs::write(&tmp, &input.content) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write tmp: {e}")).into_response();
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")).into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn backup(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let (target, history_path) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    match snapshot_current_to_history(&target, &history_path) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn restore_raw(
    Query(q): Query<HashQuery>,
    Json(input): Json<RollbackInput>,
) -> impl IntoResponse {
    let (target, history_path) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let history = read_history_raw(&history_path);
    let Some(entry) = history.get(input.index) else {
        return err(
            StatusCode::BAD_REQUEST,
            format!("history index out of range: {}", input.index),
        )
        .into_response();
    };
    let restore_content = entry.applied_content.clone();
    if let Err(e) = snapshot_current_to_history(&target, &history_path) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(parent) = target.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")).into_response();
        }
    }
    if let Err(e) = fs::write(&target, &restore_content) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")).into_response();
    }
    Json(json!({"success": true})).into_response()
}

/// raw 模式 history endpoint — 用户加的项目级 path,跟 marker history file 共用 但 schema 兼容
pub async fn history_raw(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let (_target, history_path) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let hist = read_history_raw(&history_path);
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
    Json(json!({"success": true, "history": payload})).into_response()
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
