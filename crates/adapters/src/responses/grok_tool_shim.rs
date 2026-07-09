//! [MOC-301 / MOC-304] grok passthrough 响应侧 tool-call shim.
//!
//! grok 是 Responses passthrough(`map_response` 成功流本是 1:1 直透),但请求侧把 apply_patch
//! (`custom`)/ `tool_search` 转成了 `function`(见 `grok_build.rs` leg1),grok 回的是 Responses
//! `function_call`。而 Codex 的 apply_patch handler 硬要 `custom_tool_call`(`ToolPayload::Custom`)、
//! tool_search 走 `tool_search_call`(`ToolPayload::ToolSearch`)—— 收到 `function_call` 会 abort /
//! 无法路由。本状态机**拦截这两类 function_call、把 wire 重打包**回 Codex 认的类型;其余事件原样透传。
//!
//! ## 关键决策
//! - **非流式**(apply_patch / tool_search 的 args 累积到 done 再一次性出,与 `converter.rs` chat 路径
//!   一致;客户端看不到逐字 diff。真流式落 followup)。
//! - **sequence_number 全程重新连续编号**:suppress 掉被拦截项的 `function_call_arguments.delta`
//!   会在原 grok 序号里留 gap,严格 Codex 客户端可能拒 → 由 `emit_event` 统一用本 shim 的计数器覆写
//!   每个事件的 `sequence_number`(见 `core::events::emit_sse_event`)。
//! - **envelope 一致性**:终帧 `response.completed.output[]` 里同一 item 也同步重写(否则严格客户端读
//!   envelope 会误判,甚至在 partial V4A 上跑 destructive apply)。
//! - **DRY**:复用 `converter.rs` 的 apply_patch preflight（`extract_apply_patch_input` / truncation /
//!   validation）+ `apply_patch_preflight::optimize_patch` + wire 判定 helper，不镜像逻辑。
//!
//! 只对 grok(passthrough)挂;非 grok passthrough 仍严格 1:1(见 `mapper::responses::map_response`)。

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use serde_json::{json, Value};

use crate::types::ByteStream;

use super::apply_patch_preflight;
use super::converter::{
    detect_json_truncation, detect_v4a_truncation, emit_event, extract_apply_patch_input,
    is_tool_search_tool_name, normalize_tool_search_arguments, validate_v4a_syntax,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// 从 `custom` lower 的工具:`apply_patch`(走 V4A preflight)或其他 custom(只取裸 input)。
    Custom {
        apply_patch: bool,
    },
    ToolSearch,
}

/// 一个被拦截的 tool-call item 的累积态(open 到 done 之间)。
struct Pending {
    kind: ToolKind,
    call_id: String,
    item_id: String,
    name: String,
    /// function_call 的 arguments 累积(标准形态 `{"input":"<V4A>"}` / `{"query":"..."}`)。
    args_acc: String,
}

/// grok passthrough 响应侧 SSE 转换状态机。`push(&[u8]) -> Vec<u8>` + `finish() -> Vec<u8>`,内部
/// buffer 半帧(与 `converter.rs` / gemini_native / anthropic_messages 同形态)。
pub(crate) struct GrokToolCallShim {
    /// 半帧缓冲(未遇 `\n\n` 的尾部留到下次 push)。
    buffer: Vec<u8>,
    /// 重新编号计数器(覆写每个 emit 事件的 sequence_number)。
    seq: u64,
    /// 被拦截项:output_index -> Pending。
    items: HashMap<u64, Pending>,
    /// item_id -> output_index(delta/done 事件携带 item_id,需反查 output_index)。
    id_to_index: HashMap<String, u64>,
    /// apply_patch preflight 的 cwd(路径相关修复用;无则跳过 cwd-dependent 修复,仍可用)。
    cwd: Option<String>,
    /// [review thread0] 只 repack **真被 lower** 的 custom 工具(名 → 是否 apply_patch),避免误 repack
    /// 恰好叫 apply_patch 的普通 function/MCP 工具。
    custom_lowered: HashMap<String, bool>,
    /// [review thread0] tool_search 是否被 lower(gate `tool_search` 名的 repack)。
    tool_search_lowered: bool,
    /// [review thread1] 内层 function name → namespace:grok 调发现的 MCP 工具回的普通 function_call
    /// 需补 `namespace` 字段,Codex 才能 dispatch(对齐 chat converter 的 lookup_namespace_for)。
    namespace_map: HashMap<String, String>,
}

