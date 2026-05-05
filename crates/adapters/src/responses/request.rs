//! Stage 3.2a · Responses body → Chat body 转换.
//!
//! 对应 Python 端 `backend/responses_adapter.py::convert_responses_to_chat_body`,
//! 并恢复旧版 `ResponseSessionCache` 的 `previous_response_id` 历史拼接。
//!
//! 覆盖范围:
//! - 顶层字段:`model` / `instructions` / `input` / `tools` / `tool_choice` /
//!   `max_output_tokens` → `max_tokens` / `stream` / `temperature` / `top_p` /
//!   `seed` / `stop` / `parallel_tool_calls` / `frequency_penalty` /
//!   `presence_penalty` / `user`
//! - input items:`message`(role + content)/ `function_call` /
//!   `function_call_output` / `input_image` / `input_file` / `input_audio` /
//!   `input_video`
//! - tools:`type=function` 与 `type=custom`(custom 降级为接受单字符串
//!   `input` 的 function)
//! - `text.format` → `response_format` / `reasoning` → `reasoning_effort`
//! - `store` / `metadata` / `prediction` / `service_tier` / `modalities` /
//!   `audio`
//! - 多轮 user/assistant 合并

use codex_app_transfer_registry::Provider;
use serde_json::{json, Value};

use crate::types::{AdapterError, ResponseSessionPlan};

use super::session::ResponseSessionCache;

#[derive(Debug, Clone)]
pub struct ResponsesBodyConversion {
    pub body: Value,
    pub response_session: ResponseSessionPlan,
}

/// 把 Responses API 请求体转换成 OpenAI Chat Completions 请求体.
pub fn responses_body_to_chat_body(input: &Value) -> Result<Value, AdapterError> {
    responses_body_to_chat_body_for_provider(input, None)
}

/// 把 Responses API 请求体转换成 OpenAI Chat Completions 请求体.
///
/// provider-aware 路径用于恢复 Python 版 DeepSeek/Kimi thinking 历史修复:
/// Codex 续轮工具调用时,部分上游会要求 assistant.tool_calls 历史带
/// `reasoning_content`;DeepSeek 的 thinking 还可能由 provider.requestOptions
/// 开启,而不是出现在本次请求体里。
pub fn responses_body_to_chat_body_for_provider(
    input: &Value,
    provider: Option<&Provider>,
) -> Result<Value, AdapterError> {
    Ok(responses_body_to_chat_body_for_provider_with_session(input, provider, None)?.body)
}

pub fn responses_body_to_chat_body_for_provider_with_session(
    input: &Value,
    provider: Option<&Provider>,
    session_cache: Option<&ResponseSessionCache>,
) -> Result<ResponsesBodyConversion, AdapterError> {
    let body = input
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("body 必须是 JSON 对象".into()))?;

    let mut result = serde_json::Map::new();

    // model
    if let Some(m) = body.get("model") {
        result.insert("model".into(), m.clone());
    }

    // messages: instructions(优先,作为 system 头) + input 展开;如果存在
    // previous_response_id 且 session cache 命中,先恢复历史再追加本轮 input。
    let mut messages = build_messages_from_input(input, session_cache);
    messages = merge_consecutive_user_messages(messages);
    messages = merge_consecutive_assistant_messages(messages);
    repair_tool_call_ids(&mut messages);
    ensure_thinking_tool_call_reasoning(&mut messages, input, provider);
    convert_developer_to_system_if_needed(&mut messages, provider);
    let session_messages = messages.clone();
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

    // tool_choice 规范化
    if let Some(tc) = body.get("tool_choice") {
        result.insert("tool_choice".into(), normalize_tool_choice(tc));
    }

    // text.format → response_format
    if let Some(response_format) = body.get("text").and_then(build_response_format) {
        result.insert("response_format".into(), response_format);
    }

    // reasoning → reasoning_effort
    if let Some(reasoning_effort) = body.get("reasoning").and_then(build_reasoning_effort) {
        result.insert("reasoning_effort".into(), reasoning_effort);
    }

    // max_output_tokens → max_tokens
    if let Some(v) = body.get("max_output_tokens") {
        result.insert("max_tokens".into(), v.clone());
    }

    // 特殊参数处理(store / metadata / prediction / service_tier / modalities / audio)
    if let Some(v) = body.get("store").and_then(handle_store_param) {
        result.insert("store".into(), v);
    }
    if let Some(v) = body.get("metadata").and_then(handle_metadata_param) {
        result.insert("metadata".into(), v);
    }
    if let Some(v) = body.get("prediction").and_then(handle_prediction_param) {
        result.insert("prediction".into(), v);
    }
    if let Some(v) = body.get("service_tier").and_then(handle_service_tier) {
        result.insert("service_tier".into(), v);
    }
    if let Some(v) = body.get("modalities").and_then(handle_modalities) {
        result.insert("modalities".into(), v);
    }
    if let Some(v) = body.get("audio").and_then(handle_audio_param) {
        result.insert("audio".into(), v);
    }

    // 透传白名单(已被处理过的不重复)
    const PASSTHROUGH: &[&str] = &[
        "temperature",
        "top_p",
        "seed",
        "stop",
        "logit_bias",
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
        "safety_identifier",
        "safety_settings",
        "context",
        "truncate",
        "prompt_truncation",
        "extra_headers",
        "extra_query",
        "extra_body",
        "timeout",
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

    Ok(ResponsesBodyConversion {
        body: Value::Object(result),
        response_session: ResponseSessionPlan {
            response_id: response_id_for_session(),
            messages: session_messages,
        },
    })
}

