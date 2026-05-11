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

use crate::responses::compact::{
    build_compact_chat_request, build_compact_response_plan, is_compact_path,
};
use crate::responses::global_response_session_cache;
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
        client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        // 对齐 chat<=>responses 的私有 /responses/compact 流程:
        // 入站 compact body 先转成 compact 专用 chat request(含总结 prompt),
        // 再走 gemini_native chat->Gemini wire 转换。上游用非流式 generateContent,
        // 响应侧再包装回 {"output":[{"type":"compaction",...}]}.
        if is_compact_path(client_path) {
            let compact_chat_body = build_compact_chat_request(&body, provider)?;
            let compact_chat_json: Value = serde_json::from_slice(&compact_chat_body)
                .map_err(|e| AdapterError::Internal(format!("compact chat body decode: {e}")))?;
            let model = compact_chat_json
                .get("model")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AdapterError::BadRequest("compact body missing model".into()))?
                .to_owned();
            let gemini_request =
                request::chat_normalized_to_gemini_request(&compact_chat_json, provider)?;
            let gemini_body =
                serde_json::to_vec(&gemini_request).map_err(AdapterError::BodyDecode)?;
            let upstream_path =
                request::build_gemini_upstream_path(&model, false, &provider.base_url);
            return Ok(RequestPlan {
                upstream_path,
                body: Bytes::from(gemini_body),
                response_session: None,
                is_compact: true,
                original_responses_request: None,
            });
        }

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
        let conversion = request::responses_body_to_gemini_request_with_session(
            &parsed,
            provider,
            Some(global_response_session_cache()),
        )?;
        let gemini_request = conversion.request;
        let gemini_body = serde_json::to_vec(&gemini_request).map_err(AdapterError::BodyDecode)?;

        // 3. 拼上游 URL path:Gemini 3+ 用 v1alpha,2.x 用 v1beta;若 base_url
        // 已带版本则不重复加。`/{version}/models/{model}:streamGenerateContent?alt=sse`
        let upstream_path = request::build_gemini_upstream_path(&model, stream, &provider.base_url);

        Ok(RequestPlan {
            upstream_path,
            body: Bytes::from(gemini_body),
            response_session: Some(conversion.response_session),
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
    /// 错误路径(2026-05-10 修):4xx/5xx **不再直接透传 raw Gemini JSON**。Codex.app
    /// 期待 OpenAI Responses SSE event 流,收到 raw JSON 不知道怎 parse → 卡 Thinking。
    /// 改成构造合规 Responses SSE 失败流(`response.created` + `response.failed`),
    /// 含 Gemini error 翻译过的 message + statusCode + raw upstream code,客户端
    /// 能正确识别 + 显示用户级错误而不是 silent hang。
    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        mut upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        if request_plan.is_compact {
            return build_compact_response_plan(upstream_status, upstream_headers, upstream_stream);
        }
        // 上游 4xx/5xx 也走 SSE — 两个分支都要重写 content-type;另外必须 strip
        // content-length(我们 emit 的 SSE bytes 数跟原 body 不一样)和 content-encoding
        // (上游可能返 gzip 的 JSON 错误体,如果保留 header 客户端会试图 gunzip plaintext SSE
        // → 整个流 corrupt,等于又埋一个 silent failure)
        upstream_headers.remove(http::header::CONTENT_LENGTH);
        upstream_headers.remove(http::header::CONTENT_ENCODING);
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        if !upstream_status.is_success() {
            // 构造 Responses SSE failure 流:200 response + SSE event 流(created+failed)
            let stream = response::convert_gemini_error_to_responses_failure_stream(
                upstream_status,
                upstream_stream,
                request_plan.original_responses_request.clone(),
            );
            return Ok(ResponsePlan {
                status: StatusCode::OK, // SSE 流 status 永远 200,错误信息在 SSE event 内
                headers: upstream_headers,
                stream,
            });
        }
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
    use futures_util::StreamExt;
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

    #[test]
    fn compact_path_routes_to_non_stream_generate_content_and_marks_is_compact() {
        let body = serde_json::json!({
            "model": "gemini-3.1-pro-high",
            "input": [
                {"type":"message","role":"user","content":"history x"},
            ],
            "reasoning": {"effort":"medium"}
        });
        let plan = GeminiNativeAdapter
            .prepare_request(
                "/v1/responses/compact",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider(),
            )
            .expect("compact prepare_request should succeed");
        assert!(
            plan.is_compact,
            "compact path must set RequestPlan.is_compact=true"
        );
        assert!(
            plan.upstream_path.ends_with(":generateContent"),
            "compact should call non-stream generateContent, got {}",
            plan.upstream_path
        );
        let parsed: Value = serde_json::from_slice(&plan.body).expect("gemini request json");
        assert!(
            parsed.get("contents").is_some(),
            "compact request must still convert to Gemini contents wire"
        );
    }

    #[tokio::test]
    async fn two_turn_responses_roundtrip_restores_history_via_previous_response_id() {
        let adapter = GeminiNativeAdapter;
        // turn-1: 正常问答
        let body1 = serde_json::json!({
            "model": "gemini-3.1-pro-high",
            "stream": true,
            "input": [{"type":"message","role":"user","content":"first question"}]
        });
        let plan1 = adapter
            .prepare_request(
                "/v1/responses?stream=true",
                Bytes::from(serde_json::to_vec(&body1).unwrap()),
                &dummy_provider(),
            )
            .expect("turn-1 prepare_request should succeed");
        let response_id = plan1
            .response_session
            .as_ref()
            .map(|s| s.response_id.clone())
            .expect("turn-1 must carry response_session for resume");

        // 上游 Gemini SSE(最小成功文本流),用于触发 transform_response_stream 里的
        // session save 逻辑
        let upstream_chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"first answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}

"#;
        let upstream_stream: ByteStream = Box::pin(futures_util::stream::iter(vec![Ok(
            Bytes::from_static(upstream_chunk),
        )]));
        let response_plan = adapter
            .transform_response_stream(
                StatusCode::OK,
                HeaderMap::new(),
                upstream_stream,
                &dummy_provider(),
                &plan1,
            )
            .expect("turn-1 transform_response_stream should succeed");
        // 消费完流,确保 finish/save 执行
        let mut stream = response_plan.stream;
        while let Some(chunk) = stream.next().await {
            let _ = chunk.expect("transformed chunk should be valid");
        }

        // turn-2:用 previous_response_id 续话,应当把 turn-1 的 user+assistant 历史拼回
        let body2 = serde_json::json!({
            "model": "gemini-3.1-pro-high",
            "stream": true,
            "previous_response_id": response_id,
            "input": [{"type":"message","role":"user","content":"follow up question"}]
        });
        let plan2 = adapter
            .prepare_request(
                "/v1/responses?stream=true",
                Bytes::from(serde_json::to_vec(&body2).unwrap()),
                &dummy_provider(),
            )
            .expect("turn-2 prepare_request should succeed");
        let req2: Value = serde_json::from_slice(&plan2.body).expect("gemini wire json");
        let contents = req2["contents"].as_array().expect("contents array");

        let mut all_text = String::new();
        for content in contents {
            if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        all_text.push_str(text);
                        all_text.push('\n');
                    }
                }
            }
        }
        assert!(
            all_text.contains("first question"),
            "turn-2 outgoing contents must contain restored prior user message"
        );
        assert!(
            all_text.contains("first answer"),
            "turn-2 outgoing contents must contain restored prior assistant message"
        );
        assert!(
            all_text.contains("follow up question"),
            "turn-2 outgoing contents must contain current user input"
        );
    }
}
