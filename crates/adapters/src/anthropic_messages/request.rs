use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::header::{HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::core::input::{merge_messages_with_previous_response, response_id_for_session};
use crate::core::tool_output::normalize_tool_output_for_context_with_store;
use crate::responses::{
    compact, global_response_session_cache, global_tool_artifact_store, global_tool_call_cache,
    ResponseSessionCache,
};
use crate::types::{AdapterError, RequestPlan, ResponseSessionPlan};

const DEFAULT_MAX_TOKENS: u64 = 4096;
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.92 (external, cli)";
const BETA_COMPUTER_USE_2025_01_24: &str = "computer-use-2025-01-24";
const BETA_COMPUTER_USE_2024_10_22: &str = "computer-use-2024-10-22";
const BETA_MCP_CLIENT_2025_04_04: &str = "mcp-client-2025-04-04";
const BETA_ADVANCED_TOOL_USE_2025_11_20: &str = "advanced-tool-use-2025-11-20";
const BETA_FILES_API_2025_04_14: &str = "files-api-2025-04-14";
const BETA_CODE_EXECUTION_2025_05_22: &str = "code-execution-2025-05-22";
const BETA_CODE_EXECUTION_2025_08_25: &str = "code-execution-2025-08-25";
const BETA_EFFORT_2025_11_24: &str = "effort-2025-11-24";
const BETA_SKILLS_2025_10_02: &str = "skills-2025-10-02";
const BETA_CONTEXT_MANAGEMENT_2025_06_27: &str = "context-management-2025-06-27";
const BETA_COMPACT_2026_01_12: &str = "compact-2026-01-12";
const BETA_STRUCTURED_OUTPUTS_2025_11_13: &str = "structured-outputs-2025-11-13";
const BETA_WEB_FETCH_2025_09_10: &str = "web-fetch-2025-09-10";
const BETA_FAST_MODE_2026_02_01: &str = "fast-mode-2026-02-01";
const BETA_ADVISOR_TOOL_2026_03_01: &str = "advisor-tool-2026-03-01";
const ANYROUTER_CLAUDE_CODE_BETA: &str = concat!(
    "claude-code-20250219,",
    "context-1m-2025-08-07,",
    "interleaved-thinking-2025-05-14,",
    "redact-thinking-2026-02-12,",
    "context-management-2025-06-27,",
    "prompt-caching-scope-2026-01-05,",
    "advanced-tool-use-2025-11-20,",
    "effort-2025-11-24,",
    "fast-mode-2026-02-01"
);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicToolNameMaps {
    pub forward: BTreeMap<String, String>,
    pub reverse: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct AnthropicMessagesRequestConversion {
    pub request: Value,
    pub response_session: ResponseSessionPlan,
    pub tool_name_maps: AnthropicToolNameMaps,
    provider_request_options: AppliedProviderRequestOptions,
}

#[derive(Debug, Clone, Default)]
struct AppliedProviderRequestOptions {
    claude_code_session_id: Option<String>,
    anthropic_beta_values: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct AnthropicMessagesPreparedRequest {
    pub upstream_path: String,
    pub body: Bytes,
    pub headers: HeaderMap,
    pub response_session: Option<ResponseSessionPlan>,
    pub is_compact: bool,
    pub original_responses_request: Option<Value>,
    pub tool_name_maps: AnthropicToolNameMaps,
}

/// Anthropic Messages requires a version header even when the request body is
/// otherwise OpenAI-compatible at the local gateway boundary. P5 adapter wiring
/// will merge these defaults without overriding user-configured provider
/// `extraHeaders`.
pub fn anthropic_messages_default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

/// Choose the relative path that `crates/proxy` will append to provider.base_url.
///
/// If the configured base URL already ends in `/v1`, appending `/messages`
/// avoids `.../v1/v1/messages`. Otherwise we append `/v1/messages`, matching
/// LiteLLM's Anthropic Messages URL completion behavior.
pub fn build_anthropic_messages_upstream_path(base_url: &str) -> String {
    let without_query = base_url.split('?').next().unwrap_or(base_url);
    let without_fragment = without_query.split('#').next().unwrap_or(without_query);
    let trimmed = without_fragment.trim_end_matches('/');
    let path = trimmed
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or("");
    if path.trim_end_matches('/').ends_with("v1") {
        "/messages".to_owned()
    } else {
        "/v1/messages".to_owned()
    }
}

pub fn prepare_anthropic_messages_request(
    client_path: &str,
    body: Bytes,
    provider: &Provider,
) -> Result<AnthropicMessagesPreparedRequest, AdapterError> {
    if compact::is_compact_path(client_path) {
        let compact_responses_body = compact::build_compact_responses_request_body(&body)?;
        let mut wire = responses_body_to_anthropic_messages_wire(
            &compact_responses_body,
            provider,
            None,
            false,
        )?;
        let provider_options = apply_provider_request_options(&mut wire.request, provider)?;
        let body = serde_json::to_vec(&wire.request).map_err(AdapterError::BodyDecode)?;
        let mut headers = anthropic_messages_default_headers();
        apply_provider_request_headers(&mut headers, &provider_options)?;
        return Ok(AnthropicMessagesPreparedRequest {
            upstream_path: build_anthropic_messages_upstream_path(&provider.base_url),
            body: Bytes::from(body),
            headers,
            response_session: None,
            is_compact: true,
            original_responses_request: None,
            tool_name_maps: wire.tool_name_maps,
        });
    }

    let parsed: Value = serde_json::from_slice(&body)?;
    let conversion = responses_body_to_anthropic_messages_request_with_session(
        &parsed,
        provider,
        Some(global_response_session_cache()),
    )?;
    let body = serde_json::to_vec(&conversion.request).map_err(AdapterError::BodyDecode)?;
    let mut headers = anthropic_messages_default_headers();
    apply_provider_request_headers(&mut headers, &conversion.provider_request_options)?;
    Ok(AnthropicMessagesPreparedRequest {
        upstream_path: build_anthropic_messages_upstream_path(&provider.base_url),
        body: Bytes::from(body),
        headers,
        response_session: Some(conversion.response_session),
        is_compact: false,
        original_responses_request: Some(parsed),
        tool_name_maps: conversion.tool_name_maps,
    })
}

pub fn into_request_plan(prepared: AnthropicMessagesPreparedRequest) -> RequestPlan {
    let adapter_metadata = serde_json::to_value(&prepared.tool_name_maps).ok();
    RequestPlan {
        upstream_path: prepared.upstream_path,
        body: prepared.body,
        upstream_headers: prepared.headers,
        response_session: prepared.response_session,
        adapter_metadata,
        is_compact: prepared.is_compact,
        original_responses_request: prepared.original_responses_request,
    }
}

pub fn is_anthropic_invalid_thinking_signature_error(body_bytes: &[u8]) -> bool {
    let Ok(body) = std::str::from_utf8(body_bytes) else {
        return false;
    };
    let lower = body.to_ascii_lowercase();
    lower.contains("invalid")
        && lower.contains("signature")
        && lower.contains("thinking")
        && lower.contains("block")
}

pub fn strip_thinking_blocks_for_invalid_signature_retry(
    plan: &mut RequestPlan,
) -> Result<bool, AdapterError> {
    let mut request: Value = serde_json::from_slice(&plan.body)?;
    let changed = strip_thinking_blocks_from_anthropic_messages_request_value(&mut request);
    if !changed {
        return Ok(false);
    }

    plan.body = Bytes::from(serde_json::to_vec(&request).map_err(AdapterError::BodyDecode)?);
    if let Some(session) = plan.response_session.as_mut() {
        strip_thinking_blocks_from_anthropic_messages(&mut session.messages);
    }
    Ok(true)
}

fn strip_thinking_blocks_from_anthropic_messages_request_value(request: &mut Value) -> bool {
    let Some(body) = request.as_object_mut() else {
        return false;
    };
    let mut changed = body.remove("thinking").is_some();
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        changed |= strip_thinking_blocks_from_anthropic_messages(messages);
    }
    changed
}

fn strip_thinking_blocks_from_anthropic_messages(messages: &mut Vec<Value>) -> bool {
    let mut changed = false;
    let mut next = Vec::with_capacity(messages.len());
    for mut message in std::mem::take(messages) {
        if strip_thinking_blocks_from_anthropic_message(&mut message, &mut changed) {
            next.push(message);
        } else {
            changed = true;
        }
    }
    *messages = next;
    changed
}

fn strip_thinking_blocks_from_anthropic_message(message: &mut Value, changed: &mut bool) -> bool {
    let Some(obj) = message.as_object_mut() else {
        return true;
    };
    let Some(content) = obj.get_mut("content").and_then(Value::as_array_mut) else {
        return true;
    };
    let before = content.len();
    content.retain(|block| {
        !matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    });
    if content.len() != before {
        *changed = true;
    }
    !content.is_empty()
}

pub fn responses_body_to_anthropic_messages_request(
    input: &Value,
    provider: &Provider,
) -> Result<AnthropicMessagesRequestConversion, AdapterError> {
    responses_body_to_anthropic_messages_request_with_session(input, provider, None)
}

pub fn responses_body_to_anthropic_messages_request_with_session(
    input: &Value,
    provider: &Provider,
    session_cache: Option<&ResponseSessionCache>,
) -> Result<AnthropicMessagesRequestConversion, AdapterError> {
    let mut wire = responses_body_to_anthropic_messages_wire(input, provider, session_cache, true)?;
    let provider_request_options = apply_provider_request_options(&mut wire.request, provider)?;
    Ok(AnthropicMessagesRequestConversion {
        request: wire.request,
        response_session: wire.response_session,
        tool_name_maps: wire.tool_name_maps,
        provider_request_options,
    })
}

fn apply_provider_request_options(
    request: &mut Value,
    provider: &Provider,
) -> Result<AppliedProviderRequestOptions, AdapterError> {
    let mut applied = AppliedProviderRequestOptions::default();
    let Some(body) = request.as_object_mut() else {
        return Ok(applied);
    };
    if let Some(options) = provider
        .request_options
        .get("anthropic_messages")
        .or_else(|| provider.request_options.get("messages"))
        .and_then(Value::as_object)
    {
        let claude_code_compat = option_bool(options, "claude_code_compat")
            || option_bool(options, "claude_code_compatibility");
        if claude_code_compat {
            applied.claude_code_session_id = Some(apply_claude_code_compat_body(body)?);
        }
        if let Some(thinking) = options.get("thinking").filter(|v| v.is_object()) {
            if body.get("thinking").is_none() || claude_code_compat {
                body.insert("thinking".into(), thinking.clone());
                ensure_max_tokens_exceeds_thinking_budget(body);
            }
        } else if claude_code_compat && body.get("thinking").is_none() {
            body.insert("thinking".into(), json!({"type": "adaptive"}));
        }
    }
    drop_thinking_for_forced_tool_choice(body);
    applied.anthropic_beta_values = collect_anthropic_beta_headers(body);
    Ok(applied)
}

fn option_bool(options: &Map<String, Value>, key: &str) -> bool {
    options.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn apply_provider_request_headers(
    headers: &mut HeaderMap,
    applied: &AppliedProviderRequestOptions,
) -> Result<(), AdapterError> {
    let mut beta_values = applied.anthropic_beta_values.clone();
    if applied.claude_code_session_id.is_some() {
        for beta in ANYROUTER_CLAUDE_CODE_BETA.split(',') {
            beta_values.insert(beta.trim().to_owned());
        }
    }
    if !beta_values.is_empty() {
        insert_header(
            headers,
            "anthropic-beta",
            &beta_values.into_iter().collect::<Vec<_>>().join(","),
        )?;
    }
    if let Some(session_id) = applied.claude_code_session_id.as_deref() {
        insert_header(headers, "x-app", "cli")?;
        insert_header(headers, "x-claude-code-session-id", session_id)?;
        insert_header(headers, "user-agent", CLAUDE_CODE_USER_AGENT)?;
    }
    Ok(())
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), AdapterError> {
    let value = HeaderValue::from_str(value)
        .map_err(|e| AdapterError::Internal(format!("invalid Anthropic Messages header: {e}")))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

fn collect_anthropic_beta_headers(body: &Map<String, Value>) -> BTreeSet<String> {
    let mut betas = BTreeSet::new();
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for tool in tools {
        let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
        if tool_type == "computer_20250124" {
            betas.insert(BETA_COMPUTER_USE_2025_01_24.to_owned());
        } else if tool_type == "computer_20241022" || tool_type.starts_with("computer_") {
            betas.insert(BETA_COMPUTER_USE_2024_10_22.to_owned());
        }
        if tool_type == "code_execution_20250825" {
            betas.insert(BETA_CODE_EXECUTION_2025_08_25.to_owned());
        } else if tool_type.starts_with("code_execution") {
            betas.insert(BETA_CODE_EXECUTION_2025_05_22.to_owned());
        }
        if tool_type.starts_with("web_fetch") {
            betas.insert(BETA_WEB_FETCH_2025_09_10.to_owned());
        }
        if tool_type.starts_with("memory") {
            betas.insert(BETA_CONTEXT_MANAGEMENT_2025_06_27.to_owned());
        }
        if matches!(
            tool_type,
            "tool_search_tool_regex_20251119" | "tool_search_tool_bm25_20251119"
        ) || tool_type.starts_with("tool_search_tool")
        {
            betas.insert(BETA_ADVANCED_TOOL_USE_2025_11_20.to_owned());
        }
        if tool_type == "advisor_20260301" {
            betas.insert(BETA_ADVISOR_TOOL_2026_03_01.to_owned());
        }
        if contains_code_execution_allowed_caller(tool)
            || has_non_empty_array(tool.get("input_examples"))
        {
            betas.insert(BETA_ADVANCED_TOOL_USE_2025_11_20.to_owned());
        }
    }
    if body
        .get("mcp_servers")
        .and_then(Value::as_array)
        .is_some_and(|servers| !servers.is_empty())
    {
        betas.insert(BETA_MCP_CLIENT_2025_04_04.to_owned());
    }
    collect_context_management_betas(body.get("context_management"), &mut betas);
    if messages_contain_file_id(body.get("messages")) {
        betas.insert(BETA_FILES_API_2025_04_14.to_owned());
        betas.insert(BETA_CODE_EXECUTION_2025_05_22.to_owned());
    }
    if body.get("output_format").is_some() {
        betas.insert(BETA_STRUCTURED_OUTPUTS_2025_11_13.to_owned());
    }
    if !is_adaptive_claude_model(model)
        && body
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .is_some()
    {
        betas.insert(BETA_EFFORT_2025_11_24.to_owned());
    }
    if body.get("speed").and_then(Value::as_str) == Some("fast") {
        betas.insert(BETA_FAST_MODE_2026_02_01.to_owned());
    }
    if body
        .get("container")
        .and_then(|v| v.get("skills"))
        .and_then(Value::as_array)
        .is_some_and(|skills| !skills.is_empty())
    {
        betas.insert(BETA_SKILLS_2025_10_02.to_owned());
    }
    betas
}

fn messages_contain_file_id(value: Option<&Value>) -> bool {
    value.is_some_and(value_contains_file_id)
}

fn value_contains_file_id(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(value_contains_file_id),
        Value::Object(obj) => {
            let has_file_source =
                obj.get("source")
                    .and_then(Value::as_object)
                    .is_some_and(|source| {
                        source.get("type").and_then(Value::as_str) == Some("file")
                            && source.get("file_id").and_then(Value::as_str).is_some()
                    });
            let is_file_block = obj.get("type").and_then(Value::as_str) == Some("file")
                && obj.get("file_id").and_then(Value::as_str).is_some();
            has_file_source || is_file_block || obj.values().any(value_contains_file_id)
        }
        _ => false,
    }
}

fn collect_context_management_betas(value: Option<&Value>, betas: &mut BTreeSet<String>) {
    let Some(value) = value else {
        return;
    };
    let edits = if let Some(edits) = value.get("edits").and_then(Value::as_array) {
        edits
    } else if let Some(entries) = value.as_array() {
        entries
    } else {
        return;
    };
    for edit in edits {
        match edit.get("type").and_then(Value::as_str).unwrap_or("") {
            "compact_20260112" | "compact_2026_01_12" | "compaction" => {
                betas.insert(BETA_COMPACT_2026_01_12.to_owned());
            }
            "" => {}
            _ => {
                betas.insert(BETA_CONTEXT_MANAGEMENT_2025_06_27.to_owned());
            }
        }
    }
}

fn contains_code_execution_allowed_caller(tool: &Value) -> bool {
    has_allowed_caller(tool.get("allowed_callers"))
        || has_allowed_caller(tool.pointer("/function/allowed_callers"))
}

fn has_allowed_caller(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| item.as_str() == Some("code_execution_20250825"))
    })
}

