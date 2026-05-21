//! Chat SSE → Responses SSE 状态机.
//!
//! 覆盖范围:
//! - **Stage 3.2c**:文本流(`delta.content`)→ message 生命周期
//! - **Stage 3.3a**:推理流(`delta.reasoning_content`)→ reasoning 生命周期
//! - **Stage 3.3b**:工具调用流(`delta.tool_calls[]`)→ function_call
//!   生命周期。多个 tool_call 用 OpenAI 自带的 `index` 区分;同一 index 的
//!   `function.arguments` 在多 chunk 间累计成一个完整 JSON 字符串。
//! - **Stage 3.3c**:legacy 单工具流(`delta.function_call`)→ function_call
//!   生命周期。旧版 Chat 流式适配器只读取 `choices[0]`;这里保留同一策略,
//!   不把多个 choice 合并成一个 Responses 输出,避免发明 1.0.x 没有的语义。
//!
//! reasoning / message / tool_calls 三类 item 在同一响应里独立维持,按
//! "实际出现顺序"决定它们在最终 `response.completed.output[]` 里的排列。
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
use std::mem;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{json, Value};

use super::tool_call_cache::{global_tool_call_cache, ToolCallEntry};
use crate::core::events::{build_tool_namespace_map, emit_sse_event};

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
    /// 首帧 name == `apply_patch` 时置 true,整个 item 走 Responses
    /// `custom_tool_call` wire(`output_item.added/done` 的 `type` 字段、
    /// 流式事件 name、args 解包都按 custom 路径走)而不是 `function_call`。
    /// 见 [`is_apply_patch_tool_name`] 和 `close_tool_call` 注释。
    is_apply_patch: bool,
    /// **server-side normalize 路径**:首帧 name == `exec_command` 时置 true。
    /// 整个 item **不立即** emit `output_item.added`,而是先 buffer 全部 args,
    /// `close_tool_call` 才 run `detect_shell_file_write`:
    ///   - 检测到 file-write 模式 → 改 wire 为 `custom_tool_call apply_patch`
    ///     并合成 V4A patch,让 Codex Desktop 自家 V4A applier 渲染 diff UI
    ///     而不是跑 shell。
    ///   - 未检测到 → 在 close 阶段补 emit 完整 function_call SSE 序列:
    ///     `response.output_item.added`(function_call type)+
    ///     `response.function_call_arguments.delta`(一次性整段 args)+
    ///     `response.function_call_arguments.done` +
    ///     `response.output_item.done`(`status=completed`),
    ///     Codex Desktop 当作普通 shell 调用执行,行为跟改前 100% 一致。
    /// trade-off:正常 shell 调用失去逐字符流式 UI(用户看不到 args 逐字出),
    /// 延迟在 first frame → close 之间。args 体积受上游 chat completions
    /// tool_call schema 约束(实测 issue #235 capture 数据 round 1/2 中
    /// exec_command tool_call args 远小于 reasoning/message 文本 stream),
    /// 实际延迟可接受;具体数据在 PR 完成 round 3 真机后回填 PR 评论。
    is_exec_command: bool,
    /// `response.output_item.added` 是否已经 emit 过 — 一旦 emit,后续帧
    /// 补全的 `tc.id` 不再覆盖 `call_id`,避免 `output_item.added` 用旧 id 而
    /// 后续 `input.delta` / `output_item.done` 用新 id,严格客户端会两次解读
    /// 为不同 item。
    output_item_added_emitted: bool,
    /// 仅 apply_patch 用:`close_tool_call` 把 args_acc 解出的裸 V4A 文本
    /// 缓存到这里;`tool_call_item_completed` 读这个字段而不是重新 parse,
    /// 保证流式 `output_item.done` 跟 envelope `output[]` 的 input 字符串
    /// 完全一致(避免 args_acc 在 close 与 envelope 构造之间发生变化时
    /// 静默 drift)。
    apply_patch_input: Option<String>,
}

/// Codex CLI 把 `apply_patch` 作为 freeform 工具注册
/// (`codex-rs/core/src/tools/handlers/apply_patch_spec.rs` —
/// `ToolSpec::Freeform { name: "apply_patch", ... }`),响应侧 router
/// (`codex-rs/core/src/tools/router.rs:92-130`)按 wire item type 路由:
/// `ResponseItem::FunctionCall` → `ToolPayload::Function { arguments }`,
/// `ResponseItem::CustomToolCall` → `ToolPayload::Custom { input }`,而
/// apply_patch handler 硬要求 `ToolPayload::Custom`,收 Function 直接返回
/// `"apply_patch handler received unsupported payload"` → abort
/// (`codex-rs/core/src/tools/handlers/apply_patch.rs:324`)。本 adapter
/// 把 chat completions provider(DeepSeek / Kimi / MiMo 等)回来的
/// `tool_calls[]` 默认渲染成 `function_call` wire,所以必须对 apply_patch
/// 特判 — 用 `custom_tool_call` wire 给 Codex CLI 才不 abort。
///
/// 名字以常量集中是为了和 `request/tools.rs::APPLY_PATCH_TOOL_NAME` 对齐
/// 字符串一致性(请求侧的特判描述 / 响应侧的 wire 重打包必须按同一 name 触发)。
fn is_apply_patch_tool_name(name: &str) -> bool {
    name == "apply_patch"
}

