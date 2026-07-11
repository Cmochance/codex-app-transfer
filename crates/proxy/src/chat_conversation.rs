//! [MOC-323] Codex/ChatGPT 桌面 app 的 **Chat(经典 ChatGPT 对话)** 接入 transfer 自定义模型。
//!
//! 背景(实测,详见 Linear MOC-322):26.707 起 app 新增 Chat = 嵌入的经典 ChatGPT 对话,
//! renderer 直发 `POST {CODEX_API_BASE_URL}/f/conversation`(SSE),body 是 classic ChatGPT
//! 封套 `{action, messages:[{author,content}], model, parent_message_id}`。base 由环境变量
//! `CODEX_API_BASE_URL` 决定(不读 config.toml),transfer 把它指向本 proxy 后,整个
//! `/backend-api/*` 流进 [`crate::forward::forward_handler`](fallback)。
//!
//! 本模块**只接管 1 条**(`POST …/f/conversation`)+ 就地回 CORS 预检;其余(`/prepare`、
//! `/models`、账号/会话列表/plugins…)返回 `None`,仍走既有 `passthrough_chatgpt_backend`
//! (透传真 chatgpt.com):
//! - `POST …/f/conversation` → 把 ChatGPT 封套转成 `/responses` body,**内部重派**给
//!   `forward_handler`(复用全套 provider resolve / adapter / 鉴权改写),收集 Responses SSE
//!   的文本,再回一条 ChatGPT 整条-message SSE(客户端 `decodeNonDeltaEvent` 对不带
//!   `event: delta_encoding` 的事件直接渲染 `e.data`,故免写 delta 增量协议)。
//! - `OPTIONS …/f/conversation` → 就地回 CORS 头(renderer 跨源 POST 带 Authorization 会先
//!   发预检,落 passthrough 拿不到 `Allow-*` 会阻塞真请求)。
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

/// 一轮消息的角色(封闭集,取代裸 `(String, String)` 元组 → 消除 role 拼写/大小写静默错分
/// content_type 的隐患;code-review H6)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    User,
    Assistant,
}

impl Role {
    /// Responses `role` 字段的 wire 值。
    fn wire(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
    /// Responses input item 的 content type(exhaustive,无静默 else 兜底)。
    fn content_type(self) -> &'static str {
        match self {
            Role::User => "input_text",
            Role::Assistant => "output_text",
        }
    }
}

/// 一轮对话消息。
#[derive(Clone, Debug)]
struct Turn {
    role: Role,
    text: String,
}

impl Turn {
    fn user(text: impl Into<String>) -> Self {
        Turn {
            role: Role::User,
            text: text.into(),
        }
    }
    fn assistant(text: impl Into<String>) -> Self {
        Turn {
            role: Role::Assistant,
            text: text.into(),
        }
    }
}

/// [MOC-323 对话连续] Chat 每轮**只发新消息**(实测 messages.len=1;classic ChatGPT 靠
/// conversation_id 服务端存历史),transfer 拦截后无状态,故自持每 conv_id 的历史,支持多轮。
/// app 会把上一轮响应里的 conversation_id 原样回传,据此续接。
static CONV_STORE: LazyLock<Mutex<ConvStore>> = LazyLock::new(|| Mutex::new(ConvStore::default()));

#[derive(Default)]
struct ConvStore {
    map: HashMap<String, Vec<Turn>>, // conv_id → 历史
    order: Vec<String>,              // LRU:末尾最近用(不变量:与 map 键集一致)
}

