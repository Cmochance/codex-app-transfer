//! grok.com SSE → Codex Responses SSE 转换状态机。
//!
//! ## 输入(grok.com SSE)
//!
//! 每行一个 JSON object,**无** `data: ` 前缀:
//!
//! ```text
//! {"result":{"response":{"userResponse":{...}}}}
//! {"result":{"response":{"conversation":{...}}}}
//! {"result":{"response":{"token":"...","isThinking":true,"messageTag":"header"}}}
//! {"result":{"response":{"token":"...","isThinking":true,"messageTag":"summary"}}}
//! {"result":{"response":{"toolUsageCard":{...},"messageTag":"tool_usage_card"}}}
//! {"result":{"response":{"webSearchResults":{...},"messageTag":"raw_function_result"}}}
//! {"result":{"response":{"token":"...","messageTag":"final"}}}
//! {"result":{"response":{"isSoftStop":true,"responseId":"..."}}}
//! {"result":{"response":{"modelResponse":{...}}}}
//! ```
//!
//! Wrapping 有两种形态(`{"result": {...}}` vs `{"result": {"response": {...}}}`),
//! [`extract_response_frame`] 兼容两者。
//!
//! ## 输出(Codex Responses SSE)
//!
//! 标准 OpenAI Responses event stream:
//!
//! ```text
//! event: response.created\n
//! data: {"type":"response.created","response":{...}}\n\n
//!
//! event: response.output_text.delta\n
//! data: {"type":"response.output_text.delta","delta":"hello"}\n\n
//!
//! event: response.completed\n
//! data: {"type":"response.completed",...}\n\n
//! ```
//!
//! ## R3 PoC 范围(本文件当前实现)
//!
//! - ✅ 文本 token(`messageTag=final`)→ `response.output_text.delta`
//! - ✅ thinking token(`messageTag=header`/`summary`)→ 暂时丢弃(R1 加 reasoning.delta)
//! - ✅ 流末 `isSoftStop` → `response.completed`(防御:流断也补)
//! - ✅ `userResponse` 帧 → 忽略(已在请求侧锚定)
//! - ⚠️ `tool_usage_card` 帧 → R3 不处理(R1 加 function_call event)
//! - ⚠️ `raw_function_result` 帧 → R3 不处理(R1 加 tool result + citations annotation)
//! - ⚠️ `conversation` 帧 → R3 暂不持久化 conversationId
//!
//! 详细帧 schema 见 [`super::types`] 各类型 doc comment(协议事实来源已注明)。
//!
//! ## 实现 pattern
//!
//! 用 [`futures_util::stream::unfold`] + `VecDeque<Bytes>` 状态机 buffer pending events
//! ——一次 upstream chunk parse 可能产出多个 Codex events,unfold 单 step 单 yield,
//! 用 deque 暂存。这与 [`crate::gemini_native::response`] 同一 pattern,无新依赖。

