use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};

use crate::core::events::{build_tool_namespace_map, emit_sse_event as emit_event};
use crate::responses::compact::compact_response_body_from_summary_text;
use crate::responses::{global_response_session_cache, global_tool_call_cache, ToolCallEntry};
use crate::types::{AdapterError, ByteStream, ResponsePlan, ResponseSessionPlan};

use super::request::AnthropicToolNameMaps;

const MAX_COMPACT_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const PROVIDER_TRACE_MAX_CHARS: usize = 4_000;

fn synthesize_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{ts:016x}{n:08x}")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn build_anthropic_messages_response_plan(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
    response_session: Option<ResponseSessionPlan>,
    original_responses_request: Option<Value>,
    tool_name_maps: AnthropicToolNameMaps,
    is_compact: bool,
) -> Result<ResponsePlan, AdapterError> {
    if is_compact {
        return build_anthropic_compact_response_plan(
            upstream_status,
            upstream_headers,
            upstream_stream,
        );
    }

    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);

    Ok(ResponsePlan {
        status: upstream_status,
        headers: upstream_headers,
        stream: convert_anthropic_messages_to_responses_stream(
            upstream_stream,
            response_session,
            original_responses_request,
            tool_name_maps,
        ),
    })
}

pub fn build_anthropic_compact_response_plan(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
) -> Result<ResponsePlan, AdapterError> {
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);

    let stream_with_logic = Box::pin(stream::once(async move {
        let body = collect_and_wrap_anthropic_compact_body(upstream_status, upstream_stream)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok::<Bytes, std::io::Error>(Bytes::from(body))
    }));

    Ok(ResponsePlan {
        status: if upstream_status.is_success() {
            StatusCode::OK
        } else {
            upstream_status
        },
        headers: upstream_headers,
        stream: stream_with_logic,
    })
}

async fn collect_and_wrap_anthropic_compact_body(
    upstream_status: StatusCode,
    mut upstream_stream: ByteStream,
) -> Result<Vec<u8>, AdapterError> {
    let mut buf = Vec::new();
    while let Some(chunk) = upstream_stream.next().await {
        let bytes = chunk.map_err(|e| AdapterError::Internal(format!("upstream io: {e}")))?;
        if buf.len() + bytes.len() > MAX_COMPACT_RESPONSE_BYTES {
            return Err(AdapterError::Internal(format!(
                "anthropic compact upstream response > {MAX_COMPACT_RESPONSE_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&bytes);
    }

    if !upstream_status.is_success() {
        return Ok(buf);
    }

    let parsed: Value = serde_json::from_slice(&buf).map_err(|e| {
        let preview: String = String::from_utf8_lossy(&buf).chars().take(500).collect();
        AdapterError::Internal(format!(
            "anthropic compact upstream non-JSON response: {e}; first 500 chars: {preview}"
        ))
    })?;
    let summary = extract_anthropic_text_content(&parsed).ok_or_else(|| {
        AdapterError::Internal("anthropic compact upstream missing content[].text".to_owned())
    })?;
    compact_response_body_from_summary_text(&summary)
}

fn extract_anthropic_text_content(parsed: &Value) -> Option<String> {
    if let Some(text) = parsed.get("content").and_then(|v| v.as_str()) {
        return Some(text.to_owned());
    }
    let content = parsed.get("content")?.as_array()?;
    let parts: Vec<String> = content
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            } else {
                None
            }
        })
        .filter(|s| !s.trim().is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

struct StreamState {
    input: ByteStream,
    conv: AnthropicMessagesToResponsesConverter,
    response_session: Option<ResponseSessionPlan>,
    finished: bool,
}

pub fn convert_anthropic_messages_to_responses_stream(
    input: ByteStream,
    response_session: Option<ResponseSessionPlan>,
    original_responses_request: Option<Value>,
    tool_name_maps: AnthropicToolNameMaps,
) -> ByteStream {
    let conv = AnthropicMessagesToResponsesConverter::new(
        response_session.as_ref().map(|s| s.response_id.clone()),
        original_responses_request,
        tool_name_maps,
    );
    convert_anthropic_messages_to_responses_stream_inner(input, conv, response_session)
}

fn convert_anthropic_messages_to_responses_stream_inner(
    input: ByteStream,
    conv: AnthropicMessagesToResponsesConverter,
    response_session: Option<ResponseSessionPlan>,
) -> ByteStream {
    let init = StreamState {
        input,
        conv,
        response_session,
        finished: false,
    };
    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(stream::unfold(init, |mut s| async move {
            loop {
                if s.finished {
                    return None;
                }
                match s.input.next().await {
                    Some(Ok(chunk)) => {
                        let out = s.conv.feed(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), s));
                        }
                    }
                    Some(Err(e)) => {
                        s.finished = true;
                        let mut out = s.conv.fail_transport_error(&e.to_string());
                        save_response_session(&mut s);
                        if out.is_empty() {
                            return Some((Err(e), s));
                        }
                        out.shrink_to_fit();
                        return Some((Ok(Bytes::from(out)), s));
                    }
                    None => {
                        s.finished = true;
                        let out = s.conv.finish();
                        save_response_session(&mut s);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), s));
                        }
                        return None;
                    }
                }
            }
        }));
    s
}

fn save_response_session(state: &mut StreamState) {
    let Some(session) = state.response_session.take() else {
        return;
    };
    let Some(assistant_message) = state.conv.assistant_message() else {
        return;
    };
    let mut messages = session.messages;
    messages.push(assistant_message);
    global_response_session_cache().save(&session.response_id, messages);
}

#[derive(Debug)]
pub struct AnthropicMessagesToResponsesConverter {
    buffer: BytesMut,
    response_id: String,
    model: String,
    sequence_number: u64,
    created_at: u64,
    original_request: Option<Value>,
    tool_namespace_map: std::collections::HashMap<String, String>,
    tool_name_reverse_map: BTreeMap<String, String>,
    lifecycle_opened: bool,
    terminal_emitted: bool,
    terminal_status: Option<String>,
    next_output_index: u32,
    open_blocks: BTreeMap<u32, OpenBlock>,
    closed_items: Vec<(u32, Value)>,
    closed_session_items: Vec<(u32, Value)>,
    closed_session_blocks: Vec<(u32, Value)>,
    pending_annotations: Vec<Value>,
    provider_metadata: serde_json::Map<String, Value>,
    code_by_call_id: BTreeMap<String, String>,
    container_id: Option<String>,
    final_stop_reason: Option<String>,
    final_stop_sequence: Option<String>,
    final_usage: Option<Value>,
}

