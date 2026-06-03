use super::tools::*;
use super::*;
use crate::types::AdapterError;
use codex_app_transfer_registry::Provider;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

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
        serialized.contains("image omitted"),
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
    assert!(serialized.contains("image omitted"));
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
fn minimax_merges_then_converts_system_to_user_prefix() {
    // issue #139 修(2026-05-12):MiniMax /v1/chat/completions 不接受
    // role=system,400 invalid role。先 merge_consecutive_system_messages
    // 合并(instructions + system message 拼一段),再 convert_minimax_system_to_user_prefix
    // 把 role 一次性转 user + content 前 `[System]\n` prefix。
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
    // 原 system message → 转 user role
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"], "[System]\nsystem one\n\nsystem two",
        "合并后的 system 段被转 user role + [System]\\n prefix"
    );
    // 原 user message 不动
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hi");
}

#[test]
fn minimax_sanitizes_invalid_tool_call_arguments_in_messages() {
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_bad_1",
                        "type": "function",
                        "function": {"name":"f1", "arguments": ""}
                    },
                    {
                        "id": "call_bad_2",
                        "type": "function",
                        "function": {"name":"f2", "arguments": "{bad-json"}
                    },
                    {
                        "id": "call_ok",
                        "type": "function",
                        "function": {"name":"f3", "arguments": "{\"k\":1}"}
                    }
                ]
            }
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    sanitize_minimax_chat_body(&mut body);
    let calls = body["messages"][0]["tool_calls"].as_array().unwrap();
    assert_eq!(calls[0]["function"]["arguments"], "{}");
    assert_eq!(calls[1]["function"]["arguments"], "{}");
    assert_eq!(calls[2]["function"]["arguments"], "{\"k\":1}");
}

#[test]
fn minimax_m3_keeps_tool_strict_but_m2_drops() {
    // 真机实测(2026-06-03):M3 接受 OpenAI strict function-calling(strict:true →
    // 200 + 正常 tool_call);M2.x 不接受、剥掉。strict 是严格 schema 输出保证。
    let mk = |model: &str| {
        json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f", "strict": true, "parameters": {"type": "object", "properties": {}}}}]
        })
        .as_object()
        .expect("json object")
        .clone()
    };
    let mut m3 = mk("MiniMax-M3");
    sanitize_minimax_chat_body(&mut m3);
    assert_eq!(
        m3["tools"][0]["function"]
            .get("strict")
            .and_then(|v| v.as_bool()),
        Some(true),
        "M3 应保留 tool function.strict(实测接受)"
    );
    let mut m2 = mk("MiniMax-M2.7");
    sanitize_minimax_chat_body(&mut m2);
    assert!(
        m2["tools"][0]["function"].get("strict").is_none(),
        "M2.x 应剥 tool function.strict"
    );
}

#[test]
fn minimax_m3_keeps_stream_options_but_m2_drops() {
    // #356 用户实测病态对话根因:proxy 删 stream_options → MiniMax 不返 usage →
    // Codex token 恒 0 → token-based auto-compact 失效 → 对话无限膨胀。M3 实测
    // (2026-06-03)streaming + include_usage 稳定返真实 usage,必须保留;M2.x 仍删。
    let mk = |model: &str| {
        json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        })
        .as_object()
        .expect("json object")
        .clone()
    };
    let mut m3 = mk("MiniMax-M3");
    sanitize_minimax_chat_body(&mut m3);
    assert_eq!(
        m3.get("stream_options")
            .and_then(|v| v.get("include_usage"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "M3 必须保留 stream_options.include_usage(否则 Codex token 恒 0、对话膨胀)"
    );
    let mut m2 = mk("MiniMax-M2.7");
    sanitize_minimax_chat_body(&mut m2);
    assert!(
        !m2.contains_key("stream_options"),
        "M2.x 仍删 stream_options(streaming 不稳定接受 include_usage)"
    );
}

#[test]
fn minimax_m3_keeps_system_and_standard_fields() {
    // 真机实测(2026-06-03,api.minimaxi.com 直连 MiniMax-M3):M3 原生接受
    // role=system / response_format / parallel_tool_calls(M2.x 同字段 400)。
    // sanitize 不应对 M3 做 M2.x 的 system→user 改写 + 字段剥离。
    let mut body = json!({
        "model": "MiniMax-M3",
        "messages": [
            {"role": "system", "content": "You are concise."},
            {"role": "user", "content": "hi"}
        ],
        "response_format": {"type": "json_object"},
        "parallel_tool_calls": true,
        "reasoning_effort": "high",
        "tool_choice": "required"
    })
    .as_object()
    .expect("json object")
    .clone();
    sanitize_minimax_chat_body(&mut body);
    let roles: Vec<String> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["role"].as_str().map(ToOwned::to_owned))
        .collect();
    assert!(
        roles.iter().any(|r| r == "system"),
        "M3 应保留 role=system(不转 user),实际 roles={roles:?}"
    );
    assert!(
        body.contains_key("response_format"),
        "M3 应保留 response_format(实测接受)"
    );
    assert!(
        body.contains_key("parallel_tool_calls"),
        "M3 应保留 parallel_tool_calls(实测接受)"
    );
    assert!(
        body.contains_key("reasoning_effort"),
        "M3 应保留 reasoning_effort(实测接受,按规范透传上游而非剥除)"
    );
    assert_eq!(
        body.get("tool_choice").and_then(|v| v.as_str()),
        Some("required"),
        "M3 应保留 tool_choice=required(实测支持,不降级到 auto)"
    );
}

#[test]
fn minimax_m2_still_converts_system_and_drops_unsupported_fields() {
    // 回归保护:M2.x 必须保持 #139 的 system→user + 字段剥离(M3 的宽松路径
    // 不能误伤 M2.x)。
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hi"}
        ],
        "response_format": {"type": "json_object"},
        "parallel_tool_calls": true
    })
    .as_object()
    .expect("json object")
    .clone();
    sanitize_minimax_chat_body(&mut body);
    let roles: Vec<String> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["role"].as_str().map(ToOwned::to_owned))
        .collect();
    assert!(
        !roles.iter().any(|r| r == "system"),
        "M2.x 必须把 system 转成 user(#139),实际 roles={roles:?}"
    );
    assert!(
        !body.contains_key("response_format"),
        "M2.x(非 Text-01)必须剥 response_format"
    );
    assert!(
        !body.contains_key("parallel_tool_calls"),
        "M2.x 必须剥 parallel_tool_calls"
    );
}

#[test]
fn minimax_long_system_split_into_multiple_user_prefix_messages() {
    // issue #139 修:超 max_chars 切片,每片独立 role=user + 标记部分编号
    // `[System part i/N]\n` prefix(silent-failure F4:让模型看出是同一逻辑
    // 段落的连续分片);单段不切则用 `[System]\n`。
    //
    // chatgpt-codex P1 修后,budget 算法减去 prefix 长度:max_chars=50,
    // prefix `[System part i/N]\n` static 16 char + 2*N digit;N=4 时 prefix
    // 长度 16+2 = 18 → budget=32。system 121 char(60+1+60)→ 切 4 段
    // (32+32+32+25)。
    let long_a = "a".repeat(60);
    let long_b = "b".repeat(60);
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role":"system","content": format!("{long_a}\r\n{long_b}")},
            {"role":"user","content":"hi"}
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    convert_minimax_system_to_user_prefix(&mut body, 50);
    let messages = body["messages"].as_array().unwrap();
    let split_count = messages.len() - 1; // 最后一条是 follow user
    assert!(
        split_count >= 3,
        "121 char 应至少切 3 段,实际 {split_count} 段"
    );
    for (i, msg) in messages.iter().enumerate() {
        assert_eq!(msg["role"], "user", "msg {i} should be user");
        // **chatgpt-codex P1 invariant**:每条 user message(含 prefix)≤ max_chars
        let len = msg["content"].as_str().unwrap().chars().count();
        assert!(
            len <= 50,
            "msg {i} len {len} > max_chars 50,违反 P1 invariant"
        );
    }
    assert_eq!(
        messages.last().unwrap()["content"],
        "hi",
        "last 应是 follow user"
    );
    // 验证 prefix marker + 重组原文(N 由实际切片数决定)
    let mut joined = String::new();
    for i in 0..split_count {
        let content = messages[i]["content"].as_str().unwrap();
        let expected_prefix = format!("[System part {}/{split_count}]\n", i + 1);
        let chunk = content.strip_prefix(&expected_prefix).unwrap_or_else(|| {
            panic!("chunk {i} missing prefix {expected_prefix:?}, got: {content}")
        });
        joined.push_str(chunk);
    }
    assert_eq!(joined, format!("{long_a}\n{long_b}"));
    assert!(!joined.contains('\r'));
}

#[test]
fn minimax_system_content_as_responses_array_form_extracts_text() {
    // silent-failure F2 修:Codex CLI Responses spec content 数组形
    // `[{type:"input_text", text:"..."}]` 必须抽 text 字段,不能 raw JSON stringify
    // 塞 `[System]\n[{"type":"input_text",...}]` 给模型(那会让模型看到 JSON 噪音)
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role":"system","content": [
                {"type":"input_text","text":"line 1"},
                {"type":"input_text","text":"line 2"}
            ]},
            {"role":"user","content":"q"}
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    convert_minimax_system_to_user_prefix(&mut body, 1000);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"], "[System]\nline 1\nline 2",
        "array-form content 应抽 text join \\n,不能 raw JSON stringify"
    );
}

