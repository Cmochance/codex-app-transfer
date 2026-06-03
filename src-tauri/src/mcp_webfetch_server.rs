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
//! 并发 (MOC-145): `tools/call` 在 tokio runtime 上 `spawn` 异步执行, stdin 读循环不被
//! 长抓取阻塞 —— ping / initialize / tools/list 即时响应。出站经单写线程串行化, 防并发
//! 响应交错写坏帧。详见 [`run`]。
//!
//! 后端档位(curl/wreq/headless)每次 `tools/call` 时读 `~/.codex-app-transfer/config.json`
//! 的 `settings.webFetchBackend`(改档无需重启 Codex);`off` → isError 提示(正常此时
//! 工具不该被注册, 防御性兜底)。

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use codex_app_transfer_http::WebFetchBackend;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const SERVER_NAME: &str = "cat-webfetch";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// client 未给 protocolVersion 时的兜底(回显 client 的值更优, 见 spec)。
const FALLBACK_PROTOCOL: &str = "2025-11-25";
/// 返回正文截断上限(字符)。防把 MB 级页面灌给模型(类 Claude WebFetch 的 100KB 截断)。
const MAX_CONTENT_CHARS: usize = 100_000;
/// stdin EOF 后等在途抓取写完响应的上限(略大于单次工具超时 120s)。匹配旧同步实现
/// "先跑完在途 fetch 再退"的行为, 不丢已在算的响应; 仍卡住的任务到点由 drop(rt) 中止。
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(125);
/// tools/call 并发抓取上限。旧同步实现 block_on 天然串行(隐式并发=1);异步化后加显式上限,
/// 防客户端 bug / 突发并发同时拉起 N 个 headless Chrome 耗尽资源。ping / initialize 等即时
/// 响应类不走这里(inline 派发), 不受此限。
const MAX_CONCURRENT_FETCHES: usize = 4;
/// 喂给总结模型的网页正文上限(字符)。正文已抽取, 这里再硬截防超大页吃爆总结模型上下文。
const SUMMARY_INPUT_CHARS: usize = 60_000;
/// 调总结模型的超时。略宽于一般补全(慢 provider / 长正文)。
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(90);

/// 入口: 读 stdin 逐行 JSON-RPC, 派发到 tokio runtime, 经单写线程串行写 stdout。
///
/// **并发**(MOC-145): `tools/call` 在 runtime 上 `spawn` 异步跑, 读循环不阻塞 —— 长抓取
/// (headless 最长 ~120s)期间 ping / initialize / tools/list 仍即时响应, 避免 Codex 依赖
/// ping keepalive 判活时误杀本 server。出站消息经独立线程 + channel 串行化, 防多个并发
/// 响应交错写坏帧。stdin EOF → 派发器收尾 → 有界 drain 等在途抓取写完响应 → 退出。
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
            eprintln!("[cat-webfetch] tokio runtime build 失败: {e}");
            std::process::exit(1);
        }
    };

    // 出站: 专用线程持 stdout 串行写(blocking 写不占 runtime worker; channel 序列化防
    // 并发响应交错)。所有 sender(派发器 + 各 spawn 任务的 clone)drop 后线程退出。
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Value>();
    let writer = std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        while let Ok(msg) = out_rx.recv() {
            write_msg(&mut stdout, &msg);
        }
    });

    // 入站: 专用线程阻塞读 stdin 逐行转发到 async 派发器(免给 src-tauri 的 tokio 加
    // io-std/io-util feature)。EOF / 真 IO 错 → drop sender → 派发器 recv 收到 None 收尾。
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l).is_err() {
                        break; // 派发器已退出
                    }
                }
                // 单条非 UTF-8 坏行不杀整个 server(Lines 在 Err 后可继续读); 真 IO 错才退。
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    eprintln!("[cat-webfetch] 跳过非 UTF-8 stdin 行: {e}");
                    continue;
                }
                Err(e) => {
                    eprintln!("[cat-webfetch] stdin 读失败, 退出: {e}");
                    break;
                }
            }
        }
    });

    // 派发循环跑在 runtime 上; tools/call spawn 进 JoinSet 并发, 不阻塞继续读。
    rt.block_on(async move {
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
        while let Some(line) = line_rx.recv().await {
            // 顺手回收已完成 task, 防长会话内 JoinSet 无限增长。
            while tasks.try_join_next().is_some() {}
            dispatch_line(line, &out_tx, &mut tasks, &sem);
        }
        // stdin EOF: 等在途抓取写完响应再退(有界), 不丢已在算的结果(匹配旧同步行为)。
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        // out_tx 在此 drop(closure 结束); drain 后在途 task 也都已完成并 drop 其 clone。
    });

    // drain 超时仍卡住的 task → drop(rt) 中止 → 释放其 out_tx clone → writer 收到 channel
    // 关闭后退出。join 收尾确保 flush。
    drop(rt);
    let _ = writer.join();
    let _ = reader.join();
    eprintln!("[cat-webfetch] stdin closed, exiting");
}