#[derive(Debug)]
enum OpenBlock {
    Text(OpenText),
    Reasoning(OpenReasoning),
    Tool(OpenToolCall),
    WebSearch(OpenWebSearchCall),
    CodeInterpreter(OpenCodeInterpreterCall),
    Ignored,
}

#[derive(Debug)]
struct OpenText {
    item_id: String,
    output_index: u32,
    source_index: u32,
    text_acc: String,
    annotations_acc: Vec<Value>,
}

#[derive(Debug)]
struct OpenReasoning {
    item_id: String,
    output_index: u32,
    source_index: u32,
    text_acc: String,
    block_type: String,
    signature: Option<String>,
    redacted_data: Option<String>,
}

#[derive(Debug)]
struct OpenToolCall {
    item_id: String,
    output_index: u32,
    source_index: u32,
    call_id: String,
    name: String,
    upstream_name: String,
    block_type: String,
    arguments_acc: String,
    caller: Option<Value>,
    cache_control: Option<Value>,
}

#[derive(Debug)]
struct OpenWebSearchCall {
    item_id: String,
    output_index: u32,
    source_index: u32,
    arguments_acc: String,
}

#[derive(Debug)]
struct OpenCodeInterpreterCall {
    item_id: String,
    output_index: u32,
    source_index: u32,
    call_id: String,
    block_type: String,
    code: String,
    logs_acc: String,
    raw_content: Option<Value>,
}

impl AnthropicMessagesToResponsesConverter {
    pub fn new(
        response_id: Option<String>,
        original_request: Option<Value>,
        tool_name_maps: AnthropicToolNameMaps,
    ) -> Self {
        let response_id = response_id.unwrap_or_else(|| format!("resp_{}", synthesize_id()));
        let tool_namespace_map = build_tool_namespace_map(original_request.as_ref());
        Self {
            buffer: BytesMut::with_capacity(4096),
            response_id,
            model: String::new(),
            sequence_number: 0,
            created_at: now_unix_secs(),
            original_request,
            tool_namespace_map,
            tool_name_reverse_map: tool_name_maps.reverse,
            lifecycle_opened: false,
            terminal_emitted: false,
            terminal_status: None,
            next_output_index: 0,
            open_blocks: BTreeMap::new(),
            closed_items: Vec::new(),
            closed_session_items: Vec::new(),
            closed_session_blocks: Vec::new(),
            pending_annotations: Vec::new(),
            provider_metadata: serde_json::Map::new(),
            code_by_call_id: BTreeMap::new(),
            container_id: None,
            final_stop_reason: None,
            final_stop_sequence: None,
            final_usage: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.terminal_emitted {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(frame) = drain_one_frame(&mut self.buffer) {
            self.handle_frame(&frame, &mut out);
            if self.terminal_emitted {
                break;
            }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            self.buffer.extend_from_slice(b"\n\n");
            if let Some(frame) = drain_one_frame(&mut self.buffer) {
                self.handle_frame(&frame, &mut out);
            }
        }
        if !self.terminal_emitted {
            self.final_stop_reason
                .get_or_insert_with(|| "interrupted".to_owned());
            self.emit_terminal(&mut out, true);
        }
        out
    }

    fn fail_transport_error(&mut self, message: &str) -> Vec<u8> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.emit_failure("upstream_transport_error", message, &mut out);
        out
    }

    pub fn assistant_message(&self) -> Option<Value> {
        if self.terminal_status.as_deref() == Some("failed") {
            return None;
        }
        if !self.closed_session_blocks.is_empty() {
            let mut session_blocks = self.closed_session_blocks.clone();
            session_blocks.sort_by_key(|(idx, _)| *idx);
            return Some(json!({
                "role": "assistant",
                "content": session_blocks
                    .into_iter()
                    .map(|(_, block)| block)
                    .collect::<Vec<_>>(),
            }));
        }
        let mut session_items = self.closed_session_items.clone();
        session_items.sort_by_key(|(idx, _)| *idx);
        assistant_message_from_output_items(session_items.iter().map(|(_, item)| item))
    }

    fn handle_frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let Some((event_name, data)) = parse_sse_frame(frame) else {
            return;
        };
        let event = if event_name.is_empty() {
            data.get("type").and_then(|v| v.as_str()).unwrap_or("")
        } else {
            event_name.as_str()
        };
        match event {
            "message_start" => self.handle_message_start(&data, out),
            "content_block_start" => self.handle_content_block_start(&data, out),
            "content_block_delta" => self.handle_content_block_delta(&data, out),
            "content_block_stop" => self.handle_content_block_stop(&data, out),
            "message_delta" => self.handle_message_delta(&data),
            "message_stop" => self.emit_terminal(out, false),
            "error" => self.handle_error(&data, out),
            "ping" => {}
            other => {
                tracing::trace!(
                    anthropic_event = other,
                    "ignoring unknown Anthropic Messages SSE event"
                );
            }
        }
    }

