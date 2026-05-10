//! Codex.app Responses API body → Gemini native `generateContent` RequestBody.
//!
//! 设计:**两层归一化**(用户决策 2026-05-10:跳过 ResponsesAdapter,
//! Gemini adapter 内部自给自足)。
//!
//! ① `responses_body_to_normalized_chat(body)`:Codex.app /responses 入站
//!    body → 统一 chat-shape 中间形态(messages 数组 + 顶层字段)。**不依赖**
//!    `crates/adapters/src/responses/`,本地实现 input array / tools /
//!    text.format / reasoning.effort 转换,确保 web_search 等 Gemini 关键
//!    字段不被 provider-specific drop 吃掉。
//! ② `chat_normalized_to_gemini_request(chat_body, model)`:LiteLLM 1:1 移植
//!    `litellm/llms/vertex_ai/gemini/transformation.py:_gemini_convert_messages_with_history`
//!    + `vertex_and_google_ai_studio_gemini.py:map_openai_params/_map_function/
//!    map_tool_choice_values` 的 chat → Gemini 转换。
//!
//! Must 范围(2026-05-10 用户确认):覆盖 LiteLLM 主线 + 4 关键缺漏:
//! - ✅ messages/tools/tool_choice/generation_config 主体 1:1
//! - ✅ Gemini 3+ 用 v1alpha endpoint(LiteLLM `common_utils.py:412`)
//! - ✅ Gemini 3+ 默认 temperature=1.0(LiteLLM 实证 < 1 触发 infinite loop)
//! - ✅ thinkingConfig:Gemini 3+ 用 thinkingLevel,Gemini 2.x 用 thinkingBudget
//! - ✅ schema sanitize 增强(enum 空字符串 → null / anyOf null → nullable /
//!   object type 默认 / additionalProperties+strict+$schema+$id 剥)
//!
//! Should 范围(Codex.app 当前不发,**留 TODO follow-up**):
//! - 🔵 audio/speechConfig / computer_use / google_maps / url_context /
//!   code_execution / modalities / logprobs / Anthropic-thinking-param /
//!   service_tier / include_server_side_tool_invocations / legacy
//!   google_search_retrieval / enterprise_web_search

use std::collections::HashMap;

use codex_app_transfer_registry::Provider;
use serde_json::{json, Map, Value};

use crate::types::AdapterError;

use super::types::{
    Content, FileData, FunctionCall, FunctionCallingConfig, FunctionDeclaration, FunctionResponse,
    GenerationConfig, InlineData, Part, RequestBody, SystemInstruction, ThinkingConfig, Tool,
    ToolConfig,
};

// ═══════════════════════════════════════════════════════════════════════════
// 顶层入口 — Codex.app /responses → Gemini RequestBody
// ═══════════════════════════════════════════════════════════════════════════

/// Codex.app /responses body 整体 → Gemini RequestBody。
pub fn responses_body_to_gemini_request(
    body: &Value,
    provider: &Provider,
) -> Result<RequestBody, AdapterError> {
    // Step 1: Codex.app /responses → 归一化 chat-shape 中间表示
    let chat_body = responses_body_to_normalized_chat(body)?;
    // Step 2: chat → Gemini wire(LiteLLM 1:1 移植)
    chat_normalized_to_gemini_request(&chat_body, provider)
}

/// 拼上游 URL path:`/v1beta/models/{m}:streamGenerateContent?alt=sse` 等。
///
/// LiteLLM `common_utils.py:412 _is_gemini_3_or_newer`:Gemini 3+ 用 v1alpha,
/// 老版本 v1beta。如果 `base_url` 已经带版本号(`/v1beta` 或 `/v1alpha`),
/// adapter 不再补,respect 用户配置。
pub fn build_gemini_upstream_path(model: &str, stream: bool, base_url: &str) -> String {
    let base_has_version = base_url.contains("/v1beta") || base_url.contains("/v1alpha");
    // H5 修复:用户在 base_url 里 hardcode `/v1beta` + 用 Gemini 3.x model
    // → adapter 不能自动改路由,Gemini 上游会 400("model not supported on this version")。
    // warn 帮用户定位根因,而不是让他对着不知所云的 400 抓瞎。
    if base_url.contains("/v1beta") && is_gemini_3_or_newer(model) {
        tracing::warn!(
            model,
            base_url,
            "Gemini 3+ requires /v1alpha API endpoint; base_url pins /v1beta which will likely \
             result in upstream 400. Remove the version suffix from base_url to enable \
             auto-routing (Gemini 3+ → v1alpha, Gemini 2.x → v1beta)."
        );
    }
    let model_with_prefix = if model.starts_with("models/") {
        model.to_owned()
    } else {
        format!("models/{model}")
    };
    let endpoint = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    if base_has_version {
        format!("/{model_with_prefix}:{endpoint}")
    } else {
        let api_version = if is_gemini_3_or_newer(model) {
            "v1alpha"
        } else {
            "v1beta"
        };
        format!("/{api_version}/{model_with_prefix}:{endpoint}")
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Step 1:Codex.app /responses → 归一化 chat-shape 中间表示
// ═══════════════════════════════════════════════════════════════════════════

/// Codex.app /responses body → chat completions style body(本地中间表示)。
///
/// 输入(Codex.app /responses 形态):
/// ```json
/// {"model":"...", "instructions":"sys prompt",
///  "input":[{"type":"message",...},{"type":"function_call",...},{"type":"function_call_output",...}],
///  "tools":[{"type":"function","name":...,"parameters":...},{"type":"web_search",...},{"type":"custom",...}],
///  "tool_choice":"auto", "reasoning":{"effort":"high"},
///  "text":{"format":{"type":"json_schema","schema":...}},
///  "max_output_tokens":1024, "temperature":0.7, "stream":true}
/// ```
///
/// 输出(归一化 chat shape):
/// ```json
/// {"model":"...", "messages":[{"role":"system","content":"..."}, {"role":"user","content":...}, ...],
///  "tools":[{"type":"function","function":{"name":...,"parameters":...}}, {"type":"web_search"}],
///  "tool_choice":"auto", "reasoning_effort":"high",
///  "response_format":{"type":"json_schema","json_schema":{"schema":...}},
///  "max_tokens":1024, "temperature":0.7, "stream":true}
/// ```
pub fn responses_body_to_normalized_chat(body: &Value) -> Result<Value, AdapterError> {
    let body_obj = body
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("body must be JSON object".into()))?;

    let mut chat_body = Map::new();

    // model + stream 透传
    if let Some(model) = body_obj.get("model") {
        chat_body.insert("model".into(), model.clone());
    }
    if let Some(stream) = body_obj.get("stream") {
        chat_body.insert("stream".into(), stream.clone());
    }

    // instructions(顶层 string) + input array → messages
    let instructions = body_obj.get("instructions").and_then(|v| v.as_str());
    let input = match body_obj.get("input") {
        Some(Value::Array(arr)) => arr.clone(),
        // input 也允许是单个 string(Codex CLI 历史)
        Some(Value::String(s)) => vec![Value::Object({
            let mut m = Map::new();
            m.insert("type".into(), Value::String("message".into()));
            m.insert("role".into(), Value::String("user".into()));
            m.insert("content".into(), Value::String(s.clone()));
            m
        })],
        _ => Vec::new(),
    };
    let messages = responses_input_to_chat_messages(&input, instructions)?;
    chat_body.insert("messages".into(), Value::Array(messages));

    // tools[] 转 chat shape(保留 web_search,unwrap function/custom 等)
    if let Some(tools) = body_obj.get("tools").and_then(|v| v.as_array()) {
        let chat_tools = responses_tools_to_chat_tools(tools);
        if !chat_tools.is_empty() {
            chat_body.insert("tools".into(), Value::Array(chat_tools));
        }
    }
    // tool_choice 直接透传(Responses 跟 chat 形态一致)
    if let Some(tc) = body_obj.get("tool_choice") {
        chat_body.insert("tool_choice".into(), tc.clone());
    }

    // 顶层字段映射:max_output_tokens → max_tokens / reasoning.effort → reasoning_effort /
    // text.format → response_format
    if let Some(v) = body_obj.get("max_output_tokens") {
        chat_body.insert("max_tokens".into(), v.clone());
    }
    for k in ["temperature", "top_p", "top_k", "stop", "seed", "n"] {
        if let Some(v) = body_obj.get(k) {
            chat_body.insert(k.into(), v.clone());
        }
    }
    if let Some(reasoning) = body_obj.get("reasoning").and_then(|v| v.as_object()) {
        if let Some(effort) = reasoning.get("effort") {
            chat_body.insert("reasoning_effort".into(), effort.clone());
        }
    }
    if let Some(text) = body_obj.get("text").and_then(|v| v.as_object()) {
        if let Some(rf) = responses_text_format_to_response_format(text) {
            chat_body.insert("response_format".into(), rf);
        }
    }
    if let Some(eb) = body_obj.get("extra_body") {
        chat_body.insert("extra_body".into(), eb.clone());
    }
    if let Some(safety) = body_obj.get("safety_settings") {
        chat_body.insert("safety_settings".into(), safety.clone());
    }

    Ok(Value::Object(chat_body))
}

/// Responses input array + instructions → OpenAI chat messages 数组。
///
/// Codex.app /responses input element 类型:
/// - `{type:"message", role, content}` — 跟 OpenAI chat 同形态(role + string|[blocks])
/// - `{type:"function_call", call_id, name, arguments}` — assistant role 的 tool_call
/// - `{type:"function_call_output", call_id, output}` — tool role 的响应
/// - `{type:"reasoning", id, summary?, encrypted_content?}` — 历史回放 thinking 块
fn responses_input_to_chat_messages(
    input: &[Value],
    instructions: Option<&str>,
) -> Result<Vec<Value>, AdapterError> {
    let mut messages: Vec<Value> = Vec::new();

    // instructions → 顶层 system message(Gemini 端会被 split 到 systemInstruction)
    if let Some(s) = instructions {
        if !s.is_empty() {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("system".into()));
            m.insert("content".into(), Value::String(s.to_owned()));
            messages.push(Value::Object(m));
        }
    }

    // 累积 pending assistant tool_calls,合并到下一个非 function_call/output 的 message
    let mut pending_tool_calls: Vec<Value> = Vec::new();
    let mut pending_assistant_content: Option<Value> = None;
    let mut pending_assistant_reasoning: Vec<String> = Vec::new();

    let flush_assistant = |pending_content: &mut Option<Value>,
                           pending_calls: &mut Vec<Value>,
                           pending_reasoning: &mut Vec<String>,
                           out: &mut Vec<Value>| {
        if pending_content.is_some() || !pending_calls.is_empty() || !pending_reasoning.is_empty() {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("assistant".into()));
            m.insert(
                "content".into(),
                pending_content.take().unwrap_or(Value::Null),
            );
            if !pending_calls.is_empty() {
                m.insert(
                    "tool_calls".into(),
                    Value::Array(std::mem::take(pending_calls)),
                );
            }
            if !pending_reasoning.is_empty() {
                m.insert(
                    "reasoning_content".into(),
                    Value::String(pending_reasoning.join("\n")),
                );
                pending_reasoning.clear();
            }
            out.push(Value::Object(m));
        }
    };

    for item in input {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("message");
        match item_type {
            "message" => {
                // 先 flush pending assistant
                flush_assistant(
                    &mut pending_assistant_content,
                    &mut pending_tool_calls,
                    &mut pending_assistant_reasoning,
                    &mut messages,
                );
                let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let content = obj.get("content").cloned().unwrap_or(Value::Null);
                let normalized_content = normalize_responses_message_content(&content);
                let mut m = Map::new();
                m.insert("role".into(), Value::String(role.to_owned()));
                m.insert("content".into(), normalized_content);
                messages.push(Value::Object(m));
            }
            "function_call" => {
                // 合并到 pending assistant(连续 function_call 合并 tool_calls 数组)
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("call_unknown")
                    .to_owned();
                let name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let arguments = obj
                    .get("arguments")
                    .map(|v| {
                        if let Some(s) = v.as_str() {
                            s.to_owned()
                        } else {
                            v.to_string()
                        }
                    })
                    .unwrap_or_else(|| "{}".to_owned());
                let mut tc = Map::new();
                tc.insert("id".into(), Value::String(call_id));
                tc.insert("type".into(), Value::String("function".into()));
                let mut func = Map::new();
                func.insert("name".into(), Value::String(name));
                func.insert("arguments".into(), Value::String(arguments));
                tc.insert("function".into(), Value::Object(func));
                pending_tool_calls.push(Value::Object(tc));
            }
            "function_call_output" => {
                let call_id = obj
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("call_unknown")
                    .to_owned();
                let output = obj.get("output").cloned().unwrap_or(Value::Null);
                let content_str = match &output {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                // P0-G + Bug B 修复:Codex.app 不重发 prior function_call,但 Gemini
                // 强制要求 functionCall turn(model role) 紧跟 functionResponse turn
                // (user role)。从 global ToolCallCache 拿 (name, arguments) 在 messages
                // 里 synthesize prior function_call 重建上下文。
                // 如果当前 turn 已有 prior(pending_tool_calls / 已 flush 的 messages
                // assistant)就不 synthesize 防重复;cache 也没就 fallback 让下游 BadRequest。
                let cache_entry = crate::responses::global_tool_call_cache().get(&call_id);
                let prior_in_pending = pending_tool_calls
                    .iter()
                    .any(|tc| tc.get("id").and_then(|v| v.as_str()) == Some(call_id.as_str()));
                let prior_in_messages = messages.iter().rev().take(8).any(|m| {
                    m.get("tool_calls")
                        .and_then(|v| v.as_array())
                        .is_some_and(|arr| {
                            arr.iter().any(|tc| {
                                tc.get("id").and_then(|v| v.as_str()) == Some(call_id.as_str())
                            })
                        })
                });
                let need_synthesize = !prior_in_pending && !prior_in_messages;
                let resolved_name = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| cache_entry.as_ref().map(|entry| entry.name.clone()));

                // flush pending assistant 再操作
                flush_assistant(
                    &mut pending_assistant_content,
                    &mut pending_tool_calls,
                    &mut pending_assistant_reasoning,
                    &mut messages,
                );

                if need_synthesize {
                    if let (Some(name), Some(entry)) = (&resolved_name, &cache_entry) {
                        // synthesize prior assistant tool_call message —— 让下游
                        // convert_messages_to_contents 形成 model role 的 functionCall turn,
                        // Gemini "function response turn 必须紧跟 function call turn" 满足
                        let mut tc = Map::new();
                        tc.insert("id".into(), Value::String(call_id.clone()));
                        tc.insert("type".into(), Value::String("function".into()));
                        let mut func = Map::new();
                        func.insert("name".into(), Value::String(name.clone()));
                        func.insert("arguments".into(), Value::String(entry.arguments.clone()));
                        tc.insert("function".into(), Value::Object(func));
                        let mut synthetic = Map::new();
                        synthetic.insert("role".into(), Value::String("assistant".into()));
                        synthetic.insert("content".into(), Value::Null);
                        synthetic
                            .insert("tool_calls".into(), Value::Array(vec![Value::Object(tc)]));
                        messages.push(Value::Object(synthetic));
                        tracing::debug!(
                            call_id,
                            "gemini_native: synthesized prior assistant function_call from cache \
                             (Codex.app didn't resend prior function_call; Gemini wire requires \
                             functionCall turn before functionResponse turn)"
                        );
                    }
                }

                let mut m = Map::new();
                m.insert("role".into(), Value::String("tool".into()));
                m.insert("tool_call_id".into(), Value::String(call_id));
                m.insert("content".into(), Value::String(content_str));
                if let Some(n) = resolved_name {
                    m.insert("name".into(), Value::String(n));
                }
                messages.push(Value::Object(m));
            }
            "reasoning" => {
                // 历史回放 thinking 块(Codex.app /responses 客户端会回送 reasoning items
                // 用于 session resume — Gemini 那端转 thought=true part)
                if let Some(summary) = obj.get("summary").and_then(|v| v.as_array()) {
                    for sum in summary {
                        if let Some(s) = sum.as_str() {
                            pending_assistant_reasoning.push(s.to_owned());
                        } else if let Some(t) = sum.get("text").and_then(|v| v.as_str()) {
                            pending_assistant_reasoning.push(t.to_owned());
                        }
                    }
                }
            }
            // 其他类型(computer_call / image_generation_call / file_search_call /
            // mcp_call / local_shell_call / code_interpreter_call ...)Codex.app
            // 当前不发,但加 warn_once_drop_tool 让以后 Codex 加新 type 时立刻在
            // telemetry 看到 + 帮我们快速定位需补哪种 type
            other => {
                crate::warn_once_drop_tool(&format!("gemini_native:input_item:{other}"));
            }
        }
    }
    // 收尾 flush
    flush_assistant(
        &mut pending_assistant_content,
        &mut pending_tool_calls,
        &mut pending_assistant_reasoning,
        &mut messages,
    );

    Ok(messages)
}

