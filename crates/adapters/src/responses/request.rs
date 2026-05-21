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

use super::session::ResponseSessionCache;
use crate::core::input::{
    merge_messages_with_previous_response, response_id_for_session, MergeResult,
};
use crate::types::{AdapterError, ResponseSessionPlan};

#[derive(Debug, Clone)]
pub struct ResponsesBodyConversion {
    pub body: Value,
    pub response_session: ResponseSessionPlan,
    /// `true` 表示 `previous_response_id` cache miss 后降级为仅本轮 messages，
    /// 历史已丢失。调用方可据此在响应 header 中注入信号。
    pub history_lost: bool,
}

const TOOL_OUTPUT_INLINE_MAX_CHARS: usize = 4_000;
const TOOL_OUTPUT_HEAD_CHARS: usize = 1_200;
const TOOL_OUTPUT_TAIL_CHARS: usize = 1_200;
const TOOL_OUTPUT_VISIBLE_MAX_CHARS: usize = 5_000;

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
    let merge_result = build_messages_from_input(input, session_cache)?;
    let history_lost = merge_result.history_lost;
    let mut messages = merge_result.messages;
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
        // Stage 3 note: Additional input_image stripping from raw Responses input
        // (for Computer Use base64) can be added here once we have &mut access to body.
        // Current defense relies on strip_image_blocks_in_place after input→message conversion.
    } else {
        // 含 image_url 但无 text part 时补一个空格 text part — MiMo 多模态
        // 接口强制要求(否则 400 "Param Incorrect: text is not set"),
        // 对其他 supports_vision provider 无副作用,统一处理。
        ensure_text_part_when_image_present(&mut messages);
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

    // tools(function / custom 直接处理,namespace 递归展平,web_search /
    // web_search_preview per-provider 适配上游真支持的形态,其余 Responses
    // 专属类型 drop + warn_once)
    if let Some(Value::Array(tools)) = body.get("tools") {
        // Stage 2: Filter out computer_use_preview for non-vision models early
        let filtered_tools: Vec<&Value> =
            if !provider_supports_vision(provider, body_model.as_deref()) {
                let had_computer_use = tools.iter().any(|t| {
                    t.get("type").and_then(|v| v.as_str()) == Some("computer_use_preview")
                });

                if had_computer_use {
                    tracing::warn!(
                        target: "codex_app_transfer::computer_use",
                        provider = ?provider.map(|p| p.id.as_str()),
                        model = body_model,
                        "Codex CLI attempted to use computer_use_preview with a non-vision model. \
                         The tool has been dropped to prevent sending expensive base64 screenshots."
                    );
                }

                tools
                    .iter()
                    .filter(|t| {
                        t.get("type").and_then(|v| v.as_str()) != Some("computer_use_preview")
                    })
                    .collect()
            } else {
                tools.iter().collect()
            };

        let chat_tools: Vec<Value> = filtered_tools
            .iter()
            .flat_map(|t| convert_responses_tool_to_chat_tool(t, provider))
            .collect();
        if !chat_tools.is_empty() {
            // **Kimi `$web_search` 强制 thinking disabled**:Kimi 官方文档
            // (`platform.kimi.ai/docs/guide/use-web-search`)明确写
            // "When using `$web_search` function, you must disable the thinking
            // ability of the model"。OpenAI SDK 的 `extra_body.thinking.type=
            // "disabled"` 在 wire 上等价于 request body 顶级 `thinking:
            // {type:"disabled"}` 字段。如果 outbound tools 含 Kimi 内置
            // `$web_search`,代理在这里强制注入(用户启用 web_search 时模型
            // thinking 能力被禁用是 Kimi API 限制,UI 后续会加提示)。
            if contains_kimi_web_search_tool(&chat_tools) {
                result.insert("thinking".into(), serde_json::json!({"type": "disabled"}));
            }
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
        history_lost,
    })
}

