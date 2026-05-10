//! Gemini `streamGenerateContent?alt=sse` SSE → OpenAI Responses API SSE 直转。
//!
//! 设计(2026-05-10 用户决策):**Gemini → Responses 直转**,不走 chat 中间。
//! Codex.app 入站 /responses 期待原生 Responses SSE 事件流;我们直接构造
//! `response.created` / `response.output_item.added` / `response.output_text.delta`
//! / `response.function_call_arguments.delta` / `response.output_text.annotation.added`
//! / `response.completed` 等事件,跟 ChatToResponsesConverter 同形态但消费 Gemini
//! wire 而非 chat wire。
//!
//! 依赖隔离:跟 `crates/adapters/src/responses/` 无依赖(除字段名 + envelope
//! 形态对齐 OpenAI Responses 协议),Gemini 适配器自给自足。
//!
//! Responses SSE 事件序列(Codex.app 期望):
//! ```text
//! response.created                    ← 首事件,含 envelope (status="in_progress")
//! response.in_progress                ← 立即跟在 created 后,同一份 envelope
//! [for each output item:]
//!   response.output_item.added        ← item type=message/reasoning/function_call
//!   [if message:]
//!     response.content_part.added     ← part type=output_text
//!     response.output_text.delta      ← 增量文本(可多次)
//!     response.output_text.annotation.added  ← grounding citation(可多次)
//!     response.output_text.done       ← 文本累积完毕
//!     response.content_part.done
//!   [if reasoning:]
//!     response.reasoning_summary_part.added  ← summary_index=0
//!     response.reasoning_summary_text.delta  ← 增量
//!     response.reasoning_summary_text.done
//!     response.reasoning_summary_part.done
//!   [if function_call:]
//!     response.function_call_arguments.delta  ← 一次性 emit 完整 args(Gemini 不增量)
//!     response.function_call_arguments.done
//!   response.output_item.done
//! response.completed                  ← 末事件,含完整 envelope (status="completed",
//!                                       output[],usage,finish_reason 等)
//! ```
//!
//! Gemini → Responses 字段映射:
//! - `candidates[].content.parts[].text` (thought≠true) → output_text.delta
//! - `candidates[].content.parts[].text` (thought=true) → reasoning_summary_text.delta
//! - `candidates[].content.parts[].functionCall {name, args}` → function_call output_item
//!   (args 序列化成 JSON string 灌进 function_call_arguments.delta)
//! - `candidates[].groundingMetadata` → output_text.annotation.added(在 message 内)
//! - `candidates[].finishReason` → completed envelope 的 incomplete_details(若非 STOP)
//! - `usageMetadata` → completed envelope 的 usage 字段

use std::collections::HashMap;
use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::types::{ByteStream, ResponseSessionPlan};

use super::grounding::convert_grounding_metadata_to_annotations;
use super::types::{map_finish_reason, GenerateContentResponse};

// ═══════════════════════════════════════════════════════════════════════════
// 工具函数
// ═══════════════════════════════════════════════════════════════════════════

/// 24 hex char ID(对齐 OpenAI `call_<24hex>` / `resp_<24hex>` 等形态)。
fn synthesize_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{ts:016x}{n:08x}")
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SSE event 写出。OpenAI Responses 协议:`event: <name>\ndata: <json>\n\n`,
/// payload 内 `sequence_number` 单调递增。
///
/// C2 修复:序列化失败时以前是 `unwrap_or_else(|_| "{}")` 静默回退 — 客户端会
/// 收到 `data: {}` 事件丢失原始信息。改成 tracing::error! 至少在生产里可见,
/// 仍 fallback `{}` 让 SSE event 不卡(下个事件可能 OK)。
fn emit_event(out: &mut Vec<u8>, seq: &mut u64, event_name: &str, mut payload: Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("sequence_number".into(), json!(*seq));
    }
    *seq += 1;
    let serialized = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                error = %e,
                event = event_name,
                "BUG: failed to serialize Responses SSE event payload; falling back to empty object"
            );
            "{}".into()
        }
    };
    let line = format!("event: {event_name}\ndata: {serialized}\n\n");
    out.extend_from_slice(line.as_bytes());
}

/// 找 SSE event 边界。SSE spec 允许 `\n\n` 或 `\r\n\r\n` 分隔,Google `alt=sse`
/// 走 gRPC-gateway 经常 emit CRLF 行尾,如果只识 LF 会让整个流被 buffer 到结束
/// 才一次性 process(streaming → batch 退化)。返 (边界 byte index, 边界长度)。
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = if buf.len() >= 4 {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    } else {
        None
    };
    match (lf, crlf) {
        (Some(l), Some(c)) if l <= c => Some((l, 2)),
        (Some(_), Some(c)) => Some((c, 4)),
        (Some(l), None) => Some((l, 2)),
        (None, Some(c)) => Some((c, 4)),
        (None, None) => None,
    }
}