/// 把 Responses message content 归一到 chat completions content 形态。
/// Responses 块类型:`input_text` / `input_image` / `output_text` / `input_file`。
/// 转成 chat 的 `text` / `image_url` 块。
fn normalize_responses_message_content(content: &Value) -> Value {
    if let Some(s) = content.as_str() {
        return Value::String(s.to_owned());
    }
    let Some(arr) = content.as_array() else {
        return Value::Null;
    };
    let mut out: Vec<Value> = Vec::new();
    for block in arr {
        let Some(obj) = block.as_object() else {
            continue;
        };
        let block_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match block_type {
            "input_text" | "output_text" | "text" => {
                if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                    let mut m = Map::new();
                    m.insert("type".into(), Value::String("text".into()));
                    m.insert("text".into(), Value::String(t.to_owned()));
                    out.push(Value::Object(m));
                }
            }
            "input_image" => {
                // Codex.app: {type:"input_image", image_url:"data:..." or "https://..."}
                if let Some(url) = obj.get("image_url").and_then(|v| v.as_str()) {
                    let mut img = Map::new();
                    img.insert("url".into(), Value::String(url.to_owned()));
                    if let Some(detail) = obj.get("detail").and_then(|v| v.as_str()) {
                        img.insert("detail".into(), Value::String(detail.to_owned()));
                    }
                    let mut m = Map::new();
                    m.insert("type".into(), Value::String("image_url".into()));
                    m.insert("image_url".into(), Value::Object(img));
                    out.push(Value::Object(m));
                }
            }
            "input_audio" => {
                if let Some(inner) = obj.get("input_audio").cloned() {
                    let mut m = Map::new();
                    m.insert("type".into(), Value::String("input_audio".into()));
                    m.insert("input_audio".into(), inner);
                    out.push(Value::Object(m));
                }
            }
            "input_file" => {
                // 提取 file_url / file_id / file_data 任一,转 chat 标准 image_url 块
                // (Gemini 端 image_url_block_to_part 会进一步转 fileData / inlineData,
                // 不再静默改成 [file omitted] 占位 text — 那是 destructive 降级)。
                let url = obj
                    .get("file_url")
                    .or_else(|| obj.get("file_id"))
                    .or_else(|| obj.get("file_data"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                if let Some(url) = url {
                    let mut img = Map::new();
                    img.insert("url".into(), Value::String(url));
                    if let Some(filename) = obj.get("filename").and_then(|v| v.as_str()) {
                        img.insert("filename".into(), Value::String(filename.to_owned()));
                    }
                    if let Some(mime) = obj.get("mime_type").and_then(|v| v.as_str()) {
                        img.insert("mime_type".into(), Value::String(mime.to_owned()));
                    }
                    let mut m = Map::new();
                    m.insert("type".into(), Value::String("input_file".into()));
                    m.insert("input_file".into(), Value::Object(img));
                    out.push(Value::Object(m));
                } else {
                    crate::warn_once_drop_tool("gemini_native:input_file:no_url_or_data");
                }
            }
            other => {
                crate::warn_once_drop_tool(&format!("gemini_native:content_block:{other}"));
            }
        }
    }
    Value::Array(out)
}

/// Codex.app Responses tools[] → chat completions tools[]。
/// **保留** web_search(下一步会被 chat→Gemini 转成 googleSearch),不丢。
fn responses_tools_to_chat_tools(tools: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match tool_type {
            "function" => {
                // Responses: {type:"function", name, description?, parameters}
                // chat: {type:"function", function:{name, description?, parameters}}
                let mut func = Map::new();
                if let Some(n) = obj.get("name") {
                    func.insert("name".into(), n.clone());
                }
                if let Some(d) = obj.get("description") {
                    func.insert("description".into(), d.clone());
                }
                if let Some(p) = obj.get("parameters") {
                    func.insert("parameters".into(), p.clone());
                }
                let mut wrapper = Map::new();
                wrapper.insert("type".into(), Value::String("function".into()));
                wrapper.insert("function".into(), Value::Object(func));
                out.push(Value::Object(wrapper));
            }
            "web_search" | "web_search_preview" => {
                // 直接保留,chat→Gemini 会识别并转 googleSearch
                let mut m = Map::new();
                m.insert("type".into(), Value::String("web_search".into()));
                out.push(Value::Object(m));
            }
            "custom" => {
                // Codex.app 私有 custom tool — 当 function declaration 处理
                let mut func = Map::new();
                if let Some(n) = obj.get("name") {
                    func.insert("name".into(), n.clone());
                }
                if let Some(d) = obj.get("description") {
                    func.insert("description".into(), d.clone());
                }
                if let Some(p) = obj.get("parameters") {
                    func.insert("parameters".into(), p.clone());
                }
                let mut wrapper = Map::new();
                wrapper.insert("type".into(), Value::String("function".into()));
                wrapper.insert("function".into(), Value::Object(func));
                out.push(Value::Object(wrapper));
            }
            // computer_use_preview / file_search / image_generation 等 Gemini 不直接对应。
            // warn_once 让以后用户配新 tool 类型时能在 telemetry 立刻看到 silent drop。
            other => {
                crate::warn_once_drop_tool(&format!("gemini_native:responses_tool:{other}"));
            }
        }
    }
    out
}

