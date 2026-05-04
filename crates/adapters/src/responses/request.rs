//! Stage 3.2a stateless · Responses body → Chat body 转换.
//!
//! 对应 Python 端 `backend/responses_adapter.py::convert_responses_to_chat_body`,
//! 但**不**接 `session_cache`(`previous_response_id` 历史恢复留 3.2a' 处理)。
//!
//! 覆盖范围:
//! - 顶层字段:`model` / `instructions` / `input` / `tools` / `tool_choice` /
//!   `max_output_tokens` → `max_tokens` / `stream` / `temperature` / `top_p` /
//!   `seed` / `stop` / `parallel_tool_calls` / `frequency_penalty` /
//!   `presence_penalty` / `user`
//! - input items:`message`(role + content)/ `function_call` /
//!   `function_call_output`
//! - tools:`type=function` 与 `type=custom`(custom 降级为接受单字符串
//!   `input` 的 function)
//!
//! **暂不处理**(留 3.2a' / 3.3c):
//! - `input_image` / `input_file` / `input_audio` / `input_video`
//! - `reasoning` 顶层字段(各 provider 默认行为差异大,Stage 3.3c 在
//!   provider_workarounds 里处理)
//! - `text.format` → `response_format`(罕见,留后做)
//! - `previous_response_id`(stateless 阶段直接丢弃,不报错,不恢复历史)
//! - `store` / `metadata` / `prediction` / `service_tier` / `modalities` /
//!   `audio`(罕见,留后做)
//! - 多轮 user/assistant 合并(Python 有 `merge_consecutive_*_messages`,
//!   stateless 阶段不必要,Codex CLI 自己不会发出连续同角色消息)

use serde_json::{json, Value};

use crate::types::AdapterError;

/// 把 Responses API 请求体转换成 OpenAI Chat Completions 请求体.
pub fn responses_body_to_chat_body(input: &Value) -> Result<Value, AdapterError> {
    let body = input
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("body 必须是 JSON 对象".into()))?;

    let mut result = serde_json::Map::new();

    // model
    if let Some(m) = body.get("model") {
        result.insert("model".into(), m.clone());
    }

    // messages: instructions(优先,作为 system 头) + input 展开
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = body.get("instructions").and_then(|v| v.as_str()) {
        let trimmed = instr.trim();
        if !trimmed.is_empty() {
            messages.push(json!({ "role": "system", "content": trimmed }));
        }
    }
    if let Some(input_field) = body.get("input") {
        for msg in input_field_to_messages(input_field) {
            messages.push(msg);
        }
    }
    if !messages.is_empty() {
        result.insert("messages".into(), Value::Array(messages));
    }

    // tools(只接受 function / custom,其余 Responses 专属类型丢弃)
    if let Some(Value::Array(tools)) = body.get("tools") {
        let chat_tools: Vec<Value> = tools
            .iter()
            .filter_map(convert_responses_tool_to_chat_tool)
            .collect();
        if !chat_tools.is_empty() {
            result.insert("tools".into(), Value::Array(chat_tools));
        }
    }

    // tool_choice 透传
    if let Some(tc) = body.get("tool_choice") {
        result.insert("tool_choice".into(), tc.clone());
    }

    // max_output_tokens → max_tokens
    if let Some(v) = body.get("max_output_tokens") {
        result.insert("max_tokens".into(), v.clone());
    }

    // 透传白名单(已被处理过的不重复)
    const PASSTHROUGH: &[&str] = &[
        "temperature",
        "top_p",
        "seed",
        "stop",
        "parallel_tool_calls",
        "frequency_penalty",
        "presence_penalty",
        "user",
        "n",
        "logprobs",
        "top_logprobs",
        "response_format",
        "reasoning_effort",
        "max_completion_tokens",
    ];
    for key in PASSTHROUGH {
        if let Some(v) = body.get(*key) {
            result.entry((*key).to_owned()).or_insert_with(|| v.clone());
        }
    }

    // stream + stream_options.include_usage
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    result.insert("stream".into(), Value::Bool(stream));
    if stream {
        result.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    Ok(Value::Object(result))
}

/// 把 `body.input` 字段(可能是 string 也可能是 array)展开成 messages 列表.
fn input_field_to_messages(input: &Value) -> Vec<Value> {
    match input {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "role": "user", "content": s })]
            }
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(obj) = item.as_object() {
                    out.extend(input_item_to_messages(obj));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// 单个 Responses input item → 一条或多条 Chat message.
fn input_item_to_messages(item: &serde_json::Map<String, Value>) -> Vec<Value> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = normalize_message_content(item.get("content").unwrap_or(&Value::Null));
            vec![json!({ "role": role, "content": content })]
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_owned();
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            vec![json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": if call_id.is_empty() { "call_unknown".to_owned() } else { call_id },
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }],
            })]
        }
        "function_call_output" => {
            // call_id 字段在 Codex CLI 历史里偶尔会以 tool_call_id / id 别名出现
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("tool_call_id").and_then(|v| v.as_str()))
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_owned();
            let output_value = item
                .get("output")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let output_str = match output_value {
                Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            vec![json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output_str,
            })]
        }
        // 罕见类型(input_image / input_file / reasoning / 内置 call)在 stateless
        // 阶段静默忽略,留 Stage 3.2a' 接入
        _ => {
            // 兜底:若有 content 字段,作为 user message 透传;否则丢弃
            if let Some(content) = item.get("content") {
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                vec![json!({ "role": role, "content": normalize_message_content(content) })]
            } else {
                Vec::new()
            }
        }
    }
}

