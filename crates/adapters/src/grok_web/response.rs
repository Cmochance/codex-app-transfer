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
        next_output_index: 0,
        open_reasoning: None,
        open_message: None,
        pending_url_citations: Vec::new(),
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
                        // 流末/中断前 close 所有 open items(R1 PR-1 P1 + PR-3):
                        // - reasoning item 永远 in_progress 会让 Codex APP Thinking UI 卡
                        // - message item 同理
                        close_reasoning_if_open(&mut s);
                        close_message_if_open(&mut s);
                        // pending 可能多了 close 事件,优先 yield 它们
                        if let Some(event) = s.pending.pop_front() {
                            return Some((Ok(event), s));
                        }
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

/// 当前打开的 reasoning item lifecycle 状态(R1 PR-1 P1 fix)。
///
/// 对齐 [`crate::gemini_native::response`] 的 reasoning emit 三段:
/// `output_item.added(reasoning)` + `reasoning_summary_part.added` →
/// `reasoning_summary_text.delta` * N → `reasoning_summary_text.done` +
/// `reasoning_summary_part.done` + `output_item.done`。
///
/// 必须在 final token / soft_stop / 上游中断 emit 前 close,否则 Codex APP
/// 会等待 reasoning item 闭合而卡住。
struct OpenReasoning {
    item_id: String,
    output_index: u32,
    text_acc: String,
}

/// 当前打开的 message item lifecycle 状态(R1 PR-3)。
///
/// 对齐 OpenAI Responses 消息体三段:
/// `output_item.added(message)` + `content_part.added(output_text)` →
/// `output_text.delta` * N + 累积 `output_text.annotation.added` →
/// `output_text.done` + `content_part.done` + `output_item.done`(item.content
/// 含完整 annotations 数组)。
///
/// `annotations_acc` 收集已 emit 的 url_citation,close 时回灌到 item.content。
struct OpenMessage {
    item_id: String,
    output_index: u32,
    text_acc: String,
    annotations_acc: Vec<serde_json::Value>,
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
    /// 单调递增的 output_index,用于 reasoning / message item 编号
    /// (对齐 `gemini_native` ResponsesConverter 行为)。
    next_output_index: u32,
    /// 当前打开中的 reasoning item(R1 PR-1 P1 fix);final / soft_stop 前 close。
    open_reasoning: Option<OpenReasoning>,
    /// 当前打开中的 message item(R1 PR-3);final token 触发 open,soft_stop / 上游中断 close。
    open_message: Option<OpenMessage>,
    /// thinking 阶段累积的 url citation(webSearchResults / xSearchResults),
    /// 第一个 final token 触发 open_message 后 flush 为 annotation.added 事件。
    pending_url_citations: Vec<serde_json::Value>,
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

    // 最终回答 token(非 thinking)— R1 PR-3:
    //
    // 改造 — 包进 message item lifecycle 而不是裸 push output_text.delta。
    // 首个非空 final token 触发 open_message_if_needed(emit output_item.added(message)
    // + content_part.added(output_text)),然后 emit output_text.delta。
    // pending_url_citations(thinking 阶段累积的 webSearchResults)在首次 open
    // 后立即 flush 为 annotation.added 事件,关联 message item_id。
    if matches!(tag, Some(GrokMessageTag::Final)) && frame.is_thinking != Some(true) {
        if let Some(tok) = &frame.token {
            if !tok.is_empty() {
                open_message_if_needed(state);
                if let Some(msg) = state.open_message.as_mut() {
                    msg.text_acc.push_str(tok);
                    let item_id = msg.item_id.clone();
                    let output_index = msg.output_index;
                    state.pending.push_back(emit_output_text_delta_for_item(
                        &item_id,
                        output_index,
                        tok,
                    ));
                    state.received_any_final_token = true;
                    flush_pending_url_citations(state);
                }
            }
        }
    }

    // thinking token(R1 PR-1 P1 fix):messageTag=header/summary 是 grok.com
    // 思考阶段子标记。原 R3 PoC 静默丢弃;原 PR #130 改成 emit
    // `response.reasoning.delta`(自创事件类型,客户端不认)。
    //
    // chatgpt-codex-connector PR #130 P1 反馈:本仓库 reasoning UI 走
    // `response.reasoning_summary_part.added` + `response.reasoning_summary_text.delta`
    // + `response.reasoning_summary_text.done` + `response.reasoning_summary_part.done`
    // + `response.output_item.done` 这套事件族(见 `gemini_native::response::open_reasoning`)。
    //
    // 本 fix 对齐 gemini_native 完整 reasoning lifecycle:
    // 1. 首个 thinking token 触发 `open_reasoning_part`(emit output_item.added +
    //    reasoning_summary_part.added,创建 item_id rs_<uuid> + output_index)
    // 2. 每个 thinking token emit `reasoning_summary_text.delta`,关联 item_id
    // 3. final token / soft_stop / 上游中断前 close(emit text.done + part.done +
    //    output_item.done with type=reasoning,Codex APP 才能 close UI 项)
    if matches!(
        tag,
        Some(GrokMessageTag::Header) | Some(GrokMessageTag::Summary)
    ) && frame.is_thinking == Some(true)
    {
        if let Some(tok) = &frame.token {
            if !tok.is_empty() {
                open_reasoning_if_needed(state);
                if let Some(rs) = state.open_reasoning.as_mut() {
                    rs.text_acc.push_str(tok);
                    state.pending.push_back(emit_reasoning_summary_text_delta(
                        &rs.item_id,
                        rs.output_index,
                        tok,
                    ));
                }
            }
        }
    }

