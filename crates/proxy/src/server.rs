//! axum router 构造与启动 helper.

use axum::{routing::any, Router};

use crate::forward::{forward_handler, ProxyState};
use crate::resolver::SharedResolver;

/// 把所有 HTTP 方法 / 所有路径都路由到 `forward_handler`。
/// WebSocket upgrade 不走这个 fallback router;当前 proxy 只承诺 HTTP/SSE。
pub fn build_router(resolver: SharedResolver) -> Router {
    let state = ProxyState::new(resolver);
    Router::new()
        .fallback(any(forward_handler))
        .with_state(state)
}
