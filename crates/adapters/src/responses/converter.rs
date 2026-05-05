//! Chat SSE → Responses SSE 状态机.
//!
//! 覆盖范围:
//! - **Stage 3.2c**:文本流(`delta.content`)→ message 生命周期
//! - **Stage 3.3a**:推理流(`delta.reasoning_content`)→ reasoning 生命周期
//! - **Stage 3.3b**(本次):工具调用流(`delta.tool_calls[]`)→ function_call
//!   生命周期。多个 tool_call 用 OpenAI 自带的 `index` 区分;同一 index 的
//!   `function.arguments` 在多 chunk 间累计成一个完整 JSON 字符串。
//!
//! reasoning / message / tool_calls 三类 item 在同一响应里独立维持,按
//! "实际出现顺序"决定它们在最终 `response.completed.output[]` 里的排列。
//!
//! **暂不处理**(留 Stage 3.3c):
//! - `delta.function_call`(legacy 单工具形式;现在所有主流 provider 已
//!   迁到 `tool_calls[]`,触发 legacy 形式的概率极低,命中时静默跳过)
//! - 多 choice(只看 `choices[0]`)
//!
//! 状态机生命周期(单次响应):
//! ```text
//! Idle ──first chunk parse──► Streaming ──[DONE] / EOF──► Done
//!         │
//!         emit response.created (一次)
//!                    │
//!         首次 reasoning_content delta:
//!           reasoning open → reasoning.summary_text.delta*
//!         首次 content delta:
//!           if reasoning 还开着 → reasoning close
//!           message open → output_text.delta*
//!                    │
//!         close 阶段:open 着的 item 依次 close → response.completed
//! ```
//!
//! 设计取舍:
//! - 状态机用同步 `feed(&[u8]) -> Vec<u8>` + `finish() -> Vec<u8>` 暴露,
//!   流式包装放 `mod stream`,这样状态机本身能用单测覆盖完整生命周期
//! - SSE 帧切分按 `\n\n` 终结符;增量 buffer 在 `BytesMut` 里(允许跨 chunk
//!   接续)
//! - JSON 解析允许失败:遇到非 JSON 的 `data:` 行(罕见但不能崩),静默跳过
//!   (Stage 4 接 tracing 后再 warn)

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Streaming,
    Done,
}

#[derive(Debug)]
struct PendingToolCall {
    output_index: u32,
    fc_id: String,
    /// 上游 OpenAI 给的 `tool_calls[i].id`(透传给 client);若上游没给会用
    /// fc_id 兜底。Codex CLI 后续若把工具结果回灌到 Responses,就靠它做
    /// `tool_call_id` 关联。
    call_id: String,
    name: String,
    args_acc: String,
    closed: bool,
}

#[derive(Debug)]
pub struct ChatToResponsesConverter {
    state: State,
    buffer: BytesMut,
    response_id: String,
    next_output_index: u32,

    // ── reasoning(推理流)──
    reasoning_id: String,
    reasoning_open: bool,
    reasoning_closed: bool,
    reasoning_index: u32,
    reasoning_acc: String,

    // ── message(文本流)──
    message_id: String,
    message_open: bool,
    message_closed: bool,
    message_index: u32,
    text_acc: String,

    // ── tool_calls(工具调用流)── BTreeMap 用 OpenAI 自带的 index 做 key,
    // 迭代顺序天然按 OpenAI index 升序;output_index 在首次 open 时分配
    tool_calls: BTreeMap<u32, PendingToolCall>,
    /// fc_id 生成种子:`fc_<seed>_<openai_index>`;一次响应里固定不变
    fc_id_seed: String,

    model: String,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

impl ChatToResponsesConverter {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!("{nanos:x}");
        Self::new_with_ids(
            format!("resp_{seed}"),
            format!("msg_{seed}"),
            format!("rs_{seed}"),
        )
    }

