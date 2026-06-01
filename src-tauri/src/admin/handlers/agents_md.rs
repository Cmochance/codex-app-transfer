//! `/api/codex/agents-md/*` — Codex 全局 + 自定义路径 AGENTS.md 受管块管理.
//!
//! 6 file-ops endpoints(每个接 `?hash=<>` 定位具体 AGENTS.md):
//! - GET `/status?hash=<>` — 当前受管块状态 + history 数量 + 上次 apply
//! - POST `/preview?hash=<>` — body { content: String } → 返写盘前完整 file 内容(diff 用)
//! - POST `/apply?hash=<>` — body { content: String } → 真写盘 + 推 history snapshot
//! - POST `/rollback?hash=<>` — body { index: usize } → 还原 history[index]
//! - POST `/clear?hash=<>` — 删 marker + managed 段, 还原到 app 介入前
//! - GET `/history?hash=<>` — 列 history snapshot (最多 HISTORY_LIMIT 条)
//!
//! `hash` 缺省 → 默认全局 `~/.codex/AGENTS.md`(等价于 path_hash(~/.codex/AGENTS.md))。
//!
//! 3 paths-management endpoints:
//! - GET `/paths` — 列 dropdown 全部条目(全局首条 + 用户自定义,带 category 标签)
//! - POST `/paths/add` — body { path: String } → 添加自定义路径
//! - POST `/paths/remove` — body { hash: String } → 删自定义路径(全局删不掉)

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::agents_md_paths;
use super::super::services::managed_block::{
    HistoryEntry, ManagedBlock, ManagedBlockError, MarkdownManagedBlock,
};
use super::common::err;

/// 构造 AGENTS.md 受管块实例 — 根据 hash 解析 target path(缺省 → 全局)。
fn build_block_for_hash(hash: Option<&str>) -> Result<MarkdownManagedBlock, String> {
    let target = match hash {
        Some(h) if !h.is_empty() => agents_md_paths::resolve_path_by_hash(h)?,
        _ => agents_md_paths::validated_global_agents_path()?,
    };
    let path_hash = agents_md_paths::path_hash(&target);
    let history = agents_md_paths::history_file_for(&path_hash)?;
    Ok(MarkdownManagedBlock {
        block_type: "agents",
        target,
        history,
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct HashQuery {
    #[serde(default)]
    pub hash: Option<String>,
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
pub struct AddPathInput {
    pub path: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RemovePathInput {
    pub hash: String,
}

pub async fn status(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match block.status_json() {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn preview(
    Query(q): Query<HashQuery>,
    body: Option<Json<ApplyInput>>,
) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
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

pub async fn apply(Query(q): Query<HashQuery>, Json(input): Json<ApplyInput>) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
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

pub async fn rollback(
    Query(q): Query<HashQuery>,
    Json(input): Json<RollbackInput>,
) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
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

pub async fn clear(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
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

pub async fn history(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let block = match build_block_for_hash(q.hash.as_deref()) {
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

/// 列 dropdown 全部条目(全局首条 + 用户自定义)
pub async fn list_paths() -> impl IntoResponse {
    match agents_md_paths::list_all_entries() {
        Ok(entries) => Json(json!({
            "success": true,
            "entries": entries,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 添加自定义路径
pub async fn add_path(Json(input): Json<AddPathInput>) -> impl IntoResponse {
    match agents_md_paths::add_path(&input.path) {
        Ok(entry) => Json(json!({"success": true, "entry": entry})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// 删自定义路径(全局删不掉)
pub async fn remove_path(Json(input): Json<RemovePathInput>) -> impl IntoResponse {
    match agents_md_paths::remove_by_hash(&input.hash) {
        Ok(removed) => Json(json!({"success": true, "removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Raw 全文 read/write 模式(Agents tab 新 UX,绕开 marker 受管块) ──
//
// 设计:
// - GET /raw?hash=<> → 读整个文件内容
// - POST /raw?hash=<> body { content } → 写之前先 backup 当前到 history,再 fs::write
// - POST /backup?hash=<> → 单独 snapshot 当前 file 全文到 history(不动 file)
// - POST /restore-raw?hash=<> body { index } → 把 history[index].applied_content 直接写回
//
// History schema 复用现有 HistoryEntry:`managed_content` 留空,`applied_content` =
// raw file 全文。marker 模式的 history 跟 raw 模式 history **共享同一文件**,
// 但前端按"managed_content 空" 判断是 raw entry。

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

/// 解析 hash → target_path + history_path
fn resolve_target_and_history(hash: Option<&str>) -> Result<(PathBuf, PathBuf), String> {
    let target = match hash {
        Some(h) if !h.is_empty() => agents_md_paths::resolve_path_by_hash(h)?,
        _ => agents_md_paths::validated_global_agents_path()?,
    };
    let path_hash = agents_md_paths::path_hash(&target);
    let history = agents_md_paths::history_file_for(&path_hash)?;
    Ok((target, history))
}

/// 读 history file → Vec<HistoryEntry>
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

/// 写 history file(原子 rename),自动 trim 到最近 10 条
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

/// 把当前 file 全文 snapshot 进 history,带**内容去重**:
/// - 如果已有 `applied_content` 完全一致的条目 → 删掉旧位置,把它"提升"到末尾(更新 timestamp)
/// - 否则 → 正常 push 新条目
///
/// 这避免反复 backup / pre-apply backup 产生大量重复条目。
fn snapshot_current_to_history(target: &PathBuf, history_path: &PathBuf) -> Result<(), String> {
    let content = if target.exists() {
        fs::read_to_string(target).map_err(|e| format!("read target: {e}"))?
    } else {
        String::new()
    };
    let mut history = read_history_raw(history_path);
    // 去重:扫整个 history,找到 applied_content 完全一致的条目则删旧位置
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

/// GET /raw — 返 file 全文 + exists
pub async fn raw_get(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let (target, _history) = match resolve_target_and_history(q.hash.as_deref()) {
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

/// POST /raw — 先 snapshot 当前到 history,再 fs::write 新 content
pub async fn raw_write(
    Query(q): Query<HashQuery>,
    Json(input): Json<RawWriteInput>,
) -> impl IntoResponse {
    let (target, history_path) = match resolve_target_and_history(q.hash.as_deref()) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    // pre-write backup
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

/// POST /backup — 单独 snapshot 当前 file 到 history,不动 file
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

/// POST /restore-raw — 把 history[index].applied_content 直接写回 target,**写前先 snapshot 当前**
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
    // pre-restore backup current
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
