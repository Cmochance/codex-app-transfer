//! Codex OpenAI Responses API → grok.com chat payload 转换。
//!
//! ## 输入(Codex Responses API)
//!
//! Codex APP 走 `/v1/responses`,body 形如:
//!
//! ```json
//! {
//!   "model": "gpt-5-codex",            // 槽位名,本 adapter 映射到 grok 后端模型
//!   "input": [{ "type": "message", "role": "user", "content": "..." }, ...],
//!   "tools": [...],
//!   "stream": true,
//!   "previous_response_id": "resp_abc",  // 多轮 anchor
//!   "reasoning": { "effort": "high" }
//! }
//! ```
//!
//! ## 输出([`super::types::GrokChatRequest`])
//!
//! grok.com 后端只接受单 `message` string + `parent_response_id` DAG。
//! 多轮 message 历史**不**塞 message 字段(那是 chenyme 反协议简化),
//! 走 [`super::parent_response::ParentResponseTracker`] 锚定。
//!
//! ## R3 PoC 范围(本文件当前实现)
//!
//! - ✅ 抽 Codex `input` 数组里**最后一条 user message** 作为 grok `message`
//! - ✅ 模型映射:Codex 槽位 → `provider.models[slot]` → grok `modeId`
//! - ✅ `previous_response_id` 反查 tracker → `parent_response_id`(miss 则 omit)
//! - ✅ `reasoning.effort` → `is_reasoning` flag
//! - ✅ `disable_search` 默认 false(grok 内置 web search 开启)
//! - ⚠️ tools / web_search / MCP namespace 字段:**忽略**(server-side state 自动注入)
//! - ⚠️ 历史 user/assistant messages:**忽略**(信任 grok.com 后端用 parent_response_id 拉历史)
//!
//! 协议事实详见本 module 各 type 的 doc comment(`super::types`、`super::parent_response`),
//! 以及 `crates/adapters/src/grok_web/mod.rs` 顶层文档。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use serde_json::Value;

use crate::grok_web::parent_response::{global_tracker, ParentResponseTracker};
use crate::grok_web::types::GrokChatRequest;
use crate::types::AdapterError;

/// grok.com chat endpoint path(相对 `provider.base_url`)。
pub const GROK_CHAT_PATH: &str = "/rest/app-chat/conversations/new";

/// Codex Responses request body → grok.com [`GrokChatRequest`] payload。
///
/// 不构造 HTTP 请求,只产出 payload(由 mapper 层包成 `RequestPlan`)。
///
/// # Errors
///
/// - `BadRequest`:Codex body 缺 `input` / `model` 字段,或 input 不含可提取的 user message
/// - `Internal`:Provider.models 槽位映射缺失(应在 Provider 加载时校验,这里再次防御)
pub fn responses_body_to_grok_request(
    body: &Value,
    provider: &Provider,
) -> Result<GrokChatRequest, AdapterError> {
    responses_body_to_grok_request_with_tracker(body, provider, Some(global_tracker()))
}

/// 内部入口,允许测试时注入自定义 tracker。
pub fn responses_body_to_grok_request_with_tracker(
    body: &Value,
    provider: &Provider,
    tracker: Option<&ParentResponseTracker>,
) -> Result<GrokChatRequest, AdapterError> {
    let codex_model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest("missing `model` field".into()))?;
    let mode_id = resolve_mode_id(codex_model, provider)?;

    let message = extract_last_user_message(body)?;

    let parent_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(|prev| tracker.and_then(|t| t.get(prev)));

    let is_reasoning = parse_reasoning_flag(body);
    let custom_instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    Ok(GrokChatRequest {
        message,
        mode_id,
        parent_response_id,
        is_reasoning,
        custom_instructions,
        ..GrokChatRequest::default()
    })
}

