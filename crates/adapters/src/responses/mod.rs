//! `apiFormat == "responses"` 适配器(Stage 3.2c · 仅文本流骨架).
//!
//! 范围:
//! - **请求侧**:Stage 3.2a 才做完整 Responses → Chat body 转换;本轮先把
//!   path 从 `/v1/responses` 重定到 `/chat/completions`,body 透传(意味着
//!   端到端真实场景 Codex CLI → 上游会失败,因为 body schema 对不上;
//!   但**单元 / 集成测试可以独立 driving 响应侧**)。
//! - **响应侧**:Chat SSE → Responses SSE 状态机(text-only)。tool / reasoning /
//!   function call 留 Stage 3.3。

pub mod compact;
pub mod converter;
pub(crate) mod events;
pub(crate) mod input;
pub mod request;
pub mod session;
pub mod stream;
pub mod tool_call_cache;

pub use converter::ChatToResponsesConverter;
pub use request::{
    responses_body_to_chat_body, responses_body_to_chat_body_for_provider,
    responses_body_to_chat_body_for_provider_with_session,
};
pub use session::{global_response_session_cache, ResponseSessionCache};
pub use stream::{
    convert_chat_to_responses_stream, convert_chat_to_responses_stream_with_options,
    convert_chat_to_responses_stream_with_session,
};
pub use tool_call_cache::{global_tool_call_cache, ToolCallCache, ToolCallEntry};

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
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        // 私有 `/responses/compact` 端点:OpenAI Responses API 的私有扩展,
        // 第三方 OpenAI-compatible provider 都不支持。我们在代理层本地实现:
        // 把 input 历史重组成普通 chat completions summarize 请求,响应阶段
        // 再包装成 Codex CLI 期待的 compact 响应。详见 `compact.rs`。
        if compact::is_compact_path(client_path) {
            let new_body = compact::build_compact_chat_request(&body, provider)?;
            return Ok(RequestPlan {
                upstream_path: "/chat/completions".to_owned(),
                body: Bytes::from(new_body),
                response_session: None,
                is_compact: true,
                original_responses_request: None,
            });
        }

        let upstream_path = redirect_responses_to_chat(client_path);
        // Stage 3.2a:解析 body → Responses,转出 Chat 形态。
        // 失败时(body 非 JSON / 非对象)用 BadRequest 错出去,proxy 会回 400。
        let parsed: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AdapterError::BadRequest(format!("body 不是合法 JSON: {e}")))?;
        // chat→responses 响应阶段 envelope 需要回灌入站 Responses API
        // request 的多个字段(tools / parallel_tool_calls / tool_choice /
        // reasoning / text / metadata / previous_response_id / instructions /
        // temperature / top_p / max_output_tokens / truncation 等)。tools
        // 字段尤其关键:Codex CLI 用 (namespace, function.name) 复合主键
        // 反向路由 MCP 工具的 function_call;缺其他字段会让严格 Responses
        // 协议客户端解析失败。借鉴 mimo2codex `streamToSse.ts:75-105`
        // `buildResponseSnapshot` 一次回灌全字段策略。
        let original_responses_request = Some(parsed.clone());
        let conversion = responses_body_to_chat_body_for_provider_with_session(
            &parsed,
            Some(provider),
            Some(global_response_session_cache()),
        )?;
        let new_body = serde_json::to_vec(&conversion.body)
            .map_err(|e| AdapterError::Internal(format!("re-serialize: {e}")))?;
        Ok(RequestPlan {
            upstream_path,
            body: Bytes::from(new_body),
            response_session: Some(conversion.response_session),
            is_compact: false,
            original_responses_request,
        })
    }

    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        mut upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        // /responses/compact:上游回的是非流式 chat completion JSON,
        // 收齐后包装成 `{"output":[{"type":"compaction",...}]}`。
        if request_plan.is_compact {
            return compact::build_compact_response_plan(
                upstream_status,
                upstream_headers,
                upstream_stream,
            );
        }
        // 把 content-type 强制改成 text/event-stream(上游本来就是,但保险)
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let enable_think_tag_split = provider_needs_think_tag_split(provider);
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream: convert_chat_to_responses_stream_with_options(
                upstream_stream,
                request_plan.response_session.clone(),
                enable_think_tag_split,
                request_plan.original_responses_request.clone(),
            ),
        })
    }
}

/// 哪些 provider 需要 `<think>...</think>` 兜底拆分。
/// 目前只有 MiniMax 的 OpenAI-compatible 端点在不开启 `reasoning_split` 时
/// 会把思考过程塞进 content 的 `<think>` 标签里,需要兜底解析。
fn provider_needs_think_tag_split(provider: &Provider) -> bool {
    let needles = [&provider.id, &provider.name, &provider.base_url];
    needles.iter().any(|value| {
        let lower = value.to_ascii_lowercase();
        lower.contains("minimax") || lower.contains("minimaxi")
    })
}

