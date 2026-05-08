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
use serde_json::{json, Map, Value};

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
    // **cache miss + input 空** → build_messages_from_input 返回
    // PreviousResponseNotFound,proxy 层 IntoResponse 会转成标准 OpenAI 400
    // (`code: "previous_response_not_found"`)让 Codex CLI fail-fast。
    let mut messages = build_messages_from_input(input, session_cache)?;
    messages = merge_consecutive_user_messages(messages);
    messages = merge_consecutive_assistant_messages(messages);
    repair_tool_call_ids(
        &mut messages,
        super::tool_call_cache::global_tool_call_cache(),
    );
    ensure_thinking_tool_call_reasoning(&mut messages, input, provider);
    convert_developer_to_system_if_needed(&mut messages, provider);

    // 视觉剥离:对已知不支持视觉的上游(deepseek-v4-* / moonshot-v1-* 非
    // vision-preview / mimo-v2-pro / mimo-v2.5-pro 等纯文本模型),把
    // messages.content 里所有 `image_url` block 替换为占位文本块。
    // **必须**做这一步:DeepSeek API 在 deserialize 阶段就对 `image_url`
    // content variant 报 400(实测 messages[8]: unknown variant `image_url`,
    // expected `text`),Codex CLI 历史里只要存在过一次图片(即使发给 vision
    // provider 后切换到 DeepSeek)就会让续轮全部失败。
    //
    // 用 body 里的实际 model(prepare_request 路径上,model 已被 forward.rs
    // 重写成 upstream 真实 model id),而不是 provider.models["default"] —
    // 因为 Codex CLI 经过 alias 映射,实际请求的 model 未必是 default。
    let body_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(codex_app_transfer_registry::strip_internal_model_suffix);
    if !provider_supports_vision(provider, body_model.as_deref()) {
        strip_image_blocks_in_place(&mut messages);
    }

    // 历史定位(2026-05-06 → 2026-05-08):
    // - 早期:cache miss + input 空 → 代理层主动 BadRequest 拒绝
    // - 中期:改为放行 messages:[] 给上游,期望 Codex 重试 4xx
    // - 实测:Codex CLI `codex-rs/codex-client/src/retry.rs` 对 400 fail-fast,
    //   只对 5xx 与 transport timeout 重试 → 放行后上游 19s+ 才 400,且 Codex
    //   无法重置 session,延迟分钟级
    // 现行(2026-05-08+):cache miss + input 空 → 上层 build_messages_from_input
    // 返回 `PreviousResponseNotFound`,proxy IntoResponse 转标准 OpenAI 400
    // (`code: "previous_response_not_found"`),与 OpenAI 服务端真实行为对齐。
    // 此处不再有"messages 为空"分支:进到这里 messages 必非空。
    let session_messages = messages.clone();
    result.insert("messages".into(), Value::Array(messages));

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
    // 注:对已知不支持 `json_schema` 的上游(DeepSeek 实测 2026-05-06)会自动
    // 降级为 `{"type":"json_object"}`,Codex CLI 的 system prompt 通常已写明
    // required keys,模型仍会输出符合 schema 的 JSON。Kimi / MiMo 实测都支持
    // json_schema,不在降级名单。
    if let Some(text_config) = body.get("text") {
        if let Some(response_format) = build_response_format_for_provider(text_config, provider) {
            result.insert("response_format".into(), response_format);
        }
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

    sanitize_chat_body_for_provider(&mut result, provider);

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
) -> Result<Vec<Value>, AdapterError> {
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
            // 命中 → 拼历史 + 当前输入;Miss + 当前输入也空 → 报
            // PreviousResponseNotFound 让上层返回标准 OpenAI 400。
            // (单纯 Miss + 当前输入有内容仍走"忽略 previous_response_id 只
            // 发当前输入"的旧降级逻辑,避免改变 cache miss 但 input 非空的
            // 既有行为。)
            if let Some(history) = cache.get(previous_response_id) {
                let history_has_system = history.iter().any(|msg| {
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
                messages.extend(history);
                messages.extend(current_messages);
                return Ok(messages);
            }
            // cache miss
            if current_messages.is_empty() {
                return Err(AdapterError::PreviousResponseNotFound {
                    previous_response_id: previous_response_id.to_owned(),
                });
            }
            // miss 但 input 非空 → 降级,丢 previous_response_id 只用 input
            // (保留既有行为,避免无 cache 时的纯新对话误报)
        }
    }

    messages.extend(current_messages);
    Ok(messages)
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
                    parts.push(strip_codex_reasoning_prefix(text).to_owned());
                }
            }
        }
    }

    if parts.is_empty() {
        if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(strip_codex_reasoning_prefix(text).to_owned());
                    }
                }
            }
        }
    }

    parts.join("\n")
}

/// 续轮(`previous_response_id`)时,Codex CLI 会把 v2.0.8+ 注入的 reasoning
/// `**Thinking**\n\n` prefix 通过 reasoning summary 文本回送回来。这里在
/// 写回上游 messages 的 `reasoning_content` 之前 strip 掉,避免 prefix 累积
/// 污染上游 history、长会话里出现"前面所有轮 reasoning_content 都带人造
/// header"。新一轮 reasoning 在 `converter.rs::open_reasoning` 处会再次注入,
/// 行为对 Codex CLI UI 显示无变化。
pub(crate) const CODEX_REASONING_PREFIX: &str = "**Thinking**\n\n";