fn response_id_for_session() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("resp_{nanos:x}")
}

fn build_messages_from_input(
    body: &Value,
    session_cache: Option<&ResponseSessionCache>,
) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(msg) = body
        .get("instructions")
        .and_then(build_instructions_message)
    {
        messages.push(msg);
    }

    let current_messages = body
        .get("input")
        .map(input_field_to_messages)
        .unwrap_or_default();
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if !previous_response_id.is_empty() {
        if let Some(cache) = session_cache {
            let merged = cache.build_messages_with_history(previous_response_id, &current_messages);
            let history_has_system = merged.iter().any(|msg| {
                matches!(
                    msg.get("role").and_then(|v| v.as_str()),
                    Some("system" | "developer")
                )
            });
            if history_has_system
                && messages
                    .first()
                    .and_then(|msg| msg.get("role"))
                    .and_then(|v| v.as_str())
                    == Some("system")
            {
                messages.remove(0);
            }
            messages.extend(merged);
            return messages;
        }
    }

    messages.extend(current_messages);
    messages
}

fn build_instructions_message(instructions: &Value) -> Option<Value> {
    match instructions {
        Value::Null => None,
        Value::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(json!({ "role": "system", "content": s }))
            }
        }
        Value::Object(obj) => {
            if let Some(text) = obj
                .get("text")
                .or_else(|| obj.get("content"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                return Some(json!({ "role": "system", "content": text }));
            }
            Some(json!({
                "role": "system",
                "content": serde_json::to_string(instructions).unwrap_or_else(|_| instructions.to_string()),
            }))
        }
        other => {
            let content = value_to_chat_string(other);
            if content.trim().is_empty() {
                None
            } else {
                Some(json!({ "role": "system", "content": content }))
            }
        }
    }
}

/// 把 `body.input` 字段(可能是 string 也可能是 array)展开成 messages 列表.
fn input_field_to_messages(input: &Value) -> Vec<Value> {
    let items = extract_input_items(input);
    let mut out = Vec::new();
    let mut pending_reasoning: Option<String> = None;

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
            pending_reasoning = Some(extract_reasoning_text(obj));
            continue;
        }
        let mut item_messages = input_item_to_messages(obj);
        for msg in &mut item_messages {
            if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                if let Some(reasoning) = pending_reasoning.take() {
                    let has_reasoning = msg
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty());
                    if !has_reasoning {
                        let repaired = if reasoning.trim().is_empty() {
                            " ".to_owned()
                        } else {
                            reasoning
                        };
                        if let Some(msg_obj) = msg.as_object_mut() {
                            msg_obj.insert("reasoning_content".into(), Value::String(repaired));
                        }
                    }
                }
            } else {
                pending_reasoning = None;
            }
        }
        out.extend(item_messages);
    }

    out
}