fn has_non_empty_array(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
}

fn apply_claude_code_compat_body(body: &mut Map<String, Value>) -> Result<String, AdapterError> {
    ensure_claude_code_system_prompt(body);
    let session_id = generate_uuid_v4()?;
    let device_id = random_hex(32)?;
    let user_id = json!({
        "device_id": device_id,
        "account_uuid": "",
        "session_id": session_id,
    })
    .to_string();
    let metadata = body
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    if let Some(metadata_obj) = metadata.as_object_mut() {
        metadata_obj.insert("user_id".into(), Value::String(user_id));
    }
    Ok(session_id)
}

fn ensure_claude_code_system_prompt(body: &mut Map<String, Value>) {
    let mut system_blocks = Vec::new();
    system_blocks.push(json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_PROMPT }));
    if let Some(existing) = body.remove("system") {
        match existing {
            Value::String(text) => {
                if !text.trim().is_empty() && text != CLAUDE_CODE_SYSTEM_PROMPT {
                    system_blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            Value::Array(items) => {
                let already_prefixed = items
                    .first()
                    .and_then(|v| v.get("text"))
                    .and_then(Value::as_str)
                    == Some(CLAUDE_CODE_SYSTEM_PROMPT);
                if already_prefixed {
                    body.insert("system".into(), Value::Array(items));
                    return;
                }
                system_blocks.extend(items);
            }
            other if !other.is_null() => {
                system_blocks.push(other);
            }
            _ => {}
        }
    }
    body.insert("system".into(), Value::Array(system_blocks));
}

fn generate_uuid_v4() -> Result<String, AdapterError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AdapterError::Internal(format!("random uuid generation failed: {e}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
}

fn random_hex(byte_len: usize) -> Result<String, AdapterError> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AdapterError::Internal(format!("random metadata generation failed: {e}")))?;
    let mut out = String::with_capacity(byte_len * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn ensure_max_tokens_exceeds_thinking_budget(body: &mut Map<String, Value>) {
    let Some(budget) = body
        .get("thinking")
        .and_then(|v| v.get("budget_tokens"))
        .and_then(Value::as_u64)
    else {
        return;
    };
    let current = value_to_positive_u64(body.get("max_tokens"));
    if current.map_or(true, |n| n <= budget) {
        body.insert(
            "max_tokens".into(),
            Value::Number((budget.saturating_add(1024)).into()),
        );
    }
}

fn drop_thinking_for_forced_tool_choice(body: &mut Map<String, Value>) {
    let forced_tool_choice = matches!(
        body.get("tool_choice")
            .and_then(Value::as_object)
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str),
        Some("any" | "tool")
    );
    if !forced_tool_choice || body.get("thinking").is_none() {
        return;
    }

    body.remove("thinking");
    let remove_output_config = body
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
        .map(|output_config| {
            output_config.remove("effort");
            output_config.is_empty()
        })
        .unwrap_or(false);
    if remove_output_config {
        body.remove("output_config");
    }
    tracing::warn!(
        "dropping Anthropic thinking because forced tool_choice(type=any/tool) is incompatible with extended thinking; preserving forced tool use"
    );
}

#[derive(Debug, Clone)]
struct DirectAnthropicMessagesRequestConversion {
    request: Value,
    response_session: ResponseSessionPlan,
    tool_name_maps: AnthropicToolNameMaps,
}

fn responses_body_to_anthropic_messages_wire(
    input: &Value,
    provider: &Provider,
    session_cache: Option<&ResponseSessionCache>,
    stream: bool,
) -> Result<DirectAnthropicMessagesRequestConversion, AdapterError> {
    let body = input
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("body must be a JSON object".into()))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AdapterError::BadRequest("model field required".into()))?;

    let converted_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| convert_responses_tools_to_anthropic(tools, provider))
        .transpose()?;
    let tool_name_maps = converted_tools
        .as_ref()
        .map(|converted| converted.name_maps.clone())
        .unwrap_or_default();
    let has_advisor_tool = converted_tools
        .as_ref()
        .is_some_and(|converted| converted.tools.iter().any(is_advisor_tool));

    let current_entries = responses_body_to_anthropic_conversation_entries(body, &tool_name_maps)?;
    let mut session_messages =
        merge_messages_with_previous_response(current_entries, input, session_cache)?;
    strip_advisor_blocks_if_absent(&mut session_messages, has_advisor_tool);
    repair_anthropic_tool_results(&mut session_messages)?;
    let has_container_upload = messages_contain_container_upload(&session_messages);

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_owned()));
    out.insert(
        "messages".into(),
        Value::Array(
            session_messages
                .iter()
                .filter(|message| !is_system_like_message(message))
                .cloned()
                .collect(),
        ),
    );
    out.insert("max_tokens".into(), max_tokens_for_anthropic(body));
    out.insert("stream".into(), Value::Bool(stream));

    if let Some(system) = collect_system_value(Some(&session_messages)) {
        out.insert("system".into(), system);
    }
    let mut direct_mcp_servers = body
        .get("mcp_servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(converted) = converted_tools {
        let ConvertedTools {
            mut tools,
            mcp_servers,
            ..
        } = converted;
        if has_container_upload {
            ensure_code_execution_tool_for_container_upload(&mut tools);
        }
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
        if !mcp_servers.is_empty() {
            direct_mcp_servers.extend(mcp_servers);
        }
    } else if has_container_upload {
        out.insert(
            "tools".into(),
            Value::Array(vec![json!({
                "type": "code_execution_20250522",
                "name": "code_execution",
            })]),
        );
    }
    if !direct_mcp_servers.is_empty() {
        out.insert("mcp_servers".into(), Value::Array(direct_mcp_servers));
    }
    if let Some(tool_choice) = convert_tool_choice(
        body.get("tool_choice"),
        body.get("parallel_tool_calls").and_then(Value::as_bool),
        &tool_name_maps,
    ) {
        out.insert("tool_choice".into(), tool_choice);
    }
    if let Some(stop_sequences) =
        convert_stop_sequences(body.get("stop_sequences").or_else(|| body.get("stop")))
    {
        out.insert("stop_sequences".into(), stop_sequences);
    }
    copy_if_present(body, &mut out, "temperature");
    copy_if_present(body, &mut out, "top_p");
    copy_if_present(body, &mut out, "top_k");
    copy_context_management_if_present(body, &mut out);
    copy_if_present(body, &mut out, "container");
    copy_if_present(body, &mut out, "output_config");
    copy_if_present(body, &mut out, "speed");
    copy_if_present(body, &mut out, "cache_control");
    copy_if_present(body, &mut out, "inference_geo");
    if let Some(output_format) = body.get("output_format").filter(|v| v.is_object()) {
        out.insert("output_format".into(), output_format.clone());
    } else if let Some(output_format) = body
        .get("text")
        .and_then(responses_text_to_anthropic_output_format)
    {
        out.insert("output_format".into(), output_format);
    }
    if let Some(thinking) = convert_responses_thinking(body) {
        out.insert("thinking".into(), thinking);
    }
    apply_adaptive_thinking_for_model(model, body, &mut out);
    if let Some(metadata) = convert_metadata(body) {
        out.insert("metadata".into(), metadata);
    }

    Ok(DirectAnthropicMessagesRequestConversion {
        request: Value::Object(out),
        response_session: ResponseSessionPlan {
            response_id: response_id_for_session(),
            messages: session_messages,
        },
        tool_name_maps,
    })
}

