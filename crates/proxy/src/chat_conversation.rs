//! [MOC-323] Codex/ChatGPT 桌面 app 的 **Chat(经典 ChatGPT 对话)** 接入 transfer 自定义模型。
//!
//! 背景(实测,详见 Linear MOC-322):26.707 起 app 新增 Chat = 嵌入的经典 ChatGPT 对话,
//! renderer 直发 `POST {CODEX_API_BASE_URL}/f/conversation`(SSE),body 是 classic ChatGPT
//! 封套 `{action, messages:[{author,content}], model, parent_message_id}`。base 由环境变量
//! `CODEX_API_BASE_URL` 决定(不读 config.toml),transfer 把它指向本 proxy 后,整个
//! `/backend-api/*` 流进 [`crate::forward::forward_handler`](fallback)。
//!
//! 本模块**只拦 2 条**、其余仍走既有 `passthrough_chatgpt_backend`(透传真 chatgpt.com,
//! 账号/会话列表/plugins 正常):
//! - `POST …/f/conversation` → 把 ChatGPT 封套转成 `/responses` body,**内部重派**给
//!   `forward_handler`(复用全套 provider resolve / adapter / 鉴权改写),收集 Responses SSE
//!   的文本,再回一条 ChatGPT 整条-message SSE(客户端 `decodeNonDeltaEvent` 对不带
//!   `event: delta_encoding` 的事件直接渲染 `e.data`,故免写 delta 增量协议)。
//! - `…/f/conversation/prepare`、`/models` 等 → 返回 `None`,交回调用方 passthrough。
//!
//! P1(本次)= 单轮、收集后一次性回;流式增量 / 自定义 `/models` 列表 / 多轮上下文拼接
//! 留 P2(先真机实测 Chat 怎么发上下文再定,见 MOC-323)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::forward::{forward_handler, ProxyState};

/// gate:默认开。src-tauri 按前端开关设 `CAS_CHAT_CUSTOM_MODEL`(未设=开;显式 `0`/`false`=关)。
pub fn chat_custom_model_enabled() -> bool {
    !matches!(
        std::env::var("CAS_CHAT_CUSTOM_MODEL").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    )
}

/// 是否是要拦的对话流路径(精确 `…/f/conversation`,**不含** `/prepare`、`/resume`)。
fn is_conversation_stream_path(path: &str) -> bool {
    let p = path.split('?').next().unwrap_or(path);
    p.ends_with("/f/conversation")
}

/// 入口:返回 `Some(resp)` = 已接管;`None` = 不拦,调用方继续原 passthrough。
pub async fn try_handle(
    state: &ProxyState,
    method: &Method,
    headers: &HeaderMap,
    path: &str,
    body: &[u8],
) -> Option<Response> {
    if !chat_custom_model_enabled() {
        return None;
    }
    if method == Method::POST && is_conversation_stream_path(path) {
        return Some(handle_conversation(state, headers, body).await);
    }
    None
}

async fn handle_conversation(state: &ProxyState, headers: &HeaderMap, body: &[u8]) -> Response {
    let chat: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return sse_reply(&format!("（transfer）无法解析 chat 请求: {e}"), true),
    };
    let responses_body = match build_responses_body(&chat) {
        Some(b) => b,
        None => return sse_reply("（transfer）本轮无用户消息，已忽略。", true),
    };

    let req = build_responses_request(headers, &responses_body);
    // Box::pin 打破 forward_handler → try_handle → forward_handler 的 async 递归(否则
    // future 尺寸无限、E0733)。本模块只在 `/f/conversation` 命中,重派的是 `/responses`,
    // 不会再次进本拦截分支,递归深度恒为 1。
    let upstream = match Box::pin(forward_handler(axum::extract::State(state.clone()), req)).await {
        Ok(resp) => resp,
        Err(e) => return sse_reply(&format!("（transfer）上游调用失败: {e}"), true),
    };

    let status = upstream.status();
    let bytes = match axum::body::to_bytes(upstream.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return sse_reply(&format!("（transfer）读取上游响应失败: {e}"), true),
    };
    let text = extract_assistant_text(&bytes);
    if text.is_empty() {
        let hint = if status.is_success() {
            "（transfer）上游未返回文本内容。".to_string()
        } else {
            format!("（transfer）上游返回 {status}。")
        };
        return sse_reply(&hint, true);
    }
    sse_reply(&text, false)
}

/// ChatGPT 封套 → `/responses` body。取历史里最后一条 user 消息的文本(P1 单轮)。
fn build_responses_body(chat: &serde_json::Value) -> Option<serde_json::Value> {
    let messages = chat.get("messages")?.as_array()?;
    let user_text = messages
        .iter()
        .rev()
        .find(|m| {
            m.get("author")
                .and_then(|a| a.get("role"))
                .and_then(|r| r.as_str())
                == Some("user")
        })
        .map(message_text)
        .filter(|t| !t.trim().is_empty())?;

    let model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty() && *m != "auto")
        .unwrap_or("gpt-5.5");

    Some(serde_json::json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": user_text }],
        }],
        "stream": true,
    }))
}