/// Codex CLI 的 shell 工具名(从 strings dump 验证:`exec_command` 是 Codex
/// CLI 在 chat function-call provider 上注册的 shell 工具)。
/// 命中此名 + args.cmd 是 file-write 模式 → 触发 `shell_to_apply_patch` server-side
/// normalize,改写 wire 为 `custom_tool_call apply_patch` 让 Codex Desktop 的
/// V4A applier 渲染 diff UI,而不是真跑 shell。
fn is_exec_command_tool_name(name: &str) -> bool {
    name == "exec_command"
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
    /// 是否在 `delta.content` 上启用 `<think>...</think>` 兜底拆分。
    /// 仅对 MiniMax 一类把 thinking 塞进 content 标签的 provider 开启;
    /// 其他 provider 默认透传,避免代码块/字面 `<think>` 被误吃。
    enable_think_tag_split: bool,
    think_tag_open: bool,
    think_tag_buffer: String,

    // ── tool_calls(工具调用流)── BTreeMap 用 OpenAI 自带的 index 做 key,
    // 迭代顺序天然按 OpenAI index 升序;output_index 在首次 open 时分配
    tool_calls: BTreeMap<u32, PendingToolCall>,
    /// fc_id 生成种子:`fc_<seed>_<openai_index>`;一次响应里固定不变
    fc_id_seed: String,

    model: String,
    finish_reason: Option<String>,
    usage: Option<Value>,

    /// 原入站 Responses API request 的**完整 body**(未经展平),用于在
    /// envelope 里回灌完整字段集(tools / parallel_tool_calls / tool_choice
    /// / reasoning / text / metadata / previous_response_id / instructions
    /// / temperature / top_p / max_output_tokens / truncation 等),让
    /// Codex CLI 严格 Responses 协议解析不缺字段、并能反向路由 namespace
    /// 工具调用。借鉴 mimo2codex `streamToSse.ts:75-105` `buildResponseSnapshot`。
    original_request: Option<Value>,

    /// SSE event 单调递增 sequence,从 0 开始;每次 `emit_event` 自增。
    /// 借鉴 mimo2codex `streamToSse.ts:71-72` `sequence_number: state.nextSeq()`。
    /// 严格 Responses 协议客户端用这个字段确保事件不乱序 / 不丢。
    sequence_number: u64,

    /// envelope.created_at 时间戳(秒),整个响应生命周期固定。借鉴
    /// mimo2codex `streamToSse.ts:41` `createdAt = Math.floor(Date.now() / 1000)`。
    created_at: u64,

    /// `function_name → namespace_name` 反查表,扫 `original_request.tools`
    /// 里所有 `type:"namespace"` 包的内层 function name 建立。`emit_*` function_call
    /// 相关 SSE event 时,若 function.name 命中此表,**给 item 加 `namespace`
    /// 字段** —— 这是 Codex.app 客户端 dispatch namespace 工具调用的必要字段
    /// (实测 `strings /Applications/Codex.app/Contents/Resources/codex` 含
    /// `"dynamic tool namespace must not be empty for"` 校验 + `DynamicToolCallRequest`
    /// 含 6 个字段都需 namespace),缺 namespace 字段时所有 namespace 工具调用
    /// 都返回 `"unsupported call: <name>"`。
    tool_namespace_map: std::collections::HashMap<String, String>,

    /// 当前 message item 累计的 url citation annotations(`delta.annotations`
    /// 解析后的转换结果)。每条 message item open 时清空,close 时写入
    /// final item 的 `content[0].annotations`。借鉴 mimo2codex
    /// `streamToSse.ts:48` `state.activeAnnotations` + `streamToSse.ts:338-352`
    /// 累计逻辑。
    active_annotations: Vec<Value>,
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
            enable_think_tag_split: false,
            think_tag_open: false,
            think_tag_buffer: String::new(),
            tool_calls: BTreeMap::new(),
            fc_id_seed,
            model: String::new(),
            finish_reason: None,
            usage: None,
            original_request: None,
            sequence_number: 0,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            tool_namespace_map: std::collections::HashMap::new(),
            active_annotations: Vec::new(),
        }
    }

    pub fn new_with_response_id(response_id: String) -> Self {
        let seed = response_id
            .strip_prefix("resp_")
            .unwrap_or(response_id.as_str())
            .to_owned();
        Self::new_with_ids(response_id, format!("msg_{seed}"), format!("rs_{seed}"))
    }

    /// 开启/关闭 `<think>...</think>` 兜底拆分(默认关闭)。
    pub fn with_think_tag_split(mut self, enabled: bool) -> Self {
        self.enable_think_tag_split = enabled;
        self
    }

    /// 注入原入站 Responses API request 的**完整 body**(未展平 / 未转换)。
    /// envelope 构造时从中抽取 tools / parallel_tool_calls / tool_choice /
    /// reasoning / text / metadata / previous_response_id / instructions /
    /// temperature / top_p / max_output_tokens / truncation 等字段,保协议
    /// 合规性。**同时**扫描 tools 里 `type:"namespace"` 包,建立
    /// `function.name → namespace.name` 反查表,emit function_call 相关
    /// events 时给 item 加 `namespace` 字段(Codex.app dispatch 必要字段)。
    /// 借鉴 mimo2codex `streamToSse.ts:75-105` `buildResponseSnapshot`(全字段
    /// 回灌)+ `Codex.app` binary strings 实证(`DynamicToolCallRequest` 含
    /// namespace 必填字段)。
    pub fn with_original_request(mut self, request: Option<Value>) -> Self {
        self.tool_namespace_map = build_tool_namespace_map(request.as_ref());
        self.original_request = request;
        self
    }

    /// 查询 function.name 对应的 namespace.name(用于给 function_call output
    /// 添加 `namespace` 字段,让 Codex.app 客户端能 dispatch 到对应 MCP server)。
    fn lookup_namespace_for(&self, tool_name: &str) -> Option<&str> {
        self.tool_namespace_map.get(tool_name).map(String::as_str)
    }

    /// 从 `original_request` 抽取一个字段;不存在时返回 fallback `Value`。
    fn req_field_or<'a>(&'a self, key: &str, fallback: Value) -> Value {
        self.original_request
            .as_ref()
            .and_then(|v| v.get(key))
            .cloned()
            .unwrap_or(fallback)
    }

    /// 构造 envelope 共享部分(`response.created` / `response.in_progress` /
    /// `response.completed` / `response.failed` 都用这一份字段集),对齐
    /// mimo2codex `streamToSse.ts:75-105` `buildResponseSnapshot`。
    fn build_envelope(&self, status: &str) -> Value {
        // tools / tool_choice / parallel_tool_calls / reasoning / text /
        // metadata / previous_response_id / instructions / temperature /
        // top_p / max_output_tokens 12 个字段 + envelope 自带 id/object/
        // status/model/created_at/truncation = 18 个字段。
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": if self.model.is_empty() { "unknown" } else { self.model.as_str() },
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

    pub fn assistant_message(&self) -> Option<Value> {
        if !self.message_open && self.tool_calls.is_empty() && self.reasoning_acc.is_empty() {
            return None;
        }

        let mut message = json!({
            "role": "assistant",
            "content": self.text_acc,
        });
        if !self.reasoning_acc.is_empty() {
            // ToolCallCache 重建走这条路把 reasoning 写回上游 messages —
            // 上游不需要见到 v2.0.8+ open_reasoning 注入的 `**Thinking**\n\n`
            // 人造 header(那只为 Codex CLI TUI 显示分支用),strip 后给上游。
            let cleaned = self
                .reasoning_acc
                .strip_prefix(crate::responses::request::CODEX_REASONING_PREFIX)
                .unwrap_or(self.reasoning_acc.as_str());
            message["reasoning_content"] = Value::String(cleaned.to_owned());
        }

        if !self.tool_calls.is_empty() {
            let tool_calls: Vec<Value> = self
                .tool_calls
                .values()
                .map(|pending| {
                    json!({
                        "id": pending.call_id.clone(),
                        "type": "function",
                        "function": {
                            "name": pending.name.clone(),
                            "arguments": pending.args_acc.clone(),
                        },
                    })
                })
                .collect();
            if !tool_calls.is_empty() {
                message["tool_calls"] = Value::Array(tool_calls);
            }
        }

        Some(message)
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

    /// emit `response.created` 紧跟 `response.in_progress`(同一个 response
    /// 信封)。OpenAI Responses 协议要求 `response.created` 后立即跟一个
    /// `response.in_progress`,严格客户端(litellm 自身、Anthropic 工具链)
    /// 不发就会卡住;Codex CLI 0.x/1.x 实测能容忍但不应当依赖这条容忍。
    /// 与 Python pre-refactor `streaming_adapter.py:266-281`、litellm
    /// `streaming_iterator.py:434-444` 行为一致。
    fn emit_lifecycle_open(&mut self, out: &mut Vec<u8>) {
        // open 状态下 envelope 还没有 output / usage / incomplete_details /
        // error,但已含 id/created_at/tools/tool_choice 等所有合规字段。
        let mut envelope = self.build_envelope("in_progress");
        envelope["output"] = json!([]);
        envelope["usage"] = Value::Null;
        envelope["incomplete_details"] = Value::Null;
        envelope["error"] = Value::Null;
        let created_payload = json!({"type": "response.created", "response": envelope.clone()});
        let in_progress_payload = json!({"type": "response.in_progress", "response": envelope});
        emit_event(
            out,
            &mut self.sequence_number,
            "response.created",
            created_payload,
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.in_progress",
            in_progress_payload,
        );
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

        // 确保 response.created / in_progress 只 emit 一次,且在任何 item open 之前
        if matches!(self.state, State::Idle) {
            self.state = State::Streaming;
            // model 名优先取本帧;首帧没有再用 unknown 占位
            if let Some(m) = chunk.model.as_deref() {
                self.model = m.to_owned();
            }
            self.emit_lifecycle_open(out);
        } else if self.model.is_empty() {
            if let Some(m) = chunk.model.as_deref() {
                self.model = m.to_owned();
            }
        }

        // 1.0.x 旧版 StreamingAdapter 明确只读取 choices[0]。Responses
        // 单个响应没有 Chat Completions 多候选的直接合并语义,所以这里保持
        // 首个 choice 策略并用测试锁定,不自行合并或展开其他 choice。
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
                self.emit_reasoning_delta(rs, out);
            }
        }
        for detail in &choice.delta.reasoning_details {
            if let Some(rs) = detail.text.as_deref() {
                if !rs.is_empty() {
                    self.emit_reasoning_delta(rs, out);
                }
            }
        }

        if let Some(text) = choice.delta.content.as_deref() {
            if !text.is_empty() {
                self.handle_content_delta(text, out);
            }
        }

        // url citation annotations(MiMo / Kimi / 其他支持 web_search 的 provider
        // 在 delta.annotations 里返回引用)。借鉴 mimo2codex `streamToSse.ts:338-352`:
        // 每条 annotation 转换字段(`summary` → `snippet`)、push 到 active_annotations
        // 累计、emit `response.output_text.annotation.added` event。前提是有 active
        // message item(annotation 必须挂在 message 上),没有就先 open。
        if !choice.delta.annotations.is_empty() {
            self.handle_annotations_delta(&choice.delta.annotations, out);
        }

        // tool_calls(可能与 content / reasoning 同帧;此处独立处理)
        for tc in &choice.delta.tool_calls {
            self.handle_tool_call_delta(tc, out);
        }
        if let Some(function_call) = &choice.delta.function_call {
            if function_call.has_payload() {
                let tc = ChatToolCallDelta {
                    index: 0,
                    id: None,
                    _kind: Some("function".to_owned()),
                    function: ChatToolCallFunctionDelta {
                        name: function_call.name.clone(),
                        arguments: function_call.arguments.clone(),
                    },
                };
                self.handle_tool_call_delta(&tc, out);
            }
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
            // **取舍**:wire 形态(function_call vs custom_tool_call)在 open
            // 时一次性根据**首帧 name** 决定,后续帧补全 name 不改 wire。
            // 实测 DeepSeek / Kimi / MiMo 都在首帧带 name。极端情况下首帧
            // name 为空、后续才补 apply_patch,会 fallback 到 function_call
            // wire(同当前行为,Codex CLI 仍会 abort apply_patch 一次),不
            // 比修复前差。
            let is_apply_patch = is_apply_patch_tool_name(&name);
            let is_exec_command = is_exec_command_tool_name(&name);
            self.tool_calls.insert(
                openai_index,
                PendingToolCall {
                    output_index,
                    fc_id: fc_id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args_acc: String::new(),
                    closed: false,
                    is_apply_patch,
                    is_exec_command,
                    output_item_added_emitted: false,
                    apply_patch_input: None,
                },
            );
            // apply_patch:wire 必须是 `custom_tool_call`(裸 `input` 字段)。
            // 中间增量 delta **不 emit** — chat 上游给的 args 是 JSON 字符串
            // 增量(`{"input": "*** Begin Patch\n..."`),从 JSON 字符串拼接
            // 过程中流式提取 `input` 字段值需要专门的 streaming JSON state
            // machine,本提交不引入。退而求其次:close 时一次性解 args 再
            // emit input.delta + output_item.done,代价是客户端看不到逐字
            // 流出的 diff(一次性出现整段 patch)。对一个长期完全不工作的
            // 功能,这是合理的第一步;后续可优化为真流式。
            if is_apply_patch {
                tracing::info!(
                    target = "adapters::apply_patch",
                    call_id = %call_id,
                    "apply_patch shim engaged: rewriting chat function_call wire to Responses custom_tool_call",
                );
                let item = json!({
                    "type": "custom_tool_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": name,
                    "input": "",
                    "status": "in_progress",
                });
                emit_event(
                    out,
                    &mut self.sequence_number,
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": item,
                    }),
                );
            } else if is_exec_command {
                // server-side normalize:不立即 emit `output_item.added`。
                // 整个 item 走 buffer 路径,到 `close_tool_call` run detector
                // 后再决定 wire 形态(custom_tool_call apply_patch 或回退
                // function_call exec_command)。output_item_added_emitted
                // 保持 false,delta/done 都延迟到 close。
                tracing::debug!(
                    target = "adapters::shell_to_apply_patch",
                    call_id = %call_id,
                    "exec_command buffering engaged; SSE emit deferred until close (file-write detection)",
                );
            } else {
                // 如果 function name 来自 namespace 包(从 original_request.tools
                // 反查表查到),给 item 加 `namespace` 字段 — Codex.app 客户端
                // dispatch namespace 工具时这是必要字段(strings 实证 binary 含
                // `dynamic tool namespace must not be empty for` 校验,缺字段会
                // 报 `unsupported call: <name>`)。
                let namespace = self.lookup_namespace_for(&name).map(str::to_owned);
                let mut item = json!({
                    "type": "function_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                });
                if let Some(ns) = namespace.as_ref() {
                    item["namespace"] = Value::String(ns.clone());
                }
                emit_event(
                    out,
                    &mut self.sequence_number,
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": item,
                    }),
                );
            }
            // output_item.added 已 emit。后续帧 backfill `id` 不应再换 call_id
            // (否则 `output_item.added` 与后续 `input.delta` / `output_item.done`
            // 用不同 call_id,严格客户端会两次解读为不同 item)。同样地,
            // apply_patch 的 `is_apply_patch` 决策也已固定。
            //
            // 例外:`is_exec_command` 走 server-side normalize buffer 路径,
            // 当前 frame **没有** emit `output_item.added`(延迟到 close),
            // 后续帧 backfill 的 call_id 仍可替换。
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                if !pending.is_exec_command {
                    pending.output_item_added_emitted = true;
                }
            }
        }

        // 后续帧可能补全 name(罕见但兼容)
        if let Some(name) = tc.function.name.as_deref() {
            if !name.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    if pending.name.is_empty() {
                        pending.name = name.to_owned();
                        if is_apply_patch_tool_name(name) && !pending.is_apply_patch {
                            // 罕见极端:首帧 name 为空,后续才补 apply_patch。
                            // `output_item.added` 已经 emit `function_call` wire,
                            // 不能回退。这一调用 Codex CLI 仍会 abort,但起码我们
                            // 在日志里能看到根因。
                            tracing::warn!(
                                target = "adapters::apply_patch",
                                call_id = %pending.call_id,
                                "apply_patch tool name arrived AFTER first frame; wire stays function_call and Codex CLI will reject. Investigate upstream provider chunking.",
                            );
                        }
                    }
                }
            }
        }
        // call_id 也可能在后续帧才出现 — 但只在 `output_item.added` 还没 emit
        // 时才允许替换。已 emit 后再换会让客户端看到同一 item 用两个不同
        // call_id。
        if let Some(id) = tc.id.as_deref() {
            if !id.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    if pending.output_item_added_emitted {
                        // 不再覆盖 — 同 item 已经对外暴露 call_id。
                    } else if pending.call_id.starts_with("call_") && pending.call_id.contains('_')
                    {
                        // 兜底生成的 call_id 形如 `call_<seed>_<idx>`,真 id 来了就替换
                        if !pending.call_id.starts_with(id) && pending.call_id != id {
                            pending.call_id = id.to_owned();
                        }
                    }
                }
            }
        }

        // arguments delta(增量字符串)。apply_patch 路径**只**累积不 emit
        // (理由见上文 open 处注释);非 apply_patch 仍逐 chunk emit
        // `function_call_arguments.delta` 让客户端看到逐字流。
        if let Some(args) = tc.function.arguments.as_deref() {
            if !args.is_empty() {
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    pending.args_acc.push_str(args);
                    if pending.is_apply_patch {
                        return;
                    }
                    // server-side normalize buffer 路径:exec_command 不 emit
                    // 增量 delta,close 时一次性 emit(改写为 apply_patch 或
                    // 回退 function_call)。
                    if pending.is_exec_command {
                        return;
                    }
                    let item_id = pending.fc_id.clone();
                    let output_index = pending.output_index;
                    emit_event(
                        out,
                        &mut self.sequence_number,
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

    fn close_tool_call(&mut self, openai_index: u32, interrupted: bool, out: &mut Vec<u8>) {
        // 先把所有需要的字段 clone 出来,避免 mutable borrow 跟
        // self.lookup_namespace_for 的 immutable borrow 冲突
        let (
            fc_id,
            call_id,
            name,
            args_acc,
            output_index,
            already_closed,
            is_apply_patch,
            is_exec_command,
        ) = {
            let Some(pending) = self.tool_calls.get(&openai_index) else {
                return;
            };
            (
                pending.fc_id.clone(),
                pending.call_id.clone(),
                pending.name.clone(),
                pending.args_acc.clone(),
                pending.output_index,
                pending.closed,
                pending.is_apply_patch,
                pending.is_exec_command,
            )
        };
        if already_closed {
            return;
        }

        // ===== server-side normalize: exec_command → apply_patch =====
        // 仅在非 interrupted 路径检测;interrupted 时模型未完成意图表达,即使
        // detect 在 partial args 上意外匹配也不应执行(等同于伪造模型本不打算
        // 的 apply_patch 调用)。强制让 interrupted exec_command 走"incomplete
        // 占位"路径,**对称** apply_patch interrupted 处理:emit
        // `status="incomplete"` 让 Codex Desktop 看到 partial item 不去执行
        // shell(防止跑半截 here-doc 等 destructive 行为 — code-reviewer
        // IMPORTANT-3 修复)。
        if is_exec_command && interrupted {
            tracing::warn!(
                target = "adapters::shell_to_apply_patch",
                call_id = %call_id,
                args_len = args_acc.len(),
                "exec_command tool call cut off mid-stream. Emitting output_item with status=incomplete to prevent Codex Desktop from executing partial shell (e.g. truncated here-doc).",
            );
            // buffer 期 added 没 emit,补 emit 一个 in_progress 占位让客户端
            // 看到这条 item 存在(否则后续 output_item.done 会找不到对应 item)。
            self.emit_deferred_exec_command_added(&fc_id, &call_id, &name, output_index, out);
            if !args_acc.is_empty() {
                emit_event(
                    out,
                    &mut self.sequence_number,
                    "response.function_call_arguments.delta",
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": fc_id,
                        "output_index": output_index,
                        "delta": args_acc,
                    }),
                );
            }
            // 直接 emit `output_item.done status=incomplete` 不走下方默认
            // function_call 路径(那条会 emit `status=completed`,Codex
            // Desktop 看到 completed 就执行 shell)。**不**写 ToolCallCache,
            // 让 orphan-repair 路径补占位避免 partial 上下文污染下一 turn。
            let namespace = self.lookup_namespace_for(&name).map(str::to_owned);
            let mut item = json!({
                "type": "function_call",
                "id": fc_id,
                "call_id": call_id,
                "name": name,
                "arguments": args_acc,
                "status": "incomplete",
            });
            if let Some(ns) = namespace.as_ref() {
                item["namespace"] = Value::String(ns.clone());
            }
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
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                pending.closed = true;
            }
            return;
        } else if is_exec_command && !interrupted {
            if let Some(patch) = detect_shell_file_write_in_exec_args(&args_acc) {
                tracing::info!(
                    target = "adapters::shell_to_apply_patch",
                    call_id = %call_id,
                    target_path = %patch.target_path,
                    patch_bytes = patch.patch_input.len(),
                    "rewriting exec_command file-write to apply_patch custom_tool_call",
                );
                self.emit_normalized_apply_patch(
                    openai_index,
                    &fc_id,
                    &call_id,
                    output_index,
                    patch.patch_input,
                    out,
                );
                return;
            }
            // 未匹配 file-write → 继续走下方 function_call 路径,但需要补
            // emit `output_item.added` + `function_call_arguments.delta`(open
            // 阶段被 buffer 跳过了,客户端收不到 delta 累积,需要在 .done
            // 之前补一段含整个 args 的 delta 让客户端按协议拼装)。
            self.emit_deferred_exec_command_added(&fc_id, &call_id, &name, output_index, out);
            if !args_acc.is_empty() {
                emit_event(
                    out,
                    &mut self.sequence_number,
                    "response.function_call_arguments.delta",
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": fc_id,
                        "output_index": output_index,
                        "delta": args_acc,
                    }),
                );
            }
            // fall through to function_call close path
        }

        if is_apply_patch {
            // 从累积的 chat function args(标准形态 `{"input":"<V4A patch>"}`)
            // 提取裸 V4A 文本。降级:模型可能直接吐裸 V4A(不包 JSON)— 历史
            // 上 freeform 工具的输出就是这个形态,某些 chat 上游可能没把它
            // 重新包成 JSON。fallback 把 args_acc 整段当 input,让上游能看到
            // 解析失败的具体内容(对调试 + 让 apply_patch parser 给出可读
            // 错误而不是静默 abort 都有用)。
            if args_acc.trim().is_empty() {
                tracing::warn!(
                    target = "adapters::apply_patch",
                    call_id = %call_id,
                    "apply_patch tool was called with empty arguments — model likely misbehaving or provider stripped args",
                );
            }
            let input = extract_apply_patch_input(&args_acc);
            // 缓存 input 到 pending,供 `tool_call_item_completed`(envelope
            // output[] 终态)读,避免重复 parse 与潜在 drift。
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                pending.apply_patch_input = Some(input.clone());
            }
            // interrupted 中断时,patch 文本可能 mid-stream 被截断 — emit
            // `status="incomplete"` 让 Codex CLI 看到 apply_patch handler 不
            // 应该执行 partial patch(apply_patch destructive,partial 执行
            // 可能在意外目标上写入意外内容)。同时 skip `input.done`(很多
            // 严格客户端在 `.done` 才触发执行)。
            if interrupted {
                tracing::warn!(
                    target = "adapters::apply_patch",
                    call_id = %call_id,
                    args_len = args_acc.len(),
                    "apply_patch tool call cut off mid-stream (no finish_reason and not from [DONE]). Emitting output_item with status=incomplete; skipping input.done to prevent partial patch execution.",
                );
                let item = json!({
                    "type": "custom_tool_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": name,
                    "input": input,
                    "status": "incomplete",
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
                // 不存 cache(下一轮如果引用此 call_id 重建会拿到 incomplete
                // 上下文,反而误导;让 orphan repair 路径补占位)。
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    pending.closed = true;
                }
                return;
            }
            // open 阶段 emit 了空 input 的 output_item.added;这里一次性补
            // input.delta + output_item.done,让 Codex CLI 的 streaming
            // parser(`StreamingPatchParser`)拿到完整 patch 文本后 finish。
            emit_event(
                out,
                &mut self.sequence_number,
                "response.custom_tool_call_input.delta",
                json!({
                    "type": "response.custom_tool_call_input.delta",
                    "item_id": fc_id,
                    "output_index": output_index,
                    "call_id": call_id,
                    "delta": input,
                }),
            );
            emit_event(
                out,
                &mut self.sequence_number,
                "response.custom_tool_call_input.done",
                json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": fc_id,
                    "output_index": output_index,
                    "call_id": call_id,
                    "input": input,
                }),
            );
            let item = json!({
                "type": "custom_tool_call",
                "id": fc_id,
                "call_id": call_id,
                "name": name,
                "input": input,
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
            // ToolCallCache 用于下一轮 Codex CLI 发 tool output 时重建工具
            // 调用上下文。回灌走 chat completions(messages.tool_calls.function
            // .arguments 是 JSON 字符串),所以这里仍存原始 args_acc(JSON 形态)
            // 而不是 input 裸文本,与 `assistant_message` 的 tool_calls 形态对齐。
            global_tool_call_cache().save(
                &call_id,
                ToolCallEntry {
                    name: name.clone(),
                    arguments: args_acc.clone(),
                },
            );
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                pending.closed = true;
            }
            return;
        }

        emit_event(
            out,
            &mut self.sequence_number,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": fc_id,
                "output_index": output_index,
                "arguments": args_acc,
            }),
        );
        let namespace = self.lookup_namespace_for(&name).map(str::to_owned);
        let mut item = json!({
            "type": "function_call",
            "id": fc_id,
            "call_id": call_id,
            "name": name,
            "arguments": args_acc,
            "status": "completed",
        });
        if let Some(ns) = namespace.as_ref() {
            item["namespace"] = Value::String(ns.clone());
        }
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
        // 把 (call_id → name + arguments) 写进 ToolCallCache,供下一轮
        // Codex CLI 发 function_call_output 时 repair_tool_call_ids 路径 B
        // 在前 assistant 找不到 call_id 时重建工具调用上下文。
        global_tool_call_cache().save(
            &call_id,
            ToolCallEntry {
                name: name.clone(),
                arguments: args_acc.clone(),
            },
        );
        if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
            pending.closed = true;
        }
    }

    /// server-side normalize:detected file-write exec_command 转 apply_patch
    /// custom_tool_call。一次性 emit 完整序列(added → input.delta → input.done
    /// → output_item.done),让 Codex Desktop 把它当 apply_patch 调用渲染 diff UI。
    ///
    /// **call_id 复用**:用模型原 exec_command 的 call_id,这样下一 turn
    /// Codex Desktop 把 apply_patch 执行结果(custom_tool_call_output)按同
    /// call_id 回灌给模型,模型按自己的 turn 历史关联得上。
    ///
    /// **名字改为 `apply_patch`**:Codex Desktop V4A applier 看 name 字段
    /// dispatch handler,必须是 `apply_patch` 才会跑 V4A parse + diff UI
    /// 渲染。
    ///
    /// **缓存**:apply_patch_input 缓到 pending,envelope 终态读这个;同时
    /// ToolCallCache 用合成的 `{"input":<V4A>}` JSON 形态存,跟 chat
    /// completions assistant.tool_calls.function.arguments 形态对齐(下一
    /// turn `repair_tool_call_ids` 路径 B 重建上下文用)。
    fn emit_normalized_apply_patch(
        &mut self,
        openai_index: u32,
        fc_id: &str,
        call_id: &str,
        output_index: u32,
        v4a_input: String,
        out: &mut Vec<u8>,
    ) {
        const APPLY_PATCH_NAME: &str = "apply_patch";
        // output_item.added(空 input,in_progress)— Codex Desktop 看到这条
        // 才会创建 pending custom_tool_call item;后续 delta/done 在同一
        // item 上累积。
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "custom_tool_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": APPLY_PATCH_NAME,
                    "input": "",
                    "status": "in_progress",
                },
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.custom_tool_call_input.delta",
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": fc_id,
                "output_index": output_index,
                "call_id": call_id,
                "delta": v4a_input,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.custom_tool_call_input.done",
            json!({
                "type": "response.custom_tool_call_input.done",
                "item_id": fc_id,
                "output_index": output_index,
                "call_id": call_id,
                "input": v4a_input,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {
                    "type": "custom_tool_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": APPLY_PATCH_NAME,
                    "input": v4a_input,
                    "status": "completed",
                },
            }),
        );
        // 缓存:envelope 终态读 apply_patch_input;ToolCallCache 存 chat
        // 形态 `{"input":<V4A>}` 让下一 turn repair 路径用合成 args 重建上下文,
        // 同时把 name 切换为 apply_patch 让 cache 一致。
        let synthetic_args =
            serde_json::to_string(&json!({ "input": v4a_input })).unwrap_or_default();
        global_tool_call_cache().save(
            call_id,
            ToolCallEntry {
                name: APPLY_PATCH_NAME.to_string(),
                arguments: synthetic_args.clone(),
            },
        );
        if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
            pending.name = APPLY_PATCH_NAME.to_string();
            // IMPORTANT-1 修复(code-reviewer pre-push):pending.args_acc 必须
            // 跟 normalize 后的 wire 形态 + ToolCallCache 同步,否则
            // `assistant_message()` 用 (name=apply_patch, args_acc=原 {"cmd":...})
            // 不一致组合写进 `global_response_session_cache`,下一 turn
            // `previous_response_id` 重建走 `request.rs::build_messages_from_input`
            // 时,upstream 看到 name/args 不匹配的 assistant tool_call → 严格
            // provider 直接 400,宽松 provider 误导模型记忆。
            pending.args_acc = synthetic_args;
            pending.is_apply_patch = true;
            pending.is_exec_command = false;
            pending.apply_patch_input = Some(v4a_input);
            pending.output_item_added_emitted = true;
            pending.closed = true;
        }
    }

    /// 在 exec_command 进入普通 function_call 关闭路径之前,补 emit `output_item.added`
    /// — open 阶段被 buffer 路径跳过了。namespace 字段对 exec_command 不适用
    /// (Codex CLI 内置工具,不在 namespace 包里),不加。
    fn emit_deferred_exec_command_added(
        &mut self,
        fc_id: &str,
        call_id: &str,
        name: &str,
        output_index: u32,
        out: &mut Vec<u8>,
    ) {
        emit_event(
            out,
            &mut self.sequence_number,
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

    fn emit_reasoning_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        if !self.reasoning_open {
            self.open_reasoning(out);
        }
        self.reasoning_acc.push_str(text);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "delta": text,
            }),
        );
    }

    fn emit_text_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        if text.is_empty() {
            return;
        }
        if self.reasoning_open && !self.reasoning_closed {
            self.close_reasoning(out);
        }
        if !self.message_open {
            self.open_message(out);
        }
        self.text_acc.push_str(text);
        emit_event(
            out,
            &mut self.sequence_number,
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

    fn handle_content_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        if !self.enable_think_tag_split {
            self.emit_text_delta(text, out);
            return;
        }
        self.think_tag_buffer.push_str(text);
        self.drain_think_tag_buffer(out, false);
    }

    fn flush_content_parser(&mut self, out: &mut Vec<u8>) {
        if !self.enable_think_tag_split {
            return;
        }
        self.drain_think_tag_buffer(out, true);
    }

    /// 处理 chat completions stream 的 `delta.annotations` 字段(URL citation)。
    /// 借鉴 mimo2codex `streamToSse.ts:338-352`:
    /// 1. annotation 必须挂在 message item 上,没 active message 就先开
    /// 2. 字段映射:`summary` → `snippet`,缺失字段(`type` / `url` / `title`)填默认
    /// 3. 累计到 `active_annotations`(close message 时塞进 final item content[0].annotations)
    /// 4. emit `response.output_text.annotation.added`(逐 annotation 一个事件)
    fn handle_annotations_delta(&mut self, annotations: &[Value], out: &mut Vec<u8>) {
        if annotations.is_empty() {
            return;
        }
        // annotations 必须挂在 message 上;reasoning open 时先 close,然后 open message
        if self.reasoning_open && !self.reasoning_closed {
            self.close_reasoning(out);
        }
        if !self.message_open {
            self.open_message(out);
        }
        for annotation in annotations {
            let translated = translate_annotation(annotation);
            let annotation_index = self.active_annotations.len();
            self.active_annotations.push(translated.clone());
            let item_id = self.message_id.clone();
            let output_index = self.message_index;
            emit_event(
                out,
                &mut self.sequence_number,
                "response.output_text.annotation.added",
                json!({
                    "type": "response.output_text.annotation.added",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "annotation_index": annotation_index,
                    "annotation": translated,
                }),
            );
        }
    }

    /// MiniMax 原生 OpenAI-compatible 格式会把 thinking 写进
    /// `content` 的 `<think>...</think>` 中。这里把它拆成 Responses
    /// reasoning,避免标签和思考正文污染普通 assistant 文本。请求侧会优先
    /// 使用 reasoning_split=true,本解析器作为上游未拆分时的兜底。
    fn drain_think_tag_buffer(&mut self, out: &mut Vec<u8>, flush: bool) {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        loop {
            if self.think_tag_open {
                if let Some(pos) = self.think_tag_buffer.find(CLOSE) {
                    let reasoning = self.think_tag_buffer[..pos].to_owned();
                    if !reasoning.is_empty() {
                        self.emit_reasoning_delta(&reasoning, out);
                    }
                    self.think_tag_buffer.drain(..pos + CLOSE.len());
                    self.think_tag_open = false;
                    continue;
                }

                let keep = if flush {
                    0
                } else {
                    suffix_prefix_len(&self.think_tag_buffer, CLOSE)
                };
                let emit_len = self.think_tag_buffer.len().saturating_sub(keep);
                if emit_len > 0 {
                    let reasoning = self.think_tag_buffer[..emit_len].to_owned();
                    self.think_tag_buffer.drain(..emit_len);
                    self.emit_reasoning_delta(&reasoning, out);
                }
                break;
            }

            if let Some(pos) = self.think_tag_buffer.find(OPEN) {
                let text = self.think_tag_buffer[..pos].to_owned();
                if !text.is_empty() {
                    self.emit_text_delta(&text, out);
                }
                self.think_tag_buffer.drain(..pos + OPEN.len());
                self.think_tag_open = true;
                continue;
            }

            let keep = if flush {
                0
            } else {
                suffix_prefix_len(&self.think_tag_buffer, OPEN)
            };
            let emit_len = self.think_tag_buffer.len().saturating_sub(keep);
            if emit_len > 0 {
                let text = self.think_tag_buffer[..emit_len].to_owned();
                self.think_tag_buffer.drain(..emit_len);
                self.emit_text_delta(&text, out);
            }
            break;
        }

        if flush && !self.think_tag_buffer.is_empty() {
            let rest = mem::take(&mut self.think_tag_buffer);
            if self.think_tag_open {
                self.emit_reasoning_delta(&rest, out);
            } else {
                self.emit_text_delta(&rest, out);
            }
        }
    }

    fn tool_call_item_completed(&self, pending: &PendingToolCall) -> Value {
        if pending.is_apply_patch {
            // envelope.output[] 终态必须和流式 `response.output_item.done`
            // 的 item 一致(见 close_tool_call apply_patch 分支),否则严格
            // 客户端会两次解读为不同 item。读 close 时缓存好的 input,
            // 不重新 parse args_acc — 万一 args_acc 在 close 与 envelope
            // 构造之间发生意外变化(目前看不会,但防御性写法),两侧仍一致。
            // 缓存缺失时(理论上 close 一定先于 envelope build 跑,不应触发)
            // fallback 到 raw args_acc,而不是再次 parse,避免重复 emit
            // 任何 telemetry。
            let input = pending
                .apply_patch_input
                .clone()
                .unwrap_or_else(|| pending.args_acc.clone());
            return json!({
                "type": "custom_tool_call",
                "id": pending.fc_id,
                "call_id": pending.call_id,
                "name": pending.name,
                "input": input,
                "status": "completed",
            });
        }
        let mut item = json!({
            "type": "function_call",
            "id": pending.fc_id,
            "call_id": pending.call_id,
            "name": pending.name,
            "arguments": pending.args_acc,
            "status": "completed",
        });
        if let Some(ns) = self.lookup_namespace_for(&pending.name) {
            item["namespace"] = Value::String(ns.to_owned());
        }
        item
    }

    fn open_reasoning(&mut self, out: &mut Vec<u8>) {
        self.reasoning_open = true;
        self.reasoning_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            &mut self.sequence_number,
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
            &mut self.sequence_number,
            "response.reasoning_summary_part.added",
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        );
        // 注入 `**Thinking**\n\n` header 让 Codex CLI TUI 走"显示" 分支。
        // Codex CLI 0.128 `tui/src/history_cell.rs:2783 new_reasoning_summary_block`
        // 检测累积 buffer 里是否有匹配的 `**...**` 标记 —— 命中走显示分支,
        // 否则把整段 reasoning 标记为 transcript_only,主 UI 完全不渲染
        // (只在 `/transcript` 命令可见)。OpenAI o1/o3 自带 section header,
        // 但 Kimi for Coding / DeepSeek thinking 等纯文本流默认无 `**`,
        // 不补 prefix 就会被整段隐藏。详见
        // `docs/investigation/kimi-reasoning-truncation.md` §5.4 根因结论。
        const REASONING_HEADER: &str = "**Thinking**\n\n";
        self.reasoning_acc.push_str(REASONING_HEADER);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "delta": REASONING_HEADER,
            }),
        );
    }

    fn close_reasoning(&mut self, out: &mut Vec<u8>) {
        emit_event(
            out,
            &mut self.sequence_number,
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
            &mut self.sequence_number,
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
        let reasoning_item = self.reasoning_item_completed();
        let reasoning_index = self.reasoning_index;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": reasoning_index,
                "item": reasoning_item,
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
            &mut self.sequence_number,
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
            &mut self.sequence_number,
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
            &mut self.sequence_number,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": self.message_id,
                "output_index": self.message_index,
                "content_index": 0,
                "text": self.text_acc,
            }),
        );
        // close 时 part / final item 的 annotations 用累计的实际值
        // (open 时是 `[]` 因为还没 annotation,delta.annotations 处理时累计到
        // self.active_annotations,close 时塞回)。借鉴 mimo2codex
        // `streamToSse.ts:230-256` `finalizeActive` 的 message 分支。
        let annotations = Value::Array(self.active_annotations.clone());
        emit_event(
            out,
            &mut self.sequence_number,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": self.message_id,
                "output_index": self.message_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": self.text_acc,
                    "annotations": annotations,
                },
            }),
        );
        let message_item = self.message_item_completed();
        let message_index = self.message_index;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": message_index,
                "item": message_item,
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
                // 累计 url citations(delta.annotations 解析后)。借鉴
                // mimo2codex `streamToSse.ts:245-251` `finalItem` 的 message
                // 分支结构。
                "annotations": Value::Array(self.active_annotations.clone()),
            }],
        })
    }

    fn emit_close(&mut self, out: &mut Vec<u8>, from_done: bool) {
        // 如果到 [DONE] 还没 emit 过 created(纯 [DONE] 输入 / 全是坏 JSON),
        // 仍要补 emit 一次,保证客户端拿到完整生命周期(response.created +
        // response.in_progress 一起发)。
        if matches!(self.state, State::Idle) {
            self.state = State::Streaming;
            self.emit_lifecycle_open(out);
        }
        self.flush_content_parser(out);
        if self.reasoning_open && !self.reasoning_closed {
            self.close_reasoning(out);
        }
        if self.message_open && !self.message_closed {
            self.close_message(out);
        }
        // tool_calls 按 OpenAI index 顺序闭合(BTreeMap 自然有序)。
        // `interrupted` = 没有 finish_reason **且**不是因 `[DONE]` 自然结束。
        // 这是用于让 apply_patch 在 close_tool_call 里 emit
        // `status="incomplete"` 而不是 `completed`,防止严格客户端在 stream
        // 半截断时仍把 partial patch 当成完整 tool 调用执行
        // (apply_patch 是 destructive,partial 执行风险高)。
        let interrupted = self.finish_reason.is_none() && !from_done;
        let tc_indices: Vec<u32> = self.tool_calls.keys().copied().collect();
        for idx in tc_indices {
            self.close_tool_call(idx, interrupted, out);
        }

        // finish_reason → status / incomplete_details 映射。保留现有 5 路径
        // 行为(Codex CLI 实测能容忍 `response.completed status:incomplete`)。
        // mimo2codex `streamToSse.ts:403-411` 走 `response.failed` 路径需要
        // 区分 "EOF 中断" vs "真上游错误",当前 converter 拿不到这个上下文,
        // 留 follow-up 处理(单独 PR + 改 stream.rs 错误流路径标记)。
        let (status, incomplete_details) = match (self.finish_reason.as_deref(), from_done) {
            (Some("stop") | Some("tool_calls") | Some("function_call"), _) => {
                ("completed", Value::Null)
            }
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
            all_items.push((tc.output_index, self.tool_call_item_completed(tc)));
        }
        all_items.sort_by_key(|(idx, _)| *idx);
        let output_items: Vec<Value> = all_items.into_iter().map(|(_, v)| v).collect();

        let mut envelope = self.build_envelope(status);
        envelope["output"] = Value::Array(output_items);
        envelope["incomplete_details"] = incomplete_details;
        envelope["error"] = Value::Null;
        // Codex CLI 反序列化 `ResponseCompleted` 时 usage 中的 `input_tokens` /
        // `output_tokens` / `total_tokens` 是必填,缺一帧就整流断开重连。Chat
        // 上游的 `prompt_tokens` / `completion_tokens` 与 Responses 的字段名
        // 不同,部分 provider 也可能完全不发 usage,这里统一规范化。
        envelope["usage"] = normalize_usage_to_responses_shape(self.usage.clone());

        let final_payload = json!({
            "type": "response.completed",
            "response": envelope,
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.completed",
            final_payload,
        );
        self.state = State::Done;
    }
}