    pub fn new_with_ids(response_id: String, message_id: String, reasoning_id: String) -> Self {
        let fc_id_seed = response_id
            .strip_prefix("resp_")
            .unwrap_or(response_id.as_str())
            .to_owned();
        Self {
            state: State::Idle,
            buffer: BytesMut::with_capacity(4096),
            response_id,
            next_output_index: 0,
            reasoning_id,
            reasoning_open: false,
            reasoning_closed: false,
            reasoning_index: 0,
            reasoning_acc: String::new(),
            message_id,
            message_open: false,
            message_closed: false,
            message_index: 0,
            text_acc: String::new(),
            tool_calls: BTreeMap::new(),
            fc_id_seed,
            model: String::new(),
            finish_reason: None,
            usage: None,
        }
    }

    /// 喂入站 SSE 字节;返回**已经可以 flush** 的出站 SSE 字节。
    /// 半个 frame(没遇到 `\n\n`)会留在内部 buffer 等下次 feed。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if matches!(self.state, State::Done) {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(frame) = drain_one_frame(&mut self.buffer) {
            self.handle_frame(&frame, &mut out);
            if matches!(self.state, State::Done) {
                break;
            }
        }
        out
    }

    /// 上游流结束(EOF)时调用;若 `[DONE]` 之前就断了,会补 emit
    /// `response.completed`(标记 incomplete + interrupted),保证客户端不会
    /// 看到半截流。
    pub fn finish(&mut self) -> Vec<u8> {
        if matches!(self.state, State::Done) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            self.buffer.extend_from_slice(b"\n\n");
            if let Some(frame) = drain_one_frame(&mut self.buffer) {
                self.handle_frame(&frame, &mut out);
            }
        }
        if !matches!(self.state, State::Done) {
            self.emit_close(&mut out, /*from_done=*/ false);
        }
        out
    }

    fn handle_frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let payload = match parse_sse_data_payload(frame) {
            Some(p) => p,
            None => return,
        };
        if payload == "[DONE]" {
            self.emit_close(out, /*from_done=*/ true);
            return;
        }
        let chunk: ChatChunk = match serde_json::from_str(&payload) {
            Ok(c) => c,
            Err(_) => return,
        };

        // 确保 response.created 只 emit 一次,且在任何 item open 之前
        if matches!(self.state, State::Idle) {
            self.state = State::Streaming;
            // model 名优先取本帧;首帧没有再用 unknown 占位
            if let Some(m) = chunk.model.as_deref() {
                self.model = m.to_owned();
            }
            emit_event(
                out,
                "response.created",
                json!({
                    "type": "response.created",
                    "response": {
                        "id": self.response_id,
                        "object": "response",
                        "status": "in_progress",
                        "model": if self.model.is_empty() { "unknown" } else { self.model.as_str() },
                    },
                }),
            );
        } else if self.model.is_empty() {
            if let Some(m) = chunk.model.as_deref() {
                self.model = m.to_owned();
            }
        }

        let choice = match chunk.choices.first() {
            Some(c) => c,
            None => {
                if let Some(u) = chunk.usage {
                    self.usage = Some(u);
                }
                return;
            }
        };

        // reasoning 优先于 content 处理(Kimi/DeepSeek 在同一 chunk 里通常只有一种)
        if let Some(rs) = choice.delta.reasoning_content.as_deref() {
            if !rs.is_empty() {
                if !self.reasoning_open {
                    self.open_reasoning(out);
                }
                self.reasoning_acc.push_str(rs);
                emit_event(
                    out,
                    "response.reasoning_summary_text.delta",
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": self.reasoning_id,
                        "output_index": self.reasoning_index,
                        "summary_index": 0,
                        "delta": rs,
                    }),
                );
            }
        }

        if let Some(text) = choice.delta.content.as_deref() {
            if !text.is_empty() {
                if self.reasoning_open && !self.reasoning_closed {
                    self.close_reasoning(out);
                }
                if !self.message_open {
                    self.open_message(out);
                }
                self.text_acc.push_str(text);
                emit_event(
                    out,
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": self.message_id,
                        "output_index": self.message_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                );
            }
        }

        // tool_calls(可能与 content / reasoning 同帧;此处独立处理)
        for tc in &choice.delta.tool_calls {
            self.handle_tool_call_delta(tc, out);
        }

        if let Some(reason) = choice.finish_reason.as_deref() {
            self.finish_reason = Some(reason.to_owned());
        }
        if let Some(u) = chunk.usage {
            self.usage = Some(u);
        } else if let Some(u) = choice.usage.clone() {
            self.usage = Some(u);
        }
    }

    fn handle_tool_call_delta(&mut self, tc: &ChatToolCallDelta, out: &mut Vec<u8>) {
        let openai_index = tc.index;
        // 第一次见到这个 index → open。OpenAI 在首帧通常给 id + name + ""(空 args);
        // 也有 provider 在中间帧才补 id/name(我们持续合并)。
        let is_new = !self.tool_calls.contains_key(&openai_index);
        if is_new {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let fc_id = format!("fc_{}_{}", self.fc_id_seed, openai_index);
            let call_id = tc
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{}_{}", self.fc_id_seed, openai_index));
            let name = tc.function.name.clone().unwrap_or_default();
            self.tool_calls.insert(
                openai_index,
                PendingToolCall {
                    output_index,
                    fc_id: fc_id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args_acc: String::new(),
                    closed: false,
                },
            );
            emit_event(
                out,
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "id": fc_id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                        "status": "in_progress",
                    },
                }),
            );
        }

        // 后续帧可能补全 name(罕见但兼容)
        if let Some(name) = tc.function.name.as_deref() {
            if !name.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    if pending.name.is_empty() {
                        pending.name = name.to_owned();
                    }
                }
            }
        }
        // call_id 也可能在后续帧才出现
        if let Some(id) = tc.id.as_deref() {
            if !id.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    // 只在首次给出 id 时覆盖(避免相同 index 不同 id 的混乱)
                    if pending.call_id.starts_with("call_") && pending.call_id.contains('_') {
                        // 兜底生成的 call_id 形如 `call_<seed>_<idx>`,真 id 来了就替换
                        if !pending.call_id.starts_with(id) && pending.call_id != id {
                            pending.call_id = id.to_owned();
                        }
                    }
                }
            }
        }

        // arguments delta(增量字符串)
        if let Some(args) = tc.function.arguments.as_deref() {
            if !args.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    pending.args_acc.push_str(args);
                    let item_id = pending.fc_id.clone();
                    let output_index = pending.output_index;
                    emit_event(
                        out,
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": args,
                        }),
                    );
                }
            }
        }
    }

    fn close_tool_call(&mut self, openai_index: u32, out: &mut Vec<u8>) {
        let Some(pending) = self.tool_calls.get_mut(&openai_index) else {
            return;
        };
        if pending.closed {
            return;
        }
        emit_event(
            out,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": pending.fc_id,
                "output_index": pending.output_index,
                "arguments": pending.args_acc,
            }),
        );
        let item = json!({
            "type": "function_call",
            "id": pending.fc_id,
            "call_id": pending.call_id,
            "name": pending.name,
            "arguments": pending.args_acc,
            "status": "completed",
        });
        emit_event(
            out,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": pending.output_index,
                "item": item,
            }),
        );
        pending.closed = true;
    }

    fn tool_call_item_completed(pending: &PendingToolCall) -> Value {
        json!({
            "type": "function_call",
            "id": pending.fc_id,
            "call_id": pending.call_id,
            "name": pending.name,
            "arguments": pending.args_acc,
            "status": "completed",
        })
    }

    fn open_reasoning(&mut self, out: &mut Vec<u8>) {
        self.reasoning_open = true;
        self.reasoning_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": self.reasoning_index,
                "item": {
                    "type": "reasoning",
                    "status": "in_progress",
                    "id": self.reasoning_id,
                    "summary": [],
                    "content": null,
                    "encrypted_content": null,
                },
            }),
        );
        emit_event(
            out,
            "response.reasoning_summary_part.added",
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        );
    }

    fn close_reasoning(&mut self, out: &mut Vec<u8>) {
        emit_event(
            out,
            "response.reasoning_summary_text.done",
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "text": self.reasoning_acc,
            }),
        );
        emit_event(
            out,
            "response.reasoning_summary_part.done",
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": self.reasoning_acc,
                },
            }),
        );
        emit_event(
            out,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": self.reasoning_index,
                "item": self.reasoning_item_completed(),
            }),
        );
        self.reasoning_closed = true;
    }

    fn reasoning_item_completed(&self) -> Value {
        json!({
            "type": "reasoning",
            "status": "completed",
            "id": self.reasoning_id,
            "summary": [{
                "type": "summary_text",
                "text": self.reasoning_acc,
            }],
            "content": null,
            "encrypted_content": null,
        })
    }

    fn open_message(&mut self, out: &mut Vec<u8>) {
        self.message_open = true;
        self.message_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": self.message_index,
                "item": {
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "id": self.message_id,
                    "content": [],
                },
            }),
        );
        emit_event(
            out,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": self.message_id,
                "output_index": self.message_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
            }),
        );
    }

    fn close_message(&mut self, out: &mut Vec<u8>) {
        emit_event(
            out,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": self.message_id,
                "output_index": self.message_index,
                "content_index": 0,
                "text": self.text_acc,
            }),
        );
        emit_event(
            out,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": self.message_id,
                "output_index": self.message_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": self.text_acc,
                    "annotations": [],
                },
            }),
        );
        emit_event(
            out,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": self.message_index,
                "item": self.message_item_completed(),
            }),
        );
        self.message_closed = true;
    }

    fn message_item_completed(&self) -> Value {
        json!({
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "id": self.message_id,
            "content": [{
                "type": "output_text",
                "text": self.text_acc,
                "annotations": [],
            }],
        })
    }

    fn emit_close(&mut self, out: &mut Vec<u8>, from_done: bool) {
        // 如果到 [DONE] 还没 emit 过 created(纯 [DONE] 输入 / 全是坏 JSON),
        // 仍要补 emit 一次,保证客户端拿到完整生命周期。
        if matches!(self.state, State::Idle) {
            self.state = State::Streaming;
            emit_event(
                out,
                "response.created",
                json!({
                    "type": "response.created",
                    "response": {
                        "id": self.response_id,
                        "object": "response",
                        "status": "in_progress",
                        "model": if self.model.is_empty() { "unknown" } else { self.model.as_str() },
                    },
                }),
            );
        }
        if self.reasoning_open && !self.reasoning_closed {
            self.close_reasoning(out);
        }
        if self.message_open && !self.message_closed {
            self.close_message(out);
        }
        // tool_calls 按 OpenAI index 顺序闭合(BTreeMap 自然有序)
        let tc_indices: Vec<u32> = self.tool_calls.keys().copied().collect();
        for idx in tc_indices {
            self.close_tool_call(idx, out);
        }

        let (status, incomplete_details) = match (self.finish_reason.as_deref(), from_done) {
            (Some("stop") | Some("tool_calls"), _) => ("completed", Value::Null),
            (Some("length"), _) => ("incomplete", json!({ "reason": "max_output_tokens" })),
            (Some("content_filter"), _) => ("incomplete", json!({ "reason": "content_filter" })),
            (Some(other), _) => ("incomplete", json!({ "reason": other })),
            (None, true) => ("completed", Value::Null),
            (None, false) => ("incomplete", json!({ "reason": "interrupted" })),
        };

        // output[] 严格按 output_index 排序(reasoning/message/tool_calls 全混在一起)
        let mut all_items: Vec<(u32, Value)> = Vec::new();
        if self.reasoning_open {
            all_items.push((self.reasoning_index, self.reasoning_item_completed()));
        }
        if self.message_open {
            all_items.push((self.message_index, self.message_item_completed()));
        }
        for tc in self.tool_calls.values() {
            all_items.push((tc.output_index, Self::tool_call_item_completed(tc)));
        }
        all_items.sort_by_key(|(idx, _)| *idx);
        let output_items: Vec<Value> = all_items.into_iter().map(|(_, v)| v).collect();

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "status": status,
                "model": if self.model.is_empty() { "unknown" } else { self.model.as_str() },
                "output": output_items,
                "incomplete_details": incomplete_details,
            },
        });
        if let Some(usage) = self.usage.clone() {
            completed["response"]["usage"] = usage;
        }
        emit_event(out, "response.completed", completed);
        self.state = State::Done;
    }
}