fn is_system_like_message(message: &Value) -> bool {
    matches!(
        message.get("role").and_then(Value::as_str),
        Some("system" | "developer")
    )
}

fn responses_body_to_anthropic_conversation_entries(
    body: &Map<String, Value>,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Vec<Value>, AdapterError> {
    let mut entries = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        if let Some(message) = instructions_to_system_message(instructions) {
            entries.push(message);
        }
    }
    let input_entries = body
        .get("input")
        .map(|input| responses_input_to_anthropic_entries(input, tool_names))
        .transpose()?
        .unwrap_or_default();
    entries.extend(input_entries);
    Ok(entries)
}

fn instructions_to_system_message(instructions: &Value) -> Option<Value> {
    let content = match instructions {
        Value::Null => Value::String(String::new()),
        Value::String(text) => Value::String(text.clone()),
        Value::Array(_) => instructions.clone(),
        Value::Object(obj) => obj
            .get("content")
            .cloned()
            .or_else(|| obj.get("text").cloned())
            .or_else(|| {
                obj.get("type")
                    .and_then(Value::as_str)
                    .map(|_| Value::Array(vec![instructions.clone()]))
            })
            .unwrap_or_else(|| Value::String(value_to_string(instructions))),
        other => Value::String(value_to_string(other)),
    };
    match &content {
        Value::String(text) if text.trim().is_empty() => None,
        Value::Array(items) if items.is_empty() => None,
        Value::Null => None,
        _ => Some(json!({ "role": "system", "content": content })),
    }
}

fn responses_input_to_anthropic_entries(
    input: &Value,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Vec<Value>, AdapterError> {
    let items = extract_responses_input_items(input);
    let mut entries = Vec::new();
    let mut pending_reasoning = Vec::new();

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type == "reasoning" {
            pending_reasoning.extend(reasoning_item_to_thinking_blocks(obj));
            continue;
        }

        let mut item_entries = responses_input_item_to_anthropic_entries(obj, tool_names)?;
        if !pending_reasoning.is_empty() {
            if let Some(first_assistant) = item_entries
                .iter_mut()
                .find(|entry| entry.get("role").and_then(Value::as_str) == Some("assistant"))
            {
                prepend_blocks(first_assistant, &mut pending_reasoning);
            } else {
                entries.push(json!({
                    "role": "assistant",
                    "content": std::mem::take(&mut pending_reasoning),
                }));
            }
        }
        entries.extend(item_entries);
    }

    if !pending_reasoning.is_empty() {
        entries.push(json!({
            "role": "assistant",
            "content": pending_reasoning,
        }));
    }

    Ok(entries)
}

fn extract_responses_input_items(input: &Value) -> Vec<Value> {
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
                    "role": obj.get("role").and_then(Value::as_str).unwrap_or("user"),
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
                    "role": obj.get("role").and_then(Value::as_str).unwrap_or("user"),
                    "content": obj.get("content").cloned().unwrap_or_else(|| Value::Object(obj.clone())),
                })),
                Value::String(s) => Some(json!({ "type": "message", "role": "user", "content": s })),
                other => Some(json!({ "type": "message", "role": "user", "content": value_to_string(other) })),
            })
            .collect(),
        other => vec![json!({ "type": "message", "role": "user", "content": value_to_string(other) })],
    }
}

fn responses_input_item_to_anthropic_entries(
    item: &Map<String, Value>,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Vec<Value>, AdapterError> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "message" => responses_message_item_to_anthropic_entries(item, tool_names),
        "function_call" => Ok(vec![function_call_item_to_assistant_message(
            item, tool_names,
        )?]),
        "function_call_output" => Ok(vec![function_call_output_item_to_user_message(item)]),
        "input_image" => Ok(input_image_item_to_block(item)
            .map(|block| vec![json!({ "role": "user", "content": [block] })])
            .unwrap_or_default()),
        "input_file" => Ok(input_file_item_to_block(item)
            .map(|block| vec![json!({ "role": "user", "content": [block] })])
            .unwrap_or_default()),
        "input_audio" => Ok(vec![json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": format!(
                    "[Audio input: format={}]",
                    item.get("format").and_then(Value::as_str).unwrap_or("unknown")
                ),
            }],
        })]),
        "input_video" => Ok(vec![json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": "[Video input]",
            }],
        })]),
        "compaction" | "context_compaction" | "compaction_summary" => {
            let summary = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            Ok(if summary.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "role": "user", "content": [{"type": "text", "text": summary}] })]
            })
        }
        "file_search_call"
        | "web_search_call"
        | "computer_call"
        | "code_interpreter_call"
        | "image_generation_call" => Ok(vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": format!("[{item_type}]")}],
        })]),
        _ => {
            if let Some(content) = item.get("content") {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let blocks = match role {
                    "assistant" => assistant_blocks_from_responses_content(content, tool_names)?,
                    "system" | "developer" => text_blocks_from_any_content(content),
                    _ => user_blocks_from_responses_content(content),
                };
                Ok((!blocks.is_empty())
                    .then(|| {
                        let mut message = Map::new();
                        message.insert("role".into(), Value::String(role.to_owned()));
                        message.insert("content".into(), Value::Array(blocks));
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            message.insert("name".into(), Value::String(name.to_owned()));
                        }
                        Value::Object(message)
                    })
                    .into_iter()
                    .collect())
            } else {
                Ok(Vec::new())
            }
        }
    }
}

fn responses_message_item_to_anthropic_entries(
    item: &Map<String, Value>,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Vec<Value>, AdapterError> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = item.get("content").unwrap_or(&Value::Null);
    let mut blocks = match role {
        "assistant" => assistant_blocks_from_responses_content(content, tool_names)?,
        "system" | "developer" => text_blocks_from_any_content(content),
        "tool" => {
            let mut synthetic = Map::from_iter([
                (
                    "call_id".to_owned(),
                    item.get("tool_call_id")
                        .or_else(|| item.get("call_id"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                ("output".to_owned(), content.clone()),
            ]);
            for key in ["is_error", "cache_control"] {
                if let Some(value) = item.get(key) {
                    synthetic.insert(key.to_owned(), value.clone());
                }
            }
            return Ok(vec![function_call_output_item_to_user_message(&synthetic)]);
        }
        _ => user_blocks_from_responses_content(content),
    };
    if role == "assistant" {
        for block in item
            .get("anthropic_thinking_blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(raw) = anthropic_thinking_block_for_messages(block) {
                blocks.insert(0, raw);
            }
        }
    }
    for key in ["is_error", "cache_control"] {
        if let Some(value) = item.get(key) {
            for block in &mut blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    if let Some(obj) = block.as_object_mut() {
                        obj.insert(key.to_owned(), value.clone());
                    }
                }
            }
        }
    }
    Ok((!blocks.is_empty())
        .then(|| {
            let mut message = Map::new();
            message.insert("role".into(), Value::String(role.to_owned()));
            message.insert("content".into(), Value::Array(blocks));
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                message.insert("name".into(), Value::String(name.to_owned()));
            }
            Value::Object(message)
        })
        .into_iter()
        .collect())
}

