use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// 扫 `original_request` 的 `tools[]` 与 `input[]` 里 `type:"namespace"` 包,
/// 建立 `function.name -> namespace.name` 反查表,**累积到进程级缓存**后返回全量。
///
/// 供 Responses / GeminiNative 两条 Responses SSE 转换链路共享,避免同一套
/// 扫描规则在多个 converter 中重复维护。
///
/// **为什么必须累积**:Codex 0.130+ 把 MCP 工具 defer 到 tool_search,工具发现是
/// 渐进的 —— 每轮 input 只带**最近一次** tool_search 的 BM25 top-N(实测 notion
/// 每次 8 个),`tool_search_output` **不累积**历史(很多轮 input 根本没有它)。
/// 若只按当前轮构建映射,LLM 调更早轮次发现的工具时映射缺失 → converter 不加
/// `namespace` 字段 → Codex registry 按 plain `ToolName{namespace:None}` 找不到
/// 注册的 `ToolName::namespaced(...)` → 返 "unsupported call: <tool>"(实测
/// notion_fetch / notion_create_pages 在别轮被发现却在调用轮缺失)。
///
/// 进程级累积:任何工具一旦被**任何一轮**发现就永久记住。name→namespace 映射
/// 稳定(工具名绑 MCP server),跨 session 共享安全。这是规则化全量(覆盖所有
/// 见过的工具),非硬编码白名单。
pub(crate) fn build_tool_namespace_map(
    original_request: Option<&Value>,
) -> HashMap<String, String> {
    // 扫当前 request 的 namespace 包,收集本轮可见的 name→namespace。
    let mut local = HashMap::new();
    if let Some(req) = original_request {
        // (1) req.tools[] 里的 namespace 包(active MCP server 工具集 / codex_app 等)。
        if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
            for tool in tools {
                scan_namespace_pack(tool, &mut local);
            }
        }
        // (2) input[] 的 tool_search_output.tools(tool_search 渐进发现的工具,
        // 同样的 namespace 包结构,codex `protocol/src/models.rs:839`)。
        if let Some(input_items) = req.get("input").and_then(|v| v.as_array()) {
            for item in input_items {
                if item.get("type").and_then(|v| v.as_str()) != Some("tool_search_output") {
                    continue;
                }
                if let Some(ts_tools) = item.get("tools").and_then(|v| v.as_array()) {
                    for tool in ts_tools {
                        scan_namespace_pack(tool, &mut local);
                    }
                }
            }
        }
    }

    // 累积进进程级缓存并返回全量(解决 tool_search_output 不累积导致的 per-request
    // 映射 gap)。poison 时取回内部数据继续 —— 映射缓存损坏不该中断协议转换。
    let mut acc = namespace_accumulator()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    acc.extend(local);
    acc.clone()
}

/// 扫 `original_request.tools[]`,收集所有 `type:"custom"` 工具(Codex freeform
/// 工具,如 apply_patch)的顶层 `name`,返回名字集合。
///
/// **用途**(MOC-88):GeminiNative 响应侧判定「Gemini 回来的这个 functionCall 当初
/// 是不是注册为 custom freeform 工具」。Gemini 只认 structured function,请求侧把所有
/// `type:"custom"` 工具降级成单 `input` 的 function(见 `gemini_native/request.rs`
/// `"custom"` arm);响应侧据此集合把对应 functionCall 重打包回 `custom_tool_call`
/// wire。比硬比 name 字符串 == `apply_patch`(name-only)更准:既不会把**碰巧**同名的
/// 普通 function / MCP 工具误打成 custom(丢 args / namespace),也不漏判 apply_patch
/// 之外的 custom 工具(回成 function_call → Codex router 形态不符)。
///
/// 跟 [`build_tool_namespace_map`] 的差异:**不**累积进程级缓存 —— custom 工具集合是
/// 单请求语义(本轮注册了哪些 custom 工具),无 tool_search 那种跨请求渐进发现问题,
/// 每次按当前 `original_request` 重建即可。custom 工具的 `name` 在 tool 对象顶层
/// (`{"type":"custom","name":...}`),非嵌套 function。
pub(crate) fn build_custom_tool_name_set(original_request: Option<&Value>) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(tools) = original_request
        .and_then(|r| r.get("tools"))
        .and_then(|v| v.as_array())
    {
        for tool in tools {
            if tool.get("type").and_then(|v| v.as_str()) == Some("custom") {
                if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                    set.insert(name.to_owned());
                }
            }
        }
    }
    set
}

/// 进程级 `function.name -> namespace.name` 累积缓存。见
/// [`build_tool_namespace_map`] 文档说明为何需要跨请求累积。
fn namespace_accumulator() -> &'static Mutex<HashMap<String, String>> {
    static ACC: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    ACC.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 扫一个 `type:"namespace"` 包,把内层 `type:"function"` 的 name → namespace.name
/// 写入 map。非 namespace 包 / 缺字段时跳过。供 req.tools[] 与 input[] 的
/// tool_search_output.tools 共用(两者都是同一 namespace 包结构)。
fn scan_namespace_pack(tool: &Value, map: &mut HashMap<String, String>) {
    let Some(obj) = tool.as_object() else {
        return;
    };
    if obj.get("type").and_then(|v| v.as_str()) != Some("namespace") {
        return;
    }
    let Some(ns_name) = obj.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let Some(inner_tools) = obj.get("tools").and_then(|v| v.as_array()) else {
        return;
    };
    for inner in inner_tools {
        let Some(inner_obj) = inner.as_object() else {
            continue;
        };
        if inner_obj.get("type").and_then(|v| v.as_str()) != Some("function") {
            continue;
        }
        if let Some(fname) = inner_obj.get("name").and_then(|v| v.as_str()) {
            // 后写覆盖前写(罕见同名跨 namespace 情况)
            map.insert(fname.to_owned(), ns_name.to_owned());
        }
    }
}

/// 写一帧标准 Responses SSE event:
/// `event: <name>\ndata: <json>\n\n`。
///
/// 该 helper 统一维护 `sequence_number` 注入逻辑,并在 payload 序列化失败时
/// 保留 fallback `{}` + error 日志,防止静默丢失。
pub(crate) fn emit_sse_event(
    out: &mut Vec<u8>,
    seq: &mut u64,
    event_name: &str,
    mut payload: Value,
) {
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
