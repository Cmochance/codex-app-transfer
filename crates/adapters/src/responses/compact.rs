//! 本地实现 OpenAI Responses 私有 `/responses/compact` 端点。
//!
//! Codex CLI 在累计 token 超过 `model_auto_compact_token_limit` 时会调
//! `POST /responses/compact`,期望后端做"上下文压缩"——把整段对话历史摘要成
//! 一段简短的纯文本 summary,用 `{"output":[{"type":"compaction",
//! "encrypted_content":"<SUMMARY_PREFIX>\n<text>"}]}` 形态回写。
//!
//! 这是 OpenAI 官方 Responses API 的私有扩展,**所有第三方 OpenAI-compatible
//! provider(MiMo / Kimi / DeepSeek / MiniMax / 智谱 / 百炼)都不支持**——
//! 透传必 404,litellm 也只对 openai provider 实现透传。
//!
//! 本模块在我们代理层本地实现:把 `CompactionInput` 重组成普通
//! `/chat/completions` 请求(注入抄自 codex 自家的 SUMMARIZATION_PROMPT 作
//! 为 system message),拿到上游 chat completion 响应后,提取
//! `choices[0].message.content` 作为 summary,包装成 Codex CLI 期待的
//! compact 响应。
//!
//! ## 协议来源
//!
//! 通过 `openai/codex` 公开源码反查(Apache-2 license,标注引用):
//! - 请求结构 `CompactionInput`:`codex-rs/codex-api/src/common.rs`
//! - 响应结构 `CompactHistoryResponse { output: Vec<ResponseItem> }` +
//!   `ResponseItem::Compaction { encrypted_content: String }`:
//!   `codex-rs/codex-api/src/endpoint/compact.rs` + `codex-rs/protocol/src/models.rs:882`
//! - SUMMARY_PREFIX / SUMMARIZATION_PROMPT 文本:
//!   `codex-rs/core/templates/compact/summary_prefix.md`、
//!   `codex-rs/core/templates/compact/prompt.md`
//! - `encrypted_content` 字段名是历史包袱,**实际是明文** `format!("{PREFIX}\n{summary}")`
//!   (`codex-rs/core/src/compact.rs:262`)。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use futures_util::stream::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};

use crate::types::{AdapterError, ByteStream, ResponsePlan};

use super::request::responses_body_to_chat_body_for_provider;

/// 抄自 `openai/codex` 仓库 `codex-rs/core/templates/compact/prompt.md` (Apache-2).
const COMPACT_SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// 抄自 `openai/codex` 仓库 `codex-rs/core/templates/compact/summary_prefix.md` (Apache-2).
/// Codex CLI 反序列化 compact 响应后,通过 `is_summary_message`(`startswith(PREFIX)`)
/// 识别这段文本是 compaction summary 并接管历史回放。**前缀必须保持字面一致**。
const COMPACT_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// `COMPACT_USER_MESSAGE_MAX_TOKENS` from `codex-rs/core/src/compact.rs:48`.
const COMPACT_MAX_OUTPUT_TOKENS: u32 = 20_000;

/// 收上游 chat completions 响应的最大字节数,防止异常 provider 把我们打挂内存。
/// 32 MB 远超合理 chat completion 响应大小(typical 几十 KB)。
const MAX_UPSTREAM_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// 判断入站 path 是否是 `/responses/compact`(含可选 `/v1/`、`/openai/v1/` 前缀)。
pub(super) fn is_compact_path(path: &str) -> bool {
    let path_only = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    let path_only = path_only.strip_prefix("/openai").unwrap_or(path_only);
    let path_only = path_only.strip_prefix("/v1").unwrap_or(path_only);
    path_only.trim_end_matches('/') == "/responses/compact"
}