impl Default for ChatToResponsesConverter {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 Chat Completions 风格的 `usage`(prompt_tokens / completion_tokens /
/// total_tokens / *_tokens_details)统一翻译为 Responses 风格(input_tokens /
/// output_tokens / total_tokens / input_tokens_details / output_tokens_details)。
///
/// - 已经是 Responses 形态(含 `input_tokens` 键)时原值兜底返回,只补 total。
/// - 上游完全没发 usage 时返回三零结构,避免 Codex CLI 因
///   "missing field input_tokens" 报错断流(2026-05-06)。
/// - 与 litellm 的 `_transform_chat_completion_usage_to_responses_usage`
///   (docs/litellm/.../litellm_completion_transformation/transformation.py)
///   语义一致,仅做静态字段重命名,不引入业务行为差异。
fn normalize_usage_to_responses_shape(usage: Option<Value>) -> Value {
    let zero = json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
    });
    let Some(value) = usage else {
        return zero;
    };
    let Value::Object(map) = value else {
        return zero;
    };

    let already_responses = map.contains_key("input_tokens") || map.contains_key("output_tokens");
    let mut out = serde_json::Map::new();

    let input_tokens = if already_responses {
        map.get("input_tokens").cloned().unwrap_or_else(|| json!(0))
    } else {
        map.get("prompt_tokens")
            .cloned()
            .unwrap_or_else(|| json!(0))
    };
    let output_tokens = if already_responses {
        map.get("output_tokens")
            .cloned()
            .unwrap_or_else(|| json!(0))
    } else {
        map.get("completion_tokens")
            .cloned()
            .unwrap_or_else(|| json!(0))
    };
    let total_tokens = map.get("total_tokens").cloned().unwrap_or_else(|| {
        let i = input_tokens.as_u64().unwrap_or(0);
        let o = output_tokens.as_u64().unwrap_or(0);
        json!(i + o)
    });