fn build_messages_from_input(
    body: &Value,
    session_cache: Option<&ResponseSessionCache>,
) -> Result<MergeResult, AdapterError> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(msg) = body
        .get("instructions")
        .and_then(build_instructions_message)
    {
        messages.push(msg);
    }

    // 紧跟 Codex CLI 自带 instructions 之后注入 apply_patch chat-path 指引
    // (仅当本 turn 真正注册了 apply_patch 工具 **且** 本轮是 first turn 时)。
    // 位置选择:Codex 系统指令之后,user input 之前 — 既不污染 Codex 原指令,
    // 又确保模型在读完工具列表准备调 apply_patch 时已经见过 chat-path 限制。
    //
    // **仅 first turn 注入**(Devin pre-merge review BUG 修复):带
    // `previous_response_id` 的后续 turn,`merge_messages_with_previous_response`
    // 把 cached history 拼到 current_messages 前面,history 已经包含上一轮注入的
    // guidance(session_cache 保存 merged messages)。如果继续注入,每 turn 都会
    // 加一份 ~2KB guidance,N 轮后 N 份,token 浪费 + 长 apply_patch 工作流
    // (5-10 turn)上下文被挤出。merge 阶段只去重 `messages[0]` instructions,
    // 不去重 guidance(它在 index 1),所以必须 caller 这里做 turn-gating。
    //
    // 边界:如果 first turn 没注册 apply_patch、中段 turn 才首次注册,会 miss
    // 注入 — 实测罕见(Codex Desktop 启动即注册 apply_patch tool),且 tool
    // description 本身已含完整 V4A 规则,模型仍能正确生成 patch。
    let is_first_turn = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if is_first_turn && tools_register_apply_patch(body) {
        messages.push(apply_patch_chat_guidance_message());
    }

    let current_messages = body
        .get("input")
        .map(input_field_to_messages)
        .unwrap_or_default();
    messages.extend(current_messages);
    merge_messages_with_previous_response(messages, body, session_cache)
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
            if let Some(text) = summary.as_str() {
                if !text.trim().is_empty() {
                    parts.push(strip_codex_reasoning_prefix(text).to_owned());
                }
                continue;
            }
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
            let arguments = sanitize_tool_arguments_json_string(
                item.get("arguments").and_then(|v| v.as_str()).unwrap_or(""),
            );
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
            let output_str =
                normalize_tool_output_for_context(Some(call_id.as_str()), output_value);
            vec![json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output_str,
            })]
        }
        "custom_tool_call" => {
            // Codex CLI 把 freeform apply_patch 的回放 wire 包成
            // `ResponseItem::CustomToolCall { name, input, call_id, ... }`
            // (`codex-rs/protocol/src/models.rs:824-832`)。我们在 turn N 通过
            // `converter.rs::close_tool_call` apply_patch 分支 emit 了它;
            // Codex CLI 在 turn N+1 把同一 item 通过 `input[]` 回放给我们。
            // 转下游 chat completions 时必须重新打包成 `assistant.tool_calls`
            // 的 `type:"function"` 形态(chat 端不认 custom_tool_call),且
            // `function.arguments` 必须是 JSON 字符串 `{"input":"<V4A>"}`
            // (与首轮在 `tools.rs::convert_responses_tool_to_chat_tool` 的
            // `"custom" =>` 分支 lowering 形态保持一致)—— 模型才不会因
            // wire 形态变化失忆。
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_owned();
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input_text = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
            // arguments 必须是 chat function-call 的标准 JSON 字符串形态。
            // serde_json::to_string 自动处理换行 / 引号 / 反斜杠等所有转义。
            let arguments_json = serde_json::to_string(&json!({ "input": input_text }))
                .unwrap_or_else(|_| {
                    // to_string 在 input 是 valid UTF-8 string 时不会失败;若
                    // 真发生,fallback 到空对象保持下游 chat schema 合法。
                    "{}".to_owned()
                });
            let arguments = sanitize_tool_arguments_json_string(&arguments_json);
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
        "custom_tool_call_output" => {
            // `ResponseItem::CustomToolCallOutput { call_id, output, ... }`
            // (`codex-rs/protocol/src/models.rs:839-847`)使用与 function_call_output
            // 相同的 `output` payload encoding(string 或 content_items array)。
            // 转 chat 时只需把 wire item type 对齐到普通 `role:"tool"` message,
            // tool_call_id 来源仍按 call_id / tool_call_id / id 三级兜底。
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
            let output_str =
                normalize_tool_output_for_context(Some(call_id.as_str()), output_value);
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

pub(crate) fn normalize_tool_output_for_context(
    call_id: Option<&str>,
    output_value: Value,
) -> String {
    normalize_tool_output_for_context_with_store(
        call_id,
        output_value,
        Some(super::artifact_store::global_tool_artifact_store()),
    )
}

pub(crate) fn normalize_tool_output_for_context_with_store(
    call_id: Option<&str>,
    output_value: Value,
    artifact_store: Option<&super::artifact_store::ToolArtifactStore>,
) -> String {
    let raw = match output_value {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap_or_default(),
    };
    if raw.chars().count() <= TOOL_OUTPUT_INLINE_MAX_CHARS {
        return raw;
    }
    let kind = classify_tool_output(&raw);
    let artifact = artifact_store.map(|store| store.save(call_id, kind, &raw));
    build_bounded_tool_output_summary(&raw, kind, artifact.as_ref())
}

fn build_bounded_tool_output_summary(
    raw: &str,
    kind: &str,
    artifact: Option<&super::artifact_store::StoredToolArtifact>,
) -> String {
    let original_chars = raw.chars().count();
    let original_lines = raw.lines().count();
    let mut out = String::new();

    out.push_str("[Tool output stored outside model context]\n");
    out.push_str("Visible content below is a bounded evidence summary, not the full raw output.\n");
    if let Some(artifact) = artifact {
        out.push_str(&format!("Artifact ID: {}\n", artifact.artifact_id));
        if let Some(call_id) = artifact.call_id.as_deref() {
            out.push_str(&format!("Tool call ID: {call_id}\n"));
        }
    } else {
        out.push_str("Artifact ID: unavailable; raw payload could not be stored.\n");
    }
    out.push_str(&format!("Artifact kind: {kind}\n"));
    out.push_str(&format!(
        "Original size: {original_chars} chars across {original_lines} lines.\n"
    ));
    if let Some(token_count) = extract_marker_value(raw, "Original token count:") {
        out.push_str(&format!("Original token count: {token_count}\n"));
    }
    if let Some(total_lines) = extract_marker_value(raw, "Total output lines:") {
        out.push_str(&format!("Reported output lines: {total_lines}\n"));
    }

    let path_hints = extract_path_hints(raw, 12);
    if !path_hints.is_empty() {
        out.push_str("Path hints:\n");
        for path in path_hints {
            out.push_str("- ");
            out.push_str(&path);
            out.push('\n');
        }
    }

    let url_hints = extract_url_hints(raw, 12);
    if !url_hints.is_empty() {
        out.push_str("URL hints:\n");
        for url in url_hints {
            out.push_str("- ");
            out.push_str(&url);
            out.push('\n');
        }
    }

    out.push_str("\n--- Begin head excerpt ---\n");
    out.push_str(&take_first_chars(raw, TOOL_OUTPUT_HEAD_CHARS));
    out.push_str("\n--- End head excerpt ---\n");
    out.push_str("\n--- Begin tail excerpt ---\n");
    out.push_str(&take_last_chars(raw, TOOL_OUTPUT_TAIL_CHARS));
    out.push_str("\n--- End tail excerpt ---\n");
    out.push_str(&format!(
        "\n[Omitted raw tool output from model context. Original size: {original_chars} chars.]"
    ));

    if out.chars().count() > TOOL_OUTPUT_VISIBLE_MAX_CHARS {
        let mut trimmed = take_first_chars(&out, TOOL_OUTPUT_VISIBLE_MAX_CHARS);
        trimmed.push_str("\n[Tool output compression summary truncated to visible budget.]");
        return trimmed;
    }
    out
}

fn classify_tool_output(raw: &str) -> &'static str {
    let sample = raw.chars().take(20_000).collect::<String>();
    let trimmed = sample.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<Value>(trimmed).is_ok()
    {
        return "json";
    }
    if sample.contains("https://")
        || sample.contains("http://")
        || sample.contains("web_search")
        || sample.contains("Search results")
        || sample.contains("source:")
    {
        return "web_or_search";
    }
    if sample.contains("Process exited with code")
        || sample.contains("Exit code")
        || sample.contains("Wall time:")
        || sample.contains("Output:")
    {
        return "command_output";
    }
    if !extract_path_hints(&sample, 1).is_empty() {
        return "file_or_code_output";
    }
    "opaque_tool_output"
}

fn extract_marker_value(raw: &str, marker: &str) -> Option<String> {
    let start = raw.find(marker)?;
    let rest = &raw[start + marker.len()..];
    let value = rest
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn extract_url_hints(raw: &str, max: usize) -> Vec<String> {
    let mut urls = Vec::new();
    for token in raw.lines().take(200).flat_map(str::split_whitespace) {
        let candidate = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
        });
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            continue;
        }
        if urls.iter().any(|existing| existing == candidate) {
            continue;
        }
        urls.push(candidate.to_owned());
        if urls.len() >= max {
            break;
        }
    }
    urls
}