fn extract_input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::Null => Vec::new(),
        Value::String(s) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "message", "role": "user", "content": s })]
            }
        }
        Value::Object(obj) => {
            if obj.contains_key("type") {
                vec![Value::Object(obj.clone())]
            } else {
                vec![json!({
                    "type": "message",
                    "role": obj.get("role").and_then(|v| v.as_str()).unwrap_or("user"),
                    "content": obj.get("content").cloned().unwrap_or_else(|| Value::Object(obj.clone())),
                })]
            }
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::Object(obj) if obj.contains_key("type") => Some(Value::Object(obj.clone())),
                Value::Object(obj) => Some(json!({
                    "type": "message",
                    "role": obj.get("role").and_then(|v| v.as_str()).unwrap_or("user"),
                    "content": obj.get("content").cloned().unwrap_or_else(|| Value::Object(obj.clone())),
                })),
                Value::String(s) => Some(json!({ "type": "message", "role": "user", "content": s })),
                other => Some(json!({ "type": "message", "role": "user", "content": value_to_chat_string(other) })),
            })
            .collect(),
        other => vec![json!({ "type": "message", "role": "user", "content": value_to_chat_string(other) })],
    }
}

fn extract_reasoning_text(item: &serde_json::Map<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(summaries) = item.get("summary").and_then(|v| v.as_array()) {
        for summary in summaries {
            if let Some(text) = summary.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    parts.push(text.to_owned());
                }
            }
        }
    }

    if parts.is_empty() {
        if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(text.to_owned());
                    }
                }
            }
        }
    }

    parts.join("\n")
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
        "input_image" => {
            let image_url = item
                .get("image_url")
                .or_else(|| item.get("url"))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            let detail = item
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("auto");
            vec![json!({
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": image_url_for_chat(image_url, detail),
                }],
            })]
        }
        "input_file" => convert_file_item_to_message(item),
        "input_audio" => {
            let data = item.get("data").cloned().unwrap_or_else(|| json!(""));
            let fmt = item.get("format").and_then(|v| v.as_str()).unwrap_or("wav");
            let mime_type = item
                .get("mime_type")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("audio/{fmt}"));
            vec![json!({
                "role": "user",
                "content": [{
                    "type": "input_audio",
                    "input_audio": {
                        "data": data,
                        "format": fmt,
                        "mime_type": mime_type,
                    },
                }],
            })]
        }
        "input_video" => {
            let video_url = item
                .get("video_url")
                .or_else(|| item.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if video_url.is_empty() {
                vec![json!({ "role": "user", "content": "[Video input]" })]
            } else {
                vec![json!({
                    "role": "user",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": video_url, "detail": "auto" },
                    }],
                })]
            }
        }
        "file_search_call"
        | "web_search_call"
        | "computer_call"
        | "code_interpreter_call"
        | "image_generation_call" => {
            vec![json!({ "role": "user", "content": format!("[{item_type}]") })]
        }
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

fn convert_file_item_to_message(item: &serde_json::Map<String, Value>) -> Vec<Value> {
    let file_id = item
        .get("file_id")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("id").and_then(|v| v.as_str()))
        .unwrap_or("");
    let file_data = item.get("file_data").and_then(|v| v.as_str());
    let filename = item
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let mime_type = item
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream");

    if let Some(data) = file_data.filter(|s| !s.is_empty()) {
        let data_uri = format!("data:{mime_type};base64,{data}");
        return vec![json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": data_uri, "detail": "auto" },
            }],
        })];
    }

    if !file_id.is_empty() && filename != "unknown" {
        return vec![
            json!({ "role": "user", "content": format!("[File: {filename} (id={file_id})]") }),
        ];
    }
    if !file_id.is_empty() {
        return vec![json!({ "role": "user", "content": format!("[File id={file_id}]") })];
    }
    if filename != "unknown" {
        return vec![json!({ "role": "user", "content": format!("[File: {filename}]") })];
    }
    vec![json!({ "role": "user", "content": "[File]" })]
}

fn image_url_for_chat(value: Value, detail: &str) -> Value {
    match value {
        Value::Object(_) => value,
        Value::String(url) => json!({ "url": url, "detail": detail }),
        other => json!({ "url": value_to_chat_string(&other), "detail": detail }),
    }
}

fn merge_consecutive_user_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user"
            || result
                .last()
                .and_then(|prev| prev.get("role"))
                .and_then(|v| v.as_str())
                != Some("user")
        {
            result.push(msg);
            continue;
        }

        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        let Some(prev_obj) = result.last_mut().and_then(|prev| prev.as_object_mut()) else {
            continue;
        };
        let prev_content = prev_obj.get("content").cloned().unwrap_or(Value::Null);
        let merged = merge_user_content(prev_content, content);
        prev_obj.insert("content".into(), merged);
    }
    result
}