    out.insert("input_tokens".into(), input_tokens);
    out.insert("output_tokens".into(), output_tokens);
    out.insert("total_tokens".into(), total_tokens);

    // *_tokens_details 子对象重命名;已经是 Responses 形态就原样保留。
    // **关键**:Codex CLI 0.128.0-alpha.1 严格 parse `ResponseCompleted` 时
    // 要求 `usage.input_tokens_details.cached_tokens` 必须存在(否则报
    // "missing field `cached_tokens`" → 直接当流断,触发 5 次重连 → 30s
    // Mimo 推理被重打 5 次 ≈ 150s 卡顿,2026-05-07 实测复现)。Mimo 上游
    // 实际发的是 `prompt_tokens_details: {}` 空对象,透传后下游缺字段必爆。
    // 同理 `output_tokens_details.reasoning_tokens` 也补默认 0,防同类断流。
    let mut input_details = match (
        map.get("input_tokens_details").cloned(),
        map.get("prompt_tokens_details").cloned(),
    ) {
        (Some(Value::Object(d)), _) | (_, Some(Value::Object(d))) => d,
        _ => serde_json::Map::new(),
    };
    input_details
        .entry("cached_tokens".to_owned())
        .or_insert(json!(0));
    out.insert("input_tokens_details".into(), Value::Object(input_details));

    let mut output_details = match (
        map.get("output_tokens_details").cloned(),
        map.get("completion_tokens_details").cloned(),
    ) {
        (Some(Value::Object(d)), _) | (_, Some(Value::Object(d))) => d,
        _ => serde_json::Map::new(),
    };
    output_details
        .entry("reasoning_tokens".to_owned())
        .or_insert(json!(0));
    out.insert(
        "output_tokens_details".into(),
        Value::Object(output_details),
    );

    Value::Object(out)
}

/// 写一帧 SSE event。`seq` 是 `ChatToResponsesConverter::sequence_number` 的可变
/// 引用 —— 函数自动把当前值塞进 payload `sequence_number` 字段并 +1。借鉴
/// mimo2codex `streamToSse.ts:71-72` `sequence_number: state.nextSeq()`,严格
/// Responses 协议客户端依赖此字段确保事件不丢 / 不乱序。
/// 把 chat completions `delta.annotations[]` 单条 annotation(MiMo / Kimi /
/// 其他 provider 模型回答里 URL citation 的载体)翻译成 Responses API 的
/// `response.output_text.annotation.added` event 里的 annotation 形态。
///
/// 借鉴 mimo2codex `streamToSse.ts:156-163` `translateAnnotation`:
/// - `type` 默认 `"url_citation"`(对齐 OpenAI Responses API 标准 annotation type)
/// - `url` / `title` 缺失填空字符串(严格协议客户端不容缺字段)
/// - **`summary` → `snippet`**(mimo2codex 重命名,跟 OpenAI Responses
///   url_citation annotation schema 对齐;summary 在 OpenAI 标准里没有,
///   snippet 是其引用预览的字段名)
fn translate_annotation(a: &Value) -> Value {
    let mut out = serde_json::Map::new();
    let atype = a
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("url_citation");
    out.insert("type".into(), Value::String(atype.to_owned()));
    out.insert(
        "url".into(),
        a.get("url")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    out.insert(
        "title".into(),
        a.get("title")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    if let Some(summary) = a.get("summary") {
        out.insert("snippet".into(), summary.clone());
    }
    Value::Object(out)
}

fn emit_event(out: &mut Vec<u8>, seq: &mut u64, event_name: &str, payload: Value) {
    emit_sse_event(out, seq, event_name, payload);
}

/// 从 chat function args(标准形态 `{"input": "<V4A patch>"}`)提取裸 V4A
/// 文本,供 `custom_tool_call.input` 字段使用。
///
/// 降级路径分两类,通过 tracing 区分以便事后定位:
///
/// 1. **JSON parse 失败 + 看起来像裸 V4A**(以 `*** Begin Patch` 开头):
///    debug 级,预期 happy path —— 上游 chat provider 把 freeform 工具
///    输出原样透传未包 JSON。
/// 2. **JSON parse 失败 + 不像 V4A**:warn 级,通常是 stream 截断 / UTF-8
///    损坏 / 上游加 markdown fence。把整段透传给 Codex CLI 至少能让用户
///    看到 `apply_patch verification failed: <parse_error>` 而不是 abort
///    无线索。
/// 3. **JSON valid 但缺 `input` 字段**:warn 级,通常是 schema drift
///    (模型用了 `patch` 而不是 `input` 等)。整段透传暴露真坏。
///
/// 借鉴上游 `codex-rs/apply-patch/apply_patch_tool_instructions.md` 的
/// V4A 格式约束。不做 V4A 语法校验 — 留给 Codex CLI 端的 `parse_patch`。
fn extract_apply_patch_input(args_acc: &str) -> String {
    let trimmed = args_acc.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed) => match parsed.get("input").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => {
                tracing::warn!(
                    target = "adapters::apply_patch",
                    args_preview = %args_acc.chars().take(120).collect::<String>(),
                    "apply_patch args parsed as JSON but missing `input` string field; passing raw args to Codex CLI",
                );
                args_acc.to_owned()
            }
        },
        Err(err) => {
            if trimmed.starts_with("*** Begin Patch") {
                tracing::debug!(
                    target = "adapters::apply_patch",
                    "apply_patch args are bare V4A (no JSON wrapper); passthrough",
                );
            } else {
                tracing::warn!(
                    target = "adapters::apply_patch",
                    error = %err,
                    args_len = args_acc.len(),
                    args_preview = %args_acc.chars().take(120).collect::<String>(),
                    "apply_patch args failed JSON parse and don't look like bare V4A; falling back to raw passthrough — likely truncation or schema drift",
                );
            }
            args_acc.to_owned()
        }
    }
}