fn extract_path_hints(raw: &str, max: usize) -> Vec<String> {
    let mut paths = Vec::new();
    for line in raw.lines().take(200) {
        for token in line.split_whitespace() {
            let candidate = token
                .trim_matches(|ch: char| {
                    matches!(
                        ch,
                        '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
                    )
                })
                .split(':')
                .next()
                .unwrap_or("");
            if !(candidate.starts_with('/') || candidate.starts_with("./")) {
                continue;
            }
            if !candidate.contains('.') {
                continue;
            }
            if paths.iter().any(|existing| existing == candidate) {
                continue;
            }
            paths.push(candidate.to_owned());
            if paths.len() >= max {
                return paths;
            }
        }
    }
    paths
}

fn take_first_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn take_last_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars().rev().take(max).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
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

/// 修复 / 重建工具调用 id 关联(双向)。
///
/// **正向(tool message 找 assistant.tool_calls)**:
///   1. tool_call_id 为空 → 从前一条 assistant.tool_calls 顺序补 id
///   2. tool_call_id 非空且能在前 assistant.tool_calls 找到 → 直接 ack 通过
///   3. tool_call_id 非空但前 assistant 不含该 id(history 被压缩 / 截断 /
///      跨 session 续接)→ 查 ToolCallCache:
///        - 命中:把 tool_call 注回最近一条 assistant 的 tool_calls 列表
///        - 未命中:在前面塞一条占位 assistant
///   4. 完全没有前置 assistant + cache 也没有 → 插占位 assistant + 保留 tool
///
/// **反向(assistant.tool_calls 找 tool message)** (issue #180):
///   - assistant.tool_calls = [a, b, c] 但只跟了 tool b → DeepSeek / Kimi 严格
///     校验时 400("tool result without matching tool_use" 等)。
///   - 解决:在三个状态切换点 flush pending —— 进新 assistant message / 进
///     user/system/developer / 遍历末尾 —— 对每个未应答 id 注入占位 tool
///     消息,id 严格匹配上一条 assistant.tool_calls,content 用 litellm 同款
///     "[System: Tool execution skipped/interrupted by user. ...]"。
///
/// 与 litellm 1.84.0 `factory.py::_add_missing_tool_results` +
/// `transformation.py::_ensure_tool_results_have_corresponding_tool_calls`
/// 行为一致(只是 litellm 还做 Anthropic 合并,本仓库不需要)。
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
            // 进新 assistant 前:把上一个 assistant 还没答完的 tool_calls
            // 用占位 tool 消息补齐(issue #180 反向修复点 1/3)。
            flush_pending_tool_calls_as_placeholders(
                &mut repaired,
                &mut pending_call_ids,
                tool_call_cache,
            );
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
                    Some(e) => (e.name, sanitize_tool_arguments_json_string(&e.arguments)),
                    // path B3:cache 也未命中 → 占位 (name 空字符串),
                    // 上游能 match id 不报 400 是关键,name / args 由上游能容
                    None => (String::new(), "{}".to_owned()),
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
            // 进新一轮对话前:flush 上一轮未答完的 tool_calls(issue #180
            // 并行工具调用部分应答场景的核心修复点 2/3)。
            flush_pending_tool_calls_as_placeholders(
                &mut repaired,
                &mut pending_call_ids,
                tool_call_cache,
            );
            last_assistant_idx = None;
        }

        repaired.push(msg);
    }

    // 末尾 flush:整段 input 末尾 assistant.tool_calls 没有任何 tool 应答
    // (Codex CLI 中断生成 / 续轮时可能出现) (issue #180 修复点 3/3)。
    flush_pending_tool_calls_as_placeholders(&mut repaired, &mut pending_call_ids, tool_call_cache);

    *messages = repaired;
}