/// Responses `text.format` → chat `response_format`。
/// Responses: `{format:{type:"json_schema",name:"...",schema:{...},strict:true}}`
/// chat: `{type:"json_schema",json_schema:{name:"...",schema:{...},strict:true}}`
fn responses_text_format_to_response_format(text: &Map<String, Value>) -> Option<Value> {
    let format = text.get("format")?.as_object()?;
    let format_type = format.get("type").and_then(|v| v.as_str())?;
    let mut out = Map::new();
    out.insert("type".into(), Value::String(format_type.to_owned()));
    if format_type == "json_schema" {
        let mut js = Map::new();
        if let Some(n) = format.get("name") {
            js.insert("name".into(), n.clone());
        }
        if let Some(s) = format.get("schema") {
            js.insert("schema".into(), s.clone());
        }
        if let Some(s) = format.get("strict") {
            js.insert("strict".into(), s.clone());
        }
        out.insert("json_schema".into(), Value::Object(js));
    }
    Some(Value::Object(out))
}

// ═══════════════════════════════════════════════════════════════════════════
// Step 2:chat-shape body → Gemini RequestBody(LiteLLM 1:1 移植)
// ═══════════════════════════════════════════════════════════════════════════

pub fn chat_normalized_to_gemini_request(
    body: &Value,
    _provider: &Provider,
) -> Result<RequestBody, AdapterError> {
    let body_obj = body
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("chat body must be JSON object".into()))?;

    let messages = body_obj
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::BadRequest("messages array required".into()))?;
    let model = body_obj.get("model").and_then(|v| v.as_str()).unwrap_or("");

    let (mut system_instruction, body_messages) = split_system_instruction(messages);
    let contents = convert_messages_to_contents(&body_messages)?;

    let mut tools = body_obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| convert_tools(arr))
        .filter(|v: &Vec<Tool>| !v.is_empty());

    let mut tool_config = body_obj.get("tool_choice").and_then(convert_tool_choice);

    let generation_config = build_generation_config(body_obj, model);

    let safety_settings = body_obj
        .get("safety_settings")
        .or_else(|| body_obj.get("safetySettings"))
        .and_then(|v| v.as_array())
        .cloned();

    // **Gemini wire 约束**(2026-05-10 实测 400):function_declarations + responseMimeType
    // 不能共存,Gemini 返 "Function calling with a response mime type:
    // 'application/json' is unsupported"。Codex.app 同时发 tools(function calling)+
    // text.format=json_schema 时(让 LLM 走 message 输出时强制 JSON),Gemini 拒绝。
    //
    // **不主动破坏性降级**(memory rule:用户硬性规则)— 不能简单 drop response_schema
    // 字段(那会丢失用户语义)。改用 LiteLLM `apply_response_schema_transformation`
    // 思路:wire 上必须 drop 这两字段(否则 Gemini 400),但**语义不丢**:把 schema
    // 拼成 systemInstruction text 作软约束传给 model。Model 调 tool 时不变;走 message
    // 输出时被 prompted 按 schema 输出(虽是软约束不是硬,但比 wire-level json_schema
    // 完全丢失好得多 — Gemini 模型对 system_instruction JSON 约束服从度高)。
    let has_function_decls = tools
        .as_ref()
        .is_some_and(|t| t.iter().any(|tool| tool.function_declarations.is_some()));
    let mut generation_config = generation_config;
    if has_function_decls {
        if let Some(gc) = generation_config.as_mut() {
            if gc.response_mime_type.is_some() || gc.response_schema.is_some() {
                let schema_str = gc
                    .response_schema
                    .as_ref()
                    .and_then(|s| serde_json::to_string(s).ok())
                    .filter(|s| !s.is_empty());
                let constraint_text = match schema_str {
                    Some(s) => format!(
                        "When you produce a textual answer (rather than invoking a tool), your output MUST be a valid JSON object matching this schema:\n\n{s}\n\nDo not wrap the JSON in markdown fences and do not add any commentary."
                    ),
                    None => "When you produce a textual answer (rather than invoking a tool), your output MUST be a valid JSON object. Do not wrap the JSON in markdown fences and do not add any commentary.".into(),
                };
                let si = system_instruction.get_or_insert_with(|| SystemInstruction {
                    role: None,
                    parts: Vec::new(),
                });
                si.parts.push(Part {
                    text: Some(constraint_text),
                    ..Default::default()
                });
                tracing::info!(
                    "gemini_native: responseMimeType/responseSchema cannot coexist with functionDeclarations on Gemini wire; transformed into systemInstruction soft constraint (LiteLLM apply_response_schema_transformation pattern). Function calling unaffected; textual responses still prompted to follow JSON schema."
                );
                gc.response_mime_type = None;
                gc.response_schema = None;
            }
        }
    }

    // **Gemini wire 约束** (2026-05-10 实测 400):googleSearch (built-in tool) +
    // functionDeclarations (Codex exec_command/Read/Write 等)Gemini 拒绝共存,
    // 返 "Built-in tools ({google_search}) and Function Calling cannot be combined
    // in the same request."
    //
    // 处理(用户决策 2026-05-10:B 方案):
    // - **Gemini 3+**:加 `toolConfig.includeServerSideToolInvocations=true`
    //   让两者共存(Gemini 3+ 支持 server-side tool 跟 function declaration 共存)
    // - **Gemini 2.x / 老版本**:wire 必须 drop googleSearch(否则 400),但**软约束保留**
    //   语义 — 拼 system_instruction 提示 model "用户期望联网搜索但当前 model 不支持
    //   两者共存,需联网信息时告知用户限制",function calling 保留(Codex 核心)
    let has_google_search = tools
        .as_ref()
        .is_some_and(|t| t.iter().any(|tool| tool.google_search.is_some()));
    if has_function_decls && has_google_search {
        if is_gemini_3_or_newer(model) {
            // Gemini 3+:toolConfig.includeServerSideToolInvocations=true 共存
            let tc = tool_config.get_or_insert_with(|| ToolConfig {
                function_calling_config: None,
                include_server_side_tool_invocations: None,
                retrieval_config: None,
            });
            tc.include_server_side_tool_invocations = Some(true);
            tracing::info!(
                "gemini_native: Gemini 3+ enabled toolConfig.includeServerSideToolInvocations=true \
                 to allow function declarations + googleSearch coexistence (no destructive drop)"
            );
        } else {
            // Gemini 2.x:wire drop googleSearch + system_instruction 软约束
            if let Some(tools_vec) = tools.as_mut() {
                tools_vec.retain(|tool| tool.google_search.is_none());
                if tools_vec.is_empty() {
                    tools = None;
                }
            }
            // **文案精准**(2026-05-10 修):旧文案 "If you need to fetch external
            // information, tell the user this limitation" 让 model 把"联网"泛化理解成
            // 包括 exec_command/curl 也禁 → user 让 model 查天气,model 直接 refuse
            // 不试 curl。改成只精准说明 google_search 工具不可用,**显式告知** model
            // 仍可以用 exec_command 跑 curl/wget 等 HTTP 请求实现联网(若 sandbox
            // 允许 network)。
            let constraint_text = "Note about tool availability: The built-in `google_search` \
                (web_search) tool from Gemini is currently unavailable in this request because \
                this Gemini 2.x model does not allow built-in tools and function declarations to \
                coexist on the wire. **All your other function-calling tools (e.g. exec_command, \
                Read, Write, Bash) are fully available** — including using `exec_command` to run \
                `curl`/`wget` for HTTP requests if the sandbox permissions allow network access. \
                Only mention this limitation explicitly if the user asks specifically for the \
                built-in google_search feature; do not refuse network-related tasks just because \
                google_search is disabled (use other tools to fulfill the request).";
            let si = system_instruction.get_or_insert_with(|| SystemInstruction {
                role: None,
                parts: Vec::new(),
            });
            si.parts.push(Part {
                text: Some(constraint_text.into()),
                ..Default::default()
            });
            tracing::info!(
                "gemini_native: Gemini 2.x dropped googleSearch wire (Gemini wire 强制 + functionDecls \
                 priority) + added system_instruction soft constraint (LiteLLM _resolve_search_tool_conflict pattern)"
            );
        }
    }

    // **Bug A 修复**(2026-05-10 实测 400):"Function calling config is set without
    // function_declarations" — Gemini 拒 tool_config(functionCallingConfig)单独
    // 出现而无 functionDeclarations。Codex.app 内部 task(如 Memory Writing Agent)
    // 不发 tools 但仍发 tool_choice="auto" → 我们转出 toolConfig → Gemini 400。
    //
    // **不主动破坏性降级**(用户硬性规则)— 按 tool_choice 实际值分支:
    // - "auto" / "none":没 tools 时跟"不传 tool_choice"等价,drop 是 normalize 无损
    // - "required" / {function:{name:"X"}}:client 请求自相矛盾(必须调工具但
    //   没 tool 可调 / 指定 X 但 tools 里没 X),BadRequest 让 client 看到根因,
    //   不主动 silent drop
    let has_any_function_decls = tools
        .as_ref()
        .is_some_and(|t| t.iter().any(|tool| tool.function_declarations.is_some()));
    if !has_any_function_decls {
        if let Some(tc) = tool_config.as_ref() {
            if let Some(fcc) = &tc.function_calling_config {
                let mode = fcc.mode.to_ascii_uppercase();
                let has_allowed = fcc
                    .allowed_function_names
                    .as_ref()
                    .is_some_and(|v| !v.is_empty());
                match mode.as_str() {
                    "AUTO" | "NONE" => {
                        // 无损 normalize:无 tools 时 auto/none 跟不传 tool_choice 等价
                        if let Some(tc_mut) = tool_config.as_mut() {
                            tc_mut.function_calling_config = None;
                            // 若 toolConfig 整体空(仅 functionCallingConfig 一项),整 drop
                            if tc_mut.include_server_side_tool_invocations.is_none()
                                && tc_mut.retrieval_config.is_none()
                            {
                                tool_config = None;
                            }
                        }
                    }
                    "ANY" if has_allowed => {
                        return Err(AdapterError::BadRequest(format!(
                            "tool_choice specifies function name(s) {:?} but no tools/functionDeclarations \
                             are provided in the request — this is a client-side mismatch (Gemini wire \
                             requires functionDeclarations whenever tool_config is set). Either include \
                             the matching function in tools, or omit tool_choice.",
                            fcc.allowed_function_names.as_ref().unwrap()
                        )));
                    }
                    "ANY" => {
                        return Err(AdapterError::BadRequest(
                            "tool_choice=\"required\" (Gemini ANY mode) without any tools is a \
                             client-side mismatch — model is told to invoke a tool but no tools are \
                             available. Either provide tools, or change tool_choice to \"auto\"/\"none\"."
                                .to_string(),
                        ));
                    }
                    _ => {
                        // 未知 mode — 同 normalize 处理(drop)
                        if let Some(tc_mut) = tool_config.as_mut() {
                            tc_mut.function_calling_config = None;
                            if tc_mut.include_server_side_tool_invocations.is_none()
                                && tc_mut.retrieval_config.is_none()
                            {
                                tool_config = None;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut request = RequestBody {
        contents,
        system_instruction,
        tools,
        tool_config,
        safety_settings,
        generation_config,
        cached_content: None,
    };
    apply_extra_body_overrides(&mut request, body_obj)?;
    Ok(request)
}

// ───────── system message extraction ─────────

fn split_system_instruction(messages: &[Value]) -> (Option<SystemInstruction>, Vec<Value>) {
    let mut sys_parts: Vec<Part> = Vec::new();
    let mut rest: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "system" || role == "developer" {
            if let Some(content) = msg.get("content") {
                push_text_or_parts(content, &mut sys_parts);
            }
        } else {
            rest.push(msg.clone());
        }
    }
    let si = if sys_parts.is_empty() {
        None
    } else {
        Some(SystemInstruction {
            role: None,
            parts: sys_parts,
        })
    };
    (si, rest)
}

fn push_text_or_parts(content: &Value, out: &mut Vec<Part>) {
    if let Some(s) = content.as_str() {
        if !s.is_empty() {
            out.push(Part {
                text: Some(s.to_owned()),
                ..Default::default()
            });
        }
    } else if let Some(arr) = content.as_array() {
        for element in arr {
            if let Some(part) = convert_content_block_to_part(element) {
                out.push(part);
            }
        }
    }
}

// ───────── messages → contents (LiteLLM transformation.py:311) ─────────

fn convert_messages_to_contents(messages: &[Value]) -> Result<Vec<Content>, AdapterError> {
    let mut contents: Vec<Content> = Vec::new();
    let mut tool_call_responses: Vec<Part> = Vec::new();
    let mut tool_call_id_to_name: HashMap<String, String> = HashMap::new();

    let mut msg_i = 0;
    while msg_i < messages.len() {
        let init = msg_i;

        // Phase 1: 合并连续 user 消息(system 已被 split)
        let mut user_parts: Vec<Part> = Vec::new();
        while msg_i < messages.len() && role_of(&messages[msg_i]) == "user" {
            if let Some(content) = messages[msg_i].get("content") {
                push_text_or_parts(content, &mut user_parts);
            }
            msg_i += 1;
        }
        if !user_parts.is_empty() {
            // LiteLLM issue #5515:user content 必须含至少一个 text part
            if !user_parts.iter().any(|p| p.text.is_some()) {
                user_parts.push(Part {
                    text: Some(" ".into()),
                    ..Default::default()
                });
            }
            contents.push(Content {
                role: "user".into(),
                parts: user_parts,
            });
        }

        // Phase 2: 合并连续 assistant 消息
        let mut assistant_parts: Vec<Part> = Vec::new();
        while msg_i < messages.len() && role_of(&messages[msg_i]) == "assistant" {
            let msg = &messages[msg_i];
            // reasoning_content / thinking_blocks → thought=true part(LiteLLM transformation.py:461)
            if let Some(rc) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
                if !rc.is_empty() {
                    assistant_parts.push(Part {
                        thought: Some(true),
                        text: Some(rc.to_owned()),
                        ..Default::default()
                    });
                }
            }
            if let Some(blocks) = msg.get("thinking_blocks").and_then(|v| v.as_array()) {
                for block in blocks {
                    let Some(b) = block.as_object() else { continue };
                    if b.get("type").and_then(|v| v.as_str()) != Some("thinking") {
                        continue;
                    }
                    let thinking = b.get("thinking").and_then(|v| v.as_str()).map(String::from);
                    let signature = b
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if thinking.is_some() || signature.is_some() {
                        assistant_parts.push(Part {
                            thought: Some(true),
                            text: thinking,
                            thought_signature: signature,
                            ..Default::default()
                        });
                    }
                }
            }
            if let Some(content) = msg.get("content") {
                push_text_or_parts(content, &mut assistant_parts);
            }
            // tool_calls → functionCall parts
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some((id, name, args, sig)) = extract_tool_call(tc) {
                        tool_call_id_to_name.insert(id, name.clone());
                        // P1-B:thoughtSignature 从 call_id 解出后写回 functionCall part,
                        // Gemini 3 多轮 thinking 上下文不断
                        assistant_parts.push(Part {
                            function_call: Some(FunctionCall { name, args }),
                            thought_signature: sig,
                            ..Default::default()
                        });
                    }
                }
            }
            // legacy function_call(deprecated)
            if let Some(fc) = msg.get("function_call").and_then(|v| v.as_object()) {
                if let Some(name) = fc.get("name").and_then(|v| v.as_str()) {
                    let args = fc.get("arguments");
                    let parsed_args = args
                        .and_then(|a| a.as_str().and_then(|s| serde_json::from_str(s).ok()))
                        .or_else(|| args.cloned())
                        .unwrap_or(Value::Null);
                    assistant_parts.push(Part {
                        function_call: Some(FunctionCall {
                            name: name.into(),
                            args: parsed_args,
                        }),
                        ..Default::default()
                    });
                }
            }
            msg_i += 1;
        }
        if !assistant_parts.is_empty() {
            contents.push(Content {
                role: "model".into(),
                parts: assistant_parts,
            });
        }

        // Phase 3: 收集连续 tool/function role response
        while msg_i < messages.len() {
            let role = role_of(&messages[msg_i]);
            if role != "tool" && role != "function" {
                break;
            }
            let msg = &messages[msg_i];
            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // C1 修复(silent-failure-hunter 报告):tool_call_id 找不到时
            // P1-D 修复(用户硬性规则:不主动破坏性降级):tool_call_id 找不到时
            // 不能 fake "tool" / 用 tool_call_id 当 name 给 Gemini → Gemini 400
            // (function name not in declarations)是 destructive。改成 BadRequest
            // 让客户端立刻看到清晰错。Codex.app 当前每轮发完整 input 包含
            // function_call + function_call_output 不会触发这条 path;若 Codex.app
            // 启用 session resume 而 SessionStore 还没实现,这条会触发 — 当前
            // 安全 BadRequest 让用户看到 "缺 SessionStore" 而不是 silent Gemini 400。
            let name = tool_call_id_to_name
                .get(tool_call_id)
                .cloned()
                .or_else(|| msg.get("name").and_then(|v| v.as_str()).map(String::from))
                .ok_or_else(|| {
                    AdapterError::BadRequest(format!(
                        "function_call_output call_id={tool_call_id:?} has no matching prior \
                         function_call in this request input — Gemini wire requires \
                         functionResponse.name. Pass the original function_call alongside its \
                         output in the input array, or include explicit `name` field on the \
                         function_call_output item. (Session-resume path: TODO add SessionStore lookup.)"
                    ))
                })?;
            let response_value = parse_tool_response_content(msg.get("content"))?;
            tool_call_responses.push(Part {
                function_response: Some(FunctionResponse {
                    name,
                    response: response_value,
                }),
                ..Default::default()
            });
            msg_i += 1;
        }

        if msg_i < messages.len() {
            let next_role = role_of(&messages[msg_i]);
            if next_role != "tool" && next_role != "function" && !tool_call_responses.is_empty() {
                contents.push(Content {
                    role: "user".into(),
                    parts: std::mem::take(&mut tool_call_responses),
                });
            }
        }

        if msg_i == init {
            return Err(AdapterError::BadRequest(format!(
                "invalid message at index {init} role={:?}",
                messages[init].get("role")
            )));
        }
    }

    if !tool_call_responses.is_empty() {
        contents.push(Content {
            role: "user".into(),
            parts: tool_call_responses,
        });
    }

    if contents.is_empty() {
        contents.push(Content {
            role: "user".into(),
            parts: vec![Part {
                text: Some(" ".into()),
                ..Default::default()
            }],
        });
    }

    // **Gemini wire 严格要求**(2026-05-10 实测 400):"function call turn comes
    // immediately after a user turn or after a function response turn" — contents
    // 必须以 user role 开头(且 user/model 严格交替)。Codex.app 多轮 session resume
    // 时不重发早期 user turn,仅发 function_call_output;我们 synthesize prior
    // function_call(model role)后 contents 仍以 model 开头 → Gemini 拒。
    //
    // **非破坏性修复**:contents 第一条若是 model role,前面插入 synthetic user
    // 占位 turn 解释上下文(Gemini 模型对 user turn 文案不敏感,只要满足 alternation)。
    if let Some(first) = contents.first() {
        if first.role == "model" {
            contents.insert(
                0,
                Content {
                    role: "user".into(),
                    parts: vec![Part {
                        text: Some(
                            "[Earlier conversation turns elided for brevity. The model's previous \
                             tool call is replayed below for context; please continue from there.]"
                                .into(),
                        ),
                        ..Default::default()
                    }],
                },
            );
        }
    }
    Ok(contents)
}

fn role_of(msg: &Value) -> &str {
    msg.get("role").and_then(|v| v.as_str()).unwrap_or("")
}

/// 拆 call_id 里的 thoughtSignature(P1-B 修复 — Gemini 3 多轮 thinking roundtrip)。
/// emit_function_call 用 `~~sig~~` 分隔符编码,这里反向拆。
/// 返 (clean_call_id_without_signature, Option<signature>)。
pub fn decode_tool_call_id_signature(id: &str) -> (String, Option<String>) {
    if let Some((before, after)) = id.split_once("~~sig~~") {
        if !after.is_empty() {
            return (before.to_owned(), Some(after.to_owned()));
        }
    }
    (id.to_owned(), None)
}

fn extract_tool_call(tc: &Value) -> Option<(String, String, Value, Option<String>)> {
    let id = tc.get("id")?.as_str()?.to_owned();
    let func = tc.get("function")?.as_object()?;
    let name = func.get("name")?.as_str()?.to_owned();
    let args_raw = func.get("arguments");
    let args = args_raw
        .and_then(|a| {
            if let Some(s) = a.as_str() {
                serde_json::from_str(s).ok()
            } else {
                Some(a.clone())
            }
        })
        .unwrap_or(Value::Object(Map::new()));
    let (clean_id, sig) = decode_tool_call_id_signature(&id);
    Some((clean_id, name, args, sig))
}

/// P1-D 修复(用户硬性规则:不主动破坏性降级):
/// - string 是 JSON dict → 直接用(不丢)
/// - string 是非 dict JSON(array/number/bool)→ wrap `{"content":"...原 string..."}`
///   仅在 string 形态做 wrap,因为 Codex.app function_call_output.output 永远是
///   stringified JSON,这层 wrap 是把"反序列化失败的 string"当 raw text 给 Gemini,
///   语义跟"传字符串内容"一致,**非 destructive**
/// - object → 直接用
/// - 其他原生类型(array/number/bool)→ BadRequest(Gemini wire 要求 dict;wrap
///   `{"result":...}` 是改 wire shape 影响 model 看到的结构,destructive)
fn parse_tool_response_content(content: Option<&Value>) -> Result<Value, AdapterError> {
    let Some(content) = content else {
        return Ok(Value::Object(Map::new()));
    };
    if let Some(s) = content.as_str() {
        // string 优先尝试 JSON 解析为 dict(Codex 通常发 stringified JSON)
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if v.is_object() {
                return Ok(v);
            }
        }
        // string 不是 dict → wrap "content":"..." 把它当 raw text 传给 Gemini
        // (语义保留,model 看到的就是字符串内容)
        let mut wrapper = Map::new();
        wrapper.insert("content".into(), Value::String(s.to_owned()));
        return Ok(Value::Object(wrapper));
    }
    if content.is_object() {
        return Ok(content.clone());
    }
    Err(AdapterError::BadRequest(format!(
        "function_call_output.output is {} but Gemini functionResponse.response wire requires \
         a dict (object) — silent wrapping {{result: ...}} would change the wire shape model sees. \
         Pass the output as a JSON object, or as a stringified JSON object.",
        match content {
            Value::Array(_) => "array",
            Value::Number(_) => "number",
            Value::Bool(_) => "bool",
            Value::Null => "null",
            _ => "unknown type",
        }
    )))
}

// ───────── content block → Part ─────────

fn convert_content_block_to_part(elem: &Value) -> Option<Part> {
    let obj = elem.as_object()?;
    let block_type = obj.get("type").and_then(|v| v.as_str())?;
    match block_type {
        "text" | "input_text" | "output_text" => {
            obj.get("text").and_then(|v| v.as_str()).map(|t| Part {
                text: Some(t.to_owned()),
                ..Default::default()
            })
        }
        "image_url" | "input_image" => image_url_block_to_part(obj),
        "input_audio" => audio_block_to_part(obj),
        "input_file" => file_block_to_part(obj),
        other => {
            crate::warn_once_drop_tool(&format!("gemini_native:chat_block:{other}"));
            None
        }
    }
}

/// 推断 file URL 的 mime type — 简单按扩展名 + 默认 application/octet-stream。
/// Gemini fileData 必须有 mimeType,缺会 400。
fn infer_mime_from_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".pdf") {
        "application/pdf"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".mp3") {
        "audio/mp3"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else if path.ends_with(".mp4") {
        "video/mp4"
    } else if path.ends_with(".txt") || path.ends_with(".md") {
        "text/plain"
    } else if path.ends_with(".html") || path.ends_with(".htm") {
        "text/html"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}

fn image_url_block_to_part(obj: &Map<String, Value>) -> Option<Part> {
    let url = obj
        .get("image_url")
        .and_then(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.get("url").and_then(|u| u.as_str()).map(String::from))
        })
        .or_else(|| obj.get("url").and_then(|v| v.as_str()).map(String::from))?;

    // base64 data URI → inlineData(本地数据)
    if let Some((mime, data)) = parse_data_uri(&url) {
        return Some(Part {
            inline_data: Some(InlineData {
                mime_type: mime,
                data,
            }),
            ..Default::default()
        });
    }
    // 外网 URL → fileData(让 Gemini 上游 fetch);**不再** 用 [image omitted] 占位 text
    // (那是 destructive 降级,model 完全看不到图)。Gemini fileData 接受公开 https URL。
    let mime = infer_mime_from_url(&url);
    Some(Part {
        file_data: Some(FileData {
            mime_type: mime,
            file_uri: url,
        }),
        ..Default::default()
    })
}

