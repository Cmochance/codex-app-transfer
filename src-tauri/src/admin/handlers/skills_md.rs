//! `/api/codex/skills-md/*` — Codex Skills 索引(SKILL.md raw 编辑 + 打开文件夹).
//!
//! - GET `/paths` — 扫 `~/.codex/skills/<name>/SKILL.md` 全部 entry(带 kind / child_count)
//! - GET `/raw?hash=<>` — 读 SKILL.md 全文
//! - POST `/raw?hash=<>` body { content } — 写 SKILL.md(pre-backup snapshot)
//! - POST `/backup?hash=<>` — 单独 snapshot 当前 SKILL.md 进 history
//! - POST `/restore-raw?hash=<>` body { index } — 从 history[index] 还原
//! - GET `/history?hash=<>` — 列 history snapshot
//! - POST `/reveal?hash=<>` — macOS `open <skill_dir>`(打开文件夹)

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::managed_block::HistoryEntry;
use super::super::services::skills_md_paths;
use super::common::err;

#[derive(Debug, Deserialize, Default)]
pub struct HashQuery {
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawWriteInput {
    pub content: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct RestoreInput {
    pub index: usize,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn resolve_target_and_history(hash: Option<&str>) -> Result<(PathBuf, PathBuf), String> {
    let h = hash.ok_or_else(|| "skills require ?hash=<>".to_owned())?;
    if h.is_empty() {
        return Err("skills require ?hash=<>".to_owned());
    }
    let target = skills_md_paths::resolve_path_by_hash(h)?;
    let path_hash = skills_md_paths::path_hash(&target);
    let history = skills_md_paths::history_file_for(&path_hash)?;
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
    match skills_md_paths::list_all_entries() {
        Ok(entries) => Json(json!({"success": true, "entries": entries})).into_response(),
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
    Json(input): Json<RestoreInput>,
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
    // atomic tmp+rename 防 crash 中段留 partial SKILL.md
    let tmp = target.with_extension("md.tmp");
    if let Err(e) = fs::write(&tmp, &restore_content) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write tmp: {e}")).into_response();
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")).into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn history(Query(q): Query<HashQuery>) -> impl IntoResponse {
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

/// macOS `open <skill_dir>`(Linux 用 `xdg-open`,Windows 用 `explorer`)
pub async fn reveal(Query(q): Query<HashQuery>) -> impl IntoResponse {
    let dir = match q.hash.as_deref() {
        Some(h) if !h.is_empty() => match skills_md_paths::resolve_dir_by_hash(h) {
            Ok(d) => d,
            Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
        },
        _ => return err(StatusCode::BAD_REQUEST, "missing hash".to_owned()).into_response(),
    };
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(target_os = "windows")]
    let cmd = ("explorer", vec![dir.to_string_lossy().into_owned()]);

    match Command::new(cmd.0).args(&cmd.1).spawn() {
        Ok(_) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("open dir: {e}")).into_response(),
    }
}