/// 把 `pending_call_ids` 里每个未应答的 tool_call id 翻成一条占位 tool 消息,
/// 直接 append 到 `repaired` 末尾(调用点保证此时插入位置是正确的:即上一条
/// assistant.tool_calls 之后、下一条非 tool 消息之前)。
fn flush_pending_tool_calls_as_placeholders(
    repaired: &mut Vec<Value>,
    pending_call_ids: &mut Vec<String>,
    tool_call_cache: &super::tool_call_cache::ToolCallCache,
) {
    if pending_call_ids.is_empty() {
        return;
    }
    for call_id in pending_call_ids.drain(..) {
        let tool_name = tool_call_cache
            .get(&call_id)
            .map(|entry| entry.name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "unknown_tool".to_owned());
        let content = format!(
            "[System: Tool execution skipped/interrupted by user. \
             No result provided for tool '{tool_name}'.]"
        );
        repaired.push(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        }));
    }
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

pub(crate) fn provider_looks_like(provider: &Provider, needle: &str) -> bool {
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
    const MINIMAX_SYSTEM_MESSAGE_MAX_CHARS: usize = 24_000;
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
    // **issue #139 修(2026-05-12)**:MiniMax /v1/chat/completions 不接受
    // role=system,400 invalid role。把 system 全转 user + [System]\n prefix。
    convert_minimax_system_to_user_prefix(body, MINIMAX_SYSTEM_MESSAGE_MAX_CHARS);
    sanitize_minimax_tool_call_arguments(body);
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

fn sanitize_minimax_tool_call_arguments(body: &mut Map<String, Value>) {
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for call in tool_calls.iter_mut() {
            let Some(function) = call.get_mut("function").and_then(|v| v.as_object_mut()) else {
                continue;
            };
            let arguments = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            function.insert(
                "arguments".into(),
                Value::String(sanitize_tool_arguments_json_string(arguments)),
            );
        }
    }
}