fn strip_codex_reasoning_prefix(text: &str) -> &str {
    text.strip_prefix(CODEX_REASONING_PREFIX).unwrap_or(text)
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
        "compaction" | "context_compaction" | "compaction_summary" => {
            // Codex CLI 触发 auto-compact 后把 summary 作为 ResponseItem::Compaction
            // 塞进 history(`codex-rs/protocol/src/models.rs:882`),续轮 input 里
            // 会带这个 item。`encrypted_content` 字段名是历史包袱,**实际是
            // 明文** —— Codex 自家 SUMMARY_PREFIX(`codex-rs/core/src/compact.rs:262`)
            // 已写明"based on this summary..."的语义。
            //
            // 必须把它转成 user message 注入下游 chat completions(role 与 Codex
            // 自家 inline compact 一致:`build_compacted_history_with_limit`
            // 也是 push role="user"),否则上游 LLM 完全看不到 summary,等同
            // 于 compact 后失忆 —— 体感"compact 触发了但下一轮 LLM 不记得任何
            // 之前的事"。
            let summary = item
                .get("encrypted_content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            if summary.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "role": "user", "content": summary })]
            }
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

/// 修复 / 重建工具调用 id 关联。
///
/// 改造前 Python `responses_adapter.py:466-597 _repair_tool_call_ids` 的行为
/// 等价 Rust 实现 + 把孤儿 tool 的"直接丢弃"换成"用 ToolCallCache 兜底重建,
/// 否则插占位 assistant"。
///
/// 三类输入:
///   1. tool_call_id 为空 → 从前一条 assistant.tool_calls 顺序补 id
///   2. tool_call_id 非空且能在前 assistant.tool_calls 找到 → 直接 ack 通过
///   3. tool_call_id 非空但前 assistant 不含该 id(history 被压缩 / 截断 /
///      跨 session 续接)→ 查 ToolCallCache:
///        - 命中:把 tool_call 注回最近一条 assistant 的 tool_calls 列表
///        - 未命中:在前面塞一条占位 assistant `{role:assistant, content:"",
///          tool_calls:[{id, type:function, function:{name:"", arguments:""}}]}`,
///          让 Chat 上游(Kimi / DeepSeek 严格校验)能匹配上不报 400
///   4. 完全没有前置 assistant + cache 也没有 → 插占位 assistant + 保留 tool
///
/// 与 litellm 1.84.0 `transformation.py:802-948
/// _ensure_tool_results_have_corresponding_tool_calls` 行为一致(只是 litellm
/// 还做 Anthropic 合并,本仓库不需要)。
fn repair_tool_call_ids(
    messages: &mut Vec<Value>,
    tool_call_cache: &super::tool_call_cache::ToolCallCache,
) {
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut repaired: Vec<Value> = Vec::with_capacity(messages.len());
    // 跟踪最近一条 assistant 在 repaired 里的下标,用于 path B "注回前 assistant"
    let mut last_assistant_idx: Option<usize> = None;

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
            last_assistant_idx = Some(repaired.len());
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
                // path A1:有 pending → 顺序补
                if let Some(next) = pending_call_ids.first().cloned() {
                    pending_call_ids.remove(0);
                    if let Some(obj) = msg.as_object_mut() {
                        obj.insert("tool_call_id".into(), Value::String(next));
                    }
                } else {
                    // path A2:tool_call_id 空且 pending 也空 →
                    // 没有任何 id 可以关联,作为孤儿 message 丢弃(沿用旧行为)
                    continue;
                }
            } else if let Some(pos) = pending_call_ids.iter().position(|id| id == &existing) {
                // path B1:tool_call_id 非空 + 前 assistant 含该 id → ack 通过
                pending_call_ids.remove(pos);
            } else {
                // path B2:tool_call_id 非空但前 assistant 不含该 id →
                // 查 ToolCallCache 兜底重建
                let entry = tool_call_cache.get(&existing);
                let (name, arguments) = match entry {
                    Some(e) => (e.name, e.arguments),
                    // path B3:cache 也未命中 → 占位 (name 空字符串),
                    // 上游能 match id 不报 400 是关键,name / args 由上游能容
                    None => (String::new(), String::new()),
                };
                let placeholder_tool_call = json!({
                    "id": existing,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    },
                });
                match last_assistant_idx {
                    // path B-into-existing:把重建 tool_call 注回最近 assistant
                    Some(idx) => {
                        let assistant = &mut repaired[idx];
                        let obj = assistant.as_object_mut().expect("assistant must be object");
                        let calls = obj
                            .entry("tool_calls".to_owned())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if let Value::Array(arr) = calls {
                            arr.push(placeholder_tool_call);
                        }
                    }
                    // path B-orphan:连前 assistant 都没有 → 在 tool 前插占位
                    None => {
                        let placeholder_assistant = json!({
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [placeholder_tool_call],
                        });
                        last_assistant_idx = Some(repaired.len());
                        repaired.push(placeholder_assistant);
                    }
                }
            }
        }

        if matches!(role.as_str(), "user" | "system" | "developer") {
            pending_call_ids.clear();
            last_assistant_idx = None;
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

fn sanitize_chat_body_for_provider(body: &mut Map<String, Value>, provider: Option<&Provider>) {
    let Some(provider) = provider else {
        return;
    };
    if provider_looks_like(provider, "minimax") || provider_looks_like(provider, "minimaxi") {
        sanitize_minimax_chat_body(body);
    }
}

/// MiniMax M2.x 的 OpenAI-compatible chat 端点并不接受完整 OpenAI/Codex
/// 参数集。官方文档主要列出 model/messages/stream/max_tokens/
/// max_completion_tokens/temperature/top_p/tools/tool_choice/
/// mask_sensitive_info；`response_format` 仅 MiniMax-Text-01 支持。
/// Codex Responses 转 Chat 时会生成 `reasoning_effort`、`response_format`、
/// `parallel_tool_calls` 等字段，MiniMax 会统一报 400:
/// "invalid params, invalid chat setting (2013)"。
fn sanitize_minimax_chat_body(body: &mut Map<String, Value>) {
    let response_format_allowed = body
        .get("model")
        .and_then(|v| v.as_str())
        .is_some_and(|model| model.eq_ignore_ascii_case("MiniMax-Text-01"));

    body.retain(|key, _| {
        matches!(
            key.as_str(),
            "model"
                | "messages"
                | "stream"
                | "max_tokens"
                | "max_completion_tokens"
                | "temperature"
                | "top_p"
                | "tool_choice"
                | "tools"
                | "reasoning_split"
                | "stream_options"
                | "mask_sensitive_info"
        ) || (key == "response_format" && response_format_allowed)
    });

    // MiniMax 官方建议 OpenAI-compatible M2.7 工具调用启用
    // reasoning_split,让 thinking 单独进入 reasoning_details,避免塞进
    // content 的 <think>...</think> 里。
    body.insert("reasoning_split".into(), Value::Bool(true));
    // MiniMax 的 OpenAI-compatible streaming 不稳定接受
    // `stream_options.include_usage`;缺 usage 时响应转换层会补零值 usage。
    body.remove("stream_options");
    merge_consecutive_system_messages(body);
    sanitize_minimax_tools(body);

    if let Some(choice) = body.get_mut("tool_choice") {
        let allowed = choice
            .as_str()
            .is_some_and(|s| matches!(s, "auto" | "none"));
        if !allowed {
            *choice = Value::String("auto".into());
        }
    }

    remove_non_positive_number(body, "temperature");
    remove_non_positive_number(body, "top_p");
}

fn merge_consecutive_system_messages(body: &mut Map<String, Value>) {
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };

    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages.drain(..) {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let is_system = role == "system";
        let prev_is_system = merged
            .last()
            .and_then(|prev| prev.get("role"))
            .and_then(|v| v.as_str())
            == Some("system");

        if is_system && prev_is_system {
            let current = msg
                .get("content")
                .map(value_to_chat_string)
                .unwrap_or_default();
            if let Some(prev_obj) = merged.last_mut().and_then(|prev| prev.as_object_mut()) {
                let prev = prev_obj
                    .get("content")
                    .map(value_to_chat_string)
                    .unwrap_or_default();
                let combined = if prev.is_empty() {
                    current
                } else if current.is_empty() {
                    prev
                } else {
                    format!("{prev}\n\n{current}")
                };
                prev_obj.insert("content".into(), Value::String(combined));
            }
            continue;
        }

        merged.push(msg);
    }

    *messages = merged;
}

