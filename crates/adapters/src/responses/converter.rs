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
    /// 首帧 name == `tool_search` 时置 true,整个 item 走 Responses
    /// `tool_search_call` wire(Codex 0.130+ `core/src/tools/router.rs:106-122`
    /// 按 `ResponseItem::ToolSearchCall` 路由到 `ToolPayload::ToolSearch`,
    /// 返 `ResponseInputItem::ToolSearchOutput` 进下轮 input)。
    /// 见 [`is_tool_search_tool_name`]。Refs MOC-32 / GH #288。
    is_tool_search: bool,
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
    /// `close_tool_call` 在 interrupted apply_patch 路径 emit
    /// `response.output_item.done` 时强制 `status="incomplete"`(防 Codex CLI
    /// 在 partial V4A 上跑 destructive apply)。**envelope 终态必须保持一致**
    /// (`tool_call_item_completed` 也得读 incomplete 才不会跟流式 done event
    /// 矛盾,严格客户端读 envelope 误以为完成会执行 partial patch)。
    /// Devin AI BUG_pr-review-job 报告修复。
    interrupted_during_close: bool,
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

/// Codex 0.130+ 把 MCP server tools 全 defer 到 `tool_search` builtin
/// (`Feature::ToolSearchAlwaysDeferMcpTools` 启用,见 `core/src/mcp_tool_exposure.rs:32-36`)。
/// LLM 调用 `tool_search` 时,Codex 期待 `ResponseItem::ToolSearchCall` wire
/// (`type:"tool_search_call"`,`protocol/src/models.rs:2674-2715` roundtrip 实证),
/// router 路由到 `ToolPayload::ToolSearch`(`core/src/tools/router.rs:106-122`)
/// 内部 BM25 dispatch。chat 上游回的 `function_call` wire 不被 Codex 这条路径
/// 识别 → 必须像 apply_patch 一样把 wire 重打包成 `tool_search_call`。
///
/// 跟 `request/tools.rs` 的 `"tool_search"` match arm 字符串对齐(请求侧把
/// tool_search 降级成 chat function;响应侧把 chat function_call 升回
/// `tool_search_call`,name 必须一致才能触发本特判)。
fn is_tool_search_tool_name(name: &str) -> bool {
    name == "tool_search"
}

