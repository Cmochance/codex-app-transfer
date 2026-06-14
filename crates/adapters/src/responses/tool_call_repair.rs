//! [MOC-234] responses 1:1 passthrough 的 **orphan function_call 降级修复**。
//!
//! ## 问题
//! Codex 工具续轮发 `function_call_output`(带 `call_id`)+ `previous_response_id`,
//! 依赖上游按 prev_id 回查上一轮产生的 `function_call`。但部分第三方 Responses 反代
//! (如 new-api)在 `store:false` 下**不持久化自己的响应** → 续轮按 call_id 找不到
//! function_call → 返回 400 `No tool call found for function call output with call_id ...`。
//! 实测该上游对其余历史(未知 prev_id 等)宽容,**只窄校验 function_call/output 配对**。
//!
//! ## 降级策略(error-path only)
//! 1. **始终**(不依赖 breakdown 面板)记录 passthrough 响应里上游产生的 `function_call`
//!    (按 `call_id`):[`ToolCallRepairCache::record_output`],由响应侧 tee 调。轻量缓存
//!    (只存 tool-call 本体)+ TTL/上限。
//! 2. forward 层检到上述 400 → 调 [`repair_orphan_tool_calls`]:把缺失的 function_call
//!    从缓存拼回 `input`(放到对应 output 前)+ 去掉 `previous_response_id`(已 inline,
//!    无需上游再回查),→ 透明重发上游。仅当**所有** orphan 都能补齐才算修复成功。
//!
//! **边界**:这是错误路径上的请求重写(偏离纯 1:1),仅在上游明确报 orphan 400 时触发;
//! 成功路径与非该错误一律不动。缓存命中不全 → 不重试(退回 response.failed,见
//! `mapper::responses`),避免补一半再 400。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 缓存的 tool-call 总上限(按 call_id);超限顶出最旧。
const MAX_CALLS: usize = 8192;
/// TTL:工具续轮通常紧跟产生轮,2h 足够;过期清理防无界增长。
const TTL: Duration = Duration::from_secs(2 * 3600);

struct Entry {
    inserted: Instant,
    /// 上游产生的 tool-call item 本体(`function_call` / `custom_tool_call` /
    /// `local_shell_call`),原样存,修复时原样 splice 回 input。
    item: Value,
}

#[derive(Default)]
struct Inner {
    by_call_id: HashMap<String, Entry>,
}

/// 进程级 always-on tool-call 缓存(响应侧 tee 写、forward 修复读)。
pub struct ToolCallRepairCache {
    inner: Mutex<Inner>,
}

impl Default for ToolCallRepairCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallRepairCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// 从一次响应的 output items 里提取 tool-call(带 `call_id`),按 call_id 记录。
    /// 非 tool-call / 无 call_id 的 item 跳过。best-effort,锁中毒静默跳过(不影响转发)。
    pub fn record_output(&self, output_items: &[Value]) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        inner
            .by_call_id
            .retain(|_, e| now.duration_since(e.inserted) < TTL);
        for item in output_items {
            let Some(call_id) = tool_call_id(item) else {
                continue;
            };
            if inner.by_call_id.len() >= MAX_CALLS && !inner.by_call_id.contains_key(call_id) {
                if let Some(oldest) = inner
                    .by_call_id
                    .iter()
                    .min_by_key(|(_, e)| e.inserted)
                    .map(|(k, _)| k.clone())
                {
                    inner.by_call_id.remove(&oldest);
                }
            }
            inner.by_call_id.insert(
                call_id.to_owned(),
                Entry {
                    inserted: now,
                    item: item.clone(),
                },
            );
        }
    }

    fn get(&self, call_id: &str) -> Option<Value> {
        let inner = self.inner.lock().ok()?;
        inner.by_call_id.get(call_id).map(|e| e.item.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|i| i.by_call_id.len()).unwrap_or(0)
    }
}

/// 进程级缓存单例。
pub fn global_tool_call_repair_cache() -> &'static ToolCallRepairCache {
    static C: OnceLock<ToolCallRepairCache> = OnceLock::new();
    C.get_or_init(ToolCallRepairCache::new)
}

