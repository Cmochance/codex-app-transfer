use std::path::PathBuf;

use bytes::Bytes;
use codex_app_transfer_adapters::anthropic_messages::request::{
    anthropic_messages_default_headers, build_anthropic_messages_upstream_path,
    prepare_anthropic_messages_request, responses_body_to_anthropic_messages_request,
};
use codex_app_transfer_registry::Provider;
use indexmap::IndexMap;
use serde_json::Value;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("anthropic_messages")
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_root().join(name)).expect("fixture should be readable")
}

fn read_json_fixture(name: &str) -> Value {
    serde_json::from_str(&read_fixture(name)).expect("fixture should be valid json")
}

fn dummy_provider() -> Provider {
    Provider {
        id: "anthropic-local".into(),
        name: "Anthropic Local".into(),
        base_url: "https://api.anthropic.com/v1".into(),
        auth_scheme: "bearer".into(),
        api_format: "anthropic_messages".into(),
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

fn anyrouter_provider() -> Provider {
    let mut provider = dummy_provider();
    provider.id = "anyrouter".into();
    provider.name = "Anyrouter".into();
    provider.base_url = "https://anyrouter.top".into();
    provider
        .request_options
        .insert("web_search_enabled".into(), serde_json::json!(true));
    provider
}

#[derive(Debug)]
struct SseFrame {
    event: String,
    data: Value,
}

fn parse_sse_fixture(name: &str) -> Vec<SseFrame> {
    read_fixture(name)
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let mut event = None;
            let mut data = None;
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = Some(value.to_owned());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = Some(value.to_owned());
                }
            }
            SseFrame {
                event: event.expect("SSE frame should include event"),
                data: serde_json::from_str(&data.expect("SSE frame should include data"))
                    .expect("SSE data should be valid json"),
            }
        })
        .collect()
}

#[test]
fn p2_anthropic_messages_sse_fixtures_are_valid() {
    let cases = [
        (
            "text_stream.sse",
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
        ),
        (
            "thinking_stream.sse",
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
        ),
        (
            "tool_use_stream.sse",
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
        ),
        ("error_stream.sse", vec!["error"]),
        (
            "unknown_event_stream.sse",
            vec![
                "message_start",
                "anthropic_future_event",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
        ),
    ];

    for (fixture, expected_events) in cases {
        let frames = parse_sse_fixture(fixture);
        let events: Vec<_> = frames.iter().map(|frame| frame.event.as_str()).collect();
        assert_eq!(
            events, expected_events,
            "unexpected event order in {fixture}"
        );
        for frame in frames {
            assert_eq!(
                frame.data["type"].as_str(),
                Some(frame.event.as_str()),
                "fixture {fixture} should keep event name and data.type aligned"
            );
        }
    }
}

#[test]
fn p2_request_mapper_json_fixtures_are_valid() {
    let cases = [
        ("request_text.responses.json", "request_text.anthropic.json"),
        (
            "request_tool_result.responses.json",
            "request_tool_result.anthropic.json",
        ),
    ];

    for (input_name, expected_name) in cases {
        let input = read_json_fixture(input_name);
        let expected = read_json_fixture(expected_name);
        assert!(
            input.get("input").is_some(),
            "{input_name} should model Responses input"
        );
        assert!(
            expected.get("messages").is_some(),
            "{expected_name} should model Anthropic Messages output"
        );
        assert!(
            expected.get("max_tokens").is_some(),
            "{expected_name} should include Anthropic required max_tokens"
        );
    }
}

#[test]
fn responses_text_request_lowers_to_anthropic_messages() {
    let input = read_json_fixture("request_text.responses.json");
    let expected = read_json_fixture("request_text.anthropic.json");

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual, expected);
}

#[test]
fn responses_tool_result_request_lowers_to_anthropic_messages() {
    let input = read_json_fixture("request_tool_result.responses.json");
    let expected = read_json_fixture("request_tool_result.anthropic.json");

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual, expected);
}

