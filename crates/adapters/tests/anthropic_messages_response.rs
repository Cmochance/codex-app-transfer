use std::path::PathBuf;
use std::pin::Pin;

use bytes::Bytes;
use codex_app_transfer_adapters::anthropic_messages::request::AnthropicToolNameMaps;
use codex_app_transfer_adapters::anthropic_messages::response::{
    build_anthropic_compact_response_plan, convert_anthropic_messages_to_responses_stream,
};
use codex_app_transfer_adapters::responses::{
    global_response_session_cache, global_tool_call_cache,
};
use codex_app_transfer_adapters::types::{ByteStream, ResponseSessionPlan};
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("anthropic_messages")
}

fn read_fixture(name: &str) -> Bytes {
    Bytes::from(std::fs::read(fixture_root().join(name)).expect("fixture should be readable"))
}

fn input_stream(bytes: Bytes) -> ByteStream {
    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(stream::iter(vec![Ok(bytes)]));
    s
}

fn input_stream_chunked(bytes: Bytes, chunk_size: usize) -> ByteStream {
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + chunk_size).min(bytes.len());
        chunks.push(Ok(bytes.slice(i..end)));
        i = end;
    }
    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(stream::iter(chunks));
    s
}

async fn collect_events(mut s: ByteStream) -> Vec<(String, Value)> {
    let mut buf = Vec::new();
    while let Some(item) = s.next().await {
        let chunk = item.expect("stream item");
        buf.extend_from_slice(&chunk);
    }
    let s = String::from_utf8(buf).expect("utf8");
    let mut out = Vec::new();
    for frame in s.split("\n\n") {
        if frame.trim().is_empty() {
            continue;
        }
        let mut event = String::new();
        let mut data = String::new();
        for line in frame.split('\n') {
            if let Some(v) = line.strip_prefix("event: ") {
                event = v.to_owned();
            } else if let Some(v) = line.strip_prefix("data: ") {
                data = v.to_owned();
            }
        }
        out.push((event, serde_json::from_str(&data).expect("json")));
    }
    out
}

fn convert_fixture(name: &str) -> ByteStream {
    convert_anthropic_messages_to_responses_stream(
        input_stream(read_fixture(name)),
        None,
        None,
        AnthropicToolNameMaps::default(),
    )
}

#[tokio::test]
async fn text_stream_maps_to_responses_lifecycle() {
    let events = collect_events(convert_fixture("text_stream.sse")).await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|(name, value)| {
            (name == "response.output_text.delta").then(|| value["delta"].as_str().unwrap())
        })
        .collect();
    assert_eq!(deltas, vec!["Hel", "lo"]);

    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["model"], "claude-3-5-sonnet-20241022");
    assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(completed["usage"]["input_tokens"], 12);
    assert_eq!(completed["usage"]["output_tokens"], 2);
    assert_eq!(completed["usage"]["total_tokens"], 14);
}

#[tokio::test]
async fn thinking_stream_maps_to_reasoning_summary() {
    let events = collect_events(convert_fixture("thinking_stream.sse")).await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"response.reasoning_summary_part.added"));
    assert!(names.contains(&"response.reasoning_summary_text.delta"));
    assert!(names.contains(&"response.reasoning_summary_text.done"));

    let done = events
        .iter()
        .find(|(name, _)| name == "response.reasoning_summary_text.done")
        .unwrap();
    assert_eq!(done.1["text"], "I need to inspect the request shape.");

    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["output"][0]["type"], "reasoning");
    assert_eq!(
        completed["output"][0]["summary"][0]["text"],
        "I need to inspect the request shape."
    );
}

#[tokio::test]
async fn tool_use_stream_maps_function_call_and_saves_cache() {
    let events = collect_events(convert_fixture("tool_use_stream.sse")).await;
    let completed = &events.last().unwrap().1["response"];
    let item = &completed["output"][0];
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["call_id"], "toolu_01");
    assert_eq!(item["name"], "read_file");
    assert_eq!(item["arguments"], "{\"path\":\"Cargo.toml\"}");

    let cached = global_tool_call_cache()
        .get("toolu_01")
        .expect("tool call should be cached for next-turn tool_result repair");
    assert_eq!(cached.name, "read_file");
    assert_eq!(cached.arguments, "{\"path\":\"Cargo.toml\"}");
}