fn merge_user_content(prev: Value, current: Value) -> Value {
    if prev.is_array() || current.is_array() {
        let mut arr = normalize_content_array(&prev);
        arr.extend(normalize_content_array(&current));
        Value::Array(arr)
    } else if let (Some(prev), Some(current)) = (prev.as_str(), current.as_str()) {
        Value::String(format!("{prev}\n{current}"))
    } else if !current.is_null() {
        current
    } else {
        prev
    }
}

fn merge_consecutive_assistant_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant"
            || result
                .last()
                .and_then(|prev| prev.get("role"))
                .and_then(|v| v.as_str())
                != Some("assistant")
        {
            result.push(msg);
            continue;
        }

        let Some(prev_obj) = result.last_mut().and_then(|prev| prev.as_object_mut()) else {
            continue;
        };
        if let Some(content) = msg.get("content").filter(|v| !v.is_null()) {
            let prev_content = prev_obj.get("content").cloned().unwrap_or(Value::Null);
            let merged = merge_assistant_content(prev_content, content.clone());
            prev_obj.insert("content".into(), merged);
        }
        if let Some(new_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            let entry = prev_obj
                .entry("tool_calls")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(existing) = entry.as_array_mut() {
                existing.extend(new_calls.iter().cloned());
            }
        }
        if let Some(reasoning) = msg.get("reasoning_content") {
            if let Some(prev_reasoning) = prev_obj.get("reasoning_content").and_then(|v| v.as_str())
            {
                if let Some(current) = reasoning.as_str() {
                    prev_obj.insert(
                        "reasoning_content".into(),
                        Value::String(format!("{prev_reasoning}\n{current}")),
                    );
                }
            } else {
                prev_obj.insert("reasoning_content".into(), reasoning.clone());
            }
        }
        if !prev_obj.contains_key("content") {
            prev_obj.insert("content".into(), Value::String(String::new()));
        }
    }
    result
}

fn merge_assistant_content(prev: Value, current: Value) -> Value {
    if let (Some(prev), Some(current)) = (prev.as_str(), current.as_str()) {
        if prev.is_empty() {
            Value::String(current.to_owned())
        } else if current.is_empty() {
            Value::String(prev.to_owned())
        } else {
            Value::String(format!("{prev}\n{current}"))
        }
    } else if !current.is_null() {
        current
    } else {
        prev
    }
}

fn convert_developer_to_system_if_needed(messages: &mut [Value], provider: Option<&Provider>) {
    let keep_developer = provider.is_some_and(provider_is_openai_official);
    if keep_developer {
        return;
    }
    for msg in messages {
        if msg.get("role").and_then(|v| v.as_str()) == Some("developer") {
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("role".into(), Value::String("system".into()));
            }
        }
    }
}

fn provider_is_openai_official(provider: &Provider) -> bool {
    let name = provider.name.to_ascii_lowercase();
    name.contains("openai") && !name.contains("azure")
}

fn repair_tool_call_ids(messages: &mut Vec<Value>) {
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut repaired: Vec<Value> = Vec::with_capacity(messages.len());

    for mut msg in messages.drain(..) {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if role == "assistant" {
            if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                pending_call_ids = calls
                    .iter()
                    .filter_map(|call| call.get("id").and_then(|id| id.as_str()))
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            repaired.push(msg);
            continue;
        }

        if role == "tool" {
            let existing = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            if existing.is_empty() {
                if pending_call_ids.is_empty() {
                    continue;
                }
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert(
                        "tool_call_id".into(),
                        Value::String(pending_call_ids.remove(0)),
                    );
                }
            } else if let Some(pos) = pending_call_ids.iter().position(|id| id == &existing) {
                pending_call_ids.remove(pos);
            }
        }

        if matches!(role.as_str(), "user" | "system" | "developer") {
            pending_call_ids.clear();
        }

        repaired.push(msg);
    }

    *messages = repaired;
}

fn ensure_thinking_tool_call_reasoning(
    messages: &mut [Value],
    body: &Value,
    provider: Option<&Provider>,
) {
    if !request_thinking_enabled(body, provider) {
        return;
    }

    let has_tool_loop = messages.iter().any(|msg| {
        msg.get("role").and_then(|v| v.as_str()) == Some("tool")
            || (msg.get("role").and_then(|v| v.as_str()) == Some("assistant")
                && msg
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .is_some_and(|calls| !calls.is_empty()))
    });
    if !has_tool_loop {
        return;
    }

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let has_tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .is_some_and(|calls| !calls.is_empty());
        if !has_tool_calls {
            continue;
        }
        let has_reasoning = msg
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        if !has_reasoning {
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("reasoning_content".into(), Value::String(" ".into()));
            }
        }
    }
}