fn sanitize_tool_arguments_json_string(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return "{}".to_owned();
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_owned();
    }
    "{}".to_owned()
}

/// MiniMax `/v1/chat/completions` **不接受 `role="system"` 的 message**
/// (issue #139,2026-05-12 实证 M2.7 + Text-01 都返 `400 invalid params,
/// chat content has invalid message role: system (2013)`)。
///
/// 本 fn 把所有 system message **转成 `role="user"` + content 前置 `[System]\n`
/// 前缀**,模型从前缀 token 自己理解原 system role 语义(chenyme/grok2api 同
/// pattern 在 grok_web 多轮 flatten 也已实证 work)。
///
/// 同时保留原 `normalize_and_split_minimax_system_messages` 的副作用:
/// - `\r\n` → `\n`(MiniMax 对 Windows 换行敏感)
/// - 超过 `max_chars`(24000)的 system 内容**切片成多个独立 user message**,
///   每片用 `[System part i/N]\n` 前缀(silent-failure F4:让模型看出是同一逻辑
///   段落的连续分片,不会误判为 N 个独立 system 指令);单段不切则用 `[System]\n`
///
/// **顺序依赖**:必须在 `merge_consecutive_system_messages` **之后**调用,
/// 那一步合并相邻 system + Codex instructions 字段,**仍是 system role**;
/// 本 fn 是 MiniMax-specific 兜底,把 system role 一次性转 user。
///
/// **content 形态**(silent-failure F2 修):接受 string + array of
/// `{type:input_text|text|output_text, text:"..."}` parts(Codex CLI Responses
/// spec 数组形)。其他形态 emit `tracing::warn!` 不静默 raw JSON stringify。
fn convert_minimax_system_to_user_prefix(body: &mut Map<String, Value>, max_chars: usize) {
    debug_assert!(
        max_chars > 0,
        "max_chars=0 会让本 fn 短路前不做 role 转换,违反 'no role=system 出本 fn' invariant"
    );
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };
    if max_chars == 0 {
        // 防御 prod 误传 0(同 debug_assert 但 release 不 panic):
        // 仍做 role 转换,不切片(整段当一个 user message,即便超长)。
        // **必须**保证 invariant "出本 fn 后没有 role=system"。
        tracing::warn!(
            error_id = "MINIMAX_SYS_CONVERT_MAX_CHARS_ZERO",
            "convert_minimax_system_to_user_prefix 收到 max_chars=0,跳过切片但仍做 role 转换"
        );
    }
    const SYSTEM_PREFIX_SINGLE: &str = "[System]\n";
    let mut rewritten: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages.drain(..) {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "system" {
            rewritten.push(msg);
            continue;
        }
        let normalized = extract_minimax_system_text(msg.get("content")).replace("\r\n", "\n");
        if normalized.is_empty() {
            tracing::debug!(
                error_id = "MINIMAX_SYS_CONVERT_EMPTY_SKIP",
                "convert_minimax_system_to_user_prefix 跳过空 system message"
            );
            continue;
        }
        if max_chars == 0
            || normalized.chars().count() + SYSTEM_PREFIX_SINGLE.chars().count() <= max_chars
        {
            // 单段 fast path:整段(含 `[System]\n` prefix)≤ max_chars,直接 emit
            rewritten.push(json!({
                "role": "user",
                "content": format!("{SYSTEM_PREFIX_SINGLE}{normalized}"),
            }));
            continue;
        }
        // 切片 path(chatgpt-codex P1 修):先**预估 prefix 长度**算进每片预算,
        // 保证最终 emit 的每条 user message **整体 chars().count() ≤ max_chars**
        // (之前直接 split(content, max_chars) 后加 prefix → 每条 ≈ max_chars + 22
        //  char 超限,MiniMax 仍 400)
        let chunks = split_system_content_for_prefix(&normalized, max_chars);
        let total = chunks.len();
        tracing::info!(
            error_id = "MINIMAX_SYS_CONVERT_SPLIT",
            original_chars = normalized.chars().count(),
            chunks = total,
            max_chars,
            "convert_minimax_system_to_user_prefix 长 system 切成多段 user prefix message"
        );
        for (idx, chunk) in chunks.into_iter().enumerate() {
            let prefix = format!("[System part {}/{total}]\n", idx + 1);
            rewritten.push(json!({
                "role": "user",
                "content": format!("{prefix}{chunk}"),
            }));
        }
    }
    *messages = rewritten;
}

