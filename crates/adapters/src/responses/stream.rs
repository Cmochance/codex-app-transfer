//! 把 `ChatToResponsesConverter` 包成异步字节流转换器.

use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};

use crate::types::ByteStream;

use super::converter::ChatToResponsesConverter;

struct State {
    input: ByteStream,
    conv: ChatToResponsesConverter,
    finished: bool,
}

/// 把上游 OpenAI Chat SSE 流转换为 OpenAI Responses SSE 流.
pub fn convert_chat_to_responses_stream(input: ByteStream) -> ByteStream {
    let init = State {
        input,
        conv: ChatToResponsesConverter::new(),
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