/// 上游 stream 异常终止时 synthetic finish reason — emit_completed 看到这个值
/// 会写 status="incomplete" + reason="interrupted",防止 silent truncation
/// (网络断流 / 上游 5xx 中断 / accumulated_json overflow 等)被客户端误读为
/// "completed"。**不能**跟真 Gemini finishReason 同名(INTERRUPTED 不在
/// `_GEMINI_FINISH_REASON_KEYS`)。
const FINISH_INTERRUPTED: &str = "_INTERRUPTED";

/// accumulated_json 防御上限:Gemini upstream 偶尔会发分片 JSON,但持续累积
/// 不收敛 = 上游异常 / 中间代理在乱发数据。256 KiB 足够覆盖 Gemini 单 chunk
/// 含完整 grounding metadata + 长 reasoning 的合理上限。
const MAX_ACCUMULATED_JSON_BYTES: usize = 256 * 1024;

// ═══════════════════════════════════════════════════════════════════════════
// 出 output items 的内部状态
// ═══════════════════════════════════════════════════════════════════════════

struct OpenMessage {
    item_id: String,
    output_index: u32,
    /// 累积的全文(close 时灌进 output_text.done.text + content[0].text)
    text_acc: String,
    /// 累积的 url citations(close 时灌进 message.content[0].annotations)
    annotations_acc: Vec<Value>,
}

struct OpenReasoning {
    item_id: String,
    output_index: u32,
    /// 累积的 reasoning summary 文本(close 时灌进 summary[0].text)
    text_acc: String,
}

struct ClosedFunctionCall {
    item_id: String,
    output_index: u32,
    call_id: String,
    name: String,
    arguments_json_str: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// 主转换器
// ═══════════════════════════════════════════════════════════════════════════

pub struct GeminiToResponsesConverter {
    // ─ SSE byte 解析 buffer ─
    buffer: Vec<u8>,
    accumulated_json: String,

    // ─ envelope 字段(回灌入站 Responses request 的 tools/instructions/temperature/...)─
    response_id: String,
    model: String,
    sequence_number: u64,
    created_at: u64,
    original_request: Option<Value>,
    /// Session resume:Codex.app `previous_response_id` cache 命中时,
    /// 这里持有上一轮的响应 metadata。MVP 暂不主动读取(只透传),follow-up
    /// 可以让 envelope.id 复用 session.response_id 提升 Codex.app session
    /// 链路稳定性。
    #[allow(dead_code)]
    response_session: Option<ResponseSessionPlan>,

    // ─ 生命周期 ─
    lifecycle_opened: bool,
    completed_emitted: bool,

    // ─ output items 状态 ─
    next_output_index: u32,
    open_message: Option<OpenMessage>,
    open_reasoning: Option<OpenReasoning>,
    /// 已 close 的 function_call(完整 envelope output[] 用)
    closed_function_calls: Vec<ClosedFunctionCall>,
    /// 已 close 的 message items(H3 修复:Gemini 多轮 text→fc→text 序列会让
    /// 同一 stream 产生多个 message,旧实现 Option<Value> 会让后者覆盖前者,
    /// 导致 envelope output[] 跟客户端实际 stream 收到的事件 output_index 不匹配)。
    /// 用 (output_index, item) 元组,emit_completed 时按 output_index 排序。
    closed_messages: Vec<(u32, Value)>,
    /// 已 close 的 reasoning items(同 H3 设计)
    closed_reasonings: Vec<(u32, Value)>,

    // ─ 终态 ─
    has_seen_tool_calls: bool,
    final_finish_reason: Option<String>,
    final_usage: Option<Value>,
}

impl GeminiToResponsesConverter {
    pub fn new(
        original_request: Option<Value>,
        response_session: Option<ResponseSessionPlan>,
    ) -> Self {
        let response_id = response_session
            .as_ref()
            .map(|s| s.response_id.clone())
            .unwrap_or_else(|| format!("resp_{}", synthesize_id()));
        Self {
            buffer: Vec::new(),
            accumulated_json: String::new(),
            response_id,
            model: String::new(),
            sequence_number: 0,
            created_at: now_unix_secs(),
            original_request,
            response_session,
            lifecycle_opened: false,
            completed_emitted: false,
            next_output_index: 0,
            open_message: None,
            open_reasoning: None,
            closed_function_calls: Vec::new(),
            closed_messages: Vec::new(),
            closed_reasonings: Vec::new(),
            has_seen_tool_calls: false,
            final_finish_reason: None,
            final_usage: None,
        }
    }

    // ───── envelope 构造 ─────

    /// 从 original_request 拿字段或 fallback。Codex.app 客户端按 envelope.tools
    /// 用 `(namespace, function.name)` 复合主键反向路由 namespace 包装的 MCP tool,
    /// 必须回灌完整 tools 数组。
    fn req_field_or(&self, key: &str, fallback: Value) -> Value {
        self.original_request
            .as_ref()
            .and_then(|r| r.get(key))
            .cloned()
            .unwrap_or(fallback)
    }