/// Codex `model` 字段 → grok backend `modeId`。
///
/// 入参 `codex_model` 在到达本函数前可能已被 `crates/proxy/src/forward.rs:317`
/// 的 `rewrite_model_field` 改写成 resolver 的 `rewritten_model`(concrete
/// upstream model 名,不是 slot 名)。两种来源都要 handle。
///
/// 优先级:
/// 1. `provider.models[codex_model]`(精确槽位映射,典型场景:body.model 是 slot 名)
/// 2. `provider.models["default"]`(provider 默认 slot)
/// 3. **`codex_model` 自身 as-is** —— resolver 已 rewrite 时这就是 concrete model;
///    或用户直接传 model="grok-420-computer-use-sa" 这种已知 backend 名。
///
/// **不再 hardcoded literal fallback**(原 R3 PoC 设计 silently 路由到
/// "grok-420-computer-use-sa" — review-feedback H2 标记为破坏性,删除)。
/// **不再 Err on miss**(初版改造矫枉过正:用户在 `provider.extra` 给一个
/// 已知 backend model 名 + 不配 `provider.models` 时合法 use case,
/// PR #129 chatgpt-codex-connector P1 标记)。
///
/// Provider schema 在 [`codex_app_transfer_registry::Provider::models`] 是 IndexMap,
/// 按磁盘顺序保留,槽位 key 见 [`codex_app_transfer_registry::ModelSlotKey`]。
fn resolve_mode_id(codex_model: &str, provider: &Provider) -> Result<String, AdapterError> {
    if let Some(mode_id) = provider.models.get(codex_model).cloned() {
        return Ok(mode_id);
    }
    if let Some(default) = provider.models.get("default").cloned() {
        return Ok(default);
    }
    // P1 fallback:miss 槽位 + 无 default → 用 codex_model 自身(已 rewrite 的
    // concrete model 名 or 用户直接传的 backend 名)。**不**用 hardcoded literal,
    // 不静默路由到不同模型,保持用户/resolver 意图。
    Ok(codex_model.to_owned())
}

/// 抽 Codex Responses `input` 数组里**最后一条 user message** 的文本。
///
/// grok.com 后端只接受单 message 字段,**多轮历史不塞 message,走 parent_response_id 锚定**。
///
/// `input` 元素形态(Codex Responses spec):
///
/// ```json
/// { "type": "message", "role": "user", "content": "..." }
/// // 或
/// { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "..."}] }
/// ```
///
/// 也兼容直接 `"input": "..."` 单 string(简化场景)。
fn extract_last_user_message(body: &Value) -> Result<String, AdapterError> {
    if let Some(s) = body.get("input").and_then(Value::as_str) {
        if !s.is_empty() {
            return Ok(s.to_owned());
        }
    }
    let arr = body
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::BadRequest("missing `input` field".into()))?;

    for item in arr.iter().rev() {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(text) = item.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                return Ok(text.to_owned());
            }
            continue;
        }
        if let Some(parts) = item.get("content").and_then(Value::as_array) {
            let joined = parts
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return Ok(joined);
            }
        }
    }
    Err(AdapterError::BadRequest(
        "input array contains no user message".into(),
    ))
}

/// 解析 Codex `reasoning.effort` → grok `is_reasoning` flag。
///
/// 当前简化:`"high"` / `"medium"` → true,`"low"` / `"none"` / unset → false。
/// 后续可与 modeId 一起做更细 routing(`high` 用 grok-420-heavy 等)。
fn parse_reasoning_flag(body: &Value) -> Option<bool> {
    let effort = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)?;
    Some(matches!(effort, "high" | "medium"))
}