use bytes::Bytes;
use futures_util::stream::{self, Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;

use crate::grok_web::parent_response::global_tracker;
use crate::grok_web::types::{GrokMessageTag, GrokResponseFrame};
use crate::types::ByteStream;

/// 把 SSE envelope 展平到 [`GrokResponseFrame`]。
///
/// 兼容两种 wrapping:`{"result": {...}}`(旧)和 `{"result": {"response": {...}}}`(新)。
pub(crate) fn extract_response_frame(envelope: &serde_json::Value) -> Option<GrokResponseFrame> {
    let result = envelope.get("result")?;
    let inner = result.get("response").unwrap_or(result);
    serde_json::from_value::<GrokResponseFrame>(inner.clone()).ok()
}

/// SSE 转换状态机入口(R3 PoC 简化版)。
///
/// 把上游 grok.com newline-delimited JSON 流转成 OpenAI Responses event stream。
///
/// **错误语义保证**(review-feedback A2):
/// - 上游 transport `Err` 不直接 yield 给客户端(那会让 Codex APP 看到截断+
///   一个无标签 io::Error,然后被后续 `response.completed` 伪装成成功)。
/// - 改为:catch transport err → push 合规 `response.failed` 事件到 pending →
///   设 `emitted_completed=true` 把流末的防御 `response.completed` gate 掉。
/// - 流末没收到 `final` token 也没收到 `isSoftStop` 时,补 `response.failed`
///   而不是 `response.completed`(避免把"上游中断"伪装成"成功完成")。
pub fn convert_grok_sse_to_responses_sse(
    upstream_stream: ByteStream,
    response_id: String,
) -> ByteStream {
    let initial_event = emit_response_created(&response_id);
    let mut state = ConvState {
        upstream: upstream_stream,
        response_id,
        line_buf: String::new(),
        pending: VecDeque::with_capacity(8),
        emitted_completed: false,
        upstream_exhausted: false,
        received_any_final_token: false,
    };
    // 先把 response.created 塞进 pending(unfold 第一步立即 yield)
    state.pending.push_back(initial_event);

    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(
        stream::unfold(state, |mut s| async move {
            loop {
                // 1. 有 pending event 立即 yield
                if let Some(event) = s.pending.pop_front() {
                    return Some((Ok(event), s));
                }
                // 2. 上游已 exhausted,看是否需要补 completed / failed 防御
                if s.upstream_exhausted {
                    if !s.emitted_completed {
                        s.emitted_completed = true;
                        // received_any_final_token=false → 上游中断未收到任何最终
                        // 答案,绝不能 emit response.completed 伪装成功(review-feedback A2)
                        let event = if s.received_any_final_token {
                            emit_response_completed(&s.response_id)
                        } else {
                            emit_response_failed(
                                &s.response_id,
                                "upstream_truncated",
                                "grok.com SSE stream ended before emitting any final token or soft-stop",
                            )
                        };
                        return Some((Ok(event), s));
                    }
                    return None;
                }
                // 3. 拉一个上游 chunk
                let chunk_opt = s.upstream.next().await;
                let Some(chunk_res) = chunk_opt else {
                    // EOF:先 drain 尾部未换行片段(review-feedback I4),再走 exhausted 分支
                    if !s.line_buf.is_empty() {
                        s.line_buf.push('\n');
                        process_buffered_lines(&mut s);
                    }
                    s.upstream_exhausted = true;
                    continue;
                };
                match chunk_res {
                    Ok(b) => {
                        s.line_buf.push_str(&String::from_utf8_lossy(&b));
                        process_buffered_lines(&mut s);
                    }
                    Err(e) => {
                        // 上游 transport error(review-feedback A2):
                        // 不 yield raw Err(那会让 Codex APP 看到无标签错误后被后续
                        // completed 伪装成功),改 push response.failed 后置 emitted_completed
                        // gate 流末防御。
                        s.pending.push_back(emit_response_failed(
                            &s.response_id,
                            "upstream_transport_error",
                            &format!("grok.com SSE transport error: {e}"),
                        ));
                        s.emitted_completed = true;
                        s.upstream_exhausted = true;
                    }
                }
            }
        }),
    );
    s
}

/// Cap 上游错误 body 防 DoS(同 `gemini_native` `MAX_UPSTREAM_ERROR_BODY_BYTES`)。
const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 65_536;

/// 把上游 grok.com 4xx/5xx 错误流翻译成合规 Codex Responses 失败 SSE 流。
///
/// 流形态固定两个事件:
/// 1. `response.created`(status=in_progress,让 Codex APP 进入流接收状态)
/// 2. `response.failed`(status=failed,带 classify 后的 error.code + grok message)
///
/// **classify 规则**:
/// - `401` → `auth_error`(cookie 过期 / 错误)
/// - `403` → `permission_denied`(可能 Cloudflare 挑战 / 账号风控)
/// - `408` / `504` → `timeout`
/// - `429` → `rate_limited`
/// - `5xx` → `server_error`
/// - 其他 → `upstream_error`
///
/// **防御**:
/// - body cap [`MAX_UPSTREAM_ERROR_BODY_BYTES`] 字节防 DoS
/// - 非 UTF-8 用 `from_utf8_lossy`,后缀标 `(non-UTF-8 body)`
/// - mid-read transport `Err` → `upstream_transport_error` code,带 err 文本
/// - empty body / 解析失败 → 不打断,仍 emit `response.failed` 带通用 message
pub fn convert_grok_error_to_responses_failure_stream(
    upstream_status: http::StatusCode,
    upstream_stream: ByteStream,
    response_id: String,
) -> ByteStream {
    let status_u16 = upstream_status.as_u16();
    let code = classify_grok_error_status(status_u16);

    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(
        stream::unfold((upstream_stream, false), move |(mut input, finished)| {
            let response_id = response_id.clone();
            let code = code.to_owned();
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
                let (final_code, message) = if let Some(transport) = transport_err {
                    (
                            "upstream_transport_error".to_owned(),
                            format!(
                                "grok.com HTTP {status_u16} but transport err during body read: {transport}",
                            ),
                        )
                } else {
                    let msg = if body_text.is_empty() {
                        format!("grok.com HTTP {status_u16} (empty body)")
                    } else {
                        format!("grok.com HTTP {status_u16}: {body_text}")
                    };
                    (code, msg)
                };

                // 两个事件拼一起 yield(避免 mock stream 单 chunk 截断 SSE 帧)
                let mut buf = Vec::with_capacity(512);
                buf.extend_from_slice(&emit_response_created(&response_id));
                buf.extend_from_slice(&emit_response_failed(&response_id, &final_code, &message));
                Some((Ok(Bytes::from(buf)), (input, true)))
            }
        }),
    );
    s
}