// ── 实验 exp/resources-to-tool-search ────────────────────────────────────────
//
// MOC-32: chat-completions LLM (mimo 等) 调 `list_mcp_resources` /
// `read_mcp_resource` / `list_mcp_resource_templates` 想 discover MCP 工具,但这
// 三个 builtin 返的是 resources(文档/URI 元数据)不是 callable tools — 跟 LLM
// 想要的对不上。真正返 callable tools 的是 `tool_search`。
//
// 本实验:LLM 调上述 legacy MCP discovery 工具时,response 侧把 wire **redirect**
// 成 `tool_search_call` 给 Codex,arguments `{server:X[,uri:Y]}` 转成
// `{query:X}`。Codex tool_search 返 callable tools → LLM 拿到真正能调的工具。
//
// 这是给 user 真机迭代实验的最小可行版,根据真实抓取再调对接细节。
const REDIRECT_TO_TOOL_SEARCH_NAMES: &[&str] = &[
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

fn should_redirect_to_tool_search(name: &str) -> bool {
    REDIRECT_TO_TOOL_SEARCH_NAMES.contains(&name)
}

/// tool_search 期望 arguments `{query, limit?}`。redirect 来的 legacy 工具
/// arguments 是 `{server:X[, uri:Y]}` / `{cursor:...}` 等,转成 `{query:X}`。
/// 已经是 `{query}` 形态(LLM 直接调 tool_search)则原样返回。
fn normalize_tool_search_arguments(args: Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args;
    };
    if obj.contains_key("query") {
        return args; // tool_search 原生
    }
    if let Some(server) = obj.get("server").and_then(|v| v.as_str()) {
        return json!({ "query": server });
    }
    // 加固(MOC-48 observability):既无 query 也无 server(如分页 cursor / 空 args)
    // 原样透传 —— tool_search 走本地 BM25,无 query 大概率返 0 工具(no-op)。warn 让
    // 这种静默退化可观测:LLM 调 tool_search 没拿到工具时能从日志定位 args 形态。
    tracing::warn!(
        target: "adapters::tool_search",
        args = %args,
        "tool_search arguments have neither query nor server; passing through as-is — tool_search will likely return no tools",
    );
    args
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
                    // tool_search(redirect 来的)args 必须跟 wire(close_tool_call /
                    // tool_call_item_completed)一致 normalize {server[,uri]}→{query};
                    // 否则 session cache 回灌 chat 历史跟 wire mismatch,LLM 下轮看
                    // history 学到错的 args 形态(Devin #293 BUG_..._0001 第 3 处)。
                    let arguments = if pending.is_tool_search {
                        serde_json::from_str::<Value>(&pending.args_acc)
                            .map(normalize_tool_search_arguments)
                            .map(|v| v.to_string())
                            .unwrap_or_else(|_| pending.args_acc.clone())
                    } else {
                        pending.args_acc.clone()
                    };
                    json!({
                        "id": pending.call_id.clone(),
                        "type": "function",
                        "function": {
                            "name": pending.name.clone(),
                            "arguments": arguments,
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
            let raw_name = tc.function.name.clone().unwrap_or_default();
            // 实验 exp/resources-to-tool-search: LLM 调 legacy MCP discovery
            // (list_mcp_resources / read_mcp_resource / list_mcp_resource_templates)
            // → redirect 成 tool_search(返 callable tools 而非 resources 元数据)。
            let name = if should_redirect_to_tool_search(&raw_name) {
                tracing::info!(
                    target: "adapters::tool_search",
                    from = %raw_name,
                    call_id = %call_id,
                    "redirecting legacy MCP discovery call → tool_search",
                );
                "tool_search".to_string()
            } else {
                raw_name.clone()
            };
            // **取舍**:wire 形态(function_call vs custom_tool_call)在 open
            // 时一次性根据**首帧 name** 决定,后续帧补全 name 不改 wire。
            // 实测 DeepSeek / Kimi / MiMo 都在首帧带 name。极端情况下首帧
            // name 为空、后续才补 apply_patch,会 fallback 到 function_call
            // wire(同当前行为,Codex CLI 仍会 abort apply_patch 一次),不
            // 比修复前差。
            let is_apply_patch = is_apply_patch_tool_name(&name);
            let is_tool_search = is_tool_search_tool_name(&name);
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
                    is_tool_search,
                    output_item_added_emitted: false,
                    apply_patch_input: None,
                    interrupted_during_close: false,
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
                    target: "adapters::apply_patch",
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
            } else if is_tool_search {
                // tool_search wire 跟 apply_patch 同模式:open 时 emit
                // `response.output_item.added` 带 `type=tool_search_call`,
                // close 时一次性 emit `output_item.done`(arguments JSON object
                // 在 close 时 parse,中间不流式 — BM25 query 短不需要)。
                // wire schema 实证:`protocol/src/models.rs:2674-2715` roundtrip
                // test:`{type, call_id, execution:"client", arguments:{...}}`。
                tracing::info!(
                    target: "adapters::tool_search",
                    call_id = %call_id,
                    "tool_search shim engaged: rewriting chat function_call wire to Responses tool_search_call",
                );
                let item = json!({
                    "type": "tool_search_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "execution": "client",
                    "arguments": {},
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
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                pending.output_item_added_emitted = true;
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
                                target: "adapters::apply_patch",
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
                    // tool_search 跟 apply_patch 同模式 — 中间不流式
                    // (`open_tool_call` 处注释 "BM25 query 短不需要"),也 skip
                    // `function_call_arguments.delta` event。否则严格 Codex
                    // client 看到 `tool_search_call` open + `function_call_*`
                    // delta(语义错配 — delta 属 function_call wire 不属 tool_search)
                    // + `tool_search_call` done — 三层混乱。Devin Review 抓到。
                    if pending.is_tool_search {
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
            is_tool_search,
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
                pending.is_tool_search,
            )
        };
        if already_closed {
            return;
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
                    target: "adapters::apply_patch",
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
            // [MOC-57] 截断检测(JSON 结构 + V4A 信封双层级)。#303 只靠
            // `interrupted` 模糊信号判断 patch 是否完整;这里主动检测 args 是否
            // 被流式切断,即使上游没断流也能发现。
            let json_truncation = detect_json_truncation(&args_acc);
            let v4a_truncation = detect_v4a_truncation(&input);
            let is_truncated = json_truncation.is_some() || v4a_truncation.is_some();
            // [MOC-57] V4A 后验语法校验(仅非截断时做,截断本身已是 invalid)。
            let v4a_validation = if is_truncated {
                None
            } else {
                validate_v4a_syntax(&input).err()
            };
            if let Some(ref err) = v4a_validation {
                tracing::warn!(
                    target: "adapters::apply_patch",
                    call_id = %call_id,
                    line = err.line,
                    message = %err.message,
                    "apply_patch V4A syntax validation failed",
                );
            }
            // interrupted / 截断 / 语法错误任一成立 → emit `status="incomplete"`,
            // 让 Codex CLI 看到 apply_patch handler 不应执行 partial/invalid patch
            // (apply_patch destructive,partial 执行可能在意外目标写入意外内容)。
            let should_incomplete = interrupted || is_truncated || v4a_validation.is_some();
            if should_incomplete {
                tracing::warn!(
                    target: "adapters::apply_patch",
                    call_id = %call_id,
                    args_len = args_acc.len(),
                    interrupted,
                    json_truncated = json_truncation.is_some(),
                    v4a_truncated = v4a_truncation.is_some(),
                    v4a_invalid = v4a_validation.is_some(),
                    detail = %json_truncation
                        .as_deref()
                        .or(v4a_truncation.as_deref())
                        .unwrap_or(""),
                    "apply_patch tool call incomplete (interrupted/truncated/invalid). Emitting output_item with status=incomplete; skipping input.done to prevent partial patch execution.",
                );
                // emit incomplete 不写 cache(下一轮引用此 call_id 会拿到 incomplete
                // 上下文,反而误导;让 orphan repair 路径补占位)。标记
                // interrupted_during_close 让 `tool_call_item_completed` 在 envelope
                // 终态也 emit incomplete(严格客户端读 envelope 才不误以为 patch 完整)。
                emit_apply_patch_output(
                    &fc_id,
                    &call_id,
                    &name,
                    &input,
                    &args_acc,
                    output_index,
                    /* interrupted = */ true,
                    out,
                    &mut self.sequence_number,
                    &mut self.tool_calls,
                    openai_index,
                );
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    pending.closed = true;
                }
                return;
            }
            // 完整 patch:emit input.delta + done + completed item + 写 cache。
            emit_apply_patch_output(
                &fc_id,
                &call_id,
                &name,
                &input,
                &args_acc,
                output_index,
                /* interrupted = */ false,
                out,
                &mut self.sequence_number,
                &mut self.tool_calls,
                openai_index,
            );
            if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                pending.closed = true;
            }
            return;
        }

        if is_tool_search {
            // chat 上游回的 args 是 JSON string (e.g. '{"query":"calendar"}'),
            // 但 Codex `ResponseItem::ToolSearchCall.arguments` 字段是 JSON Value
            // (`router.rs:107-115` 用 `serde_json::from_value::<SearchToolCallParams>`
            // parse,接受 object 不接受 string)。parse 失败 fallback 给 raw
            // string 包成 {"raw": "..."} — Codex 端会 fail parse,但至少能在
            // log 看到 LLM 实际发了啥(比静默 drop 强)。
            let arguments_value: Value = serde_json::from_str(&args_acc).unwrap_or_else(|err| {
                tracing::warn!(
                    target: "adapters::tool_search",
                    call_id = %call_id,
                    args_len = args_acc.len(),
                    error = %err,
                    "tool_search arguments JSON parse failed; emitting {{raw: ...}} fallback (Codex will reject but logs preserve model intent)",
                );
                json!({ "raw": args_acc })
            });
            // 实验 exp/resources-to-tool-search: redirect 来的 args 是 legacy 工具
            // 形态 ({server[,uri]}),转成 tool_search 期望的 {query}。LLM 直接调
            // tool_search ({query}) 则原样透传。
            let arguments_value = normalize_tool_search_arguments(arguments_value);
            // interrupted 中断(stream 截断,无 finish_reason 且非 [DONE]):对齐
            // apply_patch 分支 emit `status="incomplete"` + 标记 interrupted_during_close,
            // 避免 Codex CLI 把半截的 tool_search call 当完整执行。
            // (Devin #289 review BUG_..._0002:is_tool_search 之前无条件 completed)
            if interrupted {
                tracing::warn!(
                    target: "adapters::tool_search",
                    call_id = %call_id,
                    "tool_search call cut off mid-stream (no finish_reason and not from [DONE]); emitting status=incomplete",
                );
                let item = json!({
                    "type": "tool_search_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "execution": "client",
                    "arguments": arguments_value,
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
                if let Some(pending) = self.tool_calls.get_mut(&openai_index) {
                    pending.closed = true;
                    pending.interrupted_during_close = true;
                }
                return;
            }
            let item = json!({
                "type": "tool_search_call",
                "id": fc_id,
                "call_id": call_id,
                "execution": "client",
                "arguments": arguments_value,
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
            // 跟 apply_patch 一样不存 tool_call_cache — Codex 后续 turn 用
            // `ResponseInputItem::ToolSearchOutput` inject 工具列表,不需要从
            // cache 重建 call 上下文。
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
        if pending.is_tool_search {
            // envelope.output[] 终态必须跟流式 `response.output_item.done` 的
            // item 一致(见 close_tool_call tool_search 分支),否则严格客户端
            // 会两次解读为不同 item。三处都要对齐流式侧:
            // ① args parse 失败 fallback {"raw":...};
            // ② normalize(redirect 来的 {server[,uri]} → {query})——
            //    Devin #293 BUG_..._0001:envelope 漏 normalize → 跟流式
            //    arguments mismatch;
            // ③ interrupted_during_close → status="incomplete" —— Devin #293
            //    BUG_..._0002:envelope 之前写死 "completed",跟流式 incomplete
            //    不一致,严格客户端读 envelope output[] 会误以为 tool_search 完整。
            let arguments_value: Value = serde_json::from_str(&pending.args_acc)
                .unwrap_or_else(|_| json!({ "raw": pending.args_acc }));
            let arguments_value = normalize_tool_search_arguments(arguments_value);
            let status = if pending.interrupted_during_close {
                "incomplete"
            } else {
                "completed"
            };
            return json!({
                "type": "tool_search_call",
                "id": pending.fc_id,
                "call_id": pending.call_id,
                "execution": "client",
                "arguments": arguments_value,
                "status": status,
            });
        }
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
            // interrupted apply_patch envelope 终态必须跟流式 done event 一致
            // 为 `incomplete`,否则严格客户端读 envelope output[] 看到
            // `completed` 会执行 partial V4A patch(destructive)— Devin
            // pre-merge review 报告 BUG_pr-review-job-9600e18f8e4c4a90 修复。
            let status = if pending.interrupted_during_close {
                "incomplete"
            } else {
                "completed"
            };
            return json!({
                "type": "custom_tool_call",
                "id": pending.fc_id,
                "call_id": pending.call_id,
                "name": pending.name,
                "input": input,
                "status": status,
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
/// 文本,供 `custom_tool_call.input` 字段使用,并做**非破坏性信封修复**
/// (见 #302:chat function-call provider 无 lark grammar 受约束解码,模型
/// 手搓的 V4A 信封常出错)。
///
/// body 来源优先级:
/// 1. JSON `input` 字段(标准形态)。
/// 2. 常见别名 key(`patch`/`diff`/… —— schema drift 回收,真机实测模型会发
///    `{"patch": "*** Begin Patch…"}`),仅当值含 `*** Begin Patch` 才取。
/// 3. 裸 V4A(JSON parse 失败但含 `*** Begin Patch`,可能被 markdown fence 包裹)。
///
/// 取到候选后过 [`repair_v4a_envelope`] 规整信封(剥 markdown fence / Begin 前
/// End 后的杂散行)。取不到候选(截断 / 非 V4A 垃圾)则**原样透传**,交给 Codex
/// CLI `parse_patch` 暴露真坏 —— 绝不静默吞、不截断正文。不做 V4A 语法校验。
pub(crate) fn extract_apply_patch_input(args_acc: &str) -> String {
    let trimmed = args_acc.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let candidate: Option<String> = match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed) => {
            if let Some(s) = parsed.get("input").and_then(Value::as_str) {
                Some(s.to_owned())
            } else if let Some((alt_key, s)) = APPLY_PATCH_ALT_KEYS.iter().find_map(|k| {
                parsed
                    .get(*k)
                    .and_then(Value::as_str)
                    .filter(|s| s.contains("*** Begin Patch"))
                    .map(|s| (*k, s))
            }) {
                tracing::warn!(
                    target: "adapters::apply_patch",
                    alt_key,
                    "apply_patch JSON 缺 `input`,从别名 key 回收 V4A body(schema drift 修复,见 #302)",
                );
                Some(s.to_owned())
            } else {
                tracing::warn!(
                    target: "adapters::apply_patch",
                    args_preview = %args_acc.chars().take(120).collect::<String>(),
                    "apply_patch args parsed as JSON but missing `input` string field; passing raw args to Codex CLI",
                );
                None
            }
        }
        Err(err) => {
            if trimmed.contains("*** Begin Patch") {
                tracing::debug!(
                    target: "adapters::apply_patch",
                    "apply_patch args are bare V4A (no JSON wrapper); passthrough",
                );
                Some(args_acc.to_owned())
            } else {
                tracing::warn!(
                    target: "adapters::apply_patch",
                    error = %err,
                    args_len = args_acc.len(),
                    args_preview = %args_acc.chars().take(120).collect::<String>(),
                    "apply_patch args failed JSON parse and don't look like bare V4A; falling back to raw passthrough — likely truncation or schema drift",
                );
                None
            }
        }
    };
    match candidate {
        Some(body) => repair_v4a_envelope(&body),
        None => args_acc.to_owned(),
    }
}

/// `input` 缺失时尝试的常见别名 key(模型 schema drift)。仅当值是含
/// `*** Begin Patch` 的字符串才回收,避免误取无关字段。
const APPLY_PATCH_ALT_KEYS: &[&str] = &["patch", "diff", "apply_patch", "input_text", "content"];

/// 一行是否为 V4A 的 Begin/End sentinel —— **仅认列 0、无前缀的整行**(容忍尾部空白)。
///
/// 刻意**不**剥前导空白 / `+`:V4A 控制行恒在列 0 且无前缀,而正文行恒有前缀
/// (Add File `+`、context ` `、删除 `-`)。若把 `+*** End Patch` 也当 sentinel,
/// 遇到「文件内容恰含一行 `*** End Patch` + 流被截断在真正结尾之前」时,会把这条
/// **正文**行误当信封结尾、合成一个"完整"补丁 → Codex 静默执行被截断的 patch
/// (破坏性降级)。故前缀行一律按正文处理,交给 raw passthrough 让 Codex `parse_patch`
/// 暴露 incomplete。(见 #302 codex-connector P2;stray-`+` sentinel 的安全消歧留 MOC-57。)
fn v4a_sentinel(line: &str) -> Option<&'static str> {
    match line.trim_end() {
        "*** Begin Patch" => Some("*** Begin Patch"),
        "*** End Patch" => Some("*** End Patch"),
        _ => None,
    }
}

/// 非破坏性 V4A 信封修复(见 #302):丢掉首个 `*** Begin Patch` 之前、末个
/// `*** End Patch` 之后的杂散行(markdown fence / 解释性散文 / 空行)。
///
/// **绝不改动 Begin..End 之间的 `+`/`-`/context 正文行。** sentinel 仅认列 0、
/// 无前缀的整行(见 [`v4a_sentinel`]),故 `+`/空格前缀的正文行(哪怕内容恰为
/// `*** End Patch`)绝不会被误当信封边界。找不到顺序正确的 `Begin..End` 时原样
/// 返回,交给 Codex `parse_patch` 暴露真坏。
fn repair_v4a_envelope(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let begin = lines
        .iter()
        .position(|l| v4a_sentinel(l) == Some("*** Begin Patch"));
    let end = lines
        .iter()
        .rposition(|l| v4a_sentinel(l) == Some("*** End Patch"));
    let (Some(b), Some(e)) = (begin, end) else {
        return body.to_owned();
    };
    if b >= e {
        return body.to_owned();
    }
    // 已良构(首行恰 Begin、末行恰 End、无前后杂散)→ 原样返回,保字节精确
    // (happy path 不被规整,避免动 freeform 直透的常态输出)。
    if b == 0
        && e == lines.len() - 1
        && lines[b] == "*** Begin Patch"
        && lines[e] == "*** End Patch"
    {
        return body.to_owned();
    }
    let mut out = Vec::with_capacity(e - b + 1);
    out.push("*** Begin Patch".to_owned());
    out.extend(lines[b + 1..e].iter().map(|l| (*l).to_owned()));
    out.push("*** End Patch".to_owned());
    out.join("\n")
}

/// V4A 后验语法校验错误(行号 + 人类可读消息),供 close_tool_call 的
/// `tracing::warn!` 定位。
///
/// 来源:MOC-57(#321,作者 @Alpaca233114514)。
#[derive(Debug, Clone)]
pub(crate) struct V4aError {
    pub(crate) line: usize,
    pub(crate) message: String,
}

/// 检测 chat function args(JSON 形态)是否被流式截断:未闭合字符串(奇数个
/// 未转义 `"`)或 `{}` 不平衡。返回 `Some(detail)` 描述截断原因,`None` 表示
/// JSON 结构完整。**只看结构、不校验 V4A 语义**(那是 [`detect_v4a_truncation`]
/// 与 Codex CLI `parse_patch` 的事)。
///
/// 价值:#303 只靠 `interrupted` 这个模糊信号(流断 / [DONE] 缺失)判断 patch
/// 是否完整;本函数主动检测 JSON 结构截断,即使上游没断流也能发现 args 被切。
///
/// 来源:MOC-57(#321,作者 @Alpaca233114514);返回类型从原 `TruncationInfo`
/// struct 简化为 `Option<String>`(原 `level` 字段从未被读,去掉避免 dead_code)。
fn detect_json_truncation(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 单趟扫描:同时跟 in_string 状态 + 只统计**字符串外**的 `{}`。
    // [MOC-57 review #2/#3] 不能数整串 `{}` —— V4A patch 正文(在 JSON string
    // value 内)常含不平衡括号(如 `+fn main() {` 单独一行),那是字符串内容、
    // 无 JSON 结构意义。早期实现数整串会把这类合法 patch 误判 truncated → emit
    // incomplete 阻止执行。这里只数 string 外的 brace,得到真实 JSON 结构平衡度。
    let mut in_string = false;
    let mut escape = false;
    let mut depth: i64 = 0;
    for ch in trimmed.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    if in_string {
        return Some("unclosed JSON string (odd number of unescaped quotes)".to_owned());
    }
    if depth != 0 {
        return Some(format!(
            "unbalanced JSON structure (brace depth {depth} at end)"
        ));
    }
    None
}

/// 检测提取出的 V4A 文本是否被截断(缺少列 0 裸 `*** End Patch` sentinel)。
/// 返回 `Some(detail)` 表示截断,`None` 表示有正常结尾。复用 [`v4a_sentinel`]
/// 的安全判定(仅认列 0、无前缀整行),故正文里的 `+*** End Patch` 不会误判为结尾。
///
/// 来源:MOC-57(#321,作者 @Alpaca233114514)。
fn detect_v4a_truncation(v4a: &str) -> Option<String> {
    if v4a.is_empty() {
        return None;
    }
    let has_end = v4a
        .lines()
        .any(|l| v4a_sentinel(l) == Some("*** End Patch"));
    if !has_end {
        return Some("missing *** End Patch sentinel — V4A patch truncated".to_owned());
    }
    None
}

/// V4A 后验语法校验:检查首行 `*** Begin Patch` / 末行 `*** End Patch` sentinel +
/// 列 0 控制标记(`*** Add/Update/Delete File:` / `*** Move to:` / `*** End of File` /
/// `@@` hunk header)+ 正文行前缀(`+` / `-` / 空格)合法性。**不校验语义正确性**
/// (文件存在性、行匹配等留给 Codex CLI `parse_patch`)。仅在 args 未截断时调用。
///
/// **关键(MOC-57 review)**:先按**原始行首字符**判正文前缀再 fall through 到 `***`
/// 判定 —— 正文行恒有 `+`/`-`/` ` 前缀,据此先分流,避免空格前缀的 ` *** xxx` context
/// 行被 trim 后误当 operation header。
///
/// 来源:MOC-57(#321,作者 @Alpaca233114514);bug 修复见 PR #322 review。
pub(crate) fn validate_v4a_syntax(input: &str) -> Result<(), V4aError> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.is_empty() {
        return Err(V4aError {
            line: 1,
            message: "empty V4A patch".to_owned(),
        });
    }
    if v4a_sentinel(lines[0]) != Some("*** Begin Patch") {
        return Err(V4aError {
            line: 1,
            message: format!(
                "expected '*** Begin Patch' on line 1, got '{}'",
                lines[0].chars().take(80).collect::<String>()
            ),
        });
    }
    let last = lines.len();
    if v4a_sentinel(lines[last - 1]) != Some("*** End Patch") {
        return Err(V4aError {
            line: last,
            message: format!(
                "expected '*** End Patch' on line {last}, got '{}'",
                lines[last - 1].chars().take(80).collect::<String>()
            ),
        });
    }
    // V4A 列 0 控制标记全集(对齐本仓 apply_patch prompt,见 request.rs
    // 的 V4A 格式说明)。[MOC-57 review #6] 必须含 `*** End of File`,否则用
    // EOF marker 的合法 rename/update patch 被误判。
    let valid_markers: &[&str] = &[
        "*** Add File:",
        "*** Update File:",
        "*** Delete File:",
        "*** Move to:",
        "*** End of File",
    ];
    for (i, line) in lines.iter().enumerate().skip(1).take(last - 2) {
        // [MOC-57 review #4/#5] **先按原始行首字符判正文前缀**,再 fall through
        // 到 `***` header 判定。绝不能先 trim 再判 `***`:正文行 ` *** rule`
        // (空格前缀的 context 行,内容恰以 `***` 开头,如 markdown 分隔线)trim
        // 后会变成 `*** rule` 被误当 operation header 拒掉。V4A 控制行恒在列 0
        // 无前缀;正文行恒有 `+`/`-`/` ` 前缀,据此先分流。
        match line.chars().next() {
            // 正文行(含内容以 `***` 开头的)/ 空 context 行 → 合法,跳过
            Some('+') | Some('-') | Some(' ') => continue,
            None => continue, // 空行
            _ => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // `@@` / `@@ <context>` 是 Update File hunk header(列 0,无 +/-/空格前缀),合法
        if trimmed.starts_with("@@") {
            continue;
        }
        // 列 0 的 `*** End Patch`(理论上是末行,已被上面 last-1 排除;防御性放行)
        if trimmed == "*** End Patch" {
            continue;
        }
        if trimmed.starts_with("***") {
            if !valid_markers.iter().any(|h| trimmed.starts_with(h)) {
                return Err(V4aError {
                    line: i + 1,
                    message: format!(
                        "unrecognized V4A operation on line {}: '{}'",
                        i + 1,
                        trimmed.chars().take(80).collect::<String>()
                    ),
                });
            }
        } else {
            // 列 0、非控制标记、又无 +/-/空格 前缀 → 真非法
            return Err(V4aError {
                line: i + 1,
                message: format!(
                    "line {} missing V4A prefix (expected '+', '-', ' ', '@@', or '*** ' marker): '{}'",
                    i + 1,
                    line.chars().take(80).collect::<String>()
                ),
            });
        }
    }
    Ok(())
}

/// emit apply_patch 的 `custom_tool_call` wire(从 `close_tool_call` 抽出,
/// interrupted / completed 两条路径共用)。`interrupted=true` 时只 emit 一条
/// `output_item.done` 带 `status="incomplete"`(skip input.delta/done,防 Codex CLI
/// 执行 partial/invalid patch),且**不写** ToolCallCache(避免下轮引用到 incomplete
/// 上下文);`interrupted=false` 时 emit 完整 delta/done + completed + 写 cache。
///
/// 来源:MOC-57(#321,作者 @Alpaca233114514)的重构,让截断 / 语法错误 / 流中断
/// 三种 incomplete 路径复用同一 emit 逻辑。
#[allow(clippy::too_many_arguments)]
fn emit_apply_patch_output(
    fc_id: &str,
    call_id: &str,
    name: &str,
    input: &str,
    args_acc: &str,
    output_index: u32,
    interrupted: bool,
    out: &mut Vec<u8>,
    sequence_number: &mut u64,
    tool_calls: &mut BTreeMap<u32, PendingToolCall>,
    openai_index: u32,
) {
    if interrupted {
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
            sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }),
        );
        if let Some(pending) = tool_calls.get_mut(&openai_index) {
            pending.interrupted_during_close = true;
        }
        return;
    }
    emit_event(
        out,
        sequence_number,
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
        sequence_number,
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
        sequence_number,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    );
    global_tool_call_cache().save(
        call_id,
        ToolCallEntry {
            name: name.to_owned(),
            arguments: args_acc.to_owned(),
        },
    );
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

        // Devin pre-merge review BUG_pr-review-job-9600e18f8e4c4a90 防御回归:
        // envelope.output[] 终态 item.status 必须跟流式 `response.output_item.done`
        // 一致(都是 `incomplete`);若 envelope 写 `completed` 而流式 done 写
        // `incomplete`,严格客户端读 envelope 会误执行 partial V4A patch
        // (destructive)。
        let final_output = completed.1["response"]["output"].as_array().unwrap();
        let final_apply_patch_item = final_output
            .iter()
            .find(|item| item.get("type").and_then(|v| v.as_str()) == Some("custom_tool_call"))
            .expect("envelope.output 必须含 apply_patch custom_tool_call item");
        assert_eq!(
            final_apply_patch_item["status"], "incomplete",
            "interrupted apply_patch envelope.output[] item.status 必须跟流式 done event 一致(都是 incomplete)"
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
    fn extract_apply_patch_alt_key_patch_recovered() {
        // 真机实测 schema drift:模型用 "patch" 而非 "input"(#302)
        let args = r#"{"patch": "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch"}"#;
        assert_eq!(
            extract_apply_patch_input(args),
            "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch"
        );
    }

    #[test]
    fn extract_apply_patch_markdown_fence_stripped() {
        let args = "```diff\n*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n```";
        assert_eq!(
            extract_apply_patch_input(args),
            "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch"
        );
    }

    #[test]
    fn extract_apply_patch_plus_prefixed_end_not_synthesized() {
        // 安全性(#302 codex-connector P2):`+*** End Patch` 可能是被截断的正文行,
        // 不得当作信封结尾合成"完整"补丁。无列 0 裸 End → 原样透传,让 Codex 暴露 incomplete。
        let body = "*** Begin Patch\n*** Add File: a.txt\n+x\n+*** End Patch";
        let args = serde_json::json!({ "input": body }).to_string();
        assert_eq!(extract_apply_patch_input(&args), body);
    }

    #[test]
    fn extract_apply_patch_real_bare_end_after_plus_end_content_preserved() {
        // 正文行恰为 `+*** End Patch`,其后有真正的列 0 裸 `*** End Patch`:
        // 真 End 被识别,`+*** End Patch` 正文行完整保留(不被误当结尾截断)。
        let body =
            "*** Begin Patch\n*** Add File: doc.md\n+intro\n+*** End Patch\n+outro\n*** End Patch";
        let args = serde_json::json!({ "input": body }).to_string();
        assert_eq!(extract_apply_patch_input(&args), body);
    }

    #[test]
    fn extract_apply_patch_wellformed_preserved_byte_exact() {
        // 良构 input 字节精确保留,不被规整
        let patch = "*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch";
        let args = serde_json::json!({ "input": patch }).to_string();
        assert_eq!(extract_apply_patch_input(&args), patch);
        // 裸 V4A(含尾随 \n)亦原样透传
        let bare = "*** Begin Patch\nfoo\n*** End Patch\n";
        assert_eq!(extract_apply_patch_input(bare), bare);
    }

    #[test]
    fn extract_apply_patch_content_line_with_end_patch_substring_not_truncated() {
        // 正文行含 "*** End Patch" 子串(非整行 sentinel)不得被误判为结尾截断
        let patch =
            "*** Begin Patch\n*** Add File: doc.md\n+see *** End Patch marker\n+more\n*** End Patch";
        let args = serde_json::json!({ "input": patch }).to_string();
        assert_eq!(extract_apply_patch_input(&args), patch);
    }

    #[test]
    fn extract_apply_patch_truncated_no_end_raw_passthrough() {
        // 截断(无 End sentinel)→ 不静默修补/截断正文,原样暴露给 parse_patch
        let args = r#"{"input": "*** Begin Patch\n*** Add File: a.txt\n+partial"}"#;
        assert_eq!(
            extract_apply_patch_input(args),
            "*** Begin Patch\n*** Add File: a.txt\n+partial"
        );
    }

    // ── [MOC-57] 截断检测 + V4A 后验校验(移植自 #321 @Alpaca233114514)──

    #[test]
    fn detect_json_truncation_unclosed_string() {
        let s = r#"{"input": "*** Begin Patch"#;
        assert!(detect_json_truncation(s).is_some());
    }

    #[test]
    fn detect_json_truncation_unbalanced_brace() {
        let s = r#"{"input": "ok""#;
        assert!(detect_json_truncation(s).is_some());
    }

    #[test]
    fn detect_json_truncation_valid_json_passes() {
        let s = r#"{"input": "*** Begin Patch\n*** End Patch"}"#;
        assert!(detect_json_truncation(s).is_none());
    }

    #[test]
    fn detect_json_truncation_escaped_quote_not_miscounted() {
        // 转义引号不应被当成字符串边界 → 结构完整
        let s = r#"{"input": "say \"hi\""}"#;
        assert!(detect_json_truncation(s).is_none());
    }

    #[test]
    fn detect_v4a_truncation_missing_end() {
        let v4a = "*** Begin Patch\n*** Add File: a.txt\n+x";
        assert!(detect_v4a_truncation(v4a).is_some());
    }

    #[test]
    fn detect_v4a_truncation_complete_passes() {
        let v4a = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch";
        assert!(detect_v4a_truncation(v4a).is_none());
    }

    #[test]
    fn validate_v4a_syntax_valid_patch_passes() {
        let v4a = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_ok());
    }

    #[test]
    fn validate_v4a_syntax_missing_begin_rejected() {
        let v4a = "*** Add File: a.txt\n+hello\n*** End Patch";
        let err = validate_v4a_syntax(v4a).unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn validate_v4a_syntax_missing_end_rejected() {
        let v4a = "*** Begin Patch\n*** Add File: a.txt\n+hello";
        assert!(validate_v4a_syntax(v4a).is_err());
    }

    #[test]
    fn validate_v4a_syntax_unknown_operation_rejected() {
        let v4a = "*** Begin Patch\n*** Frobnicate File: a.txt\n+x\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_err());
    }

    #[test]
    fn validate_v4a_syntax_missing_line_prefix_rejected() {
        // 正文行缺 +/-/空格 前缀
        let v4a = "*** Begin Patch\n*** Update File: a.txt\nbad line no prefix\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_err());
    }

    #[test]
    fn validate_v4a_syntax_hunk_marker_allowed() {
        // `@@` / `@@ <context>` 是 Update File 的 hunk 标记,必须当合法(否则
        // 所有真实 Update File patch 都被误判截断)。
        let v4a = "*** Begin Patch\n*** Update File: foo.py\n@@\n-old\n+new\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_ok());
        let v4a_ctx =
            "*** Begin Patch\n*** Update File: foo.py\n@@ def main():\n ctx\n-old\n+new\n*** End Patch";
        assert!(validate_v4a_syntax(v4a_ctx).is_ok());
    }

    // ── [MOC-57 PR #322 review] 4 个 bot 揪出的误判合法 patch bug 的回归测试 ──

    #[test]
    fn detect_json_truncation_unbalanced_brace_inside_string_is_valid() {
        // review #2/#3:patch 正文(JSON string value 内)含不平衡 `{` 不是 JSON 截断。
        // 合法完整 JSON,但 patch 内容有 `+fn main() {` 单边括号。
        let args = r#"{"input": "*** Begin Patch\n*** Add File: a.rs\n+fn main() {\n+    todo!()\n*** End Patch"}"#;
        assert!(
            detect_json_truncation(args).is_none(),
            "字符串内的不平衡括号不该被判 JSON 截断"
        );
    }

    #[test]
    fn detect_json_truncation_real_structural_truncation_still_caught() {
        // 反向:真 JSON 结构截断(外层 `{` 没闭合)仍要抓到
        let args = r#"{"input": "*** Begin Patch\n*** End Patch""#; // 缺末尾 }
        assert!(detect_json_truncation(args).is_some());
    }

    #[test]
    fn validate_v4a_syntax_context_line_starting_with_stars_allowed() {
        // review #4/#5:context 行(空格前缀)内容恰以 `***` 开头(markdown 分隔线等)
        // 不该被 trim 后误当 operation header。
        let v4a =
            "*** Begin Patch\n*** Update File: doc.md\n@@\n *** a markdown rule\n+new line\n*** End Patch";
        assert!(
            validate_v4a_syntax(v4a).is_ok(),
            "空格前缀的 ` *** ...` context 行应合法"
        );
        // `+` 前缀的 added 行内容以 `***` 开头同理合法
        let v4a_add = "*** Begin Patch\n*** Add File: doc.md\n+*** heading\n*** End Patch";
        assert!(validate_v4a_syntax(v4a_add).is_ok());
    }

    #[test]
    fn validate_v4a_syntax_end_of_file_marker_allowed() {
        // review #6:`*** End of File` 是本仓 apply_patch prompt 文档化的合法 marker。
        let v4a =
            "*** Begin Patch\n*** Update File: a.txt\n@@\n+last line\n*** End of File\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_ok());
    }

    #[test]
    fn validate_v4a_syntax_truly_invalid_operation_still_rejected() {
        // 反向:真未知 operation header(列 0、`***` 前缀、非合法 marker)仍要拒
        let v4a = "*** Begin Patch\n*** Frobnicate File: a.txt\n+x\n*** End Patch";
        assert!(validate_v4a_syntax(v4a).is_err());
    }

    #[test]
    fn apply_patch_truncated_args_emit_incomplete_status() {
        // 端到端:apply_patch 的 args JSON 被截断(未闭合)→ close 时应 emit
        // custom_tool_call status=incomplete,不走 completed(防 Codex 执行 partial)。
        let chunk = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_t\",\"type\":\"function\",\"function\":{\"name\":\"apply_patch\",\"arguments\":\"{\\\"input\\\": \\\"*** Begin Patch\\\\n+partial\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let mut c = fixed();
        let mut out = c.feed(chunk);
        out.extend(c.finish());
        let events = parse_emitted(&out);
        let done = events
            .iter()
            .find(|(n, v)| {
                n == "response.output_item.done"
                    && v["item"]["type"] == "custom_tool_call"
                    && v["item"]["name"] == "apply_patch"
            })
            .expect("apply_patch custom_tool_call done event");
        assert_eq!(
            done.1["item"]["status"], "incomplete",
            "截断的 apply_patch 必须 emit incomplete,实际事件: {:?}",
            done.1
        );
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
}
