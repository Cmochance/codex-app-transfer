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
//! - 工具转换**复用 chat 路径的决策**([`convert_responses_tool_to_chat_tool`]:apply_patch
//!   → 单字符串 function、namespace 摊平、tool_search/image_gen drop),再把它输出的 chat 形
//!   `{type:function, function:{…}}` **unwrap 回 responses-flat** `{type:function, …}`(grok
//!   的 function 工具就是 responses-flat);`web_search` 剥成 bare `{type:web_search}`。
//! - 剥 `reasoning.effort`。
//!
//! 真机实证(2026-07-07):适配后 grok `/responses` 返 200 并正常推理 + 调 function 工具。
//! model 映射(gpt-5.x → grok-build)由 resolver 负责,不在此。
//!
//! **响应侧**:exec_command 等普通 function 两边同构、透传即可;apply_patch(此处转成
//! function)若被模型调用,响应侧需把 `function_call` 重打包回 Codex 的 `custom_tool_call`
//! (复用 `responses::converter` 的 apply_patch shim),作紧接精修 —— 本文件只管请求侧。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use serde_json::{json, Value};

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

/// 把 Codex 的 Responses 请求体适配成 grok-build `/responses` 接受的形态。
/// `Some(新 body)` = 改过;`None` = 无需改 / 解析失败(caller 用原 body 透传,零回归)。
pub(crate) fn adapt_grok_build_request_body(body: &Bytes, provider: &Provider) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
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
                // custom(apply_patch)/ namespace:复用 chat 路径转换决策,再 unwrap 回 flat。
                "custom" | "namespace" => {
                    for ct in convert_responses_tool_to_chat_tool(t, Some(provider)) {
                        out.push(unwrap_chat_tool_to_responses_flat(ct));
                    }
                }
                // tool_search / image_generation / 未知:grok 无等价 → drop。
                _ => {}
            }
        }
        if out != tools {
            obj.insert("tools".into(), Value::Array(out));
            changed = true;
        }
    }

    // 2. 剥 reasoning.effort(grok-build 不支持;它自己会 reasoning)。
    if let Some(Value::Object(r)) = obj.get_mut("reasoning") {
        if r.remove("effort").is_some() {
            changed = true;
        }
    }

    changed
        .then(|| serde_json::to_vec(&v).ok().map(Bytes::from))
        .flatten()
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
    fn adapts_tools_and_strips_reasoning_effort() {
        // Codex 请求:function(保留)+ custom apply_patch(→function)+ namespace(摊平)+
        // web_search(→bare)+ tool_search / image_generation(drop);reasoning.effort(剥)。
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
        // apply_patch 转成了 responses-flat function(顶层有 name,无嵌套 function 对象)。
        let ap = tools
            .iter()
            .find(|t| t["name"] == "apply_patch")
            .expect("apply_patch 保留为 function");
        assert_eq!(ap["type"], "function");
        assert!(
            ap.get("function").is_none(),
            "必须 unwrap 成 responses-flat(顶层 name),不留 chat 嵌套"
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
        // reasoning.effort 剥掉,summary 保留。
        assert!(v["reasoning"].get("effort").is_none(), "effort 应剥");
        assert_eq!(v["reasoning"]["summary"], "auto", "reasoning 其它字段保留");
    }

    #[test]
    fn returns_none_when_nothing_to_change() {
        // 无 tools、无 reasoning.effort → 不改(caller 走原 body)。
        let body = serde_json::to_vec(&json!({ "model": "grok-build", "input": [] })).unwrap();
        assert!(adapt_grok_build_request_body(&Bytes::from(body), &grok_provider()).is_none());
    }
}