/// 从 `exec_command` 工具调用的累积 args(chat completions function_call
/// arguments,标准形态 `{"cmd":"<shell command>", ...}`)里抽出 `cmd` 字符串,
/// 跑 [`crate::responses::shell_to_apply_patch::detect_shell_file_write`]
/// 判断是不是 MVP scope 内的 file-write 模式。
///
/// 三层 fallback:
/// 1. **JSON valid + `cmd` 字段**:主路径,直接 detect。
/// 2. **JSON valid 但缺 `cmd` 字段**:`return None`(透传 exec_command,避免
///    误判)。
/// 3. **JSON 解析失败**:`return None`(args 还在 streaming / 模型乱发,
///    透传安全)。
fn detect_shell_file_write_in_exec_args(
    args_acc: &str,
) -> Option<crate::responses::shell_to_apply_patch::V4APatch> {
    let trimmed = args_acc.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let cmd = parsed.get("cmd").and_then(Value::as_str)?;
    crate::responses::shell_to_apply_patch::detect_shell_file_write(cmd)
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

fn suffix_prefix_len(value: &str, pattern: &str) -> usize {
    let max = value.len().min(pattern.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if value.ends_with(&pattern[..len]) {
            return len;
        }
    }
    0
}

// ── 入站 chunk 反序列化结构 ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    model: Option<String>,
    /// `choices: null` 与 `tool_calls: null` 同源 —— 部分上游(MiMo /
    /// 一些聚合层)在某些 chunk 里把 choices 写成 null;直接 Vec 解析失败
    /// 会丢整帧。同样套 Option 兜底。
    #[serde(default, deserialize_with = "deserialize_null_or_missing_to_empty_vec")]
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
    /// MiniMax M2.7 在 `reasoning_split=true` 时把 thinking 单独放在
    /// reasoning_details 中。
    #[serde(default, deserialize_with = "deserialize_null_or_missing_to_empty_vec")]
    reasoning_details: Vec<ChatReasoningDetail>,
    /// OpenAI / DeepSeek / Kimi 工具调用增量;同一 `index` 的多 chunk 累计
    /// 成完整的 `function.arguments` JSON 字符串。
    ///
    /// **null 容忍**:小米 MiMo 在每个 delta 里把无关字段显式发成 `null`
    /// (`{"content":null,"reasoning_content":"...","tool_calls":null}`),
    /// 直接 `Vec<...>` 解析 `null` 会让整帧反序列化失败、被静默丢弃,导致
    /// 文本 / reasoning 全丢。这里走 Option 兜底再 flatten 回空 Vec。
    #[serde(default, deserialize_with = "deserialize_null_or_missing_to_empty_vec")]
    tool_calls: Vec<ChatToolCallDelta>,
    /// MiMo / Kimi / 其他 chat 上游在模型回答里引用网页 / 文档时,通过
    /// `delta.annotations` 增量返回 url citations。OpenAI 标准 chat
    /// completions 也支持(web_search 启用后),所有 provider 通用。
    /// 借鉴 mimo2codex `streamToSse.ts:338-352` 解析 + emit
    /// `response.output_text.annotation.added` event。
    /// 注释跟其他可空字段对齐,允许 null / missing 兜底为空 Vec。
    #[serde(default, deserialize_with = "deserialize_null_or_missing_to_empty_vec")]
    annotations: Vec<Value>,
    /// 旧版 Chat Completions 单工具调用增量。OpenAI 后续改为
    /// `tool_calls[]`,但 1.0.x 已把 `finish_reason=function_call` 视为完成,
    /// 这里把流式 delta 直接转成 index=0 的 function_call item。
    #[serde(default)]
    function_call: Option<LegacyFunctionCallDelta>,
}

fn deserialize_null_or_missing_to_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(|v| v.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
struct LegacyFunctionCallDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl LegacyFunctionCallDelta {
    fn has_payload(&self) -> bool {
        self.name.as_deref().map_or(false, |v| !v.is_empty())
            || self.arguments.as_deref().map_or(false, |v| !v.is_empty())
    }
}

#[derive(Debug, Default, Deserialize)]
struct ChatReasoningDetail {
    #[serde(default)]
    text: Option<String>,
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

    // ── <think> 兜底拆分 provider 门控回归 ─────────────────────────────

    #[test]
    fn think_tag_split_disabled_passes_literal_tags_through() {
        // 默认未开启 enable_think_tag_split:DeepSeek/Kimi 等 provider 在普通
        // content 里输出字面 <think>...</think>(代码示例、讨论)时必须原样透传,
        // 不能被解析成 reasoning,否则会丢用户实际想看到的文本。
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"deepseek-chat","choices":[{"index":0,"delta":{"content":"see <think>example</think> here"},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "应只产一个 message,不应误产 reasoning");
        assert_eq!(output[0]["type"], "message");
        assert_eq!(
            output[0]["content"][0]["text"],
            "see <think>example</think> here"
        );
    }

    #[test]
    fn think_tag_split_disabled_keeps_code_block_intact() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"qwen3","choices":[{"index":0,"delta":{"content":"usage: ```html\n<think>thoughts</think>\n```"},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        let text = output[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("<think>thoughts</think>"), "got: {text}");
    }

    #[test]
    fn think_tag_split_enabled_splits_minimax_style_content() {
        // 显式 opt-in 后,<think>...</think> 应被拆成 reasoning(MiniMax 行为)。
        let mut c =
            ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
                .with_think_tag_split(true);
        let _ = c.feed(
            br#"data: {"model":"MiniMax-M2.7","choices":[{"index":0,"delta":{"content":"<think>plan more</think>final"},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "**Thinking**\n\nplan more");
        assert_eq!(output[1]["content"][0]["text"], "final");
    }

    // ── Stage 3.2c 行为回归(content-only)── ─────────────────────────

    #[test]
    fn lifecycle_open_emits_created_and_in_progress_back_to_back() {
        // OpenAI Responses 协议要求 response.created 后立即跟 response.in_progress;
        // 严格客户端(litellm 自身、Anthropic 工具链)缺这条会卡住。
        // 同 envelope(同 id / status / model)保证语义一致。
        let mut c = fixed();
        let out = c.feed(
            br#"data: {"model":"mock","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

"#,
        );
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec!["response.created", "response.in_progress"],
            "首个 chunk 必须先 emit lifecycle open(created + in_progress)"
        );
        // 同一个 envelope:id / status / model 全一致
        assert_eq!(events[0].1["response"]["id"], events[1].1["response"]["id"]);
        assert_eq!(
            events[0].1["response"]["status"],
            events[1].1["response"]["status"]
        );
        assert_eq!(events[0].1["response"]["status"], "in_progress");
        assert_eq!(events[0].1["response"]["model"], "mock");
        assert_eq!(events[1].1["response"]["model"], "mock");
    }

    // ── envelope tools 字段(MCP namespace 反向路由)──
    // 借鉴 mimo2codex streamToSse.ts:102 / respToResponses.ts:117。Codex CLI
    // 用响应 envelope 里的 tools 数组 + (namespace, function.name) 复合主键
    // 反向路由 namespace 包装的 MCP 工具 function_call。

    #[test]
    fn envelope_includes_original_tools_in_lifecycle_events() {
        let original_tools = json!([
            {"type": "function", "name": "shell"},
            {"type": "namespace", "name": "mcp__notion__", "tools": [
                {"type": "function", "name": "notion_search"}
            ]}
        ]);
        let original_request = json!({
            "model": "kimi",
            "tools": original_tools.clone(),
        });
        let mut c =
            ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
                .with_original_request(Some(original_request));
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"model":"kimi","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        // response.created envelope 含完整原始 tools(未展平的 namespace)
        let created = events
            .iter()
            .find(|(n, _)| n == "response.created")
            .expect("response.created emitted");
        assert_eq!(
            created.1["response"]["tools"], original_tools,
            "response.created envelope 必须含原始 tools 数组,Codex CLI 据此反向路由"
        );

        // response.in_progress 同 envelope
        let in_progress = events
            .iter()
            .find(|(n, _)| n == "response.in_progress")
            .expect("response.in_progress emitted");
        assert_eq!(in_progress.1["response"]["tools"], original_tools);

        // response.completed 也含 tools
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .expect("response.completed emitted");
        assert_eq!(completed.1["response"]["tools"], original_tools);
    }

    #[test]
    fn envelope_includes_all_responses_api_fields_for_protocol_compliance() {
        // 严格 Responses 协议客户端期待 envelope 含 16+ 字段。借鉴 mimo2codex
        // streamToSse.ts:75-105 buildResponseSnapshot 全字段策略。
        let original_request = json!({
            "model": "kimi-for-coding",
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": null},
            "text": {"format": {"type": "text"}},
            "metadata": {"trace_id": "abc"},
            "previous_response_id": "resp_prev_xyz",
            "instructions": "You are helpful.",
            "temperature": 0.7,
            "top_p": 0.9,
            "max_output_tokens": 2048,
        });
        let mut c =
            ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
                .with_original_request(Some(original_request.clone()));
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"model":"kimi","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        for (event_name, expected_status) in [
            ("response.created", "in_progress"),
            ("response.in_progress", "in_progress"),
            ("response.completed", "completed"),
        ] {
            let ev = events
                .iter()
                .find(|(n, _)| n == event_name)
                .unwrap_or_else(|| panic!("{event_name} not emitted"));
            let resp = &ev.1["response"];
            assert_eq!(resp["status"], expected_status);
            // 全字段必须存在(即使 null)以满足严格协议解析
            for field in [
                "id",
                "object",
                "created_at",
                "status",
                "model",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "text",
                "metadata",
                "previous_response_id",
                "instructions",
                "temperature",
                "top_p",
                "max_output_tokens",
                "truncation",
                "output",
                "usage",
                "incomplete_details",
                "error",
            ] {
                assert!(
                    resp.get(field).is_some(),
                    "{event_name} envelope missing field `{field}`\nactual: {resp}"
                );
            }
            // 关键字段值回灌正确
            assert_eq!(resp["tool_choice"], "auto");
            assert_eq!(resp["parallel_tool_calls"], true);
            assert_eq!(resp["reasoning"]["effort"], "high");
            assert_eq!(resp["temperature"], 0.7);
            assert_eq!(resp["previous_response_id"], "resp_prev_xyz");
            assert_eq!(resp["truncation"], "disabled");
            assert!(
                resp["created_at"].as_u64().is_some(),
                "created_at must be unix seconds"
            );
        }
    }

    #[test]
    fn function_call_item_includes_namespace_field_when_tool_came_from_namespace_pack() {
        // 关键修复:Codex.app 客户端 dispatch namespace 工具时必须读 `namespace`
        // 字段(strings 实证 binary 含 `dynamic tool namespace must not be empty`
        // 校验)。converter 扫 original_request.tools 里 namespace 包,建反查表,
        // emit function_call output 时给 item 加 namespace。
        let original_request = json!({
            "model": "kimi",
            "tools": [
                {"type": "function", "name": "shell"},
                {"type": "namespace", "name": "mcp__notion__", "tools": [
                    {"type": "function", "name": "notion_search"},
                    {"type": "function", "name": "notion_create_pages"},
                ]},
                {"type": "namespace", "name": "mcp__figma__", "tools": [
                    {"type": "function", "name": "whoami"},
                ]},
            ]
        });
        let mut c =
            ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
                .with_original_request(Some(original_request));
        let mut all = Vec::new();
        // 模型生成 tool_call: notion_search
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"notion_search","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        // response.output_item.added 的 item 必须含 namespace = mcp__notion__
        let added = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.added"
                    && p["item"].get("type").and_then(|v| v.as_str()) == Some("function_call")
            })
            .expect("function_call output_item.added emitted");
        assert_eq!(
            added.1["item"]["namespace"], "mcp__notion__",
            "function_call.added item 必须含 namespace 字段(Codex.app dispatch 必要)"
        );
        assert_eq!(added.1["item"]["name"], "notion_search");

        // response.output_item.done 同理
        let done = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.done"
                    && p["item"].get("type").and_then(|v| v.as_str()) == Some("function_call")
            })
            .expect("function_call output_item.done emitted");
        assert_eq!(done.1["item"]["namespace"], "mcp__notion__");

        // response.completed 里的 output 数组 final item 也要含 namespace
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .expect("response.completed emitted");
        let final_output = completed.1["response"]["output"].as_array().unwrap();
        let final_fc = final_output
            .iter()
            .find(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
            .expect("function_call in completed output");
        assert_eq!(final_fc["namespace"], "mcp__notion__");
    }

    #[test]
    fn function_call_item_omits_namespace_when_tool_is_top_level_function() {
        // 顶级 function(非 namespace 包内层),item 不应含 namespace 字段,
        // 否则 Codex.app 可能把它误当 dynamic tool 路由到不存在的 server。
        let original_request = json!({
            "model": "kimi",
            "tools": [{"type": "function", "name": "shell"}]
        });
        let mut c =
            ChatToResponsesConverter::new_with_ids("resp_x".into(), "msg_x".into(), "rs_x".into())
                .with_original_request(Some(original_request));
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let added = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.added"
                    && p["item"].get("type").and_then(|v| v.as_str()) == Some("function_call")
            })
            .unwrap();
        assert!(
            added.1["item"].get("namespace").is_none(),
            "顶级 function 不应有 namespace 字段;got: {}",
            added.1["item"]
        );
    }

    // ── delta.annotations → response.output_text.annotation.added 通用入站处理
    // 借鉴 mimo2codex `streamToSse.ts:156-163, 338-352` 1:1 复刻,跨所有 provider
    // 通用(任何 chat 上游模型回答里 URL 引用都会用 delta.annotations 携带)。

    #[test]
    fn delta_annotations_emit_url_citation_event_with_summary_renamed_to_snippet() {
        // 关键字段重命名:`summary` → `snippet`(对齐 OpenAI Responses url_citation
        // schema,mimo2codex `streamToSse.ts:161` 同样做)
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"see ref"}}]}

