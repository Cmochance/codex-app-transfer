//! Gemini CLI OAuth adapter(`apiFormat=gemini_cli_oauth`)。
//!
//! Codex.app `/responses` → Google Cloud Code Assist `:streamGenerateContent`
//! 直转,**impersonate 官方 gemini-cli**。跟 [`crate::gemini_native`] 的关键
//! 差异:
//!
//! | 维度 | gemini_native(API key) | gemini_cli(OAuth) |
//! |---|---|---|
//! | 上游 | `generativelanguage.googleapis.com/v1{alpha,beta}/models/<m>:streamGenerateContent` | `cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse` |
//! | 鉴权 | `?key=<api_key>` query | `Authorization: Bearer <oauth_access_token>` |
//! | body | inner Gemini wire 直发 | outer `{model, project, user_prompt_id, request: <inner>}` 包一层 |
//! | SSE event | `{candidates, ...}` | `{response: {candidates, ...}}` 多包一层 |
//! | 配额 | API key 关联 GCP project 计费 | free-tier per-account,绑 `cloudaicompanionProject` |
//!
//! ## 复用 gemini_native 内部转换
//!
//! 90% inner 转换逻辑(JSON Schema sanitize / web_search 软约束 / 多轮 function
//! calling round-trip / contents 必须 user 起 / failure stream 等)从
//! [`crate::gemini_native::request::responses_body_to_gemini_request`] 直接 reuse,
//! 这里只做 outer wrap + SSE 外层 unwrap。
//!
//! ## project_id 来源
//!
//! 必须从 `provider.extra.cloud_code_project_id` 字段读 — 由前端 OAuth 流程
//! 完成后写入 provider config。**不在 adapter 里 fetch / refresh** —— OAuth
//! 流程在 `gemini_oauth` crate(用户 UI 触发),token 注入在 forward.rs。
//!
//! ## 致谢上游
//!
//! 借鉴 [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI)
//! 的 `internal/runtime/executor/gemini_cli_executor.go` 拿 wire 形态。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::{header::HeaderValue, HeaderMap, StatusCode};
use serde_json::Value;

use crate::types::{Adapter, AdapterError, ByteStream, RequestPlan, ResponsePlan};

pub mod request;
pub mod response;

#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiCliAdapter;

