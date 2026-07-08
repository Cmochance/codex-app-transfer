//! [MOC-299] grok-build 请求适配。
//!
//! grok 的 `/v1/responses` 是**真 Responses API**(实证:拒 chat 字段 `max_tokens`、要
//! `max_output_tokens`、认 `input`/`reasoning`/`output`),但**工具集受限**:只认
//! `function` / `web_search` / `x_search` / `file_search` / `code_execution` /
//! `code_interpreter` / `mcp` / `shell`,**不认** Codex 特有的 `custom`(apply_patch
//! freeform)/ `namespace`(MCP 包)/ `tool_search` / `image_generation`;且**不支持**
//! `reasoning.effort`(models_cache `supports_reasoning_effort=false`,但它自身会 reasoning)。
//!
//! 未适配时上游 422 `unknown variant 'custom'` / 400 `does not support parameter
//! reasoningEffort`。这里在 1:1 透传前把 Codex 请求体改成 grok 接受的形态:
//! - 工具转换:`namespace`(MCP 包)**复用 chat 路径决策**([`convert_responses_tool_to_chat_tool`]:
//!   摊平成 function),再把它输出的 chat 形 `{type:function, function:{…}}` **unwrap 回 responses-flat**
//!   `{type:function, …}`(grok 的 function 工具就是 responses-flat);`web_search` 剥成 bare
//!   `{type:web_search}`;`custom`(apply_patch)/ `tool_search` / `image_generation` / 未知类型
//!   **直接 drop 不 advertise**。`custom`(apply_patch)之所以 drop 而非转 function:响应侧 shim 未接线
//!   (见下方「响应侧」),转了会静默失败。`tool_search` drop 会让 defer 的 MCP/连接器工具在 grok 上
//!   无法发现(见 MOC-304)。
//! - `reasoning` 归一为 `{"summary": "concise"}`(对齐真实 grok CLI 抓包)。grok 不支持
//!   `effort`,且靠 `summary` 指令生成 + 跨轮解密**加密 reasoning**(`encrypted_content`);
//!   Codex 发 `{"effort":...}` 无 summary,若只剥成 `{}`,grok 下一轮回灌 encrypted_content 时
//!   **解不开**(400 "Could not decode the compaction blob")。encrypted_content **保留原样透传**
//!   —— grok 原生支持加密 reasoning,是它的能力,不能像 chat 路径那样丢(chat 丢是因上游不支持)。
//!
//! 真机实证(2026-07-07):适配后 grok `/responses` 返 200 并正常推理 + 调 function 工具。
//! model 映射(gpt-5.x → grok-build)由 resolver 负责,不在此。
//!
//! **响应侧 + apply_patch(followup MOC-301)**:exec_command 等普通 function 两边同构、透传即可。
//! apply_patch 要端到端可用(grok 能改文件),需请求侧转 function + 响应侧把 `function_call` 重打包回
//! Codex 的 `custom_tool_call`(Codex apply_patch handler 硬要 ToolPayload::Custom)。但 responses
//! passthrough 的 map_response 成功流是 1:1 字节直透、不经 `responses::converter` 的 shim,响应侧改写
//! 需给 passthrough 加流式 SSE 转换层。接线前**请求侧先 drop apply_patch**(见上,不 advertise),避免
//! grok 假装能改文件然后静默失败。全 shim(请求侧转 + 响应侧改写)在 MOC-301。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use serde_json::{json, Value};

use crate::responses::compact::message_text;
use crate::responses::request::tools::convert_responses_tool_to_chat_tool;

/// grok-build provider 判定(authScheme=grok_build_oauth)。
pub(crate) fn is_grok_build_provider(provider: &Provider) -> bool {
    let s = provider
        .auth_scheme
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_");
    s == "grok_build_oauth" || s == "grok_build"
}

/// [MOC-299] provider 的 api_format=responses 但上游无 Codex compaction 能力(无 /responses/compact
/// 端点、InputItem 无 compaction_trigger)→ 需在代理层本地做 compaction(见 mapper::responses)。当前
/// 命中 grok_build;未来同类第三方在此登记。真 OpenAI/ChatGPT backend 有 compaction,不进此列。
pub(crate) fn responses_upstream_lacks_compaction(provider: &Provider) -> bool {
    is_grok_build_provider(provider)
}