fn user_blocks_from_responses_content(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => text_block_vec(s),
        Value::Array(items) => items
            .iter()
            .flat_map(user_block_from_responses_part)
            .collect(),
        Value::Object(_) => user_block_from_responses_part(content),
        Value::Null => Vec::new(),
        other => text_block_vec(&value_to_string(other)),
    }
}

fn user_block_from_responses_part(part: &Value) -> Vec<Value> {
    let Some(obj) = part.as_object() else {
        let text = value_to_string(part);
        return text_block_vec(&text);
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" | "input_text" | "output_text" => obj
            .get("text")
            .and_then(Value::as_str)
            .and_then(|text| text_block_from_source(text, obj))
            .into_iter()
            .collect(),
        "input_image" | "image_url" => input_image_item_to_block(obj).into_iter().collect(),
        "image" | "document" | "container_upload" | "tool_result" => {
            vec![Value::Object(obj.clone())]
        }
        "input_file" => input_file_item_to_block(obj).into_iter().collect(),
        _ => {
            let text = content_block_to_text(part);
            text_block_vec(&text)
        }
    }
}

fn assistant_blocks_from_responses_content(
    content: &Value,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Vec<Value>, AdapterError> {
    match content {
        Value::String(s) => Ok(text_block_vec(s)),
        Value::Array(items) => {
            let mut blocks = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    let text = value_to_string(item);
                    blocks.extend(text_block_vec(&text));
                    continue;
                };
                match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" | "output_text" | "input_text" => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            blocks.extend(text_block_from_source(text, obj));
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        if let Some(block) = anthropic_thinking_block_for_messages(item) {
                            blocks.push(block);
                        }
                    }
                    "tool_use" => blocks.push(sanitize_tool_use_block(obj, tool_names)?),
                    "server_tool_use"
                    | "web_search_tool_result"
                    | "web_fetch_tool_result"
                    | "tool_search_tool_result"
                    | "advisor_tool_result"
                    | "compaction" => blocks.push(Value::Object(obj.clone())),
                    other if other.ends_with("_tool_result") => {
                        blocks.push(Value::Object(obj.clone()))
                    }
                    _ => {
                        let text = content_block_to_text(item);
                        blocks.extend(text_block_vec(&text));
                    }
                }
            }
            Ok(blocks)
        }
        Value::Object(obj) if obj.get("type").and_then(Value::as_str) == Some("tool_use") => {
            Ok(vec![sanitize_tool_use_block(obj, tool_names)?])
        }
        Value::Null => Ok(Vec::new()),
        other => Ok(text_block_vec(&value_to_string(other))),
    }
}

fn text_blocks_from_any_content(content: &Value) -> Vec<Value> {
    let text = content_to_text(content);
    text_block_vec(&text)
}

fn sanitize_tool_use_block(
    obj: &Map<String, Value>,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Value, AdapterError> {
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("call_unknown");
    let original_name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AdapterError::BadRequest("tool_use missing name".into()))?;
    let name = tool_names
        .forward
        .get(original_name)
        .map(String::as_str)
        .unwrap_or(original_name);
    let mut block = Map::new();
    block.insert("type".into(), Value::String("tool_use".into()));
    block.insert("id".into(), Value::String(id.to_owned()));
    block.insert("name".into(), Value::String(name.to_owned()));
    block.insert(
        "input".into(),
        obj.get("input").cloned().unwrap_or_else(|| json!({})),
    );
    copy_anthropic_tool_use_extension_fields(obj, &mut block);
    Ok(Value::Object(block))
}

fn function_call_item_to_assistant_message(
    item: &Map<String, Value>,
    tool_names: &AnthropicToolNameMaps,
) -> Result<Value, AdapterError> {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or("call_unknown");
    let original_name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AdapterError::BadRequest("function_call missing name for Anthropic Messages".to_owned())
        })?;
    let name = tool_names
        .forward
        .get(original_name)
        .map(String::as_str)
        .unwrap_or(original_name);
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let mut block = Map::new();
    block.insert("type".into(), Value::String("tool_use".into()));
    block.insert("id".into(), Value::String(call_id.to_owned()));
    block.insert("name".into(), Value::String(name.to_owned()));
    block.insert("input".into(), parse_tool_arguments(arguments)?);
    copy_anthropic_tool_use_extension_fields(item, &mut block);
    Ok(json!({
        "role": "assistant",
        "content": [Value::Object(block)],
    }))
}

fn function_call_output_item_to_user_message(item: &Map<String, Value>) -> Value {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("tool_call_id").and_then(Value::as_str))
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or("");
    let output = item
        .get("output")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let mut block = Map::new();
    block.insert("type".into(), Value::String("tool_result".into()));
    block.insert("tool_use_id".into(), Value::String(call_id.to_owned()));
    block.insert(
        "content".into(),
        tool_result_content_for_anthropic(&output_for_tool_result(call_id, output)),
    );
    if let Some(is_error) = item.get("is_error").and_then(Value::as_bool) {
        block.insert("is_error".into(), Value::Bool(is_error));
    }
    if let Some(cache_control) = item.get("cache_control") {
        block.insert("cache_control".into(), cache_control.clone());
    }
    json!({ "role": "user", "content": [Value::Object(block)] })
}

fn output_for_tool_result(call_id: &str, output: Value) -> Value {
    match output {
        Value::Array(_) => output,
        Value::String(_) => Value::String(normalize_tool_output_for_context_with_store(
            Some(call_id),
            output,
            Some(global_tool_artifact_store()),
        )),
        other => Value::String(normalize_tool_output_for_context_with_store(
            Some(call_id),
            other,
            Some(global_tool_artifact_store()),
        )),
    }
}

fn input_image_item_to_block(item: &Map<String, Value>) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) == Some("image") && item.get("source").is_some() {
        return Some(Value::Object(item.clone()));
    }
    let image_url = item
        .get("image_url")
        .or_else(|| item.get("url"))
        .or_else(|| item.get("image"));
    let mut block = image_url_to_anthropic_block(image_url)?;
    if let Some(obj) = block.as_object_mut() {
        copy_cache_control(item, obj);
    }
    Some(block)
}

fn input_file_item_to_block(item: &Map<String, Value>) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) == Some("document") && item.get("source").is_some()
    {
        return Some(Value::Object(item.clone()));
    }
    if let Some(file_id) = item
        .get("file_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
    {
        let mut block = Map::new();
        block.insert("type".into(), Value::String("document".into()));
        block.insert(
            "source".into(),
            json!({ "type": "file", "file_id": file_id }),
        );
        if let Some(title) = item
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| item.get("filename").and_then(Value::as_str))
        {
            block.insert("title".into(), Value::String(title.to_owned()));
        }
        for key in ["context", "citations"] {
            if let Some(value) = item.get(key) {
                block.insert(key.to_owned(), value.clone());
            }
        }
        copy_cache_control(item, &mut block);
        return Some(Value::Object(block));
    }
    if let Some(data) = item
        .get("file_data")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let media_type = item
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        let mut block = Map::new();
        block.insert("type".into(), Value::String("document".into()));
        block.insert(
            "source".into(),
            json!({
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }),
        );
        if let Some(title) = item
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| item.get("filename").and_then(Value::as_str))
        {
            block.insert("title".into(), Value::String(title.to_owned()));
        }
        for key in ["context", "citations"] {
            if let Some(value) = item.get(key) {
                block.insert(key.to_owned(), value.clone());
            }
        }
        copy_cache_control(item, &mut block);
        return Some(Value::Object(block));
    }
    item.get("filename")
        .and_then(Value::as_str)
        .map(|filename| json!({ "type": "text", "text": format!("[File: {filename}]") }))
}

fn reasoning_item_to_thinking_blocks(item: &Map<String, Value>) -> Vec<Value> {
    if let Some(block) = item.get("anthropic_thinking").filter(|v| v.is_object()) {
        return anthropic_thinking_block_for_messages(block)
            .into_iter()
            .collect();
    }
    let mut parts = Vec::new();
    if let Some(summaries) = item.get("summary").and_then(Value::as_array) {
        for summary in summaries {
            if let Some(text) = summary.as_str() {
                if !text.trim().is_empty() {
                    parts.push(text.to_owned());
                }
                continue;
            }
            if let Some(text) = summary.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.to_owned());
                }
            }
        }
    }
    if parts.is_empty() {
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for block in content {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        parts.push(text.to_owned());
                    }
                }
            }
        }
    }
    (!parts.is_empty())
        .then(|| json!({ "type": "thinking", "thinking": parts.join("\n") }))
        .into_iter()
        .collect()
}

fn prepend_blocks(message: &mut Value, blocks: &mut Vec<Value>) {
    let Some(obj) = message.as_object_mut() else {
        return;
    };
    let content = obj
        .entry("content")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !content.is_array() {
        let old = std::mem::replace(content, Value::Array(Vec::new()));
        *content = Value::Array(text_block_vec(&content_to_text(&old)));
    }
    if let Some(arr) = content.as_array_mut() {
        let mut prefix = std::mem::take(blocks);
        prefix.append(arr);
        *arr = prefix;
    }
}

fn repair_anthropic_tool_results(messages: &mut Vec<Value>) -> Result<(), AdapterError> {
    let mut known_tool_use_ids: BTreeSet<String> = BTreeSet::new();
    let mut repaired = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            collect_tool_use_ids_from_message(&message, &mut known_tool_use_ids);
            repaired.push(message);
            continue;
        }
        if message.get("role").and_then(Value::as_str) == Some("user") {
            let missing = missing_tool_result_ids(&message, &known_tool_use_ids);
            for call_id in missing {
                let Some(entry) = global_tool_call_cache().get(&call_id) else {
                    return Err(AdapterError::BadRequest(format!(
                        "tool result references unknown tool_use id {call_id}"
                    )));
                };
                let input = parse_tool_arguments(&entry.arguments)?;
                repaired.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": call_id,
                        "name": entry.name,
                        "input": input,
                    }],
                }));
                collect_tool_use_ids_from_message(
                    repaired.last().expect("just pushed assistant"),
                    &mut known_tool_use_ids,
                );
            }
        }
        repaired.push(message);
    }
    *messages = repaired;
    Ok(())
}

fn collect_tool_use_ids_from_message(message: &Value, known: &mut BTreeSet<String>) {
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                known.insert(id.to_owned());
            }
        }
    }
}