fn request_thinking_enabled(body: &Value, provider: Option<&Provider>) -> bool {
    if body.get("reasoning").is_some() {
        return true;
    }
    provider
        .is_some_and(|p| provider_looks_like(p, "deepseek") && provider_chat_thinking_enabled(p))
}

fn provider_looks_like(provider: &Provider, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    [&provider.id, &provider.name, &provider.base_url]
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&needle))
}

fn provider_chat_thinking_enabled(provider: &Provider) -> bool {
    if thinking_value_enabled(provider.request_options.get("thinking"))
        || provider.request_options.get("reasoning_effort").is_some()
    {
        return true;
    }

    let Some(chat_options) = provider
        .request_options
        .get("chat")
        .and_then(|v| v.as_object())
    else {
        return false;
    };

    thinking_value_enabled(chat_options.get("thinking"))
        || chat_options.get("reasoning_effort").is_some()
}

fn thinking_value_enabled(thinking: Option<&Value>) -> bool {
    match thinking {
        Some(Value::Object(thinking)) => {
            let thinking_type = thinking
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !thinking_type.is_empty() && thinking_type != "disabled" {
                return true;
            }
        }
        Some(Value::Bool(true)) => return true,
        Some(other) if !other.is_null() => return true,
        _ => {}
    }
    false
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
                    if let Some(text) = block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| block.as_str())
                    {
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
        other => Value::String(value_to_chat_string(other)),
    }
}

fn normalize_content_array(content: &Value) -> Vec<Value> {
    match content {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .filter_map(responses_block_to_chat_block)
            .collect(),
        other => responses_block_to_chat_block(other).into_iter().collect(),
    }
}

/// 单个 Responses content block → Chat content block.
fn responses_block_to_chat_block(block: &Value) -> Option<Value> {
    if let Some(s) = block.as_str() {
        return Some(json!({ "type": "text", "text": s }));
    }
    let Some(obj) = block.as_object() else {
        return Some(json!({ "type": "text", "text": value_to_chat_string(block) }));
    };
    let t = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match t {
        "input_text" | "output_text" | "text" => {
            let text = obj
                .get("text")
                .map(value_to_chat_string)
                .unwrap_or_default();
            Some(json!({ "type": "text", "text": text }))
        }
        "input_image" => {
            let detail = obj.get("detail").and_then(|v| v.as_str()).unwrap_or("auto");
            let image_url = obj
                .get("image_url")
                .or_else(|| obj.get("url"))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            Some(json!({
                "type": "image_url",
                "image_url": image_url_for_chat(image_url, detail),
            }))
        }
        "image_url" => Some(block.clone()),
        "input_audio" => {
            let audio = obj.get("input_audio").cloned().unwrap_or_else(|| {
                json!({
                    "data": obj.get("data").cloned().unwrap_or_else(|| json!("")),
                    "format": obj.get("format").and_then(|v| v.as_str()).unwrap_or("wav"),
                })
            });
            Some(json!({ "type": "input_audio", "input_audio": audio }))
        }
        "refusal" => Some(json!({
            "type": "refusal",
            "refusal": obj.get("refusal").cloned().unwrap_or_else(|| json!("")),
        })),
        "input_file" => {
            let marker = obj
                .get("filename")
                .or_else(|| obj.get("file_id"))
                .map(value_to_chat_string)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "input_file".into());
            Some(json!({ "type": "text", "text": format!("[input_file: {marker}]") }))
        }
        "input_video" => {
            let url = obj
                .get("video_url")
                .or_else(|| obj.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if url.is_empty() {
                Some(json!({ "type": "text", "text": "[Video input]" }))
            } else {
                Some(json!({
                    "type": "image_url",
                    "image_url": { "url": url, "detail": "auto" },
                }))
            }
        }
        "" if obj.contains_key("text") => Some(json!({
            "type": "text",
            "text": obj.get("text").map(value_to_chat_string).unwrap_or_default(),
        })),
        "" if obj.contains_key("image_url") => Some({
            let mut cloned = obj.clone();
            cloned.insert("type".into(), Value::String("image_url".into()));
            Value::Object(cloned)
        }),
        "" if obj.contains_key("input_audio") => Some({
            let mut cloned = obj.clone();
            cloned.insert("type".into(), Value::String("input_audio".into()));
            Value::Object(cloned)
        }),
        _ => Some(json!({ "type": "text", "text": value_to_chat_string(block) })),
    }
}