fn file_block_to_part(obj: &Map<String, Value>) -> Option<Part> {
    // input_file 经 normalize_responses_message_content 已转成 chat 格式:
    // {type:"input_file", input_file:{url, filename?, mime_type?}}
    let inner = obj.get("input_file").and_then(|v| v.as_object())?;
    let url = inner.get("url").and_then(|v| v.as_str())?.to_owned();
    if let Some((mime, data)) = parse_data_uri(&url) {
        return Some(Part {
            inline_data: Some(InlineData {
                mime_type: mime,
                data,
            }),
            ..Default::default()
        });
    }
    let mime = inner
        .get("mime_type")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| infer_mime_from_url(&url));
    Some(Part {
        file_data: Some(FileData {
            mime_type: mime,
            file_uri: url,
        }),
        ..Default::default()
    })
}

fn audio_block_to_part(obj: &Map<String, Value>) -> Option<Part> {
    let inner = obj.get("input_audio").and_then(|v| v.as_object())?;
    let data = inner.get("data").and_then(|v| v.as_str())?;
    let format = inner
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("wav");
    let mime = if format.starts_with("audio/") {
        format.to_string()
    } else {
        format!("audio/{format}")
    };
    Some(Part {
        inline_data: Some(InlineData {
            mime_type: mime,
            data: data.to_owned(),
        }),
        ..Default::default()
    })
}

fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let stripped = url.strip_prefix("data:")?;
    let (header, data) = stripped.split_once(",")?;
    if !header.contains("base64") {
        return None;
    }
    let mime = header
        .split(';')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned();
    Some((mime, data.to_owned()))
}

// ───────── tools → Gemini Tool[] (LiteLLM _map_function:539) ─────────

fn convert_tools(tools_arr: &[Value]) -> Vec<Tool> {
    let mut function_decls: Vec<FunctionDeclaration> = Vec::new();
    let mut google_search: Option<Value> = None;
    let mut url_context: Option<Value> = None;
    let mut code_execution: Option<Value> = None;

    for tool in tools_arr {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let tool_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match tool_type {
            "web_search" | "web_search_preview" => {
                google_search = Some(Value::Object(Map::new()));
            }
            "url_context" => {
                url_context = Some(Value::Object(Map::new()));
            }
            "code_execution" => {
                code_execution = Some(Value::Object(Map::new()));
            }
            "function" => {
                if let Some(decl) = function_object_to_declaration(obj.get("function")) {
                    function_decls.push(decl);
                }
            }
            "" => {
                if let Some(decl) = function_object_to_declaration(Some(tool)) {
                    function_decls.push(decl);
                }
            }
            "google_search" | "googleSearch" => {
                google_search = Some(
                    obj.get("google_search")
                        .or_else(|| obj.get("googleSearch"))
                        .cloned()
                        .unwrap_or(Value::Object(Map::new())),
                );
            }
            _ => {}
        }
    }

    let mut out: Vec<Tool> = Vec::new();
    if !function_decls.is_empty() {
        out.push(Tool {
            function_declarations: Some(function_decls),
            ..Default::default()
        });
    }
    if google_search.is_some() {
        out.push(Tool {
            google_search,
            ..Default::default()
        });
    }
    if url_context.is_some() {
        out.push(Tool {
            url_context,
            ..Default::default()
        });
    }
    if code_execution.is_some() {
        out.push(Tool {
            code_execution,
            ..Default::default()
        });
    }
    out
}

fn function_object_to_declaration(func: Option<&Value>) -> Option<FunctionDeclaration> {
    let func = func?.as_object()?;
    let name = func.get("name").and_then(|v| v.as_str())?.to_owned();
    let description = func
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let parameters = func.get("parameters").cloned().map(sanitize_schema);
    Some(FunctionDeclaration {
        name,
        description,
        parameters,
        response: None,
    })
}

/// Schema sanitize 增强版(LiteLLM `common_utils.py` `_build_vertex_schema` 主流程):
/// - **P1-C 修复**:`$ref` / `$defs` **inline 展开**(LiteLLM `unpack_defs`
///   思路 — 旧实现直接 remove $ref/$defs 导致引用断 + schema 不完整)
/// - 剥 OpenAPI 高级 keyword(`additionalProperties` / `strict` / `$schema` / `$id`)
/// - enum 内空字符串 → null(LiteLLM `_fix_enum_empty_strings:466`)
/// - anyOf 单一 null branch → 当作 nullable + 提取另一 branch(LiteLLM
///   `convert_anyof_null_to_nullable:745`)
/// - object 类型未指定 properties 时补 `properties:{}`(Gemini 强制要求)
pub fn sanitize_schema(mut schema: Value) -> Value {
    // 先抽 $defs 出来作为 lookup table,然后递归展开所有 $ref
    // (P1-C 修复:不再 silent remove $ref → 引用断)
    let defs = schema
        .as_object()
        .and_then(|o| o.get("$defs"))
        .cloned()
        .or_else(|| {
            schema
                .as_object()
                .and_then(|o| o.get("definitions"))
                .cloned()
        });
    if let Some(defs_value) = defs {
        if let Value::Object(defs_map) = defs_value {
            inline_refs_inplace(&mut schema, &defs_map, 0);
        }
    }
    sanitize_schema_inplace(&mut schema, 0);
    schema
}