"#,
        ));
        // 单 chunk 含 1 个 annotation,带 summary 字段
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"annotations":[{"type":"url_citation","url":"https://example.com/x","title":"Example","summary":"Brief excerpt"}]}}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let added = events
            .iter()
            .find(|(n, _)| n == "response.output_text.annotation.added")
            .expect("annotation.added emitted");
        assert_eq!(added.1["annotation"]["type"], "url_citation");
        assert_eq!(added.1["annotation"]["url"], "https://example.com/x");
        assert_eq!(added.1["annotation"]["title"], "Example");
        assert_eq!(
            added.1["annotation"]["snippet"], "Brief excerpt",
            "summary 字段必须被重命名为 snippet"
        );
        assert!(
            added.1["annotation"].get("summary").is_none(),
            "原 summary 字段不该出现在 Responses 端 annotation 里"
        );
        assert_eq!(added.1["annotation_index"], 0);
        assert_eq!(added.1["content_index"], 0);
    }

    #[test]
    fn multiple_annotations_in_one_chunk_each_get_unique_increasing_index() {
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}

"#,
        ));
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"annotations":[{"type":"url_citation","url":"https://a.com","title":"A"},{"url":"https://b.com","title":"B"}]}}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let indices: Vec<i64> = events
            .iter()
            .filter(|(n, _)| n == "response.output_text.annotation.added")
            .map(|(_, p)| p["annotation_index"].as_i64().unwrap())
            .collect();
        assert_eq!(indices, vec![0, 1], "多 annotation 索引单调递增 0,1,...");
        // 第 2 条没传 type 字段,默认 "url_citation"
        let second = events
            .iter()
            .filter(|(n, _)| n == "response.output_text.annotation.added")
            .nth(1)
            .unwrap();
        assert_eq!(second.1["annotation"]["type"], "url_citation");
        assert_eq!(second.1["annotation"]["url"], "https://b.com");
    }

    #[test]
    fn final_message_item_includes_accumulated_annotations() {
        // close 时 final message item 的 content[0].annotations 必须含累积的所有
        // annotation,不再是写死 `[]`。借鉴 mimo2codex `streamToSse.ts:245-251`
        // `finalItem` 的 message 分支结构。
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}

"#,
        ));
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"annotations":[{"url":"https://x.com","title":"X"}]}}]}

"#,
        ));
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let final_msg = completed.1["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "message")
            .expect("message in output");
        let annotations = final_msg["content"][0]["annotations"]
            .as_array()
            .expect("annotations array");
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0]["url"], "https://x.com");
        assert_eq!(annotations[0]["type"], "url_citation");
    }

    #[test]
    fn annotations_open_message_item_when_none_active() {
        // delta.annotations 出现时如果还没有 active message,自动 open 一个
        // (annotation 必须挂在 message 上)。借鉴 mimo2codex `streamToSse.ts:339`:
        // `if (state.activeKind !== "message") openMessage(sink, state)`。
        let mut c = fixed();
        let mut all = Vec::new();
        // 直接发只含 annotation 的 chunk(无 content delta)
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"annotations":[{"url":"https://only.com","title":"Only"}]}}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        // 应该 open 了 message item
        let msg_added = events
            .iter()
            .find(|(n, p)| n == "response.output_item.added" && p["item"]["type"] == "message")
            .expect("message output_item should be opened for orphan annotations");
        assert_eq!(msg_added.1["item"]["role"], "assistant");
        // 然后 emit annotation
        let annot = events
            .iter()
            .find(|(n, _)| n == "response.output_text.annotation.added")
            .expect("annotation event emitted after message open");
        assert_eq!(annot.1["annotation"]["url"], "https://only.com");
    }

    #[test]
    fn no_annotations_means_final_message_annotations_stays_empty() {
        // 普通对话(无 annotation),final message annotations 是 [],跟旧行为一致
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"plain text"},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let final_msg = completed.1["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "message")
            .unwrap();
        assert_eq!(final_msg["content"][0]["annotations"], json!([]));
    }

    #[test]
    fn every_sse_event_has_monotonically_increasing_sequence_number() {
        // 借鉴 mimo2codex streamToSse.ts:71-72 sequence_number: state.nextSeq()。
        // 每个 SSE event payload 必须含 sequence_number 字段,且整流单调递增。
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"model":"mock","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let mut prev: i64 = -1;
        for (name, payload) in &events {
            let seq = payload["sequence_number"]
                .as_i64()
                .unwrap_or_else(|| panic!("event {name} missing sequence_number: {payload}"));
            assert!(seq > prev, "{name} sequence_number {seq} not > prev {prev}");
            prev = seq;
        }
        assert_eq!(events[0].1["sequence_number"], 0);
    }

    #[test]
    fn envelope_tools_default_to_empty_array_when_unset() {
        // 没调 with_original_tools 时 envelope tools 字段必须存在且为 [],
        // 对齐 mimo2codex `state.req.tools ?? []`,严格 Responses 协议客户端
        // 不容缺字段。
        let mut c = fixed();
        let mut all = Vec::new();
        all.extend(c.feed(
            br#"data: {"model":"mock","choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}]}

"#,
        ));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        let created = events
            .iter()
            .find(|(n, _)| n == "response.created")
            .unwrap();
        assert_eq!(created.1["response"]["tools"], json!([]));
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        assert_eq!(completed.1["response"]["tools"], json!([]));
    }

    #[test]
    fn lifecycle_open_emits_once_even_across_many_chunks() {
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n");
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        // 整流里 response.created / in_progress 各只能出现 1 次
        let count = |needle: &str, body: &[u8]| {
            String::from_utf8_lossy(body)
                .lines()
                .filter(|l| l.starts_with(&format!("event: {needle}")))
                .count()
        };
        // 用 finish 的 out 检验全流(包含前两块)就麻烦,直接做端到端字符串拼接:
        let mut all = Vec::new();
        let mut c2 = fixed();
        all.extend(
            c2.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n"),
        );
        all.extend(
            c2.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\n"),
        );
        all.extend(c2.feed(b"data: [DONE]\n\n"));
        assert_eq!(count("response.created", &all), 1);
        assert_eq!(count("response.in_progress", &all), 1);
        // 顺便 sanity 一下原 c 也走完了
        assert!(!out.is_empty());
    }

    #[test]
    fn first_chunk_emits_only_lifecycle_open_when_content_is_empty() {
        let mut c = fixed();
        let out = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

"#,
        );
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec!["response.created", "response.in_progress"],
            "无实际内容时只 emit lifecycle open(created + in_progress),message 懒开"
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
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ],
            "首个非空 content 应同时懒开 message item"
        );
        assert_eq!(events[4].1["delta"], "Hi");
        assert_eq!(events[2].1["output_index"], 0);
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
        // summary[0].text 是注入 prefix + 上游 reasoning 累积的全文
        assert_eq!(output[0]["summary"][0]["text"], "**Thinking**\n\nThe");
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
        // open_reasoning 现在多 emit 一条 `**Thinking**` prefix delta,放在
        // reasoning_summary_part.added 之后、上游真实 delta "think" 之前。
        assert_eq!(
            names(&ev1),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added", // reasoning open
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta", // prefix `**Thinking**\n\n`
                "response.reasoning_summary_text.delta", // 上游 "think"
            ]
        );
        assert_eq!(ev1[2].1["item"]["type"], "reasoning");
        assert_eq!(ev1[2].1["output_index"], 0);
        assert_eq!(ev1[2].1["item"]["summary"], json!([]));
        assert_eq!(ev1[2].1["item"]["content"], Value::Null);
        assert_eq!(ev1[2].1["item"]["encrypted_content"], Value::Null);
        assert_eq!(ev1[4].1["delta"], "**Thinking**\n\n");
        assert_eq!(ev1[5].1["delta"], "think");

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
        assert_eq!(output[0]["summary"][0]["text"], "**Thinking**\n\nthink");
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
        // 多 chunk reasoning 与 prefix 合并后是 `**Thinking**\n\npart1 part2`
        assert_eq!(
            completed["output"][0]["summary"][0]["text"],
            "**Thinking**\n\npart1 part2"
        );
    }

    #[test]
    fn minimax_reasoning_details_becomes_reasoning_item() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"MiniMax-M2.7","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"think"}]}}]}