/// 派发一行 JSON-RPC: 即时响应类(initialize/ping/tools/list/未知)直接发 out_tx;
/// `tools/call` spawn 进 `tasks` 并发执行(读循环不阻塞)。须在 runtime 上下文内调用。
fn dispatch_line(
    line: String,
    out_tx: &std::sync::mpsc::Sender<Value>,
    tasks: &mut tokio::task::JoinSet<()>,
    sem: &Arc<Semaphore>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let req: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[cat-webfetch] JSON parse 失败: {e}");
            let _ = out_tx.send(rpc_error(Value::Null, -32700, "Parse error"));
            return;
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
            let _ = out_tx.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": proto,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
                }
            }));
        }
        // 通知(无 id): 不回。
        "notifications/initialized" | "notifications/cancelled" => {}
        "ping" => {
            let _ = out_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        }
        "tools/list" => {
            let _ = out_tx.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": [web_fetch_tool_def()] }
            }));
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
            let prompt = params
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("prompt"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            let call_id = id.clone().unwrap_or(Value::Null);
            let out = out_tx.clone();
            let sem = Arc::clone(sem);
            // spawn 进 JoinSet: 抓取并发跑, 读循环立即回去处理后续消息(ping 等不被长抓取
            // 阻塞);EOF 时可 drain 等其写完。catch_unwind: 第三方库(chromiumoxide 等)panic
            // 只毙掉这个 task, 不杀 server(panic=unwind);转 isError 让模型自我纠正。
            tasks.spawn(async move {
                // 限并发抓取(满了排队); 防突发并发同时拉起 N 个 headless Chrome 耗尽资源。
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return, // semaphore 已关(收尾), 放弃本次
                };
                let fut = std::panic::AssertUnwindSafe(handle_web_fetch_call(
                    call_id.clone(),
                    &name,
                    url,
                    prompt,
                ));
                let resp = match futures::FutureExt::catch_unwind(fut).await {
                    Ok(v) => v,
                    Err(_) => tool_error(
                        call_id,
                        "抓取过程内部异常(headless 浏览器崩溃?), 已跳过本次。可重试或切换后端档位。",
                    ),
                };
                let _ = out.send(resp);
            });
        }
        other => {
            // request 的未知 method → method not found;通知的未知 method → 忽略。
            if let Some(id) = id {
                let _ = out_tx.send(rpc_error(id, -32601, &format!("Method not found: {other}")));
            }
        }
    }
}

/// 处理 `tools/call`。owned 参数, 避免跨 await 借用 req。
async fn handle_web_fetch_call(
    id: Value,
    name: &str,
    url: Option<String>,
    prompt: Option<String>,
) -> Value {
    if name != "web_fetch" {
        return rpc_error(id, -32602, &format!("Unknown tool: {name}"));
    }
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return tool_error(id, "缺少必填参数 url(需绝对 http(s) URL)"),
    };
    // prompt 必填 (MOC-152): web_fetch 不返整页, 而是用『总结模型』针对 prompt 摘要/作答。
    let prompt = match prompt {
        Some(p) if !p.is_empty() => p,
        _ => {
            return tool_error(
                id,
                "缺少必填参数 prompt(描述你想从该网页了解 / 提取什么 —— 据此生成针对性摘要)",
            )
        }
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
        // 2xx 但空 body: 不静默当成功(模型会以为抓到了空页), 给明确可操作提示。MOC-145:
        // web_fetch 对合法空响应(如 204)返 Ok(""), 区分"空"与"抓取失败"的语义落在这里。
        Ok(body) if body.trim().is_empty() => tool_ok(
            id,
            &format!(
                "(请求成功但响应体为空 — 常见于需 JS 渲染的前端页 / 反爬拦截 / 重定向丢内容。\
                 当前后端: {}。若内容靠 JS 渲染, 可在 codex-app-transfer 设置把内置联网抓取工具切到 \
                 headless 档后重试; 也请确认 URL 是否正确。)",
                backend.as_str()
            ),
        ),
        // 抓到正文 → 用总结模型针对 prompt 摘要; 摘要失败(未配/proxy 未起/模型报错/格式不支持)
        // → 回退返回抓取的原文(绝不丢内容), 并注明摘要未生成。
        Ok(body) => match summarize(&body, &prompt).await {
            Ok(summary) => tool_ok(id, &truncate(&summary, MAX_CONTENT_CHARS)),
            Err(e) => {
                eprintln!("[cat-webfetch] 网页摘要未生成, 回退原文: {e}");
                let note = format!("(注: 网页摘要未生成({e});以下为抓取到的网页正文原文。)\n\n");
                tool_ok(id, &truncate(&format!("{note}{body}"), MAX_CONTENT_CHARS))
            }
        },
        Err(e) => tool_error(id, &format!("抓取失败(后端 {}): {e}", backend.as_str())),
    }
}