    // final token / softStop 之前必须 close open_reasoning(P1 fix lifecycle)
    if matches!(tag, Some(GrokMessageTag::Final)) && frame.is_thinking != Some(true) {
        close_reasoning_if_open(state);
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
            global_tracker().record(
                crate::grok_web::parent_response::CodexResponseId::from(state.response_id.clone()),
                crate::grok_web::parent_response::GrokResponseId::from(grok_response_id),
            );
            tracing::debug!(
                error_id = "GROK_TRACKER_RECORDED",
                codex_response_id = %state.response_id,
                grok_response_id = %grok_response_id,
                "recorded parent_response mapping for multi-turn anchoring"
            );
        }
    }

    // tool_usage_card 帧(R1 PR-2):模型调用 grok 内置 / MCP 工具,emit 帧
    // 含 toolUsageCard.{webSearch|browsePage|...|mcp} 字段。我们把它格式化成
    // markdown 一行(`🔍 web_search: query="..."` / `🔌 test___ask_question: {...}`)
    // 拼到当前 reasoning summary —— grok server-side state 自己 dispatch,
    // 不需要 Codex APP 走 function_call event 回调(那是 client-side tool 路径)。
    if matches!(tag, Some(GrokMessageTag::ToolUsageCard)) {
        if let Some(card) = &frame.tool_usage_card {
            if let Some(call) = crate::grok_web::types::GrokToolCall::parse(card) {
                let line = format_tool_call_for_reasoning(&call);
                if !line.is_empty() {
                    open_reasoning_if_needed(state);
                    if let Some(rs) = state.open_reasoning.as_mut() {
                        rs.text_acc.push_str(&line);
                        let item_id = rs.item_id.clone();
                        let output_index = rs.output_index;
                        state.pending.push_back(emit_reasoning_summary_text_delta(
                            &item_id,
                            output_index,
                            &line,
                        ));
                    }
                }
            }
        }
    }

    // raw_function_result 数据帧(R1 PR-2 + PR-3 + PR-5):
    // - PR-2:webSearchResults / xSearchResults 转 markdown bullet 拼 reasoning(已有)
    // - PR-3:同时累积 url_citation 到 pending(message open 后 flush annotation.added)
    // - PR-5:**codeExecutionResult** 转 markdown fenced code block + 拼 reasoning。
    //   grok.com 内置 `code_execution` 工具(_TOOL_FMT 已识别)的输出帧。
    //   实测帧形态:`{stdout, stderr?, exitCode?}`(部分字段缺时跳过)。
    if matches!(tag, Some(GrokMessageTag::RawFunctionResult)) {
        let mut summary = String::new();
        if let Some(wsr) = &frame.web_search_results {
            summary.push_str(&format_web_search_results_for_reasoning(wsr));
            accumulate_web_search_url_citations(state, wsr);
        }
        if let Some(xsr) = &frame.x_search_results {
            summary.push_str(&format_x_search_results_for_reasoning(xsr));
            accumulate_x_search_url_citations(state, xsr);
        }
        if let Some(cer) = &frame.code_execution_result {
            summary.push_str(&format_code_execution_result_for_reasoning(cer));
        }
        // R1 PR-6:connector/collection/rag search 帧累积。
        // grok modelResponse 末尾 schema 含 connectorSearchResults /
        // collectionSearchResults / ragResults 字段(实测 R1.js modelResponse
        // 验证存在),用户启用 Notion / Linear / Drive / 自定义 MCP connector 时 emit。
        // 没在 types.rs 加显式字段(实测时数组均为空,schema 不确定);
        // 通过 frame.extra 动态查找 + 保守复用 webSearchResults 形态。
        for key in [
            "connectorSearchResults",
            "collectionSearchResults",
            "ragResults",
        ] {
            if let Some(grouping) = frame.extra.get(key) {
                summary.push_str(&format_generic_search_results_for_reasoning(grouping, key));
                accumulate_generic_search_url_citations(state, grouping);
            }
        }
        if !summary.is_empty() {
            open_reasoning_if_needed(state);
            if let Some(rs) = state.open_reasoning.as_mut() {
                rs.text_acc.push_str(&summary);
                let item_id = rs.item_id.clone();
                let output_index = rs.output_index;
                state.pending.push_back(emit_reasoning_summary_text_delta(
                    &item_id,
                    output_index,
                    &summary,
                ));
            }
        }
    }

    // 流末标志 — softStop 前 close 所有 open items(PR-3:reasoning + message)
    if frame.is_soft_stop == Some(true) && !state.emitted_completed {
        close_reasoning_if_open(state);
        close_message_if_open(state);
        state.emitted_completed = true;
        state
            .pending
            .push_back(emit_response_completed(&state.response_id));
    }
}