#[test]
fn minimax_system_content_empty_array_or_no_text_parts_skipped() {
    // silent-failure F2 边界:array 有 parts 但全是 image / 无 text 字段 → skip,
    // 不注入 raw JSON 给模型
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role":"system","content": [
                {"type":"input_image","image_url":"x"}
            ]},
            {"role":"user","content":"q"}
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    convert_minimax_system_to_user_prefix(&mut body, 1000);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "image-only system 跳过,只剩 user");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "q");
}

#[test]
fn minimax_chunked_messages_total_length_within_max_chars() {
    // **chatgpt-codex P1 不变量验证**(2026-05-12):chunk + prefix 后**每条
    // emitted user message 的 chars().count() ≤ max_chars**。之前直接
    // split(content, max_chars) 后加 prefix → 每条 ≈ max_chars + 22 char
    // 超限,MiniMax 仍 400。本测试用极端长 content + 小 max_chars 验证。
    let long = "a".repeat(1_000);
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role":"system","content": long.clone()},
            {"role":"user","content":"q"}
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    const MAX: usize = 100;
    convert_minimax_system_to_user_prefix(&mut body, MAX);
    let messages = body["messages"].as_array().unwrap();
    // 全部 user role,no system
    for (i, msg) in messages.iter().enumerate() {
        assert_eq!(msg["role"], "user", "msg {i} should be user");
        let content_len = msg["content"].as_str().unwrap().chars().count();
        // **关键 invariant**:整条 user message(含 prefix)≤ MAX
        assert!(
            content_len <= MAX,
            "msg {i} content len {} > MAX {}, violates chatgpt-codex P1 invariant",
            content_len,
            MAX,
        );
    }
    // 验证 last user message 不变(role=user 但不是 split chunk)
    let last = messages.last().unwrap();
    assert_eq!(last["content"], "q");
}

#[test]
fn minimax_integration_long_system_through_public_entry() {
    // silent-failure F7:整合 test 走 public entry `responses_body_to_chat_body_for_provider`
    // (含 build_messages_from_input + merge_consecutive_system + sanitize_minimax_chat_body
    // 全链路),覆盖长 system 切片场景。MAX_CHARS=24000,我们用 30000 字符 system 触发切片。
    let long = "x".repeat(30_000);
    let req = json!({
        "model": "MiniMax-M2.7",
        "stream": true,
        "instructions": long.clone(),
        "input": [{"type":"message","role":"user","content":"q"}]
    });
    let p = minimax_provider();
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let messages = out["messages"].as_array().unwrap();
    // 至少 2 段 system(30000 / 24000 = 2 切片)+ 1 user → ≥3 messages
    assert!(messages.len() >= 3, "30000 char instructions 应切 ≥2 段");
    // 全部应是 user role(没 system 残留)
    for msg in messages {
        assert_ne!(msg["role"], "system");
    }
    // 第一段含 [System part i/N] prefix
    let first_content = messages[0]["content"].as_str().unwrap();
    assert!(
        first_content.starts_with("[System part 1/"),
        "切片第一段应带 part 1/N marker,got: {}",
        &first_content[..40.min(first_content.len())]
    );
}

#[test]
fn minimax_system_role_completely_eliminated_in_final_body() {
    // issue #139 防御:sanitize 后 messages 数组里**绝不应**有任何 role=system
    // (MiniMax API 直接 400 拒绝 role=system)
    let req = json!({
        "model": "MiniMax-M2.7",
        "stream": true,
        "instructions": "outer system",
        "input": [
            {"type":"message","role":"system","content":"inner system"},
            {"type":"message","role":"user","content":"q"}
        ]
    });
    let p = minimax_provider();
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let messages = out["messages"].as_array().unwrap();
    for msg in messages {
        assert_ne!(
            msg["role"], "system",
            "MiniMax sanitize 后绝不应保留 role=system,违反 MiniMax /v1/chat/completions API"
        );
    }
}

#[test]
fn minimax_empty_system_content_skipped() {
    // 防御:空 system content 不发 raw `[System]\n` 空 user message
    let mut body = json!({
        "model": "MiniMax-M2.7",
        "messages": [
            {"role":"system","content":""},
            {"role":"user","content":"q"}
        ]
    })
    .as_object()
    .expect("json object")
    .clone();
    convert_minimax_system_to_user_prefix(&mut body, 1000);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "空 system 应跳过");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "q");
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

// ── ensure_text_part_when_image_present 兜底:MiMo 文档强制要求图存在
// 时 content 至少有 1 个 text part,否则 400 "Param Incorrect: text is
// not set"。借鉴 7as0nch/mimo2codex reqToChat.ts:71-79。
// 对其他 supports_vision provider (Kimi / OpenAI 等) 无副作用,统一处理。

#[test]
fn mimo_image_only_message_gets_text_part_appended() {
    // MiMo vision 模型 + 仅 image 的 user 消息(用户粘图未输入文字)→
    // 必须在 content 末尾追加 {type:"text", text:" "} 兜底
    let mut mimo = mimo_provider();
    mimo.models.insert("default".into(), "mimo-v2.5".into());
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{
            "type":"message","role":"user","content":[
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        }]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&mimo)).unwrap();
    let messages = out["messages"].as_array().unwrap();
    let content = messages[0]["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")),
        "兜底 text part 必须存在,否则 MiMo 400 Param Incorrect\nactual: {content:?}"
    );
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("image_url")),
        "原 image_url 必须保留\nactual: {content:?}"
    );
}

#[test]
fn mimo_image_with_existing_text_part_unchanged() {
    // 用户既贴了图也输了字 → 原 text part 已存在,不应再追加
    let mut mimo = mimo_provider();
    mimo.models.insert("default".into(), "mimo-v2.5".into());
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{
            "type":"message","role":"user","content":[
                {"type":"input_text","text":"图里是什么"},
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        }]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&mimo)).unwrap();
    let messages = out["messages"].as_array().unwrap();
    let content = messages[0]["content"].as_array().unwrap();
    let text_blocks: Vec<&Value> = content
        .iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        .collect();
    assert_eq!(
        text_blocks.len(),
        1,
        "已有 text 时不应重复追加,只该有 1 个 text block\nactual: {content:?}"
    );
    assert_eq!(
        text_blocks[0].get("text").and_then(|v| v.as_str()),
        Some("图里是什么"),
        "原 text 内容必须保留,不能被空格 text 覆盖"
    );
}

#[test]
fn kimi_image_only_message_also_gets_text_part_appended() {
    // 兜底统一对所有 supports_vision provider 应用(避免 per-provider
    // 分支),Kimi 也加。空格 text 对 Kimi 无副作用 — 验证不会影响其
    // image_url 保留。
    let mut kimi = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
    kimi.models.insert("default".into(), "kimi-k2.6".into());
    let req = json!({
        "model": "kimi-k2.6",
        "stream": true,
        "input": [{
            "type":"message","role":"user","content":[
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        }]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&kimi)).unwrap();
    let content = out["messages"][0]["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")),
        "Kimi 也走兜底统一处理(无副作用)"
    );
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("image_url")),
        "image_url 必须保留"
    );
}

#[test]
fn text_only_provider_image_only_still_strips_to_placeholder() {
    // 非 supports_vision provider(deepseek-v4-pro)+ 仅 image →
    // 走 strip 路径,不该被 ensure_text_part 兜底干扰(strip 已自带
    // 占位文本 "image omitted")
    let req = json!({
        "model": "deepseek-v4-pro",
        "stream": true,
        "input": [{
            "type":"message","role":"user","content":[
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]
        }]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&deepseek_provider())).unwrap();
    let serialized = serde_json::to_string(&out["messages"]).unwrap();
    assert!(
        !serialized.contains("\"image_url\""),
        "DeepSeek 必须 strip 掉 image_url"
    );
    assert!(
        serialized.contains("image omitted"),
        "占位文本必须存在(strip 路径,而非 ensure_text 兜底空格)"
    );
    // ensure_text_part 不应被调用(走的是 strip 分支)
    assert!(
        !serialized.contains(r#""text":" ""#),
        "走 strip 分支时,不应额外追加空格 text"
    );
}

// ── namespace 工具递归展平(借鉴 7as0nch/mimo2codex reqToChat.ts:232-250)
// Codex CLI 用 type:"namespace" 包装 MCP server 工具集,内层是具体
// type:"function"。第三方 chat completions provider 不认 namespace,必须
// 展平为顶级 function 数组。实测每轮 218 个 namespace × 88 内层 function
// 被旧版 `_ => None` 整个 drop,模型完全看不到 MCP 具体 tools。

#[test]
fn namespace_with_two_inner_functions_flattens_to_two_function_tools() {
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type": "namespace", "name": "mcp__cloudflare_docs__",
             "description": "Tools in the mcp__cloudflare_docs__ namespace.",
             "tools": [
                {"type":"function","name":"migrate_pages_to_workers_guide",
                 "description":"Read this guide before migrating.",
                 "parameters":{"type":"object","properties":{},"additionalProperties":false},
                 "strict":false},
                {"type":"function","name":"search_cloudflare_documentation",
                 "description":"Search the Cloudflare documentation.",
                 "parameters":{"type":"object","properties":{
                    "query":{"type":"string"}},"required":["query"]},
                 "strict":false}
             ]}
        ]
    });
    let out = convert(req);
    let tools = out["tools"].as_array().expect("tools array present");
    assert_eq!(
        tools.len(),
        2,
        "namespace 内层 2 个 function 必须展平为 2 个顶级 tool"
    );
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"migrate_pages_to_workers_guide"));
    assert!(names.contains(&"search_cloudflare_documentation"));
    // namespace 包装的 name (mcp__cloudflare_docs__) 不该作为顶级工具出现
    assert!(
        !names.contains(&"mcp__cloudflare_docs__"),
        "namespace 包装名不该泄漏成 tool name"
    );
}

