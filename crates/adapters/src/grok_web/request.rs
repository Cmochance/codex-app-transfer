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
//! 多轮 message 历史**用 role prefix flatten** 到 single message string(`User: ...\n\nAssistant: ...`),
//! grok 模型从 prefix 自己理解上下文。同时 [`super::parent_response::ParentResponseTracker`]
//! 命中时传 `parent_response_id` 给 grok.com,后端 DAG 也自己拉 — **双保险**。
//!
//! ## 当前实现(2026-05-12 task 18 重构后,对齐 ARCHITECTURE_PROTOCOL_GUIDE Phase 4)
//!
//! - ✅ 模型映射:Codex 槽位 → `provider.models[slot]` → grok `modeId`
//! - ✅ **多轮上下文** + **autocompact 展开**:走 `core::input` +
//!   `responses/compact.rs` + `global_response_session_cache()`(L1 LRU + L2
//!   SQLite 持久化 `~/.codex-app-transfer/sessions.db`,30 天 TTL,**`.app`
//!   重启不丢历史**)
//! - ✅ `previous_response_id` 反查 tracker → `parent_response_id`(双保险)
//! - ✅ `reasoning.effort` → `is_reasoning` flag
//! - ✅ `disable_search` 默认 false(grok 内置 web search 开启)
//! - ⚠️ tools / web_search / MCP namespace 字段:**忽略**(server-side state 自动注入)
//!
//! 主入口:[`responses_body_to_grok_request_with_session`](生产路径,mapper 用)。
//! 兼容入口:[`responses_body_to_grok_request`] / [`responses_body_to_grok_request_with_tracker`]
//! (test fixture,**无 session cache → 不走 core → 丢历史**,prod 别用)。
//!
//! 协议事实详见本 module 各 type 的 doc comment(`super::types`、`super::parent_response`),
//! 以及 `crates/adapters/src/grok_web/mod.rs` 顶层文档。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use serde_json::Value;

use crate::core::input::response_id_for_session;
use crate::grok_web::parent_response::{global_tracker, ParentResponseTracker};
use crate::grok_web::types::GrokChatRequest;
use crate::responses::session::ResponseSessionCache;
use crate::types::{AdapterError, ResponseSessionPlan};

/// grok.com chat endpoint path(相对 `provider.base_url`)。
pub const GROK_CHAT_PATH: &str = "/rest/app-chat/conversations/new";

/// `responses_body_to_grok_request_with_session` 的输出 — 同时返回 grok request
/// payload 跟 `ResponseSessionPlan`(供 mapper 注入 `RequestPlan.response_session`,
/// 让流末 `responses/converter` 把本轮 user+assistant messages append 进 cache,
/// 下轮 `previous_response_id` 命中时自动拉历史)。
#[derive(Debug, Clone)]
pub struct GrokRequestConversion {
    pub request: GrokChatRequest,
    pub response_session: ResponseSessionPlan,
}

/// **测试 fixture / 兼容入口**(2026-05-12 task 18 后,silent-failure-hunter F1
/// 反馈降级 `pub(crate)`):Codex Responses body → grok payload,**无 session cache,
/// 不走 core,会丢历史 message + compaction + function_call_output + reasoning
/// 等非 `type=message` 输入项**。
///
/// **生产 mapper 必须走 [`responses_body_to_grok_request_with_session`]**,
/// 本入口仅供 unit test fixture。`pub(crate)` 物理上禁止跨 crate 误用。
pub(crate) fn responses_body_to_grok_request(
    body: &Value,
    provider: &Provider,
) -> Result<GrokChatRequest, AdapterError> {
    responses_body_to_grok_request_with_tracker(body, provider, Some(global_tracker()))
}

/// **测试 fixture**(无 session_cache,跟 [`responses_body_to_grok_request`] 同
/// 一降级路径,只是允许注入 tracker)。降级 `pub(crate)` 防止 prod 误用。
pub(crate) fn responses_body_to_grok_request_with_tracker(
    body: &Value,
    provider: &Provider,
    tracker: Option<&ParentResponseTracker>,
) -> Result<GrokChatRequest, AdapterError> {
    let conversion = responses_body_to_grok_request_internal(body, provider, None, tracker)?;
    Ok(conversion.request)
}