#[test]
fn request_mapper_sanitizes_tool_names_and_rewrites_tool_choice() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "fs.read file",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }
        ],
        "tools": [
            {
                "type": "function",
                "name": "fs.read file",
                "description": "Read",
                "parameters": {"type":"object"}
            }
        ],
        "tool_choice": {"type":"function", "function": {"name": "fs.read file"}},
        "parallel_tool_calls": false,
        "max_output_tokens": 128,
        "stream": true
    });

    let conversion = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed");

    assert_eq!(
        conversion.request["tools"][0]["name"],
        Value::String("fs_read_file".into())
    );
    assert_eq!(
        conversion.request["messages"][0]["content"][0]["name"],
        Value::String("fs_read_file".into())
    );
    assert_eq!(
        conversion.request["tool_choice"],
        serde_json::json!({
            "type": "tool",
            "name": "fs_read_file",
            "disable_parallel_tool_use": true
        })
    );
    assert_eq!(
        conversion
            .tool_name_maps
            .reverse
            .get("fs_read_file")
            .map(String::as_str),
        Some("fs.read file")
    );
}

#[test]
fn request_mapper_preserves_native_tool_choice_disable_parallel() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": "hi",
        "tool_choice": {
            "type": "auto",
            "disable_parallel_tool_use": true
        },
        "max_output_tokens": 128
    });

    let conversion = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed");

    assert_eq!(
        conversion.request["tool_choice"],
        serde_json::json!({
            "type": "auto",
            "disable_parallel_tool_use": true
        })
    );
}

#[test]
fn responses_web_search_lowers_to_anthropic_hosted_tool() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "search the web",
        "tools": [
            {
                "type": "web_search",
                "search_context_size": "medium",
                "user_location": {
                    "approximate": {
                        "city": "Shanghai",
                        "country": "CN",
                        "timezone": "Asia/Shanghai"
                    }
                },
                "allowed_domains": ["example.com"]
            },
            {
                "type": "function",
                "name": "read_file",
                "parameters": {"type":"object"}
            }
        ],
        "stream": true
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &anyrouter_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual["tools"][0]["type"], "web_search_20250305");
    assert_eq!(actual["tools"][0]["name"], "web_search");
    assert_eq!(actual["tools"][0]["max_uses"], 5);
    assert_eq!(actual["tools"][0]["user_location"]["city"], "Shanghai");
    assert_eq!(actual["tools"][0]["allowed_domains"][0], "example.com");
    assert_eq!(actual["tools"][1]["name"], "read_file");
}

#[test]
fn namespace_and_custom_tool_metadata_survives_anthropic_lowering() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "use tools if needed",
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__notion__",
                "description": "Read and write Notion pages.",
                "tools": [{
                    "type": "function",
                    "name": "notion_search",
                    "description": "Search pages.",
                    "parameters": {"properties": {"query": {"type": "string"}}},
                    "strict": true,
                    "cache_control": {"type": "ephemeral"}
                }]
            },
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch.",
                "format": {
                    "type": "grammar",
                    "syntax": "lark",
                    "definition": "start: /.+/"
                },
                "input_examples": [{"input": "*** Begin Patch\n*** End Patch"}]
            }
        ]
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;
    let tools = actual["tools"].as_array().expect("tools array");

    let notion = tools
        .iter()
        .find(|tool| tool["name"] == "notion_search")
        .expect("namespace inner tool should survive");
    assert!(notion["description"]
        .as_str()
        .unwrap()
        .contains("mcp__notion__"));
    assert!(notion["description"]
        .as_str()
        .unwrap()
        .contains("Read and write Notion pages."));
    assert_eq!(notion["input_schema"]["type"], "object");
    assert_eq!(notion["input_schema"]["strict"], true);
    assert_eq!(notion["cache_control"]["type"], "ephemeral");

    let custom = tools
        .iter()
        .find(|tool| tool["name"] == "apply_patch")
        .expect("custom tool should survive");
    assert!(custom["description"].as_str().unwrap().contains("grammar"));
    assert!(custom["input_schema"]["properties"]["input"]["description"]
        .as_str()
        .unwrap()
        .contains("start: /.+/"));
    assert_eq!(
        custom["input_examples"][0]["input"],
        "*** Begin Patch\n*** End Patch"
    );
}