/// 把 Codex 的 Responses 请求体适配成 grok-build `/responses` 接受的形态。
/// `Some(新 body)` = 改过;`None` = 无需改 / 解析失败(caller 用原 body 透传,零回归)。
pub(crate) fn adapt_grok_build_request_body(body: &Bytes, provider: &Provider) -> Option<Bytes> {
    // grok 的适配是**必需**的(不改则含 custom/namespace/reasoning.effort → grok 直接 422/400)。
    // parse 失败时留痕:别和「无需改动」的 None 混同 —— caller 会透传未适配 body,grok 报误导性
    // `unknown variant 'custom'`,排查者会往工具逻辑找而非「body 没被 parse」。[silent-failure M1]
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "grok-build 请求体 parse 失败,跳过适配(透传未适配 body,grok 可能 422): {e}"
            );
            return None;
        }
    };
    let obj = v.as_object_mut()?;
    let mut changed = false;

    // 1. 工具适配。
    if let Some(Value::Array(tools)) = obj.get("tools") {
        let tools = tools.clone();
        let mut out: Vec<Value> = Vec::with_capacity(tools.len());
        for t in &tools {
            match t.get("type").and_then(Value::as_str).unwrap_or("") {
                // 已是 grok 兼容的 responses-flat function,原样保留。
                "function" => out.push(t.clone()),
                // web_search:grok 认 bare `{type:web_search}`,剥 Codex 的
                // external_web_access / search_content_types 等子字段。
                "web_search" | "web_search_preview" => out.push(json!({ "type": "web_search" })),
                // namespace(MCP 包):复用 chat 路径转换决策(摊平成 function),再 unwrap 回 flat。
                "namespace" => {
                    for ct in convert_responses_tool_to_chat_tool(t, Some(provider)) {
                        out.push(unwrap_chat_tool_to_responses_flat(ct));
                    }
                }
                // custom(apply_patch freeform):[AI review P1] **暂 drop 不 advertise**。转成 function 后
                // grok 会调用它,但 responses passthrough 的 map_response 非 compact 分支是 1:1 字节直透、
                // **不把 function_call 重打包回 Codex 的 custom_tool_call**(Codex apply_patch handler 硬要
                // ToolPayload::Custom)→ grok 改文件会**静默失败**。响应侧流式 shim 见 MOC-301;接线前先不
                // advertise,让 grok 暂只读/跑命令,而不是「假装能改文件然后静默失败」。
                "custom" => {}
                // tool_search / image_generation / 未知:grok 无等价 → **本臂直接 drop**(不入 convert)。
                // 注:tool_search 关掉会让 defer 的 MCP/连接器工具在 grok 上无法发现(见 MOC-304)。
                _ => {}
            }
        }
        let out_empty = out.is_empty();
        if out != tools {
            obj.insert("tools".into(), Value::Array(out));
            changed = true;
        }
        // [silent-failure M2 / test gap1 / code-reviewer obs1] grok 约束:tool_choice 存在必须有
        // 非空 tools 陪。适配后 tools 全被 drop(如 compact 摘要 body 带 tool_choice:none、原 tools
        // 又恰全是 tool_search/image_generation)而 tool_choice 仍在 → 移除它,避免 grok 400
        // "tool_choice set but no tools"。通用安全(普通轮同理受护),不止 compact。
        if out_empty && obj.remove("tool_choice").is_some() {
            changed = true;
        }
    }

    // 2. reasoning 归一成 grok 认的形态。抓包实证(2026-07-07):真实 grok CLI 全程发
    // `reasoning: {"summary": "concise"}`(**不发 effort**,grok-build supports_reasoning_effort=false)。
    // grok 靠 `reasoning.summary` 指令生成 + **加密 reasoning(encrypted_content)** 并跨轮解密;
    // Codex 发的是 `{"effort": "medium"}`(无 summary),若只剥 effort 留 `{}`,grok 缺 summary 指令
    // → 生成的 encrypted_content 在下一轮回灌时**解不开**(400 "Could not decode the compaction
    // blob")。故直接对齐真实 CLI:reasoning = {"summary": "concise"}。**保留** input 里的
    // encrypted_content(grok 原生支持加密 reasoning,是它的能力,不能丢 —— 与 chat 路径「上游
    // 不支持才丢」不同)。
    if obj.contains_key("reasoning") {
        obj.insert("reasoning".into(), json!({ "summary": "concise" }));
        changed = true;
    }

    // 3. input 里回灌的 reasoning item:对齐真实 grok CLI 的干净结构。抓包实证 grok 自己回灌的
    // reasoning item 是 `{type, id, summary, encrypted_content}`;而 Codex 的是 `{type, summary,
    // content, encrypted_content, internal_chat_message_metadata_passthrough}` —— 多了 `content`
    // 与 Codex 私有的 `internal_chat_message_metadata_passthrough`。这些额外字段会干扰 grok 对
    // encrypted_content 的解密(400 "Could not decode the compaction blob")。剥掉它们,**保留
    // encrypted_content / summary**(grok 需要)。`internal_chat_message_metadata_passthrough` 只从
    // reasoning item 剥(message item 上 grok 已容忍,不动以免影响别的路径)。
    if let Some(Value::Array(input)) = obj.get_mut("input") {
        for it in input.iter_mut() {
            if it.get("type").and_then(Value::as_str) == Some("reasoning") {
                if let Some(o) = it.as_object_mut() {
                    // content 恒为 null/空(reasoning 文本在 summary 里),grok 结构无此字段。
                    if o.remove("content").is_some() {
                        changed = true;
                    }
                    if o.remove("internal_chat_message_metadata_passthrough")
                        .is_some()
                    {
                        changed = true;
                    }
                }
            }
        }
    }

    // 4. [MOC-299] 指令块去重(keep-latest-at-front,兼顾 cache 与内容更新)。Codex 对 grok
    // (store=false)跨轮/跨 resume 会把 61KB 的 developer(sandbox+skills+memory)+ AGENTS 指令块
    // **重复注入**(真机实测多天会话堆到 7×61KB=429KB context,压平后 ≈1 份)。这两类是系统指令、
    // 只需一份。**keep-latest-at-front**:把**最新**一份的内容搬到**最前**那份的位置、删其余 ——
    // ① 同一天内容稳定时前缀跨轮不变,prompt cache 照常命中;② 内容真更新(改 AGENTS.md / 加 skill /
    // 换天)时最新内容**立即生效**(keep-first 保旧版会让更新在本轮对话完全不可见,是正确性 bug),
    // 跨天/更新时前缀仅变一次 cache miss,可接受。分组键掩码 `<current_date>` + ≥6 位数字(plugin
    // cache 版本号噪音)让纯噪音差异归一;内容真变(签名后文本不同)→ 键不同 → 两份都留(不误折)。
    // **只碰这两类指令块**,对话 message / reasoning / function_call(_output) 全部原样保留。
    if let Some(Value::Array(input)) = obj.get_mut("input") {
        if dedupe_instruction_blocks(input) {
            changed = true;
        }
    }

    changed
        .then(|| serde_json::to_vec(&v).ok().map(Bytes::from))
        .flatten()
}