impl Default for ChatToResponsesConverter {
    fn default() -> Self {
        Self::new()
    }
}

fn emit_event(out: &mut Vec<u8>, event_name: &str, payload: Value) {
    let line = format!(
        "event: {event_name}\ndata: {}\n\n",
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
    );
    out.extend_from_slice(line.as_bytes());
}

fn drain_one_frame(buf: &mut BytesMut) -> Option<Bytes> {
    let pos = find_double_newline(buf)?;
    Some(buf.split_to(pos + 2).freeze())
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    buf.windows(2).position(|w| w == b"\n\n")
}

fn parse_sse_data_payload(frame: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(frame).ok()?;
    for line in s.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            return Some(rest.trim().to_owned());
        }
    }
    None
}

// ── 入站 chunk 反序列化结构 ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
    /// 非标准位置的 usage —— Kimi (Moonshot) 在 finish 帧把 usage 塞在
    /// `choices[0].usage`,而 OpenAI 标准是把它放顶层。两个位置都收。
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek / Kimi 等用 `reasoning_content` 表达推理链;OpenAI 标准里
    /// 没有这个字段,我们透传到 Responses 的 reasoning summary。
    #[serde(default)]
    reasoning_content: Option<String>,
    /// OpenAI / DeepSeek / Kimi 工具调用增量;同一 `index` 的多 chunk 累计
    /// 成完整的 `function.arguments` JSON 字符串。
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
    // legacy `function_call`(单工具)在 Stage 3.3c 处理
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    _kind: Option<String>,
    #[serde(default)]
    function: ChatToolCallFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ChatToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> ChatToResponsesConverter {
        ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
    }

    fn parse_emitted(bytes: &[u8]) -> Vec<(String, Value)> {
        let s = std::str::from_utf8(bytes).expect("utf8");
        let mut out = Vec::new();
        for frame in s.split("\n\n") {
            if frame.trim().is_empty() {
                continue;
            }
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.split('\n') {
                if let Some(v) = line.strip_prefix("event: ") {
                    event = v.to_owned();
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = v.to_owned();
                }
            }
            out.push((event, serde_json::from_str(&data).expect("data is JSON")));
        }
        out
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(n, _)| n.as_str()).collect()
    }

    // ── Stage 3.2c 行为回归(content-only)── ─────────────────────────

    #[test]
    fn first_chunk_emits_only_response_created_when_content_is_empty() {
        let mut c = fixed();
        let out = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

"#,
        );
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec!["response.created"],
            "无实际内容时只 emit response.created,message 懒开"
        );
    }

    #[test]
    fn first_content_delta_lazily_opens_message() {
        let mut c = fixed();
        let out = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"content":"Hi"}}]}