#[test]
fn namespace_alongside_top_level_function_both_kept() {
    // 实测真实场景:tools 数组同时含顶级 function + namespace 包,展平
    // 后总数 = 顶级 function 数 + 所有 namespace 内层 function 数
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"function","name":"shell",
             "description":"Run shell command.",
             "parameters":{"type":"object","properties":{}}},
            {"type":"namespace","name":"mcp__notion__","tools":[
                {"type":"function","name":"notion_search","description":"",
                 "parameters":{"type":"object","properties":{}}},
                {"type":"function","name":"notion_create_pages","description":"",
                 "parameters":{"type":"object","properties":{}}}
            ]}
        ]
    });
    let out = convert(req);
    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains(&"shell"));
    assert!(names.contains(&"notion_search"));
    assert!(names.contains(&"notion_create_pages"));
}

#[test]
fn namespace_with_empty_tools_array_silently_dropped() {
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"namespace","name":"mcp__empty__","tools": []}
        ]
    });
    let out = convert(req);
    // 空 namespace 不该出现在 tools 数组里;若没其他 tools,整个 tools key
    // 不应进 result(对齐"chat_tools.is_empty() 时 skip insert"逻辑)。
    assert!(out.get("tools").is_none() || out["tools"].as_array().unwrap().is_empty());
}

#[test]
fn namespace_missing_tools_field_silently_dropped() {
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"namespace","name":"mcp__broken__"}  // 无 tools 字段
        ]
    });
    let out = convert(req);
    assert!(out.get("tools").is_none() || out["tools"].as_array().unwrap().is_empty());
}

#[test]
fn nested_namespace_inside_namespace_recursively_flattens() {
    // 边界:虽然实测 Codex CLI 当前不嵌套 namespace,但实现走的是递归
    // flat_map,理应正确处理。future-safe 验证。
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"namespace","name":"outer","tools":[
                {"type":"namespace","name":"inner","tools":[
                    {"type":"function","name":"deep_tool","description":"",
                     "parameters":{"type":"object","properties":{}}}
                ]},
                {"type":"function","name":"sibling","description":"",
                 "parameters":{"type":"object","properties":{}}}
            ]}
        ]
    });
    let out = convert(req);
    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["deep_tool", "sibling"]);
}

// ── MOC-32 / GH #288: tool_search converter ──────────────────────────
//
// Codex 0.130+ 把 MCP server tools 全 defer 到 `tool_search` builtin
// (`Feature::ToolSearchAlwaysDeferMcpTools`)。chat completions 上游不认
// `type=tool_search`,必须降级成 chat `function` tool,LLM 调用后由
// converter.rs::close_tool_call 把 chat function_call wire 重打包成 Codex
// `tool_search_call` wire。本节测试请求侧 wire 转换正确性。

#[test]
fn convert_tool_search_to_chat_function() {
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{
            "type": "tool_search",
            "execution": "client",
            "description": "# Tool discovery\n\nSearches over deferred tool metadata with BM25...",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"]
            }
        }]
    });
    let out = convert(req);
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "tool_search 应转成 1 个 chat function tool");
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "tool_search");
    assert!(
        tools[0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("BM25"),
        "description 应完整透传 BM25 关键词"
    );
    let params = &tools[0]["function"]["parameters"];
    assert_eq!(params["type"], "object");
    assert_eq!(params["properties"]["query"]["type"], "string");
    assert_eq!(params["required"][0], "query");
}

#[test]
fn convert_tool_search_with_empty_description_does_not_panic() {
    let req = json!({
        "model": "x",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type": "tool_search", "execution": "client"}]
    });
    let out = convert(req);
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "tool_search");
    assert_eq!(tools[0]["function"]["description"], "");
    assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
}

#[test]
fn convert_tool_search_alongside_other_types() {
    let req = json!({
        "model": "x",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type": "function", "name": "fn1", "description": "", "parameters": {}},
            {"type": "tool_search", "execution": "client", "description": "discovery", "parameters": {}},
            {"type": "custom", "name": "apply_patch", "description": "", "format": {}},
            {"type": "namespace", "name": "codex_app", "tools": [
                {"type": "function", "name": "ns_inner", "description": "", "parameters": {}}
            ]}
        ]
    });
    let out = convert(req);
    let tools = out["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"tool_search"), "tool_search 应在 outbound");
    assert!(names.contains(&"fn1"));
    assert!(names.contains(&"apply_patch"));
    assert!(names.contains(&"ns_inner"), "namespace 内层 fn 应展平");
}

#[test]
fn unknown_tool_type_dropped_via_warn_once_path_does_not_panic() {
    // web_search / file_search / code_interpreter / image_generation 等
    // Responses 专属 server-side 工具在第三方 chat 端无等价,继续 drop。
    // 验证:不 panic,不出现在 outbound,与已有 type:"function" 共存。
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search","external_web_access":true,
             "search_content_types":["text","image"]},
            {"type":"file_search","vector_store_ids":["xx"]},
            {"type":"function","name":"keep_me","description":"",
             "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = convert(req);
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "只 keep_me 这个 function 应保留");
    assert_eq!(tools[0]["function"]["name"], "keep_me");
}

// ── web_search 工具 per-provider 适配 — MiMo 阶段 ─────────────────
// Codex.app 入站默认每轮发 `{type:"web_search", external_web_access:true,
// search_content_types:["text","image"]}`(实测 dump),代理把这个统一
// 形态转成各上游 chat API 真实支持的形态。本批仅 MiMo 实施(1:1 复刻
// mimo2codex `reqToChat.ts:196-209`),Kimi/DeepSeek/MiniMax 等留 follow-up
// (逐家文档实证后跟进,见 `docs/web-search-implementation-tracker.md`)。

/// MiMo provider 用于 web_search 测试 — 显式 enable Web Search Plugin。
/// A 层默认 false,测试需要显式开才会触发转换。
fn mimo_provider_with_web_search() -> Provider {
    let mut p = mimo_provider();
    p.models.insert("default".into(), "mimo-v2.5".into());
    p.request_options
        .insert("web_search_enabled".into(), json!(true));
    p
}

#[test]
fn mimo_web_search_converted_to_native_schema_with_user_location() {
    let p = mimo_provider_with_web_search();
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"搜索 X 最新进展"}],
        "tools": [
            {
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text", "image"],
                "user_location": {
                    "type": "approximate",
                    "country": "CN",
                    "city": "Shanghai"
                },
                "max_keyword": 5,
                "force_search": true,
                "limit": 10
            }
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(
        tool["type"], "web_search",
        "MiMo chat 端原生 type:web_search"
    );
    assert_eq!(tool["user_location"]["country"], "CN");
    assert_eq!(tool["user_location"]["city"], "Shanghai");
    assert_eq!(tool["max_keyword"], 5);
    assert_eq!(tool["force_search"], true);
    assert_eq!(tool["limit"], 10);
    // OpenAI 的 external_web_access / search_content_types 在 MiMo 无等价,silent drop
    assert!(
        tool.get("external_web_access").is_none(),
        "external_web_access 在 MiMo 无等价,必须 silent drop"
    );
    assert!(
        tool.get("search_content_types").is_none(),
        "search_content_types 在 MiMo 无等价,必须 silent drop"
    );
}

#[test]
fn mimo_web_search_with_minimal_fields_outputs_minimal_tool() {
    // 用户没传 user_location / max_keyword 等字段时,只输出 type:"web_search"
    let p = mimo_provider_with_web_search();
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search", "external_web_access": true, "search_content_types": ["text"]}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let keys: Vec<&String> = tools[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec![&"type".to_string()], "无可选字段时只剩 type");
    assert_eq!(tools[0]["type"], "web_search");
}

#[test]
fn mimo_web_search_preview_alias_handled_same_as_web_search() {
    // Codex.app 历史上有过 web_search_preview / web_search 两种 type,
    // mimo2codex `reqToChat.ts:196` 同样处理,我们也照抄。
    let p = mimo_provider_with_web_search();
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search_preview"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    assert_eq!(out["tools"][0]["type"], "web_search");
}