fn build_response_format(text_config: &Value) -> Option<Value> {
    let fmt = text_config.get("format")?.as_object()?;
    let fmt_type = fmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match fmt_type {
        "json_schema" => Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": fmt.get("name").and_then(|v| v.as_str()).unwrap_or("response_schema"),
                "schema": fmt.get("schema").cloned().unwrap_or_else(|| json!({})),
                "strict": fmt.get("strict").and_then(|v| v.as_bool()).unwrap_or(false),
            },
        })),
        "json_object" => Some(json!({ "type": "json_object" })),
        "text" => None,
        _ if fmt.contains_key("schema") => Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": fmt.get("name").and_then(|v| v.as_str()).unwrap_or("response_schema"),
                "schema": fmt.get("schema").cloned().unwrap_or_else(|| json!({})),
                "strict": fmt.get("strict").and_then(|v| v.as_bool()).unwrap_or(false),
            },
        })),
        _ => None,
    }
}

fn build_reasoning_effort(reasoning: &Value) -> Option<Value> {
    match reasoning {
        Value::String(s) => normalize_chat_reasoning_effort(s),
        Value::Object(obj) => {
            if let Some(effort) = obj.get("effort") {
                if let Some(effort) = effort.as_str() {
                    return normalize_chat_reasoning_effort(effort);
                }
                return Some(effort.clone());
            }
            if obj.contains_key("summary") {
                return Some(reasoning.clone());
            }
            Some(reasoning.clone())
        }
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn normalize_chat_reasoning_effort(effort: &str) -> Option<Value> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" | "medium" | "high" => {
            Some(Value::String(effort.trim().to_ascii_lowercase()))
        }
        "xhigh" | "max" | "highest" => Some(Value::String("high".into())),
        "none" | "off" | "auto" | "" => None,
        _ => None,
    }
}

fn normalize_tool_choice(tool_choice: &Value) -> Value {
    let Some(obj) = tool_choice.as_object() else {
        return tool_choice.clone();
    };
    if obj
        .get("function")
        .and_then(|v| v.as_object())
        .and_then(|f| f.get("name"))
        .is_some()
    {
        return tool_choice.clone();
    }
    match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "auto" => Value::String("auto".into()),
        "none" => Value::String("none".into()),
        "required" | "tool" | "any" => Value::String("required".into()),
        "function" if obj.get("function").is_none() => Value::String("required".into()),
        _ => tool_choice.clone(),
    }
}

fn handle_store_param(value: &Value) -> Option<Value> {
    value.as_bool().map(Value::Bool)
}

fn handle_metadata_param(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let mut cleaned = serde_json::Map::new();
    for (idx, (key, value)) in obj.iter().enumerate() {
        if idx >= 16 {
            break;
        }
        let key = key.chars().take(64).collect::<String>();
        let value = value_to_chat_string(value)
            .chars()
            .take(512)
            .collect::<String>();
        cleaned.insert(key, Value::String(value));
    }
    if cleaned.is_empty() {
        None
    } else {
        Some(Value::Object(cleaned))
    }
}

fn handle_prediction_param(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let prediction_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let content = obj.get("content")?;
    if prediction_type == "content" {
        return Some(json!({ "type": "content", "content": value_to_chat_string(content) }));
    }
    Some(json!({ "type": "content", "content": value_to_chat_string(content) }))
}

fn handle_service_tier(value: &Value) -> Option<Value> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| Value::String(s.to_owned()))
}

fn handle_modalities(value: &Value) -> Option<Value> {
    let arr = value.as_array()?;
    let cleaned = arr
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|v| matches!(*v, "text" | "audio" | "image"))
        .map(|v| Value::String(v.to_owned()))
        .collect::<Vec<_>>();
    if cleaned.is_empty() {
        None
    } else {
        Some(Value::Array(cleaned))
    }
}

fn handle_audio_param(value: &Value) -> Option<Value> {
    value.as_object().map(|_| value.clone())
}