impl GrokToolCallShim {
    pub(crate) fn new(
        cwd: Option<String>,
        ctx: crate::mapper::grok_build::GrokShimContext,
    ) -> Self {
        Self {
            buffer: Vec::new(),
            seq: 0,
            items: HashMap::new(),
            id_to_index: HashMap::new(),
            cwd,
            custom_lowered: ctx.custom_lowered,
            tool_search_lowered: ctx.tool_search_lowered,
            namespace_map: ctx.namespace_map,
        }
    }

    /// [review thread2] 非流式(stream:false)grok 成功响应是单 JSON `{output:[...]}`(非 SSE),
    /// SSE shim 的 `push`/`finish` 不经它。直接改写顶层 `output[]` 里的 apply_patch/tool_search/
    /// namespace function_call(与 SSE envelope 同一套 `rewrite_envelope_item`)。
    pub(crate) fn rewrite_json_response(&self, body: &mut Value) {
        if let Some(output) = body.get_mut("output").and_then(|o| o.as_array_mut()) {
            for item in output.iter_mut() {
                self.rewrite_envelope_item(item);
            }
        }
    }

    /// [review thread1] 透传的 function_call item 若是 namespace/发现工具,补 `namespace` 字段。
    fn add_namespace_if_mapped(&self, data: &mut Value) {
        let Some(item) = data.get_mut("item") else {
            return;
        };
        if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
            return;
        }
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            return;
        };
        if let Some(ns) = self.namespace_map.get(name).cloned() {
            if let Some(o) = item.as_object_mut() {
                o.insert("namespace".into(), Value::String(ns));
            }
        }
    }

    /// 喂上游 chunk,返回改写后的 SSE 字节(可能为空:半帧未满 / 被 suppress 的 delta)。
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_double_newline(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..pos + 2).collect();
            self.process_frame(&frame, &mut out);
        }
        out
    }

    /// 流结束:flush 尾部残帧;任何仍 open 的被拦截项 = 流被中途切断 → emit incomplete
    /// (防 Codex 把半截 patch 当完整执行)。
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            self.process_frame(&frame, &mut out);
        }
        let mut leftovers: Vec<(u64, Pending)> = self.items.drain().collect();
        leftovers.sort_by_key(|(idx, _)| *idx);
        self.id_to_index.clear();
        for (output_index, p) in leftovers {
            self.emit_tool_call_done(output_index, &p, true, &mut out);
        }
        out
    }

    fn process_frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let Some(data) = frame_data_json(frame) else {
            // 非 JSON data(SSE 注释 / 空帧 / `[DONE]` 等)→ 原样透传,不参与重编号。
            out.extend_from_slice(frame);
            return;
        };
        let event_name = data
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        match event_name.as_str() {
            "response.output_item.added" => self.on_item_added(data, out),
            "response.function_call_arguments.delta" => self.on_args_delta(data, out),
            "response.function_call_arguments.done" => self.on_args_done(data, out),
            "response.output_item.done" => self.on_item_done(data, out),
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.on_terminal(&event_name, data, out)
            }
            // 其余事件(created / in_progress / reasoning* / output_text* / …)原样透传 + 重编号。
            _ => emit_event(out, &mut self.seq, &event_name, data),
        }
    }

    fn on_item_added(&mut self, data: Value, out: &mut Vec<u8>) {
        let output_index = data
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let item = data.get("item").cloned().unwrap_or(Value::Null);
        let is_fc = item.get("type").and_then(|v| v.as_str()) == Some("function_call");
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        // [review thread0] 只 repack **真被 lower** 的工具(名在 custom_lowered / tool_search_lowered),
        // 不再纯按名字 —— 避免误 repack 恰好叫 apply_patch/tool_search 的普通 function/MCP 工具。
        let kind = if !is_fc {
            None
        } else if let Some(&apply_patch) = self.custom_lowered.get(name) {
            Some(ToolKind::Custom { apply_patch })
        } else if self.tool_search_lowered && is_tool_search_tool_name(name) {
            Some(ToolKind::ToolSearch)
        } else {
            None
        };
        let Some(kind) = kind else {
            // 透传 function_call:若是 namespace/发现工具,补 `namespace`(Codex dispatch MCP 需要)。
            let mut data = data;
            self.add_namespace_if_mapped(&mut data);
            emit_event(out, &mut self.seq, "response.output_item.added", data);
            return;
        };
        let call_id = item
            .get("call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let args0 = item
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let new_item = match kind {
            ToolKind::Custom { .. } => json!({
                "type": "custom_tool_call", "id": item_id, "call_id": call_id,
                "name": name, "input": "", "status": "in_progress",
            }),
            ToolKind::ToolSearch => json!({
                "type": "tool_search_call", "id": item_id, "call_id": call_id,
                "execution": "client", "arguments": {}, "status": "in_progress",
            }),
        };
        if !item_id.is_empty() {
            self.id_to_index.insert(item_id.clone(), output_index);
        }
        self.items.insert(
            output_index,
            Pending {
                kind,
                call_id,
                item_id,
                name: name.to_owned(),
                args_acc: args0,
            },
        );
        emit_event(
            out,
            &mut self.seq,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": new_item,
            }),
        );
    }

    fn on_args_delta(&mut self, data: Value, out: &mut Vec<u8>) {
        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(&idx) = self.id_to_index.get(item_id) {
            if let Some(p) = self.items.get_mut(&idx) {
                if let Some(delta) = data.get("delta").and_then(|v| v.as_str()) {
                    p.args_acc.push_str(delta);
                }
                return; // suppress(非流式:累积不转发,避免 custom_tool_call open + function delta 混排)
            }
        }
        emit_event(
            out,
            &mut self.seq,
            "response.function_call_arguments.delta",
            data,
        );
    }

    fn on_args_done(&mut self, data: Value, out: &mut Vec<u8>) {
        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(&idx) = self.id_to_index.get(item_id) {
            if let Some(p) = self.items.get_mut(&idx) {
                // done 携带完整 arguments,作权威值(delta 累积可能因 chunk 边界不全)。
                if let Some(args) = data.get("arguments").and_then(|v| v.as_str()) {
                    if !args.is_empty() {
                        p.args_acc = args.to_owned();
                    }
                }
                return; // suppress
            }
        }
        emit_event(
            out,
            &mut self.seq,
            "response.function_call_arguments.done",
            data,
        );
    }

    fn on_item_done(&mut self, data: Value, out: &mut Vec<u8>) {
        let output_index = data
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(p) = self.items.remove(&output_index) {
            self.id_to_index.remove(&p.item_id);
            self.emit_tool_call_done(output_index, &p, false, out);
        } else {
            // 透传 function_call:补 namespace(同 added)。
            let mut data = data;
            self.add_namespace_if_mapped(&mut data);
            emit_event(out, &mut self.seq, "response.output_item.done", data);
        }
    }

    /// 把一个被拦截项的 done 重打包成 Codex 认的 wire(apply_patch → custom_tool_call [+input.delta/done];
    /// tool_search → tool_search_call)。`interrupted` = 流中途切断,emit incomplete。
    fn emit_tool_call_done(
        &mut self,
        output_index: u64,
        p: &Pending,
        interrupted: bool,
        out: &mut Vec<u8>,
    ) {
        match p.kind {
            ToolKind::Custom { apply_patch } => {
                let (input, incomplete) = if apply_patch {
                    finalize_apply_patch(&p.args_acc, self.cwd.as_deref(), interrupted)
                } else {
                    // [review nL] 非 apply_patch custom:裸 input(args.input),**不**跑
                    // extract_apply_patch_input(它带 V4A 信封修复 / patch-key 别名,会污染任意自由文本
                    // 输入,如恰好含 *** Begin Patch)。用通用提取器。
                    (generic_custom_input(&p.args_acc), interrupted)
                };
                if incomplete {
                    let item = json!({
                        "type": "custom_tool_call", "id": p.item_id, "call_id": p.call_id,
                        "name": p.name, "input": input, "status": "incomplete",
                    });
                    emit_event(
                        out,
                        &mut self.seq,
                        "response.output_item.done",
                        json!({ "type": "response.output_item.done", "output_index": output_index, "item": item }),
                    );
                    return;
                }
                emit_event(
                    out,
                    &mut self.seq,
                    "response.custom_tool_call_input.delta",
                    json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": p.item_id, "output_index": output_index,
                        "call_id": p.call_id, "delta": input,
                    }),
                );
                emit_event(
                    out,
                    &mut self.seq,
                    "response.custom_tool_call_input.done",
                    json!({
                        "type": "response.custom_tool_call_input.done",
                        "item_id": p.item_id, "output_index": output_index,
                        "call_id": p.call_id, "input": input,
                    }),
                );
                let item = json!({
                    "type": "custom_tool_call", "id": p.item_id, "call_id": p.call_id,
                    "name": p.name, "input": input, "status": "completed",
                });
                emit_event(
                    out,
                    &mut self.seq,
                    "response.output_item.done",
                    json!({ "type": "response.output_item.done", "output_index": output_index, "item": item }),
                );
            }
            ToolKind::ToolSearch => {
                let arguments = parse_tool_search_arguments(&p.args_acc);
                let status = if interrupted {
                    "incomplete"
                } else {
                    "completed"
                };
                let item = json!({
                    "type": "tool_search_call", "id": p.item_id, "call_id": p.call_id,
                    "execution": "client", "arguments": arguments, "status": status,
                });
                emit_event(
                    out,
                    &mut self.seq,
                    "response.output_item.done",
                    json!({ "type": "response.output_item.done", "output_index": output_index, "item": item }),
                );
            }
        }
    }

    /// 终帧(completed / incomplete / failed):同步重写 `response.output[]` 里的 apply_patch /
    /// tool_search function_call → custom_tool_call / tool_search_call(与流式 done 一致)。
    fn on_terminal(&mut self, event_name: &str, mut data: Value, out: &mut Vec<u8>) {
        // [review Pit5c] 若终帧(completed/incomplete/failed)到来时还有拦截项 open(grok 中途截断),
        // **先**把它们 emit 成 incomplete 的 output_item.done —— 必须在终帧之前,否则 tool-call 完成
        // 事件排在终帧之后、且 envelope 会把它当完整重写,严格客户端会看到「截断响应上却有完成态 tool
        // call」。同时把这些 item 在 envelope output[] 里也强制 incomplete(rewrite 按 args 算的 status
        // 可能是 completed,与流式 done 的 incomplete 矛盾)。
        let mut interrupted: std::collections::HashSet<String> = std::collections::HashSet::new();
        if !self.items.is_empty() {
            let mut leftovers: Vec<(u64, Pending)> = self.items.drain().collect();
            leftovers.sort_by_key(|(idx, _)| *idx);
            self.id_to_index.clear();
            for (output_index, p) in leftovers {
                interrupted.insert(p.item_id.clone());
                self.emit_tool_call_done(output_index, &p, true, out);
            }
        }
        if let Some(output) = data
            .get_mut("response")
            .and_then(|r| r.get_mut("output"))
            .and_then(|o| o.as_array_mut())
        {
            for item in output.iter_mut() {
                let id = item.get("id").and_then(|v| v.as_str()).map(str::to_owned);
                self.rewrite_envelope_item(item);
                if let Some(id) = id {
                    if interrupted.contains(&id) {
                        if let Some(o) = item.as_object_mut() {
                            o.insert("status".into(), Value::String("incomplete".into()));
                        }
                    }
                }
            }
        }
        emit_event(out, &mut self.seq, event_name, data);
    }

    /// envelope `output[]` 的单个 item:真被 lower 的 apply_patch/tool_search function_call →
    /// custom_tool_call/tool_search_call(input 从 arguments 重新 finalize,与流式 done 一致);其余
    /// namespace 工具 function_call 补 `namespace`(与透传 item 一致)。
    fn rewrite_envelope_item(&self, item: &mut Value) {
        if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
            return;
        }
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let call_id = item.get("call_id").cloned().unwrap_or(Value::Null);
        let id = item.get("id").cloned().unwrap_or(Value::Null);
        let args = item
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if let Some(&apply_patch) = self.custom_lowered.get(&name) {
            let (input, incomplete) = if apply_patch {
                finalize_apply_patch(&args, self.cwd.as_deref(), false)
            } else {
                (generic_custom_input(&args), false)
            };
            *item = json!({
                "type": "custom_tool_call", "id": id, "call_id": call_id, "name": name,
                "input": input, "status": if incomplete { "incomplete" } else { "completed" },
            });
        } else if self.tool_search_lowered && is_tool_search_tool_name(&name) {
            let arguments = parse_tool_search_arguments(&args);
            *item = json!({
                "type": "tool_search_call", "id": id, "call_id": call_id,
                "execution": "client", "arguments": arguments, "status": "completed",
            });
        } else if let Some(ns) = self.namespace_map.get(&name).cloned() {
            // 透传的 namespace/发现工具:补 namespace(Codex dispatch MCP 需要)。
            if let Some(o) = item.as_object_mut() {
                o.insert("namespace".into(), Value::String(ns));
            }
        }
    }
}

