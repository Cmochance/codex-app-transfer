//! `/api/trace-viewer/*` —— 诊断流量查看器(MOC-169)生命周期 + 浏览器打开。
//!
//! 前端「诊断模式」开关 on → `start`(置位运行时采集 gate + 起独立端口 SSE 服务);off →
//! `stop`(清 gate + 关服务)。开关本身的持久化走 `save_settings`(`traceViewerEnabled`),
//! 启动自启在 `main.rs` setup 里按持久化值处理。

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use super::super::state::AdminState;
use super::common::open_url;
use crate::trace_viewer::DEFAULT_TRACE_VIEWER_PORT;

fn url_of(addr: std::net::SocketAddr) -> String {
    format!("http://{addr}")
}

/// 开启诊断:起查看器(幂等)。**采集 gate 的开关由 `manager.start`/`stop_silent` 在 start_lock
/// 内与 viewer 生命周期原子绑定**(gate 仅在 start 成功后开、失败不开 → 无残留;并发 on/off
/// 按锁顺序串行,最后一次胜),handler 不再单独动 gate。`start` 内部同步 block 到 bind 完成,
/// 放 `spawn_blocking` 不卡 async worker。
pub async fn start_trace_viewer(State(state): State<AdminState>) -> impl IntoResponse {
    let mgr = state.trace_viewer_manager.clone();
    let result = tokio::task::spawn_blocking(move || mgr.start(DEFAULT_TRACE_VIEWER_PORT))
        .await
        .unwrap_or_else(|e| Err(format!("trace-viewer start task panicked: {e}")));
    match result {
        Ok(addr) => Json(json!({"success": true, "running": true, "url": url_of(addr)})),
        // `message` 键:前端 api() 因 success:false throw 时取 data.message 作错误文案。
        Err(e) => Json(json!({"success": false, "running": false, "message": e})),
    }
}

/// 关闭诊断:关查看器(`stop_silent` 内部清运行时采集 gate;env `CAS_DIAG_TRACE` 不受影响)。
pub async fn stop_trace_viewer(State(state): State<AdminState>) -> impl IntoResponse {
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
            // gate 由 manager.start 内部管理(成功才开),handler 不动 gate。
            let mgr = state.trace_viewer_manager.clone();
            let started = tokio::task::spawn_blocking(move || mgr.start(DEFAULT_TRACE_VIEWER_PORT))
                .await
                .unwrap_or_else(|e| Err(format!("trace-viewer start task panicked: {e}")));
            match started {
                Ok(addr) => addr,
                Err(e) => return Json(json!({"success": false, "message": e})),
            }
        }
    };
    let url = url_of(addr);
    match open_url(&url) {
        Ok(()) => Json(json!({"success": true, "url": url})),
        Err(e) => Json(json!({"success": false, "url": url, "message": e})),
    }
}