data: {"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "**Thinking**\n\nthink");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn minimax_think_tags_are_split_out_of_content() {
        let mut c = fixed().with_think_tag_split(true);
        let _ = c.feed(
            br#"data: {"model":"MiniMax-M2.7","choices":[{"index":0,"delta":{"content":"<think>plan"}}]}

data: {"choices":[{"index":0,"delta":{"content":" more</think>final"},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "**Thinking**\n\nplan more");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "final");
    }

    #[test]
    fn split_think_open_tag_is_buffered() {
        let mut c = fixed().with_think_tag_split(true);
        let _ = c.feed(b"data: {\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<thi\"}}]}\n\n");
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"nk>x</think>y\"},\"finish_reason\":\"stop\"}]}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        assert_eq!(
            completed["output"][0]["summary"][0]["text"],
            "**Thinking**\n\nx"
        );
        assert_eq!(completed["output"][1]["content"][0]["text"], "y");
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
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );
        assert_eq!(events[4].1["delta"], "part1");
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
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
            ],
            "首帧:lifecycle open + tool_call open + 第一段 args delta"
        );
        assert_eq!(events[2].1["item"]["type"], "function_call");
        assert_eq!(events[2].1["item"]["call_id"], "call_a");
        assert_eq!(events[2].1["item"]["name"], "f");
        assert_eq!(events[3].1["delta"], "{}");
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
    fn apply_patch_tool_call_emits_custom_tool_call_wire_not_function_call() {
        // 回归保护(issue #235):chat 上游(DeepSeek 等)用 function call 返回
        // apply_patch 时,adapter 必须把 wire 重打包成 Codex CLI 期望的
        // `custom_tool_call` 形态(上游 router 按 wire type 路由,apply_patch
        // handler 硬要求 `ToolPayload::Custom { input }`)。
        // patch 文本走标准 JSON 转义:在 args.arguments 字符串里,V4A 原文的
        // `\n` 被双重转义成 `\\n`(JSON 字符串里写 `\n`)。
        let mut c = fixed();
        // 真实 chat 上游 wire 中,tool_call.arguments 是 JSON 字符串字面值,
        // patch 里的换行必须双重转义(SSE outer JSON 的 string value 里写
        // `\\n`,解码后是 `\n` 字面;`arguments` 值再被 client 当 JSON 解一次
        // 得到 `*** Begin Patch\n...` 真换行的 V4A patch)。
        let chunks = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_ap","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** Update File: foo.py\\n@@\\n-old\\n+new\\n*** End Patch\\n\"}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let out = c.feed(chunks.as_bytes());
        let events = parse_emitted(&out);
        let kinds = names(&events);
        // open 必须用 custom_tool_call 而不是 function_call
        let added = events
            .iter()
            .find(|(n, _)| n == "response.output_item.added")
            .expect("应当 emit output_item.added");
        assert_eq!(
            added.1["item"]["type"], "custom_tool_call",
            "apply_patch wire 必须是 custom_tool_call,实际 events: {kinds:?}"
        );
        assert_eq!(added.1["item"]["name"], "apply_patch");
        // 中间不应有 function_call_arguments.delta(apply_patch 路径 close 时
        // 一次性 emit custom_tool_call_input.delta)
        assert!(
            !kinds.contains(&"response.function_call_arguments.delta"),
            "apply_patch 路径不应 emit function_call_arguments.delta,events: {kinds:?}"
        );
        // close 必须 emit custom_tool_call_input.delta + .done
        let input_delta = events
            .iter()
            .find(|(n, _)| n == "response.custom_tool_call_input.delta")
            .expect("应当 emit custom_tool_call_input.delta");
        let expected_v4a =
            "*** Begin Patch\n*** Update File: foo.py\n@@\n-old\n+new\n*** End Patch\n";
        assert_eq!(input_delta.1["delta"], expected_v4a);
        assert_eq!(input_delta.1["call_id"], "call_ap");
        // envelope.output[] 终态也必须是 custom_tool_call
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let output = &completed.1["response"]["output"][0];
        assert_eq!(output["type"], "custom_tool_call");
        assert_eq!(output["input"], expected_v4a);
        assert_eq!(output["call_id"], "call_ap");
    }

    #[test]
    fn apply_patch_falls_back_to_raw_args_when_not_json() {
        // 模型直接吐裸 V4A 而不包 JSON(某些 chat 上游可能这样转译 freeform)。
        // adapter 必须把整段 args_acc 当 input 而不是空字符串,让 Codex CLI
        // 至少能看到 patch 内容并尝试解析。
        let raw_v4a = "*** Begin Patch\n*** Add File: a.md\n+hi\n*** End Patch\n";
        // serde_json::to_string 自动产生合法 JSON 字符串 escape(`\n` → `\\n`
        // 字面、引号转义、反斜杠转义),比手工 replace 链可靠且贴近真实 wire。
        let args_json_string = serde_json::to_string(raw_v4a).unwrap();
        let mut c = fixed();
        let frame = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_ap\",\"type\":\"function\",\"function\":{{\"name\":\"apply_patch\",\"arguments\":{args_json_string}}}}}]}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
            args_json_string = args_json_string,
        );
        let out = c.feed(frame.as_bytes());
        let events = parse_emitted(&out);
        let delta = events
            .iter()
            .find(|(n, _)| n == "response.custom_tool_call_input.delta")
            .expect("custom_tool_call_input.delta 应当 emit");
        assert_eq!(
            delta.1["delta"], raw_v4a,
            "非 JSON args 应整段当 input(裸 V4A 兜底)"
        );
    }

    #[test]
    fn apply_patch_interrupted_stream_emits_incomplete_status_skips_input_done() {
        // 回归保护:apply_patch 是 destructive 工具,stream 中途断开 → close
        // 必须 emit `status="incomplete"` 且 skip `custom_tool_call_input.done`,
        // 让 Codex CLI 看到不完整状态而不是执行 partial patch。
        let partial_v4a = "*** Begin Patch\n*** Update File: foo.py\n@@\n-old\n"; // 截断在 @@ 之后
        let inner = serde_json::to_string(&json!({ "input": partial_v4a })).unwrap();
        let args_json_string = serde_json::to_string(&inner).unwrap();
        let mut c = fixed();
        // 仅 emit tool_call 增量与 lifecycle 开头,不 emit finish_reason / [DONE],
        // 模拟 upstream EOF 中断。
        let frame = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_ap\",\"type\":\"function\",\"function\":{{\"name\":\"apply_patch\",\"arguments\":{args_json_string}}}}}]}}}}]}}\n\n",
            args_json_string = args_json_string,
        );
        let _ = c.feed(frame.as_bytes());
        let out = c.finish();
        let events = parse_emitted(&out);
        let kinds = names(&events);
        // 必须 NOT 出现 .delta 或 .done 的 custom_tool_call_input(防止 client
        // 在 .done 时触发执行 partial patch)
        assert!(
            !kinds.contains(&"response.custom_tool_call_input.done"),
            "interrupted 时禁止 emit custom_tool_call_input.done,events: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"response.custom_tool_call_input.delta"),
            "interrupted 时禁止 emit custom_tool_call_input.delta(避免提前触发执行),events: {kinds:?}"
        );
        // output_item.done item 必须含 status=incomplete
        let done = events
            .iter()
            .find(|(n, _)| n == "response.output_item.done")
            .expect("interrupted 仍应 emit output_item.done");
        assert_eq!(done.1["item"]["type"], "custom_tool_call");
        assert_eq!(
            done.1["item"]["status"], "incomplete",
            "interrupted apply_patch 必须 status=incomplete"
        );
        // envelope 也是 incomplete + interrupted
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        assert_eq!(completed.1["response"]["status"], "incomplete");
        assert_eq!(
            completed.1["response"]["incomplete_details"]["reason"],
            "interrupted"
        );
    }

    #[test]
    fn apply_patch_streaming_input_matches_envelope_output() {
        // 防御性回归:`response.output_item.done` 的 `item.input` 必须跟
        // `response.completed.output[].input` 完全一致,避免两次 emit 路径
        // 在未来重构时 drift。
        let patch = "*** Begin Patch\n*** Update File: x.txt\n@@\n-a\n+b\n*** End Patch\n";
        // chat wire 里 `arguments` 是 JSON-string(双重编码):先把 V4A 包成
        // `{"input": "<V4A>"}` JSON 文本,再 JSON-quote 一次作为字符串值。
        let inner = serde_json::to_string(&json!({ "input": patch })).unwrap();
        let args_json_string = serde_json::to_string(&inner).unwrap();
        let mut c = fixed();
        let frame = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_match\",\"type\":\"function\",\"function\":{{\"name\":\"apply_patch\",\"arguments\":{args_json_string}}}}}]}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
            args_json_string = args_json_string,
        );
        let out = c.feed(frame.as_bytes());
        let events = parse_emitted(&out);
        let done = events
            .iter()
            .find(|(n, v)| {
                n == "response.output_item.done" && v["item"]["type"] == "custom_tool_call"
            })
            .expect("应当有 custom_tool_call output_item.done");
        let streamed_input = done.1["item"]["input"].as_str().unwrap().to_owned();
        let completed = events
            .iter()
            .rev()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let envelope_input = completed.1["response"]["output"][0]["input"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            streamed_input, envelope_input,
            "streamed output_item.done.input 必须跟 envelope.output[].input 完全一致"
        );
        assert_eq!(streamed_input, patch);
    }

    #[test]
    fn extract_apply_patch_input_extracts_or_falls_back() {
        // happy path:`{input: string}` 提出 string 字段
        assert_eq!(
            extract_apply_patch_input(r#"{"input":"*** Begin Patch\nfoo"}"#),
            "*** Begin Patch\nfoo"
        );
        // 非 JSON:整段透传
        let raw = "*** Begin Patch\nfoo\n*** End Patch\n";
        assert_eq!(extract_apply_patch_input(raw), raw);
        // JSON 但无 input 字段:整段透传
        assert_eq!(
            extract_apply_patch_input(r#"{"other":"x"}"#),
            r#"{"other":"x"}"#
        );
        // 空字符串:返回空
        assert_eq!(extract_apply_patch_input(""), "");
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

    // ── Stage 3.3c legacy function_call / multi-choice 兼容 ────────

    #[test]
    fn legacy_function_call_stream_becomes_function_call_item() {
        let mut c = fixed();
        let first = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"function_call":{"name":"legacy_tool","arguments":""}}}]}

"#,
        );
        let first_events = parse_emitted(&first);
        assert_eq!(
            names(&first_events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
            ]
        );
        assert_eq!(first_events[2].1["item"]["type"], "function_call");
        assert_eq!(first_events[2].1["item"]["id"], "fc_x_0");
        assert_eq!(first_events[2].1["item"]["call_id"], "call_x_0");
        assert_eq!(first_events[2].1["item"]["name"], "legacy_tool");

        let _ = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"function_call":{"arguments":"{\"a\""}}}]}