"#,
        );
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec![
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ],
            "首个非空 content 应同时懒开 message item"
        );
        assert_eq!(events[3].1["delta"], "Hi");
        assert_eq!(events[1].1["output_index"], 0);
    }

    #[test]
    fn content_only_done_full_lifecycle() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"content":"Hello"}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let completed = &events[3].1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output"][0]["type"], "message");
        assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
    }

    // ── Stage 3.3 新行为(reasoning)──────────────────────────────────

    #[test]
    fn reasoning_only_completed_turn_emits_reasoning_lifecycle_no_message() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

data: {"choices":[{"index":0,"delta":{"reasoning_content":"The"}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let close_names = names(&events);
        assert_eq!(
            close_names,
            vec![
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.completed",
            ],
            "reasoning-only completed turns should not inject synthetic assistant text"
        );
        let completed = &events[3].1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "output contains only the reasoning item");
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["content"], Value::Null);
        assert_eq!(output[0]["encrypted_content"], Value::Null);
        assert_eq!(output[0]["summary"][0]["text"], "The");
        assert_eq!(output[0]["summary"][0]["type"], "summary_text");
    }

    #[test]
    fn reasoning_then_content_emits_two_items_in_order() {
        let mut c = fixed();
        // 第 1 chunk:首帧 + reasoning 开
        let out1 = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}

