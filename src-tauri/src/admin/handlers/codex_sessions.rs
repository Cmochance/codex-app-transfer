//! `/api/codex-sessions/*` — [CAT-255] 导入 / 恢复其他工具留下的隔离会话.
//!
//! 其他工具(cc-switch 等)给 Codex 写了第三方 `model_provider`,这些会话在 transfer
//! (锚点 = openai)视图下被隐藏。本组端点:
//! - `GET  /api/codex-sessions/detect-foreign` → 扫出第三方会话(只读,Codex 运行时也安全)
//! - `POST /api/codex-sessions/import`         → 全部第三方就地归一成 openai(transfer 可见)
//! - `POST /api/codex-sessions/restore`        → 把选中会话的 model_provider 写成用户指定值(其他工具可见)
//!
//! import / restore **写 Codex 独占的 `state_<N>.sqlite`**,所以这两个端点经
//! `process::with_codex_closed` 先退出 Codex、写完再重启(全程持维护锁,跟其他 Codex
//! 维护流程互斥)。机制见 `conversation_export::repair`。

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_codex_integration::CodexPaths;
use codex_app_transfer_conversation_export as cexp;
use codex_app_transfer_proxy::proxy_telemetry;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

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
/// **detect-before-quit**:先只读探测,0 条就直接返回、不白白强杀 + 重启用户的 Codex。
pub async fn import_handler() -> impl IntoResponse {
    let home = match codex_home() {
        Ok(p) => p,
        Err(r) => return r,
    };
    match cexp::detect_foreign_sessions(&home) {
        Ok(f) if f.is_empty() => {
            return Json(json!({
                "success": true, "imported": 0, "failed": [], "codexRelaunched": true
            }))
            .into_response();
        }
        Ok(_) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
    run_codex_write("import", home, |h| cexp::import_foreign_sessions(h)).await
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
    let ids = body.session_ids;
    run_codex_write("restore", home, move |h| {
        cexp::set_sessions_provider(h, &ids, &target, false)
    })
    .await
}

/// import / restore 共用:**spawn_blocking 包住「关 Codex→写 state DB→重启」**(别堵 tokio
/// worker),统一成 `{ success, imported, failed, codexRelaunched }` 响应。
/// 退出失败 → 报错不写;`work` 报错 → with_codex_closed 已重启 Codex,带 relaunch 状态报错;
/// 部分失败 → success=false + 完整 failed 列表(前端 raw-fetch 读 body,绝不把部分失败报成功)。
async fn run_codex_write(
    op: &'static str,
    home: PathBuf,
    work: impl FnOnce(&Path) -> Result<cexp::RepairResult, cexp::ExportError> + Send + 'static,
) -> axum::response::Response {
    let os = std::env::consts::OS;
    let outcome =
        tokio::task::spawn_blocking(move || process::with_codex_closed(os, || work(&home))).await;

    let (work_result, relaunched) = match outcome {
        Ok(Ok(pair)) => pair,
        // with_codex_closed 退出 Codex 失败 → 没写 DB
        Ok(Err(e)) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("退出 Codex 失败,未改动会话:{e}"),
            )
            .into_response()
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("任务执行失败:{e}"),
            )
            .into_response()
        }
    };

    let result = match work_result {
        Ok(r) => r,
        Err(e) => {
            // 写失败:Codex 已被 with_codex_closed 拉回来(若原本开着);带重启状态报错
            let tail = if relaunched {
                String::new()
            } else {
                "(且 Codex 未能自动重启,请手动打开)".into()
            };
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}{tail}")).into_response();
        }
    };

    let success = result.failed.is_empty();
    proxy_telemetry().logs.add(
        "INFO",
        format!(
            "[CAT-255] codex-sessions {op}: {} ok, {} failed, codex relaunched={relaunched}",
            result.repaired.len(),
            result.failed.len(),
        ),
    );

    let mut body = json!({
        "success": success,
        "imported": result.repaired.len(),
        "failed": result.failed,
        "codexRelaunched": relaunched,
    });
    if !success {
        body["message"] = json!(format!(
            "{} 条成功,{} 条失败",
            result.repaired.len(),
            result.failed.len()
        ));
    }
    Json(body).into_response()
}
