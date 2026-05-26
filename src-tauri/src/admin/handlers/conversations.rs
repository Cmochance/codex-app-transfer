//! `/api/conversations/*` — Codex CLI rollout 对话导出 (#271).
//!
//! - `GET  /api/conversations/list` → SessionMeta[]
//! - `GET  /api/conversations/{id}` → NormalizedSession JSON
//! - `POST /api/conversations/export` body `{ sessionIds, format, options }`
//!   → 单条返回内容(文本/JSON);多条返回 zip 字节流。前端拿到后调
//!   `dialog.save()` 让用户选目标路径落盘。

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use codex_app_transfer_codex_integration::CodexPaths;
use codex_app_transfer_conversation_export as cexp;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::admin::handlers::common::err;

/// 找一条 session(按 id)对应的 rollout 文件路径。线性扫 list(422 量级毫秒内)
fn find_session_path(id: &str, codex_home: &std::path::Path) -> Option<PathBuf> {
    let sessions = cexp::list_sessions(codex_home).ok()?;
    sessions.into_iter().find(|s| s.id == id).map(|s| s.path)
}

fn codex_home_from_env() -> Result<PathBuf, axum::response::Response> {
    match CodexPaths::from_home_env() {
        Ok(p) => Ok(p.codex_home),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    }
}

pub async fn list_handler() -> impl IntoResponse {
    let codex_home = match codex_home_from_env() {
        Ok(p) => p,
        Err(r) => return r,
    };
    match cexp::list_sessions(&codex_home) {
        Ok(sessions) => Json(json!({ "sessions": sessions })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn detail_handler(Path(id): Path<String>) -> impl IntoResponse {
    let codex_home = match codex_home_from_env() {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(path) = find_session_path(&id, &codex_home) else {
        return err(StatusCode::NOT_FOUND, format!("session not found: {id}")).into_response();
    };
    match cexp::parse_session(&path) {
        Ok(s) => Json(s).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub session_ids: Vec<String>,
    /// `"markdown"` | `"json"` | `"jsonl"`
    pub format: String,
    #[serde(default)]
    pub options: cexp::ExportOptions,
    /// 可选:服务端落盘目标绝对路径。前端用 Tauri dialog.save() 让用户选好后
    /// 传过来,backend 写入并返回 `{ success, path, bytes }`。**单条**导出
    /// 时是文件路径;**多条**导出时也是单个 zip 文件路径(zip 内部多个 entry)。
    /// 不传 → 走 HTTP body 返回(老路径,前端可能 fallback download)。
    #[serde(default)]
    pub target_path: Option<String>,
}

pub async fn export_handler(Json(req): Json<ExportRequest>) -> impl IntoResponse {
    let codex_home = match codex_home_from_env() {
        Ok(p) => p,
        Err(r) => return r,
    };
    if req.session_ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "sessionIds must be non-empty".to_string(),
        )
        .into_response();
    }
    let format = req.format.as_str();
    if !matches!(format, "markdown" | "json" | "jsonl") {
        return err(
            StatusCode::BAD_REQUEST,
            format!("unknown format: {format} (expected markdown / json / jsonl)"),
        )
        .into_response();
    }

    // 准备 bytes + filename + mime(无论是否落盘都要先 render):
    // - 单条 → render_one 直接给一份
    // - 多条 → 全部 render + 打 zip
    let (bytes, default_filename, mime) = if req.session_ids.len() == 1 {
        let id = &req.session_ids[0];
        let Some(path) = find_session_path(id, &codex_home) else {
            return err(StatusCode::NOT_FOUND, format!("session not found: {id}")).into_response();
        };
        match render_one(&path, format, &req.options) {
            Ok(t) => t,
            Err(e) => return e,
        }
    } else {
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(req.session_ids.len());
        for id in &req.session_ids {
            let Some(path) = find_session_path(id, &codex_home) else {
                return err(StatusCode::NOT_FOUND, format!("session not found: {id}"))
                    .into_response();
            };
            match render_one(&path, format, &req.options) {
                Ok((bytes, name, _mime)) => entries.push((sanitize_filename(&name), bytes)),
                Err(e) => return e,
            }
        }
        if let Err(e) = cexp::write_bulk_zip(&mut buf, entries) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        let zip_name = format!(
            "codex-conversations-{}.zip",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        (buf.into_inner(), zip_name, "application/zip")
    };

    // target_path 给了 → 服务端写盘 + 返回 JSON;没给 → 老路径 HTTP body 下载
    if let Some(target) = req.target_path.as_deref().filter(|s| !s.trim().is_empty()) {
        let target_path = std::path::PathBuf::from(target);
        if let Some(parent) = target_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to create parent dir: {e}"),
                    )
                    .into_response();
                }
            }
        }
        let bytes_len = bytes.len();
        if let Err(e) = std::fs::write(&target_path, &bytes) {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to write {}: {e}", target_path.display()),
            )
            .into_response();
        }
        return Json(json!({
            "success": true,
            "path": target_path.display().to_string(),
            "bytes": bytes_len,
        }))
        .into_response();
    }

    let safe_name = sanitize_filename(&default_filename);
    let mut response = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{safe_name}\"")
            .parse()
            .unwrap(),
    );
    response
}

fn render_one(
    path: &std::path::Path,
    format: &str,
    opts: &cexp::ExportOptions,
) -> Result<(Vec<u8>, String, &'static str), axum::response::Response> {
    let base_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    match format {
        "jsonl" => match cexp::read_raw_jsonl(path) {
            Ok(b) => Ok((b, format!("{base_name}.jsonl"), "application/jsonl")),
            Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
        },
        _ => {
            let session = match cexp::parse_session(path) {
                Ok(s) => s,
                Err(e) => {
                    return Err(
                        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                    )
                }
            };
            match format {
                "markdown" => Ok((
                    cexp::export_markdown(&session, opts).into_bytes(),
                    format!("{base_name}.md"),
                    "text/markdown; charset=utf-8",
                )),
                "json" => {
                    let v = cexp::export_json(&session, opts);
                    let bytes = serde_json::to_vec_pretty(&v).unwrap_or_default();
                    Ok((bytes, format!("{base_name}.json"), "application/json"))
                }
                _ => unreachable!("validated above"),
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRequest {
    pub session_ids: Vec<String>,
}

/// `POST /api/conversations/delete` — 把选中的 rollout 文件**移到回收站**(可恢复)。
/// 不彻底删除,用户在 Finder Trash 还能找回来。
pub async fn delete_handler(Json(req): Json<DeleteRequest>) -> impl IntoResponse {
    let codex_home = match codex_home_from_env() {
        Ok(p) => p,
        Err(r) => return r,
    };
    if req.session_ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "sessionIds must be non-empty".to_string(),
        )
        .into_response();
    }
    match cexp::move_sessions_to_trash(&codex_home, &req.session_ids) {
        Ok(result) => Json(serde_json::json!({
            "success": true,
            "deleted": result.deleted,
            "failed": result.failed,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// 把 session id / 时间戳里可能的 `/` `\` 等剔掉,生成安全文件名。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
