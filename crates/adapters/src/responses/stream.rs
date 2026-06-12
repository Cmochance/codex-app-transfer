//! 把 `ChatToResponsesConverter` 包成异步字节流转换器.

use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::types::{ByteStream, ResponseSessionPlan};

use super::converter::ChatToResponsesConverter;
use super::session::global_response_session_cache;

struct State {
    input: ByteStream,
    conv: ChatToResponsesConverter,
    response_session: Option<ResponseSessionPlan>,
    finished: bool,
    /// [MOC-219] 上游 Err 时若缓冲里还有未出 wire 的 message,先 yield flush
    /// 字节、错误暂存到这里下一轮再传播 —— 基线(无缓冲)时代部分文本已实时
    /// 流出,错误不应把已生成文本全吞。
    pending_err: Option<std::io::Error>,
}

/// 把上游 OpenAI Chat SSE 流转换为 OpenAI Responses SSE 流.
pub fn convert_chat_to_responses_stream(input: ByteStream) -> ByteStream {
    convert_chat_to_responses_stream_inner(input, ChatToResponsesConverter::new(), None)
}

pub fn convert_chat_to_responses_stream_with_session(
    input: ByteStream,
    response_session: ResponseSessionPlan,
) -> ByteStream {
    let conv = ChatToResponsesConverter::new_with_response_id(response_session.response_id.clone());
    convert_chat_to_responses_stream_inner(input, conv, Some(response_session))
}

/// 同上,但允许调用方按 provider 行为开启 `<think>` 兜底拆分等可选解析。
///
/// `original_responses_request` 是入站 Responses API request 的**完整 body**
/// (未经任何展平 / 协议转换),会被 envelope 在 `response.created` /
/// `response.in_progress` / `response.completed` 三处回灌完整字段集
/// (tools / parallel_tool_calls / tool_choice / reasoning / text / metadata
/// / previous_response_id / instructions / temperature / top_p /
/// max_output_tokens / truncation / created_at)。
///
/// 关键作用是 `tools` 字段:Codex CLI 用 `(namespace, function.name)` 复合
/// 主键反向路由 namespace 包装的 MCP function_call;其余字段保协议合规性。
pub fn convert_chat_to_responses_stream_with_options(
    input: ByteStream,
    response_session: Option<ResponseSessionPlan>,
    enable_think_tag_split: bool,
    original_responses_request: Option<Value>,
) -> ByteStream {
    let conv = match response_session.as_ref() {
        Some(s) => ChatToResponsesConverter::new_with_response_id(s.response_id.clone()),
        None => ChatToResponsesConverter::new(),
    }
    .with_think_tag_split(enable_think_tag_split)
    .with_original_request(original_responses_request);
    convert_chat_to_responses_stream_inner(input, conv, response_session)
}

fn convert_chat_to_responses_stream_inner(
    input: ByteStream,
    conv: ChatToResponsesConverter,
    response_session: Option<ResponseSessionPlan>,
) -> ByteStream {
    let init = State {
        input,
        conv,
        response_session,
        finished: false,
        pending_err: None,
    };
    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(stream::unfold(init, |mut s| async move {
            loop {
                if let Some(e) = s.pending_err.take() {
                    s.finished = true;
                    return Some((Err(e), s));
                }
                if s.finished {
                    return None;
                }
                match s.input.next().await {
                    Some(Ok(chunk)) => {
                        let out = s.conv.feed(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), s));
                        }
                        // 半个 frame:继续读
                    }
                    Some(Err(e)) => {
                        // [MOC-219] 缓冲中的 message 先 flush 出 wire 再传播
                        // 错误(不发 completed,保持断流语义与基线一致的
                        // Codex 重试行为)。
                        let flushed = s.conv.flush_pending_buffer();
                        // fix(#210): 流中断时也保存已累积的 session 历史,避免
                        // 下一轮 `previous_response_id` 续轮时 cache miss →
                        // `previous_response_not_found` → 对话彻底断裂。即使
                        // 本轮 assistant 回复不完整,保留已有部分也好过全丢。
                        save_response_session(&mut s);
                        if flushed.is_empty() {
                            s.finished = true;
                            return Some((Err(e), s));
                        }
                        s.pending_err = Some(e);
                        return Some((Ok(Bytes::from(flushed)), s));
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

fn save_response_session(state: &mut State) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;

    fn input_stream(bytes: &'static [u8]) -> ByteStream {
        Box::pin(stream::iter(vec![Ok(Bytes::from_static(bytes))]))
    }

    /// [MOC-219 / bot P2] 上游 Err:缓冲中的 message 先以完整生命周期 flush 出
    /// wire,错误随后传播(不发 completed);session 同样保存缓冲文本。
    #[tokio::test]
    async fn upstream_error_flushes_buffered_text_before_propagating() {
        global_response_session_cache().clear();
        let session = ResponseSessionPlan {
            response_id: "resp_err_flush_test".to_owned(),
            messages: vec![json!({"role": "user", "content": "hi"})],
        };
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial text\"}}]}\n\n",
            )),
            Err(std::io::Error::other("upstream reset")),
        ];
        let input: ByteStream = Box::pin(stream::iter(chunks));
        let mut converted = convert_chat_to_responses_stream_with_session(input, session);

        let mut collected = Vec::new();
        let mut saw_err = false;
        while let Some(item) = converted.next().await {
            match item {
                Ok(b) => collected.extend_from_slice(&b),
                Err(_) => {
                    saw_err = true;
                    break;
                }
            }
        }
        assert!(saw_err, "错误仍须传播");
        let text = String::from_utf8(collected).unwrap();
        assert!(
            text.contains("\"delta\":\"partial text\""),
            "缓冲文本应在错误前 flush 出 wire;实际输出: {text}"
        );
        assert!(
            !text.contains("response.completed"),
            "不发 completed,保持断流语义"
        );
        // session 保存含缓冲文本(#210)
        let saved = global_response_session_cache()
            .get("resp_err_flush_test")
            .unwrap();
        assert_eq!(saved[1]["content"], "partial text");
    }

    #[tokio::test]
    async fn stream_completion_saves_request_and_assistant_messages() {
        global_response_session_cache().clear();
        let session = ResponseSessionPlan {
            response_id: "resp_session_test".to_owned(),
            messages: vec![json!({"role": "user", "content": "hello"})],
        };
        let raw = br#"data: {"id":"chatcmpl_1","model":"gpt-test","choices":[{"delta":{"content":"hi"},"finish_reason":null}]}

data: {"id":"chatcmpl_1","model":"gpt-test","choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;
        let mut converted =
            convert_chat_to_responses_stream_with_session(input_stream(raw), session);
        while let Some(chunk) = converted.next().await {
            let _ = chunk.unwrap();
        }

        let saved = global_response_session_cache()
            .get("resp_session_test")
            .unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["role"], "user");
        assert_eq!(saved[1]["role"], "assistant");
        assert_eq!(saved[1]["content"], "hi");
    }
}
