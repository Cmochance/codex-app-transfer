//! `/api/trace-viewer/*` —— 诊断流量查看器(MOC-169)生命周期 + 浏览器打开。
//!
//! 前端「诊断模式」开关 on → `start`(置位运行时采集 gate + 起独立端口 SSE 服务);off →
//! `stop`(清 gate + 关服务)。开关本身的持久化走 `save_settings`(`traceViewerEnabled`),
//! 启动自启在 `main.rs` setup 里按持久化值处理。

use axum::{extract::State, response::IntoResponse, Json};
use codex_app_transfer_proxy::diagnostics::set_forward_trace_enabled;
use serde_json::json;

use super::super::state::AdminState;
use super::common::open_url;
use crate::trace_viewer::DEFAULT_TRACE_VIEWER_PORT;

fn url_of(addr: std::net::SocketAddr) -> String {
    format!("http://{addr}")
}

/// 开启诊断:置位运行时采集 gate + 起查看器(幂等)。返回 viewer URL。
/// `start` 内部同步 block 到 bootstrap 线程 bind 完成,放 `spawn_blocking` 不卡 async worker。
pub async fn start_trace_viewer(State(state): State<AdminState>) -> impl IntoResponse {
    set_forward_trace_enabled(true);
    let mgr = state.trace_viewer_manager.clone();
    let result = tokio::task::spawn_blocking(move || mgr.start(DEFAULT_TRACE_VIEWER_PORT))
        .await
        .unwrap_or_else(|e| Err(format!("trace-viewer start task panicked: {e}")));
    match result {
        Ok(addr) => Json(json!({"success": true, "running": true, "url": url_of(addr)})),
        Err(e) => Json(json!({"success": false, "running": false, "error": e})),
    }
}

/// 关闭诊断:清运行时采集 gate + 关查看器。env `CAS_DIAG_TRACE` 开的不受影响(env 恒真)。
pub async fn stop_trace_viewer(State(state): State<AdminState>) -> impl IntoResponse {
    set_forward_trace_enabled(false);
    state.trace_viewer_manager.stop_silent();
    Json(json!({"success": true, "running": false}))
}

/// 当前运行状态 + URL(前端渲染开关/按钮用)。
pub async fn trace_viewer_status(State(state): State<AdminState>) -> impl IntoResponse {
    let addr = state.trace_viewer_manager.addr();
    Json(json!({
        "running": addr.is_some(),
        "url": addr.map(url_of),
    }))
}

/// 用系统浏览器打开查看器(未运行先尝试 start)。
pub async fn open_trace_viewer(State(state): State<AdminState>) -> impl IntoResponse {
    let addr = match state.trace_viewer_manager.addr() {
        Some(addr) => addr,
        None => {
            set_forward_trace_enabled(true);
            let mgr = state.trace_viewer_manager.clone();
            let started = tokio::task::spawn_blocking(move || mgr.start(DEFAULT_TRACE_VIEWER_PORT))
                .await
                .unwrap_or_else(|e| Err(format!("trace-viewer start task panicked: {e}")));
            match started {
                Ok(addr) => addr,
                Err(e) => return Json(json!({"success": false, "error": e})),
            }
        }
    };
    let url = url_of(addr);
    match open_url(&url) {
        Ok(()) => Json(json!({"success": true, "url": url})),
        Err(e) => Json(json!({"success": false, "url": url, "error": e})),
    }
}