/// 一个 item 若是带 `call_id` 的 tool-call(`function_call` / `custom_tool_call` /
/// `local_shell_call`),返回其 call_id;否则 None。
fn tool_call_id(item: &Value) -> Option<&str> {
    let t = item.get("type").and_then(Value::as_str)?;
    let is_call = t == "function_call"
        || t == "custom_tool_call"
        || t == "local_shell_call"
        || t == "tool_call";
    if !is_call {
        return None;
    }
    item.get("call_id").and_then(Value::as_str)
}

/// `function_call_output` / `custom_tool_call_output` / `local_shell_call_output` /
/// `tool_result` 这类「工具输出」item 的 call_id。
fn tool_output_call_id(item: &Value) -> Option<&str> {
    let t = item.get("type").and_then(Value::as_str)?;
    let is_output = t == "function_call_output"
        || t == "custom_tool_call_output"
        || t == "local_shell_call_output"
        || t == "tool_result";
    if !is_output {
        return None;
    }
    item.get("call_id").and_then(Value::as_str)
}

/// [MOC-234] 检测 + 修复 orphan function_call:把 `body.input` 里**缺失配对 function_call**
/// 的 tool-output,用缓存里的 function_call 补到该 output **前面**,并去掉
/// `previous_response_id`(已 inline,无需上游回查)。
///
/// 返回 `true` 当且仅当:存在 orphan 且**全部**能从缓存补齐(补齐后 input 内每个 tool-output
/// 都有同 call_id 的 tool-call 在其前)。任一 orphan 缓存未命中 → 不动 body、返回 `false`
/// (补一半再发仍会 400,不如退回 response.failed 显示错误)。
pub fn repair_orphan_tool_calls(body: &mut Value, cache: &ToolCallRepairCache) -> bool {
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return false;
    };

    // 已在 input 内出现的 tool-call call_id(这些 output 不算 orphan)。
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in input {
        if let Some(cid) = tool_call_id(item) {
            present.insert(cid.to_owned());
        }
    }

    // 找出 orphan 的 tool-output(其 call_id 不在 present 中)及其缓存命中的 function_call。
    let mut orphan_calls: Vec<(String, Value)> = Vec::new();
    let mut has_orphan = false;
    for item in input {
        let Some(cid) = tool_output_call_id(item) else {
            continue;
        };
        if present.contains(cid) {
            continue; // 同请求内已有配对,非 orphan
        }
        has_orphan = true;
        match cache.get(cid) {
            Some(fc) => orphan_calls.push((cid.to_owned(), fc)),
            None => return false, // 任一 orphan 补不齐 → 整体放弃(避免补一半)
        }
    }
    if !has_orphan {
        return false; // 没有 orphan,无需修复(不是这个错)
    }

    // 重建 input:在每个 orphan output 前插入其 function_call(去重:同 call_id 只插一次)。
    let orphan_map: HashMap<String, Value> = orphan_calls.into_iter().collect();
    let mut inserted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let old_input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut new_input: Vec<Value> = Vec::with_capacity(old_input.len() + orphan_map.len());
    for item in old_input {
        if let Some(cid) = tool_output_call_id(&item) {
            if let Some(fc) = orphan_map.get(cid) {
                if inserted.insert(cid.to_owned()) {
                    new_input.push(fc.clone());
                }
            }
        }
        new_input.push(item);
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_owned(), Value::Array(new_input));
        // 已把缺失 function_call inline,去掉 previous_response_id —— 上游(store:false)
        // 本就没有它,带着它只会让上游再尝试回查;inline 后请求自包含。
        obj.remove("previous_response_id");
    }
    true
}

