//! `/api/codex-sessions/*` — [CAT-255] 导入 / 恢复其他工具留下的隔离会话.
//!
//! 其他工具(cc-switch 等)给 Codex 写了第三方 `model_provider`,这些会话在 transfer
//! (锚点 = openai)视图下被隐藏。本组端点:
//! - `GET  /api/codex-sessions/detect-foreign` → 扫出第三方会话(只读,Codex 运行时也安全)
//! - `POST /api/codex-sessions/import`         → 全部第三方就地归一成 openai(transfer 可见)
//! - `POST /api/codex-sessions/restore`        → 把选中会话的 model_provider 写成用户指定值(其他工具可见)
//!
//! import / restore **写 Codex 独占的 `state_<N>.sqlite`**,所以这两个端点负责
//! 先退出 Codex、写完再重启(用户在弹窗选「关闭Codex」即授权了这一流程)。
//! 机制见 `conversation_export::repair`。

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_codex_integration::CodexPaths;
use codex_app_transfer_conversation_export as cexp;
use codex_app_transfer_proxy::proxy_telemetry;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::admin::handlers::common::err;
use crate::admin::services::desktop::process;

fn codex_home() -> Result<PathBuf, axum::response::Response> {
    match CodexPaths::from_home_env() {
        Ok(p) => Ok(p.codex_home),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    }
}

/// GET `/api/codex-sessions/detect-foreign` → `{ success, count, sessions:[ForeignSession] }`.
/// 只读打开 state DB,Codex 运行时调用也安全。前端启动时调,count>0 才弹导入提示;
/// 前端同时记录 `sessions`(含各自 model_provider)供「恢复」下拉框用。
pub async fn detect_foreign_handler() -> impl IntoResponse {
    let home = match codex_home() {
        Ok(p) => p,
        Err(r) => return r,
    };
    match cexp::detect_foreign_sessions(&home) {
        Ok(sessions) => Json(json!({
            "success": true,
            "count": sessions.len(),
            "sessions": sessions,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST `/api/codex-sessions/import` → 关闭 Codex → 所有第三方会话归一成 openai → 重启 Codex.
pub async fn import_handler() -> impl IntoResponse {
    let home = match codex_home() {
        Ok(p) => p,
        Err(r) => return r,
    };
    with_codex_closed_write("import", |home| cexp::import_foreign_sessions(home), home)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreBody {
    /// 要写回的 session id(前端从上次扫描记录里、按所选 provider 过滤出来)。
    pub session_ids: Vec<String>,
    /// 写入的 model_provider(用户从下拉框选的、扫描记录到的第三方值)。
    pub model_provider: String,
}

/// POST `/api/codex-sessions/restore` body `{ sessionIds, modelProvider }`
/// → 关闭 Codex → 把这些会话的 model_provider 写成指定值 → 重启 Codex.
/// 导入的逆操作:让对应工具(cc-switch 等)重新看到这些会话。
pub async fn restore_handler(Json(body): Json<RestoreBody>) -> impl IntoResponse {
    let home = match codex_home() {
        Ok(p) => p,
        Err(r) => return r,
    };
    let target = body.model_provider.trim().to_owned();
    if target.is_empty() {
        return err(StatusCode::BAD_REQUEST, "modelProvider 不能为空").into_response();
    }
    if body.session_ids.is_empty() {
        return err(StatusCode::BAD_REQUEST, "sessionIds 不能为空").into_response();
    }
    with_codex_closed_write(
        "restore",
        move |home| cexp::set_sessions_provider(home, &body.session_ids, &target, false),
        home,
    )
}

/// import / restore 共用:**退出 Codex → 跑 `work`(写 state DB)→ 重启 Codex**,
/// 统一成 `{ success, imported, failed, codexRelaunched }` 响应。
/// 退出失败 → 直接报错不写 DB;`work` 报错 → 仍把 Codex 拉回来再报错。
fn with_codex_closed_write(
    op: &str,
    work: impl FnOnce(&std::path::Path) -> Result<cexp::RepairResult, cexp::ExportError>,
    home: PathBuf,
) -> axum::response::Response {
    let os = std::env::consts::OS;

    // ① 退出 Codex(失败直接报错,绝不在它可能运行时写它的 DB)
    let was_running = match process::quit_codex_app_blocking(os) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("退出 Codex 失败,未改动会话:{e}"),
            )
            .into_response()
        }
    };

    // ② 写 state DB + rollout
    let result = match work(&home) {
        Ok(r) => r,
        Err(e) => {
            // 失败也要把 Codex 拉回来(用户原本开着的)
            if was_running {
                let _ = process::launch_codex_app(os);
            }
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // ③ 重启 Codex(用户选「关闭Codex」就期望写完自动重启;原本没开则不擅自拉起)
    let relaunched = was_running && process::launch_codex_app(os).is_ok();

    let success = result.failed.is_empty();
    proxy_telemetry().logs.add(
        "INFO",
        format!(
            "[CAT-255] codex-sessions {op}: {} ok, {} failed, codex relaunched={relaunched}",
            result.repaired.len(),
            result.failed.len(),
        ),
    );

    Json(json!({
        "success": success,
        "imported": result.repaired.len(),
        "failed": result.failed,
        "codexRelaunched": relaunched,
    }))
    .into_response()
}