    fn handle_message_start(&mut self, data: &Value, out: &mut Vec<u8>) {
        if let Some(model) = data
            .pointer("/message/model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            self.model = model.to_owned();
        }
        if let Some(usage) = data.pointer("/message/usage") {
            self.merge_usage(usage);
        }
        if let Some(container) = data.pointer("/message/container") {
            self.capture_container(container);
            self.capture_provider_field("anthropic_container", container);
        }
        if let Some(context_management) = data.pointer("/message/context_management") {
            self.capture_provider_field("anthropic_context_management", context_management);
        }
        if let Some(compaction) = data.pointer("/message/compaction") {
            self.capture_provider_field("anthropic_compaction", compaction);
        }
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(out);
        }
    }

    fn handle_content_block_start(&mut self, data: &Value, out: &mut Vec<u8>) {
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(out);
        }
        let Some(index) = data.get("index").and_then(|v| v.as_u64()).map(|n| n as u32) else {
            return;
        };
        let block = data.get("content_block").unwrap_or(&Value::Null);
        match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let mut annotations = anthropic_citations_to_annotations(block.get("citations"));
                annotations.append(&mut self.pending_annotations);
                self.open_text(index, text, annotations, out);
            }
            "thinking" => {
                let text = block
                    .get("thinking")
                    .or_else(|| block.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.open_reasoning(index, "thinking", text, signature, None, out);
            }
            "redacted_thinking" => {
                let data = block.get("data").and_then(Value::as_str).map(str::to_owned);
                self.open_reasoning(index, "redacted_thinking", "", None, data, out);
            }
            "tool_use" => {
                self.open_tool_call(index, block, out);
            }
            "server_tool_use" => {
                if block.get("name").and_then(Value::as_str) == Some("web_search") {
                    self.open_web_search_call(index, block, out);
                } else {
                    self.open_tool_call(index, block, out);
                }
            }
            "bash_code_execution_tool_result" | "code_execution_tool_result" => {
                self.open_code_interpreter_result(index, block, out);
            }
            "web_search_tool_result" | "web_fetch_tool_result" => {
                let annotations = anthropic_web_search_result_annotations(block);
                self.pending_annotations.extend(annotations);
                self.closed_session_blocks.push((index, block.clone()));
                self.open_blocks.insert(index, OpenBlock::Ignored);
            }
            block_type @ ("tool_search_tool_result" | "advisor_tool_result" | "compaction") => {
                self.closed_session_blocks.push((index, block.clone()));
                self.emit_provider_trace_item(index, block_type, block, out);
                self.open_blocks.insert(index, OpenBlock::Ignored);
            }
            other if other.ends_with("_tool_result") => {
                self.closed_session_blocks.push((index, block.clone()));
                self.emit_provider_trace_item(index, other, block, out);
                self.open_blocks.insert(index, OpenBlock::Ignored);
            }
            other => {
                tracing::trace!(
                    content_block_type = other,
                    "preserving unsupported Anthropic content block as trace item"
                );
                self.emit_provider_trace_item(index, other, block, out);
                self.open_blocks.insert(index, OpenBlock::Ignored);
            }
        }
    }

    fn handle_content_block_delta(&mut self, data: &Value, out: &mut Vec<u8>) {
        let Some(index) = data.get("index").and_then(|v| v.as_u64()).map(|n| n as u32) else {
            return;
        };
        let delta = data.get("delta").unwrap_or(&Value::Null);
        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut handled = false;
        match (self.open_blocks.get_mut(&index), delta_type) {
            (Some(OpenBlock::Text(text)), "text_delta") => {
                if let Some(value) = delta.get("text").and_then(|v| v.as_str()) {
                    emit_text_delta(text, &mut self.sequence_number, out, value);
                    handled = true;
                }
            }
            (Some(OpenBlock::Text(text)), "citation_delta") => {
                let annotations = anthropic_citations_to_annotations(delta.get("citation"));
                emit_text_annotations(text, &mut self.sequence_number, out, annotations);
                handled = true;
            }
            (Some(OpenBlock::Reasoning(reasoning)), "thinking_delta") => {
                if let Some(value) = delta.get("thinking").and_then(|v| v.as_str()) {
                    emit_reasoning_delta(reasoning, &mut self.sequence_number, out, value);
                    handled = true;
                }
            }
            (Some(OpenBlock::Reasoning(reasoning)), "signature_delta") => {
                if let Some(value) = delta.get("signature").and_then(Value::as_str) {
                    reasoning.signature = Some(value.to_owned());
                    handled = true;
                }
            }
            (Some(OpenBlock::Tool(tool)), "input_json_delta") => {
                if let Some(value) = delta.get("partial_json").and_then(|v| v.as_str()) {
                    emit_tool_arguments_delta(tool, &mut self.sequence_number, out, value);
                    handled = true;
                }
            }
            (Some(OpenBlock::WebSearch(search)), "input_json_delta") => {
                if let Some(value) = delta.get("partial_json").and_then(Value::as_str) {
                    search.arguments_acc.push_str(value);
                    handled = true;
                }
            }
            (Some(OpenBlock::CodeInterpreter(code)), _) => {
                append_code_interpreter_delta(code, delta);
                handled = true;
            }
            _ => {}
        }
        if delta_type != "citation_delta" {
            if let Some(OpenBlock::Text(text)) = self.open_blocks.get_mut(&index) {
                let mut annotations = anthropic_citations_to_annotations(delta.get("citation"));
                annotations.extend(anthropic_citations_to_annotations(delta.get("citations")));
                emit_text_annotations(text, &mut self.sequence_number, out, annotations);
            }
        }
        if !handled && !delta_type.is_empty() {
            self.emit_provider_trace_item(index, &format!("delta:{delta_type}"), delta, out);
        }
    }

    fn handle_content_block_stop(&mut self, data: &Value, out: &mut Vec<u8>) {
        let Some(index) = data.get("index").and_then(|v| v.as_u64()).map(|n| n as u32) else {
            return;
        };
        let Some(block) = self.open_blocks.remove(&index) else {
            return;
        };
        match block {
            OpenBlock::Text(text) => self.close_text(text, out),
            OpenBlock::Reasoning(reasoning) => self.close_reasoning(reasoning, out),
            OpenBlock::Tool(tool) => self.close_tool_call(tool, out),
            OpenBlock::WebSearch(search) => self.close_web_search_call(search, out),
            OpenBlock::CodeInterpreter(code) => self.close_code_interpreter_result(code, out),
            OpenBlock::Ignored => {}
        }
    }

    fn handle_message_delta(&mut self, data: &Value) {
        if let Some(stop_reason) = data
            .pointer("/delta/stop_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            self.final_stop_reason = Some(stop_reason.to_owned());
        }
        if let Some(stop_sequence) = data
            .pointer("/delta/stop_sequence")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            self.final_stop_sequence = Some(stop_sequence.to_owned());
        }
        if let Some(usage) = data.get("usage") {
            self.merge_usage(usage);
        }
        if let Some(container) = data
            .pointer("/delta/container")
            .or_else(|| data.get("container"))
        {
            self.capture_container(container);
            self.capture_provider_field("anthropic_container", container);
        }
        if let Some(context_management) = data
            .pointer("/delta/context_management")
            .or_else(|| data.get("context_management"))
        {
            self.capture_provider_field("anthropic_context_management", context_management);
        }
        if let Some(compaction) = data
            .pointer("/delta/compaction")
            .or_else(|| data.get("compaction"))
        {
            self.capture_provider_field("anthropic_compaction", compaction);
        }
    }

    fn handle_error(&mut self, data: &Value, out: &mut Vec<u8>) {
        let code = data
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic_error");
        let message = data
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("Anthropic upstream returned an error event");
        self.emit_failure(code, message, out);
    }

    fn emit_provider_trace_item(
        &mut self,
        _index: u32,
        block_type: &str,
        block: &Value,
        out: &mut Vec<u8>,
    ) {
        let item_id = format!("rs_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let summary = format!(
            "Anthropic content block preserved as JSON trace ({block_type}): {}",
            bounded_json_trace(block)
        );
        let item = json!({
            "type": "reasoning",
            "id": item_id,
            "status": "completed",
            "summary": [{
                "type": "summary_text",
                "text": summary,
            }],
            "content": null,
            "encrypted_content": null,
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item.clone(),
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone(),
            }),
        );
        self.closed_items.push((output_index, item));
    }

    fn open_text(
        &mut self,
        index: u32,
        initial_text: &str,
        initial_annotations: Vec<Value>,
        out: &mut Vec<u8>,
    ) {
        let item_id = format!("msg_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": item_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
            }),
        );
        let mut text = OpenText {
            item_id,
            output_index,
            source_index: index,
            text_acc: String::new(),
            annotations_acc: Vec::new(),
        };
        emit_text_annotations(
            &mut text,
            &mut self.sequence_number,
            out,
            initial_annotations,
        );
        if !initial_text.is_empty() {
            emit_text_delta(&mut text, &mut self.sequence_number, out, initial_text);
        }
        self.open_blocks.insert(index, OpenBlock::Text(text));
    }

    fn close_text(&mut self, text: OpenText, out: &mut Vec<u8>) {
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "text": text.text_acc,
            }),
        );
        let annotations = Value::Array(text.annotations_acc.clone());
        emit_event(
            out,
            &mut self.sequence_number,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": text.text_acc,
                    "annotations": annotations,
                },
            }),
        );
        let item = json!({
            "type": "message",
            "id": text.item_id,
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text.text_acc,
                "annotations": text.annotations_acc,
            }],
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": text.output_index,
                "item": item.clone(),
            }),
        );
        self.closed_items.push((text.output_index, item.clone()));
        self.closed_session_items.push((text.output_index, item));
        if !text.text_acc.is_empty() {
            self.closed_session_blocks.push((
                text.source_index,
                json!({
                    "type": "text",
                    "text": text.text_acc,
                }),
            ));
        }
    }

    fn open_reasoning(
        &mut self,
        index: u32,
        block_type: &str,
        initial_text: &str,
        signature: Option<String>,
        redacted_data: Option<String>,
        out: &mut Vec<u8>,
    ) {
        let item_id = format!("rs_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "reasoning",
                    "status": "in_progress",
                    "id": item_id,
                    "summary": [],
                    "content": null,
                    "encrypted_content": null,
                },
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_part.added",
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        );
        let mut reasoning = OpenReasoning {
            item_id,
            output_index,
            source_index: index,
            text_acc: String::new(),
            block_type: block_type.to_owned(),
            signature,
            redacted_data,
        };
        if !initial_text.is_empty() {
            emit_reasoning_delta(&mut reasoning, &mut self.sequence_number, out, initial_text);
        }
        self.open_blocks
            .insert(index, OpenBlock::Reasoning(reasoning));
    }

    fn close_reasoning(&mut self, reasoning: OpenReasoning, out: &mut Vec<u8>) {
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_text.done",
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": reasoning.item_id,
                "output_index": reasoning.output_index,
                "summary_index": 0,
                "text": reasoning.text_acc,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.reasoning_summary_part.done",
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": reasoning.item_id,
                "output_index": reasoning.output_index,
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": reasoning.text_acc,
                },
            }),
        );
        let item = json!({
            "type": "reasoning",
            "id": reasoning.item_id,
            "status": "completed",
            "summary": [{
                "type": "summary_text",
                "text": reasoning.text_acc,
            }],
            "content": null,
            "encrypted_content": null,
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": reasoning.output_index,
                "item": item.clone(),
            }),
        );
        let session_item = merge_anthropic_thinking_context(item.clone(), &reasoning);
        self.closed_items.push((reasoning.output_index, item));
        self.closed_session_items
            .push((reasoning.output_index, session_item));
        if let Some(block) = anthropic_thinking_block_from_reasoning(&reasoning) {
            self.closed_session_blocks
                .push((reasoning.source_index, block));
        }
    }

    fn open_tool_call(&mut self, index: u32, block: &Value, out: &mut Vec<u8>) {
        let item_id = format!("fc_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let call_id = block
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_{}", synthesize_id()));
        let upstream_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("tool_use");
        let name = self.restore_tool_name(upstream_name);
        let mut item = json!({
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": name,
            "arguments": "",
            "status": "in_progress",
        });
        copy_json_field(block, &mut item, "caller");
        copy_json_field(block, &mut item, "cache_control");
        if let Some(namespace) = self.lookup_namespace_for(&name) {
            item["namespace"] = Value::String(namespace.to_owned());
        }
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item,
            }),
        );

        let mut tool = OpenToolCall {
            item_id,
            output_index,
            source_index: index,
            call_id,
            name,
            upstream_name: upstream_name.to_owned(),
            block_type: block_type.to_owned(),
            arguments_acc: String::new(),
            caller: block.get("caller").cloned(),
            cache_control: block.get("cache_control").cloned(),
        };
        if let Some(initial) = block.get("input").filter(|v| !is_empty_json_object(v)) {
            let initial = serde_json::to_string(initial).unwrap_or_else(|_| "{}".to_owned());
            emit_tool_arguments_delta(&mut tool, &mut self.sequence_number, out, &initial);
        }
        self.open_blocks.insert(index, OpenBlock::Tool(tool));
    }

    fn close_tool_call(&mut self, mut tool: OpenToolCall, out: &mut Vec<u8>) {
        if tool.arguments_acc.is_empty() {
            tool.arguments_acc.push_str("{}");
        }
        let session_block = anthropic_tool_call_block_from_open_tool(&tool);
        emit_event(
            out,
            &mut self.sequence_number,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": tool.item_id,
                "output_index": tool.output_index,
                "arguments": tool.arguments_acc,
            }),
        );
        let mut item = json!({
            "type": "function_call",
            "id": tool.item_id,
            "call_id": tool.call_id,
            "name": tool.name,
            "arguments": tool.arguments_acc,
            "status": "completed",
        });
        if let Some(caller) = tool.caller.take() {
            item["caller"] = caller;
        }
        if let Some(cache_control) = tool.cache_control.take() {
            item["cache_control"] = cache_control;
        }
        if let Some(namespace) = self.lookup_namespace_for(
            item.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        ) {
            item["namespace"] = Value::String(namespace.to_owned());
        }
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            if is_code_execution_tool_name(item.get("name").and_then(Value::as_str).unwrap_or(""))
                || item.get("caller").is_some()
            {
                if let Some(code) = code_from_tool_arguments(
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ) {
                    self.code_by_call_id.insert(call_id.to_owned(), code);
                }
            }
        }
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": tool.output_index,
                "item": item.clone(),
            }),
        );
        if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
            global_tool_call_cache().save(
                call_id,
                ToolCallEntry {
                    name: item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                },
            );
        }
        self.closed_items.push((tool.output_index, item.clone()));
        self.closed_session_items.push((tool.output_index, item));
        self.closed_session_blocks
            .push((tool.source_index, session_block));
    }

    fn open_web_search_call(&mut self, index: u32, block: &Value, out: &mut Vec<u8>) {
        let item_id = block
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ws_{}", synthesize_id()));
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item = json!({
            "type": "web_search_call",
            "id": item_id,
            "status": "in_progress",
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.web_search_call.in_progress",
            json!({
                "type": "response.web_search_call.in_progress",
                "item_id": item_id.clone(),
                "output_index": output_index,
            }),
        );
        let mut search = OpenWebSearchCall {
            item_id,
            output_index,
            source_index: index,
            arguments_acc: String::new(),
        };
        if let Some(initial) = block.get("input").filter(|v| !is_empty_json_object(v)) {
            search.arguments_acc = serde_json::to_string(initial).unwrap_or_default();
        }
        self.open_blocks.insert(index, OpenBlock::WebSearch(search));
    }

    fn close_web_search_call(&mut self, search: OpenWebSearchCall, out: &mut Vec<u8>) {
        let item_id = search.item_id.clone();
        let mut item = json!({
            "type": "web_search_call",
            "id": item_id,
            "status": "completed",
        });
        if let Some(action) = web_search_action_from_arguments(&search.arguments_acc) {
            item["action"] = action;
        }
        emit_event(
            out,
            &mut self.sequence_number,
            "response.web_search_call.completed",
            json!({
                "type": "response.web_search_call.completed",
                "item_id": item["id"].clone(),
                "output_index": search.output_index,
            }),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": search.output_index,
                "item": item.clone(),
            }),
        );
        self.closed_items.push((search.output_index, item.clone()));
        self.closed_session_items.push((search.output_index, item));
        let input = serde_json::from_str(&search.arguments_acc).unwrap_or_else(|_| json!({}));
        self.closed_session_blocks.push((
            search.source_index,
            json!({
                "type": "server_tool_use",
                "id": item_id,
                "name": "web_search",
                "input": input,
            }),
        ));
    }

    fn open_code_interpreter_result(&mut self, index: u32, block: &Value, out: &mut Vec<u8>) {
        let item_id = format!("ci_{}", synthesize_id());
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let call_id = block
            .get("tool_use_id")
            .or_else(|| block.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_{}", synthesize_id()));
        let code = self
            .code_by_call_id
            .get(&call_id)
            .cloned()
            .unwrap_or_default();
        let logs_acc = block
            .get("content")
            .map(anthropic_content_to_text)
            .unwrap_or_default();
        let raw_content = block.get("content").cloned();
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("bash_code_execution_tool_result")
            .to_owned();
        let mut item = json!({
            "type": "code_interpreter_call",
            "id": item_id,
            "call_id": call_id,
            "status": "in_progress",
            "code": code,
            "outputs": [],
        });
        if let Some(container_id) = self.container_id.as_deref().filter(|s| !s.is_empty()) {
            item["container_id"] = Value::String(container_id.to_owned());
        }
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item,
            }),
        );
        self.open_blocks.insert(
            index,
            OpenBlock::CodeInterpreter(OpenCodeInterpreterCall {
                item_id,
                output_index,
                source_index: index,
                call_id,
                block_type,
                code,
                logs_acc,
                raw_content,
            }),
        );
    }

    fn close_code_interpreter_result(&mut self, code: OpenCodeInterpreterCall, out: &mut Vec<u8>) {
        let mut outputs = Vec::new();
        if !code.logs_acc.trim().is_empty() {
            outputs.push(json!({
                "type": "logs",
                "logs": code.logs_acc.clone(),
            }));
        }
        let mut item = json!({
            "type": "code_interpreter_call",
            "id": code.item_id.clone(),
            "call_id": code.call_id.clone(),
            "status": "completed",
            "code": code.code.clone(),
            "outputs": outputs,
        });
        if let Some(container_id) = self.container_id.as_deref().filter(|s| !s.is_empty()) {
            item["container_id"] = Value::String(container_id.to_owned());
        }
        emit_event(
            out,
            &mut self.sequence_number,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": code.output_index,
                "item": item.clone(),
            }),
        );
        self.closed_items.push((code.output_index, item));
        self.closed_session_blocks.push((
            code.source_index,
            json!({
                "type": code.block_type,
                "tool_use_id": code.call_id,
                "content": code.raw_content.unwrap_or_else(|| {
                    Value::Array(vec![json!({
                        "type": "text",
                        "text": code.logs_acc,
                    })])
                }),
            }),
        ));
    }

    fn emit_lifecycle_open(&mut self, out: &mut Vec<u8>) {
        self.lifecycle_opened = true;
        let mut envelope = self.build_envelope("in_progress");
        envelope["output"] = json!([]);
        envelope["usage"] = Value::Null;
        envelope["incomplete_details"] = Value::Null;
        envelope["error"] = Value::Null;
        emit_event(
            out,
            &mut self.sequence_number,
            "response.created",
            json!({"type": "response.created", "response": envelope.clone()}),
        );
        emit_event(
            out,
            &mut self.sequence_number,
            "response.in_progress",
            json!({"type": "response.in_progress", "response": envelope}),
        );
    }

    fn emit_terminal(&mut self, out: &mut Vec<u8>, interrupted: bool) {
        if self.terminal_emitted {
            return;
        }
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(out);
        }
        self.close_all_blocks(out);
        let (status, incomplete_details) = self.terminal_status(interrupted);
        let mut envelope = self.build_envelope(status);
        let mut items = self.closed_items.clone();
        items.sort_by_key(|(idx, _)| *idx);
        envelope["output"] = Value::Array(items.into_iter().map(|(_, item)| item).collect());
        envelope["usage"] = normalize_anthropic_usage(self.final_usage.as_ref());
        envelope["incomplete_details"] = incomplete_details;
        envelope["error"] = Value::Null;
        if self.final_stop_sequence.is_some() {
            envelope["metadata"] = merge_metadata_field(
                envelope.get("metadata").cloned().unwrap_or(Value::Null),
                "anthropic_stop_sequence",
                self.final_stop_sequence.clone().unwrap_or_default(),
            );
        }

        let event_name = format!("response.{status}");
        emit_event(
            out,
            &mut self.sequence_number,
            &event_name,
            json!({"type": event_name, "response": envelope}),
        );
        self.terminal_status = Some(status.to_owned());
        self.terminal_emitted = true;
    }

    fn emit_failure(&mut self, code: &str, message: &str, out: &mut Vec<u8>) {
        if self.terminal_emitted {
            return;
        }
        if !self.lifecycle_opened {
            self.emit_lifecycle_open(out);
        }
        self.close_all_blocks(out);
        let mut items = self.closed_items.clone();
        items.sort_by_key(|(idx, _)| *idx);
        let mut envelope = self.build_envelope("failed");
        envelope["output"] = Value::Array(items.into_iter().map(|(_, item)| item).collect());
        envelope["usage"] = normalize_anthropic_usage(self.final_usage.as_ref());
        envelope["incomplete_details"] = Value::Null;
        envelope["error"] = json!({
            "code": code,
            "message": message,
            "type": "anthropic_error",
        });
        emit_event(
            out,
            &mut self.sequence_number,
            "response.failed",
            json!({"type": "response.failed", "response": envelope}),
        );
        self.terminal_status = Some("failed".to_owned());
        self.terminal_emitted = true;
    }

    fn close_all_blocks(&mut self, out: &mut Vec<u8>) {
        let indices: Vec<u32> = self.open_blocks.keys().copied().collect();
        for index in indices {
            if let Some(block) = self.open_blocks.remove(&index) {
                match block {
                    OpenBlock::Text(text) => self.close_text(text, out),
                    OpenBlock::Reasoning(reasoning) => self.close_reasoning(reasoning, out),
                    OpenBlock::Tool(tool) => self.close_tool_call(tool, out),
                    OpenBlock::WebSearch(search) => self.close_web_search_call(search, out),
                    OpenBlock::CodeInterpreter(code) => {
                        self.close_code_interpreter_result(code, out)
                    }
                    OpenBlock::Ignored => {}
                }
            }
        }
    }

    fn terminal_status(&self, interrupted: bool) -> (&'static str, Value) {
        if interrupted {
            return ("incomplete", json!({"reason": "interrupted"}));
        }
        match self.final_stop_reason.as_deref() {
            Some("max_tokens") => ("incomplete", json!({"reason": "max_output_tokens"})),
            Some("refusal") => ("incomplete", json!({"reason": "content_filter"})),
            Some("end_turn") | Some("tool_use") | Some("stop_sequence") | None => {
                ("completed", Value::Null)
            }
            Some(other) => ("incomplete", json!({"reason": other})),
        }
    }

    fn build_envelope(&self, status: &str) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model_for_envelope(),
            "tools": self.req_field_or("tools", json!([])),
            "tool_choice": self.req_field_or("tool_choice", json!("auto")),
            "parallel_tool_calls": self.req_field_or("parallel_tool_calls", json!(true)),
            "reasoning": self.req_field_or("reasoning", json!({"effort": null, "summary": null})),
            "text": self.req_field_or("text", json!({"format": {"type": "text"}})),
            "metadata": self.response_metadata(),
            "previous_response_id": self.req_field_or("previous_response_id", Value::Null),
            "instructions": self.req_field_or("instructions", Value::Null),
            "temperature": self.req_field_or("temperature", Value::Null),
            "top_p": self.req_field_or("top_p", Value::Null),
            "max_output_tokens": self.req_field_or("max_output_tokens", Value::Null),
            "truncation": "disabled",
        })
    }

    fn model_for_envelope(&self) -> String {
        if !self.model.is_empty() {
            return self.model.clone();
        }
        self.original_request
            .as_ref()
            .and_then(|r| r.get("model"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| "unknown".to_owned())
    }

    fn req_field_or(&self, key: &str, fallback: Value) -> Value {
        self.original_request
            .as_ref()
            .and_then(|v| v.get(key))
            .cloned()
            .unwrap_or(fallback)
    }

    fn response_metadata(&self) -> Value {
        merge_metadata_map(
            self.req_field_or("metadata", Value::Null),
            &self.provider_metadata,
        )
    }

    fn lookup_namespace_for(&self, tool_name: &str) -> Option<&str> {
        self.tool_namespace_map.get(tool_name).map(String::as_str)
    }

    fn restore_tool_name(&self, upstream_name: &str) -> String {
        self.tool_name_reverse_map
            .get(upstream_name)
            .cloned()
            .unwrap_or_else(|| upstream_name.to_owned())
    }

    fn merge_usage(&mut self, usage: &Value) {
        let Some(obj) = usage.as_object() else {
            return;
        };
        let mut merged = self
            .final_usage
            .take()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for (key, value) in obj {
            merged.insert(key.clone(), value.clone());
        }
        self.final_usage = Some(Value::Object(merged));
    }

    fn capture_container(&mut self, value: &Value) {
        if let Some(container_id) = value
            .get("id")
            .or_else(|| value.get("container_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            self.container_id = Some(container_id.to_owned());
        }
    }

    fn capture_provider_field(&mut self, key: &str, value: &Value) {
        if value.is_null() {
            return;
        }
        self.provider_metadata.insert(key.to_owned(), value.clone());
    }
}

fn emit_text_delta(text: &mut OpenText, seq: &mut u64, out: &mut Vec<u8>, delta: &str) {
    if delta.is_empty() {
        return;
    }
    text.text_acc.push_str(delta);
    emit_event(
        out,
        seq,
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": text.item_id,
            "output_index": text.output_index,
            "content_index": 0,
            "delta": delta,
        }),
    );
}