#[test]
fn anthropic_native_tools_and_mcp_server_tools_are_preserved() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "use hosted tools",
        "tools": [
            {
                "type": "computer_use_preview",
                "display_width_px": 1024,
                "display_height_px": 768,
                "display_number": 1
            },
            {
                "type": "code_execution_20250825",
                "name": "code_execution",
                "allowed_callers": ["toolu_1"]
            },
            {
                "type": "mcp",
                "server_label": "docs",
                "server_url": "https://mcp.example.com",
                "allowed_tools": ["search"],
                "headers": {"Authorization": "Bearer redacted-token"}
            }
        ]
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual["tools"][0]["type"], "computer_20250124");
    assert_eq!(actual["tools"][0]["name"], "computer");
    assert_eq!(actual["tools"][0]["display_width_px"], 1024);
    assert_eq!(actual["tools"][0]["display_height_px"], 768);
    assert_eq!(actual["tools"][0]["display_number"], 1);
    assert_eq!(actual["tools"][1]["type"], "code_execution_20250825");
    assert_eq!(actual["tools"][1]["allowed_callers"][0], "toolu_1");
    assert_eq!(actual["mcp_servers"][0]["type"], "url");
    assert_eq!(actual["mcp_servers"][0]["name"], "docs");
    assert_eq!(actual["mcp_servers"][0]["url"], "https://mcp.example.com");
    assert_eq!(
        actual["mcp_servers"][0]["tool_configuration"]["allowed_tools"][0],
        "search"
    );
    assert_eq!(
        actual["mcp_servers"][0]["authorization_token"],
        "redacted-token"
    );
}

#[test]
fn litellm_anthropic_top_level_fields_and_structured_output_survive() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "return structured output",
        "text": {
            "format": {
                "type": "json_schema",
                "name": "answer",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "answer": {
                            "type": "string",
                            "minLength": 3
                        }
                    },
                    "required": ["answer"]
                }
            }
        },
        "context_management": [{
            "type": "compaction",
            "compact_threshold": 150000
        }],
        "container": {"skills": ["python"]},
        "output_config": {"effort": "high"},
        "speed": "fast",
        "cache_control": {"type": "ephemeral"}
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(
        actual["context_management"]["edits"][0]["type"],
        "compact_20260112"
    );
    assert_eq!(
        actual["context_management"]["edits"][0]["trigger"]["value"],
        150000
    );
    assert_eq!(actual["container"]["skills"][0], "python");
    assert_eq!(actual["output_config"]["effort"], "high");
    assert_eq!(actual["speed"], "fast");
    assert_eq!(actual["cache_control"]["type"], "ephemeral");
    assert_eq!(actual["output_format"]["type"], "json_schema");
    assert_eq!(actual["output_format"]["schema"]["type"], "object");
    assert_eq!(
        actual["output_format"]["schema"]["additionalProperties"],
        false
    );
    assert!(
        actual["output_format"]["schema"]["properties"]["answer"]
            .get("minLength")
            .is_none(),
        "Anthropic output_format schema should drop unsupported minLength"
    );
    assert!(
        actual["output_format"]["schema"]["properties"]["answer"]["description"]
            .as_str()
            .unwrap()
            .contains("minimum length")
    );
}

#[test]
fn litellm_native_messages_fields_survive_direct_lowering() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "instructions": [{
            "type": "text",
            "text": "Keep cacheable system context.",
            "cache_control": {"type": "ephemeral"}
        }],
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_code",
                "name": "code_execution",
                "input": {"command": "python main.py"},
                "caller": {"type": "code_execution_20250825", "tool_id": "toolu_code"},
                "cache_control": {"type": "ephemeral"}
            }]
        }],
        "stop_sequences": ["END"],
        "inference_geo": "us",
        "mcp_servers": [{
            "type": "url",
            "name": "existing",
            "url": "https://mcp.existing.example"
        }],
        "tools": [{
            "type": "mcp",
            "server_label": "docs",
            "server_url": "https://mcp.example.com"
        }]
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(
        actual["system"][0]["text"],
        "Keep cacheable system context."
    );
    assert_eq!(actual["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(actual["stop_sequences"][0], "END");
    assert_eq!(actual["inference_geo"], "us");
    assert_eq!(actual["mcp_servers"][0]["name"], "existing");
    assert_eq!(actual["mcp_servers"][1]["name"], "docs");
    let tool_use = &actual["messages"][0]["content"][0];
    assert_eq!(tool_use["type"], "tool_use");
    assert_eq!(tool_use["caller"]["type"], "code_execution_20250825");
    assert_eq!(tool_use["cache_control"]["type"], "ephemeral");
}

#[test]
fn adaptive_claude_models_use_output_config_effort() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "think carefully",
        "reasoning": {"effort": "xhigh"},
        "max_output_tokens": 32768
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual["thinking"]["type"], "adaptive");
    assert_eq!(actual["output_config"]["effort"], "xhigh");
}