fn missing_tool_result_ids(message: &Value, known: &BTreeSet<String>) -> Vec<String> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .filter(|id| !known.contains(*id))
        .map(str::to_owned)
        .collect()
}

fn copy_if_present(body: &Map<String, Value>, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = body.get(key) {
        out.insert(key.to_owned(), value.clone());
    }
}

fn copy_context_management_if_present(body: &Map<String, Value>, out: &mut Map<String, Value>) {
    let Some(value) = body.get("context_management") else {
        return;
    };
    if let Some(mapped) = map_context_management_to_anthropic(value) {
        out.insert("context_management".into(), mapped);
    }
}

fn map_context_management_to_anthropic(value: &Value) -> Option<Value> {
    if value
        .as_object()
        .is_some_and(|obj| obj.get("edits").is_some())
    {
        return Some(value.clone());
    }
    let Value::Array(entries) = value else {
        return None;
    };
    let mut edits = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        let mut edit = Map::new();
        edit.insert("type".into(), Value::String("compact_20260112".into()));
        if let Some(threshold) = obj.get("compact_threshold").and_then(value_to_i64) {
            edit.insert(
                "trigger".into(),
                json!({ "type": "input_tokens", "value": threshold }),
            );
        }
        for (key, value) in obj {
            if matches!(key.as_str(), "type" | "compact_threshold") {
                continue;
            }
            edit.insert(key.clone(), value.clone());
        }
        edits.push(Value::Object(edit));
    }
    (!edits.is_empty()).then(|| json!({ "edits": edits }))
}

fn value_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| value.as_f64().map(|f| f.round() as i64))
}

fn responses_text_to_anthropic_output_format(value: &Value) -> Option<Value> {
    let format = value.get("format").filter(|v| v.is_object())?;
    if format.get("type").and_then(Value::as_str) == Some("text") {
        return None;
    }
    let schema = format
        .get("schema")
        .or_else(|| format.get("json_schema").and_then(|v| v.get("schema")))?;
    Some(json!({
        "type": "json_schema",
        "schema": filter_anthropic_output_schema(schema),
    }))
}

fn filter_anthropic_output_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(obj) => {
            let mut result = Map::new();
            let mut constraints = Vec::new();
            for key in [
                "minItems",
                "maxItems",
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "minLength",
                "maxLength",
            ] {
                if let Some(value) = obj.get(key) {
                    constraints.push(format!(
                        "{}: {}",
                        anthropic_constraint_label(key),
                        value_to_string(value)
                    ));
                }
            }
            for (key, value) in obj {
                if matches!(
                    key.as_str(),
                    "minItems"
                        | "maxItems"
                        | "minimum"
                        | "maximum"
                        | "exclusiveMinimum"
                        | "exclusiveMaximum"
                        | "minLength"
                        | "maxLength"
                ) {
                    continue;
                }
                let filtered = match key.as_str() {
                    "properties" | "$defs" | "definitions" => filter_schema_map(value),
                    "items" => filter_anthropic_output_schema(value),
                    "anyOf" | "allOf" | "oneOf" => filter_schema_array(value),
                    _ => value.clone(),
                };
                result.insert(key.clone(), filtered);
            }
            if !constraints.is_empty() {
                let note = format!("Note: {}.", constraints.join(", "));
                match result.get_mut("description") {
                    Some(Value::String(desc)) if !desc.trim().is_empty() => {
                        desc.push(' ');
                        desc.push_str(&note);
                    }
                    _ => {
                        result.insert("description".into(), Value::String(note));
                    }
                }
            }
            if result.get("type").and_then(Value::as_str) == Some("object")
                && !result.contains_key("additionalProperties")
            {
                result.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(result)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(filter_anthropic_output_schema)
                .collect::<Vec<_>>(),
        ),
        other => other.clone(),
    }
}

fn filter_schema_map(value: &Value) -> Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    Value::Object(
        obj.iter()
            .map(|(key, value)| (key.clone(), filter_anthropic_output_schema(value)))
            .collect(),
    )
}

fn filter_schema_array(value: &Value) -> Value {
    let Some(items) = value.as_array() else {
        return value.clone();
    };
    Value::Array(
        items
            .iter()
            .map(filter_anthropic_output_schema)
            .collect::<Vec<_>>(),
    )
}

fn anthropic_constraint_label(key: &str) -> &'static str {
    match key {
        "minItems" => "minimum number of items",
        "maxItems" => "maximum number of items",
        "minimum" => "minimum value",
        "maximum" => "maximum value",
        "exclusiveMinimum" => "exclusive minimum value",
        "exclusiveMaximum" => "exclusive maximum value",
        "minLength" => "minimum length",
        "maxLength" => "maximum length",
        _ => "constraint",
    }
}

fn max_tokens_for_anthropic(body: &Map<String, Value>) -> Value {
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(n) = value_to_positive_u64(body.get(key)) {
            return Value::Number(n.into());
        }
    }
    Value::Number(DEFAULT_MAX_TOKENS.into())
}

fn value_to_positive_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.round().max(1.0) as u64)),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
    .map(|n| n.max(1))
}

fn collect_system_value(messages: Option<&Vec<Value>>) -> Option<Value> {
    let mut text_parts = Vec::new();
    let mut block_parts = Vec::new();
    let mut saw_structured_content = false;
    for msg in messages.into_iter().flatten() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(role, "system" | "developer") {
            let content = msg.get("content").unwrap_or(&Value::Null);
            match content {
                Value::String(text) => {
                    if !text.trim().is_empty() && !is_anthropic_billing_header_text(text) {
                        text_parts.push(text.clone());
                    }
                }
                Value::Array(_) | Value::Object(_) => {
                    saw_structured_content = true;
                    if !text_parts.is_empty() {
                        block_parts.extend(text_parts.drain(..).map(|text| {
                            json!({
                                "type": "text",
                                "text": text,
                            })
                        }));
                    }
                    block_parts.extend(system_blocks_from_content(content));
                }
                Value::Null => {}
                other => {
                    let text = value_to_string(other);
                    if !text.trim().is_empty() && !is_anthropic_billing_header_text(&text) {
                        text_parts.push(text);
                    }
                }
            }
        }
    }
    if saw_structured_content {
        if !text_parts.is_empty() {
            block_parts.extend(text_parts.drain(..).map(|text| {
                json!({
                    "type": "text",
                    "text": text,
                })
            }));
        }
        (!block_parts.is_empty()).then(|| Value::Array(block_parts))
    } else if text_parts.is_empty() {
        None
    } else {
        Some(Value::String(text_parts.join("\n\n")))
    }
}

fn system_blocks_from_content(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) if is_anthropic_billing_header_text(text) => Vec::new(),
        Value::String(text) => text_block_vec(text),
        Value::Array(items) => items.iter().filter_map(system_block_from_part).collect(),
        Value::Object(_) => system_block_from_part(content).into_iter().collect(),
        Value::Null => Vec::new(),
        other => text_block_vec(&value_to_string(other)),
    }
}

fn system_block_from_part(part: &Value) -> Option<Value> {
    let obj = match part {
        Value::String(text) => return text_block_vec(text).into_iter().next(),
        Value::Object(obj) => obj,
        Value::Null => return None,
        other => return text_block_vec(&value_to_string(other)).into_iter().next(),
    };
    let text = obj
        .get("text")
        .or_else(|| obj.get("content"))
        .and_then(Value::as_str)?;
    if text.trim().is_empty() {
        return None;
    }
    if is_anthropic_billing_header_text(text) {
        return None;
    }
    let mut block = Map::new();
    block.insert("type".into(), Value::String("text".into()));
    block.insert("text".into(), Value::String(text.to_owned()));
    copy_cache_control(obj, &mut block);
    Some(Value::Object(block))
}

fn anthropic_thinking_block_for_messages(block: &Value) -> Option<Value> {
    let obj = block.as_object()?;
    match obj.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let thinking = obj.get("thinking").and_then(Value::as_str)?;
            let mut out = Map::new();
            out.insert("type".into(), Value::String("thinking".into()));
            out.insert("thinking".into(), Value::String(thinking.to_owned()));
            if let Some(signature) = obj.get("signature").and_then(Value::as_str) {
                out.insert("signature".into(), Value::String(signature.to_owned()));
            }
            copy_cache_control(obj, &mut out);
            Some(Value::Object(out))
        }
        Some("redacted_thinking") => {
            let data = obj.get("data").and_then(Value::as_str)?;
            let mut out = Map::new();
            out.insert("type".into(), Value::String("redacted_thinking".to_owned()));
            out.insert("data".into(), Value::String(data.to_owned()));
            copy_cache_control(obj, &mut out);
            Some(Value::Object(out))
        }
        _ => None,
    }
}

fn text_block_from_source(text: &str, source: &Map<String, Value>) -> Option<Value> {
    if text.trim().is_empty() {
        return None;
    }
    let mut block = Map::new();
    block.insert("type".into(), Value::String("text".into()));
    block.insert("text".into(), Value::String(text.to_owned()));
    copy_cache_control(source, &mut block);
    Some(Value::Object(block))
}

fn text_block_vec(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

fn copy_cache_control(source: &Map<String, Value>, target: &mut Map<String, Value>) {
    if let Some(cache_control) = source.get("cache_control") {
        target.insert("cache_control".into(), cache_control.clone());
    }
}

fn is_anthropic_billing_header_text(text: &str) -> bool {
    text.starts_with("x-anthropic-billing-header:")
}

fn copy_anthropic_tool_use_extension_fields(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
) {
    for key in ["cache_control", "caller"] {
        if let Some(value) = source.get(key) {
            target.insert(key.to_owned(), value.clone());
        }
    }
}

fn tool_result_content_for_anthropic(content: &Value) -> Value {
    match content {
        Value::Array(items) => {
            let mut blocks = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    let text = value_to_string(item);
                    if !text.trim().is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    continue;
                };
                match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                let mut block = Map::new();
                                block.insert("type".into(), Value::String("text".into()));
                                block.insert("text".into(), Value::String(text.to_owned()));
                                copy_cache_control(obj, &mut block);
                                blocks.push(Value::Object(block));
                            }
                        }
                    }
                    "image_url" => {
                        if let Some(mut block) = image_url_to_anthropic_block(obj.get("image_url"))
                        {
                            if let Some(block_obj) = block.as_object_mut() {
                                copy_cache_control(obj, block_obj);
                            }
                            blocks.push(block);
                        }
                    }
                    "document" => blocks.push(Value::Object(obj.clone())),
                    _ => {
                        let text = content_block_to_text(item);
                        if !text.trim().is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                }
            }
            Value::Array(blocks)
        }
        Value::String(s) => Value::String(s.clone()),
        Value::Null => Value::String(String::new()),
        other => Value::String(value_to_string(other)),
    }
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(content_block_to_text)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => value_to_string(other),
    }
}