impl ConvStore {
    fn get(&self, id: &str) -> Vec<Turn> {
        self.map.get(id).cloned().unwrap_or_default()
    }
    fn put(&mut self, id: String, mut history: Vec<Turn>) {
        if history.len() > MAX_TURNS_PER_CONV {
            history.drain(0..history.len() - MAX_TURNS_PER_CONV); // 留最新 MAX_TURNS_PER_CONV 条
        }
        self.order.retain(|x| x != &id);
        self.order.push(id.clone());
        self.map.insert(id, history);
        while self.order.len() > MAX_CONVS {
            let oldest = self.order.remove(0);
            self.map.remove(&oldest);
        }
        // 不变量护栏:map 键集必须与 order 一致(防将来改动让二者失步、泄漏不可淘汰项)。
        debug_assert_eq!(self.map.len(), self.order.len(), "ConvStore map/order 失步");
    }
}

/// gate:功能开关。**真实来源是 registry `settings.chatCustomModelEnabled`**(默认 true);
/// env `CAS_CHAT_CUSTOM_MODEL` 为显式覆盖(`0`/`false`/`FALSE`=关,其它=开),优先于 registry
/// (测试 / 临时覆盖用)。
///
/// 注:功能的**主 gate 是 `chat_launch_env` 是否注入 `CODEX_API_BASE_URL`**(关时 Chat 根本不
/// 流进本 proxy);本函数是流进来后的**二次 in-process gate**,现改为真读 registry,使运行时
/// 关开关即刻停止拦截(不必等重启 Codex)。
pub fn chat_custom_model_enabled() -> bool {
    match std::env::var("CAS_CHAT_CUSTOM_MODEL").as_deref() {
        Ok("0") | Ok("false") | Ok("FALSE") => return false,
        Ok(_) => return true,
        Err(_) => {}
    }
    read_chat_setting().unwrap_or(true)
}

/// 读 registry `settings.chatCustomModelEnabled`。文件不存在/损坏/缺键 → None(caller 默认 true)。
fn read_chat_setting() -> Option<bool> {
    let path = codex_app_transfer_registry::paths::resolve_home()?
        .join(".codex-app-transfer")
        .join("config.json");
    let bytes = std::fs::read(path).ok()?;
    let cfg: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    cfg.get("settings")?
        .get("chatCustomModelEnabled")?
        .as_bool()
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
    if !is_conversation_stream_path(path) {
        return None;
    }
    // 跨源 POST(带 Authorization + JSON)的 CORS 预检:就地回 Allow-*,避免落 passthrough 阻塞。
    if method == Method::OPTIONS {
        return Some(cors_preflight_reply());
    }
    if method == Method::POST {
        return Some(handle_conversation(state, headers, body).await);
    }
    None
}

