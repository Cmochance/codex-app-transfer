//! Codex App Transfer · 代理转发主干.
//!
//! 当前实现:
//! - **B1 多 provider 路由**:`StaticResolver` 按 body `model = "<slug>/<real>"`
//!   匹配 provider,失败 fallback 到 `default_provider_id`。
//! - **B2 鉴权改写**:剥掉 incoming `Authorization`,按 `provider.auth_scheme`
//!   注入 `Bearer <api_key>` 或 `X-Api-Key`,再叠 `provider.extra_headers`。
//! - **Adapter 协议层**:按 `provider.api_format` 选择 `openai_chat` 或
//!   `responses` adapter,完成请求 body 与流式响应转换。
//! - **HTTP/SSE 转发**:body 完整读取 → 必要时改写 model → reqwest 发起 →
//!   响应字节流(`bytes_stream`)灌回 axum。
//!
//! 当前 proxy router 没有注册 WebSocket upgrade 路由;对外承诺的是 HTTP/SSE
//! 转发入口。

pub mod fixture;
pub mod forward;
pub mod resolver;
pub mod server;
pub mod telemetry;

pub use forward::{forward_handler, ProxyState};
pub use resolver::{
    AuthScheme, ProviderResolver, ResolveError, ResolvedProvider, SharedResolver, StaticResolver,
};
pub use server::build_router;
pub use telemetry::{proxy_log_dir, proxy_telemetry, ProxyLogEntry, ProxyStatsSnapshot};