/// 从一条 ChatGPT message 提取纯文本(`content.parts[]` 字符串拼接;兼容 `content` 直接是串)。
fn message_text(m: &serde_json::Value) -> String {
    let content = m.get("content");
    if let Some(parts) = content
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        return parts
            .iter()
            .filter_map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("");
    }
    content
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// 复用 `websocket_forward_request` 同法:合成 `POST /responses`,只带 Authorization。
fn build_responses_request(headers: &HeaderMap, body: &serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder().method(Method::POST).uri("/responses");
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        builder = builder.header(axum::http::header::AUTHORIZATION, auth);
    }
    builder
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap_or_default()))
        .expect("build /responses request")
}

/// 从 Responses SSE 字节里累加 assistant 文本(宽松解析:任一 `data:` JSON 若是
/// `response.output_text.delta` 且带字符串 `delta` 就累加;兜底读 `output_text.done` 的
/// 全文)。不依赖具体 event 行,兼容带/不带 `event:` 前缀两种形态。
fn extract_assistant_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut acc = String::new();
    let mut done_full: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_start();
        let json_str = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if json_str.is_empty() || json_str == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response.output_text.delta") => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    acc.push_str(d);
                }
            }
            Some("response.output_text.done") => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    done_full = Some(t.to_owned());
                }
            }
            _ => {}
        }
    }
    if !acc.is_empty() {
        acc
    } else {
        done_full.unwrap_or_default()
    }
}

/// 回一条 ChatGPT 整条-message SSE(非 delta):in_progress → finished + `[DONE]`。
fn sse_reply(text: &str, is_error: bool) -> Response {
    let conv_id = gen_id("conv");
    let msg_id = gen_id("msg");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let frame = |parts: &str, status: &str, end: bool| {
        serde_json::json!({
            "message": {
                "id": msg_id,
                "author": { "role": "assistant" },
                "create_time": now,
                "content": { "content_type": "text", "parts": [parts] },
                "status": status,
                "end_turn": end,
                "metadata": if end { serde_json::json!({"finish_details": {"type": "stop"}}) } else { serde_json::json!({}) },
            },
            "conversation_id": conv_id,
            "error": if is_error { serde_json::json!("transfer_local_error") } else { serde_json::Value::Null },
        })
        .to_string()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        frame(text, "in_progress", false),
        frame(text, "finished_successfully", true),
    );
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// 进程内唯一 id(免 uuid 依赖):`transfer-<prefix>-<nanos>-<seq>`。
fn gen_id(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("transfer-{prefix}-{nanos}-{seq}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_conversation_stream_path_precise() {
        assert!(is_conversation_stream_path("/backend-api/f/conversation"));
        assert!(is_conversation_stream_path(
            "/backend-api/f/conversation?x=1"
        ));
        assert!(!is_conversation_stream_path(
            "/backend-api/f/conversation/prepare"
        ));
        assert!(!is_conversation_stream_path(
            "/backend-api/f/conversation/resume"
        ));
        assert!(!is_conversation_stream_path("/backend-api/models"));
    }

    #[test]
    fn build_responses_body_takes_last_user_text() {
        let chat = serde_json::json!({
            "model": "auto",
            "messages": [
                {"author": {"role": "assistant"}, "content": {"parts": ["hi"]}},
                {"author": {"role": "user"}, "content": {"content_type": "text", "parts": ["你好", "世界"]}},
            ],
        });
        let body = build_responses_body(&chat).unwrap();
        assert_eq!(body["model"], "gpt-5.5"); // auto → 默认槽,交 resolver 映射
        assert_eq!(body["input"][0]["content"][0]["text"], "你好世界");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn build_responses_body_none_without_user() {
        let chat = serde_json::json!({"messages": [{"author": {"role": "assistant"}, "content": {"parts": ["hi"]}}]});
        assert!(build_responses_body(&chat).is_none());
    }

    #[test]
    fn extract_text_accumulates_deltas() {
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n\
                   data: {\"type\":\"response.completed\"}\n\n";
        assert_eq!(extract_assistant_text(sse.as_bytes()), "Hello world");
    }

    #[test]
    fn extract_text_falls_back_to_done_full() {
        let sse = "data: {\"type\":\"response.output_text.done\",\"text\":\"full text\"}\n\n";
        assert_eq!(extract_assistant_text(sse.as_bytes()), "full text");
    }

    #[test]
    fn gate_default_on() {
        // 未显式关时默认开(不设 env)
        std::env::remove_var("CAS_CHAT_CUSTOM_MODEL");
        assert!(chat_custom_model_enabled());
    }
}