"#,
        );
        let ev1 = parse_emitted(&out1);
        assert_eq!(
            names(&ev1),
            vec![
                "response.created",
                "response.output_item.added", // reasoning open
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
            ]
        );
        assert_eq!(ev1[1].1["item"]["type"], "reasoning");
        assert_eq!(ev1[1].1["output_index"], 0);
        assert_eq!(ev1[1].1["item"]["summary"], json!([]));
        assert_eq!(ev1[1].1["item"]["content"], Value::Null);
        assert_eq!(ev1[1].1["item"]["encrypted_content"], Value::Null);

        // 第 2 chunk:content 出现,先关 reasoning 再开 message
        let out2 = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"content":"answer"}}]}

"#,
        );
        let ev2 = parse_emitted(&out2);
        assert_eq!(
            names(&ev2),
            vec![
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",  // reasoning close
                "response.output_item.added", // message open
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );
        // reasoning 关闭事件的 output_index = 0
        assert_eq!(ev2[2].1["output_index"], 0);
        assert_eq!(ev2[2].1["item"]["content"], Value::Null);
        assert_eq!(ev2[2].1["item"]["encrypted_content"], Value::Null);
        assert_eq!(ev2[2].1["item"]["summary"][0]["type"], "summary_text");
        // message 打开事件的 output_index = 1
        assert_eq!(ev2[3].1["output_index"], 1);

        // 第 3 chunk:finish + [DONE]
        let _ = c.feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        let out3 = c.feed(b"data: [DONE]\n\n");
        let ev3 = parse_emitted(&out3);
        assert_eq!(
            names(&ev3),
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done", // message close
                "response.completed",
            ]
        );
        let output = ev3[3].1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["content"], Value::Null);
        assert_eq!(output[0]["encrypted_content"], Value::Null);
        assert_eq!(output[0]["summary"][0]["type"], "summary_text");
        assert_eq!(output[0]["summary"][0]["text"], "think");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn reasoning_split_across_multiple_deltas_concatenates() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"par"}}]}