/// [MOC-299] 折叠 input 里重复的指令块(developer sandbox 块 + AGENTS 块),**keep-latest-at-front**:
/// 每个分组把**最后一份(最新)**的内容搬到**第一份(最前)**的位置、删除其余份。返回是否改过。
/// 非指令 item(对话 / reasoning / tool)一律保留、位置不动。
///
/// keep-latest 而非 keep-first:同一分组内若内容随对话演进(改 AGENTS.md / 加 skill / 换天 / 掩码
/// 误组了有意义差异),保最新才能让更新在本轮**立即生效**;保旧版会静默隐藏更新(正确性 bug)。
/// 放最前保证系统指令仍在前缀位置:同一天内容稳定 → 前缀不变、cache 命中;仅内容真变/跨天时前缀
/// 变一次(可接受)。
fn dedupe_instruction_blocks(input: &mut Vec<Value>) -> bool {
    use std::collections::HashMap;
    // 按 input 顺序收集每个分组的出现位置(升序)。
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, it) in input.iter().enumerate() {
        if let Some(key) = instruction_group_key(it) {
            groups.entry(key).or_default().push(i);
        }
    }
    let mut to_remove = vec![false; input.len()];
    let mut changed = false;
    for idxs in groups.values() {
        if idxs.len() <= 1 {
            continue; // 该指令块只出现一次,无重复。
        }
        changed = true;
        let (first, last) = (idxs[0], *idxs.last().unwrap());
        // keep-latest-at-front:最新一份的内容搬到最前那份的位置。
        if first != last {
            let latest = input[last].clone();
            input[first] = latest;
        }
        // 删除除 first 外的所有出现(first 已承载最新内容)。
        for &i in &idxs[1..] {
            to_remove[i] = true;
        }
    }
    if !changed {
        return false;
    }
    let mut i = 0;
    input.retain(|_| {
        let keep = !to_remove[i];
        i += 1;
        keep
    });
    true
}

