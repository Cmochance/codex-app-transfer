//! 把 `ChatToResponsesConverter` 包成异步字节流转换器.

use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};

use crate::types::{ByteStream, ResponseSessionPlan};

use super::converter::ChatToResponsesConverter;
use super::session::global_response_session_cache;

struct State {
    input: ByteStream,
    conv: ChatToResponsesConverter,
    response_session: Option<ResponseSessionPlan>,
    finished: bool,
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
                        // 半个 frame:继续读
                    }
                    Some(Err(e)) => {
                        s.finished = true;
                        return Some((Err(e), s));
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