#[test]
fn container_upload_and_rich_tool_result_blocks_survive() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "container_upload",
                    "file_id": "file_abc",
                    "filename": "data.csv"
                }]
            },
            {
                "type": "function_call",
                "call_id": "toolu_1",
                "name": "inspect",
                "arguments": "{}"
            },
            {
                "type": "message",
                "role": "tool",
                "tool_call_id": "toolu_1",
                "is_error": true,
                "cache_control": {"type": "ephemeral"},
                "content": [
                    {"type": "text", "text": "failed", "cache_control": {"type": "ephemeral"}},
                    {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "details"}}
                ]
            }
        ],
        "tools": [{
            "type": "function",
            "name": "inspect",
            "parameters": {"type": "object"}
        }]
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(
        actual["messages"][0]["content"][0]["type"],
        "container_upload"
    );
    assert!(actual["tools"].as_array().unwrap().iter().any(|tool| {
        tool["type"] == "code_execution_20250522" && tool["name"] == "code_execution"
    }));
    let tool_result = &actual["messages"][2]["content"][0];
    assert_eq!(tool_result["type"], "tool_result");
    assert_eq!(tool_result["is_error"], true);
    assert_eq!(tool_result["cache_control"]["type"], "ephemeral");
    assert_eq!(tool_result["content"][0]["type"], "text");
    assert_eq!(
        tool_result["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(tool_result["content"][1]["type"], "document");
}

#[test]
fn native_messages_blocks_preserve_cache_and_server_tool_history() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "instructions": [
            {"type": "text", "text": "x-anthropic-billing-header: internal"},
            {"type": "text", "text": "Keep this system block.", "cache_control": {"type": "ephemeral"}}
        ],
        "input": [
            {
                "type": "message",
                "role": "user",
                "name": "alice",
                "content": [
                    {"type": "input_text", "text": "cache this", "cache_control": {"type": "ephemeral"}},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "cache_control": {"type": "ephemeral"}},
                    {
                        "type": "input_file",
                        "file_id": "file_abc",
                        "title": "Spec",
                        "context": "reviewed context",
                        "citations": {"enabled": true},
                        "cache_control": {"type": "ephemeral"}
                    }
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "assistant cache", "cache_control": {"type": "ephemeral"}},
                    {"type": "thinking", "thinking": "signed", "signature": "sig_abc", "cache_control": {"type": "ephemeral"}},
                    {"type": "server_tool_use", "id": "srv_1", "name": "tool_search_tool", "input": {"query": "abc"}},
                    {"type": "tool_search_tool_result", "tool_use_id": "srv_1", "content": [{"type": "text", "text": "result"}]}
                ]
            }
        ]
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual["system"].as_array().unwrap().len(), 1);
    assert_eq!(actual["system"][0]["text"], "Keep this system block.");
    assert_eq!(actual["system"][0]["cache_control"]["type"], "ephemeral");

    let user = &actual["messages"][0];
    assert_eq!(user["name"], "alice");
    assert_eq!(user["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(user["content"][1]["type"], "image");
    assert_eq!(user["content"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(user["content"][2]["type"], "document");
    assert_eq!(user["content"][2]["title"], "Spec");
    assert_eq!(user["content"][2]["context"], "reviewed context");
    assert_eq!(user["content"][2]["citations"]["enabled"], true);
    assert_eq!(user["content"][2]["cache_control"]["type"], "ephemeral");

    let assistant_content = actual["messages"][1]["content"].as_array().unwrap();
    assert_eq!(assistant_content[0]["type"], "text");
    assert_eq!(assistant_content[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(assistant_content[1]["type"], "thinking");
    assert_eq!(assistant_content[1]["signature"], "sig_abc");
    assert_eq!(assistant_content[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(assistant_content[2]["type"], "server_tool_use");
    assert_eq!(assistant_content[2]["name"], "tool_search_tool");
    assert_eq!(assistant_content[3]["type"], "tool_search_tool_result");
}

#[test]
fn advisor_history_blocks_follow_litellm_native_messages_rule() {
    let input_without_advisor_tool = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "server_tool_use", "id": "advisor_1", "name": "advisor", "input": {"question": "check"}},
                {"type": "advisor_tool_result", "tool_use_id": "advisor_1", "content": [{"type": "text", "text": "feedback"}]},
                {"type": "output_text", "text": "done"}
            ]
        }]
    });
    let without_advisor = responses_body_to_anthropic_messages_request(
        &input_without_advisor_tool,
        &dummy_provider(),
    )
    .expect("request conversion should succeed")
    .request;
    let content = without_advisor["messages"][0]["content"]
        .as_array()
        .unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");

    let input_with_advisor_tool = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "server_tool_use", "id": "advisor_1", "name": "advisor", "input": {"question": "check"}},
                {"type": "advisor_tool_result", "tool_use_id": "advisor_1", "content": [{"type": "text", "text": "feedback"}]}
            ]
        }],
        "tools": [{"type": "advisor_20260301", "model": "claude-opus-4-7"}]
    });
    let with_advisor =
        responses_body_to_anthropic_messages_request(&input_with_advisor_tool, &dummy_provider())
            .expect("request conversion should succeed")
            .request;
    let content = with_advisor["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "server_tool_use");
    assert_eq!(content[1]["type"], "advisor_tool_result");
}