/// 指令块识别 + 分组键(掩码 `<current_date>` 让跨天版本归一)。非指令块返回 `None`。
/// 只认两类:role=developer 且以 `<permissions instructions>` 开头(Codex sandbox/skills/memory);
/// role=user 且以 `# AGENTS.md instructions` 开头(项目指令)。签名变了则静默 no-op(fail-safe)。
fn instruction_group_key(item: &Value) -> Option<u64> {
    let role = item.get("role").and_then(Value::as_str)?;
    // 复用 compact 的 message_text(adapters crate 已有的 content→text 提取,避免第 4 份重复)。
    // 分隔符对本用途无所谓(仅 starts_with 签名判断 + 分组键判重)。
    let text = message_text(item);
    let t = text.trim_start();
    let sig = match role {
        "developer" if t.starts_with("<permissions instructions>") => "dev",
        "user" if t.starts_with("# AGENTS.md instructions") => "agents",
        _ => return None,
    };
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sig.hash(&mut h);
    mask_volatile_for_key(&text).hash(&mut h);
    Some(h.finish())
}

/// 分组键归一(**仅用于判重,不改实际保留的块字节** → 保留的是某一份的原始 bytes,prompt
/// cache 不受影响)。掩码两类 build/session 噪音,让"同一指令块的不同快照"归一为一份:
/// - `<current_date>…</current_date>`(跨天变);
/// - **≥6 位连续数字**(plugin cache 目录版本号如 `.../browser/26.623.141536/skills/…` 里的
///   `141536` —— plugin 更新会变但技能内容相同;≥6 位避开日期/小序号等有意义短数字)。
///
/// 只差这些噪音的块才归一;技能真变(加/删一个 skill)→ 差异不止数字 → 键不同 → 两份都留(安全)。
fn mask_volatile_for_key(text: &str) -> String {
    // 1. current_date → 占位。
    const OPEN: &str = "<current_date>";
    const CLOSE: &str = "</current_date>";
    let stage1: std::borrow::Cow<'_, str> = match text.find(OPEN) {
        Some(a) => match text[a..].find(CLOSE) {
            Some(rel) => {
                let b = a + rel + CLOSE.len();
                std::borrow::Cow::Owned(format!("{}<current_date/>{}", &text[..a], &text[b..]))
            }
            None => std::borrow::Cow::Borrowed(text),
        },
        None => std::borrow::Cow::Borrowed(text),
    };
    // 2. ≥6 位连续数字 → 单个 `#`(buffer 数字串,遇非数字时按长度决定原样输出还是掩码)。
    let mut out = String::with_capacity(stage1.len());
    let mut digits = String::new();
    for ch in stage1.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            flush_digit_run(&mut out, &mut digits);
            out.push(ch);
        }
    }
    flush_digit_run(&mut out, &mut digits);
    out
}

/// ≥6 位的数字串掩成 `#`,否则原样;清空 buffer。
fn flush_digit_run(out: &mut String, digits: &mut String) {
    if digits.len() >= 6 {
        out.push('#');
    } else {
        out.push_str(digits);
    }
    digits.clear();
}

