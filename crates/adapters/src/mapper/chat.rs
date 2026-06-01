use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use futures_util::stream::{self, Stream, StreamExt};
use http::{header::HeaderValue, HeaderMap, StatusCode};
use serde_json::json;
use std::pin::Pin;

use crate::core::routes;
use crate::mapper::{RequestMapper, ResponseMapper};
use crate::responses::{
    compact, convert_chat_to_responses_stream_with_options, global_response_session_cache,
    responses_body_to_chat_body_for_provider_with_session,
};
use crate::types::{AdapterError, ByteStream, RequestPlan, ResponsePlan};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ChatResponsesMapper;

/// 哪些 provider 需要 `<think>...</think>` 兜底拆分。
/// 目前只有 MiniMax 的 OpenAI-compatible 端点在不开启 `reasoning_split` 时
/// 会把思考过程塞进 content 的 `<think>` 标签里,需要兜底解析。
pub(crate) fn provider_needs_think_tag_split(provider: &Provider) -> bool {
    let needles = [&provider.id, &provider.name, &provider.base_url];
    needles.iter().any(|value| {
        let lower = value.to_ascii_lowercase();
        lower.contains("minimax") || lower.contains("minimaxi")
    })
}

/// responses adapter 请求侧编排：
/// - `/responses/compact` 走 compact 本地包装
/// - 其他 `/responses*` 走 responses->chat 主管道转换
pub(crate) fn prepare_responses_request(
    client_path: &str,
    body: Bytes,
    provider: &Provider,
) -> Result<RequestPlan, AdapterError> {
    if compact::is_compact_path(client_path) {
        let new_body = compact::build_compact_chat_request(&body, provider)?;
        return Ok(RequestPlan {
            upstream_path: "/chat/completions".to_owned(),
            body: Bytes::from(new_body),
            upstream_headers: http::HeaderMap::new(),
            response_session: None,
            adapter_metadata: None,
            is_compact: true,
            original_responses_request: None,
        });
    }

    let upstream_path = routes::redirect_responses_to_chat(client_path);
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| AdapterError::BadRequest(format!("body 不是合法 JSON: {e}")))?;
    let original_responses_request = Some(parsed.clone());
    let conversion = responses_body_to_chat_body_for_provider_with_session(
        &parsed,
        Some(provider),
        Some(global_response_session_cache()),
    )?;
    let new_body = serde_json::to_vec(&conversion.body)
        .map_err(|e| AdapterError::Internal(format!("re-serialize: {e}")))?;
    // fix(#210 P1-1): 传递 history_lost 标志到 adapter_metadata,
    // transform_response_stream 据此注入 X-Session-History-Lost header
    let adapter_metadata = if conversion.history_lost {
        Some(serde_json::json!({"history_lost": true}))
    } else {
        None
    };
    Ok(RequestPlan {
        upstream_path,
        body: Bytes::from(new_body),
        upstream_headers: http::HeaderMap::new(),
        response_session: Some(conversion.response_session),
        adapter_metadata,
        is_compact: false,
        original_responses_request,
    })
}