/// **主入口**(对齐 ARCHITECTURE_PROTOCOL_GUIDE Phase 4):接 `ResponseSessionCache`
/// (sqlite 持久化 `~/.codex-app-transfer/sessions.db`),走 `core::input` 共性
/// 历史拼接 + `responses/compact.rs` 三种 compaction variant 自动展开。
///
/// 双保险:
/// 1. **client 端历史拼接**(本 fn 主路径):把 merged messages flatten 成 grok
///    single message string,角色 prefix(`User: ...\n\nAssistant: ...`),grok
///    模型自己理解 context。即便 grok.com 服务端 DAG miss 也能 work。
/// 2. **grok 服务端 DAG 锚定**(`ParentResponseTracker`):tracker 命中时传
///    `parent_response_id` 给 grok.com,后端 ALSO 自己拉历史。两路任一 work
///    都行 — 双保险。
pub fn responses_body_to_grok_request_with_session(
    body: &Value,
    provider: &Provider,
    session_cache: Option<&ResponseSessionCache>,
) -> Result<GrokRequestConversion, AdapterError> {
    responses_body_to_grok_request_internal(body, provider, session_cache, Some(global_tracker()))
}

fn responses_body_to_grok_request_internal(
    body: &Value,
    provider: &Provider,
    session_cache: Option<&ResponseSessionCache>,
    tracker: Option<&ParentResponseTracker>,
) -> Result<GrokRequestConversion, AdapterError> {
    let codex_model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest("missing `model` field".into()))?;
    let mode_id = resolve_mode_id(codex_model, provider)?;

    // 走 core/responses 共性历史拼接(2026-05-12 task 18,对齐架构):
    // - `responses_body_to_chat_body_for_provider_with_session` 内部调
    //   `build_messages_from_input` → 处理 previous_response_id 历史合并 +
    //   `type=compaction` 三 variant 自动展开成 user message + tool_call_repair。
    // - cache miss + input 空 → `PreviousResponseNotFound`(forward 转标准 400)
    // - cache miss + input 非空 → 降级仅本轮(已含 codex 当前输入展开)
    //
    // **silent-failure F3 修**:fallback 路径(无 session_cache,test fixture)
    // 必须独立处理 `body.instructions` —— 否则 instructions 字段被静默丢弃。
    // 走 core 路径时 instructions 已合并进 messages[0]=system,无需重复。
    let (message, merged_messages, fallback_instructions) = if session_cache.is_some() {
        let conversion = crate::responses::responses_body_to_chat_body_for_provider_with_session(
            body,
            None,
            session_cache,
        )?;
        let msgs = conversion.response_session.messages;
        let flat = flatten_messages_to_grok_single_string(&msgs);
        (flat, msgs, None)
    } else {
        // 无 cache 路径(test fixture / backwards compat):仅 flatten 当前 body.input
        // 不走 core,**会丢历史 + 非 type=message 输入项**(compaction /
        // function_call_output / reasoning)。生产路径绝不应进这里。
        // silent-failure F1:在内部 warn 让 operator 看到本流程降级
        tracing::warn!(
            error_id = "GROK_WEB_NO_SESSION_FALLBACK",
            "grok_web 走无 session_cache 降级路径(test fixture only,prod 不该命中);\
             历史 message + compaction + function_call_output 等非 type=message 输入项被丢弃"
        );
        let messages = extract_messages_from_input_only(body)?;
        let flat = flatten_messages_to_grok_single_string(&messages);
        // silent-failure F3:fallback 单独处理 instructions(走 customInstructions
        // 字段塞 grok wire,因为 messages 数组里没 system 段)
        let instructions = body
            .get("instructions")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        (flat, messages, instructions)
    };

    let parent_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(|prev| {
            tracker.and_then(|t| {
                t.get(&crate::grok_web::parent_response::CodexResponseId::from(
                    prev,
                ))
                .map(|g| g.into_inner())
            })
        });

    let is_reasoning = parse_reasoning_flag(body);

    Ok(GrokRequestConversion {
        request: GrokChatRequest {
            message,
            mode_id,
            parent_response_id,
            is_reasoning,
            custom_instructions: fallback_instructions,
            ..GrokChatRequest::default()
        },
        response_session: ResponseSessionPlan {
            response_id: response_id_for_session(),
            messages: merged_messages,
        },
    })
}

/// 把 chat-shape messages 数组 flatten 成 grok 协议的 single message string。
///
/// grok.com wire 只接受 single `message: string`,不接受 messages 数组。
/// 我们把多轮 history 用角色 prefix 拼接成 single string,grok 模型自己理解上下文。
///
/// 拼接规则(参考 chenyme/grok2api 类似 pattern):
/// - `system` / `developer` → `"System: <content>"`
/// - `user` → `"User: <content>"`
/// - `assistant` → `"Assistant: <content>"`
/// - `tool` → `"Tool Result: <content>"`(grok 自己 server-side state 处理 tool,
///   塞进文本作 context;不映射到 function_call/function_call_output schema)
/// - 各 turn 用 `\n\n` 分隔
///
/// content 兼容 string / array of {type:text|input_text|output_text, text:"..."}。
fn flatten_messages_to_grok_single_string(messages: &[Value]) -> String {
    let mut out = String::with_capacity(messages.len() * 64);
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content_str = extract_message_content_text(msg.get("content"));
        if content_str.is_empty() {
            continue;
        }
        let prefix = match role {
            "system" | "developer" => "System",
            "user" => "User",
            "assistant" => "Assistant",
            "tool" | "function" => "Tool Result",
            other if !other.is_empty() => other, // 防御未知角色:原样保留 prefix
            _ => continue,                       // 无 role 丢弃
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(prefix);
        out.push_str(": ");
        out.push_str(&content_str);
    }
    out
}