fn emit_reasoning_delta(
    reasoning: &mut OpenReasoning,
    seq: &mut u64,
    out: &mut Vec<u8>,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    reasoning.text_acc.push_str(delta);
    emit_event(
        out,
        seq,
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": reasoning.item_id,
            "output_index": reasoning.output_index,
            "summary_index": 0,
            "delta": delta,
        }),
    );
}

fn emit_tool_arguments_delta(
    tool: &mut OpenToolCall,
    seq: &mut u64,
    out: &mut Vec<u8>,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    tool.arguments_acc.push_str(delta);
    emit_event(
        out,
        seq,
        "response.function_call_arguments.delta",
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": tool.item_id,
            "output_index": tool.output_index,
            "delta": delta,
        }),
    );
}

fn emit_text_annotations(
    text: &mut OpenText,
    seq: &mut u64,
    out: &mut Vec<u8>,
    annotations: Vec<Value>,
) {
    for annotation in annotations {
        let annotation_index = text.annotations_acc.len();
        text.annotations_acc.push(annotation.clone());
        emit_event(
            out,
            seq,
            "response.output_text.annotation.added",
            json!({
                "type": "response.output_text.annotation.added",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "annotation_index": annotation_index,
                "annotation": annotation,
            }),
        );
    }
}

fn append_code_interpreter_delta(code: &mut OpenCodeInterpreterCall, delta: &Value) {
    let text = delta
        .get("text")
        .or_else(|| delta.get("logs"))
        .or_else(|| delta.get("partial_json"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            delta
                .get("content")
                .map(anthropic_content_to_text)
                .unwrap_or_default()
        });
    if !text.trim().is_empty() {
        if !code.logs_acc.is_empty() && !code.logs_acc.ends_with('\n') {
            code.logs_acc.push('\n');
        }
        code.logs_acc.push_str(&text);
    }
}

