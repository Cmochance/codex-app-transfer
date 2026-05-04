//! axum router 构造与启动 helper.

use axum::{routing::any, Router};

use crate::forward::{forward_handler, ProxyState};
use crate::resolver::SharedResolver;

/// 把所有方法 / 所有路径都路由到 `forward_handler`(裸代理 + B1 路由 + B2 鉴权改写)。
/// Stage 3 起此 router 会再叠 adapter 中间件(provider 协议转换)。
pub fn build_router(resolver: SharedResolver) -> Router {
    let state = ProxyState::new(resolver);
    Router::new()
        .fallback(any(forward_handler))
        .with_state(state)
}
