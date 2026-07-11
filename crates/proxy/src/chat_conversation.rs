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
//! **多轮上下文**:实测 Chat 每轮只发新消息(`messages.len=1`,classic ChatGPT 靠
//! conversation_id 服务端存历史),transfer 拦截后无状态,故本模块按 conv_id 自持历史
//! ([`ConvStore`]),每轮发全历史给 provider,并回填同一 conv_id 供 app 下轮续接。
//! 收集后一次性回(非流式增量,留后续)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::forward::{forward_handler, ProxyState};

/// 单对话保留的最近消息条数(user+assistant),超出截最新。
const MAX_TURNS_PER_CONV: usize = 40;
/// 内存里保留的对话数上限,超出淘汰最久未用(纯本地个人用,防无界增长)。
const MAX_CONVS: usize = 64;

/// [MOC-323 对话连续] Chat 每轮**只发新消息**(实测 messages.len=1;classic ChatGPT 靠
/// conversation_id 服务端存历史),transfer 拦截后无状态,故自持每 conv_id 的历史,支持多轮。
/// app 会把上一轮响应里的 conversation_id 原样回传,据此续接。
static CONV_STORE: LazyLock<Mutex<ConvStore>> = LazyLock::new(|| Mutex::new(ConvStore::default()));

#[derive(Default)]
struct ConvStore {
    map: HashMap<String, Vec<(String, String)>>, // conv_id → [(role, text)]
    order: Vec<String>,                          // LRU:末尾最近用
}

impl ConvStore {
    fn get(&self, id: &str) -> Vec<(String, String)> {
        self.map.get(id).cloned().unwrap_or_default()
    }
    fn put(&mut self, id: String, mut history: Vec<(String, String)>) {
        if history.len() > MAX_TURNS_PER_CONV {
            history.drain(0..history.len() - MAX_TURNS_PER_CONV);
        }
        self.order.retain(|x| x != &id);
        self.order.push(id.clone());
        self.map.insert(id, history);
        while self.order.len() > MAX_CONVS {
            let oldest = self.order.remove(0);
            self.map.remove(&oldest);
        }
    }
}

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
        Err(e) => return sse_reply("", &format!("（transfer）无法解析 chat 请求: {e}"), true),
    };
    // conv_id:复用 app 回传的(上一轮我们发的),缺省新建。据此续接多轮历史。
    let conv_id = chat
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| gen_id("conv"));

    // 本轮新用户消息(app 每轮只发新消息)。
    let user_text = match latest_user_text(&chat) {
        Some(t) => t,
        None => return sse_reply(&conv_id, "（transfer）本轮无用户消息，已忽略。", true),
    };

    // 载历史 + append 新用户消息 → 发**全历史**给 provider(多轮上下文)。
    let mut history = CONV_STORE.lock().unwrap().get(&conv_id);
    history.push(("user".to_owned(), user_text));
    let model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty() && *m != "auto")
        .unwrap_or("gpt-5.5");
    let responses_body = build_responses_from_history(&history, model);

    let req = build_responses_request(headers, &responses_body);
    // Box::pin 打破 forward_handler → try_handle → forward_handler 的 async 递归(否则
    // future 尺寸无限、E0733)。本模块只在 `/f/conversation` 命中,重派的是 `/responses`,
    // 不会再次进本拦截分支,递归深度恒为 1。
    let upstream = match Box::pin(forward_handler(axum::extract::State(state.clone()), req)).await {
        Ok(resp) => resp,
        Err(e) => return sse_reply(&conv_id, &format!("（transfer）上游调用失败: {e}"), true),
    };

    let status = upstream.status();
    let bytes = match axum::body::to_bytes(upstream.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return sse_reply(
                &conv_id,
                &format!("（transfer）读取上游响应失败: {e}"),
                true,
            )
        }
    };
    let text = extract_assistant_text(&bytes);
    if text.is_empty() {
        let hint = if status.is_success() {
            "（transfer）上游未返回文本内容。".to_string()
        } else {
            format!("（transfer）上游返回 {status}。")
        };
        return sse_reply(&conv_id, &hint, true); // 失败不落历史,避免污染后续轮
    }
    // 成功:assistant 回复入历史,存回(同一 conv_id,app 下轮回传即续接)。
    history.push(("assistant".to_owned(), text.clone()));
    CONV_STORE.lock().unwrap().put(conv_id.clone(), history);
    sse_reply(&conv_id, &text, false)
}

