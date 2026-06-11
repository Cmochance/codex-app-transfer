//! 上游非 2xx → 合规 Responses 失败流的共享骨架(MOC-118)。
//!
//! chat(`mapper/chat.rs`)/ grok(`grok_web/response.rs`)/ gemini
//! (`gemini_native/response.rs`)三处「上游错误 → `response.created` +
//! `response.failed` SSE」转换器曾各自复制同一套 body 收集 + 防御逻辑
//! (MOC-103 / MOC-90 / MOC-79),本模块把协议无关的骨架收编到 core:
//!
//! - [`collect_upstream_error_body`]:错误 body 收集 + cap/lossy/truncate/
//!   transport-err 防御(chat / grok / gemini 三处复用);
//! - [`convert_upstream_error_stream`]:完整「非 2xx → 双帧失败流」整流
//!   (chat / grok 整体收编;gemini 因 classify 特化只复用收集层);
//! - [`emit_response_created_frame`] / [`emit_response_failed_frame`]:
//!   两种事件帧的单源构造(失败帧另被 grok mid-stream 防御与
//!   `responses/compact.rs` 的 compact v2 失败尾复用)。
//!
//! 各 adapter 的差异点(HTTP status → 语义 kind 的 classify、message 前缀、
//! gemini 的 JSON message 探测)留在各自 mapper 层,不进 core。

use bytes::Bytes;
use futures_util::stream::{self, Stream, StreamExt};
use serde_json::json;
use std::pin::Pin;

use crate::core::events::emit_sse_event;
use crate::types::ByteStream;

/// 上游错误 body 最大读取字节数。上游错误 body 通常 <1KB;CDN HTML 错误页 /
/// proxy 异常体可能数 MB,无 cap → 失败请求并发时内存放大攻击面。截断后剩余
/// bytes 直接 drop(上游已经表态错误,不需要 forward 完整 body,只需要 error
/// message 给用户)。
pub(crate) const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 64 * 1024;

/// [`collect_upstream_error_body`] 的收集结果。`text` 是 lossy 转换后的原文,
/// **不带**任何 truncated / non-UTF-8 后缀 —— 后缀格式各 adapter 不同
/// (chat/grok 拼 ` …(truncated)`,gemini 拼 ` [body truncated]`),由调用方
/// 按需拼接;gemini 还要先拿原文做 JSON parse,不能被后缀污染。
pub(crate) struct CollectedErrorBody {
    pub text: String,
    pub transport_err: Option<String>,
    pub truncated: bool,
    pub lossy: bool,
}

/// 收集上游错误 body(truncate-and-continue 语义)。
///
/// **防御**:
/// - body cap `cap` 字节防 DoS,超限截断但**继续** emit(错误路径尽量带上
///   已收到的诊断信息,不因 body 过大整体失败 —— 区别于 compact v2 成功
///   路径的 oversize-即-报错语义,后者不适用本 helper);
/// - 非 UTF-8 用 `from_utf8_lossy`,`lossy` 标记返回;
/// - mid-read transport `Err` → 中断收集,err 文本进 `transport_err`
///   (调用方应覆盖语义分类为 `upstream_transport_error`:body 不完整,
///   从中提取的 message 不可信)。
pub(crate) async fn collect_upstream_error_body(
    input: &mut ByteStream,
    cap: usize,
) -> CollectedErrorBody {
    let mut body = Vec::with_capacity(1024);
    let mut transport_err: Option<String> = None;
    let mut truncated = false;
    while let Some(chunk) = input.next().await {
        match chunk {
            Ok(b) => {
                let remaining = cap.saturating_sub(body.len());
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let take = b.len().min(remaining);
                body.extend_from_slice(&b[..take]);
                if take < b.len() {
                    truncated = true;
                    break;
                }
            }
            Err(e) => {
                transport_err = Some(e.to_string());
                break;
            }
        }
    }
    let lossy = std::str::from_utf8(&body).is_err();
    let text = String::from_utf8_lossy(&body).into_owned();
    CollectedErrorBody {
        text,
        transport_err,
        truncated,
        lossy,
    }
}

/// 构造 `response.created`(in_progress)事件帧,写入 `out`。
pub(crate) fn emit_response_created_frame(out: &mut Vec<u8>, seq: &mut u64, response_id: &str) {
    emit_sse_event(
        out,
        seq,
        "response.created",
        json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
            }
        }),
    );
}

/// 构造 `response.failed` 事件帧,写入 `out`。
///
/// `code` 收**已映射**的 Codex retry-control code:Codex 只按 `error.code`
/// 字符串决定是否重试,不认识的 code 一律落 Retryable → 卡死重发到
/// max_retries(MOC-79 实证)。chat / grok 调用方传
/// `crate::codex_retry_code(kind)`;compact v2 传预映射 code(quality 类
/// kind 不能走通用映射,见 `collect_compact_summary_for_v2` doc)。
/// `upstream_kind` 是内部语义分类,保留在 `error.upstream_error_kind` 诊断
/// 字段(Codex `Error` struct 无 `deny_unknown_fields`,该字段被安全忽略)。
pub(crate) fn emit_response_failed_frame(
    out: &mut Vec<u8>,
    seq: &mut u64,
    response_id: &str,
    code: &str,
    upstream_kind: &str,
    message: &str,
) {
    emit_sse_event(
        out,
        seq,
        "response.failed",
        json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "failed",
                "error": {
                    "code": code,
                    "message": message,
                    "upstream_error_kind": upstream_kind,
                }
            }
        }),
    );
}

