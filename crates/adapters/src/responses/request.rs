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
//!   `function_call_output` / `tool_search_call`(→ assistant `tool_calls`
//!   name="tool_search")/ `tool_search_output`(→ role:tool result + 注入
//!   chat `tools[]`)/ `input_image` / `input_file` / `input_audio` /
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

        let mut chat_tools: Vec<Value> = filtered_tools
            .iter()
            .flat_map(|t| convert_responses_tool_to_chat_tool(t, provider))
            .collect();
        // 实验 exp/resources-to-tool-search:注入 tool_search 发现的工具。Codex
        // 0.130+ 把 MCP 工具 defer 到 tool_search,发现的具体工具只在 input[] 的
        // tool_search_output.tools 里,不在 body.tools[]。不注入则 LLM 看不到 →
        // 无法调用 → 只能循环调 tool_search。展平复用 namespace 同一路径。
        for discovered in discovered_tools_from_tool_search_output(input) {
            chat_tools.extend(convert_responses_tool_to_chat_tool(&discovered, provider));
        }
        dedup_chat_tools_by_name(&mut chat_tools);
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

    // reasoning → upstream effort/budget(按 provider 走 reasoning_effort_policy)
    //
    // 改前(v2.1.14 及更早):全 chat 上游共用 `normalize_chat_reasoning_effort`
    // 把 xhigh/max 砍到 high — 对 DeepSeek 致命(issue #254:max 档不可达),
    // 对 Kimi/GLM/MiMo/MiniMax/Qwen 也只是塞它们不认的字段。
    // 改后:按 provider 查 [`codex_app_transfer_registry::reasoning_effort_wire`],
    // DeepSeek 走 high/max 二档、其他 chat 上游 Drop、自定义 fallback 走 OpenAI enum。
    apply_codex_reasoning_effort_for_provider(&mut result, body, provider);

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
        "tool_search_call" => {
            // 实验 exp/resources-to-tool-search:input history 里 Codex 回放的
            // `ResponseItem::ToolSearchCall { call_id, execution, arguments }`
            // (codex `protocol/src/models.rs:792`)。这是 assistant 侧的工具调用
            // (converter.rs 把 LLM 的 tool_search / redirect 来的 list_mcp_resources
            // 改写成的 tool_search_call wire,Codex 执行后回放)。**必须**转成 chat
            // `assistant.tool_calls`(name="tool_search"),与下面 tool_search_output
            // 转的 `role:tool` message 配对 —— 否则 tool message 成孤儿,
            // repair_tool_call_ids 补空 name tool_call → 上游 400
            // (`messages[N].tool_calls[0] is missing a function name`)。
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_owned();
            // arguments 在 Responses 是 JSON Value(object,如 {"query":"notion"});
            // chat function.arguments 要 JSON **字符串**。
            let arguments_str = match item.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_else(|_| {
                    // 加固(MOC-48 observability):serialize 失败 fallback 到空对象会
                    // 静默丢掉 query,LLM 的 tool_search 调用变成无参 no-op。warn 让这种
                    // 极少见的 schema drift 可观测(Value→string 正常不会失败)。
                    tracing::warn!(
                        target: "adapters::tool_search",
                        "tool_search_call arguments failed to serialize; falling back to empty object — query lost (likely Codex schema drift)",
                    );
                    "{}".to_owned()
                }),
                None => "{}".to_owned(),
            };
            let arguments = sanitize_tool_arguments_json_string(&arguments_str);
            vec![json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": if call_id.is_empty() { "call_unknown".to_owned() } else { call_id },
                    "type": "function",
                    "function": { "name": "tool_search", "arguments": arguments },
                }],
            })]
        }
        "tool_search_output" => {
            // 实验 exp/resources-to-tool-search:Codex 0.130+ 把 LLM 调的
            // tool_search 路由到本地 BM25,返 `ResponseItem::ToolSearchOutput
            // { call_id, status, execution, tools }`(codex `protocol/src/models.rs:839`)
            // 进下轮 input。**结构是 `tools` 字段,不是 `content`/`output`**,所以
            // 之前落默认 arm(无 content)被静默丢弃 → repair_tool_call_ids 补
            // "[Tool execution skipped... unknown_]" 误导占位 → LLM 以为工具失败
            // 死循环调 tool_search。
            //
            // 发现的具体工具走 `discovered_tools_from_tool_search_output` 注入
            // chat `tools[]`(LLM 才真正可调)。这里把 item 转成 role:tool message
            // 给 call_id 一个**真实** result 消除 repair 占位,content 列出工具名
            // 提示 LLM 可直接调用。
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("tool_call_id").and_then(|v| v.as_str()))
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_owned();
            // 加固(silent-failure review):call_id 缺失 → 下面 emit 的 role:tool
            // message 成孤儿(repair_tool_call_ids 静默 drop)→ 静默重现本 PR 修的
            // "工具静默消失" bug 类。tool_search_call / tool_search_output 本应按
            // call_id 配对,缺失通常是 Codex schema drift,warn 让它可观测(发现的
            // 工具映射仍经 tools[] 注入生效,只是这条 output 配不上 pair)。
            if call_id.is_empty() {
                tracing::warn!(
                    target: "adapters::tool_search",
                    "tool_search_output missing call_id; role:tool message will be orphaned and dropped by repair_tool_call_ids — likely Codex schema drift",
                );
            }
            let names = extract_tool_search_output_tool_names(item);
            let content = if names.is_empty() {
                "tool_search returned no matching tools.".to_owned()
            } else {
                format!(
                    "tool_search discovered {} tool(s), now available to call directly: {}",
                    names.len(),
                    names.join(", ")
                )
            };
            vec![json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content,
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

/// 实验 exp/resources-to-tool-search:从 input[] 的 `tool_search_output` items
/// 收集 Codex 发现的工具(namespace/function 结构)。Codex 0.130+ 把 MCP 工具
/// defer 到 tool_search,发现的工具只出现在 tool_search_output.tools,**不在**
/// body.tools[];不注入 chat tools[] 则 LLM 永远看不到 → 无法调用。
fn discovered_tools_from_tool_search_output(input: &Value) -> Vec<Value> {
    let Some(items) = input.get("input").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let outputs: Vec<&Value> = items
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_search_output"))
        .collect();
    let discovered: Vec<Value> = outputs
        .iter()
        .filter_map(|item| item.get("tools").and_then(|t| t.as_array()))
        .flatten()
        .cloned()
        .collect();
    // 加固(MOC-48 observability):有 tool_search_output item 却收集到 0 个工具,
    // 通常是 Codex schema drift(tools 字段改名/类型变)—— 不注入则 LLM 看不到工具
    // 只能循环调 tool_search,无日志难定位。普通请求(无 tool_search_output)不打
    // 日志避免噪音;count log 让 input malformed / 真无发现可区分。
    if !outputs.is_empty() {
        tracing::debug!(
            target: "adapters::tool_search",
            tool_search_output_items = outputs.len(),
            discovered_tools = discovered.len(),
            "collected discovered tools from tool_search_output for chat tools[] injection",
        );
    }
    discovered
}

/// 按 `function.name` 去重 chat tools(保留首次出现 — body.tools[] builtin 在
/// 发现工具之前 extend,故 builtin 优先)。空 name 不参与去重(全保留)。
fn dedup_chat_tools_by_name(tools: &mut Vec<Value>) {
    let mut seen = std::collections::HashSet::new();
    tools.retain(|t| {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if name.is_empty() {
            return true;
        }
        seen.insert(name.to_owned())
    });
}

/// 提取 tool_search_output.tools 里所有具体工具名(namespace 包展开内层
/// function.name;顶级 function 直接取 name)。用于 role:tool message content。
fn extract_tool_search_output_tool_names(item: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(tools) = item.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(|t| t.as_str()) == Some("namespace") {
            if let Some(inner) = tool.get("tools").and_then(|v| v.as_array()) {
                for f in inner {
                    if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                        names.push(n.to_owned());
                    }
                }
            }
        } else if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
            names.push(n.to_owned());
        }
    }
    // 加固(MOC-48 observability):无 inner tools 的 namespace 包 / 非 namespace entry
    // 被静默跳过。tools 非空但 names 为空,通常是 Codex 0.131+ schema drift(包结构变 /
    // name 字段改名)→ role:tool content 退化成 "no matching tools" 误导 LLM。
    // debug log tools.len vs names.len 让 drift 可观测。
    if !tools.is_empty() {
        tracing::debug!(
            target: "adapters::tool_search",
            input_tools = tools.len(),
            extracted_names = names.len(),
            "extracted tool names from tool_search_output",
        );
    }
    names
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
    // MiniMax-M3 起的新模型原生接受标准 OpenAI 字段。2026-06-03 真机实测
    // (api.minimaxi.com 直连 MiniMax-M3):`role=system` / `response_format` /
    // `parallel_tool_calls` 全部 200 接受,而 M2.x 对同样字段 400(invalid
    // params 2013 / invalid role)。故 M3 走宽松路径,不做 M2.x 的字段剥离 +
    // system→user 破坏性改写;M2.x 保持原限制(#139)。
    let model_lc = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_m3_plus = model_lc.starts_with("minimax-m3");
    let response_format_allowed = is_m3_plus || model_lc == "minimax-text-01";

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
            || (is_m3_plus && matches!(key.as_str(), "parallel_tool_calls" | "reasoning_effort"))
    });

    // MiniMax 官方建议 OpenAI-compatible M2.7 工具调用启用
    // reasoning_split,让 thinking 单独进入 reasoning_details,避免塞进
    // content 的 <think>...</think> 里。
    body.insert("reasoning_split".into(), Value::Bool(true));
    // M2.x 的 OpenAI-compatible streaming 不稳定接受 `stream_options.include_usage`,
    // 删掉(响应转换层补零值 usage)。**M3 例外**:2026-06-03 真机实测 M3 streaming +
    // include_usage 稳定返回真实 usage(total_tokens/prompt_tokens/completion_tokens),
    // 必须保留——否则 Codex 收到的 token 恒 0,`auto_compact_token_limit`(context×80%)
    // 永不触发,对话无限膨胀直到撞 MiniMax TPM 429(#356 用户实测病态对话根因)。
    if !is_m3_plus {
        body.remove("stream_options");
    }
    merge_consecutive_system_messages(body);
    // **issue #139 修(2026-05-12)**:MiniMax M2.x /v1/chat/completions 不接受
    // role=system,400 invalid role。把 system 全转 user + [System]\n prefix。
    // M3 起原生接受 role=system(2026-06-03 真机实测 200),跳过转换以免破坏
    // system prompt 语义(system 指令权重 ≠ user message)。
    if !is_m3_plus {
        convert_minimax_system_to_user_prefix(body, MINIMAX_SYSTEM_MESSAGE_MAX_CHARS);
    }
    sanitize_minimax_tool_call_arguments(body);
    sanitize_minimax_tools(body);

    // M2.x 只接受 tool_choice=auto/none,其它(required / 具名 function)降级到 auto。
    // M3 起支持 required(2026-06-03 真机实测:tool_choice=required → 200 且真的发起
    // tool_call),不降级,保留客户端的强制调用意图。
    if !is_m3_plus {
        if let Some(choice) = body.get_mut("tool_choice") {
            let allowed = choice
                .as_str()
                .is_some_and(|s| matches!(s, "auto" | "none"));
            if !allowed {
                *choice = Value::String("auto".into());
            }
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
    // M2.x 用经典 OpenAI tool schema、不接受 strict function-calling 元数据,剥掉。
    // M3 例外:2026-06-03 真机实测 M3 接受 `function.strict:true`(200 + 正常发起
    // tool_call),保留以获得严格 schema 输出保证(剥掉是功能损失)。
    let is_m3_plus = body
        .get("model")
        .and_then(|v| v.as_str())
        .is_some_and(|m| m.to_ascii_lowercase().starts_with("minimax-m3"));
    if is_m3_plus {
        return;
    }
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for tool in tools.iter_mut() {
        let Some(function) = tool.get_mut("function").and_then(|v| v.as_object_mut()) else {
            continue;
        };
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

/// 从 Codex `reasoning` 字段抽出 effort 字符串.
///
/// Codex 协议两种形态都接受:
/// - `"reasoning": "high"`(legacy 字符串)
/// - `"reasoning": {"effort": "high", "summary": "..."}`(标准对象)
///
/// 抽出后已做 trim + lowercase,empty 返回 None。其他字段(summary 等)在
/// chat 协议里无对应,丢弃。
///
/// 非预期 JSON 形态(Array / Bool / Number 等)走 `warn` log 后 drop —
/// 这通常是 Codex 协议变更或调用方协议错误,需告警以便 debug。
fn extract_codex_effort(reasoning: Option<&Value>) -> Option<String> {
    let reasoning = reasoning?;
    let raw = match reasoning {
        Value::String(s) => s.as_str(),
        Value::Object(obj) => obj.get("effort").and_then(|v| v.as_str())?,
        Value::Null => return None,
        other => {
            tracing::warn!(
                target: "adapters::responses::request",
                reasoning_kind = ?other,
                "unexpected reasoning field shape in codex request; dropping effort (possible protocol change)"
            );
            return None;
        }
    };
    let trimmed = raw.trim().to_ascii_lowercase();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 按 provider 把 Codex reasoning.effort 写进 chat body.
///
/// `provider == Some` 时走 [`codex_app_transfer_registry::apply_reasoning_effort`]
/// 的 per-provider 注册表;`provider == None` 时走 OpenAI 标准 enum 保守 fallback,
/// 并发出 `debug` log 标注路径(生产代码应始终有 provider 上下文,None 是
/// 测试 / 早期协议解析旁路场景,意外走 None 会让 effort 被砍回 OpenAI 上限 high
/// — 即 issue #254 同款症状,debug log 让 troubleshooting 有线索)。
fn apply_codex_reasoning_effort_for_provider(
    body: &mut Map<String, Value>,
    src: &Map<String, Value>,
    provider: Option<&Provider>,
) {
    let Some(effort) = extract_codex_effort(src.get("reasoning")) else {
        return;
    };
    match provider {
        Some(p) => {
            codex_app_transfer_registry::apply_reasoning_effort(body, p, &effort);
        }
        None => {
            tracing::debug!(
                target: "adapters::responses::request",
                codex_effort = %effort,
                "no provider context; falling back to OpenAI enum reasoning_effort (test / bypass path)"
            );
            // 用空 id 复用同一个 wire enum 路径,保持行为跟"未知自定义 provider"完全一致 +
            // 自带未知 effort 的 warn log,DRY 避免双份映射表
            codex_app_transfer_registry::ReasoningEffortWire::OpenAIEnum.apply(
                body,
                &effort,
                "<no-provider>",
            );
        }
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

/// chat-path 实战指引(英文版),作为独立 `role:"system"` 注入,仅在该 turn 的
/// tools 数组里注册了 `apply_patch` 时启用。理由参见 issue #235 真机稳定性测试。
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
///
/// **i18n**(#262):中文 user 输入 → 注入英文 system 易让模型中英混杂思考。
/// 提供 [`APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE_ZH`] 中文翻译;V4A 关键字
/// (`*** Begin Patch` / `@@ <header>` / `-line` / `+line` 等)+ 错误消息原文
/// (`Failed to find context '...'` 等)+ shell 命令例子保英文,因 Codex CLI
/// V4A parser / error matcher 接收的就是英文字面。
const APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE_EN: &str = concat!(
    "[apply_patch chat-path guidance — injected by codex-app-transfer adapter because the upstream lark grammar constraint is unavailable on chat function-call providers]\n",
    "\n",
    "**ALWAYS use the `apply_patch` tool to write file content** — new files, single-line edits, and full-file rewrites alike. **NEVER use shell `cat <<EOF > file` / `printf '<content>' > file` / `echo '<content>' > file` / any `>` redirect to write actual file content** — doing so bypasses the Codex diff UI and audit trail. (The narrow exception in rule 5 below — `printf '\\n' > <path>` to seed an empty file before `*** Update File:` — is an apply_patch preparation step, not a content bypass.) For full-file rewrites or large changes where almost every line differs, use `*** Delete File: <path>` followed by `*** Add File: <path>` (every line of the new content prefixed with `+`) inside the same patch — this is more concise than a long `-`/`+` diff and is the correct apply_patch idiom for large rewrites.\n",
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

/// 中文版 apply_patch chat-path 指引(#262)。
///
/// **翻译原则**:
/// - V4A 关键字保英文:`*** Begin Patch` / `*** Update File:` / `*** Add File:` /
///   `*** Delete File:` / `*** Move to:` / `*** End of File` / `@@ <header>` /
///   `-line` / `+line` / ` line`(context)— Codex CLI V4A parser 只认英文字面
/// - 错误消息保英文:`Failed to find context '...'` / `is not a valid hunk header` /
///   `invalid patch: The first line of the patch must be '*** Begin Patch'` /
///   `Update file hunk for path '...' is empty` — Codex CLI 抛出的就是英文,
///   翻成中文会让 user / 模型 grep 错错误信息
/// - shell 命令例子保英文:`cat`, `sed`, `printf`, `echo` 等命令名,文件路径
///   例子(`src/config.py` / `<path>`)
/// - 程序员日常英文术语保英文:`apply_patch` / `tool` / `shell` / `hunk` /
///   `context` / `patch` / `function` 等(混入中文反而不自然)
/// - 强调词译:**ALWAYS** → **务必**;**NEVER** → **绝不**;**MUST** → **必须**;
///   PREFERRED → 推荐;SINGLE-SIDED → 单端;DEEPLY NESTED → 深层嵌套
/// - 跟英文版**逐条对应**(9 条规则 + 引言段),不简化不漏 emphasis
const APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE_ZH: &str = concat!(
    "[apply_patch chat-path 指引 — 由 codex-app-transfer adapter 注入,因为上游 lark 语法约束在 chat function-call provider 上不可用]\n",
    "\n",
    "**务必使用 `apply_patch` tool 写文件内容** —— 新建文件、单行编辑、整文件重写都一样。**绝不使用 shell `cat <<EOF > file` / `printf '<content>' > file` / `echo '<content>' > file` / 任何 `>` 重定向来写实际文件内容** —— 这样做会绕过 Codex diff UI 和审计 trail。(下面规则 5 提到的窄例外 — `printf '\\n' > <path>` 在 `*** Update File:` 之前播种空文件 — 是 apply_patch 的预处理步骤,不是内容旁路。)对于整文件重写或几乎每行都不同的大改动,在同一 patch 内用 `*** Delete File: <path>` 加上 `*** Add File: <path>`(新内容每行前缀 `+`)—— 这比长 `-`/`+` diff 更精简,是大重写的正确 apply_patch 用法。\n",
    "\n",
    "调用 `apply_patch` tool 时,遵循以下基于非 OpenAI chat provider 实战观察总结的规则:\n",
    "\n",
    "1. 推荐的 Update File 形式是**最简形态**:仅 `-line`(要删的行,byte-exact)和 `+line`(新行)直接跟在 `*** Update File: <path>` 之后 —— 无 `@@`、无 context 行。",
    "凡是 `-` 行在文件里**唯一**时(简单单行编辑、配置改动、function 签名等绝大多数场景皆是)就用这个形态。例:\n",
    "  *** Update File: src/config.py\n",
    "  -DEBUG = False\n",
    "  +DEBUG = True\n",
    "若 `-` 行单独**有歧义**(同一行文本在文件多处出现),在上方/下方加空格前缀的 context 行(` line`)钉住它。",
    "若 context 行也不足以消歧,再在独立行上加**单端** `@@ <header>` 标记(`@@ class Foo`、`@@ def bar():`、`@@ fn main() {`)。",
    "**绝不加尾随 `@@`**(`@@ <header> @@` 是错的)—— Codex Desktop 的 V4A applier 会把尾随 `@@` 当字面文本,报 `Failed to find context '... @@'`。",
    "深层嵌套消歧时用**多个** `@@` 行各占一行(例如 `@@ class Outer\\n@@ def inner():`),每条都是单端。\n",
    "\n",
    "2. Add File **不用** `@@` 标记、**不用** hunk。`*** Add File: <path>` 之后,新文件**每一行**(包括空行,写成单个 `+` 占一行)都前缀 `+`。没 `+` 前缀的原始源码(例如直接写 `def main():`)会触发 `'def main():' is not a valid hunk header` 错误。\n",
    "\n",
    "3. 每个 `-` 行和空格前缀的 context 行**必须**跟文件 byte-for-byte 一致(同样的前导 whitespace,不能 trim 尾随空格,字符完全相同)。不确定时先用 shell 跑 `cat <path>` 或 `sed -n '1,80p' <path>` 查一下,再用真实字节组 patch。靠猜会触发 `Failed to find context '<your guess>'` 错误。\n",
    "\n",
    "3a. 行前缀是**单字符**,前缀和内容之间**没有空格**:写 `-DEBUG = False`(不是 `- DEBUG = False`)、`+DEBUG = True`(不是 `+ DEBUG = True`),context 行 ` keepme`(单个前导空格)。Codex Desktop V4A applier 可能容忍多余空格,但其它 apply_patch 实现严格 —— 前缀写紧凑。\n",
    "\n",
    "4. **不要**在同一 patch 内对同一路径同时用 `*** Add File: <path>` 和 `*** Update File: <path>`。Update 步骤会在 Add 步骤落盘前读文件,看到空文件后失败。要么 (a) 让 `*** Add File:` 一次性写最终内容,要么 (b) 拆成两个独立的 `apply_patch` 调用。\n",
    "\n",
    "5. `*** Update File:` **不能**作用于完全空的文件。目标为空时先用 shell(例如 `printf '\\n' > <path>`)写至少一行,再调 `apply_patch`。\n",
    "\n",
    "6. 多行文件里,**没有**对应 `-` 行的孤立 `+` 行会**追加**在上文 context 之下 —— **不会**替换任何已有行。要修改已有行,**必须**同时包含 `-` 行(删旧内容)和 `+` 行(加新内容)。\n",
    "\n",
    "7. 同一目标上多次 Update File 都报 `Failed to find context` 错误时,fallback 到同一 patch 内的 Delete File + Add File 组合(语义上等价于整文件重写,绕开 anchor 匹配的脆弱性)。\n",
    "\n",
    "8. `*** Begin Patch` **必须**是 `input` 字符串的字面第一行 —— 不能有前导空格,前面不能有其它内容,绝不能直接写 `*** Add File:` 或任何操作 header。漏了会触发 `invalid patch: The first line of the patch must be '*** Begin Patch'`。\n",
    "\n",
    "9. `*** Update File: <old>` + `*** Move to: <new>` **要求**至少一个 hunk(带 `-`/`+` 行或 `*** End of File` 标记)。空的 Update+Move 块会报 `Update file hunk for path '<old>' is empty`。**纯重命名不改内容**时,在同一 patch 内用 `*** Delete File: <old>` + `*** Add File: <new>`(把原内容每行前缀 `+` 复制过去)。**重命名同时改内容**时,保留 Update+Move 并写真实的 `-`/`+` hunk。\n",
    "\n",
    "遵循这些规则可以避免 retry 风暴,提升首次尝试的成功率。"
);

/// 按当前 user 语言偏好选 apply_patch chat-path 指引。
fn apply_patch_chat_path_guidance_for_current_language() -> &'static str {
    use crate::core::language::{current_language, Language};
    match current_language() {
        Language::Chinese => APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE_ZH,
        Language::English => APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE_EN,
    }
}

/// 检测 Responses request body 的 tools 数组是否注册了 `apply_patch` 工具。
/// `apply_patch` 在 Responses 协议里以 `type:"custom", name:"apply_patch"` 出现,
/// 在被 [`convert_responses_tool_to_chat_tool`] 降级前。
/// 用于决定本 turn 是否注入 [`apply_patch_chat_path_guidance_for_current_language`] 的产物。
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
        "content": apply_patch_chat_path_guidance_for_current_language(),
    })
}