/// responses adapter 响应侧编排：
/// - compact 走 compact response 包装
/// - 其余路径走 chat SSE -> responses SSE 转换
pub(crate) fn transform_responses_response_stream(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
    provider: &Provider,
    request_plan: &RequestPlan,
) -> Result<ResponsePlan, AdapterError> {
    if request_plan.is_compact {
        return compact::build_compact_response_plan(
            upstream_status,
            upstream_headers,
            upstream_stream,
        );
    }
    // MOC-103:上游非 2xx 不能原样透传给 Codex.app —— 实测它收到 HTTP 4xx/5xx +
    // JSON error body 后期待 SSE 流而**卡 Thinking**(forward.rs:389 对 web_search
    // 400 的实测注释;MOC-79 gemini / MOC-90 grok 同款失败模式)。改写成合规
    // Responses 失败流:HTTP 200 + `response.created` + `response.failed`,error.code
    // 经 `crate::codex_retry_code` 映射(永久错误 surface+停,瞬时态保留 Retryable)。
    // 对齐 grok_web 的 `transform_grok_web_response_stream`。换成干净的 SSE header
    // 而非复用 upstream_headers:上游错误响应的 `content-encoding`(如 gzip)指向已
    // 被丢弃的原 body,若透传会让客户端按 gzip 解码我们重写的 SSE 明文而出错
    // (`content-length` 虽会被 forward.rs 的 hop-header 过滤剥掉,一并避开最稳妥)。
    if !upstream_status.is_success() {
        let response_id = request_plan
            .response_session
            .as_ref()
            .map(|s| s.response_id.clone())
            .unwrap_or_else(|| "resp_chat_error".to_owned());
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        return Ok(ResponsePlan {
            status: StatusCode::OK,
            headers,
            stream: convert_chat_error_to_responses_failure_stream(
                upstream_status,
                upstream_stream,
                response_id,
            ),
        });
    }
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    // fix(#210 P1-1): cache miss 降级时注入信号 header,让客户端感知历史丢失
    if request_plan
        .adapter_metadata
        .as_ref()
        .and_then(|m| m.get("history_lost"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        upstream_headers.insert(
            http::HeaderName::from_static("x-session-history-lost"),
            HeaderValue::from_static("1"),
        );
    }
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

/// Cap 上游错误 body 防 DoS(对齐 grok_web / gemini_native 的同名常量)。
const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 65_536;

/// chat 路径上游 HTTP status → 内部语义 kind(再经 [`crate::codex_retry_code`]
/// 映射成 Codex 客户端认识的 retry-control code)。
///
/// 与 grok 的 `classify_grok_error_status` 基本一致,差异:chat 把 **400 归
/// `bad_request`**(→ `invalid_prompt`,surface + 停)—— chat 上游 400 多是请求
/// 格式 / 参数错误,retry 同一请求必复现;唯一的瞬时态 400(web_search 套餐未开)
/// 已被 `forward.rs` 的 transparent retry 在更上游拦截,不会走到这里。
/// 404 / 405 等仍归 `upstream_error` → Retryable(遵循"Retryable 比误杀安全",
/// 对齐 grok)。
fn classify_chat_error_status(status_u16: u16) -> &'static str {
    match status_u16 {
        400 => "bad_request",
        401 => "auth_error",
        403 => "permission_denied",
        408 | 504 => "timeout",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "upstream_error",
    }
}

/// chat 路径上游非 2xx → 合规 Responses 失败流(MOC-103,对齐 grok_web 的
/// `convert_grok_error_to_responses_failure_stream`)。
///
/// 输出永远是 `response.created` + `response.failed` 两个 SSE 事件(HTTP status
/// 由调用方写成 200)。语义分类 [`classify_chat_error_status`] 经
/// [`crate::codex_retry_code`] 映射:永久错误(400/401/403)→ `invalid_prompt`
/// (surface + 停),瞬时态(timeout/rate_limited/server_error/404 等)保留原 code
/// → Codex Retryable。原始分类存 `error.upstream_error_kind` 诊断字段。
///
/// **防御**(对齐 grok):
/// - body cap [`MAX_UPSTREAM_ERROR_BODY_BYTES`] 字节防 DoS
/// - 非 UTF-8 用 `from_utf8_lossy`,后缀标 `(non-UTF-8 body)`
/// - mid-read transport `Err` → `upstream_transport_error` code(保留 Retryable)
/// - 空 body / 截断仍 emit `response.failed`,带通用 message
fn convert_chat_error_to_responses_failure_stream(
    upstream_status: StatusCode,
    upstream_stream: ByteStream,
    response_id: String,
) -> ByteStream {
    let status_u16 = upstream_status.as_u16();
    let kind = classify_chat_error_status(status_u16);

    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(
        stream::unfold((upstream_stream, false), move |(mut input, finished)| {
            let response_id = response_id.clone();
            let kind = kind.to_owned();
            async move {
                if finished {
                    return None;
                }
                let mut body = Vec::with_capacity(1024);
                let mut transport_err: Option<String> = None;
                let mut truncated = false;
                while let Some(chunk) = input.next().await {
                    match chunk {
                        Ok(b) => {
                            let remaining =
                                MAX_UPSTREAM_ERROR_BODY_BYTES.saturating_sub(body.len());
                            if remaining == 0 {
                                truncated = true;
                                break;
                            }
                            let take = b.len().min(remaining);
                            body.extend_from_slice(&b[..take]);
                            if take < b.len() {
                                truncated = true;
                                break;
                            }
                        }
                        Err(e) => {
                            transport_err = Some(e.to_string());
                            break;
                        }
                    }
                }
                let was_lossy = std::str::from_utf8(&body).is_err();
                let mut body_text = String::from_utf8_lossy(&body).into_owned();
                if truncated {
                    body_text.push_str(" …(truncated)");
                }
                if was_lossy {
                    body_text.push_str(" (non-UTF-8 body)");
                }
                let (final_kind, message) = if let Some(transport) = transport_err {
                    (
                        "upstream_transport_error".to_owned(),
                        format!(
                            "upstream HTTP {status_u16} but transport err during body read: {transport}"
                        ),
                    )
                } else if body_text.is_empty() {
                    (kind, format!("upstream HTTP {status_u16} (empty body)"))
                } else {
                    (kind, format!("upstream HTTP {status_u16}: {body_text}"))
                };

                // 两个事件拼一起 yield(避免 mock stream 单 chunk 截断 SSE 帧)。
                // 短路错误路径无 ConvState,起 local seq 计数器(从 0)。
                let mut seq: u64 = 0;
                let mut buf = Vec::with_capacity(512);
                buf.extend_from_slice(&emit_chat_response_created(&mut seq, &response_id));
                buf.extend_from_slice(&emit_chat_response_failed(
                    &mut seq,
                    &response_id,
                    &final_kind,
                    &message,
                ));
                Some((Ok(Bytes::from(buf)), (input, true)))
            }
        }),
    );
    s
}

/// 构造一个带 `sequence_number` 的 Responses SSE 事件帧(`event:` + `data:`)。
fn emit_chat_sse_event(seq: &mut u64, event: &str, mut data: serde_json::Value) -> Bytes {
    if let Some(obj) = data.as_object_mut() {
        obj.insert(
            "sequence_number".into(),
            serde_json::Value::Number((*seq).into()),
        );
    }
    *seq += 1;
    let mut out = String::with_capacity(128);
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
    Bytes::from(out)
}

fn emit_chat_response_created(seq: &mut u64, response_id: &str) -> Bytes {
    emit_chat_sse_event(
        seq,
        "response.created",
        json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
            }
        }),
    )
}