#[test]
fn non_mimo_provider_web_search_dropped_via_warn_once() {
    // Kimi / DeepSeek / MiniMax 等 provider 暂未文档实证,走 drop + warn_once。
    // 用户实际会看到模型走 P5 修通的 namespace MCP 工具(如 Node Repl)绕路
    // 联网搜索;后续逐家文档实证后再加映射。
    let mut kimi = provider("kimi", "Kimi", "https://api.moonshot.cn/v1");
    kimi.models.insert("default".into(), "kimi-k2.6".into());
    let req = json!({
        "model": "kimi-k2.6",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search", "external_web_access": true, "search_content_types": ["text"]},
            {"type":"function", "name":"keep_me", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&kimi)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "Kimi 暂未实施,web_search drop 只剩 keep_me");
    assert_eq!(tools[0]["function"]["name"], "keep_me");
}

#[test]
fn web_search_with_no_provider_context_dropped() {
    // 极端情况:没有 provider 上下文(应该不发生,resolver 必填),
    // 安全 drop 不 panic
    let req = json!({
        "model": "any",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = convert(req);
    // 没 provider 时整个 web_search drop,tools 字段不存在(empty 数组不写入)
    assert!(out.get("tools").is_none() || out["tools"].as_array().unwrap().is_empty());
}

// ── A 层(provider 配置开关)──
// `request_options.web_search_enabled` 默认 false,用户必须显式标 true。
// 默认关闭原因:很多 provider(如 MiMo Token Plan)没开 plugin 时发
// web_search 工具会触发上游 400。

#[test]
fn mimo_provider_without_web_search_enabled_drops_web_search_by_default() {
    // 默认状态:mimo_provider() 没设 web_search_enabled → 视为 false → drop
    let mut p = mimo_provider();
    p.models.insert("default".into(), "mimo-v2.5".into());
    // 故意不设 web_search_enabled
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search", "external_web_access": true},
            {"type":"function", "name":"keep_me", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        1,
        "默认 web_search_enabled=false → web_search 被 A 层 drop"
    );
    assert_eq!(tools[0]["function"]["name"], "keep_me");
}

#[test]
fn mimo_provider_with_explicit_web_search_enabled_false_drops_web_search() {
    // 显式标 false 跟没设效果一致 — 都触发 A 层 drop
    let mut p = mimo_provider();
    p.models.insert("default".into(), "mimo-v2.5".into());
    p.request_options
        .insert("web_search_enabled".into(), json!(false));
    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    assert!(out.get("tools").is_none() || out["tools"].as_array().unwrap().is_empty());
}

// ── B 层(运行时自动 disable cache)──
// `crate::disable_web_search_for(provider_id)` 后,即使配置 web_search_enabled=true,
// 同 provider id 后续转换也立即 drop。模拟 forward.rs 4xx fallback 后的行为。

#[test]
fn b_layer_runtime_disable_blocks_subsequent_web_search_conversion() {
    let mut p = mimo_provider_with_web_search();
    p.id = "mimo-runtime-disable-test".into();
    // 模拟 forward.rs 4xx fallback 调用
    crate::disable_web_search_for(&p.id);

    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search"},
            {"type":"function", "name":"keep_me", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        1,
        "运行时 disable cache 命中 → web_search 被 B 层 drop,只剩 keep_me"
    );
    assert_eq!(tools[0]["function"]["name"], "keep_me");
    assert!(crate::is_web_search_disabled_for(&p.id));
}

#[test]
fn b_layer_runtime_disable_only_affects_targeted_provider_id() {
    // disable provider A 不影响 provider B(各自 cache 隔离)
    let mut a = mimo_provider_with_web_search();
    a.id = "mimo-disable-a".into();
    let mut b = mimo_provider_with_web_search();
    b.id = "mimo-untouched-b".into();
    crate::disable_web_search_for(&a.id);

    let req = json!({
        "model": "mimo-v2.5",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out_b = responses_body_to_chat_body_for_provider(&req, Some(&b)).unwrap();
    // b 的 web_search_enabled=true 且没被 disable,正常转换
    assert_eq!(out_b["tools"][0]["type"], "web_search");
}

// ── Kimi (Moonshot) web_search builtin_function 映射 ──
// 来源:WebFetch `platform.kimi.ai/docs/guide/use-web-search` 真原文实证。
// 1:1 复刻官方文档:tools 形态固定 `{type:"builtin_function", function:{name:"$web_search"}}`,
// 强制配套 `thinking:{type:"disabled"}` 顶级字段(Kimi 文档明确强制)。

fn kimi_provider_with_web_search() -> Provider {
    let mut p = provider(
        "kimi-for-coding",
        "Kimi For Coding",
        "https://api.kimi.com/coding/v1",
    );
    p.models.insert("default".into(), "kimi-for-coding".into());
    p.api_format = "openai_chat".into();
    p.request_options
        .insert("web_search_enabled".into(), json!(true));
    p
}

fn moonshot_provider_with_web_search() -> Provider {
    let mut p = provider("moonshot", "Moonshot", "https://api.moonshot.cn/v1");
    p.models.insert("default".into(), "kimi-k2.6".into());
    p.api_format = "openai_chat".into();
    p.request_options
        .insert("web_search_enabled".into(), json!(true));
    p
}

#[test]
fn kimi_web_search_outputs_builtin_function_with_dollar_prefix_name() {
    let p = kimi_provider_with_web_search();
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"搜索 X"}],
        "tools": [
            {
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text", "image"],
                "user_location": {"country": "CN"},
                "max_keyword": 5
            }
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    // Kimi 形态:固定 builtin_function + $web_search,**不透传任何子字段**
    assert_eq!(tools[0]["type"], "builtin_function");
    assert_eq!(tools[0]["function"]["name"], "$web_search");
    // OpenAI 字段全部 silent drop(Kimi 文档明确只要 type + function.name)
    assert!(tools[0].get("user_location").is_none());
    assert!(tools[0].get("max_keyword").is_none());
    assert!(tools[0].get("external_web_access").is_none());
    assert!(tools[0].get("search_content_types").is_none());
}

#[test]
fn kimi_web_search_force_injects_thinking_disabled_top_level_field() {
    let p = kimi_provider_with_web_search();
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    // Kimi 文档强制:`thinking:{type:"disabled"}` 顶级字段必填
    assert_eq!(
        out["thinking"],
        json!({"type": "disabled"}),
        "Kimi $web_search 必须配套 thinking disabled(官方文档强制)"
    );
}

#[test]
fn moonshot_provider_uses_same_kimi_web_search_form() {
    // moonshot.cn / kimi.ai 同公司,provider_looks_like("moonshot") 同样命中
    let p = moonshot_provider_with_web_search();
    let req = json!({
        "model": "kimi-k2.6",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    assert_eq!(out["tools"][0]["type"], "builtin_function");
    assert_eq!(out["tools"][0]["function"]["name"], "$web_search");
    assert_eq!(out["thinking"], json!({"type": "disabled"}));
}

#[test]
fn kimi_without_web_search_enabled_does_not_inject_thinking() {
    // 未启用 web_search 时不该强制 disable thinking(用户原 thinking 配置不变)
    let mut p = provider(
        "kimi-for-coding",
        "Kimi For Coding",
        "https://api.kimi.com/coding/v1",
    );
    p.models.insert("default".into(), "kimi-for-coding".into());
    // 故意不设 web_search_enabled
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search"},
            {"type":"function", "name":"shell", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    // web_search 被 A 层 drop(默认关),不该注入 thinking disabled
    assert!(
        out.get("thinking").is_none(),
        "未启用 web_search 时不该注入 thinking disabled,实际: {:?}",
        out.get("thinking")
    );
    // shell function 仍然保留
    assert_eq!(out["tools"][0]["function"]["name"], "shell");
}

#[test]
fn kimi_web_search_b_layer_runtime_disable_skips_thinking_injection() {
    // B 层 cache 命中(运行时已 disable)→ web_search drop → 不该注入 thinking
    let mut p = kimi_provider_with_web_search();
    p.id = "kimi-runtime-disabled".into();
    crate::disable_web_search_for(&p.id);
    let req = json!({
        "model": "kimi-for-coding",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    assert!(
        out.get("thinking").is_none(),
        "B 层 cache disable 后 web_search drop,thinking 不该注入"
    );
}

// ── DeepSeek web_search drop(文档实证不支持)──
// 来源:WebFetch `api-docs.deepseek.com/api/create-chat-completion` 真原文
// (2026-05-09):"Currently, only `function` is supported." DeepSeek chat
// completions tools 数组只接受 type:"function",无任何 server-side web 搜索。

#[test]
fn deepseek_web_search_dropped_with_explicit_warn_key() {
    // DeepSeek 即使 web_search_enabled=true 也 drop(API 不支持)
    let mut p = deepseek_provider();
    p.request_options
        .insert("web_search_enabled".into(), json!(true));
    let req = json!({
        "model": "deepseek-v4-pro",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search"},
            {"type":"function", "name":"keep_me", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        1,
        "DeepSeek API 不支持 web_search,只剩 keep_me function"
    );
    assert_eq!(tools[0]["function"]["name"], "keep_me");
    // DeepSeek 不应触发 Kimi thinking 注入(它跟 thinking-disabled 路径无关)
    assert!(out.get("thinking").is_none());
}

// ── MiniMax web_search drop(文档实证不支持)──
// 来源:WebFetch `platform.minimaxi.com/docs/api-reference/` + liteLLM
// MiniMax provider 文档(2026-05-09):MiniMax chat completions tools 只接受
// type:"function",无内置 web_search;web_search 仅作 Token Plan MCP 工具存在。

#[test]
fn minimax_web_search_dropped_with_explicit_warn_key() {
    let mut p = minimax_provider();
    p.request_options
        .insert("web_search_enabled".into(), json!(true));
    let req = json!({
        "model": "MiniMax-M2.7",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [
            {"type":"web_search"},
            {"type":"function", "name":"keep_me", "parameters":{"type":"object","properties":{}}}
        ]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        1,
        "MiniMax chat API 不支持 web_search,只剩 keep_me function"
    );
    assert_eq!(tools[0]["function"]["name"], "keep_me");
    // MiniMax 不应触发 Kimi thinking 注入(它跟 thinking-disabled 路径无关)
    assert!(out.get("thinking").is_none());
}

#[test]
fn deepseek_web_search_drop_independent_of_web_search_enabled_flag() {
    // 即使用户显式标 web_search_enabled=false / 不标,DeepSeek 都 drop
    // (其实只是 DeepSeek 不支持的硬实事,跟 A 层无关)
    let p = deepseek_provider(); // 默认未标 web_search_enabled
    let req = json!({
        "model": "deepseek-v4-pro",
        "stream": true,
        "input": [{"type":"message","role":"user","content":"hi"}],
        "tools": [{"type":"web_search"}]
    });
    let out = responses_body_to_chat_body_for_provider(&req, Some(&p)).unwrap();
    assert!(
        out.get("tools").is_none() || out["tools"].as_array().unwrap().is_empty(),
        "DeepSeek 默认未启用 web_search 时,A 层先 drop(走 disabled-by-config 路径)"
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
    let out = responses_body_to_chat_body_for_provider(&req, Some(&deepseek_with_vision)).unwrap();
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
    assert!(serialized.contains("image omitted"), "应该用占位文本替换");
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

#[test]
fn function_call_without_arguments_defaults_to_json_object() {
    let out = convert(json!({
        "input": [{
            "type": "function_call",
            "call_id": "call_no_args",
            "name": "noop"
        }]
    }));
    let msg = &out["messages"][0];
    assert_eq!(msg["tool_calls"][0]["function"]["arguments"], "{}");
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
    assert_eq!(messages[0]["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "call_abc");
    assert_eq!(messages[1]["content"], "sunny");
}

#[test]
fn apply_patch_chat_path_guidance_injected_when_tool_registered() {
    // 真机稳定性测试发现:即使 wire 桥接通了 + tool description 有 V4A
    // 规则,DeepSeek 在 chat-path 上仍会反复尝试错误的 anchor / Add+Update
    // 组合 / 空文件 Update 等无效路径,平均每次任务摸索 1-3 分钟。为节省
    // tokens 和提升首次成功率,adapter 在 tools 数组里注册了 apply_patch
    // 的 turn 注入一段独立 system message 告知 chat-path 实战 workaround。
    //
    // **#262 Devin BUG-004 fix**:此 test 断言英文 guidance 字面,必须持
    // `LANG_TEST_LOCK` 锁住 USER_LANGUAGE = "en",否则跟新 i18n test 设的
    // "zh" 并发 race 时会读到中文 guidance → 英文 assertion 失败。
    with_user_language("en", || {
        let out = convert(json!({
            "input": [{"type": "message", "role": "user", "content": "edit foo.py"}],
            "instructions": "You are a coding assistant.",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Use the `apply_patch` tool to edit files."
            }]
        }));
        let messages = out["messages"].as_array().unwrap();

        // Codex CLI 原 instructions 必须保留在第一条
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("coding assistant"),
            "Codex 原 instructions 不应被覆盖"
        );

        // 紧跟在 Codex instructions 之后必须有一条 adapter-injected guidance
        assert_eq!(messages[1]["role"], "system");
        let guidance = messages[1]["content"].as_str().unwrap_or_default();
        assert!(
            guidance.contains("apply_patch chat-path guidance"),
            "注入的指引必须带可识别 marker:{guidance}"
        );
        // 关键规则覆盖(round 4 capture 实证根因修复后):
        // (1) `@@` 单端语法(NEVER trailing `@@`)— 旧版双端误导已删除
        // 注意:用精确大写 "NEVER add a trailing" 匹配本规则,不能用小写 "never"
        // (会被 "whenever" 等无关词的子串误命中,Devin pre-merge review 修复)。
        assert!(
            guidance.contains("SINGLE-SIDED") && guidance.contains("NEVER add a trailing"),
            "guidance 必须强调 @@ 单端语法 + 禁尾随 @@:{guidance}"
        );
        assert!(
            !guidance.contains("EMPTY LINE as the `@@` anchor"),
            "旧版 EMPTY LINE anchor 误导已删除:{guidance}"
        );
        // (2) Add File 全 `+` 前缀(对抗 `def main():` 当 invalid hunk header)
        assert!(
            guidance.contains("prefix EVERY line") && guidance.contains("`+`"),
            "guidance 必须强调 Add File 全 `+` 前缀:{guidance}"
        );
        // (3) byte-exact matching
        assert!(
            guidance.contains("byte-for-byte"),
            "guidance 必须含 byte-exact 匹配规则:{guidance}"
        );
        // (4) Add+Update 同 path conflict
        assert!(guidance.contains("Add File") && guidance.contains("Update File"));
        // (5) 空文件 + lone `+` APPEND + Delete+Add fallback 兜底
        assert!(guidance.contains("empty file") || guidance.contains("totally empty"));
        assert!(guidance.contains("APPEND") || guidance.contains("append"));
        assert!(
            guidance.contains("Delete File + Add File"),
            "guidance 必须含 Update 反复失败时 fallback 到 Delete+Add 兜底:{guidance}"
        );
        // Round 7 实证修复(t0002 漏写 Begin Patch + t0020 Move 空 hunk):
        assert!(
            guidance.contains("`*** Begin Patch` MUST be the literal first line"),
            "guidance 必须显式要求 Begin Patch 是第一行:{guidance}"
        );
        assert!(
            guidance.contains("Move to:") && guidance.contains("REQUIRES at least one hunk"),
            "guidance 必须显式要求 Move to 配非空 hunk:{guidance}"
        );
        assert!(
            guidance.contains("For pure rename"),
            "guidance 必须给纯重命名的 Delete+Add 替代方案:{guidance}"
        );
        // 用户实测反馈(2026-05-22):模型在"几乎全替换"场景倾向用 `cat > file`
        // 绕过 apply_patch。prompt 必须强 normative 明示:任何文件操作走 apply_patch,
        // 全文 rewrite 用 Delete File + Add File。
        assert!(
            guidance.contains("ALWAYS use the `apply_patch` tool")
                && guidance.contains("NEVER use shell"),
            "guidance 必须强 normative 禁 shell redirect:{guidance}"
        );
        assert!(
            guidance.contains("full-file rewrites")
                && guidance.contains("`*** Delete File:")
                && guidance.contains("`*** Add File:"),
            "guidance 必须明示 large rewrite 走 Delete File + Add File:{guidance}"
        );
        // 必须含 rule 5 `printf '\n' > <path>` carve-out 明示
        // (防 Devin pre-merge review 报跟 rule 5 冲突,2026-05-22)
        assert!(
            guidance.contains("narrow exception in rule 5")
                && guidance.contains("preparation step, not a content bypass"),
            "guidance 必须 carve-out rule 5 的 `printf '\\n' > <path>` setup 用法:{guidance}"
        );
    });
}

#[test]
fn apply_patch_chat_path_guidance_skipped_when_tool_not_registered() {
    // 非 apply_patch 任务不应注入指引,避免污染 token / 模型注意力
    // #262 BUG-004: 持英文 lang 确保 marker 字面一致,且跟 i18n test 串行。
    with_user_language("en", || {
        let out = convert(json!({
            "input": [{"type": "message", "role": "user", "content": "list files"}],
            "instructions": "You are a coding assistant.",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {}}
            }]
        }));
        let messages = out["messages"].as_array().unwrap();
        let has_guidance = messages.iter().any(|m| {
            m["content"]
                .as_str()
                .unwrap_or_default()
                .contains("apply_patch chat-path guidance")
        });
        assert!(
            !has_guidance,
            "无 apply_patch 注册时不应注入 chat-path guidance"
        );
    });
}

#[test]
fn apply_patch_chat_path_guidance_skipped_when_previous_response_id_set() {
    // Devin pre-merge review BUG 修复:带 previous_response_id 的后续 turn,
    // history 已经从 session cache 拼回来(其中含上一轮注入的 guidance),
    // 当前 turn **不应**再注入,否则每 turn 累积一份 ~2KB,N 轮后 N 份
    // 浪费 token + 挤出上下文。
    // #262 BUG-004: 持英文 lang,marker 字面 + 串行。
    with_user_language("en", || {
        let out = convert(json!({
            "input": [{"type": "message", "role": "user", "content": "another edit"}],
            "instructions": "You are a coding assistant.",
            "previous_response_id": "resp_18b_some_prior",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Use the `apply_patch` tool to edit files."
            }]
        }));
        let messages = out["messages"].as_array().unwrap();
        let guidance_count = messages
            .iter()
            .filter(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("apply_patch chat-path guidance")
            })
            .count();
        assert_eq!(
            guidance_count, 0,
            "后续 turn(previous_response_id 非空)不应再注入 guidance(history 已含)"
        );
    });
}

#[test]
fn apply_patch_chat_path_guidance_idempotent_across_turns() {
    // 防止 merge_consecutive_system_messages 把 adapter-injected guidance
    // 跟 Codex instructions 拼到一起后,反复 convert 时被重复累积(连发 3 个
    // turn,每 turn 转换出的 messages 里仍只含 1 段 guidance)。
    // #262 BUG-004: 持英文 lang 确保 marker 字面 + 串行(否则中文 inject 不被 EN
    // marker count 算 → count=0,assertion=1 失败)。
    with_user_language("en", || {
        let one_turn = json!({
            "input": [{"type": "message", "role": "user", "content": "edit"}],
            "instructions": "You are helpful.",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "edit"
            }]
        });
        for _ in 0..3 {
            let out = convert(one_turn.clone());
            let guidance_count = out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["content"].as_str().unwrap_or_default())
                .filter(|c| c.contains("apply_patch chat-path guidance"))
                .count();
            assert_eq!(guidance_count, 1, "每次 convert 仅注入一次 guidance");
        }
    });
}

#[test]
fn custom_tool_call_input_item_lowered_to_assistant_tool_calls() {
    // 回归保护(issue #235):turn N+1 Codex CLI 回放上一轮的
    // `ResponseItem::CustomToolCall { name, input, call_id }`,我们必须把它
    // 转成 chat completions 的 `assistant.tool_calls` 形态(function-call),
    // 否则模型完全看不到上一轮 apply_patch 调用 → 多轮上下文丢失。
    // arguments 必须是 JSON 字符串 `{"input":"<V4A>"}`,与首轮在请求侧
    // lowering 的形态保持一致,模型才不失忆。
    let patch_text = "*** Begin Patch\n*** Update File: a.py\n@@\n-x\n+y\n*** End Patch\n";
    let out = convert(json!({
        "input": [{
            "type": "custom_tool_call",
            "id": "ctc_1",
            "call_id": "call_ap_1",
            "name": "apply_patch",
            "input": patch_text,
            "status": "completed",
        }]
    }));
    let messages = out["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("custom_tool_call 应当映射成 assistant.tool_calls");
    let tc = &assistant["tool_calls"][0];
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["id"], "call_ap_1");
    assert_eq!(tc["function"]["name"], "apply_patch");
    // arguments 是 JSON 字符串值。serde_json 解一次得到 {input: <V4A>},
    // 再 V4A 的换行已被正常 JSON-escape(`\n` 字面值)。
    let args_str = tc["function"]["arguments"].as_str().unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(args_str).expect("arguments 必须是合法 JSON");
    assert_eq!(parsed["input"], patch_text);
}

#[test]
fn custom_tool_call_output_input_item_lowered_to_role_tool() {
    // 回归保护(issue #235):`ResponseItem::CustomToolCallOutput { call_id, output }`
    // 回放时必须转成 chat 端的 `role:"tool"` message,tool_call_id 跟前面的
    // assistant.tool_calls.id 配对,否则 chat 上游会因 orphan tool message 400。
    let out = convert(json!({
        "input": [
            {
                "type": "custom_tool_call",
                "call_id": "call_ap_2",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** Add File: b.md\n+hi\n*** End Patch\n",
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_ap_2",
                "output": "Patch applied successfully",
            }
        ]
    }));
    let messages = out["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("custom_tool_call_output 应当映射成 role:tool");
    assert_eq!(tool_msg["tool_call_id"], "call_ap_2");
    assert_eq!(tool_msg["content"], "Patch applied successfully");
    // 同 PR 还要保证 assistant 在 tool 前(orphan repair 不会插占位)
    let assistant_idx = messages
        .iter()
        .position(|m| m["role"] == "assistant")
        .unwrap();
    let tool_idx = messages.iter().position(|m| m["role"] == "tool").unwrap();
    assert!(
        assistant_idx < tool_idx,
        "assistant.tool_calls 必须在 role:tool 之前出现"
    );
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
fn large_function_call_output_is_bounded_before_chat_history() {
    let huge_line = "function veryLongMinifiedBundle(){return 'x';}".repeat(2_000);
    let raw_output = format!(
        "Chunk ID: 44d863\n\
         Wall time: 0.1540 seconds\n\
         Process exited with code 0\n\
         Original token count: 924828\n\
         Output:\n\
         Total output lines: 18\n\n\
         /tmp/codex-asar/webview/assets/plugins-page-selectors.js:{huge_line}"
    );

    let out = convert(json!({
        "input": [{
            "type": "function_call_output",
            "call_id": "tool_large",
            "output": raw_output
        }]
    }));
    let tool_msg = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("应当有 tool 消息");

    assert_eq!(tool_msg["tool_call_id"], "tool_large");
    let content = tool_msg["content"].as_str().unwrap();
    assert!(
        content.contains("[Tool output stored outside model context]"),
        "大工具结果必须显式标记为外置存储,实际: {content}"
    );
    assert!(
        content.contains("Artifact ID: tool_artifact_"),
        "大工具结果必须带可追踪 artifact id,实际: {content}"
    );
    assert!(
        content.contains("Original token count: 924828"),
        "应保留原始工具输出 token 规模线索"
    );
    assert!(
        content.len() < 20_000,
        "模型可见 tool.content 应有界,实际长度 {}",
        content.len()
    );
}

#[test]
fn large_function_call_output_raw_payload_round_trips_via_artifact_store() {
    let store =
        super::super::artifact_store::ToolArtifactStore::new(8, std::time::Duration::from_secs(60));
    let raw_output = format!(
        "Process exited with code 0\nOriginal token count: 9000\n{}",
        "raw-line\n".repeat(900)
    );
    let content = normalize_tool_output_for_context_with_store(
        Some("call_artifact"),
        Value::String(raw_output.clone()),
        Some(&store),
    );
    let artifact_id = content
        .lines()
        .find_map(|line| line.strip_prefix("Artifact ID: "))
        .expect("summary must expose artifact id");
    let stored = store.get(artifact_id).expect("raw artifact must be stored");

    assert_eq!(stored.call_id.as_deref(), Some("call_artifact"));
    assert_eq!(stored.kind, "command_output");
    assert_eq!(stored.raw_content, raw_output);
    assert!(
        content.len() < raw_output.len(),
        "model-visible summary must be smaller than raw payload"
    );
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

/// issue #180:并行工具调用部分应答 —— assistant.tool_calls = [a, b, c],
/// 只有 tool b 跟上,然后 user 发新消息。DeepSeek / Kimi 严格校验时 400。
/// 修复后:user 之前应该插 a 和 c 的占位 tool 消息。
#[test]
fn parallel_tool_calls_partial_answer_gets_placeholder_for_missing_ids() {
    let cache = empty_tool_cache();
    let mut messages = vec![
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
                {"id": "c", "type": "function", "function": {"name": "search", "arguments": "{}"}},
            ],
        }),
        json!({"role": "tool", "tool_call_id": "b", "content": "result_b"}),
        json!({"role": "user", "content": "继续"}),
    ];
    repair_tool_call_ids(&mut messages, &cache);

    // 期望顺序: assistant → tool(b) → placeholder(a) → placeholder(c) → user
    assert_eq!(
        messages.len(),
        5,
        "应当补齐 a/c 两条占位,实际 {:#?}",
        messages
    );
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "b");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "a");
    assert!(messages[2]["content"]
        .as_str()
        .unwrap()
        .contains("Tool execution skipped/interrupted"));
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "c");
    assert_eq!(messages[4]["role"], "user");
}

/// 末尾 flush:整段 input 末尾 assistant.tool_calls 没有任何 tool 应答
/// (Codex CLI 中断 / 续轮时偶发)。修复后末尾应当补占位 tool。
#[test]
fn trailing_unanswered_tool_calls_get_placeholder_at_end() {
    let cache = empty_tool_cache();
    let mut messages = vec![
        json!({"role": "user", "content": "go"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
            ],
        }),
    ];
    repair_tool_call_ids(&mut messages, &cache);

    // 期望: user → assistant → placeholder(a) → placeholder(b)
    assert_eq!(
        messages.len(),
        4,
        "末尾 pending 必须 flush,实际 {:#?}",
        messages
    );
    assert_eq!(messages[2]["tool_call_id"], "a");
    assert_eq!(messages[3]["tool_call_id"], "b");
}