fn content_block_to_text(block: &Value) -> String {
    if let Some(obj) = block.as_object() {
        for key in ["text", "content"] {
            if let Some(text) = obj.get(key).and_then(|v| v.as_str()) {
                return text.to_owned();
            }
        }
    }
    value_to_string(block)
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn image_url_to_anthropic_block(image_url: Option<&Value>) -> Option<Value> {
    let url = match image_url? {
        Value::String(s) => s.as_str(),
        Value::Object(obj) => obj.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        _ => "",
    };
    if url.trim().is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let (media_type, data) = rest.split_once(";base64,")?;
        return Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }));
    }
    Some(json!({
        "type": "image",
        "source": {
            "type": "url",
            "url": url,
        }
    }))
}

fn parse_tool_arguments(arguments: &str) -> Result<Value, AdapterError> {
    let parsed: Value = serde_json::from_str(arguments).map_err(|e| {
        AdapterError::BadRequest(format!("tool_call arguments are not valid JSON: {e}"))
    })?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Ok(json!({ "input": parsed }))
    }
}

struct ConvertedTools {
    tools: Vec<Value>,
    mcp_servers: Vec<Value>,
    name_maps: AnthropicToolNameMaps,
}

fn messages_contain_container_upload(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("container_upload")
                })
            })
    })
}

fn strip_advisor_blocks_if_absent(messages: &mut Vec<Value>, has_advisor_tool: bool) {
    if has_advisor_tool {
        return;
    }
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        content.retain(|block| !is_advisor_history_block(block));
    }
    messages.retain(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .map_or(true, |content| !content.is_empty())
    });
}

fn is_advisor_history_block(block: &Value) -> bool {
    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
    block_type == "advisor_tool_result"
        || (block_type == "server_tool_use"
            && block.get("name").and_then(Value::as_str) == Some("advisor"))
}

fn is_advisor_tool(tool: &Value) -> bool {
    tool.get("type").and_then(Value::as_str) == Some("advisor_20260301")
        || tool.get("name").and_then(Value::as_str) == Some("advisor")
}

fn ensure_code_execution_tool_for_container_upload(tools: &mut Vec<Value>) {
    let already_present = tools.iter().any(|tool| {
        tool.get("type")
            .and_then(Value::as_str)
            .is_some_and(|tool_type| tool_type.starts_with("code_execution"))
    });
    if !already_present {
        tools.push(json!({
            "type": "code_execution_20250522",
            "name": "code_execution",
        }));
    }
}

fn convert_responses_tools_to_anthropic(
    tools: &[Value],
    provider: &Provider,
) -> Result<ConvertedTools, AdapterError> {
    let mut original_names = Vec::new();
    collect_responses_tool_names(tools, &mut original_names);
    let name_maps = build_tool_name_maps(&original_names);
    let mut converted = Vec::new();
    let mut mcp_servers = Vec::new();
    for tool in tools {
        convert_responses_tool_to_anthropic(
            tool,
            provider,
            &name_maps,
            None,
            &mut converted,
            &mut mcp_servers,
        )?;
    }
    Ok(ConvertedTools {
        tools: converted,
        mcp_servers,
        name_maps,
    })
}

fn collect_responses_tool_names(tools: &[Value], names: &mut Vec<String>) {
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "function" | "custom" => {
                if let Some(name) = obj.get("name").and_then(Value::as_str) {
                    if !name.trim().is_empty() {
                        names.push(name.to_owned());
                    }
                }
            }
            "namespace" => {
                if let Some(inner) = obj.get("tools").and_then(Value::as_array) {
                    collect_responses_tool_names(inner, names);
                }
            }
            _ => {}
        }
    }
}

fn convert_responses_tool_to_anthropic(
    tool: &Value,
    provider: &Provider,
    name_maps: &AnthropicToolNameMaps,
    namespace_prefix: Option<&str>,
    converted: &mut Vec<Value>,
    mcp_servers: &mut Vec<Value>,
) -> Result<(), AdapterError> {
    let Some(obj) = tool.as_object() else {
        return Ok(());
    };
    let Some(tool_type) = obj.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    match tool_type {
        "function" => {
            if let Some(tool) =
                responses_function_tool_to_anthropic(obj, name_maps, namespace_prefix)
            {
                converted.push(tool);
            }
        }
        "custom" => {
            if let Some(tool) = responses_custom_tool_to_anthropic(obj, name_maps, namespace_prefix)
            {
                converted.push(tool);
            }
        }
        "namespace" => {
            let Some(inner) = obj.get("tools").and_then(Value::as_array) else {
                tracing::debug!(
                    namespace_name = ?obj.get("name").and_then(|value| value.as_str()),
                    "dropping namespace tool with no nested `tools` array"
                );
                return Ok(());
            };
            let prefix = namespace_description_prefix(obj);
            for inner_tool in inner {
                convert_responses_tool_to_anthropic(
                    inner_tool,
                    provider,
                    name_maps,
                    prefix.as_deref(),
                    converted,
                    mcp_servers,
                )?;
            }
        }
        "web_search" | "web_search_preview" => {
            if let Some(tool) = responses_web_search_tool_to_anthropic(obj, provider) {
                converted.push(tool);
            }
        }
        "computer_use_preview" | "computer_use" | "computer" => {
            if let Some(tool) = responses_computer_tool_to_anthropic(obj) {
                converted.push(tool);
            } else {
                crate::warn_once_drop_tool(tool_type);
            }
        }
        "mcp" => {
            if let Some(server) = responses_mcp_tool_to_anthropic_server(obj) {
                mcp_servers.push(server);
            } else {
                crate::warn_once_drop_tool("mcp");
            }
        }
        other if is_anthropic_hosted_tool_type(other) => {
            let mut out = obj.clone();
            out.entry("name")
                .or_insert_with(|| Value::String(default_anthropic_hosted_tool_name(other).into()));
            converted.push(Value::Object(out));
        }
        other => {
            crate::warn_once_drop_tool(other);
        }
    }
    Ok(())
}

fn responses_function_tool_to_anthropic(
    obj: &Map<String, Value>,
    name_maps: &AnthropicToolNameMaps,
    namespace_prefix: Option<&str>,
) -> Option<Value> {
    let name = obj.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let sanitized_name = name_maps
        .forward
        .get(name)
        .map(String::as_str)
        .unwrap_or(name);
    let description = description_with_namespace_prefix(
        obj.get("description").and_then(Value::as_str).unwrap_or(""),
        namespace_prefix,
    );
    let input_schema =
        normalize_anthropic_input_schema(obj.get("parameters").cloned(), obj.get("strict"));
    let mut out = Map::new();
    out.insert("name".into(), Value::String(sanitized_name.to_owned()));
    if !description.trim().is_empty() {
        out.insert("description".into(), Value::String(description));
    }
    out.insert("input_schema".into(), input_schema);
    copy_responses_tool_extension_fields(obj, &mut out);
    Some(Value::Object(out))
}

fn responses_custom_tool_to_anthropic(
    obj: &Map<String, Value>,
    name_maps: &AnthropicToolNameMaps,
    namespace_prefix: Option<&str>,
) -> Option<Value> {
    let name = obj.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let sanitized_name = name_maps
        .forward
        .get(name)
        .map(String::as_str)
        .unwrap_or(name);
    let description = custom_tool_description_with_format(
        obj.get("description").and_then(Value::as_str).unwrap_or(""),
        obj.get("format"),
    );
    let description = description_with_namespace_prefix(&description, namespace_prefix);
    let mut out = Map::new();
    out.insert("name".into(), Value::String(sanitized_name.to_owned()));
    if !description.trim().is_empty() {
        out.insert("description".into(), Value::String(description));
    }
    out.insert(
        "input_schema".into(),
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": custom_tool_input_description(obj.get("format")),
                }
            },
            "required": ["input"],
        }),
    );
    copy_responses_tool_extension_fields(obj, &mut out);
    Some(Value::Object(out))
}

fn responses_web_search_tool_to_anthropic(
    obj: &Map<String, Value>,
    provider: &Provider,
) -> Option<Value> {
    if !provider_web_search_enabled(provider) {
        crate::warn_once_drop_tool("web_search:disabled-by-config");
        return None;
    }
    if crate::is_web_search_disabled_for(&provider.id) {
        crate::warn_once_drop_tool("web_search:auto-disabled-after-failure");
        return None;
    }
    let mut out = Map::new();
    out.insert("type".into(), Value::String("web_search_20250305".into()));
    out.insert("name".into(), Value::String("web_search".into()));
    if let Some(max_uses) = anthropic_web_search_max_uses(obj.get("search_context_size")) {
        out.insert("max_uses".into(), Value::Number(max_uses.into()));
    }
    if let Some(user_location) = anthropic_web_search_user_location(obj.get("user_location")) {
        out.insert("user_location".into(), user_location);
    }
    for field in ["allowed_domains", "blocked_domains"] {
        if let Some(value) = obj.get(field) {
            out.insert(field.to_owned(), value.clone());
        }
    }
    Some(Value::Object(out))
}

fn responses_computer_tool_to_anthropic(obj: &Map<String, Value>) -> Option<Value> {
    let width = obj
        .get("display_width_px")
        .or_else(|| obj.get("display_width"))
        .and_then(Value::as_u64)?;
    let height = obj
        .get("display_height_px")
        .or_else(|| obj.get("display_height"))
        .and_then(Value::as_u64)?;
    let mut out = Map::new();
    out.insert("type".into(), Value::String("computer_20250124".into()));
    out.insert(
        "name".into(),
        obj.get("name")
            .cloned()
            .unwrap_or_else(|| Value::String("computer".into())),
    );
    out.insert("display_width_px".into(), Value::Number(width.into()));
    out.insert("display_height_px".into(), Value::Number(height.into()));
    if let Some(display_number) = obj.get("display_number") {
        out.insert("display_number".into(), display_number.clone());
    }
    if let Some(cache_control) = obj.get("cache_control") {
        out.insert("cache_control".into(), cache_control.clone());
    }
    Some(Value::Object(out))
}