/// chat 工具 `{type:function, function:{name,description,parameters,strict}}` → responses-flat
/// `{type:function, name, description, parameters, strict}`(把内层 `function` 对象提到顶层)。
/// 非 function 形态(理论上 convert 不会为 custom/namespace 产出别的)原样返回。
fn unwrap_chat_tool_to_responses_flat(chat_tool: Value) -> Value {
    if let Some(obj) = chat_tool.as_object() {
        if obj.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(f) = obj.get("function").and_then(Value::as_object) {
                let mut flat = serde_json::Map::with_capacity(f.len() + 1);
                flat.insert("type".into(), json!("function"));
                for (k, val) in f {
                    flat.insert(k.clone(), val.clone());
                }
                return Value::Object(flat);
            }
        }
    }
    chat_tool
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn grok_provider() -> Provider {
        Provider {
            id: "grok-build".into(),
            name: "Grok Build".into(),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            auth_scheme: "grok_build_oauth".into(),
            api_format: "responses".into(),
            api_key: String::new(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: true,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    #[test]
    fn detects_grok_build_provider() {
        assert!(is_grok_build_provider(&grok_provider()));
        let mut p = grok_provider();
        p.auth_scheme = "bearer".into();
        assert!(!is_grok_build_provider(&p));
    }

    #[test]
    fn responses_upstream_lacks_compaction_only_for_grok_build() {
        // grok_build_oauth → 需本地 compaction。
        assert!(responses_upstream_lacks_compaction(&grok_provider()));
        // 其它 authScheme(bearer 等)→ 上游自有 compaction,不本地做。
        let mut p = grok_provider();
        p.auth_scheme = "bearer".into();
        assert!(!responses_upstream_lacks_compaction(&p));
    }

    #[test]
    fn adapts_tools_and_normalizes_reasoning() {
        // Codex 请求:function(保留)+ namespace(摊平→function)+ web_search(→bare)+
        // custom apply_patch / tool_search / image_generation(drop);reasoning.effort(剥)。
        let body = serde_json::to_vec(&json!({
            "model": "grok-build",
            "input": [],
            "reasoning": { "effort": "medium", "summary": "auto" },
            "tools": [
                { "type": "function", "name": "exec_command", "parameters": {"type":"object"}, "strict": true },
                { "type": "custom", "name": "apply_patch", "description": "Use apply_patch",
                  "format": {"type":"grammar","syntax":"lark","definition":"start: x"} },
                { "type": "namespace", "name": "mcp__srv__", "tools": [
                    { "type": "function", "name": "srv_do", "parameters": {"type":"object"} }
                ]},
                { "type": "web_search", "external_web_access": true, "search_content_types": ["text"] },
                { "type": "tool_search", "execution": {} },
                { "type": "image_generation", "output_format": "png" }
            ]
        })).unwrap();

        let out =
            adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).expect("改过");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let tools = v["tools"].as_array().unwrap();

        // 所有输出工具都必须是 grok 认的类型(function / web_search),无 custom/namespace/tool_search/image_generation。
        let types: Vec<&str> = tools.iter().filter_map(|t| t["type"].as_str()).collect();
        assert!(
            types.iter().all(|t| *t == "function" || *t == "web_search"),
            "适配后只应剩 function / web_search,实得 {types:?}"
        );
        // 明确不含被剥/转换的原类型。
        assert!(!types.contains(&"custom"), "custom 应转成 function");
        assert!(!types.contains(&"namespace"), "namespace 应摊平");
        assert!(!types.contains(&"tool_search"), "tool_search 应 drop");
        assert!(
            !types.contains(&"image_generation"),
            "image_generation 应 drop"
        );
        // [AI review P1] apply_patch(custom)现在 **drop 不 advertise**(响应侧 shim 未接线,转 function
        // 会静默失败 —— 见 MOC-301),故不应出现在输出里。
        assert!(
            !tools.iter().any(|t| t["name"] == "apply_patch"),
            "apply_patch 应被 drop(响应侧 shim 未接线前不 advertise)"
        );
        // namespace 摊平出内层 function。
        assert!(
            tools.iter().any(|t| t["name"] == "srv_do"),
            "namespace 内层工具应摊平出来"
        );
        // web_search 剥成 bare(无 external_web_access)。
        let ws = tools.iter().find(|t| t["type"] == "web_search").unwrap();
        assert!(
            ws.get("external_web_access").is_none(),
            "web_search 应剥成 bare"
        );
        // reasoning 归一为 {summary:concise}(对齐真实 grok CLI,去 effort、去 Codex 的 summary 值)。
        assert_eq!(
            v["reasoning"],
            json!({ "summary": "concise" }),
            "reasoning 应归一为 {{summary:concise}}"
        );
    }

    #[test]
    fn keeps_encrypted_reasoning_and_normalizes() {
        // grok 原生支持加密 reasoning(encrypted_content),**不能丢**(与 chat 路径「上游不支持才丢」
        // 不同)。适配只归一 reasoning + include 保留;input 里的 encrypted_content 原样透传。
        let body = serde_json::to_vec(&json!({
            "model": "grok-build",
            "include": ["reasoning.encrypted_content"],
            "reasoning": { "effort": "medium" },
            "input": [
                { "type": "message", "role": "user", "content": "hi",
                  "internal_chat_message_metadata_passthrough": {"x": 1} },
                { "type": "reasoning", "summary": [{"type":"summary_text","text":"thinking"}],
                  "content": [], "encrypted_content": "AAAA-opaque-grok-blob-BBBB",
                  "internal_chat_message_metadata_passthrough": {"turn": 1} },
                { "type": "function_call", "name": "exec_command", "arguments": "{}", "call_id": "c1" }
            ]
        }))
        .unwrap();
        let out =
            adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).expect("改过");
        let v: Value = serde_json::from_slice(&out).unwrap();
        // reasoning 归一。
        assert_eq!(v["reasoning"], json!({ "summary": "concise" }));
        // include 保留(grok 支持加密 reasoning)。
        assert_eq!(
            v["include"],
            json!(["reasoning.encrypted_content"]),
            "include 应保留"
        );
        let ritem = v["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["type"] == "reasoning")
            .unwrap();
        // encrypted_content + summary **保留**(grok 需要它跨轮解密 + 结构)。
        assert_eq!(
            ritem["encrypted_content"], "AAAA-opaque-grok-blob-BBBB",
            "encrypted_content 必须原样保留,不能丢"
        );
        assert!(ritem.get("summary").is_some(), "summary 保留");
        // Codex 私有字段剥掉(对齐 grok 干净结构)。
        assert!(
            ritem.get("content").is_none(),
            "reasoning item 的 content 应剥"
        );
        assert!(
            ritem
                .get("internal_chat_message_metadata_passthrough")
                .is_none(),
            "reasoning item 的 internal_chat_message_metadata_passthrough 应剥"
        );
        // message item 的 internal_metadata 不动(grok 已容忍)。
        let msg = v["input"].as_array().unwrap()[0].as_object().unwrap();
        assert!(
            msg.contains_key("internal_chat_message_metadata_passthrough"),
            "message item 的 internal_metadata 不动"
        );
    }

    #[test]
    fn returns_none_when_nothing_to_change() {
        // 无 tools、无 reasoning → 不改(caller 走原 body)。
        let body = serde_json::to_vec(&json!({ "model": "grok-build", "input": [] })).unwrap();
        assert!(adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).is_none());
    }

    // ---- [MOC-299] 指令块去重 ----

    fn dev_block(date: &str) -> Value {
        json!({"type":"message","role":"developer",
            "content":[{"type":"text","text":format!("<permissions instructions>\nsandbox…skills…<current_date>{date}</current_date>…memory")}]})
    }
    fn agents_block(date: &str) -> Value {
        json!({"type":"message","role":"user",
            "content":[{"type":"text","text":format!("# AGENTS.md instructions\n…<current_date>{date}</current_date>…")}]})
    }

    #[test]
    fn dedupes_repeated_instruction_blocks_keep_latest_across_date_versions() {
        // 4×[dev,agents](2 日期版本:7-06×2 + 7-07×2)+ 真实对话 → 各折成 1 份,keep-latest。
        let input = vec![
            dev_block("2026-07-06"),    // [0] first 位置:承载最新内容
            agents_block("2026-07-06"), // [1] first 位置:承载最新内容
            dev_block("2026-07-06"),    // 删
            agents_block("2026-07-06"), // 删
            dev_block("2026-07-07"),    // 删(掩码 current_date 后同组;它是 dev 组的最新一份)
            agents_block("2026-07-07"), // 删(agents 组最新一份)
            dev_block("2026-07-07"),    // 删
            agents_block("2026-07-07"), // 删
            json!({"type":"message","role":"user","content":"回复意见"}),
            json!({"type":"reasoning","summary":[{"type":"summary_text","text":"t"}],"encrypted_content":"BLOB"}),
            json!({"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}),
            json!({"type":"function_call_output","call_id":"c1","output":"ok"}),
        ];
        let body = serde_json::to_vec(&json!({"model":"grok-build","input":input})).unwrap();
        let out =
            adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).expect("改过");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let inp = v["input"].as_array().unwrap();
        // 8 个指令块 → 2 个(1 dev + 1 agents),+ 对话 4 项 = 6。
        assert_eq!(inp.len(), 6, "指令块应折成各 1 份:{v}");
        let devs = inp.iter().filter(|it| it["role"] == "developer").count();
        let agents = inp
            .iter()
            .filter(|it| {
                it["role"] == "user" && message_text(it).trim_start().starts_with("# AGENTS.md")
            })
            .count();
        assert_eq!(devs, 1, "developer 块折成 1");
        assert_eq!(agents, 1, "AGENTS 块折成 1");
        // keep-latest-at-front:保留在最前位置(inp[0]/[1]),但内容是**最新一份**(7-07 日期)。
        assert!(
            message_text(&inp[0]).contains("2026-07-07"),
            "keep-latest 保最新那份的内容"
        );
        assert!(
            !message_text(&inp[0]).contains("2026-07-06"),
            "旧版内容不应残留"
        );
        assert_eq!(inp[0]["role"], "developer", "指令块仍在最前位置");
        assert_eq!(inp[1]["role"], "user");
        assert!(
            message_text(&inp[1]).contains("2026-07-07"),
            "AGENTS 也保最新"
        );
        // 对话/reasoning/tool 全保留且顺序不乱。
        assert_eq!(inp[2]["content"], "回复意见");
        assert_eq!(inp[3]["type"], "reasoning");
        assert_eq!(inp[3]["encrypted_content"], "BLOB");
        assert_eq!(inp[4]["type"], "function_call");
        assert_eq!(inp[5]["type"], "function_call_output");
    }

    #[test]
    fn dedupe_leaves_single_prefix_and_conversation_untouched() {
        // 只有 1 份指令块 + 对话 → 不动(无重复可删)。
        let input = vec![
            dev_block("2026-07-07"),
            agents_block("2026-07-07"),
            json!({"type":"message","role":"user","content":"hi"}),
        ];
        let mut arr = input.clone();
        assert!(!dedupe_instruction_blocks(&mut arr), "无重复不应删");
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn dedupe_does_not_touch_non_instruction_developer_or_user() {
        // 非指令签名的 developer / user 消息(不以固定前缀开头)不参与去重,即便内容相同也保留。
        let input = vec![
            json!({"type":"message","role":"user","content":"continue"}),
            json!({"type":"message","role":"user","content":"continue"}),
            json!({"type":"message","role":"developer","content":"某个非 sandbox 的 developer 提示"}),
        ];
        let mut arr = input.clone();
        assert!(!dedupe_instruction_blocks(&mut arr), "非指令块不去重");
        assert_eq!(arr.len(), 3, "普通重复 user / 非签名 developer 全保留");
    }

    #[test]
    fn dedupe_masks_plugin_cache_version_numbers() {
        // 两版 dev 块仅差 plugin cache 目录版本号(≥6 位数字)→ 掩码后归一 → 折成 1。
        let mk = |ver: &str| {
            json!({"type":"message","role":"developer","content":[{"type":"text","text":
                format!("<permissions instructions>\n…\n### Available skills\n- browser: (file: /Users/x/.codex/plugins/cache/openai-bundled/browser/26.623.{ver}/skills/SKILL.md)\n…")}]})
        };
        let mut arr = vec![mk("101652"), mk("141536")];
        assert!(dedupe_instruction_blocks(&mut arr), "仅差版本号应折叠");
        assert_eq!(arr.len(), 1, "版本号噪音掩码后两版归一");
        // keep-latest-at-front:保最新一份(141536)的内容,搬到最前位置。
        assert!(
            message_text(&arr[0]).contains("141536"),
            "keep-latest 保最新版本"
        );
        assert!(!message_text(&arr[0]).contains("101652"), "旧版本号不残留");
    }

    #[test]
    fn dedupe_keeps_both_when_skills_genuinely_differ() {
        // 差异不止数字(真加了一个 skill)→ 键不同 → 两份都留(不误折)。
        let a = json!({"type":"message","role":"developer","content":[{"type":"text","text":
            "<permissions instructions>\n### Available skills\n- browser\n- canvas"}]});
        let b = json!({"type":"message","role":"developer","content":[{"type":"text","text":
            "<permissions instructions>\n### Available skills\n- browser\n- canvas\n- new-skill"}]});
        let mut arr = vec![a, b];
        assert!(!dedupe_instruction_blocks(&mut arr), "技能真变不应折叠");
        assert_eq!(arr.len(), 2, "技能真变两份都留");
    }

    #[test]
    fn dedupe_handles_interleaved_layout() {
        // [review gap5] 真实 Codex 回灌是交错的:[dev, agents, msg, dev, agents, msg, ...],
        // 不是把重复块全堆在最前。HashMap 分组与位置无关,应仍各折成 1 份、对话按原序保留。
        let mut arr = vec![
            dev_block("2026-07-06"),
            agents_block("2026-07-06"),
            json!({"type":"message","role":"user","content":"turn1"}),
            dev_block("2026-07-07"),
            agents_block("2026-07-07"),
            json!({"type":"message","role":"user","content":"turn2"}),
        ];
        assert!(dedupe_instruction_blocks(&mut arr), "交错布局应折叠");
        // dev 1 + agents 1 + 2 条 user = 4。
        assert_eq!(arr.len(), 4, "交错布局仍折成各 1 份");
        assert_eq!(arr[0]["role"], "developer");
        assert!(message_text(&arr[0]).contains("2026-07-07"), "keep-latest");
        // 两条对话按原序保留(未被去重逻辑打乱)。
        let users: Vec<String> = arr
            .iter()
            .filter(|it| it["content"].as_str().is_some())
            .map(|it| it["content"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(users, vec!["turn1", "turn2"], "对话原序保留");
    }

    #[test]
    fn dedupe_does_not_mask_below_6_digit_differences() {
        // [review gap6] flush_digit_run 阈值 >=6:仅差 5 位数字(有意义的短数字,如小计数)不掩码
        // → 两版不归一 → 都留(防误折)。
        let mk = |n: &str| {
            json!({"type":"message","role":"developer","content":[{"type":"text","text":
                format!("<permissions instructions>\ncount={n}")}]})
        };
        let mut arr = vec![mk("10001"), mk("20002")]; // 5 位
        assert!(
            !dedupe_instruction_blocks(&mut arr),
            "5 位数字差异不应掩码折叠"
        );
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn adapt_removes_tool_choice_when_tools_all_dropped() {
        // [silent-failure M2] 原 tools 全是可 drop 类型(tool_search)+ 已设 tool_choice → 适配后
        // tools 空,必须移除 tool_choice,否则 grok 400 "tool_choice set but no tools"。
        let body = serde_json::to_vec(&json!({
            "model":"grok-build",
            "input":[],
            "tool_choice":"none",
            "tools":[{"type":"tool_search","execution":{}}],
        }))
        .unwrap();
        let out =
            adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).expect("改过");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["tools"].as_array().unwrap().len(),
            0,
            "tool_search 被 drop,tools 空"
        );
        assert!(
            v.get("tool_choice").is_none(),
            "tools 空时 tool_choice 必须一并移除"
        );
    }
}