fn value_to_chat_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
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
    use codex_app_transfer_registry::Provider;
    use indexmap::IndexMap;

    fn convert(body: Value) -> Value {
        responses_body_to_chat_body(&body).unwrap()
    }

    fn provider(id: &str, name: &str, base_url: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: base_url.into(),
            auth_scheme: "bearer".into(),
            api_format: "responses".into(),
            api_key: "sk-test".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    fn deepseek_thinking_provider() -> Provider {
        let mut p = provider("deepseek", "DeepSeek V4 Pro", "https://api.deepseek.com/v1");
        p.request_options.insert(
            "chat".into(),
            json!({
                "thinking": {"type": "enabled"},
                "reasoning_effort": "max",
            }),
        );
        p
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
    fn input_image_file_audio_video_items_are_lowered_to_chat_messages() {
        let out = convert(json!({
            "input": [
                {"type": "input_image", "image_url": "https://x.test/i.png", "detail": "low"},
                {"type": "input_file", "file_id": "file_1", "filename": "notes.pdf"},
                {"type": "input_audio", "data": "YWJj", "format": "mp3"},
                {"type": "input_video", "url": "https://x.test/v.mp4"}
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "连续 user message 应按旧版逻辑合并");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "https://x.test/i.png");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "[File: notes.pdf (id=file_1)]");
        assert_eq!(content[2]["type"], "input_audio");
        assert_eq!(content[2]["input_audio"]["format"], "mp3");
        assert_eq!(content[2]["input_audio"]["mime_type"], "audio/mp3");
        assert_eq!(content[3]["type"], "image_url");
        assert_eq!(content[3]["image_url"]["url"], "https://x.test/v.mp4");
    }

    #[test]
    fn input_file_data_becomes_data_uri_image_url() {
        let out = convert(json!({
            "input": [{
                "type": "input_file",
                "file_data": "ZmFrZQ==",
                "mime_type": "image/png",
                "filename": "image.png"
            }]
        }));
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,ZmFrZQ=="
        );
    }

    #[test]
    fn unknown_input_item_with_content_is_normalized() {
        let out = convert(json!({
            "input": [{
                "type": "unknown_event",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "inspect"},
                    {"type": "input_file", "filename": "a.txt"}
                ]
            }]
        }));
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "inspect");
        assert_eq!(content[1]["text"], "[input_file: a.txt]");
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
    fn empty_tool_call_id_is_repaired_from_previous_assistant_call() {
        let out = convert(json!({
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "shell",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "output": "ok"
                }
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_abc");
    }

    #[test]
    fn orphan_tool_message_without_call_id_is_dropped() {
        let out = convert(json!({
            "input": [
                {
                    "type": "function_call_output",
                    "output": "orphan"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": "continue"
                }
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn reasoning_summary_is_attached_to_following_tool_call() {
        let out = convert(json!({
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": "I should inspect the repo."
                    }],
                    "content": null,
                    "encrypted_content": null
                },
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"pwd\"}"
                }
            ]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["reasoning_content"], "I should inspect the repo.");
    }

    #[test]
    fn opaque_reasoning_item_uses_blank_placeholder_for_tool_call() {
        let out = convert(json!({
            "input": [
                {
                    "type": "reasoning",
                    "summary": [],
                    "content": null,
                    "encrypted_content": "opaque"
                },
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "shell",
                    "arguments": "{}"
                }
            ]
        }));
        assert_eq!(out["messages"][0]["reasoning_content"], " ");
    }

    #[test]
    fn request_reasoning_repairs_tool_call_assistant_reasoning() {
        let out = convert(json!({
            "reasoning": {"effort": "high"},
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "shell",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": "ok"
                }
            ]
        }));
        assert_eq!(out["messages"][0]["reasoning_content"], " ");
    }

    #[test]
    fn deepseek_provider_thinking_repairs_without_request_reasoning() {
        let provider = deepseek_thinking_provider();
        let out = responses_body_to_chat_body_for_provider(
            &json!({
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_abc",
                        "name": "shell",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_abc",
                        "output": "ok"
                    }
                ]
            }),
            Some(&provider),
        )
        .unwrap();
        assert_eq!(out["messages"][0]["reasoning_content"], " ");
    }

    #[test]
    fn non_deepseek_provider_thinking_does_not_repair_by_config_alone() {
        let mut provider = provider("other", "Other", "https://example.test/v1");
        provider
            .request_options
            .insert("chat".into(), json!({"thinking": {"type": "enabled"}}));
        let out = responses_body_to_chat_body_for_provider(
            &json!({
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_abc",
                        "name": "shell",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_abc",
                        "output": "ok"
                    }
                ]
            }),
            Some(&provider),
        )
        .unwrap();
        assert!(out["messages"][0].get("reasoning_content").is_none());
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
            "logit_bias": {"1": -1},
            "safety_identifier": "safe-1",
            "extra_body": {"provider_flag": true},
            "timeout": 30,
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
        assert_eq!(out["logit_bias"]["1"], -1);
        assert_eq!(out["safety_identifier"], "safe-1");
        assert_eq!(out["extra_body"]["provider_flag"], true);
        assert_eq!(out["timeout"], 30);
    }

    #[test]
    fn text_format_reasoning_and_special_fields_follow_legacy_conversion() {
        let out = convert(json!({
            "input": "hi",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "schema": {"type": "object"},
                    "strict": true
                }
            },
            "reasoning": {"effort": "xhigh"},
            "store": true,
            "metadata": {
                "short": "value",
                "number": 123
            },
            "prediction": {"type": "diff", "content": {"patch": "same"}},
            "service_tier": "priority",
            "modalities": ["text", "audio", "bad"],
            "audio": {"voice": "alloy", "format": "mp3"},
            "tool_choice": {"type": "any"}
        }));
        assert_eq!(out["response_format"]["type"], "json_schema");
        assert_eq!(out["response_format"]["json_schema"]["name"], "answer");
        assert_eq!(out["response_format"]["json_schema"]["strict"], true);
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["store"], true);
        assert_eq!(out["metadata"]["short"], "value");
        assert_eq!(out["metadata"]["number"], "123");
        assert_eq!(out["prediction"]["type"], "content");
        assert_eq!(out["prediction"]["content"], "{\"patch\":\"same\"}");
        assert_eq!(out["service_tier"], "priority");
        assert_eq!(out["modalities"].as_array().unwrap().len(), 2);
        assert_eq!(out["audio"]["voice"], "alloy");
        assert_eq!(out["tool_choice"], "required");
    }

    #[test]
    fn invalid_special_fields_are_dropped_or_sanitized() {
        let out = convert(json!({
            "input": "hi",
            "store": "yes",
            "metadata": "bad",
            "prediction": {"type": "bad"},
            "service_tier": "",
            "modalities": ["bad"],
            "audio": "loud",
            "reasoning": {"effort": "none"},
            "text": {"format": {"type": "text"}}
        }));
        assert!(out.get("store").is_none());
        assert!(out.get("metadata").is_none());
        assert!(out.get("prediction").is_none());
        assert!(out.get("service_tier").is_none());
        assert!(out.get("modalities").is_none());
        assert!(out.get("audio").is_none());
        assert!(out.get("reasoning_effort").is_none());
        assert!(out.get("response_format").is_none());
    }

    #[test]
    fn developer_role_downgrades_to_system_except_openai_official_provider() {
        let non_openai = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
        let out = responses_body_to_chat_body_for_provider(
            &json!({
                "input": [{
                    "type": "message",
                    "role": "developer",
                    "content": "rules"
                }]
            }),
            Some(&non_openai),
        )
        .unwrap();
        assert_eq!(out["messages"][0]["role"], "system");

        let openai = provider("openai", "OpenAI", "https://api.openai.com/v1");
        let out = responses_body_to_chat_body_for_provider(
            &json!({
                "input": [{
                    "type": "message",
                    "role": "developer",
                    "content": "rules"
                }]
            }),
            Some(&openai),
        )
        .unwrap();
        assert_eq!(out["messages"][0]["role"], "developer");
    }

    #[test]
    fn previous_response_id_without_session_cache_keeps_current_input_only() {
        let out = convert(json!({
            "previous_response_id": "resp_abc",
            "input": "hi"
        }));
        // 没有传入 session cache 的公开 helper 保持无状态兼容。
        assert!(out.get("previous_response_id").is_none());
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn previous_response_id_restores_history_before_current_input() {
        let cache = ResponseSessionCache::new(1000, std::time::Duration::from_secs(3600));
        cache.save(
            "resp_prev",
            vec![
                json!({"role": "system", "content": "old instructions"}),
                json!({"role": "user", "content": "what is the weather?"}),
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"loc\":\"NYC\"}"}
                    }]
                }),
            ],
        );

        let conversion = responses_body_to_chat_body_for_provider_with_session(
            &json!({
                "instructions": "new duplicate instructions",
                "previous_response_id": "resp_prev",
                "input": [
                    {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
                    {"type": "message", "role": "user", "content": "summarize"}
                ]
            }),
            None,
            Some(&cache),
        )
        .unwrap();

        let msgs = conversion.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "old instructions");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["content"], "summarize");
        assert_eq!(conversion.response_session.messages, msgs.clone());
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