/// 连续 assistant message 之间没有 tool 应答:老 pending 应在新 assistant
/// 之前 flush,而不是被静默覆盖。
#[test]
fn consecutive_assistants_flush_pending_before_overwrite() {
    let cache = empty_tool_cache();
    let mut messages = vec![
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "old", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
            ],
        }),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "new", "type": "function", "function": {"name": "search", "arguments": "{}"}},
            ],
        }),
    ];
    repair_tool_call_ids(&mut messages, &cache);

    // 期望: assistant1 → placeholder(old) → assistant2 → placeholder(new)
    assert_eq!(messages.len(), 4, "实际 {:#?}", messages);
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["id"], "old");
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "old");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "new");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "new");
}

/// flush 时优先从 ToolCallCache 拿真实 tool name,占位 content 带工具名。
#[test]
fn flush_uses_tool_name_from_cache_when_available() {
    let cache = empty_tool_cache();
    cache.save(
        "call_a",
        super::super::tool_call_cache::ToolCallEntry {
            name: "web_search".to_owned(),
            arguments: "{}".to_owned(),
        },
    );
    let mut messages = vec![
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "web_search", "arguments": "{}"}},
            ],
        }),
        json!({"role": "user", "content": "stop"}),
    ];
    repair_tool_call_ids(&mut messages, &cache);
    let placeholder = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_a")
        .expect("应有占位 tool");
    let content = placeholder["content"].as_str().unwrap();
    assert!(
        content.contains("'web_search'"),
        "cache 命中时占位 content 应带真实 tool name,实际 {content}"
    );
}

