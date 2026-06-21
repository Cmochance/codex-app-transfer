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
use codex_app_transfer_proxy::proxy_telemetry;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::admin::handlers::common::err;

/// 找一条 session(按 id)对应的 rollout 文件路径。
///
/// **single-shot 用**(detail 端点 1 个 id)。**批量场景**(export N 个 ids)
/// 必须用 [`build_session_index_map`] 一次扫 + HashMap 查,否则 N 次
/// list_sessions 是 O(N×M) 全目录扫(devin #272 review fix)。
///
/// `Ok(None)` = 真没找到该 id;`Err(_)` = list_sessions 失败(IO / perm)。
/// 之前用 `.ok()?` 把两者糊在一起 → 真正的目录读权限错被报成 404 "not found",
/// 用户去找不存在的问题(devin #272 silent-failure-hunter fix)。
fn find_session_path(
    id: &str,
    codex_home: &std::path::Path,
) -> Result<Option<PathBuf>, cexp::ExportError> {
    let sessions = cexp::list_sessions(codex_home)?;
    Ok(sessions.into_iter().find(|s| s.id == id).map(|s| s.path))
}

/// 构建 `session_id → path` 索引,**只扫一次目录**。批量端点 export 用,
/// 循环内 O(1) 查表替代 N 次 list_sessions。
fn build_session_index_map(
    codex_home: &std::path::Path,
) -> Result<std::collections::HashMap<String, PathBuf>, axum::response::Response> {
    let sessions = match cexp::list_sessions(codex_home) {
        Ok(s) => s,
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    };
    let mut map = std::collections::HashMap::with_capacity(sessions.len());
    for s in sessions {
        map.insert(s.id, s.path);
    }
    Ok(map)
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
    let path = match find_session_path(&id, &codex_home) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return err(StatusCode::NOT_FOUND, format!("session not found: {id}")).into_response()
        }
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    match cexp::parse_session(&path) {
        Ok(mut s) => {
            // **devin #272 code-reviewer fix**:detail 同样从 session_index.jsonl
            // 注入 thread_name,跟 list view 一致(否则用户在 list 看 "分析数据",
            // 点开变 "cwd-basename (019df883)" 像换了 session)
            if let Some(ref mut meta) = s.meta {
                if meta.title.is_none() {
                    let titles = cexp::read_session_index_titles(&codex_home);
                    if let Some(name) = titles.get(&meta.id).cloned() {
                        meta.title = Some(name);
                    }
                }
            }
            Json(s).into_response()
        }
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

    // devin #272 review fix:**只扫一次目录**构 HashMap,循环内 O(1) 查
    let index = match build_session_index_map(&codex_home) {
        Ok(m) => m,
        Err(r) => return r,
    };

    // 准备 bytes + filename + mime(无论是否落盘都要先 render):
    // - 单条 → render_one 直接给一份
    // - 多条 → 全部 render + 打 zip
    let (bytes, default_filename, mime) = if req.session_ids.len() == 1 {
        let id = &req.session_ids[0];
        let Some(path) = index.get(id) else {
            return err(StatusCode::NOT_FOUND, format!("session not found: {id}")).into_response();
        };
        match render_one(path, format, &req.options) {
            Ok(t) => t,
            Err(e) => return e,
        }
    } else {
        // devin #272 silent-failure-hunter MED-6: 批量场景下单个 id 缺失不再
        // 整批 abort,跟 delete_handler 的 partial 语义一致;skipped 收集到
        // 单独列表,后续打进 response(target_path 模式的 JSON,或 zip 里附
        // skipped.txt 文件)
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(req.session_ids.len());
        let mut skipped: Vec<String> = Vec::new();
        for id in &req.session_ids {
            let Some(path) = index.get(id) else {
                skipped.push(id.clone());
                continue;
            };
            match render_one(path, format, &req.options) {
                Ok((bytes, name, _mime)) => entries.push((sanitize_filename(&name), bytes)),
                Err(_) => skipped.push(id.clone()),
            }
        }
        if entries.is_empty() {
            return err(
                StatusCode::NOT_FOUND,
                format!("all {} sessions failed; none in index", skipped.len()),
            )
            .into_response();
        }
        if !skipped.is_empty() {
            entries.push((
                "skipped.txt".to_string(),
                format!(
                    "Sessions skipped during export (not found / render failed):\n{}\n",
                    skipped.join("\n")
                )
                .into_bytes(),
            ));
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
        // **devin #272 code-reviewer fix #2 (security)**:
        // 1. 必须绝对路径(避免 cwd 相对引用 leak 到不可预期位置)
        // 2. 禁含 `..` 组件(防 path traversal)
        // 3. 父目录**必须已存在**,不自动 create_dir_all(让用户预先 dialog 挑过
        //    的目录写入是正常 case;静默建目录把整套 CSRF-style 攻击面打开)
        if !target_path.is_absolute() {
            return err(
                StatusCode::BAD_REQUEST,
                "targetPath must be absolute".to_string(),
            )
            .into_response();
        }
        if target_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return err(
                StatusCode::BAD_REQUEST,
                "targetPath must not contain `..` segments".to_string(),
            )
            .into_response();
        }
        if let Some(parent) = target_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                return err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "parent directory does not exist: {} — pick a valid folder first",
                        parent.display()
                    ),
                )
                .into_response();
            }
        }
        // **devin #272 silent-failure-hunter fix (HIGH-4)**:原子写入避免部分写
        // 留下半截文件 — write to `<name>.part` 再 rename 同 fs 原子。
        let tmp_path = target_path.with_extension({
            let mut s = target_path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !s.is_empty() {
                s.push('.');
            }
            s.push_str("part");
            s
        });
        let bytes_len = bytes.len();
        if let Err(e) = std::fs::write(&tmp_path, &bytes) {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to write {}: {e}", tmp_path.display()),
            )
            .into_response();
        }
        if let Err(e) = std::fs::rename(&tmp_path, &target_path) {
            // best-effort 清理 part 文件
            let _ = std::fs::remove_file(&tmp_path);
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to finalize {}: {e}", target_path.display()),
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
                    // devin #272 silent-failure-hunter HIGH-3: 序列化失败必须
                    // 报 500,不能 unwrap_or_default 写 0 字节假装成功
                    match serde_json::to_vec_pretty(&v) {
                        Ok(bytes) => Ok((bytes, format!("{base_name}.json"), "application/json")),
                        Err(e) => Err(err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("json serialize failed: {e}"),
                        )
                        .into_response()),
                    }
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
        Ok(result) => {
            // devin #272 silent-failure-hunter MED-7: 全部失败时 success=false
            // (而不是 success=true + 空 deleted),前端能据此弹错误而非"部分成功"
            let success = !result.deleted.is_empty();
            Json(serde_json::json!({
                "success": success,
                "deleted": result.deleted,
                "failed": result.failed,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `POST /api/conversations/clear-all` — 一键清空本地会话历史(两者都清):
/// ① 先清 proxy 的 L2 Responses 续轮缓存(`~/.codex-app-transfer/sessions.db` 全表 + 内存 hot cache)。
/// ② 再把全部 Codex 对话 rollout(`~/.codex/sessions` + `~/.codex/archived_sessions` 的 `.jsonl`;
///    冷归档 `.jsonl.zst` 当前不含、见 MOC-214)**移到系统回收站**(可恢复)。
///
/// **顺序**:先做可再生的缓存清除——若 db 不可达直接报错,此时 rollout 还没动、磁盘状态一致;
/// 缓存清完再 trash rollout,trash 部分失败不报 500、收进 `failed` 由前端逐条提示。
///
/// **注意**:清 L2 缓存会让正在进行的 Codex 会话下一轮 cache-miss → OpenAI 400
/// `previous_response_not_found` → fail-fast(需重发)。确认弹窗已告知用户。
///
/// 前端设置页「会话历史」→「清空会话历史」按钮(二次确认)调用。非破坏:rollout 走系统回收站、
/// 不彻底删,用户可在 Finder Trash 恢复。无会话时也照常清缓存。
pub async fn clear_all_handler() -> impl IntoResponse {
    let codex_home = match codex_home_from_env() {
        Ok(p) => p,
        Err(r) => return r,
    };

    // ① 先清 proxy L2 续轮缓存(db 不可达 → 直接报错,此时 rollout 未动)
    let cache_rows =
        match codex_app_transfer_adapters::responses::session::global_response_session_cache()
            .clear_all_persisted()
        {
            Ok(rows) => rows,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("clear sessions.db failed: {e}"),
                )
                .into_response()
            }
        };

    // ② 再把全部 rollout 移回收站(单次扫描;部分失败收进 failed、不报 500)
    let trash = match cexp::move_all_sessions_to_trash(&codex_home) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    // 有任何 rollout 移失败 → success=false,前端据此弹错误 + 逐条 failed 提示
    // (对齐 delete_handler 的 MED-7:绝不把「全/部分失败」报成成功)。
    let success = trash.failed.is_empty();
    proxy_telemetry().logs.add(
        "INFO",
        format!(
            "session history cleared by admin: {} rollout trashed, {} failed, {cache_rows} L2 rows removed",
            trash.deleted.len(),
            trash.failed.len(),
        ),
    );

    Json(json!({
        "success": success,
        "sessionsTrashed": trash.deleted.len(),
        "sessionsFailed": trash.failed.len(),
        "failed": trash.failed,
        "cacheRowsRemoved": cache_rows,
    }))
    .into_response()
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