#[tokio::test]
async fn server_web_search_maps_to_responses_call_and_annotations() {
    global_response_session_cache().clear();
    let session = ResponseSessionPlan {
        response_id: "resp_web_search_native_blocks".to_owned(),
        messages: vec![json!({"role": "user", "content": "search"})],
    };
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_web","name":"web_search","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"codex app transfer\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_web","content":[{"type":"web_search_result","url":"https://example.com/a","title":"Example A","cited_text":"Snippet A"}]}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Found it."}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8,"server_tool_use":{"web_search_requests":1}}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        Some(session),
        Some(json!({"model": "claude-opus-4-7"})),
        AnthropicToolNameMaps::default(),
    ))
    .await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"response.web_search_call.in_progress"));
    assert!(names.contains(&"response.web_search_call.completed"));
    assert!(names.contains(&"response.output_text.annotation.added"));

    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["output"][0]["type"], "web_search_call");
    assert_eq!(completed["output"][0]["status"], "completed");
    assert_eq!(
        completed["output"][0]["action"]["query"],
        "codex app transfer"
    );
    let annotations = completed["output"][1]["content"][0]["annotations"]
        .as_array()
        .expect("annotations array");
    assert_eq!(annotations[0]["type"], "url_citation");
    assert_eq!(annotations[0]["url"], "https://example.com/a");
    assert_eq!(annotations[0]["snippet"], "Snippet A");
    assert_eq!(
        completed["usage"]["server_tool_use"]["web_search_requests"],
        1
    );

    let saved = global_response_session_cache()
        .get("resp_web_search_native_blocks")
        .expect("response session should be saved");
    let saved_content = saved[1]["content"].as_array().unwrap();
    assert_eq!(saved_content[0]["type"], "server_tool_use");
    assert_eq!(saved_content[0]["name"], "web_search");
    assert_eq!(saved_content[0]["input"]["query"], "codex app transfer");
    assert_eq!(saved_content[1]["type"], "web_search_tool_result");
    assert_eq!(saved_content[2]["type"], "text");
    assert_eq!(saved_content[2]["text"], "Found it.");
}

#[tokio::test]
async fn code_execution_tool_result_maps_to_code_interpreter_call() {
    global_response_session_cache().clear();
    let session = ResponseSessionPlan {
        response_id: "resp_code_execution_native_blocks".to_owned(),
        messages: vec![json!({"role": "user", "content": "run code"})],
    };
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-4-7","container":{"id":"container_abc"},"usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"toolu_code","name":"code_execution","input":{"command":"python main.py"}}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"bash_code_execution_tool_result","tool_use_id":"toolu_code","content":[{"type":"text","text":"stdout line"}]}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        Some(session),
        Some(json!({"model": "claude-opus-4-7"})),
        AnthropicToolNameMaps::default(),
    ))
    .await;
    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["output"][0]["type"], "function_call");
    assert_eq!(completed["output"][0]["call_id"], "toolu_code");
    assert_eq!(completed["output"][1]["type"], "code_interpreter_call");
    assert_eq!(completed["output"][1]["call_id"], "toolu_code");
    assert_eq!(completed["output"][1]["container_id"], "container_abc");
    assert_eq!(completed["output"][1]["code"], "python main.py");
    assert_eq!(completed["output"][1]["outputs"][0]["type"], "logs");
    assert_eq!(completed["output"][1]["outputs"][0]["logs"], "stdout line");
    assert_eq!(
        completed["metadata"]["anthropic_container"]["id"],
        "container_abc"
    );

    let saved = global_response_session_cache()
        .get("resp_code_execution_native_blocks")
        .expect("response session should be saved");
    let saved_content = saved[1]["content"].as_array().unwrap();
    assert_eq!(saved_content[0]["type"], "server_tool_use");
    assert_eq!(saved_content[0]["name"], "code_execution");
    assert_eq!(saved_content[0]["input"]["command"], "python main.py");
    assert_eq!(saved_content[1]["type"], "bash_code_execution_tool_result");
    assert_eq!(saved_content[1]["tool_use_id"], "toolu_code");
}

#[tokio::test]
async fn non_web_server_tool_result_is_preserved_for_previous_response() {
    global_response_session_cache().clear();
    let session = ResponseSessionPlan {
        response_id: "resp_tool_search_native_blocks".to_owned(),
        messages: vec![json!({"role": "user", "content": "search docs"})],
    };
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_tool_search","name":"tool_search_tool","input":{"query":"abc"}}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_search_tool_result","tool_use_id":"srv_tool_search","content":[{"type":"text","text":"doc result"}]}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        Some(session),
        Some(json!({"model": "claude-opus-4-7"})),
        AnthropicToolNameMaps::default(),
    ))
    .await;

    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["output"][0]["type"], "function_call");
    assert_eq!(completed["output"][0]["name"], "tool_search_tool");

    let saved = global_response_session_cache()
        .get("resp_tool_search_native_blocks")
        .expect("response session should be saved");
    let saved_content = saved[1]["content"].as_array().unwrap();
    assert_eq!(saved_content[0]["type"], "server_tool_use");
    assert_eq!(saved_content[0]["name"], "tool_search_tool");
    assert_eq!(saved_content[0]["input"]["query"], "abc");
    assert_eq!(saved_content[1]["type"], "tool_search_tool_result");
    assert_eq!(saved_content[1]["content"][0]["text"], "doc result");
}