/// 把 [`GrokChatRequest`] 序列化为 chat endpoint 请求 body。
///
/// 序列化前先调 [`GrokChatRequest::validate`] 检查 message/mode_id 非空 +
/// `extra` 不含 forbidden keys(review-feedback TD3 防 forbidden 字段偷渡)。
pub fn serialize_grok_request(req: &GrokChatRequest) -> Result<Bytes, AdapterError> {
    req.validate()?;
    let bytes = serde_json::to_vec(req).map_err(AdapterError::BodyDecode)?;
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_transfer_registry::Provider;
    use indexmap::IndexMap;
    use serde_json::json;

    fn make_provider() -> Provider {
        let mut models = IndexMap::new();
        models.insert("default".into(), "grok-420-computer-use-sa".into());
        models.insert("gpt_5_codex".into(), "grok-420-computer-use-sa".into());
        Provider {
            id: "grok-web".into(),
            name: "Grok Web".into(),
            base_url: "https://grok.com".into(),
            auth_scheme: "grok_cookie".into(),
            api_format: "grok_web".into(),
            api_key: String::new(),
            models,
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    #[test]
    fn last_user_message_string_form() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [
                {"type": "message", "role": "user", "content": "hello world"}
            ]
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(req.message, "hello world");
        assert_eq!(req.mode_id, "grok-420-computer-use-sa");
    }

    #[test]
    fn last_user_message_parts_array() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "part 1"},
                    {"type": "input_text", "text": "part 2"}
                ]}
            ]
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(req.message, "part 1\npart 2");
    }

    #[test]
    fn last_user_message_when_multiple_turns_takes_latest_user() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [
                {"type": "message", "role": "user", "content": "first user"},
                {"type": "message", "role": "assistant", "content": "model reply"},
                {"type": "message", "role": "user", "content": "second user"}
            ]
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(req.message, "second user");
    }

    #[test]
    fn previous_response_id_resolved_via_tracker() {
        let tracker = ParentResponseTracker::new();
        tracker.record("resp_abc", "9f82a10c-grok-uuid");
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "follow-up"}],
            "previous_response_id": "resp_abc"
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, Some(&tracker)).unwrap();
        assert_eq!(
            req.parent_response_id.as_deref(),
            Some("9f82a10c-grok-uuid")
        );
    }

    #[test]
    fn previous_response_id_miss_omits_parent_response_id() {
        let tracker = ParentResponseTracker::new();
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "x"}],
            "previous_response_id": "resp_unknown"
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, Some(&tracker)).unwrap();
        assert!(req.parent_response_id.is_none());
    }

    #[test]
    fn missing_input_array_errors_with_bad_request() {
        let body = json!({ "model": "gpt_5_codex" });
        let p = make_provider();
        let err = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn reasoning_high_sets_is_reasoning_true() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "x"}],
            "reasoning": { "effort": "high" }
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(req.is_reasoning, Some(true));
    }

    #[test]
    fn instructions_field_maps_to_custom_instructions() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "x"}],
            "instructions": "You are a Rust expert."
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(
            req.custom_instructions.as_deref(),
            Some("You are a Rust expert.")
        );
    }

    #[test]
    fn resolve_mode_id_falls_back_to_codex_model_when_no_slot_and_no_default() {
        // chatgpt-codex-connector PR #129 P1:miss 槽位 + 无 default 时,fallback
        // 用 codex_model 自身(已被 resolver rewrite 成 concrete upstream model,
        // 或用户直接传的已知 backend 名);**不**走 hardcoded literal。
        let p = Provider {
            id: "grok-web".into(),
            name: "Grok Web".into(),
            base_url: "https://grok.com".into(),
            auth_scheme: "grok_cookie".into(),
            api_format: "grok_web".into(),
            api_key: String::new(),
            models: IndexMap::new(), // 空
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        };
        // 模拟 resolver.rs:317 已 rewrite body.model 成 concrete grok backend
        let body = json!({
            "model": "grok-420-computer-use-sa",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        });
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(
            req.mode_id, "grok-420-computer-use-sa",
            "已 rewrite 的 concrete model 应直接当 modeId,不被静默替换"
        );
    }

    #[test]
    fn serialize_grok_request_rejects_forbidden_extra_field() {
        // review-feedback TD3:serialize_grok_request 调 validate,connectorIds 偷渡被拦截
        let mut req = GrokChatRequest::default();
        req.message = "hi".into();
        req.extra.insert("connectorIds".into(), json!(["uuid-1"]));
        let err = serialize_grok_request(&req).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn serialized_payload_excludes_connector_fields() {
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "x"}]
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        let bytes = serialize_grok_request(&req).unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let obj = json.as_object().unwrap();
        // Connector 字段一律不传(server-side state 黑名单)
        assert!(!obj.contains_key("connectorIds"));
        assert!(!obj.contains_key("connectors"));
        assert!(!obj.contains_key("toolOverrides"));
        // 但黑名单字段存在(空)
        assert_eq!(obj["disabledConnectorIds"], json!([]));
    }
}