/// 把 `/v1/responses` / `/responses` / `/openai/v1/responses` 以及旧版 message
/// aliases 重定向到 `/chat/completions`(上游 OpenAI Chat 的标准入口)。其它路径透传不动。
fn redirect_responses_to_chat(path: &str) -> String {
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
    let normalized = normalize_local_responses_path(path_only);

    let target = if let Some(after) = normalized.strip_prefix("/responses") {
        format!("/chat/completions{after}")
    } else if let Some(after) = normalized.strip_prefix("/messages") {
        format!("/chat/completions{after}")
    } else {
        normalized
    };

    if query.is_empty() {
        target
    } else {
        format!("{target}?{query}")
    }
}

fn normalize_local_responses_path(path: &str) -> String {
    let path = path.strip_prefix("/openai").unwrap_or(path);
    if path == "/claude/v1/messages" {
        return "/messages".to_owned();
    }
    path.strip_prefix("/v1")
        .map(|s| if s.is_empty() { "/" } else { s }.to_owned())
        .unwrap_or_else(|| path.to_owned())
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
        assert_eq!(
            redirect_responses_to_chat("/v1/messages"),
            "/chat/completions"
        );
        assert_eq!(
            redirect_responses_to_chat("/claude/v1/messages"),
            "/chat/completions"
        );
        assert_eq!(
            redirect_responses_to_chat("/v1/messages?stream=1"),
            "/chat/completions?stream=1"
        );
    }

    #[test]
    fn passes_through_unrelated_paths() {
        assert_eq!(redirect_responses_to_chat("/v1/models"), "/models");
        assert_eq!(redirect_responses_to_chat("/health"), "/health");
    }

    /// 集成测试:prepare_request → RequestPlan.original_responses_request →
    /// (后续 transform_response_stream 会读这个字段塞进 envelope)。
    /// 验证 P3+P4 修复在真实 adapter trait 调用下整条链路通。
    #[test]
    fn prepare_request_preserves_original_inbound_body_for_envelope_replay() {
        use codex_app_transfer_registry::Provider;
        use indexmap::IndexMap;

        let mut p = Provider {
            id: "kimi".into(),
            name: "Kimi".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            auth_scheme: "bearer".into(),
            api_format: "responses".into(),
            api_key: "sk-test".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        };
        p.models.insert("default".into(), "kimi-for-coding".into());

        // 构造一个**含 namespace 包装 + 多个 envelope 字段**的入站 body
        // (模拟真实 Codex CLI 发的形态)
        let inbound = serde_json::json!({
            "model": "kimi-for-coding",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "list my notion pages"}
            ],
            "tools": [
                {"type": "function", "name": "shell"},
                {"type": "namespace", "name": "mcp__notion__", "tools": [
                    {"type": "function", "name": "notion_search"},
                    {"type": "function", "name": "notion_create_pages"}
                ]}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": null},
            "temperature": 0.7,
            "top_p": 0.9,
            "max_output_tokens": 4096,
            "metadata": {"trace": "abc"},
            "previous_response_id": "resp_prev",
            "instructions": "Be helpful.",
        });
        let body = bytes::Bytes::from(serde_json::to_vec(&inbound).unwrap());

        let adapter = ResponsesAdapter::new();
        let plan = adapter
            .prepare_request("/v1/responses", body, &p)
            .expect("prepare_request 成功");

        // P3 修复:RequestPlan 必须保留入站完整 body 供 envelope 回灌
        let saved = plan
            .original_responses_request
            .as_ref()
            .expect("original_responses_request 必须填");
        assert_eq!(saved["model"], "kimi-for-coding");
        assert_eq!(
            saved["tools"], inbound["tools"],
            "原 tools 数组(含 namespace 包装)必须原样保留"
        );
        assert_eq!(saved["tool_choice"], "auto");
        assert_eq!(saved["parallel_tool_calls"], true);
        assert_eq!(saved["temperature"], 0.7);
        assert_eq!(saved["previous_response_id"], "resp_prev");
        assert_eq!(saved["instructions"], "Be helpful.");
        assert_eq!(saved["reasoning"]["effort"], "high");

        // P2 修复:出站 body(plan.body)tools 已展平,namespace 包装已被
        // recursively flat_map 成顶级 function tools(`shell` + `notion_search`
        // + `notion_create_pages` 共 3 个 function)
        let outbound: serde_json::Value =
            serde_json::from_slice(&plan.body).expect("plan.body 是合法 JSON");
        let outbound_tools = outbound["tools"].as_array().expect("outbound tools 数组");
        let outbound_names: Vec<&str> = outbound_tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            outbound_names.len(),
            3,
            "namespace 已展平,共 3 个顶级 function"
        );
        assert!(outbound_names.contains(&"shell"));
        assert!(outbound_names.contains(&"notion_search"));
        assert!(outbound_names.contains(&"notion_create_pages"));
        // 验证已无任何 namespace type 残留(全部展平为 function)
        for t in outbound_tools {
            assert_eq!(t["type"], "function", "outbound 不应有 type:namespace 残留");
        }

        // 出站路径正确(/v1/responses → /chat/completions)
        assert_eq!(plan.upstream_path, "/chat/completions");
        assert!(!plan.is_compact);
    }
}