    fn build_envelope(&self, status: &str) -> Value {
        // H4 修复:Gemini 上游 modelVersion 字段在首 chunk 里经常缺失
        // (有时只在末 chunk 出现),旧实现 envelope.model="unknown" 会让
        // Codex.app session 的 cost 归因 / 历史 model 过滤全错。fallback 到
        // original_request["model"](客户端发出来时的 alias)做诊断价值更高。
        let model_str: String = if !self.model.is_empty() {
            self.model.clone()
        } else {
            self.original_request
                .as_ref()
                .and_then(|r| r.get("model"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| "unknown".into())
        };
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": model_str,
            "tools": self.req_field_or("tools", json!([])),
            "tool_choice": self.req_field_or("tool_choice", json!("auto")),
            "parallel_tool_calls": self.req_field_or("parallel_tool_calls", json!(true)),
            "reasoning": self.req_field_or("reasoning", json!({"effort": null, "summary": null})),
            "text": self.req_field_or("text", json!({"format": {"type": "text"}})),
            "metadata": self.req_field_or("metadata", Value::Null),
            "previous_response_id": self.req_field_or("previous_response_id", Value::Null),
            "instructions": self.req_field_or("instructions", Value::Null),
            "temperature": self.req_field_or("temperature", Value::Null),
            "top_p": self.req_field_or("top_p", Value::Null),
            "max_output_tokens": self.req_field_or("max_output_tokens", Value::Null),
            "truncation": "disabled",
        })
    }

    // ───── 字节 feed ─────

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.completed_emitted {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some((pos, sep_len)) = find_event_boundary(&self.buffer) else {
                break;
            };
            let event_bytes: Vec<u8> = self.buffer.drain(..pos).collect();
            // 跳过分隔符(2 字节 \n\n 或 4 字节 \r\n\r\n)
            self.buffer
                .drain(..sep_len.min(self.buffer.len()))
                .for_each(drop);
            self.process_event(&event_bytes, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if self.completed_emitted {
            return Vec::new();
        }
        let mut out = Vec::new();
        // 残留 buffer 当最后一个 event 强行 process(可能是网络断流没收到 \n\n)
        if !self.buffer.is_empty() {
            let event = std::mem::take(&mut self.buffer);
            self.process_event(&event, &mut out);
        }
        // 关掉所有 open items
        if self.open_message.is_some() {
            self.close_message(&mut out);
        }
        if self.open_reasoning.is_some() {
            self.close_reasoning(&mut out);
        }
        // 若从未收到任何上游数据,补 lifecycle open(防客户端卡)
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(&mut out);
        }
        // 末事件 response.completed
        self.emit_completed(&mut out);
        out
    }