#[test]
fn prepared_request_headers_follow_litellm_anthropic_beta_detection() {
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "use native tools",
        "text": {
            "format": {
                "type": "json_schema",
                "schema": {"type": "object"}
            }
        },
        "context_management": [{"type": "compaction", "compact_threshold": 150000}],
        "container": {"skills": ["python"]},
        "speed": "fast",
        "tools": [
            {"type": "computer_use_preview", "display_width_px": 1024, "display_height_px": 768},
            {"type": "mcp", "server_label": "docs", "server_url": "https://mcp.example.com"},
            {"type": "code_execution_20250825", "name": "code_execution"},
            {"type": "web_fetch_20250910", "name": "web_fetch"},
            {"type": "memory_20250818", "name": "memory"},
            {"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool"},
            {"type": "advisor_20260301", "model": "claude-opus-4-7"}
        ]
    });
    let prepared = prepare_anthropic_messages_request(
        "/v1/responses",
        Bytes::from(body.to_string()),
        &dummy_provider(),
    )
    .expect("request preparation should succeed");
    let beta = prepared
        .headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .expect("anthropic-beta header");
    let values: std::collections::BTreeSet<_> = beta.split(',').map(|v| v.trim()).collect();

    for expected in [
        "computer-use-2025-01-24",
        "mcp-client-2025-04-04",
        "code-execution-2025-08-25",
        "web-fetch-2025-09-10",
        "context-management-2025-06-27",
        "compact-2026-01-12",
        "structured-outputs-2025-11-13",
        "fast-mode-2026-02-01",
        "skills-2025-10-02",
        "advanced-tool-use-2025-11-20",
        "advisor-tool-2026-03-01",
    ] {
        assert!(
            values.contains(expected),
            "missing beta header {expected}: {beta}"
        );
    }
}

#[test]
fn prepared_request_headers_include_file_id_and_effort_betas() {
    let body = serde_json::json!({
        "model": "claude-opus-4-5",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "document",
                "source": {"type": "file", "file_id": "file_abc123"}
            }]
        }],
        "output_config": {"effort": "high"}
    });
    let prepared = prepare_anthropic_messages_request(
        "/v1/responses",
        Bytes::from(body.to_string()),
        &dummy_provider(),
    )
    .expect("request preparation should succeed");
    let beta = prepared
        .headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .expect("anthropic-beta header");
    let values: std::collections::BTreeSet<_> = beta.split(',').map(|v| v.trim()).collect();

    for expected in [
        "files-api-2025-04-14",
        "code-execution-2025-05-22",
        "effort-2025-11-24",
    ] {
        assert!(
            values.contains(expected),
            "missing beta header {expected}: {beta}"
        );
    }
}