/// 抽 chat message 的 content 字段成 text(兼容 string / array of parts)。
///
/// **silent-failure F2 修**:content 数组**有元素但无可提取 text**(image-only /
/// audio-only / tool-call-only / 未识别 part type)时,返回**占位 token**而不是
/// 静默返空字符串。让 grok 模型至少知道"这一轮有非文本内容"。
fn extract_message_content_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.to_owned();
    }
    let Some(parts) = c.as_array() else {
        return String::new();
    };
    if parts.is_empty() {
        return String::new();
    }
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|p| {
            // OpenAI chat schema: {type:"text",text:"..."} 或 OpenAI Responses
            // schema: {type:"input_text"|"output_text", text:"..."}
            p.get("text").and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty())
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    // F2 修:有 parts 但全无 text → 收集 part types 作 placeholder
    let part_types: Vec<&str> = parts
        .iter()
        .filter_map(|p| p.get("type").and_then(Value::as_str))
        .collect();
    if part_types.is_empty() {
        return String::new();
    }
    let unique_types: std::collections::BTreeSet<&&str> = part_types.iter().collect();
    let summary = unique_types
        .iter()
        .map(|s| **s)
        .collect::<Vec<_>>()
        .join(", ");
    format!("[non-text content omitted: {summary}]")
}