/// apply_patch args(`{"input":"<V4A>"}`)→ 最终 V4A input + 是否 incomplete(截断 / 语法错 /
/// interrupted)。复用 converter 的提取 + preflight + 校验(与 chat 路径同一套逻辑,DRY)。
fn finalize_apply_patch(args_acc: &str, cwd: Option<&str>, interrupted: bool) -> (String, bool) {
    let input = extract_apply_patch_input(args_acc);
    let json_trunc = detect_json_truncation(args_acc);
    let (input, _repairs) =
        apply_patch_preflight::optimize_patch(&input, cwd, json_trunc.is_none());
    let v4a_trunc = detect_v4a_truncation(&input);
    let is_trunc = json_trunc.is_some() || v4a_trunc.is_some();
    let v4a_invalid = if is_trunc {
        false
    } else {
        validate_v4a_syntax(&input).is_err()
    };
    let incomplete = interrupted || is_trunc || v4a_invalid;
    (input, incomplete)
}

/// [review nL] 非 apply_patch custom 工具的 input 提取:只取 args JSON 的 `input` 字段(与请求侧
/// `{input:string}` lowering 对齐),**不做** V4A 信封修复 / patch-key 别名(那是 apply_patch 专用,
/// 会污染任意自由文本)。parse 失败 / 无 input 字段 → 原样返回 args(不吞、可观测)。
fn generic_custom_input(args_acc: &str) -> String {
    serde_json::from_str::<Value>(args_acc)
        .ok()
        .and_then(|v| v.get("input").and_then(|i| i.as_str()).map(str::to_owned))
        .unwrap_or_else(|| args_acc.to_owned())
}

