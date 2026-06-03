//! MCP stdio server 模式 (MOC-144 模型侧注入): transfer 二进制以 `--mcp-serve-webfetch`
//! 启动时进入此模式, 给 Codex CLI 暴露一个 `web_fetch` 工具。Codex 把本二进制作为
//! stdio mcp_server spawn, 走 newline-delimited JSON-RPC over stdin/stdout。
//!
//! 协议 (官方 MCP spec + openai/codex rmcp 1.7.0 实证):
//! - **stdout 只能写 JSON-RPC 消息**(日志一律走 stderr);逐行 `\n` 分隔的紧凑 JSON。
//! - `initialize` → 回 `capabilities.tools` + 回显 client 的 protocolVersion;
//!   `notifications/initialized` → 不回;`tools/list` → 暴露 web_fetch;
//!   `tools/call` → 调 [`codex_app_transfer_http::web_fetch`]。
//! - 工具执行失败 = `result.isError=true`(让模型自我纠正), **非** JSON-RPC error;
//!   未知 method / tool / 坏参数 = JSON-RPC error。
//!
//! 后端档位(curl/wreq/headless)每次 `tools/call` 时读 `~/.codex-app-transfer/config.json`
//! 的 `settings.webFetchBackend`(改档无需重启 Codex);`off` → isError 提示(正常此时
//! 工具不该被注册, 防御性兜底)。

use std::io::{BufRead, Write};

use codex_app_transfer_http::WebFetchBackend;
use serde_json::{json, Value};

const SERVER_NAME: &str = "cas-webfetch";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// client 未给 protocolVersion 时的兜底(回显 client 的值更优, 见 spec)。
const FALLBACK_PROTOCOL: &str = "2025-11-25";
/// 返回正文截断上限(字符)。防把 MB 级页面灌给模型(类 Claude WebFetch 的 100KB 截断)。
const MAX_CONTENT_CHARS: usize = 100_000;

/// 入口: 阻塞读 stdin 逐行 JSON-RPC, 写 stdout。stdin EOF → 退出。
pub fn run() {
    // 自带 tokio runtime(Tauri 未启动)。web_fetch 是 async, headless 还要驱动 CDP
    // handler task, 用 multi_thread 更稳。
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // exit(1) 而非 return: 让 Codex 从非 0 退出码识别"server 启动失败", 不当成正常退出。
            eprintln!("[cas-webfetch] tokio runtime build 失败: {e}");
            std::process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            // 单条非 UTF-8 坏行不该杀掉整个 server(io::Lines 在 Err 后可继续读下一行);
            // 真 IO 错误才退出, 否则一条畸形帧 = 本会话 web_fetch 全灭。
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                eprintln!("[cas-webfetch] 跳过非 UTF-8 stdin 行: {e}");
                continue;
            }
            Err(e) => {
                eprintln!("[cas-webfetch] stdin 读失败, 退出: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[cas-webfetch] JSON parse 失败: {e}");
                write_msg(&mut stdout, &rpc_error(Value::Null, -32700, "Parse error"));
                continue;
            }
        };
        let id = req.get("id").cloned(); // 通知无 id
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "initialize" => {
                let proto = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(FALLBACK_PROTOCOL)
                    .to_string();
                write_msg(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": proto,
                            "capabilities": { "tools": { "listChanged": false } },
                            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
                        }
                    }),
                );
            }
            // 通知(无 id): 不回。
            "notifications/initialized" | "notifications/cancelled" => {}
            "ping" => {
                write_msg(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                );
            }
            "tools/list" => {
                write_msg(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "tools": [web_fetch_tool_def()] }
                    }),
                );
            }
            "tools/call" => {
                let params = req.get("params");
                let name = params
                    .and_then(|p| p.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let url = params
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("url"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string());
                let call_id = id.clone().unwrap_or(Value::Null);
                // catch_unwind: 第三方库(chromiumoxide 等)在主路径 panic 会 unwind 出
                // block_on 杀掉整个 server 进程(panic=unwind), 使本会话 web_fetch 永久失效。
                // 包一层 → panic 转 isError, server 存活。panic 后丢弃 future, AssertUnwindSafe 安全。
                let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(handle_web_fetch_call(call_id.clone(), &name, url))
                }))
                .unwrap_or_else(|_| {
                    tool_error(
                        call_id,
                        "抓取过程内部异常(headless 浏览器崩溃?), 已跳过本次。可重试或切换后端档位。",
                    )
                });
                write_msg(&mut stdout, &resp);
            }
            other => {
                // request 的未知 method → method not found;通知的未知 method → 忽略。
                if let Some(id) = id {
                    write_msg(
                        &mut stdout,
                        &rpc_error(id, -32601, &format!("Method not found: {other}")),
                    );
                }
            }
        }
    }
    eprintln!("[cas-webfetch] stdin closed, exiting");
}