/// 无 session_cache 的 fallback:从 body.input 数组按 Codex Responses spec 抽 messages,
/// **不**走 core(测试 fixture 路径)。生产路径用 with_session 入口。
fn extract_messages_from_input_only(body: &Value) -> Result<Vec<Value>, AdapterError> {
    let mut out: Vec<Value> = Vec::new();
    // 字符串形态:整段当 user
    if let Some(s) = body.get("input").and_then(Value::as_str) {
        if !s.is_empty() {
            out.push(serde_json::json!({"role": "user", "content": s}));
        }
        return Ok(out);
    }
    let arr = body
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::BadRequest("missing `input` field".into()))?;
    for item in arr {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        let content = item.get("content").cloned();
        out.push(serde_json::json!({"role": role, "content": content}));
    }
    if out.is_empty() {
        // silent-failure F4 修:错误消息更详细,帮 debug 时认出"实际是 fallback
        // 路径过滤掉 compaction / function_call_output 等非 message item"。
        let observed_types: std::collections::BTreeSet<String> = body
            .get("input")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|i| i.get("type").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let types_str = if observed_types.is_empty() {
            "[]".to_owned()
        } else {
            observed_types.into_iter().collect::<Vec<_>>().join(", ")
        };
        return Err(AdapterError::BadRequest(format!(
            "grok_web fallback: input array contains no items of type=message \
             (observed types: [{types_str}]). This path is test-only — production \
             routes should use responses_body_to_grok_request_with_session(.., session_cache)."
        )));
    }
    Ok(out)
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
    fn single_user_message_flattened_with_user_prefix() {
        // task 18:no-cache 路径(test fixture)— input 数组按 spec 抽完 flatten
        // 成 grok single message string,**带 "User:" 前缀**(grok 模型理解角色)
        let body = json!({
            "model": "gpt_5_codex",
            "input": [
                {"type": "message", "role": "user", "content": "hello world"}
            ]
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        assert_eq!(req.message, "User: hello world");
        assert_eq!(req.mode_id, "grok-420-computer-use-sa");
    }

    #[test]
    fn content_parts_array_joined_then_prefixed() {
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
        // content parts join "\n",再带 "User:" prefix
        assert_eq!(req.message, "User: part 1\npart 2");
    }

    #[test]
    fn multi_turn_history_all_roles_flattened_with_prefixes() {
        // task 18 核心修复:多轮 history 全展开(不再只取最后一条 user message)。
        // grok 模型从 role-prefixed string 自己理解上下文。
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
        assert_eq!(
            req.message,
            "User: first user\n\nAssistant: model reply\n\nUser: second user"
        );
    }

    #[test]
    fn previous_response_id_resolved_via_tracker() {
        let tracker = ParentResponseTracker::default();
        tracker.record_str("resp_abc", "9f82a10c-grok-uuid");
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
        let tracker = ParentResponseTracker::default();
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
    fn instructions_field_handled_differently_in_two_paths() {
        // task 18 + silent-failure F3:`instructions` 字段处理分两条路径:
        //   - **with_session 路径**(生产):core build_messages_from_input 把
        //     instructions 合并进 messages[0]=system,flatten 后变成 "System: ..."
        //     prefix 段,customInstructions=None(避免双重 system)。
        //   - **无 session 路径**(test fixture):不走 core,直接读 body.instructions
        //     塞 customInstructions 字段防止 instructions 完全丢失(F3 修)。
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "x"}],
            "instructions": "You are a Rust expert."
        });
        let p = make_provider();
        let req = responses_body_to_grok_request_with_tracker(&body, &p, None).unwrap();
        // fallback 路径:instructions 走 customInstructions 字段
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

    // ── task 18:with_session 路径(对齐 ARCHITECTURE_PROTOCOL_GUIDE)─────────

    #[test]
    fn with_session_no_cache_falls_back_to_input_only_path() {
        // session_cache 缺省时走 fallback,跟 with_tracker(None) 等价
        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        });
        let p = make_provider();
        let conv = responses_body_to_grok_request_with_session(&body, &p, None).unwrap();
        assert_eq!(conv.request.message, "User: hi");
        // response_session 仍 emit(供 mapper 注入 RequestPlan,即便 messages 没历史)
        assert!(!conv.response_session.response_id.is_empty());
        assert_eq!(conv.response_session.messages.len(), 1);
    }

    #[test]
    fn with_session_cache_hit_flattens_history_into_grok_message() {
        // task 18 核心场景:.app 重启 / 长对话场景,session_cache 拉历史。
        // 模拟 cache 已有 1 轮 user+assistant 历史,本轮 body 只带 follow-up question
        let cache = ResponseSessionCache::new(8, std::time::Duration::from_secs(60));
        cache.save(
            "resp_prev_grok",
            vec![
                json!({"role": "user", "content": "what is Rust?"}),
                json!({"role": "assistant", "content": "Rust is a systems language."}),
            ],
        );

        let body = json!({
            "model": "gpt_5_codex",
            "input": [{"type": "message", "role": "user", "content": "tell me more"}],
            "previous_response_id": "resp_prev_grok"
        });
        let p = make_provider();
        let conv = responses_body_to_grok_request_with_session(&body, &p, Some(&cache)).unwrap();
        // 验证 grok message 包含完整 history + 当前 follow-up
        assert!(
            conv.request.message.contains("User: what is Rust?"),
            "missing history user, got: {}",
            conv.request.message
        );
        assert!(
            conv.request
                .message
                .contains("Assistant: Rust is a systems language."),
            "missing history assistant, got: {}",
            conv.request.message
        );
        assert!(
            conv.request.message.contains("User: tell me more"),
            "missing follow-up user, got: {}",
            conv.request.message
        );
    }

    #[test]
    fn with_session_compaction_item_unfolded_by_core() {
        // task 18:Codex CLI autocompact 把长对话压缩成 `type=compaction` item
        // (字段 `encrypted_content` 实际是 plain summary text)。core/responses
        // `build_messages_from_input` 把它展开成 user message 注入历史。
        // grok mapper 走 with_session 路径自动享受 — 不再 drop 这条信息。
        let body = json!({
            "model": "gpt_5_codex",
            "input": [
                {"type": "compaction",
                 "encrypted_content": "[Summary of previous turns]\nUser asked about Rust, assistant explained."},
                {"type": "message", "role": "user", "content": "and lifetimes?"}
            ]
        });
        let p = make_provider();
        let cache = ResponseSessionCache::new(8, std::time::Duration::from_secs(60));
        let conv = responses_body_to_grok_request_with_session(&body, &p, Some(&cache)).unwrap();
        // compaction item 展开后作 user message 注入(具体 prefix 行为由
        // responses/request.rs::build_messages_from_input 控制),只要文本能在 grok
        // 看到 final message 里就 OK
        // 注:core merge_consecutive_user_messages 把"compaction 展开后的
        // user message"跟"后续 user message"合并成单 user 段落,所以
        // "User: and lifetimes?" 不是独立 prefix,而是跟 summary 同 user content
        // 内合并(用 \n 分隔)。grok 模型看到的是同一段 user context。
        assert!(
            conv.request.message.contains("Summary of previous turns"),
            "compaction summary missing, got: {}",
            conv.request.message
        );
        assert!(
            conv.request.message.contains("and lifetimes?"),
            "current user follow-up missing, got: {}",
            conv.request.message
        );
        // F5 修(review-feedback):不 assert `starts_with("User:")` —— 那是
        // core/responses `merge_consecutive_user_messages` 的内部实现细节,
        // grok adapter 不该耦合;弱化为"flatten 输出含至少一段 User: prefix"
        assert!(
            conv.request.message.contains("User:"),
            "flatten should include at least one `User:` prefix, got: {}",
            conv.request.message
        );
    }
}