/// cache 没命中时,占位 content 应当退化为 'unknown_tool'(还是带文案,
/// 上游能 match id 不报 400)。
#[test]
fn flush_falls_back_to_unknown_tool_when_cache_misses() {
    let cache = empty_tool_cache();
    let mut messages = vec![
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "ghost", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
            ],
        }),
        json!({"role": "user", "content": "stop"}),
    ];
    repair_tool_call_ids(&mut messages, &cache);
    let placeholder = messages
        .iter()
        .find(|m| m["tool_call_id"] == "ghost")
        .expect("应有占位 tool");
    let content = placeholder["content"].as_str().unwrap();
    assert!(
        content.contains("'unknown_tool'"),
        "cache miss 时占位文案应退化为 unknown_tool,实际 {content}"
    );
}

/// 综合回归:同一段 history 里既有正向孤儿(tool 找不到 assistant.tool_calls)
/// 又有反向孤儿(assistant.tool_calls 找不到 tool),两侧都要修。
#[test]
fn forward_and_reverse_orphans_both_get_repaired() {
    // history(模拟 Codex CLI 长会话压缩 + 并行工具调用部分应答):
    //   tool a (孤儿,没有 assistant)
    //   assistant{tool_calls:[b, c]}
    //   tool b
    //   user
    let cache = empty_tool_cache();
    cache.save(
        "a",
        super::super::tool_call_cache::ToolCallEntry {
            name: "shell".to_owned(),
            arguments: "{}".to_owned(),
        },
    );
    let mut messages = vec![
        json!({"role": "tool", "tool_call_id": "a", "content": "result_a"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "b", "type": "function", "function": {"name": "shell", "arguments": "{}"}},
                {"id": "c", "type": "function", "function": {"name": "search", "arguments": "{}"}},
            ],
        }),
        json!({"role": "tool", "tool_call_id": "b", "content": "result_b"}),
        json!({"role": "user", "content": "继续"}),
    ];
    repair_tool_call_ids(&mut messages, &cache);

    // 正向:tool a 前应有占位 assistant
    let first = &messages[0];
    assert_eq!(
        first["role"], "assistant",
        "正向孤儿应插占位 assistant 在前"
    );
    assert_eq!(first["tool_calls"][0]["id"], "a");
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "a");

    // 反向:assistant{b,c} → tool b → 应有占位 tool c → user
    let assistant_bc = messages
        .iter()
        .find(|m| {
            m["role"] == "assistant" && m["tool_calls"].as_array().map(|a| a.len()) == Some(2)
        })
        .expect("应有 [b,c] 的 assistant");
    assert_eq!(assistant_bc["tool_calls"][0]["id"], "b");
    assert_eq!(assistant_bc["tool_calls"][1]["id"], "c");
    let placeholder_c = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "c")
        .expect("应有 c 的占位 tool");
    assert!(placeholder_c["content"]
        .as_str()
        .unwrap()
        .contains("Tool execution skipped/interrupted"));
    assert_eq!(messages.last().unwrap()["role"], "user");
}