#[test]
fn assistant_thinking_blocks_preserve_anthropic_signature() {
    let input = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": [{
            "type": "message",
            "role": "assistant",
            "content": "",
            "anthropic_thinking_blocks": [{
                "type": "thinking",
                "thinking": "I should keep the signed thinking block.",
                "signature": "sig_abc"
            }]
        }],
        "stream": true
    });

    let actual = responses_body_to_anthropic_messages_request(&input, &anyrouter_provider())
        .expect("request conversion should succeed")
        .request;

    assert_eq!(actual["messages"][0]["content"][0]["type"], "thinking");
    assert_eq!(
        actual["messages"][0]["content"][0]["thinking"],
        "I should keep the signed thinking block."
    );
    assert_eq!(actual["messages"][0]["content"][0]["signature"], "sig_abc");
}

#[test]
fn valid_underscore_tool_name_is_not_rewritten() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": "hi",
        "tools": [
            {
                "type": "function",
                "name": "_valid-tool",
                "parameters": {"type":"object"}
            }
        ],
        "max_output_tokens": 128
    });

    let conversion = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed");

    assert_eq!(
        conversion.request["tools"][0]["name"],
        Value::String("_valid-tool".into())
    );
    assert!(conversion.tool_name_maps.forward.is_empty());
    assert!(conversion.tool_name_maps.reverse.is_empty());
}

#[test]
fn request_mapper_filters_email_user_metadata_and_maps_reasoning() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": "hi",
        "user": "person@example.com",
        "reasoning": {"effort":"high"}
    });

    let request = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect("request conversion should succeed")
        .request;

    assert!(request.get("metadata").is_none());
    assert_eq!(
        request["thinking"],
        serde_json::json!({"type":"enabled","budget_tokens":8192})
    );
    assert_eq!(request["max_tokens"], Value::Number(4096.into()));
}

#[test]
fn prepare_request_exposes_path_body_and_anthropic_headers() {
    let input = read_json_fixture("request_text.responses.json");
    let body = Bytes::from(serde_json::to_vec(&input).unwrap());
    let prepared = prepare_anthropic_messages_request("/v1/responses", body, &dummy_provider())
        .expect("prepare should succeed");

    assert_eq!(prepared.upstream_path, "/messages");
    assert_eq!(
        prepared
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&prepared.body).unwrap(),
        read_json_fixture("request_text.anthropic.json")
    );
    assert!(prepared.response_session.is_some());
    assert!(!prepared.is_compact);
    assert!(prepared.original_responses_request.is_some());
}

#[test]
fn compact_prepare_uses_non_streaming_messages_request() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": "summarize this conversation"
    });
    let body = Bytes::from(serde_json::to_vec(&input).unwrap());
    let prepared =
        prepare_anthropic_messages_request("/responses/compact", body, &dummy_provider())
            .expect("compact prepare should succeed");
    let request: Value = serde_json::from_slice(&prepared.body).unwrap();

    assert_eq!(prepared.upstream_path, "/messages");
    assert!(prepared.is_compact);
    assert!(prepared.response_session.is_none());
    assert!(prepared.original_responses_request.is_none());
    assert_eq!(request["stream"], Value::Bool(false));
    assert_eq!(request["max_tokens"], Value::Number(20_000.into()));
    assert!(request["messages"]
        .as_array()
        .is_some_and(|m| !m.is_empty()));
}

#[test]
fn orphan_tool_result_returns_diagnostic_bad_request() {
    let input = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "input": [
            {
                "type": "function_call_output",
                "call_id": "missing_call",
                "output": "orphan"
            }
        ],
        "max_output_tokens": 128
    });

    let err = responses_body_to_anthropic_messages_request(&input, &dummy_provider())
        .expect_err("orphan tool output should not be silently converted");

    assert!(
        err.to_string().contains("tool_call missing function.name")
            || err
                .to_string()
                .contains("tool result references unknown tool_use")
    );
}

#[test]
fn base_url_path_helper_handles_v1_prefixes() {
    assert_eq!(
        build_anthropic_messages_upstream_path("https://api.anthropic.com"),
        "/v1/messages"
    );
    assert_eq!(
        build_anthropic_messages_upstream_path("https://api.anthropic.com/v1"),
        "/messages"
    );
    assert_eq!(
        build_anthropic_messages_upstream_path("https://proxy.example/anthropic/v1/"),
        "/messages"
    );
}

#[test]
fn default_headers_include_anthropic_contract_values() {
    let headers = anthropic_messages_default_headers();
    assert_eq!(
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers
            .get(http::header::ACCEPT)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}