/// `upstream_kind` 是内部语义分类,经 [`crate::codex_retry_code`] 映射成 Codex
/// 客户端认识的 retry-control `error.code`(永久 → `invalid_prompt`,瞬时态保留
/// 原值)。原始分类保留在 `error.upstream_error_kind` 诊断字段(Codex `Error`
/// struct 无 `deny_unknown_fields`,该字段被安全忽略)。
fn emit_chat_response_failed(
    seq: &mut u64,
    response_id: &str,
    upstream_kind: &str,
    message: &str,
) -> Bytes {
    let codex_code = crate::codex_retry_code(upstream_kind);
    emit_chat_sse_event(
        seq,
        "response.failed",
        json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "failed",
                "error": {
                    "code": codex_code,
                    "message": message,
                    "upstream_error_kind": upstream_kind,
                }
            }
        }),
    )
}

impl RequestMapper for ChatResponsesMapper {
    fn map_request(
        &self,
        client_path: &str,
        body: Bytes,
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        prepare_responses_request(client_path, body, provider)
    }
}

impl ResponseMapper for ChatResponsesMapper {
    fn map_response(
        &self,
        upstream_status: StatusCode,
        upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        transform_responses_response_stream(
            upstream_status,
            upstream_headers,
            upstream_stream,
            provider,
            request_plan,
        )
    }
}