/// 网页摘要配置(每次 `tools/call` 读 config, 改配置无需重启 Codex)。
struct SummaryConfig {
    proxy_port: u16,
    /// 本地 proxy 的 gateway key(`Authorization: Bearer <key>`);MOC-108 后默认强制有。
    gateway_key: Option<String>,
    /// 发给本地 proxy 的 model 字段, 形如 `<provider-slug>/<summaryModel>`(slug 前缀使
    /// proxy 逐字转发该模型, 不被重映射成 `models["default"]`)。
    model: String,
    /// provider 的 api 格式(决定走 chat/completions 还是其它;当前仅 openai_chat 支持摘要)。
    api_format: String,
}

/// 读 `~/.codex-app-transfer/config.json`, 解析当前 active provider 的摘要配置。
fn read_summary_config() -> Result<SummaryConfig, String> {
    let path = codex_app_transfer_registry::config_file()
        .ok_or_else(|| "无法定位 config.json(HOME 未设置?)".to_string())?;
    let cfg = codex_app_transfer_registry::load_raw_config(&path)
        .map_err(|e| format!("读取 config.json 失败: {e}"))?;
    parse_summary_config(&cfg)
}

/// 从 config `Value` 解析当前 active provider 的摘要配置(纯函数, 便于单测)。
fn parse_summary_config(cfg: &Value) -> Result<SummaryConfig, String> {
    let proxy_port = cfg
        .get("settings")
        .and_then(|s| s.get("proxyPort"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "config 缺 settings.proxyPort".to_string())? as u16;
    let gateway_key = cfg
        .get("gatewayApiKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let active = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "未选择当前提供商(activeProvider 为空)".to_string())?;
    let prov_val = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(active))
        })
        .ok_or_else(|| format!("当前提供商({active})不在 providers 列表中"))?;
    // 反序列化成 Provider, 复用规范的 provider_slug(slug 路由)+ extra flatten(读 summaryModel)。
    let provider: codex_app_transfer_registry::Provider = serde_json::from_value(prov_val.clone())
        .map_err(|e| format!("解析当前提供商配置失败: {e}"))?;
    let api_format = provider.api_format.clone();
    // summaryModel(经 extra flatten 透传)优先, 空/缺 → 回退 models["default"]。
    let model_value = provider
        .extra
        .get("summaryModel")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            provider
                .models
                .get("default")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| "未配置总结模型, 且该提供商 models.default 为空".to_string())?
        .to_string();
    // slug 前缀: 让 proxy 把该模型**逐字**转发到当前 provider —— 绕过 resolver
    // `map_model_for_provider` 对"非 slot-key 模型"降级到 `models["default"]` 的重映射
    // (否则用户选的 summaryModel 不生效, 永远用 default;见 decide_provider 的 `<slug>/<model>` 透传)。
    let slug = codex_app_transfer_registry::provider_slug(&provider);
    Ok(SummaryConfig {
        proxy_port,
        gateway_key,
        model: format!("{slug}/{model_value}"),
        api_format,
    })
}