fn anthropic_citations_to_annotations(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(anthropic_citation_to_annotation)
            .collect(),
        Some(Value::Object(_)) => value
            .and_then(anthropic_citation_to_annotation)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn anthropic_citation_to_annotation(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let url = obj
        .get("url")
        .or_else(|| obj.get("uri"))
        .and_then(Value::as_str)?;
    if url.trim().is_empty() {
        return None;
    }
    let title = obj
        .get("title")
        .or_else(|| obj.get("source_title"))
        .or_else(|| obj.get("page_title"))
        .and_then(Value::as_str)
        .unwrap_or(url);
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String("url_citation".into()));
    out.insert("url".into(), Value::String(url.to_owned()));
    out.insert("title".into(), Value::String(title.to_owned()));
    out.insert("start_index".into(), json!(0));
    out.insert("end_index".into(), json!(0));
    if let Some(snippet) = obj
        .get("cited_text")
        .or_else(|| obj.get("snippet"))
        .or_else(|| obj.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        out.insert("snippet".into(), Value::String(snippet.to_owned()));
    }
    Some(Value::Object(out))
}

fn anthropic_web_search_result_annotations(block: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    collect_web_search_result_annotations(block.get("content").unwrap_or(block), &mut out);
    out
}

fn collect_web_search_result_annotations(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_web_search_result_annotations(item, out);
            }
        }
        Value::Object(obj) => {
            if let Some(annotation) = anthropic_citation_to_annotation(value) {
                out.push(annotation);
            }
            for key in ["content", "results", "citations"] {
                if let Some(child) = obj.get(key) {
                    collect_web_search_result_annotations(child, out);
                }
            }
        }
        _ => {}
    }
}