/// 上游非 2xx → 合规 Responses 失败流(`response.created` + `response.failed`
/// 双帧,HTTP status 由调用方写成 200)。
///
/// `upstream_kind` 是调用方按自家 classify 算好的语义分类(chat:
/// `classify_chat_error_status`;grok:`classify_grok_error_status`),经
/// [`crate::codex_retry_code`] 映射:永久错误(400/401/403)→ `invalid_prompt`
/// (surface + 停),瞬时态(timeout/rate_limited/server_error 等)保留原 code
/// → Codex Retryable。原始分类存 `error.upstream_error_kind` 诊断字段。
/// `msg_prefix` 是 message 的上游标识前缀(chat: `upstream` / grok:
/// `grok.com`),拼成 `{msg_prefix} HTTP {status}: {body}`。
///
/// 防御骨架见 [`collect_upstream_error_body`];空 body / 截断仍 emit
/// `response.failed`,带通用 message。
pub(crate) fn convert_upstream_error_stream(
    upstream_status: http::StatusCode,
    upstream_stream: ByteStream,
    response_id: String,
    upstream_kind: &'static str,
    msg_prefix: &'static str,
) -> ByteStream {
    let status_u16 = upstream_status.as_u16();

    let s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(
        stream::unfold((upstream_stream, false), move |(mut input, finished)| {
            let response_id = response_id.clone();
            async move {
                if finished {
                    return None;
                }
                let collected =
                    collect_upstream_error_body(&mut input, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
                let mut body_text = collected.text;
                if collected.truncated {
                    body_text.push_str(" …(truncated)");
                }
                if collected.lossy {
                    body_text.push_str(" (non-UTF-8 body)");
                }
                let (final_kind, message) = if let Some(transport) = collected.transport_err {
                    (
                        "upstream_transport_error",
                        format!(
                            "{msg_prefix} HTTP {status_u16} but transport err during body read: {transport}"
                        ),
                    )
                } else if body_text.is_empty() {
                    (
                        upstream_kind,
                        format!("{msg_prefix} HTTP {status_u16} (empty body)"),
                    )
                } else {
                    (
                        upstream_kind,
                        format!("{msg_prefix} HTTP {status_u16}: {body_text}"),
                    )
                };

                // 两个事件拼一起 yield(避免 mock stream 单 chunk 截断 SSE 帧)。
                // 短路错误路径无转换器 state,起 local seq 计数器(从 0)。
                let mut seq: u64 = 0;
                let mut buf = Vec::with_capacity(512);
                emit_response_created_frame(&mut buf, &mut seq, &response_id);
                emit_response_failed_frame(
                    &mut buf,
                    &mut seq,
                    &response_id,
                    crate::codex_retry_code(final_kind),
                    final_kind,
                    &message,
                );
                Some((Ok(Bytes::from(buf)), (input, true)))
            }
        }),
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_stream(chunks: Vec<Result<Bytes, std::io::Error>>) -> ByteStream {
        Box::pin(stream::iter(chunks))
    }

    #[tokio::test]
    async fn collect_small_utf8_body() {
        let mut s = mock_stream(vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"world")),
        ]);
        let c = collect_upstream_error_body(&mut s, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
        assert_eq!(c.text, "hello world");
        assert!(c.transport_err.is_none());
        assert!(!c.truncated);
        assert!(!c.lossy);
    }

    #[tokio::test]
    async fn collect_caps_oversize_body_and_marks_truncated() {
        let big = vec![b'x'; 100];
        let mut s = mock_stream(vec![Ok(Bytes::from(big))]);
        let c = collect_upstream_error_body(&mut s, 10).await;
        assert_eq!(c.text.len(), 10);
        assert!(c.truncated);
        assert!(c.transport_err.is_none());
    }

    #[tokio::test]
    async fn collect_marks_non_utf8_as_lossy() {
        let mut s = mock_stream(vec![Ok(Bytes::from_static(&[0xff, 0xfe, b'a']))]);
        let c = collect_upstream_error_body(&mut s, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
        assert!(c.lossy);
        assert!(c.text.contains('a'));
    }

    #[tokio::test]
    async fn collect_records_transport_err_and_stops() {
        let mut s = mock_stream(vec![
            Ok(Bytes::from_static(b"partial")),
            Err(std::io::Error::new(std::io::ErrorKind::Other, "conn reset")),
            Ok(Bytes::from_static(b"never read")),
        ]);
        let c = collect_upstream_error_body(&mut s, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
        assert_eq!(c.text, "partial");
        assert!(c.transport_err.as_deref().unwrap().contains("conn reset"));
    }

    #[tokio::test]
    async fn failed_frame_uses_premapped_code_verbatim() {
        // compact v2 传预映射 code(如 rate_limit_exceeded),不得被二次映射
        let mut out = Vec::new();
        let mut seq = 1u64;
        emit_response_failed_frame(
            &mut out,
            &mut seq,
            "resp_x",
            "rate_limit_exceeded",
            "http_429",
            "too many",
        );
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""code":"rate_limit_exceeded""#));
        assert!(s.contains(r#""upstream_error_kind":"http_429""#));
        assert!(s.contains(r#""sequence_number":1"#));
        assert_eq!(seq, 2);
    }
}
