//! Adapter trait 与协议数据结构.

use std::pin::Pin;

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use futures_core::Stream;
use http::{HeaderMap, StatusCode};
use thiserror::Error;

/// Adapter 内部 / 上下游间使用的字节流.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>>;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("body decode: {0}")]
    BodyDecode(#[from] serde_json::Error),
    #[error("internal: {0}")]
    Internal(String),
}

/// 单次请求经过 adapter 后,proxy 拿到的"出站计划".
#[derive(Debug, Clone)]
pub struct RequestPlan {
    /// 相对 `provider.base_url` 的上游路径(以 `/` 开头),
    /// 例如 `/chat/completions`。proxy 会用 `{base}{this}` 拼最终 URL。
    pub upstream_path: String,
    /// 改写后的请求体。openai_chat 路径下与入参等同。
    pub body: Bytes,
}

/// adapter 处理完上游响应后,交给 proxy 的"回灌计划".
pub struct ResponsePlan {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub stream: ByteStream,
}

/// 适配器接口。**所有方法须保持无内部状态**(状态走方法参数),
/// 让单个 adapter 实例可在多个并发请求间安全共享。
pub trait Adapter: Send + Sync {
    /// 唯一标识,用于日志与 registry 匹配,与 `apiFormat` 字符串对齐。
    fn name(&self) -> &'static str;

    /// 接收入站请求(已剥掉 gateway auth),输出上游路径与改写后的 body。
    ///
    /// `client_path` 是入站请求的路径(可能含 query),例如
    /// `/v1/chat/completions?stream=true`。
    fn prepare_request(
        &self,
        client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError>;

    /// 接收上游响应的状态 + 头 + 流,产出回灌给客户端的状态 + 头 + 流。
    ///
    /// 默认实现是 0 转换的透传(用于 openai_chat)。Stage 3.2 起的
    /// SSE 状态机适配器会重写这一方法。
    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
    ) -> Result<ResponsePlan, AdapterError> {
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream: upstream_stream,
        })
    }
}
