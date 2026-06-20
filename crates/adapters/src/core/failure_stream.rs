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

/// 从上游错误 body(可能是 JSON)提取人类可读的 message。优先 `error.message`,
/// 退而求其次顶层 `message`;都没有(或非 JSON / 空)返回 None。
fn extract_upstream_error_message(body_text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body_text.trim()).ok()?;
    let msg = v
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())?;
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// body 是否带「配额/额度耗尽」(计费/使用窗口用满,在 reset 前 immediate retry
/// 必同样失败)的强信号 —— 区别于瞬时 per-minute 限流 / 并发上限(退避后能成功)。
/// 调用方只对 429 调用本函数。
///
/// **保守匹配**:只认无歧义的「计费/使用窗口耗尽」信号,不认裸 `rate limit` /
/// `too many requests` / `并发` 等瞬时态 —— 把瞬时误判成永久会误杀重试(见 lib.rs
/// `codex_retry_code` doc 的 MOC-79 教训);宁可漏判(继续 Retryable)也不误判。
/// 触发样本:GLM Coding `code 1308`「已达到 N 小时的使用上限。您的限额将在 … 重置。」。
///
/// 瞬时 vs 耗尽的判据分两类标记,**别混用**(MOC-264 bot review 五轮收敛):
/// - **THROTTLE 专属**(无歧义瞬时,退避后能成功):per-minute / RPM / per-second /
///   每分钟 / 每秒 / 过于频繁 / 并发。命中即**一律保持可重试**(即便措辞含"使用上限")。
/// - **耗尽专属**(永久窗口耗尽):reset / window / quota / credit / balance / 使用上限。
/// - **模糊的 `retry after` / `try again in` / 稍后重试**:**不作判据** —— 因为既可能是
///   30s 瞬时,也可能是「resets in 5 小时;try again in 5 小时」的窗口耗尽。把它当瞬时
///   会把窗口耗尽误判可重试、重新陷入重连循环(bot 第 5 轮反例)。改由耗尽专属标记决定。
///
/// 英文裸 `usage limit` 仍额外要求伴随耗尽专属标记(reset/quota/credit/balance/upgrade/
/// daily/weekly),因其单独歧义大;中文「使用上限」「余额不足」等本身已无歧义。
fn body_has_usage_limit_signal(body_text: &str) -> bool {
    // 关键词均 ASCII;中文 .contains 直接走原文。
    let lower = body_text.to_ascii_lowercase();
    // 仅 THROTTLE 专属标记算无歧义瞬时;命中即可重试,不再看耗尽词。
    let throttle_specific = lower.contains("per minute")
        || lower.contains("per-minute")
        || lower.contains("per second")
        || lower.contains("rpm")
        || body_text.contains("每分钟")
        || body_text.contains("每秒")
        || body_text.contains("过于频繁")
        || body_text.contains("并发");
    if throttle_specific {
        return false;
    }
    let usage_limit_is_exhaustion = lower.contains("usage limit")
        && (lower.contains("reset")
            || lower.contains("quota")
            || lower.contains("credit")
            || lower.contains("balance")
            || lower.contains("upgrade")
            || lower.contains("daily")
            || lower.contains("weekly"));
    body_text.contains("使用上限")   // GLM 计费窗口上限
        || body_text.contains("余额不足")
        || body_text.contains("额度不足")
        || body_text.contains("额度已用")
        || body_text.contains("配额已用")
        || lower.contains("insufficient_quota")       // OpenAI 计费耗尽(非 per-minute)
        || lower.contains("insufficient balance")
        || lower.contains("out of credits")
        || lower.contains("exceeded your current quota")
        || usage_limit_is_exhaustion
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
/// **429 配额耗尽特判(对所有调用方生效)**:本函数被 chat(`mapper/chat.rs`)、
/// grok_web(`grok_web/response.rs`)、native-responses passthrough
/// (`mapper/responses.rs` 流式分支)共用。对 429 用 [`body_has_usage_limit_signal`]
/// 做保守强信号检测,命中则覆盖 `upstream_kind` 为 `usage_limit_reached`
/// (→ `codex_retry_code` 映射成非重试 `invalid_prompt`,Codex 原样 surface 上游
/// message + 停,不再 Reconnecting);message 用上游结构化 `error.message`(含 reset
/// 时间),提取不到且 body 残破(截断/非 UTF-8)时退固定兜底文案,不 dump raw body。
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
                let (final_kind, message) = if let Some(transport) = &collected.transport_err {
                    (
                        "upstream_transport_error",
                        format!(
                            "{msg_prefix} HTTP {status_u16} but transport err during body read: {transport}"
                        ),
                    )
                } else if collected.text.is_empty() {
                    (
                        upstream_kind,
                        format!("{msg_prefix} HTTP {status_u16} (empty body)"),
                    )
                } else if status_u16 == 429 && body_has_usage_limit_signal(&collected.text) {
                    // 配额/额度耗尽(计费窗口用满,immediate retry 在重置前必同样失败):
                    // 归 `usage_limit_reached` → `codex_retry_code` 映射成非重试
                    // `invalid_prompt`,Codex 原样 surface 上游 message + 停,不再
                    // Reconnecting 5/5(重连还会触发空体探活报错)。message 优先上游
                    // 结构化 `error.message`(含 reset 时间);提取不到且 body 残破
                    // (截断/非 UTF-8)时给固定干净兜底,**不** dump 带内部后缀的 raw
                    // body。区别于瞬时 per-minute 限流(不命中信号,仍走
                    // upstream_kind→Retryable)。
                    let clean = extract_upstream_error_message(&collected.text).unwrap_or_else(|| {
                        if collected.truncated || collected.lossy {
                            "已达到使用上限 / usage limit reached(上游配额错误详情已省略,请在限额重置后重试)".to_owned()
                        } else {
                            collected.text.trim().to_owned()
                        }
                    });
                    ("usage_limit_reached", clean)
                } else {
                    // 非配额错误:保留诊断 message(上游前缀 + HTTP status + body,带
                    // truncated/lossy 后缀利于排障)。
                    let mut body_text = collected.text.clone();
                    if collected.truncated {
                        body_text.push_str(" …(truncated)");
                    }
                    if collected.lossy {
                        body_text.push_str(" (non-UTF-8 body)");
                    }
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

    async fn drive_error_stream(
        status: u16,
        body: &'static str,
        kind: &'static str,
        prefix: &'static str,
    ) -> String {
        let upstream = mock_stream(vec![Ok(Bytes::from_static(body.as_bytes()))]);
        let mut s = convert_upstream_error_stream(
            http::StatusCode::from_u16(status).unwrap(),
            upstream,
            "resp_test".to_owned(),
            kind,
            prefix,
        );
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn usage_limit_429_glm_fail_fast_with_clean_message() {
        // GLM Coding code 1308:计费窗口用满。应识别为 usage_limit_reached →
        // 非重试 invalid_prompt(Codex 原样 surface message),message 用上游干净
        // error.message(含 reset 时间),不裹 "upstream HTTP 429:" / 不 dump JSON body。
        let body = r#"{"error":{"code":"1308","message":"已达到 5 小时的使用上限。您的限额将在 2026-06-20 18:50:41 重置。"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains("已达到 5 小时的使用上限"));
        assert!(s.contains("将在 2026-06-20 18:50:41 重置"));
        assert!(!s.contains("upstream HTTP 429"));
        assert!(!s.contains(r#""code":"1308""#));
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains(r#""upstream_error_kind":"usage_limit_reached""#));
    }

    #[tokio::test]
    async fn transient_429_rate_limit_stays_retryable() {
        // 瞬时 per-minute 限流:无配额耗尽强信号 → 不触发 usage_limit_reached,
        // 保留调用方 kind(rate_limited)→ Codex Retryable,走原 "upstream HTTP" 格式。
        let body = r#"{"error":{"message":"Rate limit exceeded, please retry later","type":"rate_limit_error"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"rate_limited""#));
        assert!(s.contains("upstream HTTP 429"));
        assert!(!s.contains("usage_limit_reached"));
        assert!(!s.contains("invalid_prompt"));
    }

    #[tokio::test]
    async fn usage_limit_detection_gated_on_429_status() {
        // 非 429 即便 body 含"使用上限"也不触发(配额耗尽语义只在 429 成立)。
        let body = r#"{"error":{"message":"服务异常,触及使用上限相关逻辑"}}"#;
        let s = drive_error_stream(500, body, "server_error", "upstream").await;
        assert!(s.contains(r#""code":"server_error""#));
        assert!(!s.contains("usage_limit_reached"));
    }

    #[tokio::test]
    async fn usage_limit_429_english_insufficient_quota() {
        // OpenAI 计费耗尽标准 code insufficient_quota(非 per-minute)→ fail-fast。
        let body = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains(r#""upstream_error_kind":"usage_limit_reached""#));
        assert!(s.contains("exceeded your current quota"));
    }

    #[tokio::test]
    async fn usage_limit_429_per_minute_phrasing_stays_retryable() {
        // 瞬时 per-minute 限流即便措辞含 "usage limit" 也保持可重试(MOC-264 bot P2):
        // 不命中耗尽信号 → rate_limited → Retryable,不误杀退避。
        let body = r#"{"error":{"message":"Usage limit exceeded: 60 requests per minute, retry after 30s","type":"rate_limit"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"rate_limited""#));
        assert!(!s.contains("usage_limit_reached"));
        assert!(!s.contains("invalid_prompt"));
    }

    #[tokio::test]
    async fn usage_limit_429_retry_after_and_rpm_stay_retryable() {
        // MOC-264 bot P2 二轮:泛词 reached/exceeded 不足以判永久。带 retry-after /
        // RPM 的瞬时消息(无 reset/quota/credit/balance 耗尽专属标记)保持可重试。
        for body in [
            r#"{"error":{"message":"Usage limit reached, retry after 30 seconds"}}"#,
            r#"{"error":{"message":"Usage limit exceeded: 60 RPM"}}"#,
        ] {
            let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
            assert!(s.contains(r#""code":"rate_limited""#), "body={body}");
            assert!(!s.contains("usage_limit_reached"), "body={body}");
            assert!(!s.contains("invalid_prompt"), "body={body}");
        }
    }

    #[tokio::test]
    async fn usage_limit_429_chinese_per_minute_stays_retryable() {
        // MOC-264 bot P2 三轮:中文「每分钟使用上限,请稍后重试」是瞬时 per-minute 限流。
        // throttle_specific 守卫(每分钟)对中文「使用上限」信号同样生效 → 可重试。
        let body = r#"{"error":{"code":"1302","message":"已达到每分钟使用上限,请稍后重试"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"rate_limited""#));
        assert!(!s.contains("usage_limit_reached"));
        assert!(!s.contains("invalid_prompt"));
    }

    #[tokio::test]
    async fn usage_limit_429_reset_window_with_try_again_fail_fast() {
        // MOC-264 bot P2 五轮:模糊的 "try again in 5 hours" 不作瞬时判据——这是重置窗口
        // 耗尽(带 resets),不是 per-minute throttle。reset 耗尽标记 → fail-fast,不重连。
        let body = r#"{"error":{"message":"You've reached your usage limit; resets in 5 hours; try again in 5 hours"}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains(r#""upstream_error_kind":"usage_limit_reached""#));
    }

    #[tokio::test]
    async fn usage_limit_429_reached_with_marker_fail_fast() {
        // "usage limit" 伴随耗尽/重置语义(非 per-minute)→ 永久耗尽 fail-fast。
        let body =
            r#"{"error":{"message":"You've reached your usage limit. Resets at 2026-06-21."}}"#;
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains(r#""upstream_error_kind":"usage_limit_reached""#));
    }

    #[tokio::test]
    async fn usage_limit_429_truncated_body_uses_clean_fallback_not_raw_dump() {
        // 命中配额信号(使用上限)但 body 超 cap 截断 → JSON 损坏、提取失败。
        // 应给固定干净兜底,**不** dump 带 " …(truncated)" 后缀的残破 raw body。
        let mut head = String::from(r#"{"error":{"message":"已达到使用上限 "#);
        head.push_str(&"垃圾填充".repeat(20000)); // 远超 64KB cap,强制截断
        let upstream = mock_stream(vec![Ok(Bytes::from(head.into_bytes()))]);
        let mut stream = convert_upstream_error_stream(
            http::StatusCode::from_u16(429).unwrap(),
            upstream,
            "resp_test".to_owned(),
            "rate_limited",
            "upstream",
        );
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        let s = String::from_utf8(out).unwrap();
        // fail-fast 仍生效(命中信号)
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains(r#""upstream_error_kind":"usage_limit_reached""#));
        // 但不泄露内部后缀 / 残破 body,走固定兜底文案
        assert!(s.contains("usage limit reached"));
        assert!(!s.contains("truncated"));
        assert!(!s.contains("垃圾填充"));
    }

    #[tokio::test]
    async fn usage_limit_429_plain_text_body_shown_as_is() {
        // 命中信号但 body 是非 JSON 纯文本(短、未截断)→ 直接展示原文(已干净)。
        let body = "余额不足,请充值后重试";
        let s = drive_error_stream(429, body, "rate_limited", "upstream").await;
        assert!(s.contains(r#""code":"invalid_prompt""#));
        assert!(s.contains("余额不足,请充值后重试"));
        assert!(!s.contains("upstream HTTP 429"));
    }

    #[test]
    fn extract_message_nested_flat_and_invalid() {
        assert_eq!(
            extract_upstream_error_message(r#"{"error":{"message":"hi"}}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(
            extract_upstream_error_message(r#"{"message":"flat"}"#).as_deref(),
            Some("flat")
        );
        assert_eq!(extract_upstream_error_message("not json"), None);
        assert_eq!(
            extract_upstream_error_message(r#"{"error":{"message":"   "}}"#),
            None
        );
    }
}