/// 把一个 [`GrokToolCall`] 格式化成 reasoning summary 一行 markdown。
///
/// 形态:
/// - Builtin:`\n🔍 web_search: query="..."` / `🌐 browse_page: url="..."`
/// - MCP:    `\n🔌 test___ask_question: {"repoName":"..."}`(args JSON 截断 200 字)
///
/// 前缀 `\n` 用于在 reasoning summary 中独占一行(text_acc 累积时不与前一段粘连)。
fn format_tool_call_for_reasoning(call: &crate::grok_web::types::GrokToolCall) -> String {
    use crate::grok_web::types::GrokToolCall;
    const MAX_ARG_PREVIEW: usize = 200;
    match call {
        GrokToolCall::Builtin { name, args } => {
            let icon = builtin_tool_icon(name);
            // 优先抽 query / url / image_description 等"主参数"为简短形式;
            // 其余 args 字段省略(R1 PR-2 简化,reasoning UI 主要给用户看)
            let primary = primary_arg_for_builtin(name, args);
            if primary.is_empty() {
                format!("\n{icon} {name}")
            } else {
                format!("\n{icon} {name}: {primary}")
            }
        }
        GrokToolCall::Mcp {
            tool_name,
            tool_args_json,
        } => {
            let preview = if tool_args_json.chars().count() > MAX_ARG_PREVIEW {
                let truncated: String = tool_args_json.chars().take(MAX_ARG_PREVIEW).collect();
                format!("{truncated}…")
            } else {
                tool_args_json.clone()
            };
            format!("\n🔌 {tool_name}: {preview}")
        }
    }
}

/// 内置工具名 → emoji 图标(对照 chenyme `xai_chat.py::_TOOL_FMT`)。
fn builtin_tool_icon(name: &str) -> &'static str {
    match name {
        "web_search" | "x_search" | "x_keyword_search" | "x_semantic_search" => "🔍",
        "browse_page" => "🌐",
        "search_images" | "image_search" => "🖼️",
        "chatroom_send" => "📋",
        "code_execution" => "💻",
        _ => "🔧",
    }
}

/// 抽内置工具的"主参数"字符串展示(简短,reasoning UI 用)。
fn primary_arg_for_builtin(name: &str, args: &serde_json::Value) -> String {
    let key = match name {
        "web_search" | "x_search" | "x_keyword_search" | "x_semantic_search" => "query",
        "browse_page" => "url",
        "search_images" | "image_search" => "image_description",
        _ => return String::new(),
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| format!("\"{s}\""))
        .unwrap_or_default()
}

/// 把 `webSearchResults.results` 数组转 markdown bullet list 拼到 reasoning。
///
/// 实测帧形态(R1 抓包):`{"results":[{"url":"...","title":"...","preview":"..."}]}`
/// 最多列前 5 条(避免 reasoning summary 爆长)。
fn format_web_search_results_for_reasoning(wsr: &serde_json::Value) -> String {
    let Some(results) = wsr.get("results").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if results.is_empty() {
        return String::new();
    }
    const MAX_RESULTS: usize = 5;
    let mut s = String::new();
    for r in results.iter().take(MAX_RESULTS) {
        let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let title = r.get("title").and_then(|v| v.as_str()).unwrap_or(url);
        if url.is_empty() {
            continue;
        }
        s.push_str(&format!("\n  · [{title}]({url})"));
    }
    if results.len() > MAX_RESULTS {
        s.push_str(&format!("\n  · …({} more)", results.len() - MAX_RESULTS));
    }
    s
}

/// 把 `xSearchResults.results`(X/Twitter 帖子)转 markdown bullet list。
///
/// 实测帧形态:`{"results":[{"postId":"...","username":"...","text":"..."}]}`
fn format_x_search_results_for_reasoning(xsr: &serde_json::Value) -> String {
    let Some(results) = xsr.get("results").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if results.is_empty() {
        return String::new();
    }
    const MAX_RESULTS: usize = 5;
    let mut s = String::new();
    for r in results.iter().take(MAX_RESULTS) {
        let username = r.get("username").and_then(|v| v.as_str()).unwrap_or("");
        let text = r.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let post_id = r.get("postId").and_then(|v| v.as_str()).unwrap_or("");
        if username.is_empty() || post_id.is_empty() {
            continue;
        }
        let preview: String = text.chars().take(60).collect();
        s.push_str(&format!(
            "\n  · 𝕏 @{username}: {preview}{} (https://x.com/{username}/status/{post_id})",
            if text.chars().count() > 60 { "…" } else { "" }
        ));
    }
    if results.len() > MAX_RESULTS {
        s.push_str(&format!("\n  · …({} more)", results.len() - MAX_RESULTS));
    }
    s
}