data: {"choices":[{"index":0,"delta":{"function_call":{"arguments":":1}"}},"finish_reason":"function_call"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        assert_eq!(
            names(&events),
            vec![
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        let item_done = &events[1].1["item"];
        assert_eq!(item_done["type"], "function_call");
        assert_eq!(item_done["status"], "completed");
        assert_eq!(item_done["call_id"], "call_x_0");
        assert_eq!(item_done["name"], "legacy_tool");
        assert_eq!(item_done["arguments"], "{\"a\":1}");

        let completed = &events[2].1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["incomplete_details"], Value::Null);
        assert_eq!(completed["output"][0]["type"], "function_call");
        assert_eq!(completed["output"][0]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn multi_choice_uses_first_choice_only_like_legacy_adapter() {
        let mut c = fixed();
        let _ = c.feed(
            br#"data: {"model":"m","choices":[{"index":0,"delta":{"content":"first"}},{"index":1,"delta":{"content":"second"}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"},{"index":1,"delta":{},"finish_reason":"stop"}]}

"#,
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let completed = &events.last().unwrap().1["response"];
        assert_eq!(completed["output"][0]["type"], "message");
        assert_eq!(completed["output"][0]["content"][0]["text"], "first");
        assert_ne!(completed["output"][0]["content"][0]["text"], "second");
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

    // ── usage 规范化 ──────────────────────────────────────────────────
    // Codex CLI ResponseCompleted 反序列化要求 usage.{input_tokens,output_tokens,
    // total_tokens} 都到位;Chat 上游用的是 prompt/completion_tokens,部分
    // provider 完全不发 usage —— 都要兜住。

    #[test]
    fn missing_upstream_usage_emits_zero_usage_block() {
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["output_tokens"], 0);
        assert_eq!(usage["total_tokens"], 0);
    }

    #[test]
    fn chat_usage_prompt_completion_remapped_to_responses() {
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = c.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 7);
        assert_eq!(usage["output_tokens"], 3);
        assert_eq!(usage["total_tokens"], 10);
        assert!(usage.get("prompt_tokens").is_none());
        assert!(usage.get("completion_tokens").is_none());
    }

    #[test]
    fn responses_shape_usage_passes_through() {
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = c.feed(b"data: {\"choices\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 7);
        assert_eq!(usage["output_tokens"], 3);
        assert_eq!(usage["total_tokens"], 10);
    }

    #[test]
    fn chat_usage_subdetails_remapped_to_responses_subdetails() {
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = c.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":2},\"completion_tokens_details\":{\"reasoning_tokens\":1}}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 2);
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 1);
        assert!(usage.get("prompt_tokens_details").is_none());
        assert!(usage.get("completion_tokens_details").is_none());
    }

    /// Mimo 上游实测会发 `prompt_tokens_details: {}`(空对象,无 `cached_tokens`)。
    /// Codex CLI 0.128.0-alpha.1 严格 parse `ResponseCompleted` 必须有
    /// `usage.input_tokens_details.cached_tokens`,缺字段会触发"stream
    /// disconnected" → 5 次重连重试 → 30s Mimo 推理被打 5 遍 ≈ 150s 卡顿
    /// (2026-05-07 实测 fake_proxy + codex exec --json 复现)。本测试锁定:
    /// 即使上游 details 是空对象或缺,我们 emit 都补 `cached_tokens: 0` /
    /// `reasoning_tokens: 0` 默认值。
    #[test]
    fn empty_upstream_details_get_default_cached_and_reasoning_fields() {
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        // Mimo 实测形态:prompt_tokens_details 是空 {},completion_tokens_details
        // 不发(整字段缺)。
        let _ = c.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":54,\"completion_tokens\":1042,\"total_tokens\":1096,\"prompt_tokens_details\":{}}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(
            usage["input_tokens_details"]["cached_tokens"], 0,
            "上游 prompt_tokens_details 空时必须默认 cached_tokens=0"
        );
        assert_eq!(
            usage["output_tokens_details"]["reasoning_tokens"], 0,
            "上游缺 completion_tokens_details 时必须默认 reasoning_tokens=0"
        );
    }

    #[test]
    fn missing_subdetails_entirely_still_emit_required_fields() {
        // 极端场景:上游 usage 完全没有 *_tokens_details 任何形态。
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = c.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 0);
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 0);
    }

    // ── null 容忍(MiMo 真实帧形态)─────────────────────────────────────
    // 上游在每个 delta 里把无关字段显式发 null:
    //   {"delta":{"content":null,"reasoning_content":"...","tool_calls":null}}
    // 直接 `Vec<ChatToolCallDelta>` 反序列化 null 会报错,导致整帧被
    // serde_json::from_str 静默丢弃,文本和 reasoning 全丢失。

    #[test]
    fn delta_with_explicit_null_tool_calls_does_not_drop_content() {
        let mut c = fixed();
        let _ = c.feed(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning_content\":null,\"tool_calls\":null},\"finish_reason\":null}]}\n\n".as_bytes(),
        );
        let out = c.feed(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\",\"reasoning_content\":null,\"tool_calls\":null,\"role\":null},\"finish_reason\":null}]}\n\n".as_bytes(),
        );
        let events = parse_emitted(&out);
        let kinds = names(&events);
        assert!(
            kinds.contains(&"response.output_text.delta"),
            "delta.content 必须 emit;实际事件: {kinds:?}"
        );
        let delta_event = events
            .iter()
            .find(|(n, _)| n == "response.output_text.delta")
            .unwrap();
        assert_eq!(delta_event.1["delta"], "你好");
    }

    #[test]
    fn delta_with_explicit_null_tool_calls_keeps_reasoning_content() {
        let mut c = fixed();
        let out = c.feed(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":null,\"reasoning_content\":\"想想\",\"tool_calls\":null,\"role\":null},\"finish_reason\":null}]}\n\n".as_bytes(),
        );
        let events = parse_emitted(&out);
        // open_reasoning 注入一次 `**Thinking**\n\n` prefix delta(让 Codex CLI
        // TUI 走 bold-header 显示分支),然后再 emit 上游 "想想" delta —— 总计 2 条。
        let reasoning_deltas: Vec<&Value> = events
            .iter()
            .filter(|(n, _)| n == "response.reasoning_summary_text.delta")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            reasoning_deltas.len(),
            2,
            "应有 prefix + 上游 reasoning_content 共两条 delta;实际事件: {:?}",
            names(&events)
        );
        assert_eq!(reasoning_deltas[0]["delta"], "**Thinking**\n\n");
        assert_eq!(reasoning_deltas[1]["delta"], "想想");
    }

    #[test]
    fn chunk_with_explicit_null_choices_is_not_dropped() {
        // 部分聚合层在 usage-only 帧里写 `choices: null` 而非 `[]`
        let mut c = fixed();
        let _ = c.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n");
        let _ = c.feed(b"data: {\"choices\":null,\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n");
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 3);
        assert_eq!(usage["output_tokens"], 1);
        assert_eq!(usage["total_tokens"], 4);
    }

    #[test]
    fn missing_total_tokens_is_computed_from_input_output_sum() {
        let mut c = fixed();
        let _ = c.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = c.feed(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6}}\n\n",
        );
        let out = c.feed(b"data: [DONE]\n\n");
        let events = parse_emitted(&out);
        let usage = &events.last().unwrap().1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 4);
        assert_eq!(usage["output_tokens"], 6);
        assert_eq!(usage["total_tokens"], 10);
    }

    // ===== server-side normalize: exec_command → apply_patch =====
    // 关键回归保护(issue #235 round 2 真机数据):chat-completions provider
    // (Kimi 实测 t0024/t0027/t0030/t0037)调 exec_command 跑 `cat <<EOF >`
    // `printf > file` `echo > file` 写文件,Codex Desktop 看不到 apply_patch
    // wire → 不渲染 diff UI(4+2 元素全失效)。adapter 在 server-side normalize
    // 这三种 shell file-write 为 custom_tool_call apply_patch SSE,让 Codex
    // Desktop V4A applier 渲染 diff card / 行号 / +N -M / 文件清单。

    fn exec_command_sse_chunk(cmd_value: &str) -> Vec<u8> {
        // 构造单 chunk 含完整 exec_command tool_call:首帧 name + 完整 args
        // (chat 上游一般首帧带 name,中间帧追加 args delta;我们 buffer
        // 整段 args 后才决策,所以单 chunk vs 多 chunk 等价测)
        //
        // chat completions schema: `function.arguments` 是个**JSON 字符串**,
        // 它的 value 又是个 JSON object 字面('{"cmd":"<cmd>"}')。因此需要
        // **双重 JSON encode**:先把 `{"cmd": cmd_value}` 序列化成 JSON 字符串,
        // 再把这个字符串当 string value 序列化进 SSE chunk。
        let args_obj = serde_json::to_string(&serde_json::json!({"cmd": cmd_value})).unwrap();
        let escaped_args = serde_json::to_string(&args_obj).unwrap();
        format!(
            r#"data: {{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_test_1","function":{{"name":"exec_command","arguments":{escaped_args}}}}}]}},"finish_reason":"tool_calls"}}]}}

"#
        )
        .into_bytes()
    }

    fn collect_apply_patch_inputs(events: &[(String, Value)]) -> Vec<String> {
        events
            .iter()
            .filter(|(n, _)| n == "response.custom_tool_call_input.done")
            .filter_map(|(_, p)| p["input"].as_str().map(str::to_owned))
            .collect()
    }

    fn find_function_call_done_with_name<'a>(
        events: &'a [(String, Value)],
        name: &str,
    ) -> Option<&'a Value> {
        events.iter().find_map(|(n, p)| {
            if n == "response.output_item.done"
                && p["item"].get("type").and_then(|v| v.as_str()) == Some("function_call")
                && p["item"].get("name").and_then(|v| v.as_str()) == Some(name)
            {
                Some(p)
            } else {
                None
            }
        })
    }

    #[test]
    fn exec_command_here_doc_rewritten_to_apply_patch_custom_tool_call() {
        let mut c = fixed();
        let mut all = Vec::new();
        // 真机 round 2 turn 0024 Kimi 同款 `cat <<'PYEOF' > /tmp/...py\n...\nPYEOF`
        let cmd = "cat <<'PYEOF' > /tmp/apply-patch-test-A/app-1.py\ndef main():\n    pass\n\n\nif __name__ == \"__main__\":\n    main()\nPYEOF";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        // emit 含完整 custom_tool_call apply_patch 序列
        let added = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.added"
                    && p["item"].get("type").and_then(|v| v.as_str()) == Some("custom_tool_call")
            })
            .expect("custom_tool_call output_item.added 必须 emit");
        assert_eq!(added.1["item"]["name"], "apply_patch");
        assert_eq!(
            added.1["item"]["call_id"], "call_test_1",
            "call_id 必须复用模型原 exec_command call_id"
        );

        // V4A input.done 含 Add File header + 行 `+` 前缀
        let inputs = collect_apply_patch_inputs(&events);
        assert_eq!(inputs.len(), 1, "应只有 1 个 apply_patch input.done");
        let v4a = &inputs[0];
        assert!(v4a.starts_with("*** Begin Patch\n"));
        assert!(v4a.contains("*** Add File: /tmp/apply-patch-test-A/app-1.py"));
        assert!(v4a.contains("+def main():"));
        assert!(v4a.contains("+    pass"));
        assert!(v4a.ends_with("*** End Patch\n"));

        // 不应再 emit 原 exec_command function_call 序列
        assert!(
            find_function_call_done_with_name(&events, "exec_command").is_none(),
            "exec_command 已被 normalize,不应再有 function_call.done with name=exec_command"
        );

        // response.completed.output 数组里的 final item 应为 custom_tool_call apply_patch
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .expect("response.completed 必须 emit");
        let final_output = completed.1["response"]["output"].as_array().unwrap();
        let has_apply_patch = final_output.iter().any(|item| {
            item.get("type").and_then(|v| v.as_str()) == Some("custom_tool_call")
                && item.get("name").and_then(|v| v.as_str()) == Some("apply_patch")
        });
        assert!(
            has_apply_patch,
            "envelope final output 必须含 apply_patch item"
        );
    }

    #[test]
    fn exec_command_printf_with_escapes_rewritten_to_apply_patch() {
        let mut c = fixed();
        let mut all = Vec::new();
        // 真机 round 2 turn 0027/0030 Kimi 同款 `printf '...\n' > file`
        let cmd = r#"printf '# 测试笔记\n\n今天天气不错。\n' > /tmp/apply-patch-test-B/note.md"#;
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        let inputs = collect_apply_patch_inputs(&events);
        assert_eq!(inputs.len(), 1);
        let v4a = &inputs[0];
        assert!(v4a.contains("*** Add File: /tmp/apply-patch-test-B/note.md"));
        // \n 转义应展开成真实 newline,每个非空行 `+` 前缀
        assert!(v4a.contains("+# 测试笔记"));
        assert!(v4a.contains("+今天天气不错。"));
    }

    #[test]
    fn exec_command_echo_simple_rewritten_to_apply_patch() {
        let mut c = fixed();
        let mut all = Vec::new();
        let cmd = "echo 'hello world' > /tmp/x.txt";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        let inputs = collect_apply_patch_inputs(&events);
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].contains("*** Add File: /tmp/x.txt"));
        assert!(inputs[0].contains("+hello world"));
    }

    #[test]
    fn exec_command_non_file_write_passes_through_as_function_call() {
        let mut c = fixed();
        let mut all = Vec::new();
        // `ls -la /tmp` 不是 file-write 模式 → 走 function_call 透传
        let cmd = "ls -la /tmp";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        // 应 emit 完整 function_call exec_command 序列(added/delta/done/output_item.done)
        let added = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.added"
                    && p["item"].get("name").and_then(|v| v.as_str()) == Some("exec_command")
            })
            .expect("exec_command output_item.added 必须 emit(buffer 期延迟后补)");
        assert_eq!(added.1["item"]["type"], "function_call");

        // function_call_arguments.delta 应含完整 args(buffer 期没 emit,close 时一次补)
        let args_delta = events
            .iter()
            .find(|(n, p)| {
                n == "response.function_call_arguments.delta"
                    && p["delta"]
                        .as_str()
                        .map(|s| s.contains("ls -la"))
                        .unwrap_or(false)
            })
            .expect("deferred function_call_arguments.delta 必须含完整 args");
        let _ = args_delta;

        let done = find_function_call_done_with_name(&events, "exec_command")
            .expect("function_call.done exec_command 必须 emit");
        assert_eq!(done["item"]["status"], "completed");

        // 不应有 custom_tool_call apply_patch
        let has_apply_patch = events.iter().any(|(n, p)| {
            n == "response.output_item.added"
                && p["item"].get("name").and_then(|v| v.as_str()) == Some("apply_patch")
        });
        assert!(
            !has_apply_patch,
            "非 file-write 不应被 normalize 成 apply_patch"
        );
    }

    #[test]
    fn exec_command_append_redirect_passes_through() {
        let mut c = fixed();
        let mut all = Vec::new();
        // `>>` append 模式被 detector reject,透传 function_call
        let cmd = "echo 'log line' >> /tmp/app.log";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        assert!(
            find_function_call_done_with_name(&events, "exec_command").is_some(),
            "append redirect 透传为 function_call exec_command"
        );
        assert!(
            collect_apply_patch_inputs(&events).is_empty(),
            "append redirect 不应被 normalize"
        );
    }

    #[test]
    fn exec_command_multi_command_passes_through() {
        let mut c = fixed();
        let mut all = Vec::new();
        // multi-cmd `&&` reject
        let cmd = "mkdir -p /tmp/d && echo 'x' > /tmp/d/a.txt";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        assert!(find_function_call_done_with_name(&events, "exec_command").is_some());
        assert!(collect_apply_patch_inputs(&events).is_empty());
    }

    #[test]
    fn exec_command_var_substitution_passes_through() {
        let mut c = fixed();
        let mut all = Vec::new();
        let cmd = r#"echo "$(date)" > /tmp/timestamp.txt"#;
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);
        assert!(find_function_call_done_with_name(&events, "exec_command").is_some());
        assert!(collect_apply_patch_inputs(&events).is_empty());
    }

    #[test]
    fn normalized_apply_patch_envelope_assistant_message_uses_synthetic_args() {
        // IMPORTANT-1 修复(code-reviewer):normalize 后 pending.args_acc 必须
        // 跟 wire 形态 + ToolCallCache 同步,否则 `assistant_message()` 用
        // (name=apply_patch, args_acc=原 {"cmd":...}) 写进 response_session_cache,
        // 下一 turn previous_response_id 重建给上游 name/args mismatch 数据。
        let mut c = fixed();
        let mut all = Vec::new();
        let cmd = "echo 'hello' > /tmp/x.txt";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        // envelope.output[] 含 custom_tool_call apply_patch + input = V4A
        let completed = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .expect("response.completed");
        let output = completed.1["response"]["output"].as_array().unwrap();
        let apply_patch_item = output
            .iter()
            .find(|item| {
                item.get("type").and_then(|v| v.as_str()) == Some("custom_tool_call")
                    && item.get("name").and_then(|v| v.as_str()) == Some("apply_patch")
            })
            .expect("envelope.output 含 apply_patch");
        let input = apply_patch_item["input"].as_str().expect("input field");
        assert!(input.contains("*** Add File: /tmp/x.txt"));
        assert!(input.contains("+hello"));
    }

    #[test]
    fn exec_command_interrupted_emits_incomplete_not_completed() {
        // IMPORTANT-3 修复(code-reviewer):exec_command 在 partial args 上被
        // 中断时(stream 没收到 finish_reason / [DONE]),必须 emit
        // `status="incomplete"`,防止 Codex Desktop 跑半截 shell(truncated
        // here-doc 等 destructive 行为)。原默认 function_call 关闭路径 emit
        // `status="completed"`,对 interrupted exec_command 必须特判。
        let mut c = fixed();
        let mut all = Vec::new();
        // 不带 finish_reason,模拟 stream 中断
        all.extend(c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_partial","function":{"name":"exec_command","arguments":"{\"cmd\": \"cat <<EOF > /tmp/x.py\nincomplete"}}]}}]}

"#,
        ));
        // **不**发 [DONE],直接 finish() 触发 interrupted
        let final_out = c.finish();
        all.extend(final_out);

        let events = parse_emitted(&all);

        // exec_command output_item.done 必须 status=incomplete
        let done = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.done"
                    && p["item"].get("name").and_then(|v| v.as_str()) == Some("exec_command")
            })
            .expect("interrupted exec_command output_item.done 必须 emit");
        assert_eq!(
            done.1["item"]["status"], "incomplete",
            "interrupted exec_command 必须 emit status=incomplete 防 Codex Desktop 跑半截 shell"
        );
    }

    #[test]
    fn normalized_apply_patch_call_id_reused_for_tool_output_correlation() {
        // 关键:下一 turn Codex Desktop 把 apply_patch 执行结果通过同 call_id
        // 回灌给模型,模型按 call_id 关联到自己原 exec_command 的 tool_call。
        // 必须 call_id 100% 复用,否则模型看到 orphan tool result。
        let mut c = fixed();
        let mut all = Vec::new();
        let cmd = "echo 'x' > /tmp/a.txt";
        all.extend(c.feed(&exec_command_sse_chunk(cmd)));
        all.extend(c.feed(b"data: [DONE]\n\n"));
        let events = parse_emitted(&all);

        let added = events
            .iter()
            .find(|(n, p)| {
                n == "response.output_item.added"
                    && p["item"].get("name").and_then(|v| v.as_str()) == Some("apply_patch")
            })
            .unwrap();
        assert_eq!(
            added.1["item"]["call_id"], "call_test_1",
            "normalize 后 call_id 必须复用模型原 exec_command call_id"
        );

        let done_item_call_id = events
            .iter()
            .find_map(|(n, p)| {
                if n == "response.output_item.done"
                    && p["item"].get("name").and_then(|v| v.as_str()) == Some("apply_patch")
                {
                    p["item"]["call_id"].as_str().map(str::to_owned)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(done_item_call_id, "call_test_1");
    }
}