fn sanitize_minimax_tools(body: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for tool in tools.iter_mut() {
        let Some(function) = tool.get_mut("function").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        // MiniMax tool examples use the classic OpenAI tool schema and do not
        // accept OpenAI strict function-calling metadata.
        function.remove("strict");
    }
}

fn remove_non_positive_number(body: &mut Map<String, Value>, key: &str) {
    let should_remove = body
        .get(key)
        .and_then(|v| v.as_f64())
        .is_some_and(|v| v <= 0.0);
    if should_remove {
        body.remove(key);
    }
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

/// 当前请求(provider × model 组合)是否支持 vision(messages.content
/// 里允许 `image_url` block)。
///
/// 判断顺序:
/// 1. **请求 body 的 model** 在 `provider.modelCapabilities[<model>].supports_vision`
///    显式 false/true → 直接返回(粒度细到模型,允许同 provider 不同模型差异)
/// 2. fallback 到 `provider.modelCapabilities[<default_model>].supports_vision`
///    显式声明(向后兼容旧配置)
/// 3. 模型 id 命中**模型名黑名单**(2026-05-07 实测验证的纯文本模型) → 不支持
/// 4. 其余默认支持(走 OpenAI 标准多模态)
///
/// **2026-05-07 实测覆盖**(所有 5 接入 provider 的所有公开 model):
///
/// | Model | Vision | 来源 |
/// |---|---|---|
/// | `deepseek-v4-pro` / `deepseek-v4-flash` | ❌ | 实测 400 unknown variant `image_url` |
/// | `moonshot-v1-{8k,32k,128k}` / `moonshot-v1-auto` | ❌ | 实测 400 "Image input not supported" |
/// | `kimi-k2.5` / `kimi-k2.6` | ✅ | 实测 SAW_RED + 官方 vision guide |
/// | `moonshot-v1-{8k,32k,128k}-vision-preview` | ✅ | 实测 SAW_RED |
/// | `kimi-for-coding` | ✅ | 实测 SAW_RED(虽然 base_url 像 coding-only) |
/// | `mimo-v2-omni` / `mimo-v2-flash` / `mimo-v2.5` | ✅ | 实测 SAW_RED + 官方 omni 标识 |
/// | `mimo-v2-pro` / `mimo-v2.5-pro` | ❌ | 实测响应 "I don't see any image attached" |
/// | `mimo-v2*-tts*` | n/a | 不接受 chat 接口 |
///
/// **粒度从 provider 子串改成模型名精确匹配**:旧版 `["deepseek", "xiaomi",
/// "mimo", "qwen3.6"]` 子串黑名单会把整个 MiMo provider 的 omni / flash /
/// 2.5 这三个**支持视觉的**模型一刀切掉(误杀);也会让 Moonshot 的
/// `moonshot-v1-8k` 这种纯文本模型逃过(漏杀,因为 "moonshot" 不在子串名单)。
///
/// 这条防御对应 DeepSeek `deepseek-v4-pro` 在 deserialize 阶段直接对
/// `messages[i].content[*].type == "image_url"` 报 400 unknown variant,
/// 让 Codex CLI 续轮 history 一旦含过图就全链路阻塞(2026-05-06 实测)。
fn provider_supports_vision(provider: Option<&Provider>, model: Option<&str>) -> bool {
    let Some(p) = provider else {
        return true;
    };

    // **关键**:request body 缺 model 字段时仍要保护文本-only 上游不收图。
    // codex-connector P1 review (PR #43) 指出:几条 conversion path 允许 body
    // 不带 model 进来,如果只在 `model.is_some()` 时跑黑名单,DeepSeek 这类
    // text-only provider 一旦 model 缺失就会让 image_url 透传出去,触发原本
    // 要修的 400 unknown variant 失败。
    //
    // 解决:把 body model + provider.models["default"] 合并成 effective_model,
    // 所有检查(modelCapabilities 显式 / TEXT_ONLY_MODELS 黑名单)都在它上面跑。
    let default_model = p
        .models
        .get("default")
        .map(|s| codex_app_transfer_registry::strip_internal_model_suffix(s))
        .filter(|s| !s.is_empty());
    let effective_model: Option<String> = model
        .map(|s| s.to_owned())
        .or_else(|| default_model.clone());

    // 1:effective_model 命中 provider.modelCapabilities 显式声明 → 直接采用
    if let Some(m) = effective_model.as_deref() {
        if let Some(b) = p
            .model_capabilities
            .get(m)
            .and_then(|v| v.get("supports_vision"))
            .and_then(|v| v.as_bool())
        {
            return b;
        }
    }
    // 2:body model 不在 capabilities 但 default_model 在(向后兼容旧配置:
    //    用户可能只在 modelCapabilities 标过 default model 的能力)
    if let (Some(body), Some(def)) = (model, default_model.as_deref()) {
        if body != def {
            if let Some(b) = p
                .model_capabilities
                .get(def)
                .and_then(|v| v.get("supports_vision"))
                .and_then(|v| v.as_bool())
            {
                return b;
            }
        }
    }

    // 3:effective_model 命中**硬编码模型名黑名单**(2026-05-07 实测 — 详见函数 doc)
    if let Some(m) = effective_model.as_deref() {
        let lc = m.to_ascii_lowercase();
        const TEXT_ONLY_MODELS: &[&str] = &[
            // DeepSeek v4 系列(实测 400 unknown variant `image_url`)
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            // Moonshot 标准 v1 系列(无 -vision-preview 后缀,实测 400
            // "Image input not supported for model ...")
            "moonshot-v1-8k",
            "moonshot-v1-32k",
            "moonshot-v1-128k",
            "moonshot-v1-auto",
            // Xiaomi MiMo 文本-only 子集(实测响应 "I don't see any image attached")
            "mimo-v2-pro",
            "mimo-v2.5-pro",
        ];
        if TEXT_ONLY_MODELS.iter().any(|n| lc == *n) {
            return false;
        }
    }

    // 4:默认支持(覆盖未列在白名单的新模型 / OpenAI 标准 vision provider)
    true
}

/// 把 messages 中所有 `image_url` content block 替换为占位文本块,
/// 防止纯文本上游(deepseek-v4-pro / mimo-v2.5-pro 等)拒绝整 body。
/// 替换后若 content 数组只剩 text 块,会进一步合并为单 string,与
/// 普通文本消息序列化形态一致。
fn strip_image_blocks_in_place(messages: &mut [Value]) {
    const PLACEHOLDER: &str = "[图片省略:当前 provider 不支持视觉输入]";
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let Some(content) = obj.get_mut("content") else {
            continue;
        };
        let Value::Array(arr) = content else {
            continue;
        };
        let mut had_image = false;
        for block in arr.iter_mut() {
            let Some(block_obj) = block.as_object_mut() else {
                continue;
            };
            let block_type = block_obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if block_type == "image_url" {
                had_image = true;
                block_obj.clear();
                block_obj.insert("type".into(), Value::String("text".into()));
                block_obj.insert("text".into(), Value::String(PLACEHOLDER.into()));
            }
        }
        if had_image {
            // 替换后若全是 text,合并成单 string,跟其它纯文本消息一致
            let all_text = arr
                .iter()
                .all(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"));
            if all_text {
                let combined: Vec<String> = arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(str::to_owned))
                    .collect();
                obj.insert("content".into(), Value::String(combined.join("\n")));
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

/// 把 Responses API 的 `text.format` 翻译成 Chat Completions 的 `response_format`。对已知不支持 `json_schema` 的上游(实测 DeepSeek
/// API 在 deserialize 阶段对 `response_format.type=json_schema` 报 400
/// "This response_format type is unavailable now"),把
/// `{type:"json_schema", ...}` 降级为 `{type:"json_object"}`,让模型
/// 仍输出 JSON,schema 字段顺序由 Codex CLI 的 system prompt 中"required
/// keys"指示驱动(2026-05-06 实测各家在 system prompt 给约束时,模型
/// 输出的 JSON 都能匹配三个 key)。
///
/// 实测结果(2026-05-06,真实 API):
/// - DeepSeek v4-pro:json_schema → 400;json_object → 200 + 合法 JSON
/// - Kimi k2.6:json_schema → 200 + 合法 JSON(不降级)
/// - MiMo v2.5-pro(PAYG / Token Plan):json_schema → 200 + 合法 JSON(**不降级**)
fn build_response_format_for_provider(
    text_config: &Value,
    provider: Option<&Provider>,
) -> Option<Value> {
    let fmt = text_config.get("format")?.as_object()?;
    let fmt_type = fmt.get("type").and_then(|v| v.as_str()).unwrap_or("");

    let json_schema_value = || {
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": fmt.get("name").and_then(|v| v.as_str()).unwrap_or("response_schema"),
                "schema": fmt.get("schema").cloned().unwrap_or_else(|| json!({})),
                "strict": fmt.get("strict").and_then(|v| v.as_bool()).unwrap_or(false),
            },
        })
    };

    let raw = match fmt_type {
        "json_schema" => json_schema_value(),
        "json_object" => json!({ "type": "json_object" }),
        "text" => return None,
        _ if fmt.contains_key("schema") => json_schema_value(),
        _ => return None,
    };

    // json_schema 降级:命中 provider 黑名单时,转 json_object
    if raw.get("type").and_then(|v| v.as_str()) == Some("json_schema")
        && !provider_supports_json_schema_response_format(provider)
    {
        return Some(json!({ "type": "json_object" }));
    }
    Some(raw)
}

/// 上游 provider 是否支持 `response_format = {type:"json_schema", json_schema:{...}}`。
///
/// 判断顺序:
/// 1. `provider.modelCapabilities[<default_model>].supports_json_schema_response_format`
///    显式 false → 不支持;true → 支持
/// 2. fallback 黑名单(只放经实测确认拒绝 `json_schema` 的上游):
///    - `deepseek` → 不支持(API 直接 400 unavailable)
/// 3. 其他默认支持(Kimi / MiMo 实测都支持完整 OpenAI `json_schema` 语义)
///
/// **不要把 mimo / qwen3.6 加入名单**:实测 MiMo 两家都支持
/// json_schema(2026-05-06)。误加会导致正常 schema 被无谓降级。
fn provider_supports_json_schema_response_format(provider: Option<&Provider>) -> bool {
    let Some(p) = provider else {
        return true;
    };

    // 1. 显式 modelCapabilities 优先
    let default_model = p
        .models
        .get("default")
        .map(|s| codex_app_transfer_registry::strip_internal_model_suffix(s))
        .unwrap_or_default();
    let candidates: [&str; 2] = [default_model.as_str(), default_model.trim()];
    for key in candidates {
        if key.is_empty() {
            continue;
        }
        if let Some(b) = p
            .model_capabilities
            .get(key)
            .and_then(|v| v.get("supports_json_schema_response_format"))
            .and_then(|v| v.as_bool())
        {
            return b;
        }
    }

    // 2. 实测黑名单(命中即视为不支持)。
    //    **慎重添加新条目**:必须先用真实凭据 curl 验证 json_schema 真的报错
    //    (DeepSeek 形态:400 + "This response_format type is unavailable now")。
    const KNOWN_NOT_SUPPORTED: &[&str] = &["deepseek"];
    !KNOWN_NOT_SUPPORTED
        .iter()
        .any(|needle| provider_looks_like(p, needle))
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

    fn deepseek_provider() -> Provider {
        let mut p = provider("deepseek", "DeepSeek", "https://api.deepseek.com");
        p.models.insert("default".into(), "deepseek-v4-pro".into());
        p.api_format = "openai_chat".into();
        p
    }

    fn minimax_provider() -> Provider {
        let mut p = provider("minimax", "MiniMax", "https://api.minimaxi.com/v1");
        p.models.insert("default".into(), "MiniMax-M2.7".into());
        p.api_format = "openai_chat".into();
        p
    }

    #[test]
    fn deepseek_history_strips_image_blocks_to_text_placeholder() {
        // 真实 Codex CLI history:第 9 条 user 消息含 image_url,DeepSeek 实测
        // 在 deserialize 阶段对 image_url variant 报 400(2026-05-06 实测)。
        // 转换后 image_url 必须不再存在 messages.content 任何块里。
        let req = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "input": [
                {"type":"message","role":"user","content":"hi"},
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"看这张图"},
                    {"type":"input_image","image_url":"data:image/png;base64,AAA"}
                ]}
            ]
        });
        let p = deepseek_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        let messages = out["messages"].as_array().unwrap();
        let serialized = serde_json::to_string(messages).unwrap();
        assert!(
            !serialized.contains("\"image_url\""),
            "DeepSeek 上游不接 image_url,转换后必须不含此 variant\nactual: {serialized}"
        );
        assert!(
            serialized.contains("图片省略"),
            "应当用占位文本替换,而不是直接丢弃,让模型知道历史里曾有图\nactual: {serialized}"
        );
    }

    #[test]
    fn deepseek_input_image_top_level_item_strips_to_text_placeholder() {
        // input_image 作为顶层 item(Codex CLI 当前轮直接贴图)也要被剥
        let req = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "input": [
                {"type":"input_image","image_url":"data:image/png;base64,AAA","detail":"low"}
            ]
        });
        let p = deepseek_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        let serialized = serde_json::to_string(&out["messages"]).unwrap();
        assert!(!serialized.contains("\"image_url\""));
        assert!(serialized.contains("图片省略"));
    }

    // ── response_format json_schema 降级(基于实测 2026-05-06)─────────
    // - DeepSeek v4-pro:json_schema → 400;json_object → 200(必须降级)
    // - Kimi k2.6:json_schema → 200(不降级)
    // - MiMo v2.5-pro:json_schema → 200(不降级,实测两家都支持)

    fn json_schema_text_config() -> Value {
        json!({
            "format": {
                "type": "json_schema",
                "name": "risk_review",
                "strict": true,
                "schema": {
                    "type":"object",
                    "properties": {
                        "risk_level":{"type":"string","enum":["low","medium","high"]},
                        "outcome":{"type":"string","enum":["allow","deny"]}
                    },
                    "required": ["risk_level","outcome"],
                    "additionalProperties": false,
                }
            }
        })
    }

    #[test]
    fn deepseek_downgrades_json_schema_response_format_to_json_object() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "instructions": "Output strict JSON. Required keys: risk_level, outcome.",
            "input": [{"type":"message","role":"user","content":"Risk of ls?"}],
            "text": json_schema_text_config(),
        });
        let p = deepseek_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        let rf = &out["response_format"];
        assert_eq!(
            rf["type"], "json_object",
            "DeepSeek 必须把 json_schema 降级为 json_object;实际: {rf}"
        );
        assert!(
            rf.get("json_schema").is_none(),
            "降级后不能保留 json_schema 字段:{rf}"
        );
    }

    #[test]
    fn kimi_keeps_json_schema_response_format_intact() {
        let mut kimi = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
        kimi.models.insert("default".into(), "kimi-k2.6".into());
        let req = json!({
            "model": "kimi-k2.6",
            "stream": true,
            "instructions": "x",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "text": json_schema_text_config(),
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&kimi)).unwrap();
        let rf = &out["response_format"];
        assert_eq!(rf["type"], "json_schema", "Kimi 应保留 json_schema:{rf}");
        assert_eq!(rf["json_schema"]["name"], "risk_review");
        assert_eq!(rf["json_schema"]["strict"], true);
    }

    #[test]
    fn mimo_keeps_json_schema_response_format_intact() {
        // MiMo 实测两家(PAYG / Token Plan)都支持 json_schema,不能降级
        let mut mimo = provider(
            "xiaomi-mimo",
            "Xiaomi MiMo",
            "https://api.xiaomimimo.com/v1",
        );
        mimo.models.insert("default".into(), "mimo-v2.5-pro".into());
        let req = json!({
            "model": "mimo-v2.5-pro",
            "stream": true,
            "instructions": "x",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "text": json_schema_text_config(),
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&mimo)).unwrap();
        let rf = &out["response_format"];
        assert_eq!(rf["type"], "json_schema", "MiMo 实测支持,不应降级:{rf}");
    }

    #[test]
    fn explicit_supports_json_schema_true_overrides_blacklist() {
        // 用户在 modelCapabilities 显式标 supports_json_schema_response_format: true
        // 即使 base_url 命中黑名单(deepseek)也保留(给未来能力升级预留)。
        let mut p = deepseek_provider();
        p.model_capabilities.insert(
            "deepseek-v4-pro".into(),
            json!({"supports_json_schema_response_format": true}),
        );
        let req = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "instructions": "x",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "text": json_schema_text_config(),
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        assert_eq!(out["response_format"]["type"], "json_schema");
    }

    #[test]
    fn explicit_supports_json_schema_false_forces_downgrade() {
        // 即使 base_url 不在黑名单(例如 Kimi),用户显式标 false 也要降级
        let mut kimi = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
        kimi.models.insert("default".into(), "kimi-k2.6".into());
        kimi.model_capabilities.insert(
            "kimi-k2.6".into(),
            json!({"supports_json_schema_response_format": false}),
        );
        let req = json!({
            "model": "kimi-k2.6",
            "stream": true,
            "instructions": "x",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "text": json_schema_text_config(),
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&kimi)).unwrap();
        assert_eq!(out["response_format"]["type"], "json_object");
    }

    #[test]
    fn minimax_m2_drops_unsupported_chat_settings() {
        // MiniMax M2.7 OpenAI-compatible chat 对 OpenAI/Codex 的扩展字段会报
        // 400 invalid chat setting (2013)。保留工具相关标准字段,剥掉
        // response_format/reasoning_effort/parallel_tool_calls 等不兼容项。
        let req = json!({
            "model": "MiniMax-M2.7",
            "stream": true,
            "reasoning": {"effort": "high"},
            "parallel_tool_calls": true,
            "store": false,
            "metadata": {"k": "v"},
            "instructions": "Output strict JSON.",
            "input": [{"type":"message","role":"user","content":"hi"}],
            "text": json_schema_text_config(),
            "tool_choice": "auto",
            "tools": [{
                "type":"function",
                "name":"shell",
                "parameters":{"type":"object"}
            }]
        });
        let p = minimax_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        assert!(out.get("response_format").is_none());
        assert!(out.get("reasoning_effort").is_none());
        assert!(out.get("parallel_tool_calls").is_none());
        assert!(out.get("store").is_none());
        assert!(out.get("metadata").is_none());
        assert!(out.get("tools").is_some(), "MiniMax M2 支持 tool use");
        assert_eq!(out["tool_choice"], "auto");
        assert_eq!(out["reasoning_split"], true);
        assert!(out.get("stream_options").is_none());
        assert!(out["tools"][0]["function"].get("strict").is_none());
    }

    #[test]
    fn minimax_tool_choice_required_is_downgraded_to_auto() {
        let req = json!({
            "model": "MiniMax-M2.7",
            "stream": true,
            "input": "hi",
            "tool_choice": {"type": "required"}
        });
        let p = minimax_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn minimax_merges_consecutive_system_messages() {
        let req = json!({
            "model": "MiniMax-M2.7",
            "stream": true,
            "instructions": "system one",
            "input": [
                {"type":"message","role":"system","content":"system two"},
                {"type":"message","role":"user","content":"hi"}
            ]
        });
        let p = minimax_provider();
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "system one\n\nsystem two");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn minimax_text_01_keeps_response_format() {
        let mut p = provider("minimax", "MiniMax", "https://api.minimaxi.com/v1");
        p.models.insert("default".into(), "MiniMax-Text-01".into());
        let req = json!({
            "model": "MiniMax-Text-01",
            "stream": true,
            "input": "hi",
            "text": json_schema_text_config(),
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        assert_eq!(out["response_format"]["type"], "json_schema");
    }

    #[test]
    fn kimi_history_keeps_image_blocks_intact() {
        // Kimi(月之暗面)部分模型支持视觉,默认放行 → image_url 必须保留
        let mut kimi = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
        kimi.models.insert("default".into(), "kimi-k2.6".into());
        let req = json!({
            "model": "kimi-k2.6",
            "stream": true,
            "input": [{
                "type":"message","role":"user","content":[
                    {"type":"input_text","text":"图里是什么"},
                    {"type":"input_image","image_url":"data:image/png;base64,AAA"}
                ]
            }]
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&kimi)).unwrap();
        let serialized = serde_json::to_string(&out["messages"]).unwrap();
        assert!(
            serialized.contains("\"image_url\""),
            "Kimi 应保留 image_url"
        );
    }

    #[test]
    fn explicit_supports_vision_true_overrides_text_only_blacklist() {
        // 用户在 modelCapabilities 显式标 supports_vision: true → 即使模型
        // 命中黑名单(deepseek-v4-pro)也保留 image_url。给未来视觉版预留口子。
        let mut deepseek_with_vision = deepseek_provider();
        deepseek_with_vision
            .model_capabilities
            .insert("deepseek-v4-pro".into(), json!({"supports_vision": true}));
        let req = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "input": [{
                "type":"input_image","image_url":"data:image/png;base64,AAA"
            }]
        });
        let out =
            responses_body_to_chat_body_for_provider(&req, Some(&deepseek_with_vision)).unwrap();
        let serialized = serde_json::to_string(&out["messages"]).unwrap();
        assert!(serialized.contains("\"image_url\""));
    }

    // ── vision 白名单的模型级 granularity 验证(2026-05-07 实测覆盖所有 5 接入 provider)──
    //
    // 旧版 provider-id 子串黑名单(["deepseek","xiaomi","mimo","qwen3.6"])会:
    // - 误杀:Mimo 的 mimo-v2-omni / mimo-v2-flash / mimo-v2.5(实测均支持视觉)
    // - 漏杀:Moonshot 的 moonshot-v1-{8k,32k,128k}(实测 400 "Image input not supported")
    //
    // 新版按**请求 body 的 model**精确匹配模型名黑名单。

    fn moonshot_provider() -> Provider {
        let mut p = provider("moonshot", "Moonshot", "https://api.moonshot.cn/v1");
        p.models.insert("default".into(), "kimi-k2.6".into());
        p.api_format = "openai_chat".into();
        p
    }

    fn mimo_provider() -> Provider {
        let mut p = provider(
            "xiaomi-mimo",
            "Xiaomi MiMo",
            "https://api.xiaomimimo.com/v1",
        );
        p.models.insert("default".into(), "mimo-v2.5-pro".into());
        p.api_format = "openai_chat".into();
        p
    }

    fn vision_request_for(model: &str) -> Value {
        json!({
            "model": model,
            "stream": true,
            "input": [{"type":"input_image","image_url":"data:image/png;base64,AAA"}]
        })
    }

    fn image_url_kept(req: &Value, p: &Provider) -> bool {
        let out = responses_body_to_chat_body_for_provider(req, Some(p)).unwrap();
        serde_json::to_string(&out["messages"])
            .unwrap()
            .contains("\"image_url\"")
    }

    #[test]
    fn vision_blacklist_blocks_deepseek_v4_pro() {
        let req = vision_request_for("deepseek-v4-pro");
        assert!(!image_url_kept(&req, &deepseek_provider()));
    }

    #[test]
    fn vision_blacklist_blocks_deepseek_v4_flash() {
        let req = vision_request_for("deepseek-v4-flash");
        let mut p = deepseek_provider();
        p.models
            .insert("default".into(), "deepseek-v4-flash".into());
        assert!(!image_url_kept(&req, &p));
    }

    #[test]
    fn vision_blacklist_blocks_moonshot_v1_non_preview_models() {
        // moonshot-v1-{8k,32k,128k}/auto 实测 400 "Image input not supported"
        for model in [
            "moonshot-v1-8k",
            "moonshot-v1-32k",
            "moonshot-v1-128k",
            "moonshot-v1-auto",
        ] {
            let req = vision_request_for(model);
            let mut p = moonshot_provider();
            p.models.insert("default".into(), model.into());
            assert!(
                !image_url_kept(&req, &p),
                "{model} 实测纯文本,必须 strip image_url"
            );
        }
    }

    #[test]
    fn vision_whitelist_keeps_moonshot_vision_preview_variants() {
        // moonshot-v1-{8k,32k,128k}-vision-preview 实测 SAW_RED
        for model in [
            "moonshot-v1-8k-vision-preview",
            "moonshot-v1-32k-vision-preview",
            "moonshot-v1-128k-vision-preview",
        ] {
            let req = vision_request_for(model);
            let mut p = moonshot_provider();
            p.models.insert("default".into(), model.into());
            assert!(
                image_url_kept(&req, &p),
                "{model} 实测支持视觉,必须保留 image_url"
            );
        }
    }

    #[test]
    fn vision_whitelist_keeps_kimi_k2_models() {
        // kimi-k2.5 / kimi-k2.6 实测 SAW_RED + 官方 vision guide 列出 k2.6
        for model in ["kimi-k2.5", "kimi-k2.6"] {
            let req = vision_request_for(model);
            let mut p = moonshot_provider();
            p.models.insert("default".into(), model.into());
            assert!(image_url_kept(&req, &p), "{model} 实测支持视觉");
        }
    }

    #[test]
    fn vision_whitelist_keeps_kimi_for_coding() {
        // 实测意外:kimi-for-coding 居然支持视觉(SAW_RED)
        let req = vision_request_for("kimi-for-coding");
        let mut p = provider("kimi-code", "Kimi Code", "https://api.kimi.com/coding/v1");
        p.models.insert("default".into(), "kimi-for-coding".into());
        assert!(image_url_kept(&req, &p));
    }

    #[test]
    fn vision_whitelist_keeps_mimo_omni_flash_2_5() {
        // mimo-v2-omni / mimo-v2-flash / mimo-v2.5 实测 SAW_RED
        for model in ["mimo-v2-omni", "mimo-v2-flash", "mimo-v2.5"] {
            let req = vision_request_for(model);
            let mut p = mimo_provider();
            p.models.insert("default".into(), model.into());
            assert!(
                image_url_kept(&req, &p),
                "{model} 实测支持视觉,旧版子串黑名单(\"mimo\")会误杀"
            );
        }
    }

    #[test]
    fn vision_blacklist_blocks_mimo_v2_pro_and_v2_5_pro() {
        // mimo-v2-pro / mimo-v2.5-pro 实测响应 "I don't see any image attached"
        for model in ["mimo-v2-pro", "mimo-v2.5-pro"] {
            let req = vision_request_for(model);
            let mut p = mimo_provider();
            p.models.insert("default".into(), model.into());
            assert!(!image_url_kept(&req, &p), "{model} 实测纯文本");
        }
    }

    #[test]
    fn vision_check_uses_body_model_not_provider_default() {
        // 关键:provider.default = "kimi-k2.6"(支持视觉),但 body 实际请求
        // moonshot-v1-8k(纯文本)→ 必须按 body model 判定,strip 图。
        // 旧版 provider_supports_vision(provider) 只看 default_model 会误判。
        let mut p = moonshot_provider();
        p.models.insert("default".into(), "kimi-k2.6".into());
        let req = vision_request_for("moonshot-v1-8k");
        assert!(
            !image_url_kept(&req, &p),
            "body.model=moonshot-v1-8k 必须当前请求级 strip,与 default 无关"
        );
    }

    #[test]
    fn vision_unknown_model_defaults_to_supported() {
        // 未在黑名单的模型默认放行(覆盖 OpenAI gpt-4o / 新接入 vision provider)
        let req = vision_request_for("gpt-4o");
        let mut p = provider("openai", "OpenAI", "https://api.openai.com/v1");
        p.models.insert("default".into(), "gpt-4o".into());
        assert!(image_url_kept(&req, &p));
    }

    #[test]
    fn vision_explicit_capability_overrides_blacklist_for_per_model() {
        // 用户在 modelCapabilities 显式标 supports_vision = true,即使该模型
        // 在硬编码黑名单(mimo-v2-pro)里也放行。给"我知道这是视觉版升级"留口子。
        let mut p = mimo_provider();
        p.model_capabilities
            .insert("mimo-v2-pro".into(), json!({"supports_vision": true}));
        let req = vision_request_for("mimo-v2-pro");
        assert!(image_url_kept(&req, &p));
    }

    #[test]
    fn vision_explicit_capability_false_overrides_default_pass() {
        // 反向:模型不在黑名单(默认放行),但用户标 supports_vision = false
        // → 必须 strip。给"我知道这上游临时挂了 vision"留口子。
        let mut p = provider("custom", "Custom", "https://api.custom.example/v1");
        p.models.insert("default".into(), "custom-text".into());
        p.model_capabilities
            .insert("custom-text".into(), json!({"supports_vision": false}));
        let req = vision_request_for("custom-text");
        assert!(!image_url_kept(&req, &p));
    }

    #[test]
    fn vision_falls_back_to_default_model_when_body_omits_model() {
        // codex-connector P1 review (2026-05-07 PR #43) 指出:旧改法在 body
        // 缺 model 字段时直接 return true,DeepSeek 这类 text-only provider
        // 一旦 model 缺失就让 image_url 透传 → 触发原本要修的 400 unknown
        // variant 失败。新版必须 fallback 到 provider.models["default"]。
        let p = deepseek_provider(); // default = "deepseek-v4-pro"
        let req = json!({
            // 故意不写 "model" 字段,模拟某些 conversion path 的合法形态
            "stream": true,
            "input": [
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        let serialized = serde_json::to_string(&out["messages"]).unwrap();
        assert!(
            !serialized.contains("\"image_url\""),
            "body 缺 model + default=deepseek-v4-pro → 必须按 default 命中黑名单 strip"
        );
        assert!(serialized.contains("图片省略"), "应该用占位文本替换");
    }

    #[test]
    fn vision_falls_back_to_default_model_for_explicit_capability_too() {
        // body 缺 model,但 default 在 modelCapabilities 标了 supports_vision = false
        // → 同样要 strip,而不是默认放行。
        let mut p = provider("custom", "Custom", "https://api.custom.example/v1");
        p.models.insert("default".into(), "future-text-v1".into());
        p.model_capabilities
            .insert("future-text-v1".into(), json!({"supports_vision": false}));
        let req = json!({
            "stream": true,
            "input": [
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        });
        let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
        assert!(!serde_json::to_string(&out["messages"])
            .unwrap()
            .contains("\"image_url\""));
    }

    #[test]
    fn empty_input_no_session_cache_helper_returns_empty_messages() {
        // 底层 helper `responses_body_to_chat_body`(不传 session_cache)的契约:
        // 没有 session_cache 时,根本不进 cache 查询路径,纯按当前 input 拼;
        // input 空就空 — 这条路径只服务于工具/测试场景,生产代理永远传
        // `Some(global_response_session_cache())`,见生产路径测试。
        let req = json!({
            "model": "x",
            "stream": true,
            "previous_response_id": "resp_unknown_to_cache",
            "tools": [{"type":"function","name":"shell","parameters":{"type":"object"}}],
            "input": []
        });
        let out = responses_body_to_chat_body(&req).expect("无 session_cache 路径不报错");
        let msgs = out["messages"].as_array().expect("messages 字段必须存在");
        assert!(msgs.is_empty(), "无 session_cache 时纯按 input 拼");
    }

    #[test]
    fn cache_miss_with_empty_input_returns_previous_response_not_found() {
        // 关键回归(2026-05-08):生产路径(传 session_cache),Codex CLI 用旧
        // previous_response_id 续轮(代理重启 / TTL 过期 / LRU 淘汰),但当前
        // input 为空 → 没有任何上下文可发上游 → 返回 OpenAI 标准
        // PreviousResponseNotFound,proxy IntoResponse 转 HTTP 400 +
        // `code: "previous_response_not_found"`,Codex CLI fail-fast 不重试。
        //
        // 历史:2026-05-06 ~ 2026-05-08 期间代码放行 messages:[] 给上游想触发
        // Codex 重试,但实测 Codex CLI `should_retry` 对 400 直接 fail-fast
        // (`codex-rs/codex-client/src/retry.rs`),只对 5xx + transport timeout
        // 重试 → 旧策略既不能修复,又额外引入上游 RTT(实测 Kimi 19s+)。
        let cache = ResponseSessionCache::new(8, std::time::Duration::from_secs(60));
        let req = json!({
            "model": "x",
            "stream": true,
            "previous_response_id": "resp_unknown_to_cache",
            "input": []
        });
        let err = responses_body_to_chat_body_for_provider_with_session(&req, None, Some(&cache))
            .err()
            .expect("cache miss + empty input 必须报错");
        match err {
            AdapterError::PreviousResponseNotFound {
                previous_response_id,
            } => {
                assert_eq!(previous_response_id, "resp_unknown_to_cache");
            }
            other => panic!("预期 PreviousResponseNotFound,实际 {other:?}"),
        }
    }

    #[test]
    fn cache_miss_with_nonempty_input_falls_back_to_current_only() {
        // cache miss 但 input 非空 → 保留旧降级:丢 previous_response_id,只用
        // 当前 input。这条路径不报错(模型可能丢上下文,但至少能继续对话),
        // 跟 PreviousResponseNotFound 路径区分清楚。
        let cache = ResponseSessionCache::new(8, std::time::Duration::from_secs(60));
        let req = json!({
            "model": "x",
            "stream": true,
            "previous_response_id": "resp_unknown_to_cache",
            "input": [{"type":"message","role":"user","content":"hi"}]
        });
        let out = responses_body_to_chat_body_for_provider_with_session(&req, None, Some(&cache))
            .expect("cache miss 但 input 非空 → 降级,不报错");
        let msgs = out.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn empty_input_but_with_instructions_passes_through() {
        // 只要有 instructions(system 头),messages 就非空,正常往上游发。
        let req = json!({
            "model": "x",
            "stream": true,
            "instructions": "You are Codex.",
            "input": []
        });
        let out = responses_body_to_chat_body(&req).expect("应当通过");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "system");
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
    fn compaction_item_renders_as_user_message_with_summary_text() {
        // 关键回归:Codex CLI auto-compact 后,续轮 input[] 会带
        // {"type":"compaction","encrypted_content":"<SUMMARY_PREFIX>\n<summary>"}。
        // 必须转成 user message,跟 Codex 自家 inline compact 行为对齐;否则
        // 上游 LLM 完全看不到 summary,等于 compact 后失忆。
        let out = convert(json!({
            "input": [{
                "type": "compaction",
                "encrypted_content": "Another language model started... <SUMMARY>: user wanted X, we did Y."
            }]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "user");
        assert!(msg["content"]
            .as_str()
            .unwrap_or("")
            .contains("user wanted X, we did Y"));
    }

    #[test]
    fn context_compaction_alias_renders_same_as_compaction() {
        // ResponseItem::ContextCompaction 是 Codex protocol 里同一概念的别名
        // (`codex-rs/protocol/src/models.rs:884`),也要识别。
        let out = convert(json!({
            "input": [{
                "type": "context_compaction",
                "encrypted_content": "summary body"
            }]
        }));
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], "summary body");
    }

    #[test]
    fn compaction_item_with_empty_encrypted_content_is_dropped() {
        // 防御:空 summary 不应往上游塞空 user message(会触发某些 provider
        // "user message must not be empty" 400)
        let out = convert(json!({
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "real user msg"}
                ]},
                {"type": "compaction", "encrypted_content": "   "}
            ]
        }));
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "空 compaction 应被丢弃,只剩真实 user");
        // content 可能是 string 或 array,都接受 — 关键是没 compaction 留下来
        let content_str = serde_json::to_string(&messages[0]["content"]).unwrap();
        assert!(
            content_str.contains("real user msg"),
            "应保留真实 user message 内容,实际: {content_str}"
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

    /// 给单测用的隔离 cache,避免并行测试互相污染。
    fn empty_tool_cache() -> super::super::tool_call_cache::ToolCallCache {
        super::super::tool_call_cache::ToolCallCache::new(16, std::time::Duration::from_secs(60))
    }

    #[test]
    fn function_call_output_becomes_tool_message_with_placeholder_assistant() {
        // 孤儿 function_call_output(无前置 function_call):repair 路径 B-orphan
        // 必须在它前面插占位 assistant.tool_calls,Chat 上游(Kimi/DeepSeek)
        // 严格校验时才能匹配住 tool_call_id,不会 400。
        let mut messages = vec![json!({
            "role": "tool",
            "tool_call_id": "call_abc",
            "content": "sunny",
        })];
        let cache = empty_tool_cache();
        repair_tool_call_ids(&mut messages, &cache);
        assert_eq!(messages.len(), 2, "孤儿 tool 前应插占位 assistant");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_abc");
        assert_eq!(messages[1]["content"], "sunny");
    }

    #[test]
    fn function_call_output_non_string_is_json_serialized() {
        // 走完整 convert 路径(global cache 在生产里就这条路);
        // 这里只关心 content 序列化,不关心占位 assistant 行为(见上一条测试)。
        let out = convert(json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "c",
                "output": {"temp": 72}
            }]
        }));
        let tool_msg = out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("应当有 tool 消息");
        assert_eq!(tool_msg["content"], "{\"temp\":72}");
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
    fn orphan_tool_with_call_id_rebuilds_from_tool_call_cache() {
        // path B-orphan + cache 命中:占位 assistant 应当用 cache 里的 name +
        // arguments,让 Chat 上游能按真实工具名重建上下文。
        let cache = empty_tool_cache();
        cache.save(
            "call_rebuild",
            super::super::tool_call_cache::ToolCallEntry {
                name: "shell".to_owned(),
                arguments: r#"{"cmd":"ls"}"#.to_owned(),
            },
        );
        let mut messages = vec![json!({
            "role": "tool",
            "tool_call_id": "call_rebuild",
            "content": "/repo",
        })];
        repair_tool_call_ids(&mut messages, &cache);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_rebuild");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"cmd":"ls"}"#
        );
        assert_eq!(messages[1]["tool_call_id"], "call_rebuild");
    }

    #[test]
    fn orphan_tool_with_call_id_inserts_tool_call_into_existing_assistant() {
        // path B-into-existing:user → assistant(无 tool_calls)→ tool
        // (call_id 不在前 assistant 的 tool_calls 里)。应当把重建的
        // tool_call 注回到那条 assistant 里,而不是再插一条占位。
        let cache = empty_tool_cache();
        cache.save(
            "call_inject",
            super::super::tool_call_cache::ToolCallEntry {
                name: "search".to_owned(),
                arguments: "{}".to_owned(),
            },
        );
        let mut messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "thinking"}),
            json!({"role": "tool", "tool_call_id": "call_inject", "content": "ok"}),
        ];
        repair_tool_call_ids(&mut messages, &cache);
        assert_eq!(
            messages.len(),
            3,
            "不应插占位 assistant,只在已有 assistant 里加 tool_calls"
        );
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_inject");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_inject");
    }

    #[test]
    fn user_message_after_tool_call_resets_pending_state() {
        // path "boundary":user / system / developer 出现时清掉 pending +
        // last_assistant_idx,后续孤儿 tool 不会错把那条 assistant 当作注入
        // 目标,而是在 tool 前再插占位 assistant。
        let cache = empty_tool_cache();
        let mut messages = vec![
            json!({"role": "assistant", "content": ""}),
            json!({"role": "user", "content": "next"}),
            json!({"role": "tool", "tool_call_id": "call_after_user", "content": "x"}),
        ];
        repair_tool_call_ids(&mut messages, &cache);
        let assistant_count = messages.iter().filter(|m| m["role"] == "assistant").count();
        assert!(
            assistant_count >= 2,
            "user 边界后再来 orphan tool 必须重新插占位 assistant,实际 {assistant_count}"
        );
        let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool_msg["tool_call_id"], "call_after_user");
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
    fn reasoning_summary_strips_codex_thinking_prefix_on_continuation() {
        // 续轮场景:Codex CLI 把上一轮 v2.0.8 注入的 `**Thinking**\n\n` prefix
        // 通过 reasoning summary 文本回送回来。proxy 在写回上游 messages.reasoning_content
        // 之前必须 strip,避免 prefix 累积污染上游 history。
        let out = convert(json!({
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": "**Thinking**\n\nI should inspect the repo."
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
        assert_eq!(
            msg["reasoning_content"], "I should inspect the repo.",
            "**Thinking**\\n\\n prefix 应被 strip,只保留原始 reasoning"
        );
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
            "input": "hi",
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
            "input": "hi",
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
            "input": "hi",
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
            "input": "hi",
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
        let out = convert(json!({"input": "hi", "max_output_tokens": 256}));
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