fn classify_grok_error_status(status_u16: u16) -> &'static str {
    match status_u16 {
        401 => "auth_error",
        403 => "permission_denied",
        408 | 504 => "timeout",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "upstream_error",
    }
}

/// `unfold` 内部状态。
struct ConvState {
    upstream: ByteStream,
    response_id: String,
    /// 已读但未切完 line 的尾部缓冲。
    line_buf: String,
    /// 已 parse 出来还没 yield 的 SSE events。
    pending: VecDeque<Bytes>,
    emitted_completed: bool,
    upstream_exhausted: bool,
    /// 是否收到过任何 `messageTag=final` 的非空 token。流末没有 `isSoftStop`
    /// 时,本字段决定补 `response.completed`(true)还是 `response.failed`
    /// `upstream_truncated`(false)。review-feedback A2 / I4 防御 gate。
    received_any_final_token: bool,
}

/// 从 `line_buf` 切出所有完整 line,parse 成 Codex SSE events 入 pending。
///
/// **error envelope 帧检测**(review-feedback H3):grok.com 偶尔在流末
/// emit `{"error": {...}}` 替代 `[DONE]`,SSE schema 文档化在 `types.rs::GrokSseEnvelope`。
/// 之前实现忽略这种帧,本函数显式检测后翻译成 `response.failed`,gate 后续防御。
fn process_buffered_lines(state: &mut ConvState) {
    while let Some(idx) = state.line_buf.find('\n') {
        let raw: String = state.line_buf.drain(..=idx).collect();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // 非 JSON 行 —— 协议漂移信号,记 warn(operator 可 grep);不打断流。
            tracing::warn!(
                error_id = "GROK_STREAM_NONJSON_LINE",
                preview = %trimmed.chars().take(120).collect::<String>(),
                "grok_web SSE got non-JSON line (possibly protocol drift)"
            );
            continue;
        };
        // error envelope 优先(review-feedback H3):部分流末 grok.com emit
        // `{"error": {...}}` 替代 done 信号。检测后翻译成 response.failed。
        if let Some(err_obj) = envelope.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("upstream error frame without message")
                .to_owned();
            state.pending.push_back(emit_response_failed(
                &state.response_id,
                "grok_stream_error",
                &msg,
            ));
            state.emitted_completed = true;
            state.upstream_exhausted = true;
            continue;
        }
        let Some(frame) = extract_response_frame(&envelope) else {
            // envelope 缺 result 字段 → 协议漂移,记 warn(review-feedback M4)
            tracing::warn!(
                error_id = "GROK_STREAM_UNKNOWN_ENVELOPE",
                preview = %trimmed.chars().take(120).collect::<String>(),
                "grok_web SSE envelope missing `result` field"
            );
            continue;
        };
        translate_frame(state, &frame);
    }
}