/// forward 层用:上游错误 body 是否为「orphan function_call」400(new-api 类反代在
/// `store:false` 下找不到自己产生的 function_call)。**只认无歧义的该错误**,避免误触发
/// 重写重试。错误形如 `{"error":{"message":"No tool call found for function call output ..."}}`;
/// 非 JSON / 裹在 SSE 里时退化为子串匹配。
pub fn is_orphan_function_call_error(error_body: &[u8]) -> bool {
    const MARKER: &str = "No tool call found for function call output";
    if let Ok(v) = serde_json::from_slice::<Value>(error_body) {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .or_else(|| v.get("message").and_then(Value::as_str))
            .unwrap_or("");
        if msg.contains(MARKER) {
            return true;
        }
    }
    std::str::from_utf8(error_body)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

/// forward 层用:对 orphan-function_call 续轮请求体做降级修复。解析 `body` → 用全局缓存把
/// 缺失的 function_call 拼回 `input` + 去 `previous_response_id` → 返回修复后的 bytes。
/// 不可修复(无 orphan / 缓存命中不全 / 非 JSON)→ `None`(调用方不重试)。
pub fn repair_orphan_tool_calls_bytes(body: &[u8]) -> Option<bytes::Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    if repair_orphan_tool_calls(&mut v, global_tool_call_repair_cache()) {
        serde_json::to_vec(&v).ok().map(bytes::Bytes::from)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fc(call_id: &str) -> Value {
        json!({"type":"function_call","name":"exec_command","arguments":"{}","call_id":call_id})
    }
    fn fco(call_id: &str) -> Value {
        json!({"type":"function_call_output","call_id":call_id,"output":"ok"})
    }

    #[test]
    fn records_and_repairs_orphan_function_call() {
        let cache = ToolCallRepairCache::new();
        // 上一轮上游响应产生了 call_A 的 function_call。
        cache.record_output(&[fc("call_A"), json!({"type":"message","role":"assistant"})]);
        assert_eq!(cache.len(), 1, "只记 tool-call,message 跳过");

        // 续轮:input 只有 call_A 的 output(orphan)+ previous_response_id。
        let mut body = json!({
            "model":"gpt-5.5",
            "previous_response_id":"resp_x",
            "input":[ fco("call_A") ]
        });
        assert!(repair_orphan_tool_calls(&mut body, &cache), "应修复");
        let input = body["input"].as_array().unwrap();
        // function_call 被插到 output 前
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_A");
        assert_eq!(input[1]["type"], "function_call_output");
        // previous_response_id 被去掉
        assert!(body.get("previous_response_id").is_none(), "应去掉 prev_id");
    }

    #[test]
    fn no_repair_when_call_already_present() {
        let cache = ToolCallRepairCache::new();
        cache.record_output(&[fc("call_A")]);
        // input 同请求内已含 function_call → 非 orphan,不该重写
        let mut body = json!({"input":[ fc("call_A"), fco("call_A") ], "previous_response_id":"r"});
        assert!(!repair_orphan_tool_calls(&mut body, &cache));
        assert_eq!(body["input"].as_array().unwrap().len(), 2, "不动 input");
        assert_eq!(body["previous_response_id"], "r", "不动 prev_id");
    }

    #[test]
    fn no_repair_when_cache_misses_any_orphan() {
        let cache = ToolCallRepairCache::new();
        cache.record_output(&[fc("call_A")]); // 只有 A,没有 B
        let mut body =
            json!({"input":[ fco("call_A"), fco("call_B") ], "previous_response_id":"r"});
        // B 补不齐 → 整体放弃(避免补一半再 400)
        assert!(!repair_orphan_tool_calls(&mut body, &cache));
        assert_eq!(body["input"].as_array().unwrap().len(), 2, "未命中则不动");
    }

    #[test]
    fn no_repair_when_no_orphan_output() {
        let cache = ToolCallRepairCache::new();
        let mut body = json!({"input":[ json!({"type":"message","role":"user","content":"hi"}) ]});
        assert!(
            !repair_orphan_tool_calls(&mut body, &cache),
            "无 tool-output,非此错"
        );
    }

    #[test]
    fn repairs_multiple_orphans_in_order() {
        let cache = ToolCallRepairCache::new();
        cache.record_output(&[fc("call_A"), fc("call_B")]);
        let mut body =
            json!({"input":[ fco("call_A"), fco("call_B") ], "previous_response_id":"r"});
        assert!(repair_orphan_tool_calls(&mut body, &cache));
        let input = body["input"].as_array().unwrap();
        // 顺序:fc(A), fco(A), fc(B), fco(B)
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["call_id"], "call_A");
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_B");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }
}