impl GeminiCliAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Adapter for GeminiCliAdapter {
    fn name(&self) -> &'static str {
        "gemini_cli_oauth"
    }

    fn prepare_request(
        &self,
        _client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
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
        let project_id = provider
            .extra
            .get("cloud_code_project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AdapterError::BadRequest(
                    "provider.extra.cloud_code_project_id required for OAuth path \
                     (run OAuth login first to bootstrap project)"
                        .into(),
                )
            })?
            .to_owned();

        // 1. 复用 gemini_native 把 Codex /responses 转 Gemini inner body
        let inner =
            crate::gemini_native::request::responses_body_to_gemini_request(&parsed, provider)?;
        let inner_value = serde_json::to_value(&inner).map_err(AdapterError::BodyDecode)?;

        // 2. outer envelope: {model, project, user_prompt_id, request: <inner>}
        // RNG 失败(极罕见,iOS-style sandbox 可能触发)→ BadRequest 让 client 看到失败,
        // 不进 silent zero UUID 路径(2026-05-11 silent-failure 修)
        let outer =
            request::wrap_cloud_code_envelope(&model, &project_id, inner_value).map_err(|e| {
                AdapterError::BadRequest(format!("OS RNG unavailable for user_prompt_id: {e}"))
            })?;
        let outer_body = serde_json::to_vec(&outer).map_err(AdapterError::BodyDecode)?;

        // 3. cloud-code 上游 path: 不像 gemini_native 是 /v1{alpha,beta}/models/<m>:method
        //    这里固定 /v1internal:streamGenerateContent?alt=sse 或 :generateContent
        let upstream_path = if stream {
            "/v1internal:streamGenerateContent?alt=sse".to_owned()
        } else {
            "/v1internal:generateContent".to_owned()
        };

        Ok(RequestPlan {
            upstream_path,
            body: Bytes::from(outer_body),
            response_session: None,
            is_compact: false,
            original_responses_request: Some(parsed),
        })
    }

    fn transform_response_stream(
        &self,
        upstream_status: StatusCode,
        mut upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        upstream_headers.remove(http::header::CONTENT_LENGTH);
        upstream_headers.remove(http::header::CONTENT_ENCODING);
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        if !upstream_status.is_success() {
            // 错误路径直接复用 gemini_native 的 failure stream 转换 — Cloud Code
            // 错误 shape 跟 generativelanguage 上游基本一致(google.json envelope),
            // 错误分类 + 用户级 message 输出可共用
            let stream =
                crate::gemini_native::response::convert_gemini_error_to_responses_failure_stream(
                    upstream_status,
                    upstream_stream,
                    request_plan.original_responses_request.clone(),
                );
            return Ok(ResponsePlan {
                status: StatusCode::OK,
                headers: upstream_headers,
                stream,
            });
        }
        // 成功路径:每 SSE event 外包 {response:{...}},先 unwrap outer 再喂给
        // gemini_native 的 SSE→Responses 状态机
        let unwrapped = response::unwrap_cloud_code_sse_envelope(upstream_stream);
        let stream = crate::gemini_native::response::convert_gemini_to_responses_stream(
            unwrapped,
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

    fn dummy_provider_with_project() -> Provider {
        let mut extra = IndexMap::new();
        extra.insert(
            "cloud_code_project_id".into(),
            Value::String("test-project-12345".into()),
        );
        Provider {
            id: "gemini-cli".into(),
            name: "Gemini CLI (OAuth)".into(),
            base_url: "https://cloudcode-pa.googleapis.com".into(),
            auth_scheme: "google_oauth_cloud_code".into(),
            api_format: "gemini_cli_oauth".into(),
            api_key: "".into(), // OAuth 路径不用 api_key
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: true,
            sort_index: 0,
            extra,
        }
    }

    #[test]
    fn name_is_stable_id() {
        assert_eq!(GeminiCliAdapter.name(), "gemini_cli_oauth");
    }

    #[test]
    fn prepare_request_outputs_outer_envelope_with_project() {
        let body = serde_json::json!({
            "model": "gemini-2.5-pro",
            "stream": true,
            "instructions": "sys",
            "input": [{"type":"message","role":"user","content":"hi"}]
        });
        let plan = GeminiCliAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider_with_project(),
            )
            .unwrap();
        // upstream path: cloud-code internal
        assert_eq!(
            plan.upstream_path,
            "/v1internal:streamGenerateContent?alt=sse"
        );
        // body 必须有 outer envelope
        let parsed: Value = serde_json::from_slice(&plan.body).unwrap();
        assert_eq!(parsed["model"], "gemini-2.5-pro");
        assert_eq!(parsed["project"], "test-project-12345");
        assert!(parsed["user_prompt_id"].is_string());
        // request 内层应该是 Gemini wire(contents / systemInstruction)
        assert!(parsed["request"]["contents"].is_array());
        assert!(parsed["request"]["systemInstruction"].is_object());
    }

    #[test]
    fn prepare_request_non_stream_uses_generate_content() {
        let body = serde_json::json!({
            "model": "gemini-2.5-flash",
            "input": [{"type":"message","role":"user","content":"x"}]
        });
        let plan = GeminiCliAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider_with_project(),
            )
            .unwrap();
        assert_eq!(plan.upstream_path, "/v1internal:generateContent");
    }

    #[test]
    fn missing_project_id_returns_bad_request_with_hint() {
        let mut p = dummy_provider_with_project();
        p.extra.shift_remove("cloud_code_project_id");
        let body = serde_json::json!({
            "model": "gemini-2.5-pro",
            "input": [{"type":"message","role":"user","content":"x"}]
        });
        let err = GeminiCliAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &p,
            )
            .unwrap_err();
        match err {
            AdapterError::BadRequest(msg) => {
                assert!(
                    msg.contains("cloud_code_project_id"),
                    "错误必须 hint 用户跑 OAuth login,实际:{msg}"
                );
                assert!(msg.contains("OAuth login"));
            }
            other => panic!("期待 BadRequest,得到 {other:?}"),
        }
    }

    #[test]
    fn missing_model_returns_bad_request() {
        let body = serde_json::json!({
            "input": [{"type":"message","role":"user","content":"x"}]
        });
        let err = GeminiCliAdapter
            .prepare_request(
                "/v1/responses",
                Bytes::from(serde_json::to_vec(&body).unwrap()),
                &dummy_provider_with_project(),
            )
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }
}