/// 抽 MiniMax system message 的 content 文本(silent-failure F2 修)。
///
/// 接受形态:
/// 给 `[System part i/N]\n` prefix 留预算后切 system content。
///
/// **不变量(chatgpt-codex P1 修)**:返回的每个 chunk 加上其最终 prefix 后
/// `chars().count() ≤ max_chars`。算法两轮迭代:
/// 1. 第一轮假设 N ≤ 9(1 digit prefix `[System part 1/9]\n` = 19 char)算 budget
/// 2. 若实际切出 chunks ≥ 10 → digit 数升,用更大 prefix length 再算一次
///
/// 99 段以内单 / 双轮收敛(99 段 prefix 最长 21 char,极少触发);MAX_CHARS=24000
/// 下 system 内容 > 24000*99 ≈ 2.3MB 才可能 99+ 段,实际不可能。
///
/// **edge case**:`max_chars ≤ prefix_max` 时 budget=0,降级为 budget=1
/// (`.max(1)`)避免无限切割空 chunk。这种 misconfiguration 下 emit 单字符
/// chunks 但仍保证 invariant(prefix + 1 char ≤ max_chars 要求 max_chars ≥ 22,
/// MAX_CHARS=24000 远高于此)。
fn split_system_content_for_prefix(normalized: &str, max_chars: usize) -> Vec<String> {
    let estimate_budget = |n_digits: usize| -> usize {
        // prefix 形 "[System part {i}/{N}]\n",静态部分 "[System part /]\n" + 2*N digits
        // 保守按"i 也用 N 位数"估上限
        const STATIC_LEN: usize = "[System part /]\n".len();
        let prefix_max = STATIC_LEN + 2 * n_digits;
        max_chars.saturating_sub(prefix_max).max(1)
    };
    // 第一轮:假设 N ≤ 9(1 digit)
    let chunks = split_string_by_char_limit(normalized, estimate_budget(1));
    if chunks.len() <= 9 {
        return chunks;
    }
    // 第二轮:N ≥ 10,用更大 prefix budget 重切
    let n_digits = chunks.len().to_string().len();
    split_string_by_char_limit(normalized, estimate_budget(n_digits))
}

/// 抽 MiniMax system message 的 content 文本(silent-failure F2 修)。
///
/// 接受形态:
/// - `Value::String(s)` → 直接返回
/// - `Value::Array(parts)` → 抽 `parts[i].text` 字段 join `\n`(Codex CLI
///   Responses spec 形:`[{type:"input_text", text:"..."}, ...]`)
/// - 其他形态 → `tracing::warn!` 加 stable error_id,**不静默** raw JSON 注入
fn extract_minimax_system_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            if parts.is_empty() {
                return String::new();
            }
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .collect();
            if !texts.is_empty() {
                texts.join("\n")
            } else {
                // 数组有 parts 但无可抽 text(eg image-only system,异常 schema):
                // warn + 返回空,上层 skip。比注入 raw JSON 安全。
                let part_types: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.get("type").and_then(Value::as_str))
                    .collect();
                tracing::warn!(
                    error_id = "MINIMAX_SYS_CONTENT_NO_TEXT_PARTS",
                    part_types = ?part_types,
                    "MiniMax system message 的 content array 无 text parts,跳过"
                );
                String::new()
            }
        }
        other => {
            let shape = if other.is_null() {
                "null"
            } else if other.is_boolean() {
                "bool"
            } else if other.is_number() {
                "number"
            } else if other.is_object() {
                "object"
            } else {
                "unknown"
            };
            tracing::warn!(
                error_id = "MINIMAX_SYS_CONTENT_UNEXPECTED_SHAPE",
                shape,
                "MiniMax system message 的 content 既不是 string 也不是 array,跳过"
            );
            String::new()
        }
    }
}