/// CORS 预检响应:204 + `Access-Control-Allow-*`(无 cookie 的 Authorization 请求,`*` 合法)。
fn cors_preflight_reply() -> Response {
    use axum::http::header::{
        ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
        ACCESS_CONTROL_MAX_AGE,
    };
    (
        StatusCode::NO_CONTENT,
        [
            (ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS"),
            (ACCESS_CONTROL_ALLOW_HEADERS, "authorization, content-type"),
            (ACCESS_CONTROL_MAX_AGE, "600"),
        ],
    )
        .into_response()
}

async fn handle_conversation(state: &ProxyState, headers: &HeaderMap, body: &[u8]) -> Response {
    let chat: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "[Chat] 无法解析 /f/conversation 请求 body");
            return sse_reply("", &format!("（transfer）无法解析 chat 请求: {e}"), true);
        }
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
    history.push(Turn::user(user_text));
    // picker 选中的原始 gpt model id(gpt-5.5 / gpt-5.6-sol / auto …)。空/缺 → 主槽 gpt-5.5。
    // **不再把 `auto` 强制改写成 gpt-5.5**(code-review M2):`auto` 原样透传,resolver 的
    // `openai_model_slot("auto")=None` 会降级到 provider default —— 与 relabel 注入器把 Auto
    // 显示成 default 一致(显示=实际调用)。gpt-5.5 则命中 gpt_5_5 槽。
    let model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or("gpt-5.5");
    let responses_body = build_responses_from_history(&history, model);

    let req = build_responses_request(headers, &responses_body);
    // Box::pin 打破 forward_handler → try_handle → forward_handler 的 async 递归(否则
    // future 尺寸无限、E0733)。本模块只在 `/f/conversation` 命中,重派的是 `/responses`,
    // 不会再次进本拦截分支,递归深度恒为 1。
    let upstream = match Box::pin(forward_handler(axum::extract::State(state.clone()), req)).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, conv = %conv_id, "[Chat] 上游调用失败");
            return sse_reply(&conv_id, &format!("（transfer）上游调用失败: {e}"), true);
        }
    };

    let status = upstream.status();
    let bytes = match axum::body::to_bytes(upstream.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, conv = %conv_id, "[Chat] 读取上游响应失败");
            return sse_reply(
                &conv_id,
                &format!("（transfer）读取上游响应失败: {e}"),
                true,
            );
        }
    };
    let text = extract_assistant_text(&bytes);
    if text.is_empty() {
        // [code-review H2] 不把上游真错吞成「未返回文本」:先从 SSE / body 抠出 error.message
        // (model-not-found / 额度 / 上下文超长 等),带给用户 + 记 warn 便于诊断。
        let err = extract_error_text(&bytes);
        let hint = match (&err, status.is_success()) {
            (Some(m), _) => format!("（transfer）上游错误：{m}"),
            (None, true) => "（transfer）上游未返回文本内容。".to_string(),
            (None, false) => format!("（transfer）上游返回 {status}。"),
        };
        tracing::warn!(%status, conv = %conv_id, error = err.as_deref().unwrap_or("(无)"), "[Chat] 上游无有效文本");
        return sse_reply(&conv_id, &hint, true); // 失败不落历史,避免污染后续轮
    }
    // 成功:assistant 回复入历史,存回(同一 conv_id,app 下轮回传即续接)。
    history.push(Turn::assistant(text.clone()));
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
fn build_responses_from_history(history: &[Turn], model: &str) -> serde_json::Value {
    let input: Vec<serde_json::Value> = history
        .iter()
        .map(|turn| {
            serde_json::json!({
                "type": "message",
                "role": turn.role.wire(),
                "content": [{ "type": turn.role.content_type(), "text": turn.text }],
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

/// 从上游响应里抠错误文本:先扫 SSE 每行 JSON 的 error(`response.failed`/`response.error`/
/// `error.message` 等),再兜底把整体 body 当 JSON 试。无 → None(caller 用泛化提示)。
fn extract_error_text(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let l = line.trim_start();
        let js = l.strip_prefix("data:").map(str::trim).unwrap_or(l);
        if js.is_empty() || js == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(js) {
            if let Some(m) = error_message_of(&v) {
                return Some(m);
            }
        }
    }
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .as_ref()
        .and_then(error_message_of)
}

/// 从一个 JSON value 按常见路径取错误消息串(非空)。
fn error_message_of(v: &serde_json::Value) -> Option<String> {
    const PATHS: &[&[&str]] = &[
        &["response", "error", "message"],
        &["error", "message"],
        &["error"],
        &["message"],
        &["detail"],
    ];
    for path in PATHS {
        let mut cur = v;
        let mut ok = true;
        for k in *path {
            match cur.get(*k) {
                Some(n) => cur = n,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(s) = cur.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
    }
    None
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
    fn message_text_string_content_fallback() {
        // content 直接是串(而非 {parts:[]})的兼容路径。
        let m = serde_json::json!({"author": {"role": "user"}, "content": "直接是串"});
        assert_eq!(message_text(&m), "直接是串");
    }

    #[test]
    fn build_from_history_maps_roles_to_content_types() {
        let history = vec![
            Turn::user("记住42"),
            Turn::assistant("好的"),
            Turn::user("多少?"),
        ];
        let body = build_responses_from_history(&history, "gpt-5.5");
        assert_eq!(body["input"].as_array().unwrap().len(), 3); // 全历史,非单轮
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text"); // assistant
        assert_eq!(body["input"][2]["content"][0]["text"], "多少?");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn conv_store_appends_and_caps() {
        let mut s = ConvStore::default();
        s.put("c1".into(), vec![Turn::user("a")]);
        let mut h = s.get("c1");
        assert_eq!(h.len(), 1);
        h.push(Turn::assistant("b"));
        s.put("c1".into(), h);
        assert_eq!(s.get("c1").len(), 2); // 续接,非覆盖丢失
                                          // 超 MAX_CONVS 淘汰最旧;数量恰为上限、map/order 不失步
        for i in 0..(MAX_CONVS + 5) {
            s.put(format!("k{i}"), vec![Turn::user("x")]);
        }
        assert_eq!(s.map.len(), MAX_CONVS);
        assert_eq!(s.order.len(), s.map.len());
    }

    #[test]
    fn conv_store_trims_to_newest_turns() {
        // 超 MAX_TURNS_PER_CONV 时保留的必须是**尾部最新**,不是最旧(多轮上下文命脉)。
        let mut s = ConvStore::default();
        let hist: Vec<Turn> = (0..MAX_TURNS_PER_CONV + 5)
            .map(|i| Turn::user(format!("m{i}")))
            .collect();
        s.put("c".into(), hist);
        let got = s.get("c");
        assert_eq!(got.len(), MAX_TURNS_PER_CONV);
        assert_eq!(got.first().unwrap().text, "m5"); // 丢了最旧 5 条(m0..m4)
        assert_eq!(
            got.last().unwrap().text,
            format!("m{}", MAX_TURNS_PER_CONV + 4)
        );
    }

    #[test]
    fn conv_store_reput_survives_eviction() {
        // re-put 已有 conv 会把它移到 order 尾部,后续 flood 时不被当「最旧」淘汰。
        let mut s = ConvStore::default();
        s.put("keep".into(), vec![Turn::user("v")]);
        for i in 0..MAX_CONVS {
            if i == MAX_CONVS / 2 {
                s.put("keep".into(), vec![Turn::user("v2")]); // 中途 touch
            }
            s.put(format!("f{i}"), vec![Turn::user("x")]);
        }
        assert_eq!(s.get("keep").len(), 1); // 仍在(未被淘汰)
        assert_eq!(s.get("keep")[0].text, "v2");
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
    fn extract_text_prefers_deltas_over_done() {
        // 真实流同时有 delta 和 done;代码优先累加的 delta(done 是兜底)。
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"AB\"}\n\n\
                   data: {\"type\":\"response.output_text.done\",\"text\":\"WRONG\"}\n\n";
        assert_eq!(extract_assistant_text(sse.as_bytes()), "AB");
    }

    #[test]
    fn extract_error_from_failed_event() {
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"model not found\"}}}\n\n";
        assert_eq!(
            extract_error_text(sse.as_bytes()).as_deref(),
            Some("model not found")
        );
        // 纯文本增量、无 error → None(不误报)。
        let ok = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        assert!(extract_error_text(ok.as_bytes()).is_none());
        // 非 SSE 的整体 JSON 错误 body 也能抠。
        let plain = br#"{"error":{"message":"insufficient quota"}}"#;
        assert_eq!(
            extract_error_text(plain).as_deref(),
            Some("insufficient quota")
        );
    }

    #[test]
    fn gate_env_override() {
        // env 显式覆盖 registry(不依赖本机 config,避免测试 flaky)。
        std::env::set_var("CAS_CHAT_CUSTOM_MODEL", "0");
        assert!(!chat_custom_model_enabled());
        std::env::set_var("CAS_CHAT_CUSTOM_MODEL", "1");
        assert!(chat_custom_model_enabled());
        std::env::remove_var("CAS_CHAT_CUSTOM_MODEL");
    }
}