/// Responses message.content 可能是 string 或 [{type, text/image_url}].
/// stateless 阶段:string 保留;text 块拼成 string;含 image_url 的块降级为
/// Chat 多模态格式(`[{type: "text", text}, {type: "image_url", image_url}]`).
fn normalize_message_content(content: &Value) -> Value {
    match content {
        Value::String(s) => Value::String(s.clone()),
        Value::Array(arr) => {
            // 全是 text 块:拼成单 string(Codex CLI 大多数场景)
            // 任一块是非文本:转成 Chat 多模态 array
            let mut text_only = true;
            for block in arr {
                let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if !matches!(t, "input_text" | "output_text" | "text") {
                    text_only = false;
                    break;
                }
            }
            if text_only {
                let mut combined = String::new();
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        if !combined.is_empty() {
                            combined.push('\n');
                        }
                        combined.push_str(text);
                    }
                }
                Value::String(combined)
            } else {
                let mut chat_blocks: Vec<Value> = Vec::new();
                for block in arr {
                    if let Some(b) = responses_block_to_chat_block(block) {
                        chat_blocks.push(b);
                    }
                }
                Value::Array(chat_blocks)
            }
        }
        Value::Null => Value::String(String::new()),
        other => Value::String(other.to_string()),
    }
}

/// 单个 Responses content block → Chat content block.
fn responses_block_to_chat_block(block: &Value) -> Option<Value> {
    let obj = block.as_object()?;
    let t = obj.get("type").and_then(|v| v.as_str())?;
    match t {
        "input_text" | "output_text" | "text" => {
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "type": "text", "text": text }))
        }
        "input_image" => {
            let url = obj
                .get("image_url")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("url").and_then(|v| v.as_str()))
                .unwrap_or("");
            let detail = obj.get("detail").and_then(|v| v.as_str()).unwrap_or("auto");
            Some(json!({
                "type": "image_url",
                "image_url": { "url": url, "detail": detail },
            }))
        }
        // 其他块暂时丢弃(stateless 阶段)
        _ => None,
    }
}