#[cfg(test)]
mod upstream_error_tests {
    use super::*;
    use indexmap::IndexMap;

    /// drive 失败流到底,拼成字符串(convert 输出的 chunk 永远是 Ok 的 SSE bytes)。
    async fn collect(stream: ByteStream) -> String {
        let mut s = stream;
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.expect("failure stream chunk must be Ok"));
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn body_stream(body: Vec<u8>) -> ByteStream {
        Box::pin(stream::once(async move { Ok(Bytes::from(body)) }))
    }

    fn transport_err_stream() -> ByteStream {
        Box::pin(stream::once(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "connection reset",
            ))
        }))
    }

    fn convert(status: StatusCode, body: &str) -> ByteStream {
        convert_chat_error_to_responses_failure_stream(
            status,
            body_stream(body.as_bytes().to_vec()),
            "resp_test".to_owned(),
        )
    }

    #[test]
    fn classify_maps_status_to_kind() {
        assert_eq!(classify_chat_error_status(400), "bad_request");
        assert_eq!(classify_chat_error_status(401), "auth_error");
        assert_eq!(classify_chat_error_status(403), "permission_denied");
        assert_eq!(classify_chat_error_status(408), "timeout");
        assert_eq!(classify_chat_error_status(504), "timeout");
        assert_eq!(classify_chat_error_status(429), "rate_limited");
        assert_eq!(classify_chat_error_status(500), "server_error");
        assert_eq!(classify_chat_error_status(503), "server_error");
        // 404 / 405 → upstream_error → Retryable(对齐 grok,"Retryable 比误杀安全")
        assert_eq!(classify_chat_error_status(404), "upstream_error");
        assert_eq!(classify_chat_error_status(405), "upstream_error");
    }

    // ── 永久错误 → invalid_prompt(Codex surface + 停,不重试)──
    #[tokio::test]
    async fn upstream_401_maps_to_invalid_prompt() {
        let out = collect(convert(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"bad key"}}"#,
        ))
        .await;
        assert!(out.contains("event: response.created"));
        assert!(out.contains("event: response.failed"));
        assert!(out.contains(r#""code":"invalid_prompt""#));
        assert!(!out.contains(r#""code":"auth_error""#));
        assert!(out.contains(r#""upstream_error_kind":"auth_error""#));
        assert!(out.contains("bad key"));
    }

    #[tokio::test]
    async fn upstream_403_maps_to_invalid_prompt() {
        let out = collect(convert(StatusCode::FORBIDDEN, "forbidden")).await;
        assert!(out.contains(r#""code":"invalid_prompt""#));
        assert!(out.contains(r#""upstream_error_kind":"permission_denied""#));
    }

    #[tokio::test]
    async fn upstream_400_maps_to_invalid_prompt() {
        let out = collect(convert(StatusCode::BAD_REQUEST, "bad request")).await;
        assert!(out.contains(r#""code":"invalid_prompt""#));
        assert!(out.contains(r#""upstream_error_kind":"bad_request""#));
    }

    // ── 瞬时态 → 保留原 code(Codex Retryable)──
    #[tokio::test]
    async fn upstream_429_stays_rate_limited() {
        let out = collect(convert(StatusCode::TOO_MANY_REQUESTS, "slow down")).await;
        assert!(out.contains(r#""code":"rate_limited""#));
        assert!(!out.contains("invalid_prompt"));
        assert!(out.contains(r#""upstream_error_kind":"rate_limited""#));
    }

    #[tokio::test]
    async fn upstream_5xx_stays_server_error() {
        let out = collect(convert(StatusCode::INTERNAL_SERVER_ERROR, "boom")).await;
        assert!(out.contains(r#""code":"server_error""#));
        assert!(out.contains(r#""upstream_error_kind":"server_error""#));
    }

    #[tokio::test]
    async fn upstream_404_stays_retryable_not_invalid_prompt() {
        // 对齐 grok:404 归 upstream_error → 保留 → Codex Retryable
        let out = collect(convert(StatusCode::NOT_FOUND, "no such model")).await;
        assert!(out.contains(r#""code":"upstream_error""#));
        assert!(!out.contains("invalid_prompt"));
        assert!(out.contains(r#""upstream_error_kind":"upstream_error""#));
    }

    // ── 防御(对齐 grok)──
    #[tokio::test]
    async fn transport_err_maps_to_upstream_transport_error() {
        let out = collect(convert_chat_error_to_responses_failure_stream(
            StatusCode::BAD_GATEWAY,
            transport_err_stream(),
            "r".into(),
        ))
        .await;
        assert!(out.contains(r#""code":"upstream_transport_error""#));
        assert!(out.contains("connection reset"));
    }

    #[tokio::test]
    async fn empty_body_still_emits_failed() {
        let out = collect(convert(StatusCode::UNAUTHORIZED, "")).await;
        assert!(out.contains("event: response.failed"));
        assert!(out.contains("(empty body)"));
        assert!(out.contains(r#""code":"invalid_prompt""#));
    }

    #[tokio::test]
    async fn oversized_body_is_truncated() {
        let big = "x".repeat(MAX_UPSTREAM_ERROR_BODY_BYTES + 4096);
        let out = collect(convert(StatusCode::INTERNAL_SERVER_ERROR, &big)).await;
        assert!(out.contains("(truncated)"));
        assert!(out.contains(r#""code":"server_error""#));
    }

    #[tokio::test]
    async fn non_utf8_body_marked_lossy() {
        let out = collect(convert_chat_error_to_responses_failure_stream(
            StatusCode::INTERNAL_SERVER_ERROR,
            body_stream(vec![0xff, 0xfe, 0x00]),
            "r".into(),
        ))
        .await;
        assert!(out.contains("(non-UTF-8 body)"));
    }

    // ── 端到端 transform_responses_response_stream ──
    fn provider() -> Provider {
        Provider {
            id: "kimi".into(),
            name: "Kimi".into(),
            base_url: "https://api.kimi.com/v1".into(),
            auth_scheme: "bearer".into(),
            api_format: "openai_chat".into(),
            api_key: "sk".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    fn request_plan() -> RequestPlan {
        RequestPlan {
            upstream_path: "/chat/completions".to_owned(),
            body: Bytes::from_static(b"{}"),
            upstream_headers: HeaderMap::new(),
            response_session: None,
            adapter_metadata: None,
            is_compact: false,
            original_responses_request: Some(json!({"model": "kimi-for-coding"})),
        }
    }

    #[tokio::test]
    async fn transform_non_2xx_returns_200_sse_failure_stream() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "42".parse().unwrap());
        headers.insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let plan = request_plan();
        let resp = transform_responses_response_stream(
            StatusCode::UNAUTHORIZED,
            headers,
            body_stream(br#"{"error":{"message":"nope"}}"#.to_vec()),
            &provider(),
            &plan,
        )
        .unwrap();
        // 关键:HTTP status 改写成 200(否则 Codex 卡 Thinking)
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(
            resp.headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        // 旧 content-length / content-encoding 必须丢弃(指向已被替换的上游 body)
        assert!(resp.headers.get(http::header::CONTENT_LENGTH).is_none());
        assert!(resp.headers.get(http::header::CONTENT_ENCODING).is_none());
        let out = collect(resp.stream).await;
        assert!(out.contains("event: response.failed"));
        assert!(out.contains(r#""code":"invalid_prompt""#));
        // response_session=None → fallback id
        assert!(out.contains("resp_chat_error"));
    }

    #[tokio::test]
    async fn transform_success_status_passed_through() {
        // 2xx 不进失败流分支:status 透传(成功流由 converter 接管)
        let plan = request_plan();
        let resp = transform_responses_response_stream(
            StatusCode::OK,
            HeaderMap::new(),
            body_stream(b"data: [DONE]\n\n".to_vec()),
            &provider(),
            &plan,
        )
        .unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(
            resp.headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
    }
}