    fn process_event(&mut self, event_bytes: &[u8], output: &mut Vec<u8>) {
        let Ok(event_str) = std::str::from_utf8(event_bytes) else {
            return;
        };
        let mut data_buf = String::new();
        for line in event_str.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let trimmed = rest.trim_start();
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(trimmed);
        }
        if data_buf.is_empty() {
            return;
        }
        if data_buf.trim() == "[DONE]" {
            // Gemini 通常不发 [DONE],收到也无害,等 finish() 处理
            return;
        }
        // 直接解析,失败进 accumulated_json 兜底
        match serde_json::from_str::<GenerateContentResponse>(&data_buf) {
            Ok(gemini) => {
                self.accumulated_json.clear();
                self.process_gemini_chunk(gemini, output);
            }
            Err(_) => {
                if !self.accumulated_json.is_empty() {
                    self.accumulated_json.push('\n');
                }
                self.accumulated_json.push_str(&data_buf);
                // 防御无界增长(C3b):上游持续 emit 解不开的 JSON 时主动 cut 流
                // + 标 INTERRUPTED,emit_completed 会输出 incomplete + interrupted
                if self.accumulated_json.len() > MAX_ACCUMULATED_JSON_BYTES {
                    tracing::error!(
                        size = self.accumulated_json.len(),
                        cap = MAX_ACCUMULATED_JSON_BYTES,
                        "gemini SSE accumulated JSON exceeds safety cap, dropping buffer + marking interrupted"
                    );
                    self.accumulated_json.clear();
                    self.final_finish_reason = Some(FINISH_INTERRUPTED.to_owned());
                    return;
                }
                if let Ok(gemini) =
                    serde_json::from_str::<GenerateContentResponse>(&self.accumulated_json)
                {
                    self.accumulated_json.clear();
                    self.process_gemini_chunk(gemini, output);
                }
            }
        }
    }

    // ───── 处理一个 Gemini chunk ─────

    fn process_gemini_chunk(&mut self, gemini: GenerateContentResponse, out: &mut Vec<u8>) {
        // 首 chunk:补全 model + 触发 lifecycle open
        if let Some(model) = gemini.model_version {
            if !model.is_empty() {
                self.model = model;
            }
        }
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(out);
        }

        // 处理候选(MVP 只关心 candidates[0],n>1 的多 candidate 留 follow-up)
        for candidate in &gemini.candidates {
            if let Some(content) = &candidate.content {
                for part in &content.parts {
                    // text part
                    if let Some(text) = &part.text {
                        if part.thought == Some(true) {
                            // reasoning text:必要时关 message + 开 reasoning
                            if self.open_message.is_some() {
                                self.close_message(out);
                            }
                            if self.open_reasoning.is_none() {
                                self.open_reasoning(out);
                            }
                            self.emit_reasoning_delta(out, text);
                        } else {
                            // 文本 text:必要时关 reasoning + 开 message
                            if self.open_reasoning.is_some() {
                                self.close_reasoning(out);
                            }
                            if self.open_message.is_none() {
                                self.open_message(out);
                            }
                            self.emit_text_delta(out, text);
                        }
                    }
                    // functionCall part
                    if let Some(fc) = &part.function_call {
                        // function_call 是独立 output item,关掉所有 message/reasoning
                        if self.open_message.is_some() {
                            self.close_message(out);
                        }
                        if self.open_reasoning.is_some() {
                            self.close_reasoning(out);
                        }
                        self.emit_function_call(out, &fc.name, &fc.args);
                        self.has_seen_tool_calls = true;
                    }
                }
            }
            // groundingMetadata → annotations(挂到 active message)
            if let Some(gm) = &candidate.grounding_metadata {
                let annotations = convert_grounding_metadata_to_annotations(gm);
                if !annotations.is_empty() {
                    if self.open_message.is_none() {
                        // annotation 必须挂在 message 上;若还没开,先开
                        self.open_message(out);
                    }
                    self.emit_annotations(out, annotations);
                }
            }
            // finishReason 累积到末态(末 chunk emit completed 时用)
            // **粘性保护**(sanity check 报告):INTERRUPTED 是 C3b/C4 cap-trip /
            // upstream-Err 路径标记的"已宣告中断",不能被后续合法 chunk 的
            // finishReason="STOP" 覆盖回 completed —— 那会让"宣告 incomplete 后又
            // 静默 recover"成 silent truncation 的孪生 bug。
            if let Some(fr) = &candidate.finish_reason {
                if self.final_finish_reason.as_deref() != Some(FINISH_INTERRUPTED) {
                    self.final_finish_reason = Some(fr.clone());
                }
            }
        }
        // usageMetadata 累积到末态
        if let Some(um) = gemini.usage_metadata {
            self.final_usage = Some(json!({
                "input_tokens": um.prompt_token_count,
                "output_tokens": um.candidates_token_count,
                "total_tokens": um.total_token_count,
                "input_tokens_details": {
                    "cached_tokens": um.cached_content_token_count.unwrap_or(0),
                },
                "output_tokens_details": {
                    "reasoning_tokens": um.thoughts_token_count.unwrap_or(0),
                },
            }));
        }
    }

    // ───── lifecycle ─────

    fn emit_lifecycle_open(&mut self, out: &mut Vec<u8>) {
        self.lifecycle_opened = true;
        let mut envelope = self.build_envelope("in_progress");
        envelope["output"] = json!([]);
        envelope["usage"] = Value::Null;
        envelope["incomplete_details"] = Value::Null;
        envelope["error"] = Value::Null;
        let created = json!({"type": "response.created", "response": envelope.clone()});
        let in_progress = json!({"type": "response.in_progress", "response": envelope});
        emit_event(out, &mut self.sequence_number, "response.created", created);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.in_progress",
            in_progress,
        );
    }

    fn emit_completed(&mut self, out: &mut Vec<u8>) {
        if self.completed_emitted {
            return;
        }
        // **关键防御**(C4 + C5):None / FINISH_INTERRUPTED 都映射成 "incomplete"。
        // 原因:Gemini 上游断流 / 5xx mid-stream / 网络 reset 都会让 final_finish_reason
        // 维持 None,如果按"completed"上报就是 silent truncation —— 客户端把半截响应
        // 当成完整响应,无法识别。强制成 incomplete 让客户端走错误恢复路径。
        let status = match self.final_finish_reason.as_deref() {
            Some("STOP") => "completed",
            Some("MAX_TOKENS")
            | Some("SAFETY")
            | Some("RECITATION")
            | Some("BLOCKLIST")
            | Some("PROHIBITED_CONTENT")
            | Some("SPII")
            | Some("IMAGE_SAFETY")
            | Some("IMAGE_PROHIBITED_CONTENT") => "incomplete",
            Some(s) if s == FINISH_INTERRUPTED => "incomplete",
            None => "incomplete",
            _ => "completed",
        };
        let mut envelope = self.build_envelope(status);

        // H3 修复:output[] 按 output_index 升序合并所有 items(message / reasoning /
        // function_call),保持跟客户端实际 stream 收到的事件顺序一致。
        // 旧实现假设 reasoning < message < function_calls 顺序固定,但 Gemini 多轮
        // text→fc→text 序列会破这条假设。
        let mut all_items: Vec<(u32, Value)> = Vec::new();
        all_items.extend(self.closed_messages.drain(..));
        all_items.extend(self.closed_reasonings.drain(..));
        for fc in self.closed_function_calls.drain(..) {
            all_items.push((
                fc.output_index,
                json!({
                    "type": "function_call",
                    "id": fc.item_id,
                    "call_id": fc.call_id,
                    "name": fc.name,
                    "arguments": fc.arguments_json_str,
                    "status": "completed",
                }),
            ));
        }
        all_items.sort_by_key(|(idx, _)| *idx);
        let output_items: Vec<Value> = all_items.into_iter().map(|(_, item)| item).collect();
        envelope["output"] = Value::Array(output_items);
        envelope["usage"] = self.final_usage.clone().unwrap_or(Value::Null);
        envelope["incomplete_details"] = if status == "incomplete" {
            json!({"reason": match self.final_finish_reason.as_deref() {
                Some("MAX_TOKENS") => "max_output_tokens",
                Some("SAFETY") | Some("RECITATION") | Some("BLOCKLIST")
                | Some("PROHIBITED_CONTENT") | Some("SPII") | Some("IMAGE_SAFETY")
                | Some("IMAGE_PROHIBITED_CONTENT") => "content_filter",
                Some(s) if s == FINISH_INTERRUPTED => "interrupted",
                None => "interrupted",
                _ => "interrupted",
            }})
        } else {
            Value::Null
        };
        // 上游断流时附 error 字段帮客户端做诊断
        envelope["error"] = if matches!(self.final_finish_reason.as_deref(), Some(s) if s == FINISH_INTERRUPTED)
            || (status == "incomplete" && self.final_finish_reason.is_none())
        {
            json!({
                "code": "upstream_interrupted",
                "message": "Gemini upstream stream ended without finishReason; treating as interrupted.",
            })
        } else {
            Value::Null
        };

        let payload = json!({"type": format!("response.{status}"), "response": envelope});
        let event_name = format!("response.{status}");
        emit_event(out, &mut self.sequence_number, &event_name, payload);
        self.completed_emitted = true;

        // 兼容 finish reason 跟 OpenAI 客户端期望(底层 _check_finish_reason 实证)—
        // 主要由 Codex.app 自己 mapping,我们 envelope 已正确,不再额外 tooling。
        let _ = map_finish_reason;
    }

    // ───── message item ─────

    fn open_message(&mut self, out: &mut Vec<u8>) {
        let item_id = format!("msg_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
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
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
            }),
        );
        self.open_message = Some(OpenMessage {
            item_id,
            output_index,
            text_acc: String::new(),
            annotations_acc: Vec::new(),
        });
    }

    fn emit_text_delta(&mut self, out: &mut Vec<u8>, delta: &str) {
        let Some(msg) = self.open_message.as_mut() else { return };
        msg.text_acc.push_str(delta);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": msg.item_id,
                "output_index": msg.output_index,
                "content_index": 0,
                "delta": delta,
            }),
        );
    }

    fn emit_annotations(&mut self, out: &mut Vec<u8>, annotations: Vec<Value>) {
        let Some(msg) = self.open_message.as_mut() else { return };
        for annotation in annotations {
            // emit + 累积(close 时塞进 message.content[0].annotations 整体)
            let payload = json!({
                "type": "response.output_text.annotation.added",
                "item_id": msg.item_id,
                "output_index": msg.output_index,
                "content_index": 0,
                "annotation_index": msg.annotations_acc.len(),
                "annotation": annotation.clone(),
            });
            msg.annotations_acc.push(annotation);
            emit_event(
                out,
                &mut self.sequence_number,
                "response.output_text.annotation.added",
                payload,
            );
        }
    }

    fn close_message(&mut self, out: &mut Vec<u8>) {
        let Some(msg) = self.open_message.take() else { return };
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": msg.item_id,
                "output_index": msg.output_index,
                "content_index": 0,
                "text": msg.text_acc,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": msg.item_id,
                "output_index": msg.output_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": msg.text_acc,
                    "annotations": msg.annotations_acc,
                },
            }),
        );
        let item = json!({
            "type": "message",
            "id": msg.item_id,
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": msg.text_acc,
                "annotations": msg.annotations_acc,
            }],
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": msg.output_index,
                "item": item.clone(),
            }),
        );
        self.closed_messages.push((msg.output_index, item));
    }

    // ───── reasoning item ─────

    fn open_reasoning(&mut self, out: &mut Vec<u8>) {
        let item_id = format!("rs_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
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
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_part.added",
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        );
        self.open_reasoning = Some(OpenReasoning {
            item_id,
            output_index,
            text_acc: String::new(),
        });
    }

    fn emit_reasoning_delta(&mut self, out: &mut Vec<u8>, delta: &str) {
        let Some(rs) = self.open_reasoning.as_mut() else { return };
        rs.text_acc.push_str(delta);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": rs.item_id,
                "output_index": rs.output_index,
                "summary_index": 0,
                "delta": delta,
            }),
        );
    }

    fn close_reasoning(&mut self, out: &mut Vec<u8>) {
        let Some(rs) = self.open_reasoning.take() else { return };
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_text.done",
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": rs.item_id,
                "output_index": rs.output_index,
                "summary_index": 0,
                "text": rs.text_acc,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_part.done",
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": rs.item_id,
                "output_index": rs.output_index,
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": rs.text_acc,
                },
            }),
        );
        let item = json!({
            "type": "reasoning",
            "status": "completed",
            "id": rs.item_id,
            "summary": [{ "type": "summary_text", "text": rs.text_acc }],
            "content": null,
            "encrypted_content": null,
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": rs.output_index,
                "item": item.clone(),
            }),
        );
        self.closed_reasonings.push((rs.output_index, item));
    }

    // ───── function_call item ─────

    fn emit_function_call(&mut self, out: &mut Vec<u8>, name: &str, args: &Value) {
        let item_id = format!("fc_{}", synthesize_id());
        let call_id = format!("call_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        // OpenAI function_call.arguments 是 JSON 字符串,Gemini 是结构化对象 → 序列化
        // (LOW from sanity check):跟 emit_event 的 C2 fix 一致,失败时至少 log。
        // 实际 serde_json::to_string(&Value) 几乎不可能失败(只有 NaN/Infinity 等
        // 非标准 number 才会 trip),但留 log 能帮 debug。
        let args_json_str = match serde_json::to_string(args) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    name,
                    "BUG: failed to serialize functionCall.args to JSON string; \
                     falling back to '{{}}'. This may produce a tool call with no args."
                );
                "{}".into()
            }
        };

        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                },
            }),
        );
        // Gemini 一次性给完整 args(无增量),emit 单条 delta = 完整 args
        emit_event(
            out,
            &mut self.sequence_number,
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "delta": args_json_str,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id,
                "output_index": output_index,
                "arguments": args_json_str,
            }),
        );
        let item = json!({
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": name,
            "arguments": args_json_str,
            "status": "completed",
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }),
        );
        self.closed_function_calls.push(ClosedFunctionCall {
            item_id,
            output_index,
            call_id,
            name: name.to_owned(),
            arguments_json_str: args_json_str,
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ByteStream wrapper
// ═══════════════════════════════════════════════════════════════════════════

/// 包装 Gemini SSE byte stream → Responses SSE byte stream。
pub fn convert_gemini_to_responses_stream(
    input: ByteStream,
    original_request: Option<Value>,
    response_session: Option<ResponseSessionPlan>,
) -> ByteStream {
    struct State {
        input: ByteStream,
        conv: GeminiToResponsesConverter,
        finished: bool,
    }
    let init = State {
        input,
        conv: GeminiToResponsesConverter::new(original_request, response_session),
        finished: false,
    };
    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(stream::unfold(init, |mut s| async move {
            loop {
                if s.finished {
                    return None;
                }
                match s.input.next().await {
                    Some(Ok(chunk)) => {
                        let out = s.conv.feed(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), s));
                        }
                    }
                    Some(Err(e)) => {
                        // **C4 修复**:上游 mid-stream Err 不能让 finish() 走默认路径
                        // 以"completed"结尾(那会让客户端把半截响应当成完整响应 — silent
                        // truncation)。先标 INTERRUPTED,finish() 看到这个值会发
                        // status=incomplete + reason=interrupted + error=upstream_interrupted。
                        tracing::warn!(error = %e, "gemini upstream stream errored mid-stream");
                        s.conv.final_finish_reason = Some(FINISH_INTERRUPTED.to_owned());
                        let final_out = s.conv.finish();
                        s.finished = true;
                        if !final_out.is_empty() {
                            return Some((Ok(Bytes::from(final_out)), s));
                        }
                        return Some((Err(e), s));
                    }
                    None => {
                        let final_out = s.conv.finish();
                        s.finished = true;
                        if !final_out.is_empty() {
                            return Some((Ok(Bytes::from(final_out)), s));
                        }
                        return None;
                    }
                }
            }
        }));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn drive_to_events(conv: &mut GeminiToResponsesConverter, chunks: &[&[u8]]) -> Vec<String> {
        let mut all: Vec<u8> = Vec::new();
        for c in chunks {
            all.extend_from_slice(&conv.feed(c));
        }
        all.extend_from_slice(&conv.finish());
        let s = String::from_utf8(all).unwrap();
        s.split("\n\n")
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    }

    fn parse_event(event: &str) -> (String, Value) {
        let mut name = String::new();
        let mut data = String::new();
        for line in event.lines() {
            if let Some(n) = line.strip_prefix("event: ") {
                name = n.to_owned();
            }
            if let Some(d) = line.strip_prefix("data: ") {
                data = d.to_owned();
            }
        }
        (name, serde_json::from_str(&data).unwrap_or(Value::Null))
    }

    #[test]
    fn lifecycle_emits_created_in_progress_completed() {
        // 最简流程:文本 chunk + finishReason → created/in_progress/output_item.added/
        // content_part.added/output_text.delta/output_text.done/content_part.done/
        // output_item.done/completed
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        // 必须包含 lifecycle 事件
        assert!(names.contains(&"response.created".into()));
        assert!(names.contains(&"response.in_progress".into()));
        assert!(names.contains(&"response.output_item.added".into()));
        assert!(names.contains(&"response.content_part.added".into()));
        assert!(names.contains(&"response.output_text.delta".into()));
        assert!(names.contains(&"response.output_text.done".into()));
        assert!(names.contains(&"response.content_part.done".into()));
        assert!(names.contains(&"response.output_item.done".into()));
        assert!(names.contains(&"response.completed".into()));
        // sequence_number 单调递增
        for (i, e) in events.iter().enumerate() {
            let (_, v) = parse_event(e);
            assert_eq!(v["sequence_number"], i, "event {i} sequence_number 必须 = i");
        }
    }

    #[test]
    fn text_delta_accumulates_into_completed_envelope_output() {
        let chunk1 = br#"data: {"candidates":[{"content":{"parts":[{"text":"He"}]}}]}

"#;
        let chunk2 = br#"data: {"candidates":[{"content":{"parts":[{"text":"llo"}]}}]}

"#;
        let chunk3 = br#"data: {"candidates":[{"finishReason":"STOP"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk1, chunk2, chunk3]);
        // 找 completed envelope
        let completed = events.iter()
            .map(|e| parse_event(e))
            .find(|(n, _)| n == "response.completed")
            .expect("response.completed 应存在");
        let output = &completed.1["response"]["output"];
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "Hello", "完整文本应在 envelope output 中");
    }

    #[test]
    fn function_call_emits_separate_output_item() {
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"search","args":{"q":"weather"}}}]},"finishReason":"STOP"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        // 必含 function_call_arguments.delta + done
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        assert!(names.contains(&"response.function_call_arguments.delta".into()));
        assert!(names.contains(&"response.function_call_arguments.done".into()));
        // completed envelope 的 output[] 含 function_call item
        let completed = events.iter()
            .map(|e| parse_event(e))
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let output = &completed.1["response"]["output"];
        let fc = output.as_array().unwrap().iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["name"], "search");
        // arguments 必须是 JSON 字符串(OpenAI 兼容)
        let args_str = fc["arguments"].as_str().unwrap();
        let args: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["q"], "weather");
        assert!(fc["call_id"].as_str().unwrap().starts_with("call_"));
    }

    #[test]
    fn reasoning_text_emits_summary_events() {
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"thinking step","thought":true},{"text":"answer"}]},"finishReason":"STOP"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        assert!(names.contains(&"response.reasoning_summary_part.added".into()));
        assert!(names.contains(&"response.reasoning_summary_text.delta".into()));
        assert!(names.contains(&"response.reasoning_summary_text.done".into()));
        // envelope output 既有 reasoning 又有 message
        let completed = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.completed").unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        assert!(output.iter().any(|i| i["type"] == "reasoning"));
        assert!(output.iter().any(|i| i["type"] == "message"));
        // reasoning summary text 应该是 "thinking step"
        let r = output.iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(r["summary"][0]["text"], "thinking step");
    }

    #[test]
    fn grounding_metadata_emits_annotation_added_events() {
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"NYC weather"}]},"groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://w.com","title":"W"}}],"groundingSupports":[{"segment":{"startIndex":0,"endIndex":11},"groundingChunkIndices":[0]}]},"finishReason":"STOP"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        assert!(names.contains(&"response.output_text.annotation.added".into()));
        // envelope output[].content[0].annotations 含 url_citation
        let completed = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.completed").unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        let msg = output.iter().find(|i| i["type"] == "message").unwrap();
        let annos = msg["content"][0]["annotations"].as_array().unwrap();
        assert_eq!(annos[0]["type"], "url_citation");
        assert_eq!(annos[0]["url_citation"]["url"], "https://w.com");
    }

    #[test]
    fn split_chunks_buffered_correctly() {
        let part1 = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi";
        let part2 = b"\"}]}}]}\n\n";
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[part1, part2]);
        // 注:此 chunk 没 finishReason → emit_completed 走 "incomplete" 路径(C5 修复后行为)。
        // 但 message item 的 text 内容仍要正确累积进 envelope output[]。
        let last = events
            .iter()
            .map(|s| parse_event(s.as_str()))
            .find(|(n, _)| n == "response.incomplete" || n == "response.completed")
            .unwrap();
        let output = &last.1["response"]["output"];
        assert_eq!(output[0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn crlf_sse_boundary_recognized() {
        // C3 修复:Google `alt=sse` 经常 emit `\r\n\r\n` 边界,只识 `\n\n` 会让
        // 整个流被 buffer 到结束才一次性 process(streaming → batch 退化)。
        let chunk = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"crlf\"}]}}]}\r\n\r\n";
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        // 必须能 emit 出来(有 output_text.delta + 终态事件)
        assert!(names.contains(&"response.output_text.delta".into()));
        assert!(
            names.contains(&"response.completed".into())
                || names.contains(&"response.incomplete".into()),
            "CRLF 边界必须被识别;实际 events:{names:?}"
        );
    }

    #[test]
    fn upstream_missing_finish_reason_treated_as_interrupted() {
        // C5 修复:Gemini 上游断流没 emit finishReason → final_finish_reason=None,
        // 必须映射成 "incomplete" + reason="interrupted" + error 字段,防 silent truncation。
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"half"}]}}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let last = events
            .iter()
            .map(|s| parse_event(s.as_str()))
            .find(|(n, _)| n == "response.incomplete")
            .expect("missing finishReason 必须 emit response.incomplete,实际未 emit");
        assert_eq!(last.1["response"]["status"], "incomplete");
        assert_eq!(
            last.1["response"]["incomplete_details"]["reason"], "interrupted",
            "missing finishReason → reason=interrupted"
        );
        assert_eq!(
            last.1["response"]["error"]["code"], "upstream_interrupted",
            "必须附 error 字段帮客户端诊断"
        );
    }

    #[test]
    fn accumulated_json_overflow_aborts_with_interrupted() {
        // C3b 修复:malformed JSON 持续累积超 256 KiB → 标 INTERRUPTED + drop buffer。
        // 构造一个永远解不开的 JSON(开括号没闭合)+ 大 payload 触发 cap。
        let bad_chunk: Vec<u8> = {
            let mut v = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"".to_vec();
            v.extend(std::iter::repeat(b'x').take(MAX_ACCUMULATED_JSON_BYTES + 1024));
            v.extend_from_slice(b"\n\n"); // 终结这个 SSE event(JSON 仍未闭合)
            v
        };
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[&bad_chunk]);
        // 必须 emit incomplete(防 OOM 后假装 completed)
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        assert!(
            names.contains(&"response.incomplete".into()),
            "accumulated JSON 超 cap 必须强制 incomplete,实际:{names:?}"
        );
    }

    #[test]
    fn envelope_includes_original_request_fields() {
        // tools / instructions / temperature 必须从 original_request 回灌
        let original = json!({
            "model":"gemini-3.1-pro-preview",
            "instructions":"You are helpful.",
            "tools":[{"type":"function","name":"test_fn","parameters":{"type":"object"}}],
            "temperature": 0.5,
            "tool_choice": "auto"
        });
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(Some(original), None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let created = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.created").unwrap();
        let env = &created.1["response"];
        assert_eq!(env["instructions"], "You are helpful.");
        assert_eq!(env["temperature"], 0.5);
        assert_eq!(env["tool_choice"], "auto");
        let tools = env["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "test_fn", "envelope.tools 必须是 Codex.app 原始 tools");
    }

    #[test]
    fn max_tokens_finish_reason_emits_incomplete_status() {
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"MAX_TOKENS"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let last = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.incomplete").expect("MAX_TOKENS → response.incomplete");
        assert_eq!(last.1["response"]["status"], "incomplete");
        assert_eq!(last.1["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn safety_finish_reason_emits_incomplete_with_content_filter() {
        let chunk = br#"data: {"candidates":[{"finishReason":"SAFETY"}]}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let last = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.incomplete").unwrap();
        assert_eq!(last.1["response"]["incomplete_details"]["reason"], "content_filter");
    }

    #[test]
    fn usage_metadata_appears_in_completed_envelope() {
        let chunk = br#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"totalTokenCount":150,"thoughtsTokenCount":25,"cachedContentTokenCount":80}}

"#;
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[chunk]);
        let completed = events.iter().map(|s| parse_event(s.as_str())).find(|(n,_)| n == "response.completed").unwrap();
        let usage = &completed.1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["output_tokens"], 50);
        assert_eq!(usage["total_tokens"], 150);
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 25);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 80);
    }

    #[test]
    fn no_upstream_data_still_emits_lifecycle_and_terminal() {
        // 极端情况:上游断流没发任何 chunk → 客户端不能卡死。
        // C5 修复后:无任何 chunk 必须 emit incomplete(不能假装 completed)。
        let mut conv = GeminiToResponsesConverter::new(None, None);
        let events = drive_to_events(&mut conv, &[]);
        let names: Vec<String> = events.iter().map(|e| parse_event(e).0).collect();
        assert!(names.contains(&"response.created".into()));
        assert!(
            names.contains(&"response.incomplete".into()),
            "无 upstream data 必须 emit incomplete(防 silent truncation),实际:{names:?}"
        );
    }

    #[test]
    fn stream_wrapper_end_to_end() {
        let upstream_bytes = Bytes::from(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"end-to-end"}]},"finishReason":"STOP"}]}

"#,
        );
        let upstream: ByteStream = Box::pin(stream::iter(vec![Ok(upstream_bytes)]));
        let mut output_stream = convert_gemini_to_responses_stream(upstream, None, None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut all_bytes = Vec::new();
        runtime.block_on(async {
            while let Some(item) = output_stream.next().await {
                all_bytes.extend_from_slice(&item.unwrap());
            }
        });
        let s = String::from_utf8(all_bytes).unwrap();
        assert!(s.contains("event: response.created"));
        assert!(s.contains("event: response.completed"));
        assert!(s.contains("end-to-end"));
    }
}