/// 用『总结模型』针对 `prompt` 对网页正文 `content` 作答 —— 经本地 proxy 调当前 provider 的
/// 模型(复用其路由 + 鉴权改写)。返回 `Err` 时上层回退原文(绝不丢内容)。
///
/// 当前仅支持 `openai_chat` 格式 provider(`/v1/chat/completions`, 主流);其它格式返 Err →
/// 回退原文。后续可扩 `/v1/responses` 覆盖 responses-format provider。
async fn summarize(content: &str, prompt: &str) -> Result<String, String> {
    let cfg = read_summary_config()?;
    if cfg.api_format != "openai_chat" {
        return Err(format!(
            "当前提供商 apiFormat={} 暂不支持网页摘要(仅 openai_chat)",
            cfg.api_format
        ));
    }
    // 控制喂给总结模型的输入大小(正文已抽取, 这里再硬截一层防超大页吃爆上下文)。
    // 截断时**显式告知模型**输入不完整 —— 否则模型会把前 N 字当全文, 给出"看似完整"实则
    // 漏后段的答案(silent completeness failure)。
    let total = content.chars().count();
    let truncated = total > SUMMARY_INPUT_CHARS;
    let capped: String = content.chars().take(SUMMARY_INPUT_CHARS).collect();
    let trunc_hint = if truncated {
        format!(
            "\n\n(注意:网页正文超长, 以下仅为前 {SUMMARY_INPUT_CHARS}/{total} 字符, **可能不完整**;\
             若关键信息可能在后段, 请在回答中明确指出内容被截断。)"
        )
    } else {
        String::new()
    };
    let instruction = format!(
        "你是网页内容摘要助手。下面是一篇网页的正文(已转 markdown)。请**仅依据正文**, 针对\
         「用户需求」给出准确、简洁的回答或摘要;正文未提及的不要编造, 不确定就说明。{trunc_hint}\n\n\
         ## 用户需求\n{prompt}\n\n## 网页正文\n{capped}"
    );
    let client = reqwest::Client::builder()
        .timeout(SUMMARY_TIMEOUT)
        .build()
        .map_err(|e| format!("建 HTTP client 失败: {e}"))?;
    let endpoint = format!("http://127.0.0.1:{}/v1/chat/completions", cfg.proxy_port);
    let body = json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": instruction }],
        "stream": false,
    });
    let mut req = client.post(&endpoint).json(&body);
    if let Some(k) = &cfg.gateway_key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("调本地 proxy 摘要失败(proxy 未启动?): {e}"))?;
    let status = resp.status();
    // 用 map_err 而非 unwrap_or_default: body 读失败(连接中断/解码错)不该被吞成空串再误报
    // "响应非 JSON", 给真因。
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取摘要响应体失败: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "摘要模型 HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("摘要响应非 JSON: {e}"))?;
    let out = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| "摘要响应缺 choices[0].message.content".to_string())?;
    if out.trim().is_empty() {
        return Err("摘要模型返回空内容".to_string());
    }
    // 输入被截断时也给消费者(Codex)带一句, 不只告诉总结模型 —— 避免拿"基于部分正文的摘要"
    // 当完整答案。
    if truncated {
        Ok(format!(
            "(注:网页正文过长, 本摘要基于前 {SUMMARY_INPUT_CHARS} 字符, 可能不完整。)\n\n{out}"
        ))
    } else {
        Ok(out.to_string())
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
        "description": "抓取一个 http(s) URL 的网页, 用配置的『总结模型』针对你的 prompt 生成\
    摘要 / 回答后返回(类 Claude WebFetch, 省 context)。由 codex-app-transfer 代抓(curl / wreq / \
    headless 三档, 绕 Cloudflare + 跑 JS)并抽取正文。**必须提供 prompt** 说明你想从该页了解 / \
    提取什么。若未配置总结模型 / proxy 未起 / 摘要失败, 自动回退返回网页正文原文(不丢内容)。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的绝对 http(s) URL。" },
                "prompt": {
                    "type": "string",
                    "description": "你想从该网页了解 / 提取什么(如『v0.136 的 breaking changes』\
                    『把安装步骤原样列出』)。据此用总结模型生成针对性摘要 / 回答。"
                }
            },
            "required": ["url", "prompt"]
        }
    })
}