fn responses_mcp_tool_to_anthropic_server(obj: &Map<String, Value>) -> Option<Value> {
    let server_url = obj.get("server_url").and_then(Value::as_str)?;
    let server_label = obj
        .get("server_label")
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)?;
    let mut out = Map::new();
    out.insert("type".into(), Value::String("url".into()));
    out.insert("url".into(), Value::String(server_url.to_owned()));
    out.insert("name".into(), Value::String(server_label.to_owned()));
    if let Some(allowed_tools) = obj.get("allowed_tools") {
        out.insert(
            "tool_configuration".into(),
            json!({ "allowed_tools": allowed_tools }),
        );
    }
    if let Some(auth) = obj
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        out.insert(
            "authorization_token".into(),
            Value::String(
                auth.strip_prefix("Bearer ")
                    .unwrap_or(auth)
                    .trim()
                    .to_owned(),
            ),
        );
    }
    Some(Value::Object(out))
}

fn copy_responses_tool_extension_fields(source: &Map<String, Value>, out: &mut Map<String, Value>) {
    for key in [
        "cache_control",
        "defer_loading",
        "allowed_callers",
        "input_examples",
    ] {
        if let Some(value) = source.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
}

fn namespace_description_prefix(namespace: &Map<String, Value>) -> Option<String> {
    let ns_name = namespace
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let ns_desc = namespace
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (ns_name, ns_desc) {
        (Some(name), Some(desc)) => Some(format!("[MCP server `{name}`: {desc}]")),
        (Some(name), None) => Some(format!("[MCP server `{name}`]")),
        (None, Some(desc)) => Some(format!("[MCP server: {desc}]")),
        (None, None) => None,
    }
}

fn description_with_namespace_prefix(description: &str, namespace_prefix: Option<&str>) -> String {
    let Some(prefix) = namespace_prefix else {
        return description.to_owned();
    };
    if description.trim().is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}\n\n{description}")
    }
}

fn custom_tool_description_with_format(description: &str, format: Option<&Value>) -> String {
    let Some(format) = format else {
        return description.to_owned();
    };
    let format_text = value_to_string(format);
    if description.trim().is_empty() {
        format!("Responses custom tool format: {format_text}")
    } else {
        format!("{description}\n\nResponses custom tool format: {format_text}")
    }
}

fn custom_tool_input_description(format: Option<&Value>) -> String {
    let Some(format) = format else {
        return "Free-form input passed verbatim to the tool.".into();
    };
    format!(
        "Free-form input passed verbatim to the tool. The input must follow this Responses custom tool format: {}",
        value_to_string(format)
    )
}

fn anthropic_web_search_max_uses(search_context_size: Option<&Value>) -> Option<u64> {
    match search_context_size.and_then(Value::as_str) {
        Some("low") => Some(1),
        Some("medium") => Some(5),
        Some("high") => Some(10),
        _ => None,
    }
}

fn anthropic_web_search_user_location(user_location: Option<&Value>) -> Option<Value> {
    let obj = user_location?.as_object()?;
    let approximate = obj
        .get("approximate")
        .and_then(Value::as_object)
        .unwrap_or(obj);
    let mut out = Map::new();
    out.insert("type".into(), Value::String("approximate".into()));
    for field in ["city", "region", "country", "timezone"] {
        if let Some(value) = approximate.get(field) {
            out.insert(field.to_owned(), value.clone());
        }
    }
    (out.len() > 1).then_some(Value::Object(out))
}

fn provider_web_search_enabled(provider: &Provider) -> bool {
    provider
        .request_options
        .get("web_search_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_anthropic_hosted_tool_type(tool_type: &str) -> bool {
    [
        "web_search",
        "bash",
        "text_editor",
        "code_execution",
        "web_fetch",
        "memory",
        "tool_search_tool",
        "advisor_",
        "computer_",
    ]
    .iter()
    .any(|prefix| tool_type.starts_with(prefix))
}

fn default_anthropic_hosted_tool_name(tool_type: &str) -> &'static str {
    if tool_type.starts_with("code_execution") {
        "code_execution"
    } else if tool_type.starts_with("web_fetch") {
        "web_fetch"
    } else if tool_type.starts_with("web_search") {
        "web_search"
    } else if tool_type.starts_with("text_editor") {
        "str_replace_editor"
    } else if tool_type.starts_with("computer_") {
        "computer"
    } else if tool_type.starts_with("memory") {
        "memory"
    } else if tool_type.starts_with("tool_search_tool") {
        "tool_search_tool"
    } else if tool_type.starts_with("advisor_") {
        "advisor"
    } else {
        "bash"
    }
}

fn normalize_anthropic_input_schema(schema: Option<Value>, strict: Option<&Value>) -> Value {
    let mut schema = schema.unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("type").and_then(Value::as_str) != Some("object") {
            obj.insert("type".into(), Value::String("object".into()));
            obj.entry("properties")
                .or_insert_with(|| Value::Object(Map::new()));
        }
        if !obj.contains_key("type") {
            obj.insert("type".into(), Value::String("object".into()));
        }
        if let Some(strict) = strict.filter(|v| v.as_bool() == Some(true)) {
            obj.insert("strict".into(), strict.clone());
        }
    }
    schema
}

fn build_tool_name_maps(original_names: &[String]) -> AnthropicToolNameMaps {
    let mut used = BTreeSet::new();
    let mut forward = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    for original in original_names {
        let base = sanitize_tool_name_base(original);
        let mut candidate = base.clone();
        let mut suffix = 2u32;
        while used.contains(&candidate) {
            let suffix_text = format!("_{suffix}");
            let head_len = 128usize.saturating_sub(suffix_text.len());
            candidate = format!("{}{}", truncate_chars(&base, head_len), suffix_text);
            suffix += 1;
        }
        used.insert(candidate.clone());
        if &candidate != original {
            forward.insert(original.clone(), candidate.clone());
            reverse.insert(candidate, original.clone());
        }
    }
    AnthropicToolNameMaps { forward, reverse }
}

fn sanitize_tool_name_base(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
        if out.len() >= 128 {
            break;
        }
    }
    if out.is_empty() {
        "tool".to_owned()
    } else {
        truncate_chars(&out, 128)
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn convert_tool_choice(
    tool_choice: Option<&Value>,
    parallel_tool_calls: Option<bool>,
    tool_names: &AnthropicToolNameMaps,
) -> Option<Value> {
    let mut mapped = match tool_choice {
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            "none" => Some(json!({ "type": "none" })),
            _ => None,
        },
        Some(Value::Object(obj)) => {
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
            {
                Some(json!({
                    "type": "tool",
                    "name": tool_names.forward.get(name).map(String::as_str).unwrap_or(name),
                }))
            } else {
                match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "auto" => Some(json!({ "type": "auto" })),
                    "required" | "any" => Some(json!({ "type": "any" })),
                    "none" => Some(json!({ "type": "none" })),
                    "tool" => obj.get("name").and_then(|v| v.as_str()).map(|name| {
                        json!({
                            "type": "tool",
                            "name": tool_names.forward.get(name).map(String::as_str).unwrap_or(name),
                        })
                    }),
                    _ => None,
                }
            }
        }
        _ => None,
    };
    if let Some(Value::Object(obj)) = &mut mapped {
        if obj.get("type").and_then(|v| v.as_str()) != Some("none") {
            if let Some(parallel) = parallel_tool_calls {
                obj.insert("disable_parallel_tool_use".into(), Value::Bool(!parallel));
            } else if let Some(disable_parallel) = tool_choice
                .and_then(Value::as_object)
                .and_then(|choice| choice.get("disable_parallel_tool_use"))
                .and_then(Value::as_bool)
            {
                obj.insert(
                    "disable_parallel_tool_use".into(),
                    Value::Bool(disable_parallel),
                );
            }
        }
    } else if let Some(parallel) = parallel_tool_calls {
        mapped = Some(json!({
            "type": "auto",
            "disable_parallel_tool_use": !parallel,
        }));
    }
    mapped
}

fn convert_stop_sequences(stop: Option<&Value>) -> Option<Value> {
    match stop? {
        Value::String(s) if !s.is_empty() => Some(Value::Array(vec![Value::String(s.clone())])),
        Value::Array(items) => {
            let values = items
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect::<Vec<_>>();
            (!values.is_empty()).then_some(Value::Array(values))
        }
        _ => None,
    }
}

fn convert_responses_thinking(body: &Map<String, Value>) -> Option<Value> {
    if let Some(thinking) = body.get("thinking").filter(|v| v.is_object()) {
        return Some(thinking.clone());
    }
    let effort = body
        .get("reasoning")
        .and_then(|v| v.get("effort"))
        .and_then(Value::as_str)
        .or_else(|| body.get("reasoning_effort").and_then(Value::as_str))
        .map(str::to_ascii_lowercase)?;
    match effort.as_str() {
        "none" | "off" => None,
        "minimal" | "low" => Some(json!({ "type": "enabled", "budget_tokens": 1024 })),
        "medium" => Some(json!({ "type": "enabled", "budget_tokens": 4096 })),
        "high" => Some(json!({ "type": "enabled", "budget_tokens": 8192 })),
        "xhigh" | "max" => Some(json!({ "type": "enabled", "budget_tokens": 16384 })),
        _ => None,
    }
}

fn apply_adaptive_thinking_for_model(
    model: &str,
    body: &Map<String, Value>,
    out: &mut Map<String, Value>,
) {
    if !is_adaptive_claude_model(model) {
        return;
    }
    let thinking_type = out
        .get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str);
    let Some(effort) = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            body.get("reasoning")
                .and_then(|v| v.get("effort"))
                .and_then(Value::as_str)
        })
        .and_then(reasoning_effort_to_output_config_effort)
        .or_else(|| {
            out.get("thinking")
                .and_then(|v| v.get("budget_tokens"))
                .and_then(Value::as_u64)
                .map(thinking_budget_to_output_config_effort)
        })
    else {
        return;
    };
    if matches!(thinking_type, Some("enabled" | "adaptive")) {
        out.insert("thinking".into(), json!({ "type": "adaptive" }));
        let output_config = out
            .entry("output_config")
            .or_insert_with(|| Value::Object(Map::new()));
        if !output_config.is_object() {
            *output_config = Value::Object(Map::new());
        }
        if let Some(obj) = output_config.as_object_mut() {
            obj.entry("effort")
                .or_insert_with(|| Value::String(effort.to_owned()));
        }
    }
}