/// 递归 inline 展开 $ref(LiteLLM unpack_defs 简化实现)。
/// 仅支持 `#/$defs/<name>` 和 `#/definitions/<name>` 的 local ref(JSON Schema
/// 标准用法,Codex.app + OpenAI tool schema 都用这两种)。
/// External ref(`http://...`)+ recursive ref 跳过(防无限递归)。
fn inline_refs_inplace(v: &mut Value, defs: &Map<String, Value>, depth: usize) {
    if depth > 32 {
        return; // 防 self-recursive ref 死循环
    }
    if let Value::Object(obj) = v {
        // 当前节点是 $ref → 替换为 defs 里对应 schema 的 clone
        if let Some(ref_val) = obj.get("$ref").and_then(|r| r.as_str()) {
            let key = ref_val
                .trim_start_matches("#/$defs/")
                .trim_start_matches("#/definitions/");
            if let Some(resolved) = defs.get(key).cloned() {
                // 把 resolved 整个替换当前节点(merge 其他 keys 优先用 resolved 的)
                let merged = if let Value::Object(mut resolved_obj) = resolved {
                    // 保留当前节点 $ref 之外的 sibling keys(JSON Schema spec:$ref
                    // 跟其他 keyword 共存时实施 merge)
                    for (k, val) in obj.iter() {
                        if k != "$ref" {
                            resolved_obj.entry(k.clone()).or_insert_with(|| val.clone());
                        }
                    }
                    Value::Object(resolved_obj)
                } else {
                    resolved
                };
                *v = merged;
                // 展开后的节点本身也可能含 $ref,继续递归
                inline_refs_inplace(v, defs, depth + 1);
                return;
            }
            // ref 找不到 → 留原样(后面 sanitize_schema_inplace 会 remove $ref)
        }
        for (_k, vv) in obj.iter_mut() {
            inline_refs_inplace(vv, defs, depth + 1);
        }
    } else if let Value::Array(arr) = v {
        for item in arr.iter_mut() {
            inline_refs_inplace(item, defs, depth + 1);
        }
    }
}

fn sanitize_schema_inplace(v: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    match v {
        Value::Object(obj) => {
            obj.remove("additionalProperties");
            obj.remove("strict");
            obj.remove("$schema");
            obj.remove("$id");
            obj.remove("$ref");
            obj.remove("$defs");

            // enum 空字符串 → null
            if let Some(Value::Array(enum_arr)) = obj.get_mut("enum") {
                for item in enum_arr.iter_mut() {
                    if matches!(item, Value::String(s) if s.is_empty()) {
                        *item = Value::Null;
                    }
                }
            }
            // **JSON Schema array type → Gemini schema(P2-A:不丢 union 信息)**:
            // JSON Schema 允许 `"type": ["string","number","null"]` 表示 union type,
            // Gemini protobuf 要求 type 是单 string("Proto field is not repeating")。
            // 转换规则(不丢信息):
            // - ["X","null"] → {type:"X", nullable:true}
            // - ["X"] → {type:"X"}
            // - ["X","Y", ...](多 non-null)→ {anyOf:[{type:"X"},{type:"Y"},...], nullable?}
            //   Gemini Schema 文档支持 anyOf,union 信息保留
            // - ["null"] 仅 → {nullable:true}(无 type)
            if let Some(Value::Array(types)) = obj.get("type").cloned().as_ref() {
                let mut has_null = false;
                let mut non_null_types: Vec<String> = Vec::new();
                for t in types {
                    if let Some(s) = t.as_str() {
                        if s == "null" {
                            has_null = true;
                        } else if !non_null_types.contains(&s.to_owned()) {
                            non_null_types.push(s.to_owned());
                        }
                    }
                }
                match non_null_types.len() {
                    0 => {
                        obj.remove("type");
                        if has_null {
                            obj.insert("nullable".into(), Value::Bool(true));
                        }
                    }
                    1 => {
                        obj.insert("type".into(), Value::String(non_null_types[0].clone()));
                        if has_null {
                            obj.insert("nullable".into(), Value::Bool(true));
                        }
                    }
                    _ => {
                        // 多 non-null type → anyOf 表达 union(Gemini 支持)
                        obj.remove("type");
                        let any_of: Vec<Value> =
                            non_null_types.iter().map(|t| json!({"type": t})).collect();
                        obj.insert("anyOf".into(), Value::Array(any_of));
                        if has_null {
                            obj.insert("nullable".into(), Value::Bool(true));
                        }
                    }
                }
            }
            // P2-A 修复(用户硬性规则:不主动破坏性降级):
            // Gemini Schema 文档(`vertex-ai/docs/reference/rest/v1beta1/Schema`)
            // **明确支持 anyOf**。旧实现把多 non-null branch silent 砍到 first,
            // 是 destructive(union type 信息丢失)。改成:
            // - single non-null + null → 转 nullable + merge non-null 字段(更地道,
            //   Gemini nullable 比 anyOf null 处理更优)
            // - 其他形态 anyOf(多 non-null / pure null)→ **保留 anyOf 字段不剥**,
            //   让 Gemini 自己 validate;若拒就 BadRequest 反馈给用户(user-visible)
            if let Some(Value::Array(any_of)) = obj.get("anyOf").cloned().as_ref() {
                let non_null: Vec<&Value> = any_of
                    .iter()
                    .filter(|b| {
                        b.as_object()
                            .and_then(|o| o.get("type"))
                            .and_then(|t| t.as_str())
                            != Some("null")
                    })
                    .collect();
                let has_null_branch = any_of.len() != non_null.len();
                if non_null.len() == 1 && has_null_branch {
                    // 经典 nullable 场景 — 单 non-null branch + null branch → 提到外层 + nullable=true
                    if let Some(Value::Object(target)) =
                        non_null.first().map(|v| (*v).clone()).as_mut()
                    {
                        for (k, vv) in target.iter() {
                            // entry.or_insert 不覆盖 parent 已有字段(防丢 description 等)
                            obj.entry(k.clone()).or_insert_with(|| vv.clone());
                        }
                    }
                    obj.insert("nullable".into(), Value::Bool(true));
                    obj.remove("anyOf");
                }
                // 其他形态(多 non-null / pure null)— anyOf 字段**保留**不剥,
                // Gemini 自己 validate(它文档支持 anyOf union type)
            }
            // object 类型 properties 默认补空对象
            if obj.get("type").and_then(|v| v.as_str()) == Some("object")
                && !obj.contains_key("properties")
            {
                obj.insert("properties".into(), Value::Object(Map::new()));
            }
            // 递归
            for (_k, vv) in obj.iter_mut() {
                sanitize_schema_inplace(vv, depth + 1);
            }
        }
        Value::Array(arr) => {
            for vv in arr.iter_mut() {
                sanitize_schema_inplace(vv, depth + 1);
            }
        }
        _ => {}
    }
}

// ───────── tool_choice → ToolConfig (LiteLLM map_tool_choice_values:333) ─────────

fn convert_tool_choice(tc: &Value) -> Option<ToolConfig> {
    if let Some(s) = tc.as_str() {
        let mode = match s {
            "none" => "NONE",
            "required" => "ANY",
            "auto" => "AUTO",
            _ => return None,
        };
        return Some(ToolConfig {
            function_calling_config: Some(FunctionCallingConfig {
                mode: mode.into(),
                allowed_function_names: None,
            }),
            include_server_side_tool_invocations: None,
            retrieval_config: None,
        });
    }
    if let Some(obj) = tc.as_object() {
        let name = obj
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())?;
        return Some(ToolConfig {
            function_calling_config: Some(FunctionCallingConfig {
                mode: "ANY".into(),
                allowed_function_names: Some(vec![name.to_owned()]),
            }),
            include_server_side_tool_invocations: None,
            retrieval_config: None,
        });
    }
    None
}

// ───────── generation_config (LiteLLM map_openai_params:1073) ─────────

fn build_generation_config(body: &Map<String, Value>, model: &str) -> Option<GenerationConfig> {
    let mut gc = GenerationConfig::default();
    let mut any_set = false;

    if let Some(t) = body.get("temperature").and_then(|v| v.as_f64()) {
        gc.temperature = Some(t);
        any_set = true;
    }
    if let Some(t) = body.get("top_p").and_then(|v| v.as_f64()) {
        gc.top_p = Some(t);
        any_set = true;
    }
    if let Some(t) = body.get("top_k").and_then(|v| v.as_i64()) {
        gc.top_k = Some(t);
        any_set = true;
    }
    if let Some(t) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(|v| v.as_i64())
    {
        gc.max_output_tokens = Some(t);
        any_set = true;
    }
    if let Some(stop) = body.get("stop") {
        let seqs: Vec<String> = match stop {
            Value::String(s) => vec![s.clone()],
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => Vec::new(),
        };
        if !seqs.is_empty() {
            gc.stop_sequences = Some(seqs);
            any_set = true;
        }
    }
    if let Some(seed) = body.get("seed").and_then(|v| v.as_i64()) {
        gc.seed = Some(seed);
        any_set = true;
    }
    if let Some(n) = body.get("n").and_then(|v| v.as_i64()) {
        gc.candidate_count = Some(n);
        any_set = true;
    }

    if let Some(rf) = body.get("response_format").and_then(|v| v.as_object()) {
        let rf_type = rf.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match rf_type {
            "json_object" => {
                gc.response_mime_type = Some("application/json".into());
                any_set = true;
            }
            "json_schema" => {
                gc.response_mime_type = Some("application/json".into());
                if let Some(schema) = rf
                    .get("json_schema")
                    .and_then(|v| v.get("schema"))
                    .or_else(|| rf.get("schema"))
                {
                    gc.response_schema = Some(sanitize_schema(schema.clone()));
                }
                any_set = true;
            }
            _ => {}
        }
    }

    // reasoning_effort → thinkingConfig:Gemini 3+ 用 thinkingLevel,Gemini 2.x 用 thinkingBudget
    // (LiteLLM `vertex_and_google_ai_studio_gemini.py:822 _map_reasoning_effort_to_thinking_budget`
    // + `:873 _map_reasoning_effort_to_thinking_level`)
    if let Some(effort) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
        let normalized = effort.to_ascii_lowercase();
        let is_g3 = is_gemini_3_or_newer(model);
        let tc = if is_g3 {
            // Gemini 3+ thinking_level:none/low/medium/high(LiteLLM 实证 string-based)
            let level = match normalized.as_str() {
                "none" | "off" | "disabled" => Some("off"),
                "low" | "minimal" => Some("low"),
                "medium" => Some("medium"),
                "high" | "max" | "maximum" => Some("high"),
                _ => None,
            };
            level.map(|l| ThinkingConfig {
                thinking_level: Some(l.into()),
                include_thoughts: Some(true),
                ..Default::default()
            })
        } else {
            // Gemini 2.x thinking_budget:none → -1, low → 1024, medium → 8192, high → 16384
            let budget = match normalized.as_str() {
                "none" | "off" | "disabled" => Some(-1),
                "low" | "minimal" => Some(1024),
                "medium" => Some(8192),
                "high" | "max" | "maximum" => Some(16384),
                _ => None,
            };
            budget.map(|b| ThinkingConfig {
                thinking_budget: Some(b),
                include_thoughts: Some(true),
                ..Default::default()
            })
        };
        if let Some(tc) = tc {
            gc.thinking_config = Some(tc);
            any_set = true;
        }
    }

    // Gemini 3+ 默认 temperature=1.0(LiteLLM `vertex_and_google_ai_studio_gemini.py:1215`
    // 实证:< 1.0 触发 infinite loop / degraded reasoning;若用户没指定,补 1.0)
    if is_gemini_3_or_newer(model) && gc.temperature.is_none() {
        gc.temperature = Some(1.0);
        any_set = true;
    }

    if any_set {
        Some(gc)
    } else {
        None
    }
}