/// 把 `codeExecutionResult.{stdout, stderr, exitCode}` 转 markdown fenced
/// code block 拼到 reasoning。R1 PR-5(stacked 在 PR-4 cleanup 之上)。
///
/// 实测帧形态在 R1.js 抓包里出现的 grok 内置 `code_execution` 工具
/// (chenyme `_TOOL_FMT`:`"code_execution" → ("💻", ())`)。实际字段名可能是
/// `stdout` / `stderr` / `output` / `exit_code` / `exitCode` 中之一,本函数
/// **尽量兼容**(任何一个有内容就用 fenced block 展示);全空则返回空 String。
///
/// 输出形态:
/// ```text
/// \n💻 code_execution stdout:
/// ```
/// <stdout content>
/// ```
/// (and similar for stderr)
/// ```
fn format_code_execution_result_for_reasoning(cer: &serde_json::Value) -> String {
    const MAX_OUTPUT_BYTES: usize = 4096;
    let mut s = String::new();
    let stdout = cer
        .get("stdout")
        .or_else(|| cer.get("output"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let stderr = cer
        .get("stderr")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let exit_code = cer
        .get("exitCode")
        .or_else(|| cer.get("exit_code"))
        .and_then(|v| v.as_i64());

    if let Some(out) = stdout {
        let truncated = truncate_for_reasoning(out, MAX_OUTPUT_BYTES);
        s.push_str("\n💻 code_execution stdout:\n```\n");
        s.push_str(&truncated);
        s.push_str("\n```");
    }
    if let Some(err) = stderr {
        let truncated = truncate_for_reasoning(err, MAX_OUTPUT_BYTES);
        s.push_str("\n💻 code_execution stderr:\n```\n");
        s.push_str(&truncated);
        s.push_str("\n```");
    }
    if let Some(code) = exit_code {
        if code != 0 {
            s.push_str(&format!("\n💻 code_execution exit code: {code}"));
        }
    }
    s
}

/// 截断长输出防 reasoning summary 爆长。按字节截断 + UTF-8 边界对齐。
fn truncate_for_reasoning(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // 找最近的 UTF-8 字符边界(不超过 max_bytes)
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    format!("{}…(truncated, full {} bytes)", &s[..idx], s.len())
}

/// R1 PR-6 通用 search results 帧 → markdown bullet list。
///
/// 适用 grok.com connectorSearchResults / collectionSearchResults / ragResults
/// 帧 — 实测期间用户没启用对应 connector 致这些字段均为空,schema 不确定。
/// 保守复用 webSearchResults 形态:`{results: [{url, title, preview?}]}`。
/// 不匹配的 result 项 silently skip(只接受含 url 的 entry)。
fn format_generic_search_results_for_reasoning(
    grouping: &serde_json::Value,
    source_key: &str,
) -> String {
    let Some(results) = grouping.get("results").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if results.is_empty() {
        return String::new();
    }
    const MAX_RESULTS: usize = 5;
    let mut s = format!("\n🔎 {source_key}:");
    let mut emitted = 0_usize;
    for r in results.iter() {
        let Some(url) = r.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let title = r.get("title").and_then(|v| v.as_str()).unwrap_or(url);
        s.push_str(&format!("\n  · [{title}]({url})"));
        emitted += 1;
        if emitted >= MAX_RESULTS {
            break;
        }
    }
    if emitted == 0 {
        // 全 skip(schema 不符)→ 不污染 reasoning summary
        return String::new();
    }
    if results.len() > emitted {
        s.push_str(&format!("\n  · …({} more)", results.len() - emitted));
    }
    s
}

/// 把通用 search 帧的 url citation 累积到 pending,等 message open 后 flush。
/// 同样保守:只取 url + title 字段。
fn accumulate_generic_search_url_citations(state: &mut ConvState, grouping: &serde_json::Value) {
    let Some(results) = grouping.get("results").and_then(|v| v.as_array()) else {
        return;
    };
    for r in results {
        let Some(url) = r.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(url)
            .to_owned();
        state.pending_url_citations.push(serde_json::json!({
            "type": "url_citation",
            "url": url,
            "title": title,
            "start_index": 0,
            "end_index": 0,
        }));
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

// ─────────────────────────────────────────────────────────────────────────────
// reasoning lifecycle(R1 PR-1 P1 fix — 对齐 gemini_native + responses/converter)
// ─────────────────────────────────────────────────────────────────────────────

/// 首次遇到 thinking token 时打开 reasoning item lifecycle。
///
/// emit 两个 event 到 pending:
/// - `response.output_item.added`(item.type=reasoning, status=in_progress)
/// - `response.reasoning_summary_part.added`(summary_index=0, empty text)
fn open_reasoning_if_needed(state: &mut ConvState) {
    if state.open_reasoning.is_some() {
        return;
    }
    let item_id = format!(
        "rs_{}",
        crate::grok_web::auth::generate_uuid_v4().replace('-', "")
    );
    let output_index = state.next_output_index;
    state.next_output_index += 1;
    state.pending.push_back(emit_event(
        "response.output_item.added",
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "type": "reasoning",
                "status": "in_progress",
                "id": item_id,
                "summary": [],
                "content": null,
                "encrypted_content": null,
            },
        }),
    ));
    state.pending.push_back(emit_event(
        "response.reasoning_summary_part.added",
        serde_json::json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": item_id,
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" },
        }),
    ));
    state.open_reasoning = Some(OpenReasoning {
        item_id,
        output_index,
        text_acc: String::new(),
    });
}

