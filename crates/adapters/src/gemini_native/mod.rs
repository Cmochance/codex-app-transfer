//! Gemini native `generateContent` adapter(`apiFormat=gemini_native`)。
//!
//! 设计:跟 OpenAiChatAdapter / ResponsesAdapter 同级,实现 `Adapter` trait。
//! 接 Codex.app /responses 入站,直接转 Gemini RequestBody,不依赖
//! ResponsesAdapter(用户决策 2026-05-10:web_search 等 Gemini 关键工具
//! 不能被 ResponsesAdapter 的 provider-specific drop 吃掉)。
//!
//! 模块结构:
//! - `types.rs` — Gemini wire types(Content/Part/Tool/RequestBody/Candidate/...)
//! - `request.rs` — Codex.app /responses → Gemini RequestBody 转换
//!   - `responses_body_to_normalized_chat`(本地归一化,不依赖 ResponsesAdapter)
//!   - `chat_normalized_to_gemini_request`(LiteLLM 1:1 移植)
//! - `mod.rs`(本文件)— GeminiNativeAdapter impl Adapter trait
//! - **下轮加** `response.rs` — SSE chunks → chat completions delta + Responses 包装
//!
//! 当前响应侧:`transform_response_stream` 暂用 trait 默认实现(passthrough,
//! 即把上游 Gemini SSE 字节直接回灌客户端)。Codex.app 拿到 Gemini SSE
//! 不认识 → 客户端会卡。但这一步至少让请求侧能 work 上游,本地能验证
//! 出站请求 wire 形态。下轮做完整 SSE 状态机 + Responses 包装就端到端 work。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::{header::HeaderValue, HeaderMap, StatusCode};
use serde_json::Value;

use crate::types::{Adapter, AdapterError, ByteStream, RequestPlan, ResponsePlan};

pub mod grounding;
pub mod request;
pub mod response;
pub mod types;

#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiNativeAdapter;

impl GeminiNativeAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Adapter for GeminiNativeAdapter {
    fn name(&self) -> &'static str {
        "gemini_native"
    }

    fn prepare_request(
        &self,
        _client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        // 1. 解析入站 body(Codex.app /responses 形态)
        let parsed: Value = serde_json::from_slice(&body)?;
        let stream = parsed
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let model = parsed
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AdapterError::BadRequest("model field required".into()))?
            .to_owned();

        // 2. Codex.app /responses → Gemini RequestBody(完整转换 1:1 LiteLLM
        // chat→Gemini + 我们项目的 Responses→chat 归一化)
        let gemini_request = request::responses_body_to_gemini_request(&parsed, provider)?;
        let gemini_body = serde_json::to_vec(&gemini_request).map_err(AdapterError::BodyDecode)?;

        // 3. 拼上游 URL path:Gemini 3+ 用 v1alpha,2.x 用 v1beta;若 base_url
        // 已带版本则不重复加。`/{version}/models/{model}:streamGenerateContent?alt=sse`
        let upstream_path = request::build_gemini_upstream_path(&model, stream, &provider.base_url);

        Ok(RequestPlan {
            upstream_path,
            body: Bytes::from(gemini_body),
            response_session: None,
            is_compact: false,
            // Codex.app /responses 入站时 original_responses_request 用于回灌
            // Responses envelope 字段 — 但响应侧 SSE 状态机下轮才做,留 None。
            original_responses_request: Some(parsed),
        })
    }

    /// 响应侧:Gemini SSE → Responses SSE **直转**(2026-05-10 用户决策)。
    ///
    /// 不走 chat 中间形态,Gemini adapter 自给自足 — `response.rs::GeminiToResponsesConverter`
    /// 直接 emit `response.created/in_progress/output_item.added/output_text.delta/
    /// function_call_arguments.delta/output_text.annotation.added/completed` 等事件,
    /// envelope 字段从 `request_plan.original_responses_request` 回灌(tools / instructions
    /// / temperature / etc.)。
    ///
    /// 错误路径:5xx / 4xx body 直接透传(Codex.app 看到错误体可重试 / 显示)。
    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        mut upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        if !upstream_status.is_success() {
            return Ok(ResponsePlan {
                status: upstream_status,
                headers: upstream_headers,
                stream: upstream_stream,
            });
        }
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let stream = response::convert_gemini_to_responses_stream(
            upstream_stream,
            request_plan.original_responses_request.clone(),
            request_plan.response_session.clone(),
        );
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream,
        })
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use indexmap::IndexMap;

    fn dummy_provider() -> Provider {
        Provider {
            id: "google-ai-studio".into(),
            name: "Google AI Studio".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            auth_scheme: "google_api_key".into(),
            api_format: "gemini_native".into(),
            api_key: "fake".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: true,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    #[test]
    fn name_is_stable_id() {
        assert_eq!(GeminiNativeAdapter.name(), "gemini_native");
    }

    #[test]
    fn prepare_request_outputs_gemini_wire_with_v1alpha_path() {
        let body = serde_json::json!({
            "model": "gemini-3.1-pro-preview",
            "stream": true,
            "instructions": "sys",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "tools": [{"type":"web_search"}]
        });
        let plan = GeminiNativeAdapter
            .prepare_request(
                "/v1/responses?stream=true",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider(),
            )
            .unwrap();
        assert_eq!(
            plan.upstream_path,
            "/v1alpha/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse"
        );
        // body 必须是 Gemini wire(`contents` / `systemInstruction` / `tools[].googleSearch`)
        let parsed: Value = serde_json::from_slice(&plan.body).unwrap();
        assert!(parsed.get("contents").is_some());
        assert!(parsed.get("systemInstruction").is_some());
        let tools = parsed["tools"].as_array().unwrap();
        assert!(
            tools.iter().any(|t| t.get("googleSearch").is_some()),
            "出站 body 必须含 googleSearch tool;实际:{tools:?}"
        );
        // original_responses_request 保留供下轮 SSE 状态机用
        assert!(plan.original_responses_request.is_some());
    }

    #[test]
    fn prepare_request_non_stream_uses_generate_content_endpoint() {
        let body = serde_json::json!({
            "model": "gemini-2.0-flash",
            "input": [{"type":"message","role":"user","content":"x"}]
        });
        let plan = GeminiNativeAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider(),
            )
            .unwrap();
        assert_eq!(
            plan.upstream_path,
            "/v1beta/models/gemini-2.0-flash:generateContent"
        );
    }

    #[test]
    fn missing_model_returns_bad_request() {
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}]
        });
        let err = GeminiNativeAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider(),
            )
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }
}