/// 把 Codex CLI 的 `CompactionInput` JSON 改写成上游 `/chat/completions` 请求体。
///
/// 策略:
/// - 注入 `COMPACT_SUMMARIZATION_PROMPT` 作为 `instructions`(覆盖原 instructions)
/// - 保留 `input` 数组(原对话历史),交给现有 `responses_body_to_chat_body_for_provider`
///   做 ResponseItem → ChatMessage 转换、merge consecutive、tool call repair、vision 剥离等
/// - `stream = false`(上游回完整 chat completion JSON,不是 SSE)
/// - 丢弃 `tools`(摘要任务不需要工具调用)
pub(super) fn build_compact_chat_request(
    body_bytes: &[u8],
    provider: &Provider,
) -> Result<Vec<u8>, AdapterError> {
    let parsed: Value = serde_json::from_slice(body_bytes)
        .map_err(|e| AdapterError::BadRequest(format!("compact body 不是合法 JSON: {e}")))?;
    let model = parsed.get("model").cloned().unwrap_or(Value::Null);
    let input = parsed
        .get("input")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));

    let synthetic_responses_body = json!({
        "model": model,
        "instructions": COMPACT_SUMMARIZATION_PROMPT,
        "input": input,
        "stream": false,
        "max_output_tokens": COMPACT_MAX_OUTPUT_TOKENS,
    });

    let chat_body =
        responses_body_to_chat_body_for_provider(&synthetic_responses_body, Some(provider))?;
    serde_json::to_vec(&chat_body)
        .map_err(|e| AdapterError::Internal(format!("re-serialize compact body: {e}")))
}

/// 把上游 `/chat/completions` 的非流式 JSON 响应包装成 Codex CLI 期待的
/// compact response。
///
/// 当上游返回非 2xx 时,把它的 status + body 透传给客户端(让 Codex CLI
/// 拿到上游真实错误而不是被我们包成"假成功")。
pub(super) fn build_compact_response_plan(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
) -> Result<ResponsePlan, AdapterError> {
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);

    let stream_with_logic = Box::pin(futures_util::stream::once(async move {
        let body = collect_and_wrap_compact_body(upstream_status, upstream_stream)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok::<Bytes, std::io::Error>(Bytes::from(body))
    }));

    Ok(ResponsePlan {
        status: if upstream_status.is_success() {
            StatusCode::OK
        } else {
            upstream_status
        },
        headers: upstream_headers,
        stream: stream_with_logic,
    })
}

