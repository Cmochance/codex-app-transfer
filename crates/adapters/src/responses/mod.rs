//! `apiFormat == "responses"` 适配器(Stage 3.2c · 仅文本流骨架).
//!
//! 范围:
//! - **请求侧**:Stage 3.2a 才做完整 Responses → Chat body 转换;本轮先把
//!   path 从 `/v1/responses` 重定到 `/chat/completions`,body 透传(意味着
//!   端到端真实场景 Codex CLI → 上游会失败,因为 body schema 对不上;
//!   但**单元 / 集成测试可以独立 driving 响应侧**)。
//! - **响应侧**:Chat SSE → Responses SSE 状态机(text-only)。tool / reasoning /
//!   function call 留 Stage 3.3。

pub mod converter;
pub mod request;
pub mod stream;

pub use converter::ChatToResponsesConverter;
pub use request::responses_body_to_chat_body;
pub use stream::convert_chat_to_responses_stream;

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::{HeaderMap, HeaderValue, StatusCode};

use crate::types::{Adapter, AdapterError, ByteStream, RequestPlan, ResponsePlan};

#[derive(Debug, Default, Clone, Copy)]
pub struct ResponsesAdapter;

impl ResponsesAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Adapter for ResponsesAdapter {
    fn name(&self) -> &'static str {
        "responses"
    }

    fn prepare_request(
        &self,
        client_path: &str,
        body: Bytes,
        _provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        let upstream_path = redirect_responses_to_chat(client_path);
        // Stage 3.2a stateless:解析 body → Responses,转出 Chat 形态。
        // 失败时(body 非 JSON / 非对象)用 BadRequest 错出去,proxy 会回 400。
        let parsed: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AdapterError::BadRequest(format!("body 不是合法 JSON: {e}")))?;
        let chat_body = responses_body_to_chat_body(&parsed)?;
        let new_body = serde_json::to_vec(&chat_body)
            .map_err(|e| AdapterError::Internal(format!("re-serialize: {e}")))?;
        Ok(RequestPlan {
            upstream_path,
            body: Bytes::from(new_body),
        })
    }

    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        mut upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
    ) -> Result<ResponsePlan, AdapterError> {
        // 把 content-type 强制改成 text/event-stream(上游本来就是,但保险)
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream: convert_chat_to_responses_stream(upstream_stream),
        })
    }
}

/// 把 `/v1/responses` / `/responses` / `/openai/v1/responses` 等路径重定向到
/// `/chat/completions`(上游 OpenAI Chat 的标准入口)。其它路径透传不动。
fn redirect_responses_to_chat(path: &str) -> String {
    let normalized = path
        .strip_prefix("/openai")
        .unwrap_or(path)
        .strip_prefix("/v1")
        .map(|s| if s.is_empty() { "/" } else { s }.to_owned())
        .unwrap_or_else(|| path.to_owned());

    // 现在 normalized 形如 "/responses" 或 "/chat/completions" 或别的
    if let Some(after) = normalized.strip_prefix("/responses") {
        format!("/chat/completions{after}")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable_id() {
        assert_eq!(ResponsesAdapter.name(), "responses");
    }

    #[test]
    fn redirects_responses_to_chat_completions() {
        assert_eq!(
            redirect_responses_to_chat("/v1/responses"),
            "/chat/completions"
        );
        assert_eq!(
            redirect_responses_to_chat("/openai/v1/responses"),
            "/chat/completions"
        );
        assert_eq!(
            redirect_responses_to_chat("/responses"),
            "/chat/completions"
        );
        assert_eq!(
            redirect_responses_to_chat("/v1/responses?stream=1"),
            "/chat/completions?stream=1"
        );
    }

    #[test]
    fn passes_through_unrelated_paths() {
        assert_eq!(redirect_responses_to_chat("/v1/models"), "/models");
        assert_eq!(redirect_responses_to_chat("/health"), "/health");
    }
}