/// 把一帧 grok response 翻译成 0~N 个 Codex SSE events,入 pending。
///
/// **R3 PoC 范围**:只翻译 `final` 文本 token + `isSoftStop` + 录入
/// `modelResponse.responseId` 到 [`ParentResponseTracker`](crate::grok_web::parent_response::ParentResponseTracker)。
/// 其他帧(`tool_usage_card` / `raw_function_result` / `userResponse` /
/// `conversation` / `finalMetadata`)按 R3 scope 暂不翻译,但用 `tracing::debug!`
/// 记录帧类型(review-feedback I5 / H4 防御 — operator 可 grep 验证 R1 即将
/// 接管的帧种类是否在流里真的出现)。
fn translate_frame(state: &mut ConvState, frame: &GrokResponseFrame) {
    let tag = frame.message_tag.as_deref().map(GrokMessageTag::parse);

    // 最终回答 token(非 thinking)
    if matches!(tag, Some(GrokMessageTag::Final)) && frame.is_thinking != Some(true) {
        if let Some(tok) = &frame.token {
            if !tok.is_empty() {
                state.pending.push_back(emit_output_text_delta(tok));
                state.received_any_final_token = true;
            }
        }
    }

    // modelResponse 帧 → 录入 ParentResponseTracker(review-feedback A5/H4)。
    // grok.com 流末 emit modelResponse 含完整 metadata + responseId;客户端的
    // `previous_response_id`(Codex Responses ID)→ grok 的 `responseId` 映射
    // 必须在此录入,否则下一轮 follow-up 永远 tracker miss → 多轮上下文丢失。
    if let Some(model_response) = &frame.model_response {
        if let Some(grok_response_id) = model_response
            .get("responseId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            global_tracker().record(state.response_id.clone(), grok_response_id);
            tracing::debug!(
                error_id = "GROK_TRACKER_RECORDED",
                codex_response_id = %state.response_id,
                grok_response_id = %grok_response_id,
                "recorded parent_response mapping for multi-turn anchoring"
            );
        }
    }

    // 流末标志
    if frame.is_soft_stop == Some(true) && !state.emitted_completed {
        state.emitted_completed = true;
        state
            .pending
            .push_back(emit_response_completed(&state.response_id));
    }

    // 其他帧 R3 不翻译,debug log 记录(R1 PR 会逐一接管)
    if let Some(t) = tag {
        if matches!(
            t,
            GrokMessageTag::Header
                | GrokMessageTag::Summary
                | GrokMessageTag::ToolUsageCard
                | GrokMessageTag::RawFunctionResult
        ) {
            tracing::debug!(
                error_id = "GROK_FRAME_R3_DROPPED",
                tag = ?t,
                "grok_web R3 dropping frame (R1 will handle)"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Event 构造 helpers
// ─────────────────────────────────────────────────────────────────────────────

fn emit_response_created(response_id: &str) -> Bytes {
    emit_event(
        "response.created",
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
            }
        }),
    )
}

fn emit_output_text_delta(delta: &str) -> Bytes {
    emit_event(
        "response.output_text.delta",
        serde_json::json!({
            "type": "response.output_text.delta",
            "delta": delta,
        }),
    )
}

/// 合规 OpenAI Responses `response.failed` 事件构造。
///
/// 用于以下场景(全部 review-feedback A1/A2/H3 防护):
/// - 上游 4xx/5xx 错误 → `mapper/grok_web` 调,classify by status code
/// - 上游 transport `Err` mid-stream → `convert_grok_sse_to_responses_sse` 内部调
/// - error envelope `{"error":{...}}` 帧 → `process_buffered_lines` 内部调
/// - 流末无 `final` token / `isSoftStop` → `unfold` 流末防御调
///
/// 字段对齐 [OpenAI Responses API spec](https://platform.openai.com/docs/api-reference/responses):
/// `response.failed` 顶层 `error.code` + `error.message` 两个必需字段。
pub(crate) fn emit_response_failed(response_id: &str, code: &str, message: &str) -> Bytes {
    emit_event(
        "response.failed",
        serde_json::json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "failed",
                "error": {
                    "code": code,
                    "message": message,
                }
            }
        }),
    )
}