data: {"choices":[{"index":0,"delta":{"reasoning_content":"t1 "}}]}

data: {"choices":[{"index":0,"delta":{"reasoning_content":"part2"}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        assert_eq!(completed["output"][0]["summary"][0]["text"], "part1 part2");
    }

    // ── 边界回归(已有用例迁移)──────────────────────────────────────

    #[test]
    fn after_done_further_feed_is_noop() {
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n");
        let _ = c.feed(b"data: [DONE]\n\n");
        let out = c.feed(b"data: anything\n\n");
        assert!(out.is_empty(), "Done 之后不应再 emit");
    }

    #[test]
    fn frame_split_across_chunks_is_buffered() {
        let mut c = fixed();
        let out1 = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par");
        assert!(out1.is_empty(), "半帧不应 emit");
        let out2 = c.feed(b"t1\"}}]}\n\n");
        let events = parse_emitted(&out2);
        assert_eq!(
            names(&events),
            vec![
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );
        assert_eq!(events[3].1["delta"], "part1");
    }

    #[test]
    fn finish_without_done_emits_incomplete_close() {
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"abc\"}}]}\n\n");
        let out = c.finish();
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(events[3].1["response"]["status"], "incomplete");
        assert_eq!(
            events[3].1["response"]["incomplete_details"]["reason"],
            "interrupted"
        );
    }

    #[test]
    fn usage_in_last_chunk_is_carried_to_completed() {
        let mut c = fixed();
        let all = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}

data: {"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}

data: [DONE]

"#,
        );
        let events = parse_emitted(&all);
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        assert_eq!(completed.1["response"]["usage"]["total_tokens"], 3);
    }

    #[test]
    fn invalid_json_data_line_is_silently_skipped() {
        let mut c = fixed();
        let out = c.feed(b"data: not json at all\n\n");
        assert!(out.is_empty());
    }

    // ── Stage 3.3b 新行为(tool_calls)──────────────────────────────

    #[test]
    fn single_tool_call_full_lifecycle() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"NYC\"}"}}]}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let n = names(&events);
        assert_eq!(
            n,
            vec![
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ],
            "[DONE] 时只 close tool_call(open 在前面已经 emit 过)"
        );
        let done = &events[0];
        assert_eq!(done.1["arguments"], "{\"location\":\"NYC\"}");

        let item_done = &events[1].1["item"];
        assert_eq!(item_done["type"], "function_call");
        assert_eq!(item_done["status"], "completed");
        assert_eq!(item_done["call_id"], "call_abc");
        assert_eq!(item_done["name"], "get_weather");
        assert_eq!(item_done["arguments"], "{\"location\":\"NYC\"}");

        let completed = &events[2].1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output"][0]["type"], "function_call");
        assert_eq!(completed["output"][0]["call_id"], "call_abc");
    }

    #[test]
    fn tool_call_open_emits_added_and_first_args_delta() {
        let mut c = fixed();
        let out = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}

