//! Grok Web `RequestMapper` + `ResponseMapper` 实现。
//!
//! 按 [`docs/protocol-unification-rfc-phase4.md`] 的 Phase 4 规范,本文件是 grok_web
//! adapter 的**核心逻辑落地点**(adapter 自身仅 thin wrapper)。
//!
//! - [`prepare_grok_web_request`]:Codex Responses body → grok payload + 鉴权头
//! - [`transform_grok_web_response_stream`]:grok SSE → Codex Responses SSE

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;

use crate::grok_web::{
    auth::generate_uuid_v4,
    request::{responses_body_to_grok_request, serialize_grok_request, GROK_CHAT_PATH},
    response::{convert_grok_error_to_responses_failure_stream, convert_grok_sse_to_responses_sse},
};
use crate::mapper::{RequestMapper, ResponseMapper};
use crate::types::{AdapterError, ByteStream, RequestPlan, ResponsePlan};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct GrokWebMapper;

impl RequestMapper for GrokWebMapper {
    fn map_request(
        &self,
        _client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        prepare_grok_web_request(body, provider)
    }
}

impl ResponseMapper for GrokWebMapper {
    fn map_response(
        &self,
        upstream_status: StatusCode,
        upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        transform_grok_web_response_stream(
            upstream_status,
            upstream_headers,
            upstream_stream,
            provider,
            request_plan,
        )
    }
}

/// grok_web 请求侧:Codex Responses body → grok chat payload + headers。
///
/// R3 PoC 暂不支持 `/responses/compact`(grok.com 后端不暴露独立 compact endpoint);
/// 如果 client_path 是 compact,fallback 当普通 chat 请求处理 + warn log。
pub(crate) fn prepare_grok_web_request(
    body: Bytes,
    provider: &Provider,
) -> Result<RequestPlan, AdapterError> {
    let parsed: Value = serde_json::from_slice(&body)?;
    let grok_req = responses_body_to_grok_request(&parsed, provider)?;
    let grok_body = serialize_grok_request(&grok_req)?;

    // R3 PoC:暂不维护 session,后续 R1 用 ParentResponseTracker.record 在响应侧
    // 流末把(client 看到的 response_id, grok 返回的 modelResponse.responseId)记入。

    Ok(RequestPlan {
        upstream_path: GROK_CHAT_PATH.to_owned(),
        body: grok_body,
        response_session: None,
        is_compact: false,
        original_responses_request: Some(parsed),
    })
}

/// grok_web 响应侧:grok newline-delimited JSON SSE → Codex Responses SSE。
///
/// **错误处理**(review-feedback A1):上游 4xx/5xx 时,**不直接透传 raw grok JSON
/// 但伪装 SSE Content-Type**(那会让 Codex APP 卡 "Thinking" — gemini_native
/// 已踩过同一个坑,见 `gemini_native/response.rs:1474`)。改成合规 Responses 失败流
/// `response.created` + `response.failed`,classify status code 给结构化 error.code,
/// 内附 grok body 摘录(cap 防 DoS)。
///
/// **返回 status 永远 200**(因为 body 是合规 SSE event stream,客户端按 SSE 解析),
/// 真正的 error 信息走 `response.failed` event.error.{code,message} 字段。
pub(crate) fn transform_grok_web_response_stream(
    upstream_status: StatusCode,
    _upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
    _provider: &Provider,
    _request_plan: &RequestPlan,
) -> Result<ResponsePlan, AdapterError> {
    let response_id = format!("resp_grok_{}", generate_uuid_v4());

    if !upstream_status.is_success() {
        // 翻译成合规 Responses failure SSE 流(review-feedback A1):
        // grok body 由 `convert_grok_error_to_responses_failure_stream` cap+UTF-8 lossy 处理,
        // 输出永远是 `response.created` + `response.failed` 两个事件。
        let downstream = convert_grok_error_to_responses_failure_stream(
            upstream_status,
            upstream_stream,
            response_id,
        );
        return Ok(ResponsePlan {
            status: StatusCode::OK,
            headers: build_sse_headers(),
            stream: downstream,
        });
    }

    let downstream = convert_grok_sse_to_responses_sse(upstream_stream, response_id);
    Ok(ResponsePlan {
        status: StatusCode::OK,
        headers: build_sse_headers(),
        stream: downstream,
    })
}

fn build_sse_headers() -> HeaderMap {
    let mut h = HeaderMap::with_capacity(2);
    h.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    h
}

// 注:鉴权头注入(cookie / statsig / xai-request-id)**不**经过 mapper 层 wrapper,
// `forward.rs` 直接调用 `crate::grok_web::auth::apply_grok_headers`。
// 这是 grok_web 与其他 adapter(走 inject_auth 的 Bearer/x-api-key)的差异点,
// 因为 grok.com 需要一组复合 headers(7~10 个),用单一 fn 接口最清晰。

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_transfer_registry::Provider;
    use indexmap::IndexMap;
    use serde_json::json;

    fn make_provider() -> Provider {
        let mut models = IndexMap::new();
        models.insert("default".into(), "grok-420-computer-use-sa".into());
        let mut extra = IndexMap::new();
        extra.insert(
            "grokWeb".into(),
            json!({
                "cookies": {
                    "sso": "j1",
                    "sso-rw": "j2",
                    "cf_clearance": "c"
                },
                "statsigId": "stat-id"
            }),
        );
        Provider {
            id: "grok-web".into(),
            name: "Grok Web".into(),
            base_url: "https://grok.com".into(),
            auth_scheme: "grok_cookie".into(),
            api_format: "grok_web".into(),
            api_key: String::new(),
            models,
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra,
        }
    }

    #[test]
    fn prepare_request_emits_grok_chat_path() {
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "default",
                "input": [{"type": "message", "role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let plan = prepare_grok_web_request(body, &make_provider()).unwrap();
        assert_eq!(plan.upstream_path, GROK_CHAT_PATH);
        assert!(plan.original_responses_request.is_some());
        // payload 必须含 disabledConnectorIds 黑名单,无 connectorIds 白名单
        let payload: Value = serde_json::from_slice(&plan.body).unwrap();
        assert_eq!(payload["disabledConnectorIds"], json!([]));
        assert!(!payload.as_object().unwrap().contains_key("connectorIds"));
    }
}