/// 按字符截断(非字节, 防截断多字节 UTF-8)。超限时尽量退到最近的换行边界, 避免从句中 /
/// 词中硬切(markdown 段落更完整);仅当该边界离上限不太远(末 1/4 内)才退, 否则宁可硬切
/// 也不浪费过多预算。提示语引导模型抓更具体子页, 而非反复抓同一巨页。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max).collect();
    // 退到最后一个换行边界(就近, 浪费不超过 1/4 预算时才退)。
    if let Some(i) = cut.rfind('\n') {
        if i >= cut.len() * 3 / 4 {
            cut.truncate(i);
        }
    }
    format!("{cut}\n\n[... 内容超过 {max} 字符已截断;需要后续内容请抓取更具体的子页 URL ...]")
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
                eprintln!("[cat-webfetch] 写 stdout 失败: {e}");
            }
        }
        Err(e) => eprintln!("[cat-webfetch] 消息序列化失败(逻辑 bug): {e}"),
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
        // MOC-152: prompt 设为必填(摘要驱动)。
        assert_eq!(d["inputSchema"]["required"][1], "prompt");
        assert_eq!(d["inputSchema"]["properties"]["url"]["type"], "string");
        assert_eq!(d["inputSchema"]["properties"]["prompt"]["type"], "string");
    }

    #[test]
    fn parse_summary_config_prefers_summary_then_default() {
        let cfg = json!({
            "activeProvider": "p1",
            "gatewayApiKey": "cas_x",
            "settings": { "proxyPort": 18080 },
            "providers": [{
                "id": "p1", "name": "P1", "baseUrl": "https://api.p1.com/v1",
                "apiFormat": "openai_chat",
                "summaryModel": "deepseek-chat",
                "models": { "default": "deepseek-reasoner" }
            }]
        });
        let c = parse_summary_config(&cfg).unwrap();
        // summaryModel 优先, 且 slug 前缀(slug("p1")="p1")绕过 proxy 重映射。
        assert_eq!(c.model, "p1/deepseek-chat");
        assert_eq!(c.proxy_port, 18080);
        assert_eq!(c.gateway_key.as_deref(), Some("cas_x"));
        assert_eq!(c.api_format, "openai_chat");
    }

    #[test]
    fn parse_summary_config_falls_back_to_default() {
        // summaryModel 空白 → 回退 models.default; 缺 apiFormat → 默认 openai_chat; 无 gateway key
        let cfg = json!({
            "activeProvider": "p1",
            "settings": { "proxyPort": 1234 },
            "providers": [{
                "id": "p1", "name": "P1", "baseUrl": "https://api.p1.com/v1",
                "summaryModel": "   ", "models": { "default": "glm-4" }
            }]
        });
        let c = parse_summary_config(&cfg).unwrap();
        assert_eq!(c.model, "p1/glm-4"); // 空白 summaryModel → default, slug 前缀
        assert_eq!(c.api_format, "openai_chat");
        assert!(c.gateway_key.is_none());
    }

    #[test]
    fn parse_summary_config_errors_on_missing() {
        // 无 activeProvider
        assert!(parse_summary_config(&json!({ "settings": { "proxyPort": 1 } })).is_err());
        // 缺 proxyPort
        assert!(parse_summary_config(&json!({ "activeProvider": "p1" })).is_err());
        // active 但既无 summaryModel 又无 models.default(provider 字段齐全, 仅模型为空)
        let cfg = json!({
            "activeProvider": "p1",
            "settings": { "proxyPort": 1 },
            "providers": [{
                "id": "p1", "name": "P1", "baseUrl": "https://api.p1.com/v1",
                "models": { "default": "" }
            }]
        });
        assert!(parse_summary_config(&cfg).is_err());
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
    fn truncate_prefers_newline_boundary() {
        // 换行落在末 1/4 内 → 退到换行边界, 不带半行
        let s = format!("{}\n{}", "a".repeat(16), "b".repeat(10));
        let t = truncate(&s, 20); // 前 20 字符 = 16a + \n + 3b; \n@16 >= 15 → 退到 16
        assert!(t.starts_with(&"a".repeat(16)), "应保留整段 a: {t}");
        assert!(!t.contains('b'), "应退到换行边界、不含半行 b: {t}");
        assert!(t.contains("已截断"));
        // 换行太靠前(末 1/4 外)→ 不退, 硬切以免浪费预算
        let s2 = format!("{}\n{}", "c".repeat(4), "d".repeat(40));
        let t2 = truncate(&s2, 20); // \n@4 < 15 → 硬切, 含 d
        assert!(t2.contains('d'), "边界太靠前应硬切: {t2}");
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
