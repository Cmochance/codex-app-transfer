//! [MOC-257] 模拟(伪造)账号模式 —— `/backend-api/*` 逐条伪造器。
//!
//! 背景:无真实 ChatGPT 账号时,旧的「强制注入」靠 CDP 在渲染进程改 `setAuthMethod('chatgpt')`,
//! 但活动 `auth.json` 仍是 apikey → 通信层不自洽,插件功能跑不通。新方案改为写一份**合规但伪造**
//! 的 `auth.json`(`auth_mode=chatgpt` + 合成 JWT,见 `src-tauri::codex_real_account`),让 Codex 原生
//! 走 chatgpt 路径(原生显示 Plugins、CLI 原生发 `/backend-api/*`),再在本 proxy **截断**这些请求、
//! 逐条下发伪造响应,而**不**透传真 `chatgpt.com`(伪造 token 会被上游 401)。
//!
//! 关键约束(抓包 + 解包 Codex 实证):
//! - **一律返 200(或语义对的 204),绝不返 401** —— 401 会进 Codex auth 失败路径、可能触发重登;
//!   200 + 合法 JSON(哪怕空)只会让 UI 软降级到空态,不重试风暴、不锁 tab。
//! - Plugins tab 可见性是 Codex 本地 feature flag(默认 true),不依赖任何 backend 成功;伪造只为
//!   填充 marketplace/featured 内容。**MVP:marketplace 一律空目录**(`{"plugins":[]}` 等)。
//! - 已安装插件来自 CLI 读本地 `~/.codex/plugins/cache/`,与本伪造无关、照常显示。
//!
//! 伪造 vs 透传的判定:进程级 [`FAKE_ACCOUNT_MODE`] atomic,由 src-tauri 在开关 enable/disable 与
//! 启动调谐时通过 [`set_fake_account_mode`] 设置(与 `ProxyState::with_relogin_notify` 反向注入同
//! 范式,proxy crate 不反依赖 src-tauri)。

use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::forward::ForwardError;
use crate::telemetry::proxy_telemetry;

/// 模拟账号模式总开关。`true` = 活动 `auth.json` 是合成伪造账号 → `/backend-api/*` 走本模块伪造;
/// `false` = 关闭 / 真实账号 relay → 走 `forward::passthrough_chatgpt_backend` 透传真 chatgpt.com。
/// 默认 `false`(proxy 启动即关,由 src-tauri 启动调谐按持久 flag 设置)。
static FAKE_ACCOUNT_MODE: AtomicBool = AtomicBool::new(false);

/// 设置模拟账号模式开关。src-tauri 在 fake-account enable/disable handler 与启动调谐中调用。
pub fn set_fake_account_mode(on: bool) {
    FAKE_ACCOUNT_MODE.store(on, Ordering::SeqCst);
}

/// 模拟账号模式是否开启。`forward_handler` 命中 `is_chatgpt_backend_path` 时据此选伪造 vs 透传。
pub fn fake_account_mode_enabled() -> bool {
    FAKE_ACCOUNT_MODE.load(Ordering::SeqCst)
}

/// 逐条伪造 `/backend-api/*` 响应。**永不返 401**:未显式建模的 path 一律兜底 200 `{}`。
///
/// `method` / `headers` 暂未用于内容生成(MVP 空目录),保留签名对齐 `passthrough_chatgpt_backend`,
/// 便于后续按请求体/头(如 wham JSON-RPC 的 `id`)做更精细伪造。
pub async fn fabricate(
    method: &Method,
    _headers: &HeaderMap,
    client_path: &str,
    body: Bytes,
) -> Result<Response, ForwardError> {
    let path = client_path.split('?').next().unwrap_or(client_path);
    let telemetry = proxy_telemetry();
    telemetry
        .logs
        .add("INFO", format!("[fake-account] fabricate {method} {path}"));

    // (status, body):204 表示无内容(纯埋点 / JSON-RPC notification)。
    let (status, payload): (u16, Value) = match (method.as_str(), path) {
        // 纯埋点,直接吞掉(不落任何上游)。
        ("POST", "/backend-api/codex/analytics-events/events") => (204, Value::Null),

        // 插件:MVP 一律空目录。已装插件由 CLI 读本地 cache 显示,与此无关。
        ("GET", "/backend-api/ps/plugins/installed") => (
            200,
            json!({ "plugins": [], "pagination": { "limit": 50, "next_page_token": "" } }),
        ),
        ("GET", "/backend-api/ps/plugins/list") => (200, json!({ "plugins": [] })),
        ("GET", "/backend-api/ps/plugins/suggested") => (200, json!({ "plugins": [] })),
        ("GET", "/backend-api/plugins/featured") => (200, json!({ "plugins": [] })),

        // Connectors 目录:空。
        ("GET", "/backend-api/connectors/directory/list") => {
            (200, json!({ "data": [], "next_token": Value::Null }))
        }
        ("GET", "/backend-api/connectors/directory/list_workspace") => {
            (200, json!({ "data": [], "next_token": Value::Null }))
        }

        // 插件商店 MCP / wham apps:JSON-RPC over HTTP。echo `id` 返空 result;notification(无 id)→ 204。
        ("POST", "/backend-api/ps/mcp") | ("POST", "/backend-api/wham/apps") => {
            jsonrpc_empty_reply(&body)
        }

        // wham 额度/使用:空对象(客户端 `retry:false`,失败也不重试)。
        ("GET", "/backend-api/wham/usage") => (200, json!({})),

        // wham 远程控制:空(本项目不开远程控制;给最小合法形状防解析报错)。
        ("POST", "/backend-api/wham/remote/control/server/refresh") => (200, json!({})),
        _ if path.starts_with("/backend-api/wham/remote/control") => {
            (200, json!({ "clients": [] }))
        }

        // 兜底:任何未建模的 backend-api path 一律 200 `{}`,**绝不 401**(避免触发 Codex 重登)。
        _ => (200, json!({})),
    };

    build_json_response(status, &payload)
}

/// 解析 JSON-RPC 请求体,echo `id` 返 `{"jsonrpc":"2.0","id":<id>,"result":{}}`;无 `id`(notification)
/// 返 204。空 result 是最小安全形状(MVP 不暴露任何 marketplace MCP server / app)。
fn jsonrpc_empty_reply(body: &Bytes) -> (u16, Value) {
    let id = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned());
    match id {
        Some(id) => (200, json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        None => (204, Value::Null),
    }
}

/// 构造伪造响应。204 → 空体无 content-type;其余 → `application/json`。
fn build_json_response(status: u16, payload: &Value) -> Result<Response, ForwardError> {
    let mut builder = Response::builder().status(status);
    if status == 204 {
        return Ok(builder.body(Body::empty())?);
    }
    builder = builder.header("content-type", "application/json");
    let bytes = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Ok(builder.body(Body::from(bytes))?)
}