/// tool_search args 字符串 → Codex `ToolSearchCall.arguments` 期望的 JSON object(parse 失败 fallback
/// `{"raw": ...}`,让 Codex 端可 log 模型意图而非静默 drop)。
fn parse_tool_search_arguments(args_acc: &str) -> Value {
    let v: Value =
        serde_json::from_str(args_acc).unwrap_or_else(|_| json!({ "raw": args_acc.to_owned() }));
    normalize_tool_search_arguments(v)
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// 从一帧 SSE 里抽 `data:` 行并 parse JSON。非 JSON / 无 data 行 → None(caller 原样透传)。
fn frame_data_json(frame: &[u8]) -> Option<Value> {
    let s = std::str::from_utf8(frame).ok()?;
    for line in s.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            return serde_json::from_str(rest.trim()).ok();
        }
    }
    None
}

/// 把 [`GrokToolCallShim`] 包成 `ByteStream`:poll 上游 → `shim.push` 出改写字节;上游 EOF →
/// `shim.finish` flush 尾部。仅 grok passthrough 成功流套(见 `mapper::responses::map_response`)。
pub(crate) struct GrokShimStream {
    inner: ByteStream,
    shim: GrokToolCallShim,
    /// 上游 EOF 已见 + finish() 已 flush → 下次 poll 返回 None。
    finished: bool,
}