/// 本轮 ChatGPT 封套里最后一条 user 消息文本(app 每轮只发新消息)。
fn latest_user_text(chat: &serde_json::Value) -> Option<String> {
    chat.get("messages")?
        .as_array()?
        .iter()
        .rev()
        .find(|m| {
            m.get("author")
                .and_then(|a| a.get("role"))
                .and_then(|r| r.as_str())
                == Some("user")
        })
        .map(message_text)
        .filter(|t| !t.trim().is_empty())
}

/// 全历史 → `/responses` body。user 用 `input_text`、assistant 用 `output_text`(Responses
/// input item 惯例);交 forward_handler 复用全套 provider 转换。
fn build_responses_from_history(history: &[(String, String)], model: &str) -> serde_json::Value {
    let input: Vec<serde_json::Value> = history
        .iter()
        .map(|(role, text)| {
            let content_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            serde_json::json!({
                "type": "message",
                "role": role,
                "content": [{ "type": content_type, "text": text }],
            })
        })
        .collect();
    serde_json::json!({ "model": model, "input": input, "stream": true })
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
/// `conv_id` 由调用方传入并回填响应,app 下轮会原样回传以续接历史(空串=解析失败前的兜底)。
fn sse_reply(conv_id: &str, text: &str, is_error: bool) -> Response {
    let conv_id = if conv_id.is_empty() {
        gen_id("conv")
    } else {
        conv_id.to_owned()
    };
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
    fn latest_user_text_takes_last_user() {
        let chat = serde_json::json!({
            "messages": [
                {"author": {"role": "assistant"}, "content": {"parts": ["hi"]}},
                {"author": {"role": "user"}, "content": {"content_type": "text", "parts": ["你好", "世界"]}},
            ],
        });
        assert_eq!(latest_user_text(&chat).as_deref(), Some("你好世界"));
    }

    #[test]
    fn latest_user_text_none_without_user() {
        let chat = serde_json::json!({"messages": [{"author": {"role": "assistant"}, "content": {"parts": ["hi"]}}]});
        assert!(latest_user_text(&chat).is_none());
    }

    #[test]
    fn build_from_history_maps_roles_to_content_types() {
        let history = vec![
            ("user".to_owned(), "记住42".to_owned()),
            ("assistant".to_owned(), "好的".to_owned()),
            ("user".to_owned(), "多少?".to_owned()),
        ];
        let body = build_responses_from_history(&history, "gpt-5.5");
        assert_eq!(body["input"].as_array().unwrap().len(), 3); // 全历史,非单轮
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text"); // assistant
        assert_eq!(body["input"][2]["content"][0]["text"], "多少?");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn conv_store_appends_and_caps() {
        let mut s = ConvStore::default();
        s.put("c1".into(), vec![("user".into(), "a".into())]);
        let mut h = s.get("c1");
        assert_eq!(h.len(), 1);
        h.push(("assistant".into(), "b".into()));
        s.put("c1".into(), h);
        assert_eq!(s.get("c1").len(), 2); // 续接,非覆盖丢失
                                          // 超 MAX_CONVS 淘汰最旧
        for i in 0..(MAX_CONVS + 5) {
            s.put(format!("k{i}"), vec![("user".into(), "x".into())]);
        }
        assert!(s.map.len() <= MAX_CONVS);
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