fn web_search_action_from_arguments(arguments: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    let query = parsed
        .get("query")
        .or_else(|| parsed.get("q"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())?;
    Some(json!({
        "type": "search",
        "query": query,
    }))
}

fn anthropic_content_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(anthropic_content_to_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(obj) => {
            for key in ["text", "content", "output", "logs"] {
                if let Some(text) = obj.get(key).and_then(Value::as_str) {
                    return text.to_owned();
                }
            }
            for key in ["content", "output", "outputs", "results"] {
                if let Some(child) = obj.get(key) {
                    let text = anthropic_content_to_text(child);
                    if !text.trim().is_empty() {
                        return text;
                    }
                }
            }
            bounded_json_trace(value)
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_code_execution_tool_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("code_execution") || lower == "bash" || lower.ends_with("_bash")
}

fn code_from_tool_arguments(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    for key in ["command", "code", "script", "input"] {
        if let Some(value) = parsed.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn copy_json_field(source: &Value, target: &mut Value, key: &str) {
    if let Some(value) = source.get(key) {
        target[key] = value.clone();
    }
}

fn bounded_json_trace(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    let total = text.chars().count();
    if total <= PROVIDER_TRACE_MAX_CHARS {
        return text;
    }
    let kept = text
        .chars()
        .take(PROVIDER_TRACE_MAX_CHARS)
        .collect::<String>();
    format!(
        "{kept}...(+{} chars truncated)",
        total - PROVIDER_TRACE_MAX_CHARS
    )
}

fn merge_anthropic_thinking_context(mut item: Value, reasoning: &OpenReasoning) -> Value {
    let Some(obj) = item.as_object_mut() else {
        return item;
    };
    let Some(block) = anthropic_thinking_block_from_reasoning(reasoning) else {
        return item;
    };
    obj.insert("anthropic_thinking".into(), block);
    item
}

fn anthropic_thinking_block_from_reasoning(reasoning: &OpenReasoning) -> Option<Value> {
    let mut block = serde_json::Map::new();
    if reasoning.block_type == "redacted_thinking" {
        let data = reasoning.redacted_data.as_deref()?;
        block.insert("type".into(), Value::String("redacted_thinking".into()));
        block.insert("data".into(), Value::String(data.to_owned()));
    } else {
        if reasoning.text_acc.is_empty() {
            return None;
        }
        block.insert("type".into(), Value::String("thinking".into()));
        block.insert("thinking".into(), Value::String(reasoning.text_acc.clone()));
        if let Some(signature) = reasoning.signature.as_deref().filter(|s| !s.is_empty()) {
            block.insert("signature".into(), Value::String(signature.to_owned()));
        }
    }
    Some(Value::Object(block))
}

fn anthropic_tool_call_block_from_open_tool(tool: &OpenToolCall) -> Value {
    let mut block = serde_json::Map::new();
    block.insert("type".into(), Value::String(tool.block_type.clone()));
    block.insert("id".into(), Value::String(tool.call_id.clone()));
    block.insert("name".into(), Value::String(tool.upstream_name.clone()));
    block.insert(
        "input".into(),
        serde_json::from_str(&tool.arguments_acc).unwrap_or_else(|_| json!({})),
    );
    if let Some(caller) = tool.caller.clone() {
        block.insert("caller".into(), caller);
    }
    if let Some(cache_control) = tool.cache_control.clone() {
        block.insert("cache_control".into(), cache_control);
    }
    Value::Object(block)
}

fn is_empty_json_object(value: &Value) -> bool {
    value.as_object().map(|obj| obj.is_empty()).unwrap_or(false)
}

fn normalize_anthropic_usage(usage: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = usage else {
        return zero_usage();
    };
    let input_tokens = map
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = map
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_tokens = map
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = map
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = map
        .get("thinking_tokens")
        .or_else(|| map.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut out = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "input_tokens_details": {
            "cached_tokens": cached_tokens,
            "cache_creation_tokens": cache_creation_tokens,
        },
        "output_tokens_details": {
            "reasoning_tokens": reasoning_tokens,
        },
    });
    if let Some(server_tool_use) = map.get("server_tool_use") {
        out["server_tool_use"] = server_tool_use.clone();
    }
    out
}

fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
    })
}