/// 端到端:走完整 convert(包含 build_messages_from_input +
/// merge_consecutive_assistant_messages + repair_tool_call_ids),复现
/// issue #180 用户描述的 Responses input → Chat messages 转换路径。
#[test]
fn issue_180_responses_input_parallel_partial_answer_end_to_end() {
    let out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "a", "name": "shell", "arguments": "{}"},
            {"type": "function_call", "call_id": "b", "name": "shell", "arguments": "{}"},
            {"type": "function_call", "call_id": "c", "name": "search", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "b", "output": "result_b"},
            {"type": "message", "role": "user", "content": "继续"}
        ]
    }));
    let msgs = out["messages"].as_array().unwrap();

    // 期望: assistant{a,b,c} → tool b → placeholder a → placeholder c → user
    assert_eq!(msgs.len(), 5, "端到端 messages 长度,实际 {:#?}", msgs);
    assert_eq!(msgs[0]["role"], "assistant");
    let tool_calls = msgs[0]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 3);
    assert_eq!(tool_calls[0]["id"], "a");
    assert_eq!(tool_calls[1]["id"], "b");
    assert_eq!(tool_calls[2]["id"], "c");
    assert_eq!(msgs[1]["tool_call_id"], "b");
    assert_eq!(msgs[1]["content"], "result_b");
    assert_eq!(msgs[2]["tool_call_id"], "a");
    assert!(msgs[2]["content"]
        .as_str()
        .unwrap()
        .contains("skipped/interrupted"));
    assert_eq!(msgs[3]["tool_call_id"], "c");
    assert_eq!(msgs[4]["role"], "user");
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
    // 非 apply_patch 的 custom 工具仍透传 outer description,input 用泛指
    // 兜底描述,不注入 V4A 提示。
    assert_eq!(tool["function"]["description"], "anything");
    assert!(
        tool["function"]["parameters"]["properties"]["input"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("verbatim"),
        "非 apply_patch 应保留泛指 input 描述,实际:{}",
        tool["function"]["parameters"]["properties"]["input"]["description"]
    );
}