/// 处理 `tools/call`。owned 参数, 避免跨 await 借用 req。
async fn handle_web_fetch_call(id: Value, name: &str, url: Option<String>) -> Value {
    if name != "web_fetch" {
        return rpc_error(id, -32602, &format!("Unknown tool: {name}"));
    }
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return tool_error(id, "缺少必填参数 url(需绝对 http(s) URL)"),
    };
    let backend = match current_backend() {
        Ok(Some(b)) => b,
        Ok(None) => {
            return tool_error(
                id,
                "联网抓取工具已关闭。请在 codex-app-transfer 设置 → 内置联网抓取工具 选 curl / wreq / headless。",
            )
        }
        // 读失败(权限/损坏/无 HOME)区别于"真 off": 给真因而非误导"去开启"(server 能跑起来
        // 说明 backend 当时已 != off, 运行期 None 几乎必是读失败)。
        Err(e) => {
            return tool_error(
                id,
                &format!("读取联网设置失败: {e}(请检查 ~/.codex-app-transfer/config.json)"),
            )
        }
    };
    match codex_app_transfer_http::web_fetch(backend, &url).await {
        Ok(body) => tool_ok(id, &truncate(&body, MAX_CONTENT_CHARS)),
        Err(e) => tool_error(id, &format!("抓取失败(后端 {}): {e}", backend.as_str())),
    }
}

/// 读 `~/.codex-app-transfer/config.json` 的 `settings.webFetchBackend` 当前档(每次调用都读,
/// 改档无需重启 Codex)。`off` / 未知 / 读失败 → None。
fn current_backend() -> Result<Option<WebFetchBackend>, String> {
    let path = codex_app_transfer_registry::config_file()
        .ok_or_else(|| "无法定位 ~/.codex-app-transfer/config.json(HOME 未设置?)".to_string())?;
    let cfg = codex_app_transfer_registry::load_raw_config(&path)
        .map_err(|e| format!("读取 config.json 失败: {e}"))?;
    // 字段缺失视作 off(Ok(None));只有 IO/解析失败才 Err。
    let s = cfg
        .get("settings")
        .and_then(|s| s.get("webFetchBackend"))
        .and_then(|v| v.as_str())
        .unwrap_or("off");
    Ok(WebFetchBackend::parse(s))
}

fn web_fetch_tool_def() -> Value {
    json!({
        "name": "web_fetch",
        "title": "Web Fetch",
        "description": "抓取一个 http(s) URL 的网页内容并以文本返回。由 codex-app-transfer 代抓: \
    按设置档位走 curl(reqwest 静态)/ wreq(浏览器 TLS 指纹, 绕 Cloudflare)/ headless(无头 \
    Chrome 跑 JS, 抓 JS 渲染 SPA)。适合读取网页正文 / 在线文档 / 文本型 API 响应。返回内容超过 \
    约 100KB 会被截断。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的绝对 http(s) URL。" }
            },
            "required": ["url"]
        }
    })
}

/// 按字符截断(非字节, 防截断多字节 UTF-8)。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n\n[... 内容超过 {max} 字符已截断 ...]")
}

fn tool_ok(id: Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": false }
    })
}

fn tool_error(id: Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": true }
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 写一条 MCP 消息: 紧凑单行 JSON + `\n` + flush。stdout 只写 MCP 消息。
fn write_msg(out: &mut std::io::Stdout, msg: &Value) {
    match serde_json::to_string(msg) {
        Ok(s) => {
            // 写失败(EPIPE: Codex 关了 pipe / 磁盘满)记 stderr, 不静默吞。
            if let Err(e) = writeln!(out, "{s}").and_then(|_| out.flush()) {
                eprintln!("[cas-webfetch] 写 stdout 失败: {e}");
            }
        }
        Err(e) => eprintln!("[cas-webfetch] 消息序列化失败(逻辑 bug): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_def_shape() {
        let d = web_fetch_tool_def();
        assert_eq!(d["name"], "web_fetch");
        assert_eq!(d["inputSchema"]["required"][0], "url");
        assert_eq!(d["inputSchema"]["properties"]["url"]["type"], "string");
    }

    #[test]
    fn truncate_under_over_and_multibyte() {
        assert_eq!(truncate("abc", 10), "abc"); // 未超 → 原样
        let long = "x".repeat(50);
        let t = truncate(&long, 10);
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.contains("已截断"));
        // 多字节按字符截断不 panic
        let cjk = "你好世界".repeat(5);
        let _ = truncate(&cjk, 3);
    }

    #[test]
    fn result_and_error_shapes() {
        let ok = tool_ok(json!(1), "body");
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(ok["result"]["content"][0]["type"], "text");
        assert_eq!(ok["result"]["content"][0]["text"], "body");

        let err = tool_error(json!(2), "boom");
        assert_eq!(err["result"]["isError"], true);
        assert_eq!(err["result"]["content"][0]["text"], "boom");

        let rpc = rpc_error(json!(3), -32601, "nope");
        assert_eq!(rpc["jsonrpc"], "2.0");
        assert_eq!(rpc["id"], 3);
        assert_eq!(rpc["error"]["code"], -32601);
    }
}