/// thinking token 增量 emit。调用前必须先 [`open_reasoning_if_needed`]。
fn emit_reasoning_summary_text_delta(item_id: &str, output_index: u32, delta: &str) -> Bytes {
    emit_event(
        "response.reasoning_summary_text.delta",
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "summary_index": 0,
            "delta": delta,
        }),
    )
}

/// final token / soft_stop / 上游中断前 close open reasoning。
///
/// emit 三个 event 到 pending(无 open 则 no-op):
/// - `response.reasoning_summary_text.done`(累计 text)
/// - `response.reasoning_summary_part.done`
/// - `response.output_item.done`(item.type=reasoning, status=completed)
///
/// 若不调用,Codex APP 永远等 reasoning item 闭合,UI 卡 "Thinking..."。
fn close_reasoning_if_open(state: &mut ConvState) {
    let Some(rs) = state.open_reasoning.take() else {
        return;
    };
    state.pending.push_back(emit_event(
        "response.reasoning_summary_text.done",
        serde_json::json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": rs.item_id,
            "output_index": rs.output_index,
            "summary_index": 0,
            "text": rs.text_acc,
        }),
    ));
    state.pending.push_back(emit_event(
        "response.reasoning_summary_part.done",
        serde_json::json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": rs.item_id,
            "output_index": rs.output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": rs.text_acc },
        }),
    ));
    state.pending.push_back(emit_event(
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": rs.output_index,
            "item": {
                "type": "reasoning",
                "status": "completed",
                "id": rs.item_id,
                "summary": [{ "type": "summary_text", "text": rs.text_acc }],
                "content": null,
                "encrypted_content": null,
            },
        }),
    ));
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

// ─────────────────────────────────────────────────────────────────────────────
// message item lifecycle(R1 PR-3 — 对齐 OpenAI Responses message item spec)
// ─────────────────────────────────────────────────────────────────────────────

/// 首次遇到 final token 时打开 message item lifecycle。
///
/// emit 两个 event 到 pending:
/// - `response.output_item.added`(item.type=message, status=in_progress, role=assistant)
/// - `response.content_part.added`(part.type=output_text, content_index=0)
fn open_message_if_needed(state: &mut ConvState) {
    if state.open_message.is_some() {
        return;
    }
    let item_id = format!(
        "msg_{}",
        crate::grok_web::auth::generate_uuid_v4().replace('-', "")
    );
    let output_index = state.next_output_index;
    state.next_output_index += 1;
    state.pending.push_back(emit_event(
        "response.output_item.added",
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "type": "message",
                "id": item_id,
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            },
        }),
    ));
    state.pending.push_back(emit_event(
        "response.content_part.added",
        serde_json::json!({
            "type": "response.content_part.added",
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] },
        }),
    ));
    state.open_message = Some(OpenMessage {
        item_id,
        output_index,
        text_acc: String::new(),
        annotations_acc: Vec::new(),
    });
}

/// 关联到 message item_id / output_index 的 output_text.delta。
fn emit_output_text_delta_for_item(item_id: &str, output_index: u32, delta: &str) -> Bytes {
    emit_event(
        "response.output_text.delta",
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "delta": delta,
        }),
    )
}