/// LiteLLM `vertex_and_google_ai_studio_gemini.py` `_is_gemini_3_or_newer` 等价。
/// 简化版:检测 model 名是否含 "gemini-3" 或 "gemini-4"(更高版本时再扩)。
pub fn is_gemini_3_or_newer(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("gemini-3") || m.contains("gemini-4")
}

// ───────── extra_body 顶层合并 ─────────

fn apply_extra_body_overrides(
    req: &mut RequestBody,
    body: &Map<String, Value>,
) -> Result<(), AdapterError> {
    let Some(extra) = body.get("extra_body").and_then(|v| v.as_object()) else {
        return Ok(());
    };
    let req_value = serde_json::to_value(&*req).map_err(|e| {
        AdapterError::Internal(format!(
            "failed to serialize RequestBody for extra_body merge: {e}"
        ))
    })?;
    let mut merged = req_value;
    let merged_obj = merged
        .as_object_mut()
        .ok_or_else(|| AdapterError::Internal("RequestBody serialization not an object".into()))?;
    for (k, v) in extra {
        match (merged_obj.get_mut(k), v) {
            (Some(Value::Object(existing)), Value::Object(new_obj)) => {
                for (kk, vv) in new_obj {
                    existing.insert(kk.clone(), vv.clone());
                }
            }
            _ => {
                merged_obj.insert(k.clone(), v.clone());
            }
        }
    }
    // P2-A 修复(用户硬性规则:不主动破坏性降级)— extra_body 解析失败前是
    // tracing::warn + silent drop,用户 override 被吞,提"我的 extra_body 没生效"
    // 几乎找不到原因。改 BadRequest 让客户端立刻看到具体哪个字段类型错。
    *req = serde_json::from_value::<RequestBody>(merged).map_err(|e| {
        AdapterError::BadRequest(format!(
            "extra_body merge produced an invalid Gemini RequestBody (field type / path \
             mismatch): {e}. Check your extra_body schema against Gemini generateContent docs \
             (https://ai.google.dev/api/generate-content)."
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn dummy_provider() -> Provider {
        Provider {
            id: "google-ai-studio".into(),
            name: "Google AI Studio".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            auth_scheme: "google_api_key".into(),
            api_format: "gemini_native".into(),
            api_key: "fake".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: true,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    // ───── upstream URL ─────

    #[test]
    fn upstream_path_gemini_3_uses_v1alpha() {
        let p = build_gemini_upstream_path("gemini-3.1-pro-preview", true, "https://x.com");
        assert!(
            p.starts_with("/v1alpha/"),
            "Gemini 3+ 必须用 v1alpha,实际:{p}"
        );
    }

    #[test]
    fn upstream_path_gemini_2_uses_v1beta() {
        let p = build_gemini_upstream_path("gemini-2.0-flash", true, "https://x.com");
        assert!(p.starts_with("/v1beta/"), "Gemini 2.x 用 v1beta,实际:{p}");
    }

    #[test]
    fn upstream_path_respects_baseurl_with_version() {
        // 用户在 baseUrl 指定了 /v1beta → 不重复加版本前缀
        let p = build_gemini_upstream_path("gemini-3.1-pro", true, "https://x.com/v1beta");
        assert!(!p.contains("v1alpha"), "用户已指定版本不能覆盖,实际:{p}");
        assert_eq!(p, "/models/gemini-3.1-pro:streamGenerateContent?alt=sse");
    }

    #[test]
    fn upstream_path_streaming_has_alt_sse() {
        let p = build_gemini_upstream_path("gemini-3.1-flash-lite", true, "https://x.com");
        assert!(p.ends_with(":streamGenerateContent?alt=sse"));
    }

    #[test]
    fn upstream_path_non_stream() {
        let p = build_gemini_upstream_path("gemini-2.0-flash", false, "https://x.com");
        assert!(p.ends_with(":generateContent"));
        assert!(!p.contains("alt=sse"));
    }

    // ───── Responses input → chat normalization ─────

    #[test]
    fn responses_message_input_normalizes_to_chat_messages() {
        let body = serde_json::json!({
            "model": "gemini-3.1-flash-lite",
            "instructions": "You are helpful.",
            "input": [
                {"type":"message","role":"user","content":"hi"}
            ]
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn responses_function_call_becomes_assistant_with_tool_calls() {
        let body = serde_json::json!({
            "input": [
                {"type":"message","role":"user","content":"x"},
                {"type":"function_call","call_id":"c1","name":"search","arguments":"{\"q\":\"a\"}"},
                {"type":"function_call_output","call_id":"c1","output":"sunny"}
            ]
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // user / assistant(tool_calls) / tool
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(assistant["tool_calls"].is_array());
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["id"], "c1");
        assert_eq!(tcs[0]["function"]["name"], "search");
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(tool["content"], "sunny");
    }

    #[test]
    fn responses_web_search_tool_preserved_in_chat_normalized() {
        // 关键回归:web_search 必须在 Responses → 归一化 chat 阶段保留(不能被 drop)
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}],
            "tools":[{"type":"web_search","external_web_access":true}]
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "web_search");
    }

    #[test]
    fn responses_function_tool_unwraps_to_chat_function_wrapper() {
        // Responses: {type:function, name, parameters} → chat: {type:function, function:{...}}
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}],
            "tools":[{
                "type":"function","name":"get_weather","description":"...",
                "parameters":{"type":"object","properties":{"city":{"type":"string"}}}
            }]
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        let t = &chat["tools"].as_array().unwrap()[0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["function"]["name"], "get_weather");
        assert!(t["function"]["parameters"]["properties"]["city"].is_object());
    }

    #[test]
    fn responses_text_format_json_schema_becomes_chat_response_format() {
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}],
            "text":{"format":{"type":"json_schema","name":"r","schema":{"type":"object"}}}
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        assert_eq!(chat["response_format"]["type"], "json_schema");
        assert_eq!(chat["response_format"]["json_schema"]["name"], "r");
    }

    #[test]
    fn responses_reasoning_effort_passed_through() {
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}],
            "reasoning":{"effort":"high"}
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        assert_eq!(chat["reasoning_effort"], "high");
    }

    #[test]
    fn responses_max_output_tokens_renamed_to_max_tokens() {
        let body = serde_json::json!({
            "input":[{"type":"message","role":"user","content":"x"}],
            "max_output_tokens":1024
        });
        let chat = responses_body_to_normalized_chat(&body).unwrap();
        assert_eq!(chat["max_tokens"], 1024);
    }

    // ───── chat → Gemini full pipeline ─────

    #[test]
    fn responses_to_gemini_full_pipeline_simple_user() {
        let body = serde_json::json!({
            "model":"gemini-3.1-flash-lite",
            "instructions":"sys",
            "input":[{"type":"message","role":"user","content":"hi"}]
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let si = req.system_instruction.unwrap();
        assert_eq!(si.parts[0].text.as_deref(), Some("sys"));
        assert_eq!(req.contents.len(), 1);
        assert_eq!(req.contents[0].role, "user");
        assert_eq!(req.contents[0].parts[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn responses_to_gemini_with_web_search_emits_google_search_tool() {
        // 关键端到端回归:Codex.app /responses + tools=[web_search] →
        // Gemini RequestBody 必须含 tools=[{googleSearch:{}}]
        let body = serde_json::json!({
            "model":"gemini-3.1-pro-preview",
            "input":[{"type":"message","role":"user","content":"今天纽约天气?"}],
            "tools":[{"type":"web_search","external_web_access":true}]
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let tools = req.tools.expect("tools 应存在");
        assert!(
            tools.iter().any(|t| t.google_search.is_some()),
            "必须含 googleSearch tool;实际:{}",
            serde_json::to_string(&tools).unwrap()
        );
    }

    #[test]
    fn gemini_3_default_temperature_is_1() {
        let body = serde_json::json!({
            "model":"gemini-3.1-pro-preview",
            "input":[{"type":"message","role":"user","content":"hi"}]
            // 没指定 temperature
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let gc = req.generation_config.unwrap();
        assert_eq!(gc.temperature, Some(1.0), "Gemini 3+ 默认 temp=1.0");
    }

    #[test]
    fn gemini_2_no_default_temperature() {
        let body = serde_json::json!({
            "model":"gemini-2.0-flash",
            "input":[{"type":"message","role":"user","content":"hi"}]
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        // Gemini 2.x 没默认 temp,应 None / generation_config 整个 None
        let temp = req.generation_config.and_then(|g| g.temperature);
        assert!(temp.is_none(), "Gemini 2.x 不补默认 temp");
    }

    #[test]
    fn gemini_3_reasoning_effort_uses_thinking_level() {
        let body = serde_json::json!({
            "model":"gemini-3.1-pro-preview",
            "input":[{"type":"message","role":"user","content":"x"}],
            "reasoning":{"effort":"high"}
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let tc = req.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_level.as_deref(), Some("high"));
        assert!(
            tc.thinking_budget.is_none(),
            "Gemini 3+ 不写 budget,只写 level"
        );
    }

    #[test]
    fn gemini_2_reasoning_effort_uses_thinking_budget() {
        let body = serde_json::json!({
            "model":"gemini-2.5-flash",
            "input":[{"type":"message","role":"user","content":"x"}],
            "reasoning":{"effort":"high"}
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let tc = req.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, Some(16384));
        assert!(
            tc.thinking_level.is_none(),
            "Gemini 2.x 用 budget 不用 level"
        );
    }

    #[test]
    fn schema_sanitize_strips_additional_properties_and_strict() {
        let schema = serde_json::json!({
            "type":"object",
            "additionalProperties":false,
            "strict":true,
            "$schema":"http://example.com",
            "properties":{"x":{"type":"string","strict":true}}
        });
        let cleaned = sanitize_schema(schema);
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(cleaned.get("strict").is_none());
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned["properties"]["x"].get("strict").is_none(), "递归剥");
    }

    #[test]
    fn schema_sanitize_enum_empty_string_to_null() {
        let schema = serde_json::json!({
            "enum":["a","","b"]
        });
        let cleaned = sanitize_schema(schema);
        let arr = cleaned["enum"].as_array().unwrap();
        assert_eq!(arr[0], "a");
        assert!(arr[1].is_null(), "空字符串必须转 null");
        assert_eq!(arr[2], "b");
    }

    #[test]
    fn schema_sanitize_array_type_becomes_single_type_plus_nullable() {
        // 实测 2026-05-10:Codex.app text.format.json_schema 含 `"type":["string","null"]`
        // → Gemini 拒 "Proto field is not repeating, cannot start list"。修复后:
        // → `{"type":"string","nullable":true}`
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "value": {"type": ["string", "null"]},
                "count": {"type": ["number", "null"], "description": "optional count"}
            }
        });
        let cleaned = sanitize_schema(schema);
        assert_eq!(cleaned["properties"]["value"]["type"], "string");
        assert_eq!(cleaned["properties"]["value"]["nullable"], true);
        assert_eq!(cleaned["properties"]["count"]["type"], "number");
        assert_eq!(cleaned["properties"]["count"]["nullable"], true);
        // 描述等其他字段保留
        assert_eq!(
            cleaned["properties"]["count"]["description"],
            "optional count"
        );
        // 非 array type 不动
        assert_eq!(cleaned["properties"]["name"]["type"], "string");
        assert!(cleaned["properties"]["name"].get("nullable").is_none());
    }

    #[test]
    fn function_decls_with_json_schema_transforms_to_system_instruction_soft_constraint() {
        // 实测 2026-05-10:Gemini "Function calling with a response mime type:
        // 'application/json' is unsupported" — function declarations 跟
        // responseMimeType/responseSchema 不能共存。
        //
        // **不主动破坏性降级**(memory rule):wire 上必须 drop(Gemini 拒绝),
        // 但语义不丢 — schema 拼成 systemInstruction text 作软约束。
        let body = serde_json::json!({
            "model": "gemini-2.5-flash",
            "messages": [{"role":"user","content":"x"}],
            "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "response_format": {
                "type":"json_schema",
                "json_schema":{"schema":{"type":"object","properties":{"answer":{"type":"string"}}}}
            }
        });
        let req = chat_normalized_to_gemini_request(&body, &dummy_provider()).unwrap();
        // function declarations 保留
        assert!(
            req.tools
                .as_ref()
                .unwrap()
                .iter()
                .any(|t| t.function_declarations.is_some()),
            "functionDeclarations 必须保留"
        );
        // generation_config wire 字段被剥(Gemini 400 防御)
        let gc = req.generation_config.as_ref().unwrap();
        assert!(
            gc.response_mime_type.is_none() && gc.response_schema.is_none(),
            "wire 上必须 drop responseMimeType/responseSchema 防 Gemini 400,实际 gc:{gc:?}"
        );
        // **关键**:schema 语义必须以 systemInstruction text 形式传达(不丢用户语义)
        let si = req
            .system_instruction
            .expect("systemInstruction 必须含软约束 text");
        let combined: String = si
            .parts
            .iter()
            .filter_map(|p| p.text.as_ref())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("MUST be a valid JSON object")
                && combined.contains("matching this schema"),
            "systemInstruction 必须含 JSON schema 软约束 prompt;实际:{combined}"
        );
        assert!(
            combined.contains("answer") && combined.contains("type"),
            "schema JSON 内容必须 inline 在软约束 text;实际:{combined}"
        );
    }

    #[test]
    fn gemini_3_plus_function_decls_with_google_search_enables_server_side_tool_invocations() {
        // 实测 2026-05-10:Gemini "Built-in tools ({google_search}) and Function Calling
        // cannot be combined in the same request" — Gemini 3+ 通过开
        // toolConfig.includeServerSideToolInvocations=true 让两者共存(无破坏性 drop)。
        let body = serde_json::json!({
            "model": "gemini-3.1-pro-preview",
            "messages": [{"role":"user","content":"x"}],
            "tools": [
                {"type":"function","function":{"name":"f","parameters":{"type":"object"}}},
                {"type":"web_search"}
            ]
        });
        let req = chat_normalized_to_gemini_request(&body, &dummy_provider()).unwrap();
        // 两者都保留(function declarations + googleSearch)
        let tools = req.tools.expect("tools 必须存在");
        assert!(
            tools.iter().any(|t| t.function_declarations.is_some()),
            "Gemini 3+ functionDeclarations 必须保留"
        );
        assert!(
            tools.iter().any(|t| t.google_search.is_some()),
            "Gemini 3+ googleSearch 必须保留(toolConfig.includeServerSideToolInvocations 让共存)"
        );
        // toolConfig.includeServerSideToolInvocations=true
        let tc = req.tool_config.expect("toolConfig 必须存在");
        assert_eq!(
            tc.include_server_side_tool_invocations,
            Some(true),
            "Gemini 3+ 必须开 includeServerSideToolInvocations 让 googleSearch + function 共存"
        );
    }

    #[test]
    fn gemini_2_function_decls_with_google_search_drops_search_and_adds_soft_constraint() {
        // Gemini 2.x 不支持两者共存,wire 必须 drop googleSearch + 软约束 system_instruction
        // 提示 model "用户期望联网但不可用,需要时告知用户"。Function calling 完全保留。
        let body = serde_json::json!({
            "model": "gemini-2.5-flash",
            "messages": [{"role":"user","content":"x"}],
            "tools": [
                {"type":"function","function":{"name":"f","parameters":{"type":"object"}}},
                {"type":"web_search"}
            ]
        });
        let req = chat_normalized_to_gemini_request(&body, &dummy_provider()).unwrap();
        let tools = req.tools.expect("function declarations 必须保留");
        // googleSearch 被 drop (Gemini 2.x wire 不允许共存)
        assert!(
            !tools.iter().any(|t| t.google_search.is_some()),
            "Gemini 2.x wire 必须 drop googleSearch(防 400)"
        );
        // function declarations 保留
        assert!(
            tools.iter().any(|t| t.function_declarations.is_some()),
            "Gemini 2.x functionDeclarations 必须保留(Codex 核心)"
        );
        // system_instruction 含软约束告知 model 限制
        let si = req
            .system_instruction
            .expect("systemInstruction 必须含软约束");
        let combined: String = si
            .parts
            .iter()
            .filter_map(|p| p.text.as_ref())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("web_search") && combined.contains("disabled"),
            "软约束必须告诉 model googleSearch 被禁;实际:{combined}"
        );
    }

    #[test]
    fn schema_sanitize_anyof_null_becomes_nullable() {
        // {anyOf:[{type:"string"},{type:"null"}]} → {type:"string",nullable:true}
        let schema = serde_json::json!({
            "anyOf":[{"type":"string","format":"email"},{"type":"null"}]
        });
        let cleaned = sanitize_schema(schema);
        assert_eq!(cleaned["type"], "string");
        assert_eq!(cleaned["nullable"], true);
        assert_eq!(cleaned["format"], "email");
        assert!(cleaned.get("anyOf").is_none());
    }

    #[test]
    fn schema_sanitize_object_without_properties_gets_empty_object() {
        let schema = serde_json::json!({"type":"object"});
        let cleaned = sanitize_schema(schema);
        assert_eq!(cleaned["properties"], serde_json::json!({}));
    }

    #[test]
    fn assistant_reasoning_content_becomes_thought_part() {
        let body = serde_json::json!({
            "input":[
                {"type":"message","role":"user","content":"x"},
                {"type":"reasoning","summary":["I should think carefully"]},
                {"type":"message","role":"assistant","content":"my answer"}
            ]
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        // 找 model role 的 content,part 里要含 thought=true text=I should think carefully
        let model_content = req
            .contents
            .iter()
            .find(|c| c.role == "model")
            .expect("应有 model content");
        let has_thought_part = model_content.parts.iter().any(|p| {
            p.thought == Some(true) && p.text.as_deref() == Some("I should think carefully")
        });
        assert!(
            has_thought_part,
            "reasoning summary 必须转 thought=true part"
        );
    }

    #[test]
    fn web_search_full_e2e_with_function_tool_coexisting() {
        // Codex.app 实际 case:同 turn 既有 function tool 又有 web_search,Gemini
        // 必须输出两个 Tool 对象(function declarations 一个,googleSearch 一个)
        let body = serde_json::json!({
            "model":"gemini-3.1-pro-preview",
            "input":[{"type":"message","role":"user","content":"x"}],
            "tools":[
                {"type":"function","name":"calc","parameters":{"type":"object"}},
                {"type":"web_search"}
            ]
        });
        let req = responses_body_to_gemini_request(&body, &dummy_provider()).unwrap();
        let tools = req.tools.unwrap();
        let has_decls = tools.iter().any(|t| t.function_declarations.is_some());
        let has_search = tools.iter().any(|t| t.google_search.is_some());
        assert!(
            has_decls && has_search,
            "function declarations + googleSearch 必须共存"
        );
        // 验证 functionDeclarations 内的 schema 已 sanitize
        let decls = tools
            .iter()
            .find_map(|t| t.function_declarations.clone())
            .unwrap();
        assert_eq!(decls[0].name, "calc");
    }
}