fn emit_response_completed(response_id: &str) -> Bytes {
    emit_event(
        "response.completed",
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
            }
        }),
    )
}

/// 把 `event: <name>\ndata: <json>\n\n` 拼成 SSE 字节段。
fn emit_event(event: &str, data: serde_json::Value) -> Bytes {
    let mut out = String::with_capacity(128);
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_byte_stream(lines: Vec<&'static str>) -> ByteStream {
        let chunks: Vec<Result<Bytes, std::io::Error>> = lines
            .into_iter()
            .map(|s| Ok(Bytes::from(format!("{s}\n"))))
            .collect();
        Box::pin(stream::iter(chunks))
    }

    async fn collect(stream: ByteStream) -> String {
        let mut s = stream;
        let mut out = String::new();
        while let Some(chunk) = s.next().await {
            out.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        out
    }

    #[test]
    fn extract_response_frame_old_wrapping() {
        let env = serde_json::json!({
            "result": {
                "token": "hi",
                "messageTag": "final",
                "isThinking": false
            }
        });
        let frame = extract_response_frame(&env).unwrap();
        assert_eq!(frame.token.as_deref(), Some("hi"));
        assert_eq!(frame.message_tag.as_deref(), Some("final"));
    }

    #[test]
    fn extract_response_frame_new_wrapping() {
        let env = serde_json::json!({
            "result": {
                "response": {
                    "token": "hello",
                    "messageTag": "final",
                    "isThinking": false
                }
            }
        });
        let frame = extract_response_frame(&env).unwrap();
        assert_eq!(frame.token.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn simple_text_stream_emits_created_delta_completed() {
        let lines = vec![
            r#"{"result":{"response":{"token":"hello","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"token":" world","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true,"responseId":"r1"}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "resp_abc".into(),
        ))
        .await;
        assert!(out.contains("event: response.created"));
        assert!(out.contains(r#""delta":"hello""#));
        assert!(out.contains(r#""delta":" world""#));
        assert!(out.contains("event: response.completed"));
    }

    #[tokio::test]
    async fn thinking_frames_do_not_leak_to_output_text() {
        let lines = vec![
            r#"{"result":{"response":{"token":"thinking...","messageTag":"header","isThinking":true}}}"#,
            r#"{"result":{"response":{"token":"answer","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(!out.contains("thinking..."));
        assert!(out.contains(r#""delta":"answer""#));
    }

    #[tokio::test]
    async fn stream_end_without_soft_stop_still_emits_completed() {
        let lines = vec![
            r#"{"result":{"response":{"token":"x","messageTag":"final","isThinking":false}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        let created_count = out.matches("event: response.created").count();
        let completed_count = out.matches("event: response.completed").count();
        assert_eq!(created_count, 1);
        assert_eq!(completed_count, 1);
    }

    #[tokio::test]
    async fn no_final_token_no_softstop_emits_response_failed_not_completed() {
        // review-feedback A2 防御:流末没收到 final token 也没 isSoftStop 时,补
        // response.failed `upstream_truncated`,而不是 response.completed(那会
        // 把上游中断伪装成成功)。
        let lines = vec![
            r#"{"result":{"response":{"token":"thinking","isThinking":true,"messageTag":"header"}}}"#,
            // 没有 final token, 没有 isSoftStop
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("event: response.created"));
        assert!(
            out.contains("event: response.failed"),
            "must emit failed, got: {out}"
        );
        assert!(out.contains(r#""code":"upstream_truncated""#));
        assert!(
            !out.contains("event: response.completed"),
            "must NOT emit completed when truncated"
        );
    }

    #[tokio::test]
    async fn transport_error_emits_response_failed_not_raw_io_error() {
        // review-feedback A2 防御:上游 mid-stream transport err 不 yield raw io::Error,
        // 转 response.failed `upstream_transport_error` event。
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(
                r#"{"result":{"response":{"token":"partial","messageTag":"final","isThinking":false}}}"#.to_owned()
                + "\n",
            )),
            Err(std::io::Error::other("simulated network drop")),
        ];
        let upstream: ByteStream = Box::pin(stream::iter(chunks));
        let out = collect(convert_grok_sse_to_responses_sse(upstream, "r".into())).await;
        assert!(out.contains(r#""delta":"partial""#));
        assert!(out.contains("event: response.failed"));
        assert!(out.contains(r#""code":"upstream_transport_error""#));
        assert!(
            !out.contains("event: response.completed"),
            "transport err must NOT result in completed event"
        );
    }

    #[tokio::test]
    async fn error_envelope_frame_emits_response_failed() {
        // review-feedback H3 防御:`{"error": {...}}` envelope 帧检测后翻译
        let lines = vec![r#"{"error": {"message": "rate limited by upstream", "code": 429}}"#];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("event: response.failed"));
        assert!(out.contains(r#""code":"grok_stream_error""#));
        assert!(out.contains("rate limited"));
    }

    #[tokio::test]
    async fn model_response_frame_records_to_parent_tracker() {
        // review-feedback A5/H4 决定性:modelResponse 帧 → ParentResponseTracker.record
        use crate::grok_web::parent_response::global_tracker;
        let codex_id = format!("resp_test_{}", uuid_seed());
        let grok_id = "9f82a10c-1234-1234-1234-bdeb21a37b16";
        let lines = vec![Box::leak(
            format!(
                r#"{{"result":{{"response":{{"modelResponse":{{"responseId":"{grok_id}"}}}}}}}}"#
            )
            .into_boxed_str(),
        ) as &'static str];
        let _ = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            codex_id.clone(),
        ))
        .await;
        // 流执行后 tracker 应有记录
        assert_eq!(
            global_tracker().get(&codex_id).as_deref(),
            Some(grok_id),
            "multi-turn anchoring broken: modelResponse → tracker.record 未生效"
        );
    }

    fn uuid_seed() -> String {
        // 仅供 test 跨案例隔离 codex_response_id;不需要密码学随机
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_else(|_| "0".to_owned())
    }

    #[tokio::test]
    async fn upstream_4xx_translates_to_response_failed() {
        // review-feedback A1:401 → response.failed code=auth_error
        let body = Bytes::from_static(b"{\"error\":\"unauthorized\"}");
        let upstream: ByteStream = Box::pin(stream::iter(vec![Ok(body)]));
        let out = collect(convert_grok_error_to_responses_failure_stream(
            http::StatusCode::UNAUTHORIZED,
            upstream,
            "r".into(),
        ))
        .await;
        assert!(out.contains("event: response.created"));
        assert!(out.contains("event: response.failed"));
        assert!(out.contains(r#""code":"auth_error""#));
        assert!(out.contains("unauthorized"));
    }

    #[tokio::test]
    async fn upstream_5xx_translates_to_server_error_code() {
        let body = Bytes::from_static(b"internal explosion");
        let upstream: ByteStream = Box::pin(stream::iter(vec![Ok(body)]));
        let out = collect(convert_grok_error_to_responses_failure_stream(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            upstream,
            "r".into(),
        ))
        .await;
        assert!(out.contains(r#""code":"server_error""#));
    }

    #[tokio::test]
    async fn handles_chunk_split_mid_line() {
        // 模拟上游 byte chunk 切在 line 中间(常见)
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(r#"{"result":{"response":{"to"#.to_owned())),
            Ok(Bytes::from(
                r#"ken":"x","messageTag":"final","isThinking":false}}}"#.to_owned() + "\n",
            )),
            Ok(Bytes::from(
                r#"{"result":{"response":{"isSoftStop":true}}}"#.to_owned() + "\n",
            )),
        ];
        let upstream: ByteStream = Box::pin(stream::iter(chunks));
        let out = collect(convert_grok_sse_to_responses_sse(upstream, "r".into())).await;
        assert!(out.contains(r#""delta":"x""#));
        assert_eq!(out.matches("event: response.completed").count(), 1);
    }
}