/// final token / soft_stop / 上游中断前 close open message。
///
/// emit 三个 event:
/// - `response.output_text.done`(累计 text + 全 annotations)
/// - `response.content_part.done`(part.text=累计,annotations 数组完整)
/// - `response.output_item.done`(item.content=[{type:output_text,text,annotations}],
///   status=completed)
fn close_message_if_open(state: &mut ConvState) {
    let Some(msg) = state.open_message.take() else {
        return;
    };
    let annotations = serde_json::Value::Array(msg.annotations_acc.clone());
    state.pending.push_back(emit_event(
        "response.output_text.done",
        serde_json::json!({
            "type": "response.output_text.done",
            "item_id": msg.item_id,
            "output_index": msg.output_index,
            "content_index": 0,
            "text": msg.text_acc,
            "annotations": annotations,
        }),
    ));
    state.pending.push_back(emit_event(
        "response.content_part.done",
        serde_json::json!({
            "type": "response.content_part.done",
            "item_id": msg.item_id,
            "output_index": msg.output_index,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": msg.text_acc,
                "annotations": serde_json::Value::Array(msg.annotations_acc.clone()),
            },
        }),
    ));
    state.pending.push_back(emit_event(
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": msg.output_index,
            "item": {
                "type": "message",
                "id": msg.item_id,
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": msg.text_acc,
                    "annotations": serde_json::Value::Array(msg.annotations_acc),
                }],
            },
        }),
    ));
}

/// 把 pending_url_citations 全部 emit 为 `response.output_text.annotation.added` 事件。
///
/// 只在 message 已 open 时调用(由 final-token 分支触发)。
/// emit 后 pending 清空(annotations_acc 累积到 message item state,供 done 回灌)。
fn flush_pending_url_citations(state: &mut ConvState) {
    if state.pending_url_citations.is_empty() {
        return;
    }
    let Some(msg) = state.open_message.as_mut() else {
        return;
    };
    let item_id = msg.item_id.clone();
    let output_index = msg.output_index;
    let citations = std::mem::take(&mut state.pending_url_citations);
    for (annotation_index, citation) in citations.into_iter().enumerate() {
        state.pending.push_back(emit_event(
            "response.output_text.annotation.added",
            serde_json::json!({
                "type": "response.output_text.annotation.added",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "annotation_index": annotation_index + msg.annotations_acc.len(),
                "annotation": citation,
            }),
        ));
        msg.annotations_acc.push(citation);
    }
}

/// 把 grok webSearchResults.results 转成 OpenAI Responses url_citation 标准 annotation,
/// 累积到 pending_url_citations 等 message open 后 flush。
///
/// OpenAI url_citation schema:
/// ```json
/// {"type":"url_citation","url":"...","title":"...","start_index":0,"end_index":0}
/// ```
/// start/end index 对应 message text 内位置 — R3 阶段尚不支持精确定位,统一设 0(标记
/// 整条 message 与该 url 相关)。R1 后续 PR 加 inline `[N]` 定位再 update。
fn accumulate_web_search_url_citations(state: &mut ConvState, wsr: &serde_json::Value) {
    let Some(results) = wsr.get("results").and_then(|v| v.as_array()) else {
        return;
    };
    for r in results {
        let Some(url) = r.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(url)
            .to_owned();
        state.pending_url_citations.push(serde_json::json!({
            "type": "url_citation",
            "url": url,
            "title": title,
            "start_index": 0,
            "end_index": 0,
        }));
    }
}