async fn collect_and_wrap_compact_body(
    upstream_status: StatusCode,
    mut upstream_stream: ByteStream,
) -> Result<Vec<u8>, AdapterError> {
    let mut buf = Vec::new();
    while let Some(chunk) = upstream_stream.next().await {
        let bytes = chunk.map_err(|e| AdapterError::Internal(format!("upstream io: {e}")))?;
        if buf.len() + bytes.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(AdapterError::Internal(format!(
                "compact upstream response > {MAX_UPSTREAM_RESPONSE_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&bytes);
    }

    if !upstream_status.is_success() {
        // 上游错误:body 可能是 HTML/JSON/纯文本,无脑透传给客户端
        // (Codex CLI 收到非 2xx 会显示原始 body)。
        return Ok(buf);
    }

    let parsed: Value = serde_json::from_slice(&buf).map_err(|e| {
        let preview: String = String::from_utf8_lossy(&buf).chars().take(500).collect();
        AdapterError::Internal(format!(
            "compact upstream non-JSON response: {e}; first 500 chars: {preview}"
        ))
    })?;
    let summary = parsed
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AdapterError::Internal("compact upstream missing choices[0].message.content".to_owned())
        })?
        .trim()
        .to_owned();

    let encrypted_content = format!("{COMPACT_SUMMARY_PREFIX}\n{summary}");
    let compact_response = json!({
        "output": [{
            "type": "compaction",
            "encrypted_content": encrypted_content,
        }]
    });
    serde_json::to_vec(&compact_response)
        .map_err(|e| AdapterError::Internal(format!("serialize compact response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use codex_app_transfer_registry::Provider;
    use futures_util::stream;
    use serde_json::json;

    fn make_provider() -> Provider {
        let mut p = Provider {
            id: "mimo".into(),
            name: "MiMo".into(),
            base_url: "https://example.com/v1".into(),
            auth_scheme: "bearer".into(),
            api_format: "responses".into(),
            api_key: String::new(),
            models: Default::default(),
            extra_headers: Default::default(),
            model_capabilities: Default::default(),
            request_options: Default::default(),
            is_builtin: false,
            sort_index: 0,
            extra: Default::default(),
        };
        p.models.insert("default".into(), "mimo-v2.5".into());
        p
    }

    #[test]
    fn is_compact_path_recognizes_v1_and_bare_forms() {
        assert!(is_compact_path("/responses/compact"));
        assert!(is_compact_path("/v1/responses/compact"));
        assert!(is_compact_path("/openai/v1/responses/compact"));
        assert!(is_compact_path("/responses/compact?foo=bar"));
        assert!(is_compact_path("/responses/compact/"));
        // 负向
        assert!(!is_compact_path("/responses"));
        assert!(!is_compact_path("/responses/compact/extra"));
        assert!(!is_compact_path("/chat/completions"));
    }

    #[test]
    fn build_compact_chat_request_injects_summarization_prompt_and_drops_stream() {
        let p = make_provider();
        let body = json!({
            "model": "mimo-v2.5",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hello"}
                ]}
            ],
            "instructions": "ORIGINAL_PROJECT_INSTRUCTIONS",
            "tools": [{"type": "function", "name": "shell"}],
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();

        // 注入了 SUMMARIZATION_PROMPT 作为 system,覆盖原 instructions
        let messages = parsed["messages"].as_array().unwrap();
        let system_msg = messages
            .iter()
            .find(|m| m["role"] == "system")
            .expect("system message");
        assert!(
            system_msg["content"]
                .as_str()
                .unwrap_or("")
                .contains("CONTEXT CHECKPOINT COMPACTION"),
            "system 应含 SUMMARIZATION_PROMPT 关键字"
        );
        assert!(
            !messages.iter().any(|m| m["content"]
                .as_str()
                .unwrap_or("")
                .contains("ORIGINAL_PROJECT_INSTRUCTIONS")),
            "原 instructions 应被覆盖,不应进 messages"
        );
        // 历史 user message 保留
        assert!(messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"].as_str().unwrap_or("").contains("hello")));
        // 不带 stream(stream=false 在 chat body 转换里会被丢)
        assert!(parsed.get("stream").is_none() || parsed["stream"] == false);
    }

    fn one_chunk_stream(bytes: Vec<u8>) -> ByteStream {
        Box::pin(stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(bytes))
        }))
    }

    #[tokio::test]
    async fn collect_and_wrap_extracts_summary_into_compaction_item() {
        let upstream_body = serde_json::to_vec(&json!({
            "id": "chatcmpl_x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "summary text body"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }))
        .unwrap();

        let body = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        let output = parsed["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "compaction");
        let enc = output[0]["encrypted_content"].as_str().unwrap();
        assert!(
            enc.starts_with(COMPACT_SUMMARY_PREFIX),
            "encrypted_content 必须以 SUMMARY_PREFIX 开头(Codex CLI 用它识别 summary)"
        );
        assert!(enc.ends_with("summary text body"));
    }

    #[tokio::test]
    async fn collect_and_wrap_chunked_upstream_response() {
        // 上游分多 chunk 来,我们应该正确拼接后解析
        let upstream_body = serde_json::to_vec(&json!({
            "choices": [{"message": {"content": "chunked summary"}, "finish_reason": "stop"}]
        }))
        .unwrap();
        let mid = upstream_body.len() / 2;
        let part1 = upstream_body[..mid].to_vec();
        let part2 = upstream_body[mid..].to_vec();
        let s: ByteStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from(part1)),
            Ok(Bytes::from(part2)),
        ]));
        let body = collect_and_wrap_compact_body(StatusCode::OK, s)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["output"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .ends_with("chunked summary"));
    }

    #[tokio::test]
    async fn collect_and_wrap_passes_through_upstream_error_body() {
        // 上游 4xx/5xx 时直接透传 body,让 Codex CLI 看到真实错误
        let body = collect_and_wrap_compact_body(
            StatusCode::BAD_REQUEST,
            one_chunk_stream(b"<html>upstream rate limit</html>".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(body, b"<html>upstream rate limit</html>");
    }

    #[tokio::test]
    async fn collect_and_wrap_rejects_oversized_response() {
        let huge: Vec<u8> = vec![0; MAX_UPSTREAM_RESPONSE_BYTES + 1];
        let err = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(huge))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("> "));
    }

    #[tokio::test]
    async fn collect_and_wrap_errors_on_missing_message_content() {
        let upstream_body =
            serde_json::to_vec(&json!({"choices": [{"finish_reason": "stop"}]})).unwrap();
        let err = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("missing choices[0].message.content"));
    }
}