#[tokio::test]
async fn tool_use_stream_restores_sanitized_tool_name() {
    let maps = AnthropicToolNameMaps {
        forward: [("fs.read file".to_owned(), "fs_read_file".to_owned())].into(),
        reverse: [("fs_read_file".to_owned(), "fs.read file".to_owned())].into(),
    };
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-test","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_sanitized","name":"fs_read_file","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        None,
        None,
        maps,
    ))
    .await;
    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["output"][0]["name"], "fs.read file");
}

#[tokio::test]
async fn thinking_signature_is_saved_for_previous_response_continuation() {
    global_response_session_cache().clear();
    let session = ResponseSessionPlan {
        response_id: "resp_anthropic_signature_test".to_owned(),
        messages: vec![json!({"role": "user", "content": "hello"})],
    };
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need a signed block."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let converted = convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        Some(session),
        Some(json!({"model": "claude-opus-4-7"})),
        AnthropicToolNameMaps::default(),
    );
    let _ = collect_events(converted).await;

    let saved = global_response_session_cache()
        .get("resp_anthropic_signature_test")
        .expect("response session should be saved");
    assert_eq!(saved[1]["content"][0]["thinking"], "I need a signed block.");
    assert_eq!(saved[1]["content"][0]["signature"], "sig_abc");
}

#[tokio::test]
async fn error_event_maps_to_response_failed() {
    let events = collect_events(convert_fixture("error_stream.sse")).await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.in_progress",
            "response.failed"
        ]
    );
    let failed = &events.last().unwrap().1["response"];
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["code"], "overloaded_error");
    assert_eq!(failed["error"]["message"], "Overloaded");
}

#[tokio::test]
async fn unknown_events_are_ignored() {
    let events = collect_events(convert_fixture("unknown_event_stream.sse")).await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert!(!names.contains(&"anthropic_future_event"));
    assert_eq!(events.last().unwrap().0, "response.completed");
    assert_eq!(
        events.last().unwrap().1["response"]["output"][0]["content"][0]["text"],
        "ok"
    );
}

#[tokio::test]
async fn unsupported_content_deltas_are_preserved_as_trace_items() {
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-test","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"compaction","id":"cmp_1"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","summary":"short summary"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        None,
        None,
        AnthropicToolNameMaps::default(),
    ))
    .await;
    let completed = &events.last().unwrap().1["response"];
    let traces = completed["output"].as_array().expect("output array");
    assert!(traces.iter().any(|item| {
        item["type"] == "reasoning"
            && item["summary"][0]["text"]
                .as_str()
                .unwrap()
                .contains("delta:compaction_delta")
    }));
}

#[tokio::test]
async fn max_tokens_stop_reason_emits_incomplete() {
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-test","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":8}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream(raw),
        None,
        None,
        AnthropicToolNameMaps::default(),
    ))
    .await;
    assert_eq!(events.last().unwrap().0, "response.incomplete");
    assert_eq!(
        events.last().unwrap().1["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
}

#[tokio::test]
async fn stream_interruption_emits_incomplete_not_completed() {
    let raw = Bytes::from_static(
        br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-test","usage":{"input_tokens":1,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

"#,
    );
    let events = collect_events(convert_anthropic_messages_to_responses_stream(
        input_stream_chunked(raw, 7),
        None,
        None,
        AnthropicToolNameMaps::default(),
    ))
    .await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"response.incomplete"));
    assert!(!names.contains(&"response.completed"));
    assert_eq!(
        events.last().unwrap().1["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
}

#[tokio::test]
async fn stream_completion_saves_response_session() {
    global_response_session_cache().clear();
    let session = ResponseSessionPlan {
        response_id: "resp_anthropic_session_test".to_owned(),
        messages: vec![json!({"role": "user", "content": "hello"})],
    };
    let converted = convert_anthropic_messages_to_responses_stream(
        input_stream(read_fixture("text_stream.sse")),
        Some(session),
        Some(json!({"model": "claude-test"})),
        AnthropicToolNameMaps::default(),
    );
    let _ = collect_events(converted).await;

    let saved = global_response_session_cache()
        .get("resp_anthropic_session_test")
        .expect("response session should be saved");
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0]["role"], "user");
    assert_eq!(saved[1]["role"], "assistant");
    assert_eq!(saved[1]["content"][0]["type"], "text");
    assert_eq!(saved[1]["content"][0]["text"], "Hello");
}

#[tokio::test]
async fn compact_response_extracts_anthropic_content_text() {
    let upstream = json!({
        "id": "msg_compact",
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "text",
            "text": "<analysis>hidden</analysis><summary>Keep this context.</summary>",
        }],
    });
    let plan = build_anthropic_compact_response_plan(
        StatusCode::OK,
        HeaderMap::new(),
        input_stream(Bytes::from(serde_json::to_vec(&upstream).unwrap())),
    )
    .unwrap();
    let mut body = Vec::new();
    let mut stream = plan.stream;
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.unwrap());
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["output"][0]["type"], "compaction");
    let encrypted = parsed["output"][0]["encrypted_content"].as_str().unwrap();
    assert!(encrypted.ends_with("Keep this context."));
    assert!(!encrypted.contains("hidden"));
}