/// X 帖子转 url_citation(URL = https://x.com/<username>/status/<postId>)。
fn accumulate_x_search_url_citations(state: &mut ConvState, xsr: &serde_json::Value) {
    let Some(results) = xsr.get("results").and_then(|v| v.as_array()) else {
        return;
    };
    for r in results {
        let username = r.get("username").and_then(|v| v.as_str());
        let post_id = r.get("postId").and_then(|v| v.as_str());
        let (Some(u), Some(p)) = (username, post_id) else {
            continue;
        };
        if u.is_empty() || p.is_empty() {
            continue;
        }
        let url = format!("https://x.com/{u}/status/{p}");
        let text_preview: String = r
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let title = if text_preview.is_empty() {
            format!("𝕏 @{u}")
        } else {
            format!("𝕏 @{u}: {text_preview}")
        };
        state.pending_url_citations.push(serde_json::json!({
            "type": "url_citation",
            "url": url,
            "title": title,
            "start_index": 0,
            "end_index": 0,
        }));
    }
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
    async fn thinking_frames_emit_reasoning_summary_lifecycle() {
        // R1 PR-1 P1 fix(chatgpt-codex-connector PR #130):
        // thinking 帧 emit OpenAI Responses reasoning_summary 事件族
        // (output_item.added → reasoning_summary_part.added →
        //  reasoning_summary_text.delta * N →
        //  reasoning_summary_text.done + reasoning_summary_part.done +
        //  output_item.done),对齐 gemini_native + responses/converter pattern。
        let lines = vec![
            r#"{"result":{"response":{"token":"thinking about request","messageTag":"header","isThinking":true}}}"#,
            r#"{"result":{"response":{"token":"- inspecting tools","messageTag":"summary","isThinking":true}}}"#,
            r#"{"result":{"response":{"token":"answer","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        // 1. output_item.added (type=reasoning, in_progress)
        assert!(out.contains("event: response.output_item.added"));
        assert!(out.contains(r#""type":"reasoning""#));
        assert!(out.contains(r#""status":"in_progress""#));
        // 2. reasoning_summary_part.added
        assert!(out.contains("event: response.reasoning_summary_part.added"));
        // 3. reasoning_summary_text.delta * 2(header + summary)
        let delta_count = out
            .matches("event: response.reasoning_summary_text.delta")
            .count();
        assert_eq!(delta_count, 2, "应有两个 text.delta(header + summary)");
        assert!(out.contains(r#""delta":"thinking about request""#));
        assert!(out.contains(r#""delta":"- inspecting tools""#));
        // 4. close 三段(final 之前触发):text.done + part.done + output_item.done
        assert!(out.contains("event: response.reasoning_summary_text.done"));
        assert!(out.contains("event: response.reasoning_summary_part.done"));
        let item_done_count = out.matches("event: response.output_item.done").count();
        assert!(
            item_done_count >= 1,
            "应至少 emit 一次 reasoning output_item.done"
        );
        // 5. final 仍走 output_text.delta
        assert!(out.contains("event: response.output_text.delta"));
        assert!(out.contains(r#""delta":"answer""#));
        // 关键:reasoning token **不能**出现在 output_text 流里
        assert!(
            !out.contains(r#""type":"response.output_text.delta","delta":"thinking"#),
            "thinking token 不应出现在 output_text.delta 事件"
        );
        // 不再 emit 自创的 response.reasoning.delta(P1 fix)
        assert!(
            !out.contains("event: response.reasoning.delta"),
            "不应 emit 自创的 response.reasoning.delta(项目用 reasoning_summary 事件族)"
        );
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
            global_tracker().get_str(&codex_id).as_deref(),
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
    async fn tool_usage_card_builtin_web_search_appends_to_reasoning() {
        // R1 PR-2:tool_usage_card 帧 → reasoning summary markdown line
        // 内置 web_search 工具调用应转成 `🔍 web_search: query="..."` 拼到 thinking 流
        let lines = vec![
            r#"{"result":{"response":{"token":"thinking","messageTag":"header","isThinking":true}}}"#,
            r#"{"result":{"response":{"messageTag":"tool_usage_card","isThinking":true,"toolUsageCardId":"c1","toolUsageCard":{"toolUsageCardId":"c1","webSearch":{"args":{"query":"Rust async"}}}}}}"#,
            r#"{"result":{"response":{"token":"answer","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        // tool_usage_card 帧应作为 reasoning summary text.delta emit
        assert!(
            out.contains("🔍 web_search"),
            "应有 web_search 图标 + 名字: {out}"
        );
        assert!(out.contains(r#"Rust async"#), "应含 query 参数: {out}");
        // 这些应出现在 reasoning_summary_text.delta 事件中,不污染 output_text
        assert!(
            !out.contains(r#""type":"response.output_text.delta","delta":"\n🔍"#),
            "tool_usage_card 不应进 output_text.delta"
        );
    }

    #[tokio::test]
    async fn tool_usage_card_mcp_appends_to_reasoning() {
        // R1 PR-2:MCP `call_connected_tool` wrapper 帧
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"tool_usage_card","isThinking":true,"toolUsageCardId":"c1","toolUsageCard":{"toolUsageCardId":"c1","mcp":{"toolName":"test___ask_question","toolArgsJson":"{\"repoName\":\"foo/bar\"}"}}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(
            out.contains("🔌 test___ask_question"),
            "MCP 应有插头图标 + 三下划线 namespace"
        );
        assert!(out.contains(r#"foo/bar"#), "应含 args JSON 预览");
    }

    #[tokio::test]
    async fn raw_function_result_web_search_appends_bullet_list() {
        // R1 PR-2:raw_function_result webSearchResults → markdown bullet list 拼 reasoning
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"tool_usage_card","isThinking":true,"toolUsageCardId":"c1","toolUsageCard":{"toolUsageCardId":"c1","webSearch":{"args":{"query":"MCP"}}}}}"#,
            r#"{"result":{"response":{"messageTag":"raw_function_result","webSearchResults":{"results":[{"url":"https://modelcontextprotocol.io","title":"MCP Home","preview":"open standard"},{"url":"https://anthropic.com/news/model-context-protocol","title":"MCP Intro"}]}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("[MCP Home](https://modelcontextprotocol.io)"));
        assert!(out.contains("[MCP Intro](https://anthropic.com/news/model-context-protocol)"));
    }

    #[tokio::test]
    async fn code_execution_result_appends_fenced_code_block_to_reasoning() {
        // R1 PR-5:codeExecutionResult.stdout 转 markdown fenced code block
        // 拼到 reasoning summary。grok 内置 code_execution 工具产物。
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"tool_usage_card","isThinking":true,"toolUsageCardId":"c1","toolUsageCard":{"toolUsageCardId":"c1","codeExecution":{"args":{}}}}}}"#,
            r#"{"result":{"response":{"messageTag":"raw_function_result","codeExecutionResult":{"stdout":"hello\nworld","exitCode":0}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("💻 code_execution stdout"));
        assert!(
            out.contains("hello\\nworld"),
            "应含 stdout 内容(JSON 字符串形态): {out}"
        );
    }

    #[tokio::test]
    async fn code_execution_result_nonzero_exit_shows_exit_code() {
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"raw_function_result","codeExecutionResult":{"stdout":"","stderr":"Traceback...","exitCode":1}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("💻 code_execution stderr"));
        assert!(out.contains("Traceback"));
        assert!(out.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn connector_search_results_appends_to_reasoning_and_emits_citation() {
        // R1 PR-6:connectorSearchResults 帧(Notion/Linear/MCP connector
        // emit)→ 同 webSearchResults 处理:reasoning summary bullet +
        // url_citation annotation。
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"raw_function_result","connectorSearchResults":{"results":[{"url":"https://notion.so/abc","title":"Notes Page"},{"url":"https://notion.so/xyz","title":"Tasks DB"}]}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        // reasoning bullet
        assert!(out.contains("🔎 connectorSearchResults"));
        assert!(out.contains("[Notes Page](https://notion.so/abc)"));
        // url_citation annotation
        assert!(out.contains("event: response.output_text.annotation.added"));
        assert!(out.contains(r#""url":"https://notion.so/abc""#));
        assert!(out.contains(r#""url":"https://notion.so/xyz""#));
    }

    #[tokio::test]
    async fn rag_results_empty_no_pollution() {
        // 防御:empty results 不污染 reasoning。
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"raw_function_result","ragResults":{"results":[]}}}}"#,
            r#"{"result":{"response":{"token":"done","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(!out.contains("🔎 ragResults"));
    }

    #[tokio::test]
    async fn web_search_emits_url_citation_annotations_on_message() {
        // R1 PR-3:webSearchResults 累积 → 首个 final token open_message →
        // flush pending_url_citations 为 annotation.added 事件 → close 时回灌
        // 完整 annotations 数组到 output_text.done / content_part.done / output_item.done。
        let lines = vec![
            r#"{"result":{"response":{"messageTag":"tool_usage_card","isThinking":true,"toolUsageCardId":"c1","toolUsageCard":{"toolUsageCardId":"c1","webSearch":{"args":{"query":"MCP"}}}}}}"#,
            r#"{"result":{"response":{"messageTag":"raw_function_result","webSearchResults":{"results":[{"url":"https://modelcontextprotocol.io","title":"MCP Home"}]}}}}"#,
            r#"{"result":{"response":{"token":"hello","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"token":" world","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        // 1. message item 已开
        assert!(out.contains("event: response.output_item.added"));
        assert!(out.contains(r#""type":"message""#));
        assert!(out.contains("event: response.content_part.added"));
        // 2. output_text.delta 关联 item_id(含 content_index=0)
        assert!(out.contains("event: response.output_text.delta"));
        assert!(out.contains(r#""content_index":0"#));
        assert!(out.contains(r#""delta":"hello""#));
        assert!(out.contains(r#""delta":" world""#));
        // 3. url_citation annotation.added 事件
        assert!(out.contains("event: response.output_text.annotation.added"));
        assert!(out.contains(r#""type":"url_citation""#));
        assert!(out.contains(r#""url":"https://modelcontextprotocol.io""#));
        assert!(out.contains(r#""title":"MCP Home""#));
        // 4. close 三段
        assert!(out.contains("event: response.output_text.done"));
        assert!(out.contains("event: response.content_part.done"));
        // output_item.done 应至少有两个(reasoning close + message close)
        let item_done_count = out.matches("event: response.output_item.done").count();
        assert!(item_done_count >= 2);
    }

    #[tokio::test]
    async fn final_token_without_web_search_still_emits_message_lifecycle() {
        // PR-3 防御:无 webSearchResults 时,message lifecycle 也要走完整
        // (open_message + delta + close 三段),只是 annotations 数组为空。
        let lines = vec![
            r#"{"result":{"response":{"token":"plain","messageTag":"final","isThinking":false}}}"#,
            r#"{"result":{"response":{"isSoftStop":true}}}"#,
        ];
        let out = collect(convert_grok_sse_to_responses_sse(
            build_byte_stream(lines),
            "r".into(),
        ))
        .await;
        assert!(out.contains("event: response.output_item.added"));
        assert!(out.contains(r#""type":"message""#));
        assert!(out.contains("event: response.content_part.added"));
        assert!(out.contains(r#""delta":"plain""#));
        assert!(out.contains("event: response.output_text.done"));
        assert!(out.contains("event: response.content_part.done"));
        // 无 annotation.added 事件
        assert!(!out.contains("event: response.output_text.annotation.added"));
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