fn split_string_by_char_limit(input: &str, max_chars: usize) -> Vec<String> {
    if input.is_empty() || max_chars == 0 {
        return vec![input.to_owned()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in input.chars() {
        current.push(ch);
        count += 1;
        if count == max_chars {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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
            // 智谱 GLM 文本旗舰模型（官方明确为文本模型，视觉走 GLM-5V 等独立模型）
            "glm-5.1",
            "glm-4.7",
            // 阿里云百炼 Qwen 标准版（视觉能力走 Qwen-VL 系列）
            "qwen3.6-plus",
            "qwen3.6-flash",
            // MiniMax M2.7（文本模型，图像理解通过独立 MCP 工具实现）
            "MiniMax-M2.7",
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
    tracing::debug!(
        target: "codex_app_transfer::vision",
        "Stripping vision content because current provider/model does not support vision (Computer Use protection active)"
    );

    const PLACEHOLDER: &str = "[image omitted: current provider does not support vision]";
    const COMPUTER_USE_PLACEHOLDER: &str =
        "[Computer Use screenshot omitted: current model does not support vision]";

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
            if block_type == "image_url" || block_type == "input_image" {
                had_image = true;

                // Simple heuristic for Computer Use screenshots
                let is_likely_computer_use = block_obj
                    .get("image_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len() > 20000)
                    .unwrap_or(false);

                let placeholder = if is_likely_computer_use {
                    COMPUTER_USE_PLACEHOLDER
                } else {
                    PLACEHOLDER
                };

                block_obj.clear();
                block_obj.insert("type".into(), Value::String("text".into()));
                block_obj.insert("text".into(), Value::String(placeholder.to_string()));
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

/// 兜底:有 `image_url` block 但完全没 `text` block 的 message,在 content
/// 数组末尾追加 `{type:"text", text:" "}` —— 防止 MiMo 多模态接口因
/// "Param Incorrect: text is not set" 拒绝纯图请求(MiMo 文档要求含
/// `image_url` 时至少 1 个 text part)。其他 `supports_vision` provider
/// (Kimi / OpenAI 等)对此无副作用,统一处理避免 per-provider 分支,
/// 也省掉未来新接 vision provider 时重新评估的工作。
///
/// 对照实现:`7as0nch/mimo2codex` `reqToChat.ts:71-79` 同名兜底逻辑。
fn ensure_text_part_when_image_present(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let Some(Value::Array(arr)) = obj.get_mut("content") else {
            continue;
        };
        let has_image = arr
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("image_url"));
        let has_text = arr
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"));
        if has_image && !has_text {
            arr.push(json!({"type": "text", "text": " "}));
        }
    }
}

/// Stage 3 (in progress): Stronger stripping for `input_image` (Computer Use screenshots)
/// will be added here in a follow-up refinement. For now, the main defense is
/// in strip_image_blocks_in_place + tool filtering (Stage 2).

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

pub mod tools;

#[cfg(test)]
mod tests;

use tools::{
    contains_kimi_web_search_tool, convert_responses_tool_to_chat_tool, normalize_tool_choice,
    APPLY_PATCH_TOOL_NAME,
};

/// chat-path 实战指引,作为独立 `role:"system"` 注入,仅在该 turn 的 tools
/// 数组里注册了 `apply_patch` 时启用。理由参见 issue #235 真机稳定性测试。
///
/// **本版本(round 4 capture 实证根因修复)** :
/// 旧版第 1 条"Use an EMPTY LINE as the `@@` anchor"是事实错误 — 上游
/// V4A 官方规范(`codex-rs/core/prompt_with_apply_patch_instructions.md`
/// L298-314)的 `@@` 是**单端语法**:`@@ <header>` 命名 class/function 等
/// section,**不带尾随 `@@`**。旧版误写为 `@@ <context> @@` 双端 + 推荐
/// "empty content as anchor" 双重错误导致 Codex Desktop V4A applier
/// 全程匹配失败(`Failed to find context '... @@'`)。本次修订:
///   1. 删除 EMPTY LINE anchor 建议(误导)
///   2. 显式说明 `@@` 单端语法 + 给出 `@@ class X` / `@@ def f():` 示例
///   3. 加 Add File 必须每行 `+` 前缀的强调
///   4. 加 "If Update repeatedly fails, fall back to Delete + Add File" 兜底
const APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE: &str = concat!(
    "[apply_patch chat-path guidance — injected by codex-app-transfer adapter because the upstream lark grammar constraint is unavailable on chat function-call providers]\n",
    "\n",
    "**ALWAYS use the `apply_patch` tool for any file creation or modification** — including new files, single-line edits, and full-file rewrites. **NEVER use shell `cat <<EOF > file` / `printf > file` / `echo > file` / any `>` redirect to bypass apply_patch** — doing so loses the Codex diff UI and audit trail. For full-file rewrites or large changes where almost every line differs, use `*** Delete File: <path>` followed by `*** Add File: <path>` (every line of the new content prefixed with `+`) inside the same patch — this is more concise than a long `-`/`+` diff and is the correct apply_patch idiom for large rewrites.\n",
    "\n",
    "When you call the `apply_patch` tool, follow these rules empirically observed with non-OpenAI chat providers:\n",
    "\n",
    "1. PREFERRED Update File form is MINIMAL: just `-line` (the row to remove, byte-exact) and `+line` (the new row) directly after `*** Update File: <path>` — NO `@@`, NO context lines. ",
    "Use this whenever the `-` line is unique in the file (true for most simple single-line edits, config changes, function signatures, etc.). Example:\n",
    "  *** Update File: src/config.py\n",
    "  -DEBUG = False\n",
    "  +DEBUG = True\n",
    "If the `-` line alone is ambiguous (same line text in multiple places), add space-prefixed context lines (` line`) above/below to pin it down. ",
    "Only if context lines are also insufficient, add a SINGLE-SIDED `@@ <header>` marker on its own row (`@@ class Foo`, `@@ def bar():`, `@@ fn main() {`). ",
    "**NEVER add a trailing `@@`** (`@@ <header> @@` is wrong) — Codex Desktop's V4A applier treats trailing `@@` as literal text and fails with `Failed to find context '... @@'`. ",
    "For deeply nested disambiguation use MULTIPLE `@@` lines on separate rows (e.g. `@@ class Outer\\n@@ def inner():`), each single-sided.\n",
    "\n",
    "2. Add File uses NO `@@` markers and NO hunks. After `*** Add File: <path>`, prefix EVERY line of the new file's content with `+`, including blank lines (write them as a bare `+` on its own row). Raw source code without `+` prefix (e.g. `def main():` directly) causes `'def main():' is not a valid hunk header` errors.\n",
    "\n",
    "3. Every `-` line and space-prefixed context line MUST match the file byte-for-byte (same leading whitespace, no trimmed trailing spaces, exact characters). If unsure, run `cat <path>` or `sed -n '1,80p' <path>` via shell first, then compose the patch from real bytes. Guessing produces `Failed to find context '<your guess>'` errors.\n",
    "\n",
    "3a. Line prefix is a SINGLE character with NO space between prefix and content: write `-DEBUG = False` (not `- DEBUG = False`), `+DEBUG = True` (not `+ DEBUG = True`), and ` keepme` (single leading space, for unchanged context). Codex Desktop V4A applier may tolerate a stray space, but other apply_patch implementations are strict — keep the prefix tight.\n",
    "\n",
    "4. Do NOT combine `*** Add File: <path>` and `*** Update File: <path>` for the same path in a single patch. The Update step reads the file before the Add step lands on disk, so it sees an empty file and fails. Either: (a) make `*** Add File:` write the final content in one shot, or (b) split into two separate `apply_patch` invocations.\n",
    "\n",
    "5. `*** Update File:` cannot operate on a totally empty file. If the target is empty, first use shell (e.g. `printf '\\n' > <path>`) to write at least one line, then call `apply_patch`.\n",
    "\n",
    "6. In a multi-line file, lone `+` lines without a corresponding `-` line APPEND below the previous context — they do NOT replace any existing line. To change an existing line, you MUST include BOTH a `-` line (removing the old content) AND a `+` line (adding the new content).\n",
    "\n",
    "7. If repeated Update File attempts on the same target fail with `Failed to find context` errors, fall back to a Delete File + Add File pair within the same patch (semantically equivalent to a full rewrite, avoids anchor-matching fragility).\n",
    "\n",
    "8. `*** Begin Patch` MUST be the literal first line of the `input` string — no leading whitespace, no other content before it, never put `*** Add File:` or any operation header directly. Forgetting this causes `invalid patch: The first line of the patch must be '*** Begin Patch'`.\n",
    "\n",
    "9. `*** Update File: <old>` + `*** Move to: <new>` REQUIRES at least one hunk (with `-`/`+` lines or `*** End of File` marker). An empty Update+Move block fails with `Update file hunk for path '<old>' is empty`. **For pure rename without content change**, use `*** Delete File: <old>` + `*** Add File: <new>` within the same patch (copy original content with `+` prefix per line). **For rename WITH content change**, keep Update+Move and include the actual `-`/`+` hunks.\n",
    "\n",
    "Following these rules avoids retry storms and improves the success rate on first attempt."
);

/// 检测 Responses request body 的 tools 数组是否注册了 `apply_patch` 工具。
/// `apply_patch` 在 Responses 协议里以 `type:"custom", name:"apply_patch"` 出现,
/// 在被 [`convert_responses_tool_to_chat_tool`] 降级前。
/// 用于决定本 turn 是否注入 [`APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE`]。
fn tools_register_apply_patch(body: &Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.iter().any(|t| {
        t.get("name").and_then(Value::as_str) == Some(APPLY_PATCH_TOOL_NAME)
            && t.get("type").and_then(Value::as_str) == Some("custom")
    })
}

fn apply_patch_chat_guidance_message() -> Value {
    json!({
        "role": "system",
        "content": APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE,
    })
}