impl GrokShimStream {
    pub(crate) fn new(
        inner: ByteStream,
        cwd: Option<String>,
        ctx: crate::mapper::grok_build::GrokShimContext,
    ) -> Self {
        Self {
            inner,
            shim: GrokToolCallShim::new(cwd, ctx),
            finished: false,
        }
    }
}

impl Stream for GrokShimStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        loop {
            if this.finished {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let out = this.shim.push(&chunk);
                    if out.is_empty() {
                        // 本 chunk 只含被 suppress 的 delta / 半帧 → 无输出,继续 poll 上游。
                        continue;
                    }
                    return Poll::Ready(Some(Ok(Bytes::from(out))));
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    // 上游 EOF:flush finish()(残帧 + 未闭合项 incomplete),再 EOF。
                    this.finished = true;
                    let tail = this.shim.finish();
                    if tail.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(Bytes::from(tail))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析 shim 输出为 (event_name, data) 帧列表。
    fn parse_frames(bytes: &[u8]) -> Vec<(String, Value)> {
        let s = std::str::from_utf8(bytes).unwrap();
        let mut frames = Vec::new();
        for frame in s.split("\n\n") {
            if frame.trim().is_empty() {
                continue;
            }
            let mut event = String::new();
            let mut data = None;
            for line in frame.split('\n') {
                if let Some(e) = line.strip_prefix("event: ") {
                    event = e.to_owned();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = serde_json::from_str::<Value>(d).ok();
                }
            }
            if let Some(d) = data {
                frames.push((event, d));
            }
        }
        frames
    }

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn ctx(
        custom: &[(&str, bool)],
        tool_search: bool,
        ns: &[(&str, &str)],
    ) -> crate::mapper::grok_build::GrokShimContext {
        crate::mapper::grok_build::GrokShimContext {
            custom_lowered: custom.iter().map(|(n, ap)| (n.to_string(), *ap)).collect(),
            tool_search_lowered: tool_search,
            namespace_map: ns
                .iter()
                .map(|(n, s)| (n.to_string(), s.to_string()))
                .collect(),
        }
    }

    fn run_ctx(
        input: &str,
        ctx: crate::mapper::grok_build::GrokShimContext,
    ) -> Vec<(String, Value)> {
        let mut shim = GrokToolCallShim::new(None, ctx);
        let mut out = shim.push(input.as_bytes());
        out.extend(shim.finish());
        parse_frames(&out)
    }

    /// 默认 context:apply_patch(custom)+ tool_search 都被 lower(覆盖多数测试)。
    fn run(input: &str) -> Vec<(String, Value)> {
        run_ctx(input, ctx(&[("apply_patch", true)], true, &[]))
    }

    fn events(frames: &[(String, Value)]) -> Vec<&str> {
        frames.iter().map(|(e, _)| e.as_str()).collect()
    }

    fn seqs(frames: &[(String, Value)]) -> Vec<u64> {
        frames
            .iter()
            .map(|(_, d)| d["sequence_number"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn apply_patch_function_call_rewritten_to_custom_tool_call() {
        let patch = "*** Begin Patch\n*** Add File: foo.txt\n+hello\n*** End Patch\n";
        let args = serde_json::to_string(&json!({ "input": patch })).unwrap();
        let input = [
            frame(
                "response.output_item.added",
                json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                    "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":""}}),
            ),
            frame(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","sequence_number":1,"item_id":"fc_1","output_index":0,"delta":args}),
            ),
            frame(
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","sequence_number":2,"item_id":"fc_1","output_index":0,"arguments":args}),
            ),
            frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,
                    "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":args}}),
            ),
            frame(
                "response.completed",
                json!({"type":"response.completed","sequence_number":4,
                    "response":{"output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":args}]}}),
            ),
        ]
        .concat();
        let frames = run(&input);
        // delta / args.done 被 suppress;apply_patch close 出 input.delta+done+output_item.done。
        assert_eq!(
            events(&frames),
            vec![
                "response.output_item.added",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
                "response.completed",
            ],
            "实得 {:?}",
            events(&frames)
        );
        // added / done item 都是 custom_tool_call;done 带 input。
        assert_eq!(frames[0].1["item"]["type"], "custom_tool_call");
        assert_eq!(frames[0].1["item"]["status"], "in_progress");
        let done = &frames[3].1["item"];
        assert_eq!(done["type"], "custom_tool_call");
        assert_eq!(done["status"], "completed");
        assert!(done["input"].as_str().unwrap().contains("*** Begin Patch"));
        // envelope output[0] 同步重写。
        assert_eq!(
            frames[4].1["response"]["output"][0]["type"],
            "custom_tool_call"
        );
        // sequence_number 重新连续编号 0..5。
        assert_eq!(seqs(&frames), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn tool_search_function_call_rewritten_to_tool_search_call() {
        let args = r#"{"query":"notion"}"#;
        let input = [
            frame(
                "response.output_item.added",
                json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                    "item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"tool_search","arguments":""}}),
            ),
            frame(
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","sequence_number":1,"item_id":"fc_2","output_index":0,"arguments":args}),
            ),
            frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,
                    "item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"tool_search","arguments":args}}),
            ),
        ]
        .concat();
        let frames = run(&input);
        assert_eq!(
            events(&frames),
            vec!["response.output_item.added", "response.output_item.done",],
            "tool_search:added(tool_search_call)+done,args.done suppress;实得 {:?}",
            events(&frames)
        );
        assert_eq!(frames[0].1["item"]["type"], "tool_search_call");
        let done = &frames[1].1["item"];
        assert_eq!(done["type"], "tool_search_call");
        assert_eq!(done["status"], "completed");
        assert_eq!(done["arguments"]["query"], "notion");
        assert_eq!(seqs(&frames), vec![0, 1]);
    }

    #[test]
    fn regular_function_call_passes_through_unchanged() {
        // exec_command 等普通 function 两边同构 → 原样透传(仅 seq 重编号)。
        let input = [
            frame(
                "response.output_item.added",
                json!({"type":"response.output_item.added","sequence_number":9,"output_index":0,
                    "item":{"type":"function_call","id":"fc_3","call_id":"call_3","name":"exec_command","arguments":""}}),
            ),
            frame(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","sequence_number":10,"item_id":"fc_3","output_index":0,"delta":"{\"cmd\":\"ls\"}"}),
            ),
            frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","sequence_number":11,"output_index":0,
                    "item":{"type":"function_call","id":"fc_3","call_id":"call_3","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}),
            ),
        ]
        .concat();
        let frames = run(&input);
        // 普通 function:三事件全透传(delta 不 suppress),item.type 保持 function_call。
        assert_eq!(
            events(&frames),
            vec![
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.output_item.done",
            ]
        );
        assert_eq!(frames[0].1["item"]["type"], "function_call");
        assert_eq!(frames[2].1["item"]["type"], "function_call");
        // seq 重编号成连续 0,1,2(原始是 9,10,11)。
        assert_eq!(seqs(&frames), vec![0, 1, 2]);
    }

    #[test]
    fn non_tool_events_pass_through_and_renumber() {
        let input = [
            frame(
                "response.created",
                json!({"type":"response.created","sequence_number":100,"response":{"id":"r1"}}),
            ),
            frame(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","sequence_number":101,"delta":"hi"}),
            ),
        ]
        .concat();
        let frames = run(&input);
        assert_eq!(
            events(&frames),
            vec!["response.created", "response.output_text.delta"]
        );
        assert_eq!(seqs(&frames), vec![0, 1]);
    }

    #[test]
    fn function_named_apply_patch_but_not_lowered_passes_through() {
        // [review thread0] apply_patch 不在 custom_lowered(是普通 function/MCP 工具)→ 不 repack。
        let input = frame(
            "response.output_item.added",
            json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                "item":{"type":"function_call","id":"fc_x","call_id":"c_x","name":"apply_patch","arguments":""}}),
        );
        let frames = run_ctx(&input, ctx(&[], false, &[]));
        assert_eq!(
            frames[0].1["item"]["type"], "function_call",
            "未 lower 的 apply_patch 应原样透传,不 repack 成 custom_tool_call"
        );
    }

    #[test]
    fn namespaced_discovered_tool_gets_namespace_field() {
        // [review thread1] grok 调发现的 MCP 工具(namespace 内层)→ 透传 function_call 补 namespace。
        let input = [
            frame(
                "response.output_item.added",
                json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                    "item":{"type":"function_call","id":"fc_n","call_id":"c_n","name":"notion_create_pages","arguments":""}}),
            ),
            frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,
                    "item":{"type":"function_call","id":"fc_n","call_id":"c_n","name":"notion_create_pages","arguments":"{}"}}),
            ),
        ]
        .concat();
        let frames = run_ctx(
            &input,
            ctx(&[], false, &[("notion_create_pages", "mcp__notion__")]),
        );
        assert_eq!(frames[0].1["item"]["type"], "function_call");
        assert_eq!(
            frames[0].1["item"]["namespace"], "mcp__notion__",
            "added 的 namespace 工具应补 namespace"
        );
        assert_eq!(
            frames[1].1["item"]["namespace"], "mcp__notion__",
            "done 的 namespace 工具应补 namespace"
        );
    }

    #[test]
    fn json_response_rewrites_output_array() {
        // [review thread2] stream:false 的 grok 成功响应是单 JSON,直接改写顶层 output[]。
        let args = serde_json::to_string(
            &json!({"input":"*** Begin Patch\n*** Add File: f.txt\n+x\n*** End Patch\n"}),
        )
        .unwrap();
        let mut body = json!({"output":[
            {"type":"function_call","id":"fc1","call_id":"c1","name":"apply_patch","arguments":args},
            {"type":"message","role":"assistant","content":[]}
        ]});
        let shim = GrokToolCallShim::new(None, ctx(&[("apply_patch", true)], true, &[]));
        shim.rewrite_json_response(&mut body);
        assert_eq!(
            body["output"][0]["type"], "custom_tool_call",
            "JSON output[] 的 apply_patch function_call 应重写成 custom_tool_call"
        );
        assert!(body["output"][0]["input"]
            .as_str()
            .unwrap()
            .contains("*** Begin Patch"));
        assert_eq!(body["output"][1]["type"], "message", "非工具 item 不动");
    }
}