#[test]
fn tools_custom_apply_patch_injects_v4a_format_hint() {
    // 回归保护(issue #235):chat 上游(DeepSeek 等)拿到 freeform apply_patch
    // 时,上游的 "do not wrap in JSON" 描述会误导模型;且原始描述里没有 V4A
    // 格式样例。adapter 必须替换描述为 chat 路径准确的 V4A 指引,模型才能
    // 正确填充 `input` 字段。
    let out = convert(json!({
        "input": "hi",
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
        }]
    }));
    let tool = &out["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(tool["function"]["name"], "apply_patch");

    // outer description 必须替换(不能保留误导性的 "do not wrap" 文本)
    let outer = tool["function"]["description"].as_str().unwrap_or_default();
    assert!(!outer.contains("do not wrap"), "误导性原描述未替换:{outer}");
    assert!(
        outer.contains("V4A"),
        "outer description 应当包含 V4A 关键字:{outer}"
    );
    assert!(
        outer.contains("*** Begin Patch"),
        "outer description 应当含 V4A 边界标记:{outer}"
    );

    // input 参数描述必须含 V4A 格式约束(provider 可能更看 parameter desc)
    let input_desc = tool["function"]["parameters"]["properties"]["input"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        input_desc.contains("V4A") && input_desc.contains("*** Begin Patch"),
        "input description 应含 V4A 与边界标记:{input_desc}"
    );

    // 回归保护(issue #235 真机验证暴露的二级问题):tool 描述必须显式解释
    // hunk semantics —— context 锚点 vs space-prefixed 行的区别。DeepSeek 在没
    // 有 lark grammar 强约束的 chat 路径上反复栽在这里(把 anchor 当 space 行
    // 重复一次),花 20 分钟、25+ 次 retry 最后 fallback 到 sed。description
    // 必须含可执行的最小示例 + 显式的"do not repeat the anchor"指引。
    // 关键断言(round 4 真机 capture 修复后):
    // (1) 单端 `@@ <header>` 语法(禁双端 `@@ ... @@` — round 4 根因)
    assert!(
        outer.contains("single-sided") && outer.contains("@@"),
        "tool description 必须显式说明 @@ 单端语法:{outer}"
    );
    let outer_lc = outer.to_lowercase();
    assert!(
        outer_lc.contains("never write a trailing")
            || outer_lc.contains("never add a trailing")
            || outer.contains("no trailing `@@`"),
        "必须显式禁止尾随 @@:{outer}"
    );
    // (2) Add File 全 `+` 前缀(对抗 `def main():` 当 invalid hunk header)
    assert!(
        outer.contains("prefix EVERY line") || outer.contains("prefixed with `+`"),
        "Add File 必须强调每行 `+` 前缀:{outer}"
    );
    // (3) byte-exact matching(对抗 Failed to find context)
    assert!(
        outer.contains("byte-for-byte") || outer.contains("byte-exact"),
        "必须含 byte-exact 匹配规则:{outer}"
    );
    // (4) 完整 V4A example 必须包含
    assert!(
        outer.contains("*** Update File:") && outer.contains("@@ fn main()"),
        "必须包含一个最小可执行 V4A Update example:{outer}"
    );
    assert!(
        outer.contains("*** Add File: hello.py"),
        "必须包含一个 Add File example:{outer}"
    );
    // (5) Delete + Add File fallback 兜底(Update 反复失败时)
    assert!(
        outer.contains("Delete File + Add File"),
        "必须含 Update 反复失败时 fallback 到 Delete+Add 兜底:{outer}"
    );
    // Round 7 实证修复(t0002 漏写 Begin Patch + t0020 Move 空 hunk):
    assert!(
        outer.contains("`*** Begin Patch` as the literal first line")
            || outer.contains("`*** Begin Patch` MUST be the literal first line"),
        "outer description 必须显式要求 Begin Patch 是第一行:{outer}"
    );
    assert!(
        outer.contains("RENAME / MOVE FILE") && outer.contains("at least one hunk"),
        "outer description 必须含 RENAME/MOVE 章节 + Move 配非空 hunk 规则:{outer}"
    );
    assert!(
        outer.contains("pure rename") && outer.contains("Delete File"),
        "outer description 必须给纯重命名的 Delete+Add 替代方案:{outer}"
    );
    // 用户实测反馈(2026-05-22)— 全文 rewrite 倾向 shell 绕过:
    assert!(
        outer.contains("ALWAYS use this tool") && outer.contains("NEVER use shell"),
        "outer description 必须强 normative 禁 shell redirect:{outer}"
    );
    assert!(
        outer.contains("full-file rewrites") && outer.contains("`*** Delete File:"),
        "outer description 必须明示 large rewrite 走 Delete + Add File:{outer}"
    );
    assert!(
        outer.contains("narrow exception is seeding a totally empty file"),
        "outer description 必须 carve-out 空文件 seed 用法:{outer}"
    );

    // 参数描述紧凑版必须含同样核心规则(round 4 修复后)
    assert!(
        input_desc.contains("single-sided") && input_desc.contains("@@"),
        "input description 必须含 @@ 单端语法紧凑版:{input_desc}"
    );
    let input_lc = input_desc.to_lowercase();
    assert!(
        input_lc.contains("never write a trailing") || input_desc.contains("trailing `@@`"),
        "input description 必须含禁尾随 @@ 紧凑版:{input_desc}"
    );
    assert!(
        input_desc.contains("byte-exact") || input_desc.contains("byte-for-byte"),
        "input description 必须含 byte-exact 紧凑版:{input_desc}"
    );
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

// ─── reasoning_effort per-provider 映射矩阵(issue #254) ───────────────────
//
// 覆盖 reasoning_effort_policy 的 3 个 wire 分支 × 5 个 Codex effort 档位。
// 端到端验证从 Codex `reasoning.effort` 到上游 chat body 字段的转换正确性。

fn chat_provider(id: &str) -> Provider {
    let mut p = provider(id, id, "https://example.test");
    p.api_format = "openai_chat".into();
    p
}

fn effort_for(provider_id: &str, effort: &str) -> Value {
    responses_body_to_chat_body_for_provider(
        &json!({
            "input": "hi",
            "reasoning": {"effort": effort},
        }),
        Some(&chat_provider(provider_id)),
    )
    .unwrap()
}

#[test]
fn effort_deepseek_xhigh_maps_to_max() {
    let out = effort_for("deepseek", "xhigh");
    assert_eq!(out["reasoning_effort"], "max");
}

#[test]
fn effort_deepseek_max_maps_to_max() {
    let out = effort_for("deepseek", "max");
    assert_eq!(out["reasoning_effort"], "max");
}

#[test]
fn effort_deepseek_high_stays_high() {
    let out = effort_for("deepseek", "high");
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn effort_deepseek_low_normalizes_to_high() {
    // DeepSeek 官方:上游会把 low/medium 升到 high。本端也 normalize 一次(冗余但
    // 语义明确,真机抓包能直接看到 wire 上是 "high")。
    let out = effort_for("deepseek", "low");
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn effort_deepseek_none_drops() {
    let out = effort_for("deepseek", "none");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_kimi_all_levels_drop() {
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let out = effort_for("kimi", effort);
        assert!(
            out.get("reasoning_effort").is_none(),
            "kimi effort={effort} 不应写 reasoning_effort"
        );
    }
}

#[test]
fn effort_kimi_code_drops() {
    let out = effort_for("kimi-code", "xhigh");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_zhipu_drops() {
    let out = effort_for("zhipu", "high");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_bailian_drops() {
    let out = effort_for("bailian", "xhigh");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_bailian_token_plan_drops() {
    let out = effort_for("bailian-token-plan", "max");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_mimo_payg_drops() {
    let out = effort_for("xiaomi-mimo-payg", "high");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_mimo_token_plan_drops() {
    let out = effort_for("xiaomi-mimo-token-plan", "xhigh");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_minimax_drops() {
    let out = effort_for("minimax", "high");
    assert!(out.get("reasoning_effort").is_none());
}

#[test]
fn effort_custom_provider_xhigh_clamps_to_high() {
    // 自定义 / 未知 provider id → OpenAI 标准 enum fallback
    let out = effort_for("my-custom-proxy", "xhigh");
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn effort_custom_provider_low_passthrough() {
    let out = effort_for("my-custom-proxy", "low");
    assert_eq!(out["reasoning_effort"], "low");
}

#[test]
fn effort_no_provider_uses_openai_enum_fallback() {
    // 不传 provider(测试 / 旁路场景)走 OpenAI 标准 enum
    let out = responses_body_to_chat_body(&json!({
        "input": "hi",
        "reasoning": {"effort": "xhigh"},
    }))
    .unwrap();
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn effort_string_form_also_extracted() {
    // Codex 协议可能也发 "reasoning": "high"(字符串形态,非对象)
    let out = responses_body_to_chat_body_for_provider(
        &json!({
            "input": "hi",
            "reasoning": "xhigh",
        }),
        Some(&chat_provider("deepseek")),
    )
    .unwrap();
    assert_eq!(out["reasoning_effort"], "max");
}

// ── #262: prompt i18n tests ──────────────────────────────────────────

/// 串行化 i18n 测试 — 共享全局 USER_LANGUAGE state 不能并发跑。
///
/// Devin BUG-003 fix:跟 [`crate::core::language::TEST_I18N_LOCK`] 共用同一把
/// 锁,跨模块 serialize 同一全局 `USER_LANGUAGE`。原版每模块独立 mutex 无法
/// serialize cargo test 跨模块的并发,会 race。
use crate::core::language::TEST_I18N_LOCK as LANG_TEST_LOCK;

fn with_user_language<F: FnOnce()>(lang: &str, f: F) {
    let _guard = LANG_TEST_LOCK.lock().unwrap();
    crate::core::language::set_user_language(lang);
    f();
    crate::core::language::set_user_language("en");
}

fn first_turn_request_with_apply_patch() -> Value {
    json!({
        "instructions": "test",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
        "tools": [{"type": "custom", "name": "apply_patch"}],
    })
}

#[test]
fn apply_patch_guidance_injected_in_english_by_default() {
    with_user_language("en", || {
        let out = convert(first_turn_request_with_apply_patch());
        let messages = out["messages"].as_array().unwrap();
        // messages[0] = instructions, messages[1] = apply_patch guidance
        let guidance = messages[1]["content"].as_str().unwrap();
        assert!(
            guidance.contains("[apply_patch chat-path guidance"),
            "EN guidance header missing"
        );
        assert!(
            guidance.contains("ALWAYS use the `apply_patch` tool"),
            "EN guidance body missing"
        );
        assert!(
            !guidance.contains("务必使用"),
            "EN guidance must not contain ZH text"
        );
    });
}

#[test]
fn apply_patch_guidance_injected_in_chinese_when_language_zh() {
    with_user_language("zh-CN", || {
        let out = convert(first_turn_request_with_apply_patch());
        let guidance = out["messages"][1]["content"].as_str().unwrap();
        assert!(
            guidance.contains("[apply_patch chat-path 指引"),
            "ZH guidance header missing"
        );
        assert!(
            guidance.contains("务必使用 `apply_patch` tool"),
            "ZH guidance body missing"
        );
        // V4A 关键字必须保英文(Codex CLI parser 不接受翻译)
        for keyword in &[
            "*** Begin Patch",
            "*** Update File:",
            "*** Add File:",
            "*** Delete File:",
            "*** Move to:",
            "*** End of File",
            "@@ <header>",
            "apply_patch",
        ] {
            assert!(
                guidance.contains(keyword),
                "ZH guidance must keep V4A keyword `{keyword}` in English"
            );
        }
        // 错误消息必须保英文(Codex CLI 抛的就是英文 + user grep 用)
        for msg in &[
            "Failed to find context",
            "is not a valid hunk header",
            "invalid patch: The first line of the patch must be '*** Begin Patch'",
            "Update file hunk for path",
        ] {
            assert!(
                guidance.contains(msg),
                "ZH guidance must keep error message `{msg}` in English"
            );
        }
    });
}

#[test]
fn apply_patch_guidance_zh_covers_all_nine_rules() {
    with_user_language("zh", || {
        let out = convert(first_turn_request_with_apply_patch());
        let guidance = out["messages"][1]["content"].as_str().unwrap();
        // 9 条规则编号都必须出现(防漏译)
        for n in 1..=9 {
            let marker = format!("{n}.");
            assert!(
                guidance.contains(&marker),
                "ZH guidance must cover rule {n} (marker `{marker}` missing)"
            );
        }
        // 引言段强调词翻译完整
        assert!(
            guidance.contains("务必使用"),
            "missing **ALWAYS** equivalent"
        );
        assert!(guidance.contains("绝不"), "missing **NEVER** equivalent");
    });
}
