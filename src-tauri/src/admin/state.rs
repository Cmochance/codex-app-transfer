//! Admin server 共享状态.

use std::sync::Arc;

use crate::proxy_runner::ProxyManager;
use crate::trace_viewer::TraceViewerManager;

#[derive(Clone)]
pub struct AdminState {
    pub proxy_manager: Arc<ProxyManager>,
    /// [MOC-169] 诊断流量查看器(独立端口 SSE)生命周期管理,admin toggle 端点用。
    pub trace_viewer_manager: Arc<TraceViewerManager>,
}