fn merge_metadata_field(mut metadata: Value, key: &str, value: String) -> Value {
    if value.is_empty() {
        return metadata;
    }
    merge_metadata_value(&mut metadata, key, Value::String(value));
    metadata
}

fn merge_metadata_map(mut metadata: Value, fields: &serde_json::Map<String, Value>) -> Value {
    for (key, value) in fields {
        merge_metadata_value(&mut metadata, key, value.clone());
    }
    metadata
}

fn merge_metadata_value(metadata: &mut Value, key: &str, value: Value) {
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(key.to_owned(), value);
    } else if metadata.is_null() {
        *metadata = json!({ key: value });
    } else {
        let old = std::mem::replace(metadata, Value::Null);
        *metadata = json!({ "user": old, key: value });
    }
}

fn assistant_message_from_output_items<'a>(
    output_items: impl Iterator<Item = &'a Value>,
) -> Option<Value> {
    let mut blocks = Vec::new();

    for item in output_items {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                    for part in content {
                        if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    blocks.push(json!({
                                        "type": "text",
                                        "text": text,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            Some("reasoning") => {
                if let Some(block) = item.get("anthropic_thinking").filter(|v| v.is_object()) {
                    blocks.push(block.clone());
                } else if let Some(summary) = item.get("summary").and_then(|v| v.as_array()) {
                    for part in summary {
                        if part.get("type").and_then(|v| v.as_str()) == Some("summary_text") {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": text,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                let mut block = serde_json::Map::new();
                block.insert("type".into(), Value::String("tool_use".into()));
                block.insert(
                    "id".into(),
                    item.get("call_id")
                        .or_else(|| item.get("id"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                block.insert(
                    "name".into(),
                    item.get("name")
                        .cloned()
                        .unwrap_or(Value::String(String::new())),
                );
                block.insert(
                    "input".into(),
                    tool_input_from_response_arguments(item.get("arguments")),
                );
                for key in ["caller", "cache_control"] {
                    if let Some(value) = item.get(key) {
                        block.insert(key.to_owned(), value.clone());
                    }
                }
                blocks.push(Value::Object(block));
            }
            _ => {}
        }
    }

    if blocks.is_empty() {
        return None;
    }

    Some(json!({
        "role": "assistant",
        "content": blocks,
    }))
}

fn tool_input_from_response_arguments(arguments: Option<&Value>) -> Value {
    let Some(arguments) = arguments.and_then(Value::as_str) else {
        return json!({});
    };
    let parsed = serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_owned()));
    if parsed.is_object() {
        parsed
    } else {
        json!({ "input": parsed })
    }
}

fn drain_one_frame(buf: &mut BytesMut) -> Option<Bytes> {
    let (pos, sep_len) = find_frame_boundary(buf)?;
    let frame = buf.split_to(pos).freeze();
    let _ = buf.split_to(sep_len.min(buf.len()));
    Some(frame)
}

fn find_frame_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = if buf.len() >= 4 {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    } else {
        None
    };
    match (lf, crlf) {
        (Some(l), Some(c)) if l <= c => Some((l, 2)),
        (Some(_), Some(c)) => Some((c, 4)),
        (Some(l), None) => Some((l, 2)),
        (None, Some(c)) => Some((c, 4)),
        (None, None) => None,
    }
}

fn parse_sse_frame(frame: &[u8]) -> Option<(String, Value)> {
    let s = std::str::from_utf8(frame).ok()?;
    let mut event_name = String::new();
    let mut data_buf = String::new();
    for line in s.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim_start());
        }
    }
    if data_buf.trim().is_empty() {
        return None;
    }
    let data = serde_json::from_str(&data_buf).ok()?;
    Some((event_name, data))
}