/// Responses tool 定义 → Chat tool 定义.
fn convert_responses_tool_to_chat_tool(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    let ttype = obj.get("type").and_then(|v| v.as_str())?;
    match ttype {
        "function" => {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut parameters = obj.get("parameters").cloned().unwrap_or_else(|| json!({}));
            if let Some(po) = parameters.as_object_mut() {
                if !po.contains_key("type") {
                    po.insert("type".into(), Value::String("object".into()));
                }
            }
            let strict = obj.get("strict").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                    "strict": strict,
                },
            }))
        }
        "custom" => {
            // Custom tool(无 JSON schema)降级为接受单字符串 input 的 function
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Free-form input passed verbatim to the tool.",
                            }
                        },
                        "required": ["input"],
                    },
                    "strict": false,
                },
            }))
        }
        // Responses 专属类型(local_shell / web_search* / file_search /
        // computer_use* / code_interpreter / image_generation / mcp 等)
        // Chat 端点不认,丢弃
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(body: Value) -> Value {
        responses_body_to_chat_body(&body).unwrap()
    }

    #[test]
    fn string_input_becomes_single_user_message() {
        let out = convert(json!({
            "model": "x",
            "input": "hello"
        }));
        assert_eq!(out["model"], "x");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
        // stream 默认 false,但 stream 字段总会被设上
        assert_eq!(out["stream"], false);
        assert!(out.get("stream_options").is_none());
    }

    #[test]
    fn instructions_prepended_as_system_message() {
        let out = convert(json!({
            "model": "x",
            "instructions": "Be concise.",
            "input": "hi"
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be concise.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn empty_instructions_is_skipped() {
        let out = convert(json!({
            "instructions": "   ",
            "input": "hi"
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn array_input_message_item_passthrough() {
        let out = convert(json!({
            "input": [
                {"type": "message", "role": "user", "content": "hello"}
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn message_with_text_blocks_concatenates_to_string() {
        let out = convert(json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "line1"},
                    {"type": "input_text", "text": "line2"}
                ]
            }]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], "line1\nline2");
    }

    #[test]
    fn message_with_image_block_becomes_chat_multimodal_array() {
        let out = convert(json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what is this?"},
                    {"type": "input_image", "image_url": "https://x.test/i.png", "detail": "high"}
                ]
            }]
        }));
        let content = &out["messages"][0]["content"];
        let arr = content.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "what is this?");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "https://x.test/i.png");
        assert_eq!(arr[1]["image_url"]["detail"], "high");
    }

    #[test]
    fn function_call_becomes_assistant_with_tool_calls() {
        let out = convert(json!({
            "input": [{
                "type": "function_call",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"loc\":\"NYC\"}"
            }]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "");
        assert_eq!(msg["tool_calls"][0]["id"], "call_abc");
        assert_eq!(msg["tool_calls"][0]["type"], "function");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            msg["tool_calls"][0]["function"]["arguments"],
            "{\"loc\":\"NYC\"}"
        );
    }

    #[test]
    fn function_call_output_becomes_tool_message() {
        let out = convert(json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "call_abc",
                "output": "sunny"
            }]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["tool_call_id"], "call_abc");
        assert_eq!(msg["content"], "sunny");
    }

    #[test]
    fn function_call_output_non_string_is_json_serialized() {
        let out = convert(json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "c",
                "output": {"temp": 72}
            }]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["content"], "{\"temp\":72}");
    }

    #[test]
    fn tools_function_passes_through() {
        let out = convert(json!({
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "fetch forecast",
                "parameters": {
                    "type": "object",
                    "properties": {"loc": {"type": "string"}},
                    "required": ["loc"]
                },
                "strict": true
            }]
        }));
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "get_weather");
        assert_eq!(tool["function"]["description"], "fetch forecast");
        assert_eq!(tool["function"]["strict"], true);
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tools_parameters_default_type_object() {
        let out = convert(json!({
            "tools": [{
                "type": "function",
                "name": "f",
                "parameters": {"properties": {}}
            }]
        }));
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["type"], "object",
            "缺 type 字段时应自动补 object"
        );
    }

    #[test]
    fn tools_custom_type_is_lowered_to_function_with_input() {
        let out = convert(json!({
            "tools": [{
                "type": "custom",
                "name": "free_text_tool",
                "description": "anything"
            }]
        }));
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "free_text_tool");
        assert_eq!(
            tool["function"]["parameters"]["properties"]["input"]["type"],
            "string"
        );
        assert_eq!(tool["function"]["parameters"]["required"][0], "input");
    }

    #[test]
    fn tools_unknown_responses_only_types_dropped() {
        let out = convert(json!({
            "tools": [
                {"type": "function", "name": "keep_me"},
                {"type": "web_search_preview"},
                {"type": "file_search"},
                {"type": "computer_use_preview"}
            ]
        }));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "keep_me");
    }

    #[test]
    fn max_output_tokens_renamed_to_max_tokens() {
        let out = convert(json!({"max_output_tokens": 256}));
        assert_eq!(out["max_tokens"], 256);
        assert!(out.get("max_output_tokens").is_none());
    }

    #[test]
    fn stream_true_adds_stream_options_include_usage() {
        let out = convert(json!({"stream": true, "input": "hi"}));
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn passthrough_fields_kept() {
        let out = convert(json!({
            "temperature": 0.7,
            "top_p": 0.95,
            "seed": 42,
            "stop": ["END"],
            "parallel_tool_calls": true,
            "frequency_penalty": 0.1,
            "presence_penalty": 0.2,
            "user": "u-1",
            "input": "hi"
        }));
        assert_eq!(out["temperature"], 0.7);
        assert_eq!(out["top_p"], 0.95);
        assert_eq!(out["seed"], 42);
        assert_eq!(out["stop"][0], "END");
        assert_eq!(out["parallel_tool_calls"], true);
        assert_eq!(out["frequency_penalty"], 0.1);
        assert_eq!(out["presence_penalty"], 0.2);
        assert_eq!(out["user"], "u-1");
    }

    #[test]
    fn previous_response_id_silently_dropped_for_now() {
        let out = convert(json!({
            "previous_response_id": "resp_abc",
            "input": "hi"
        }));
        // stateless 阶段不报错、不恢复历史,只丢字段
        assert!(out.get("previous_response_id").is_none());
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn full_codex_cli_loop_pattern() {
        // 真实 Codex CLI 一次工具循环的形态:instructions + 用户问题 +
        // 模型上一轮的 function_call + 用户提供的 function_call_output + 新提问
        let out = convert(json!({
            "model": "gpt-x",
            "instructions": "You are an assistant.",
            "input": [
                {"type": "message", "role": "user", "content": "what's the weather?"},
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"loc\":\"NYC\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "{\"temp\":72,\"cond\":\"sunny\"}"
                },
                {"type": "message", "role": "user", "content": "thanks!"}
            ],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"loc": {"type": "string"}}}
            }],
            "stream": true,
            "max_output_tokens": 1024,
            "temperature": 0.0
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "system + user + assistant + tool + user");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "thanks!");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["temperature"], 0.0);
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn non_object_body_rejected() {
        let err = responses_body_to_chat_body(&json!("not an object"));
        assert!(matches!(err, Err(AdapterError::BadRequest(_))));
    }
}