fn is_adaptive_claude_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    [
        "opus-4-6",
        "opus_4_6",
        "opus-4.6",
        "opus_4.6",
        "sonnet-4-6",
        "sonnet_4_6",
        "sonnet-4.6",
        "sonnet_4.6",
        "opus-4-7",
        "opus_4_7",
        "opus-4.7",
        "opus_4.7",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn reasoning_effort_to_output_config_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

fn thinking_budget_to_output_config_effort(budget: u64) -> &'static str {
    if budget >= 24_000 {
        "xhigh"
    } else if budget >= 10_000 {
        "high"
    } else if budget >= 5_000 {
        "medium"
    } else {
        "low"
    }
}

fn convert_metadata(body: &Map<String, Value>) -> Option<Value> {
    let user = body
        .get("user")
        .and_then(|v| v.as_str())
        .or_else(|| {
            body.get("metadata")
                .and_then(|v| v.get("user"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            body.get("metadata")
                .and_then(|v| v.get("user_id"))
                .and_then(|v| v.as_str())
        })?;
    is_valid_anthropic_user_id(user).then(|| json!({ "user_id": user }))
}

fn is_valid_anthropic_user_id(user_id: &str) -> bool {
    let trimmed = user_id.trim();
    if trimmed.is_empty() {
        return false;
    }
    let looks_like_email = trimmed.contains('@')
        && trimmed.rsplit_once('.').is_some()
        && !trimmed.contains(char::is_whitespace);
    if looks_like_email {
        return false;
    }
    let digit_count = trimmed.chars().filter(|ch| ch.is_ascii_digit()).count();
    let phone_chars_only = trimmed
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '+' | '-' | '(' | ')' | ' '));
    !(phone_chars_only && digit_count >= 7)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn provider_with_request_options(request_options: IndexMap<String, Value>) -> Provider {
        Provider {
            id: "anyrouter".into(),
            name: "Anyrouter".into(),
            base_url: "https://anyrouter.top".into(),
            auth_scheme: "bearer".into(),
            api_format: "anthropic_messages".into(),
            api_key: "sk-test".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options,
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    #[test]
    fn upstream_path_avoids_double_v1() {
        assert_eq!(
            build_anthropic_messages_upstream_path("https://api.anthropic.com/v1"),
            "/messages"
        );
        assert_eq!(
            build_anthropic_messages_upstream_path("https://api.anthropic.com"),
            "/v1/messages"
        );
        assert_eq!(
            build_anthropic_messages_upstream_path("https://proxy.example/anthropic/v1/"),
            "/messages"
        );
    }

    #[test]
    fn default_headers_include_anthropic_version() {
        let headers = anthropic_messages_default_headers();
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some(DEFAULT_ANTHROPIC_VERSION)
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers.get(ACCEPT).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn invalid_thinking_signature_error_detector_matches_litellm_keywords() {
        assert!(is_anthropic_invalid_thinking_signature_error(
            br#"{"error":{"message":"messages.3.content.0: Invalid `signature` in `thinking` block"}}"#
        ));
        assert!(!is_anthropic_invalid_thinking_signature_error(
            br#"{"error":{"message":"Invalid model name"}}"#
        ));
        assert!(!is_anthropic_invalid_thinking_signature_error(&[
            0xff, 0xfe, 0xfd,
        ]));
    }

    #[test]
    fn invalid_signature_retry_strips_thinking_blocks_from_body_and_session() {
        let mut plan = RequestPlan {
            upstream_path: "/v1/messages".into(),
            body: Bytes::from(
                json!({
                    "model": "claude-opus-4-7",
                    "stream": true,
                    "thinking": {"type": "adaptive"},
                    "messages": [
                        {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                        {
                            "role": "assistant",
                            "content": [
                                {"type": "thinking", "thinking": "old", "signature": "bad"},
                                {"type": "redacted_thinking", "data": "sealed"},
                                {"type": "text", "text": "visible"}
                            ]
                        },
                        {
                            "role": "assistant",
                            "content": [
                                {"type": "thinking", "thinking": "only thinking", "signature": "bad"}
                            ]
                        }
                    ]
                })
                .to_string(),
            ),
            upstream_headers: HeaderMap::new(),
            response_session: Some(ResponseSessionPlan {
                response_id: "resp_retry".into(),
                messages: vec![
                    json!({
                        "role": "assistant",
                        "content": [
                            {"type": "thinking", "thinking": "old", "signature": "bad"},
                            {"type": "text", "text": "visible"}
                        ]
                    }),
                    json!({
                        "role": "assistant",
                        "content": [
                            {"type": "redacted_thinking", "data": "sealed"}
                        ]
                    }),
                ],
            }),
            adapter_metadata: None,
            is_compact: false,
            original_responses_request: None,
        };

        assert!(strip_thinking_blocks_for_invalid_signature_retry(&mut plan).unwrap());
        let body: Value = serde_json::from_slice(&plan.body).unwrap();
        assert!(body.get("thinking").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            2,
            "messages whose content becomes empty must be omitted"
        );
        assert_eq!(
            messages[1]["content"],
            json!([{"type": "text", "text": "visible"}])
        );
        assert_eq!(
            plan.response_session.as_ref().unwrap().messages,
            vec![json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "visible"}]
            })]
        );
    }

    #[test]
    fn invalid_user_ids_are_filtered() {
        assert!(!is_valid_anthropic_user_id("a@example.com"));
        assert!(!is_valid_anthropic_user_id("+1 (555) 123-4567"));
        assert!(is_valid_anthropic_user_id("local-user-123"));
    }

    #[test]
    fn provider_request_options_can_inject_thinking() {
        let mut options = IndexMap::new();
        options.insert(
            "anthropic_messages".into(),
            json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}),
        );
        let provider = provider_with_request_options(options);
        let converted = responses_body_to_anthropic_messages_request(
            &json!({
                "model": "claude-opus-4-7[1m]",
                "input": "hi",
                "max_output_tokens": 64
            }),
            &provider,
        )
        .unwrap();
        assert_eq!(
            converted.request["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        assert_eq!(
            converted.request["max_tokens"], 2048,
            "max_tokens must exceed thinking budget when provider injects thinking"
        );
    }

    #[test]
    fn explicit_request_thinking_overrides_provider_request_options() {
        let mut options = IndexMap::new();
        options.insert(
            "anthropic_messages".into(),
            json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}),
        );
        let provider = provider_with_request_options(options);
        let converted = responses_body_to_anthropic_messages_request(
            &json!({
                "model": "claude-opus-4-7[1m]",
                "input": "hi",
                "reasoning": {"effort": "medium"},
                "max_output_tokens": 8192
            }),
            &provider,
        )
        .unwrap();
        assert_eq!(converted.request["thinking"], json!({"type": "adaptive"}));
        assert_eq!(converted.request["output_config"]["effort"], "medium");
        assert_eq!(converted.request["max_tokens"], 8192);
    }

    #[test]
    fn provider_request_options_can_enable_claude_code_compat() {
        let mut options = IndexMap::new();
        options.insert(
            "anthropic_messages".into(),
            json!({"claude_code_compat": true, "thinking": {"type": "adaptive"}}),
        );
        let provider = provider_with_request_options(options);
        let converted = responses_body_to_anthropic_messages_request(
            &json!({
                "model": "claude-opus-4-7",
                "instructions": "Follow the local Codex instructions.",
                "input": "hi",
                "reasoning": {"effort": "high"},
                "max_output_tokens": 8192
            }),
            &provider,
        )
        .unwrap();
        assert_eq!(converted.request["thinking"], json!({"type": "adaptive"}));
        let system = converted.request["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_PROMPT);
        assert!(system
            .iter()
            .any(|item| item["text"] == "Follow the local Codex instructions."));
        let user_id = converted.request["metadata"]["user_id"].as_str().unwrap();
        let parsed_user_id: Value = serde_json::from_str(user_id).unwrap();
        assert_eq!(parsed_user_id["device_id"].as_str().unwrap().len(), 64);
        assert!(parsed_user_id["session_id"].as_str().unwrap().contains('-'));
    }

    #[test]
    fn forced_tool_choice_drops_incompatible_thinking_but_preserves_tool_choice() {
        let mut options = IndexMap::new();
        options.insert(
            "anthropic_messages".into(),
            json!({"claude_code_compat": true, "thinking": {"type": "adaptive"}}),
        );
        let provider = provider_with_request_options(options);
        let converted = responses_body_to_anthropic_messages_request(
            &json!({
                "model": "claude-opus-4-7",
                "input": "must call echo_probe",
                "tools": [{
                    "type": "function",
                    "name": "echo_probe",
                    "description": "Echo a probe value.",
                    "parameters": {
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }
                }],
                "tool_choice": {"type": "function", "function": {"name": "echo_probe"}},
                "reasoning": {"effort": "high"},
                "max_output_tokens": 8192
            }),
            &provider,
        )
        .unwrap();

        assert!(converted.request.get("thinking").is_none());
        assert!(converted.request.get("output_config").is_none());
        assert_eq!(
            converted.request["tool_choice"],
            json!({"type": "tool", "name": "echo_probe"})
        );
        assert_eq!(converted.request["tools"][0]["name"], "echo_probe");
    }

    #[test]
    fn prepared_request_headers_match_claude_code_metadata_session() {
        let mut options = IndexMap::new();
        options.insert(
            "anthropic_messages".into(),
            json!({"claude_code_compat": true, "thinking": {"type": "adaptive"}}),
        );
        let provider = provider_with_request_options(options);
        let prepared = prepare_anthropic_messages_request(
            "/v1/responses",
            Bytes::from(
                json!({
                    "model": "claude-opus-4-7",
                    "input": "hi",
                    "max_output_tokens": 1024
                })
                .to_string(),
            ),
            &provider,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let user_id = body["metadata"]["user_id"].as_str().unwrap();
        let parsed_user_id: Value = serde_json::from_str(user_id).unwrap();
        let session_id = parsed_user_id["session_id"].as_str().unwrap();
        assert_eq!(
            prepared
                .headers
                .get("x-claude-code-session-id")
                .and_then(|v| v.to_str().ok()),
            Some(session_id)
        );
        assert_eq!(
            prepared.headers.get("x-app").and_then(|v| v.to_str().ok()),
            Some("cli")
        );
        assert_eq!(
            prepared
                .headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok()),
            Some(CLAUDE_CODE_USER_AGENT)
        );
    }
}