"#,
        );
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec![
                "response.created",
                "response.output_item.added",
                "response.function_call_arguments.delta",
            ],
            "首帧:created + tool_call open + 第一段 args delta"
        );
        assert_eq!(events[1].1["item"]["type"], "function_call");
        assert_eq!(events[1].1["item"]["call_id"], "call_a");
        assert_eq!(events[1].1["item"]["name"], "f");
        assert_eq!(events[2].1["delta"], "{}");
    }

    #[test]
    fn multiple_tool_calls_get_distinct_output_indices() {
        let mut c = fixed();
        // SSE data 必须单行,所以这里手工拼起来(不用 raw string 多行)
        let chunk1 = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_0","type":"function","function":{"name":"a","arguments":"{}"}},{"index":1,"id":"call_1","type":"function","function":{"name":"b","arguments":"{}"}}]}}]}
"#;
        let _ = c.feed(chunk1);
        let _ = c.feed(b"\n");
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "两个 tool_call 应各自占一个 output item");
        assert_eq!(output[0]["call_id"], "call_0");
        assert_eq!(output[0]["name"], "a");
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[1]["name"], "b");
    }

    #[test]
    fn tool_call_call_id_falls_back_when_upstream_omits_id() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{}"}}]}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let call_id = completed.1["response"]["output"][0]["call_id"]
            .as_str()
            .unwrap();
        assert!(
            call_id.starts_with("call_"),
            "call_id 兜底应以 call_ 开头,实际:{call_id}"
        );
    }

    #[test]
    fn tool_call_arguments_concatenate_across_chunks() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"f","arguments":"{\"a"}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":1"}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let done = events
            .iter()
            .find(|(n, _)| n == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(done.1["arguments"], "{\"a\":1}");
    }

    #[test]
    fn message_then_tool_call_keeps_output_index_order() {
        let mut c = fixed();
        // 罕见但合法:有 content 也有 tool_call
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"content":"hi"}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_t","type":"function","function":{"name":"t","arguments":"{}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        // message 先出现,output_index=0
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "hi");
        // tool_call 后出现,output_index=1
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_t");
    }

    // ── 边界回归(已有用例迁移)──────────────────────────────────────

    #[test]
    fn finish_reason_length_maps_to_max_output_tokens() {
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"length\"}]}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        assert_eq!(completed["status"], "incomplete");
        assert_eq!(
            completed["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }
}
