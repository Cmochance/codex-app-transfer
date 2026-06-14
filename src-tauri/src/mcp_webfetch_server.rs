//! MCP stdio server 模式 (MOC-144 模型侧注入): transfer 二进制以 `--mcp-serve-webfetch`
//! 启动时进入此模式, 给 Codex CLI 暴露一个 `web_fetch` 工具。Codex 把本二进制作为
//! stdio mcp_server spawn, 走 newline-delimited JSON-RPC over stdin/stdout。
//!
//! ## 数据路径
//! headless/curl/wreq 在**本地**抓取目标 URL 正文 → 正文经 **MCP stdio** 回 Codex core →
//! core 作为 tool 结果发给**主 LLM**(MOC-227 起固定返回全文, summarize 摘要兜底已移除)。
//! 抓取内容**不**由浏览器直接发给模型(headless 只 GET 外部站点、经 CDP 把 DOM 回本地进程)。
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
//!
//! ## cat-webfetch 诊断埋点 (MOC-181)
//! 当主 app 诊断查看器**正在运行**(viewer start 成功写了 runtime sentinel)时, 每次 `tools/call`
//! 完成后把结构化链路条目(请求参数 / 抓取档+升级链+status / 返回字符数)
//! POST 到 viewer 的 `POST /api/ingest`(端口取自 sentinel)。主 app 为其分配全局 seq 后 push 进
//! trace_store, 诊断查看器 cat-webfetch 分页实时可见。**gate on running viewer**(非持久化 config):
//! viewer 没在跑 → 无 sentinel → 不构造不上报。两道防线确保不把 prompt 数据发给非 viewer 进程:
//! ① `diag_target` 读 sentinel 拿端口(sentinel 仅 viewer start 写、stop 删);② 上报前 GET
//! `/api/health` 确认该端口上真是本 viewer(防 crash 残留 sentinel / 端口被占, chatgpt-codex P2)。

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use codex_app_transfer_adapters::responses::read_tool_artifact_raw;
use codex_app_transfer_http::{WebFetchBackend, WebFetchOutcome};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const SERVER_NAME: &str = "cat-webfetch";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP `initialize` 回给模型的整体工具使用规则(MOC-190): 把「回看已抓 URL 优先用 read_url_local」抬到
/// server 级指引(比单个 tool description 优先级高), 提高 read_url_local 调用率、避免重复 web_fetch 同一页。
const SERVER_INSTRUCTIONS: &str = "联网工具使用规则:\n\
1. 需要网上信息时先 web_search(query) 找来源, 再 web_fetch(url) 读该页完整正文。web_search 只返第 1 页(约一二十条);第 1 页没覆盖到你要的信息时, 用 web_search_more(传同一 query + page=2、3…)取下一批**不重复**的新结果, **别用同样 query 重复调 web_search**(会返回同一批)。\n\
2. **凡是本次对话里你已经用 web_fetch 抓过的 URL —— 当你要再次引用 / 摘录 / 附上它的原文 / 回看更多细节时, 必须先用 read_url_local(url) 从本地缓存取回, 不要重复 web_fetch 同一个 URL**。read_url_local 不联网、瞬时返回, 且能拿回已被对话历史折叠/压缩、你当前看不到的完整原文。\n\
3. 抓「新」URL 才用 web_fetch; 回看「旧」(本会话已抓过的)URL 一律 read_url_local。\n\
4. web_fetch 默认返回完整正文供当前轮直接阅读。\n\
5. **任何工具(shell / 飞书 MCP / 其它)的输出在对话历史里被折叠成 `[Tool output stored outside model context]` 摘要时, 摘要里给了 `Artifact ID`。需要那条输出的完整原文时, 调 read_tool_artifact(artifact_id) 取回不截断全文 —— 不要为了拿回历史输出去重跑工具**。read_tool_artifact 瞬时返回, 取回内容仅供当前轮阅读, 之后会再被自动折叠(不长期占上下文)。";
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
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION,
                        // MCP 2025-11-25 SEP-973 icons: 用本应用图标(Codex 折叠的「工具活动」汇总会
                        // 渲染这个小叠层图标)。注: 展开的单条工具调用 header 那个图标由 Codex 的
                        // connector catalog 控制、不读 MCP icons, 故只影响折叠汇总(调研 MOC-190 followup)。
                        "icons": [{ "src": app_icon_data_uri(), "mimeType": "image/png", "sizes": ["128x128"] }]
                    },
                    "instructions": SERVER_INSTRUCTIONS
                }
            }));
        }
        // 通知(无 id): 不回。
        "notifications/initialized" | "notifications/cancelled" => {}
        "ping" => {
            let _ = out_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        }
        "tools/list" => {
            // web_fetch 任意启用档都暴露(off 时本 server 根本不注册);web_search 仅 headless 档
            // 暴露 —— 它必须 headless(DDG 反爬只有真浏览器能过), 非 headless 档列出来模型也只能被
            // handle 拒, 徒增无效调用(用户 #386 验收反馈)。改档需重启 Codex 重新 tools/list 才反映
            // (与 web_fetch 改 on/off 需重启一致);运行期切档的 stale 缓存由 handle_web_search_call
            // 的 backend gate 兜底(failing 双保险)。
            // MOC-235: read_tool_artifact 不联网、始终可用(本 server 始终注册, 见
            // sync_web_fetch_server)。web_fetch / read_url_local 是联网工具, 仅 backend 非 off
            // 时暴露 —— off 档 server 仍在(为 read_tool_artifact), 但不列联网工具(列了也只会被
            // handle 拒, 徒增无效调用)。
            let mut tools = vec![read_tool_artifact_tool_def()];
            if matches!(current_backend(), Ok(Some(_))) {
                tools.push(web_fetch_tool_def());
                tools.push(read_url_local_tool_def());
            }
            // web_search 暴露条件(MOC-190): backend 非 off + Chrome 就绪(系统装了或已下载),
            // 与 web_fetch 档位选择解耦 —— 系统有 Chrome 的用户在 curl/wreq 档也能用 search 且不
            // 触发下载;两者皆无则不暴露(避免在没走过 consent 的档静默拉 ~86MB)。
            if matches!(current_backend(), Ok(Some(_))) && chrome_ready() {
                tools.push(web_search_tool_def());
                tools.push(web_search_more_tool_def()); // MOC-215: 独立翻页工具
            }
            let _ = out_tx.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools }
            }));
        }
        "tools/call" => {
            let params = req.get("params");
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 取整个 arguments 对象, 按 tool name 在 dispatch_tool_call 里分派取各自参数。
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
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
                let fut = std::panic::AssertUnwindSafe(dispatch_tool_call(
                    call_id.clone(),
                    &name,
                    arguments,
                ));
                let resp = match futures::FutureExt::catch_unwind(fut).await {
                    Ok(v) => v,
                    Err(_) => tool_error(
                        call_id,
                        "工具调用内部异常(headless 浏览器崩溃?), 已跳过本次。可重试或切换后端档位。",
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
/// 按 tool name 分派 `tools/call`(owned 参数避免跨 await 借用 req)。新增工具在此加分支。
/// 宽容解析数值工具参数:接受 JSON number、浮点(2.0→2)、数字字符串("2")。模型(尤其 MiMo)
/// 常把整数序列化进 tool_call arguments 时用字符串("2"),只认 `as_u64()` 会漏 → 静默退默认值。
/// MOC-215 实证:web_search_more 的 `page="2"` 字符串被漏解析, page 退 1 → 翻页永远返第 1 页。
fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    let v = args.get(key)?;
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_f64().map(|f| f as usize))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<usize>().ok()))
}

async fn dispatch_tool_call(id: Value, name: &str, args: Value) -> Value {
    match name {
        "web_fetch" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            // MOC-227: 旧 query / summarize / prompt 参数已随摘要兜底移除, 不再读取——旧会话
            // 模型若仍传, 静默忽略、走默认全文路径(非破坏性回退)。
            handle_web_fetch_call(id, url).await
        }
        "read_url_local" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            handle_read_url_local_call(id, url).await
        }
        "read_tool_artifact" => {
            let artifact_id = args
                .get("artifact_id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            // offset 宽容解析(模型常把整数传成字符串 "90000", 见 arg_usize 注释);缺省从头读。
            let offset = arg_usize(&args, "offset").unwrap_or(0);
            handle_read_tool_artifact_call(id, artifact_id, offset).await
        }
        "web_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            let max = arg_usize(&args, "max_results");
            // web_search 固定第 1 页;翻页走独立 web_search_more 工具(MOC-215)。
            handle_web_search_call(id, query, max, Some(1)).await
        }
        "web_search_more" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
            let max = arg_usize(&args, "max_results");
            // page 必填(tool def required, ≥2);缺失/非法时 handle 内 unwrap_or(1) 兜底。arg_usize
            // 宽容解析:模型常把 page 传成字符串 "2"(MiMo 实测), 只认 as_u64 会漏 → 退第 1 页(MOC-215)。
            let page = arg_usize(&args, "page");
            handle_web_search_call(id, query, max, page).await
        }
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// 返回**抓取的全文**(当前轮全文进 LLM; adapter 层保留最新 1 条全文、历史轮才压缩;MOC-190)。
/// 正文经进程内缓存复用、同 URL 不重抓(给 `read_url_local` 取回工具用)。`url` 必填。
/// MOC-227: summarize 摘要兜底已移除, 本工具固定返回全文。
async fn handle_web_fetch_call(id: Value, url: Option<String>) -> Value {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return tool_error(id, "缺少必填参数 url(需绝对 http(s) URL)"),
    };
    let backend = match current_backend() {
        Ok(Some(b)) => b,
        Ok(None) => {
            return tool_error(
                id,
                "联网抓取工具已关闭。请在 codex-app-transfer 设置 → 内置联网抓取工具 选 auto(推荐) / curl / wreq / headless。",
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
    let diag_port = diag_target();
    let diag = diag_port.is_some();
    let captured_at = now_iso();
    let mut fetch_v = Value::Null;
    let mut result_v = Value::Null;

    // 1) 取正文: 进程内缓存命中则复用(offset 翻页 / 重读同 URL 不重抓), miss 才真抓。
    //    Ok(正文) 进入分流; Err(已构造的早退 resp) 用于空 body / 抓取失败。
    // 缓存 key 含 backend(MOC-190 chatgpt-codex P2): 否则 curl/wreq 抓的 body 缓存后, 切 headless
    // 重试同 URL 在 TTL 内命中 stale 非渲染 body, 回归 per-call backend reread。
    let cache_key = format!("{}|{}", backend.as_str(), url);
    let content = match cache_get(&cache_key) {
        Some(c) => {
            if diag {
                fetch_v = json!({ "cache_hit": true, "body_chars": c.chars().count() });
            }
            Ok(c)
        }
        None => match codex_app_transfer_http::web_fetch(backend, &url).await {
            // 2xx 但空 body(204 / 反爬丢内容): 不静默当成功, 给可操作提示。不缓存空。MOC-145。
            Ok(outcome) if outcome.content.trim().is_empty() => {
                if diag {
                    fetch_v = fetch_value(backend, &outcome);
                    result_v = json!({ "returned_chars": 0, "is_error": false, "empty": true });
                }
                Err(tool_ok(id.clone(), &empty_body_msg(backend)))
            }
            Ok(outcome) => {
                if diag {
                    fetch_v = fetch_value(backend, &outcome);
                }
                cache_put(cache_key.clone(), outcome.content.clone());
                Ok(outcome.content)
            }
            Err(e) => {
                if diag {
                    fetch_v = json!({ "backend": backend.as_str(), "error": e.to_string() });
                    result_v = json!({ "is_error": true });
                }
                Err(tool_error(
                    id.clone(),
                    &format!("抓取失败(后端 {}): {e}", backend.as_str()),
                ))
            }
        },
    };

    // 2) 返回**全文**(当前轮全文进 LLM, adapter 层保留最新 1 条全文;MOC-227 摘要兜底已移除)。
    let resp = match content {
        Err(early) => early,
        Ok(content) => {
            // 返回全文(截到 MAX_CONTENT_CHARS 防对抗巨页)。当前轮全文进 LLM, adapter 层保留
            // 最新 1 条全文、历史轮压缩成 evidence + artifact, 回看用 read_url_local 工具。
            let full = truncate(&content, MAX_CONTENT_CHARS);
            if diag {
                result_v = json!({
                    "returned_chars": full.chars().count(),
                    "is_error": false,
                    "mode": "full",
                });
            }
            tool_ok(id.clone(), &full)
        }
    };

    if let Some(port) = diag_port {
        post_diag_entry(
            port,
            json!({
                "trace_kind": "cat_webfetch",
                "captured_at": captured_at,
                "tool": "web_fetch",
                "request": { "url": url },
                "fetch": fetch_v,
                "result": result_v,
            }),
        )
        .await;
    }
    resp
}

/// 从本地缓存取之前 web_fetch 抓过的某 URL 的完整正文(历史轮被压缩后回看用,MOC-190)。
/// 不联网、不重抓;缓存未命中(>15min 或本会话未抓过)时返回可操作错误提示用户改用 web_fetch。
async fn handle_read_url_local_call(id: Value, url: Option<String>) -> Value {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return tool_error(id, "缺少必填参数 url(需绝对 http(s) URL)"),
    };
    // 仅校验联网工具是否开着(off → 提示); 缓存内容跨所有档找(见下)。
    match current_backend() {
        Ok(Some(_)) => {}
        Ok(None) => {
            return tool_error(
                id,
                "联网抓取工具已关闭。请在 codex-app-transfer 设置 → 内置联网抓取工具 选 auto(推荐) / curl / wreq / headless。",
            )
        }
        Err(e) => {
            return tool_error(
                id,
                &format!("读取联网设置失败: {e}(请检查 ~/.codex-app-transfer/config.json)"),
            )
        }
    }
    // P2(chatgpt-codex): 缓存 key 含 backend, 用户抓完切档会让 read_url_local 用新档 key miss 掉旧档
    // 存的内容 → 跨所有档找; 且同 URL 多档都缓存时按 cached_at 取最新(避免返回旧的弱档骨架)。
    let hit = cache_get_newest_for_url(&url);
    let (resp, returned_chars, is_error) = match hit {
        Some(content) => {
            let out = truncate(&content, MAX_CONTENT_CHARS);
            let n = out.chars().count();
            (tool_ok(id.clone(), &out), n, false)
        }
        None => (
            tool_error(
                id.clone(),
                &format!(
                    "该 URL 未在本地缓存(可能已过期 >15min, 或本会话未用 web_fetch 抓过): {url}。请改用 web_fetch 重新抓取。"
                ),
            ),
            0usize,
            true,
        ),
    };
    // MOC-190: 补诊断埋点 —— 此前 read_url_local 不记 trace, 诊断里看不到回看是否触发/是否命中缓存。
    if let Some(port) = diag_target() {
        post_diag_entry(
            port,
            json!({
                "trace_kind": "cat_webfetch",
                "captured_at": now_iso(),
                "tool": "read_url_local",
                "request": { "url": url },
                "result": { "returned_chars": returned_chars, "is_error": is_error, "cache_hit": !is_error },
            }),
        )
        .await;
    }
    resp
}

/// 单次回取返回的字符上限。必须**低于** proxy 的 `TOOL_OUTPUT_KEEP_FULL_MAX_CHARS`(100k,
/// `crates/adapters/src/responses/request.rs`)—— 否则回取结果回传后会被 `keep_recent_tool_output_full`
/// 当成超限当前轮输出再压成摘要, 大 artifact 永远取不全(MOC-235 review #3)。留 10k headroom 给
/// 续读提示 + 安全余量。超过此长度的 artifact 分页返回, 模型用 `offset` 逐块读完任意大小。
const ARTIFACT_CHUNK_CHARS: usize = 90_000;

/// 把 artifact 全文按 `offset` 切一块(≤ [`ARTIFACT_CHUNK_CHARS`]),`end < total` 时末尾附续读提示。
/// 返回 `(body, 本块字符数)`;`offset` 越界返回 `Err`(说明)。按 char 切分避免劈裂 UTF-8。纯函数便于测。
fn artifact_chunk_body(content: &str, offset: usize) -> Result<(String, usize), String> {
    let total = content.chars().count();
    if offset > 0 && offset >= total {
        return Err(format!(
            "offset {offset} 超出 artifact 长度({total} 字符);从头读用 offset=0。"
        ));
    }
    let chunk: String = content
        .chars()
        .skip(offset)
        .take(ARTIFACT_CHUNK_CHARS)
        .collect();
    let n = chunk.chars().count();
    let end = offset + n;
    let body = if end < total {
        format!(
            "{chunk}\n\n[本次返回 artifact 第 {offset}–{end} / 共 {total} 字符。读取剩余部分: 用同一 artifact_id + offset={end} 再次调用 read_tool_artifact。]"
        )
    } else {
        chunk
    };
    Ok((body, n))
}

/// 通用工具输出回取(MOC-235): 按 `artifact_id` 从共享 `tool_artifacts.db`(proxy 压缩历史工具
/// 输出时落盘, SQLite WAL 跨进程)取回原文。与 read_url_local 互补 —— 后者按 URL 取 web_fetch
/// 缓存, 本工具按 artifact_id 取**任意**被折叠的工具输出(shell / 飞书 MCP / 其它)。不联网、不受
/// webfetch 档位影响。取回内容当前轮进上下文, 下一轮被 proxy 的 MOC-190 滚动压缩收回, 不长期占用。
/// ≤[`ARTIFACT_CHUNK_CHARS`] 一次返全文;更大则从 `offset` 起返回一块 + 续读提示(逐块可取完整)。
async fn handle_read_tool_artifact_call(
    id: Value,
    artifact_id: Option<String>,
    offset: usize,
) -> Value {
    let artifact_id = match artifact_id {
        Some(a) if !a.is_empty() => a,
        _ => {
            return tool_error(
                id,
                "缺少必填参数 artifact_id(取历史里 `[Tool output stored outside model context]` 摘要给出的 Artifact ID)",
            )
        }
    };
    // 三态(MOC-235 review): Ok(Some)=命中 / Ok(None)=真不存在或过期 / Err=瞬时读失败。
    // 瞬时失败若也提示「重跑原工具」会误导模型白跑(本功能正是要省这成本)→ 提示重试同一调用。
    let (resp, returned_chars, is_error) = match read_tool_artifact_raw(&artifact_id) {
        Ok(Some(content)) => match artifact_chunk_body(&content, offset) {
            Ok((body, n)) => (tool_ok(id.clone(), &body), n, false),
            Err(msg) => (tool_error(id.clone(), &msg), 0usize, true),
        },
        Ok(None) => (
            tool_error(
                id.clone(),
                &format!(
                    "未找到该 artifact(可能已过期 >30 天, 或 ID 有误): {artifact_id}。如仍需该数据请用对应工具重新获取。"
                ),
            ),
            0usize,
            true,
        ),
        Err(e) => (
            tool_error(
                id.clone(),
                &format!(
                    "读取 artifact 暂时失败(存储可能正被占用或损坏): {e}。这是临时性读取错误, 请稍后**用同一 artifact_id 重试 read_tool_artifact**, 不要为此重跑原工具。"
                ),
            ),
            0usize,
            true,
        ),
    };
    if let Some(port) = diag_target() {
        post_diag_entry(
            port,
            json!({
                "trace_kind": "cat_webfetch",
                "captured_at": now_iso(),
                "tool": "read_tool_artifact",
                "request": { "artifact_id": artifact_id, "offset": offset },
                "result": { "returned_chars": returned_chars, "is_error": is_error, "hit": !is_error },
            }),
        )
        .await;
    }
    resp
}

/// 网页正文进程内缓存(MOC-190): URL → readability+markdown 后的完整正文。给 `read_url_local` 取回 /
/// 同 URL 重读复用, 避免重抓(也省一次 headless 冷启动)。TTL 短(15min, 对齐 Anthropic/OpenAI
/// fetch 缓存), 容量上限驱逐最旧。cat-webfetch 是长驻进程, 故进程内缓存跨多次 tools/call 有效;
/// 进程重启缓存丢失时 `read_url_local` 走 miss 分支提示重抓 —— 缓存仅加速、非正确性依赖。
const FETCH_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const FETCH_CACHE_CAP: usize = 32;

struct CachedDoc {
    content: String,
    cached_at: Instant,
}

fn fetch_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, CachedDoc>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, CachedDoc>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 命中且未过期 → 返回正文克隆。全局 wrapper, 真逻辑在 [`cache_get_in`](便于用本地 map 单测、
/// 不碰全局共享态、不受并行测试串扰)。
fn cache_get(url: &str) -> Option<String> {
    let mut guard = match fetch_cache().lock() {
        Ok(g) => g,
        // poison 仅持锁线程 panic 才发生(这里持锁只做纯 map 操作, 实际近乎不可能);真发生时
        // 降级无缓存(重抓)是安全的, 但留 stderr 痕避免静默缓存失效(silent-failure-hunter)。
        Err(_) => {
            eprintln!("[cat-webfetch] fetch 缓存锁 poisoned, 本次降级无缓存(重抓)");
            return None;
        }
    };
    cache_get_in(&mut guard, url)
}

/// 跨所有 backend 档找同 URL 缓存, 返回 `cached_at` 最新的正文(read_url_local 用)。同 URL 先弱档(curl
/// 骨架)后 headless(渲染正文)抓时两档都缓存, 固定顺序会先返回旧的 curl 骨架 → 按 cached_at 取最新, 拿
/// 到更完整的渲染版(chatgpt-codex P2)。
fn cache_get_newest_for_url(url: &str) -> Option<String> {
    let mut guard = match fetch_cache().lock() {
        Ok(g) => g,
        Err(_) => {
            eprintln!("[cat-webfetch] fetch 缓存锁 poisoned, read_url_local 降级 miss");
            return None;
        }
    };
    guard.retain(|_, d| d.cached_at.elapsed() < FETCH_CACHE_TTL);
    ["auto", "curl", "wreq", "headless"]
        .iter()
        .filter_map(|b| guard.get(&format!("{b}|{url}")))
        .max_by_key(|d| d.cached_at)
        .map(|d| d.content.clone())
}

/// 写缓存。全局 wrapper, 真逻辑在 [`cache_put_in`]。
fn cache_put(url: String, content: String) {
    match fetch_cache().lock() {
        Ok(mut m) => cache_put_in(&mut m, url, content),
        Err(_) => eprintln!("[cat-webfetch] fetch 缓存锁 poisoned, 本次跳过写缓存"),
    }
}

/// [`cache_get`] 纯逻辑(操作传入 map): retain 清过期(惰性 GC)+ 命中返回克隆。
fn cache_get_in(
    map: &mut std::collections::HashMap<String, CachedDoc>,
    url: &str,
) -> Option<String> {
    map.retain(|_, d| d.cached_at.elapsed() < FETCH_CACHE_TTL);
    map.get(url).map(|d| d.content.clone())
}

/// [`cache_put`] 纯逻辑(操作传入 map): 容量满且是新 key 时驱逐最旧一条(按 cached_at)。
fn cache_put_in(
    map: &mut std::collections::HashMap<String, CachedDoc>,
    url: String,
    content: String,
) {
    if map.len() >= FETCH_CACHE_CAP && !map.contains_key(&url) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, d)| d.cached_at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        url,
        CachedDoc {
            content,
            cached_at: Instant::now(),
        },
    );
}

/// Chrome 是否就绪可跑 headless: 系统装了 Chrome / Edge / Chromium, 或已下载内置 chrome-headless-shell
/// (**不触发下载**)。web_search 的暴露 gate(tools/list)与调用 gate 共用(MOC-190)—— 把 web_search
/// 可见性从 web_fetch 档位解耦为「Chrome 就绪」, 同时守住「不静默下载 86MB」。
fn chrome_ready() -> bool {
    codex_app_transfer_http::headless::detect_system_chrome().is_some()
        || codex_app_transfer_http::headless::chrome_headless_shell_path().is_some()
}

/// 处理 `web_search` tools/call: 走 DDG(headless)搜索, 返回结构化结果列表给模型。
/// **要求 Chrome 就绪**(MOC-190: 暴露/调用 gate 从 web_fetch 档位解耦为「Chrome 就绪」):
/// web_search 内部固定 headless(DDG 纯 HTTP 被 202 反爬拦, MOC-12), 故 ① 尊重 off(用户运行期
/// 关联网即拒)② 非 off 但 Chrome 未就绪拒 + 引导(不在没走过 consent 的档静默下载 ~86MB);系统
/// 装了 Chrome / 已下载内置 Chrome 即放行 —— curl/wreq 档只要 Chrome 在也能用(不再强制 headless 档)。
async fn handle_web_search_call(
    id: Value,
    query: Option<String>,
    max_results: Option<usize>,
    page: Option<usize>,
) -> Value {
    let query = match query {
        Some(q) if !q.is_empty() => q,
        _ => return tool_error(id, "缺少必填参数 query(搜索关键词 / 问题)"),
    };
    // web_search 必须用真浏览器(DDG 反爬)→ 要求 Chrome 就绪(MOC-190: 从 web_fetch 档位解耦为
    // 「Chrome 就绪」)。off 拒(尊重关闭);非 off 但 Chrome 未就绪拒 + 引导(避免静默下载 86MB);
    // Chrome 就绪(系统装了或已下载)则任意非 off 档放行 —— web_search 内部固定 headless、不跟随
    // web_fetch 档位, 故 curl/wreq 档只要 Chrome 在就能用。
    match current_backend() {
        Ok(Some(_)) => {}
        Ok(None) => {
            return tool_error(
                id,
                "联网抓取工具已关闭。web_search 需在 codex-app-transfer 设置 → 内置联网抓取工具 选 auto 或 headless 后使用。",
            )
        }
        Err(e) => {
            return tool_error(
                id,
                &format!("读取联网设置失败: {e}(请检查 ~/.codex-app-transfer/config.json)"),
            )
        }
    }
    // Chrome 就绪判定用「不会触发下载」语义(已下载 shell, 或系统 Chrome 自检通过)—— 与 launch 的
    // resolve_chrome_binary 一致, 避免 stale/损坏系统 Chrome 路径 gate 放行后 launch 自检失败
    // fallback 下载 86MB(chatgpt-codex P2)。off 已在上面 match 早退, 此处只对非 off 档判 Chrome。
    if !codex_app_transfer_http::headless::chrome_ready_without_download().await {
        return tool_error(
            id,
            "web_search 需要本机有可用的 Chrome / Edge / Chromium, 或已下载内置 Chrome(DDG 反爬只有\
             真浏览器能过)。请在 codex-app-transfer 设置 → 内置联网抓取工具 选 headless 档完成首次\
             chrome-headless-shell(~86MB)下载后再用。",
        );
    }
    let max = max_results.unwrap_or(codex_app_transfer_http::search::DEFAULT_MAX_RESULTS);
    let page = page.unwrap_or(1); // MOC-215 step2: 1-indexed, 默认第 1 页
    let diag_port = diag_target();
    let diag = diag_port.is_some();
    let captured_at = now_iso();
    let mut result_v = Value::Null;
    let resp = match codex_app_transfer_http::web_search(&query, max, page).await {
        Ok(results) => {
            let formatted = truncate(
                &format_search_results(&query, &results, page),
                MAX_CONTENT_CHARS,
            );
            if diag {
                result_v = json!({
                    "result_count": results.len(),
                    "returned_chars": formatted.chars().count(),
                    "is_error": false,
                });
            }
            tool_ok(id, &formatted)
        }
        Err(e) => {
            if diag {
                result_v = json!({ "is_error": true, "error": e.to_string() });
            }
            tool_error(id, &format!("web_search 失败: {e}"))
        }
    };
    if let Some(port) = diag_port {
        // web_search 走 DDG headless, 故只记 request + result(无 fetch 段)。
        post_diag_entry(
            port,
            json!({
                "trace_kind": "cat_webfetch",
                "captured_at": captured_at,
                "tool": "web_search",
                "request": { "query": query, "max_results": max, "page": page },
                "result": result_v,
            }),
        )
        .await;
    }
    resp
}

/// 把结果列表格式化成给模型的 markdown(序号 + 标题 + URL + 摘要 + 两段式用法提示)。
fn format_search_results(
    query: &str,
    results: &[codex_app_transfer_http::SearchResult],
    page: usize,
) -> String {
    let mut s = format!(
        "web_search「{query}」第 {page} 页共 {} 条结果。挑你需要的用 web_fetch 抓 URL 取正文:\n\n",
        results.len()
    );
    for (i, r) in results.iter().enumerate() {
        s.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            s.push_str(&format!("   {}\n", r.snippet));
        }
        s.push('\n');
    }
    // 尾部诱导(MOC-215): 单页只 ~10-15 条, 引导模型用 web_search_more 翻页拿**新结果**, 而非用
    // 同 query 重复 web_search(会返回同一批)。这是让翻页功能真被用上的关键(模型默认不会主动翻页)。
    s.push_str(&format!(
        "—— 以上为第 {page} 页。没找到需要的信息?**别用同样 query 再调 web_search**(会返回同一批结果);\
         改用 `web_search_more`(传完全相同的 query「{query}」+ page={})取**下一批不重复**的结果, \
         或换一个更具体的 query 重搜。",
        page + 1
    ));
    s
}

// ============ MOC-181 cat-webfetch 诊断埋点 (上报 viewer ingest) ============

/// 诊断目标端口:仅当 viewer **真在跑**(start 成功写了 runtime sentinel)才返回其监听端口。
/// **gate on running viewer** 而非持久化 config(chatgpt-codex P2):config `traceViewerEnabled`
/// =true 但 viewer bind 失败 / app 已关时无 sentinel → 不上报, 避免把 prompt 数据 POST 给占用
/// 固定端口的任意进程、避免每次 tool call 无谓 POST/超时。sentinel 由 `trace_viewer::start` 写、
/// `stop_silent` 删。读不到 / 解析失败 → None(不上报)。
fn diag_target() -> Option<u16> {
    let path = codex_app_transfer_registry::config_dir()?.join(".trace-viewer-runtime.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("port").and_then(|p| p.as_u64()).map(|p| p as u16)
}

/// captured_at 时间戳 (RFC3339 / ISO8601 local 含时区; 与 forward-trace 其它 producer 的
/// `to_rfc3339()` 一致、与前端解析兼容)。
fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// web_fetch 抓取段诊断 value: 用户选的档 + 实际命中档 + 升级链 + status + 正文字符数。
fn fetch_value(selected: WebFetchBackend, o: &WebFetchOutcome) -> Value {
    json!({
        "backend": selected.as_str(),
        "final_tier": o.final_tier.as_str(),
        "status": o.status,
        "escalation_trail": o.trail,
        "body_chars": o.content.chars().count(),
    })
}

/// 抓取成功但 2xx 空 body 时给模型的提示(抽成函数, handler 正常路径与诊断共用同一文案)。
fn empty_body_msg(backend: WebFetchBackend) -> String {
    format!(
        "(请求成功但响应体为空 — 常见于需 JS 渲染的前端页 / 反爬拦截 / 重定向丢内容。\
         当前后端: {}。{} 也请确认 URL 是否正确。)",
        backend.as_str(),
        if backend.may_use_headless() {
            "该档已用真浏览器渲染, 空多半是页面本身无内容 / 需登录 / 被反爬, 换更具体的 URL 再试;"
        } else {
            "若内容靠 JS 渲染, 可把内置联网抓取工具切到 auto / headless 档后重试;"
        }
    )
}

/// 把诊断条目 POST 到 viewer ingest(失败静默 —— 诊断旁路绝不影响 web_fetch 主功能 / 不阻塞返回)。
/// `port` 来自 viewer 写的 runtime sentinel; **POST 前先 GET `/api/health` 确认该端口上真是本
/// viewer**(sentinel 可能 crash 残留、端口可能被别的进程占)—— 身份不符 / 探测失败就放弃, 绝不把
/// prompt 数据发给非 viewer 进程(chatgpt-codex P2)。短超时, viewer 假死 / 端口不通时快速放弃。
async fn post_diag_entry(port: u16, value: Value) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let base = format!("http://127.0.0.1:{port}");
    let is_viewer = match client.get(format!("{base}/api/health")).send().await {
        Ok(r) if r.status().is_success() => r
            .text()
            .await
            .map(|t| t.contains("cas-trace-viewer"))
            .unwrap_or(false),
        _ => false,
    };
    if !is_viewer {
        return;
    }
    let _ = client
        .post(format!("{base}/api/ingest"))
        .json(&value)
        .send()
        .await;
}

/// 读 `~/.codex-app-transfer/config.json` 的 `settings.webFetchBackend` 当前档(每次调用都读,
/// 改档无需重启 Codex)。`off` / 未知 / 读失败 → None。
fn current_backend() -> Result<Option<WebFetchBackend>, String> {
    let path = codex_app_transfer_registry::config_file()
        .ok_or_else(|| "无法定位 ~/.codex-app-transfer/config.json(HOME 未设置?)".to_string())?;
    let cfg = codex_app_transfer_registry::load_raw_config(&path)
        .map_err(|e| format!("读取 config.json 失败: {e}"))?;
    // 字段缺失视作默认档(MOC-215: auto,对齐 schema 默认,否则缺字段时 web_search 不暴露);
    // 只有 IO/解析失败才 Err。
    let s = cfg
        .get("settings")
        .and_then(|s| s.get("webFetchBackend"))
        .and_then(|v| v.as_str())
        .unwrap_or(codex_app_transfer_registry::schema::DEFAULT_WEB_FETCH_BACKEND);
    Ok(WebFetchBackend::parse(s))
}

/// 本应用图标(128x128 PNG)的 data URI —— 给 MCP `serverInfo.icons`。一次编码后缓存(initialize 不
/// 频繁, OnceLock 足够)。图标取自 `src-tauri/icons/128x128.png`(Tauri app icon)。
fn app_icon_data_uri() -> &'static str {
    use base64::Engine as _;
    static URI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URI.get_or_init(|| {
        let png = include_bytes!("../icons/128x128.png");
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        format!("data:image/png;base64,{b64}")
    })
}

fn web_fetch_tool_def() -> Value {
    json!({
        "name": "web_fetch",
        "title": "Web Fetch",
        "description": "抓取一个 http(s) URL 的网页正文,**直接返回抓取到的完整正文**供你阅读(不分页)。想回看之前抓过的某 URL 全文用 `read_url_local`。由 codex-app-transfer 代抓(curl/wreq/headless 自动升级,绕 Cloudflare)。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的绝对 http(s) URL。" }
            },
            "required": ["url"]
        },
        // MOC-172: readOnlyHint=true → Codex guardian 的 requires_mcp_tool_approval
        // (core/src/mcp_tool_call.rs)命中 read_only_hint 直接返回「不需审批」,只读抓取工具
        // 跳过 auto-review 审批往返、消除联网延迟;destructiveHint=false 确保不被强制审批
        // (destructive=true 优先级最高会触发审批)。openWorldHint=true 如实声明访问开放网络。
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": true }
    })
}

fn read_url_local_tool_def() -> Value {
    json!({
        "name": "read_url_local",
        "title": "Read URL (local cache)",
        "description": "取之前用 web_fetch 抓过的某 URL 的**完整正文**(从本地缓存)。当较早抓取的内容在对话历史里被折叠 / 压缩、你需要回看完整原文时用它,避免重新联网抓取。仅 url 一个参数。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要读取的绝对 http(s) URL(需与之前 web_fetch 时一致)。" }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false }
    })
}

fn read_tool_artifact_tool_def() -> Value {
    json!({
        "name": "read_tool_artifact",
        "title": "Read tool artifact (full output)",
        "description": "取回**任意**被折叠工具输出的完整原文(从本地存储)。当某条历史工具输出(shell / 飞书等 MCP / 其它)在对话里被压成 `[Tool output stored outside model context]` 摘要、你需要其完整内容时,用摘要里给出的 `Artifact ID` 调用本工具,不要为此重跑原工具。不联网、瞬时返回;取回内容仅供当前轮阅读,之后会再被自动折叠。超大输出会分页返回(每次约 90k 字符),返回末尾会提示用 `offset` 续读下一块,逐块可取完整。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "artifact_id": { "type": "string", "description": "要取回的 Artifact ID(取自历史里 `[Tool output stored outside model context]` 摘要的 `Artifact ID:` 行)。" },
                "offset": { "type": "integer", "description": "起始字符偏移(默认 0 从头读)。仅当上次返回末尾提示「用 offset=N 续读」时,传该 N 取下一块。" }
            },
            "required": ["artifact_id"]
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false }
    })
}

fn web_search_tool_def() -> Value {
    json!({
        "name": "web_search",
        "title": "Web Search",
        "description": "用 DuckDuckGo + Bing 双引擎合并搜索一个查询, 返回**第 1 页**结构化结果(标题 + URL + 摘要, 已跨引擎去重)。\
    拿到结果后用 web_fetch 抓你需要的 URL 取正文 —— 两段式: 先 search 找信息源, 再 fetch 读内容。\
    **不知道确切 URL 时用它, 别瞎猜 URL**(尤其官方文档 / 帮助中心 / 论坛帖)。第 1 页(约一二十条)没覆盖到要的信息时, 用 `web_search_more`(传同一 query + page=2)取下一批新结果, 别用同样 query 重复本工具。由 codex-app-transfer \
    经 headless 浏览器代搜(免 key, 不依赖当前 provider 是否支持原生 web_search)。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "搜索关键词 / 问题(如『codex cli 0.136 release notes』『openai 退款政策』)。"
                },
                "max_results": {
                    "type": "integer",
                    "description": "返回结果数上限(默认 15, 最多 30)。",
                    "minimum": 1,
                    "maximum": 30
                }
            },
            "required": ["query"]
        },
        // MOC-172: 同 web_fetch —— 只读搜索工具,readOnlyHint 让 guardian 跳过审批。
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": true }
    })
}

/// MOC-215: web_search 的**独立翻页工具**。单独成 tool(而非 web_search 的 page 参数)是因为模型
/// 几乎不会主动用搜索工具的分页参数 —— 一个名字明确、描述完整的独立工具发现性更高, 配合 web_search
/// 结果尾部的诱导提示, 模型在第 1 页不够时才会真去翻页。单页只抓一页(~10-15 条, 不一次扩抓多页避免
/// headless 延迟过高), 深页按需取。
fn web_search_more_tool_def() -> Value {
    json!({
        "name": "web_search_more",
        "title": "Web Search — 下一页",
        "description": "取 **web_search 同一个 query 的下一页**结果(更多、不重复第 1 页的来源)。\
    用法: 先用 `web_search` 搜得到第 1 页;若第 1 页没覆盖到你要的信息、需要更多来源, **不要用同样的 \
    query 再调 web_search(会返回同一批结果)**, 改用本工具 —— 传**与刚才完全相同的 query** + 目标页码 \
    `page`(从 2 开始, 2=第 2 页、3=第 3 页…)。每页约 10-15 条新结果。拿到结果后照样用 web_fetch 抓 URL \
    取正文。由 codex-app-transfer 经 headless 浏览器代搜(免 key, 不依赖当前 provider 是否支持原生 web_search)。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "与之前 web_search 用的**完全相同**的查询词(必须一致, 翻的是这个 query 的后续页)。"
                },
                "page": {
                    "type": "integer",
                    "description": "要取的页码, 从 2 开始(2=第 2 页, 3=第 3 页…)。第 1 页用 web_search。",
                    "minimum": 2
                },
                "max_results": {
                    "type": "integer",
                    "description": "本页返回结果数上限(默认 15, 最多 30)。",
                    "minimum": 1,
                    "maximum": 30
                }
            },
            "required": ["query", "page"]
        },
        // 只读搜索工具(同 web_search): readOnlyHint 让 guardian 跳过审批。
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": true }
    })
}

/// 按字符截断(非字节, 防截断多字节 UTF-8)。超限时尽量退到最近的换行边界, 避免从句中 /
/// 词中硬切(markdown 段落更完整);仅当该边界离上限不太远(末 1/4 内)才退, 否则宁可硬切
/// 也不浪费过多预算。提示语引导模型抓更具体子页, 而非反复抓同一巨页。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // 给截断提示留出空间, 保证「正文 + 提示」总长 ≤ max —— 否则正好卡满上限的页会因这点溢出, 被上层
    // adapter 的 keep-full 上限(TOOL_OUTPUT_KEEP_FULL_MAX_CHARS, 同为 100k)判超限、把整条当前轮全文
    // bound 掉(chatgpt-codex P2)。max 极小时(单测)放不下提示, 退化为不预留。
    const NOTICE_RESERVE: usize = 160;
    let body_budget = if max > NOTICE_RESERVE * 2 {
        max - NOTICE_RESERVE
    } else {
        max
    };
    let mut cut: String = s.chars().take(body_budget).collect();
    // 退到最后一个换行边界(就近, 浪费不超过 1/4 预算时才退)。
    if let Some(i) = cut.rfind('\n') {
        if i >= cut.len() * 3 / 4 {
            cut.truncate(i);
        }
    }
    format!("{cut}\n\n[... 内容超过 {max} 字符上限已截断, 后续内容未包含; 若该页有分章/分节, 可 web_fetch 更具体的子页 URL ...]")
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
        // MOC-190: 仅 url 必填(默认返原文); MOC-227: summarize 摘要兜底移除后仅 url 一个参数。
        assert_eq!(d["inputSchema"]["required"][0], "url");
        assert_eq!(d["inputSchema"]["required"].as_array().unwrap().len(), 1);
        assert_eq!(d["inputSchema"]["properties"]["url"]["type"], "string");
        assert!(d["inputSchema"]["properties"]["query"].is_null());
        assert!(d["inputSchema"]["properties"]["summarize"].is_null());
        assert!(d["inputSchema"]["properties"]["prompt"].is_null());
        // MOC-172: readOnlyHint=true / destructiveHint=false 让 guardian 跳过 auto-review 审批。
        assert_eq!(d["annotations"]["readOnlyHint"], true);
        assert_eq!(d["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn read_tool_artifact_tool_def_shape() {
        // MOC-235: 通用回取工具 —— 仅 artifact_id 必填(offset 可选), readOnlyHint=true 跳过审批。
        let d = read_tool_artifact_tool_def();
        assert_eq!(d["name"], "read_tool_artifact");
        assert_eq!(d["inputSchema"]["required"][0], "artifact_id");
        assert_eq!(d["inputSchema"]["required"].as_array().unwrap().len(), 1);
        assert_eq!(
            d["inputSchema"]["properties"]["artifact_id"]["type"],
            "string"
        );
        // offset 可选(分页续读), 不在 required 里。
        assert_eq!(d["inputSchema"]["properties"]["offset"]["type"], "integer");
        assert_eq!(d["annotations"]["readOnlyHint"], true);
        assert_eq!(d["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn artifact_chunk_body_paginates_under_keep_full_cap() {
        // 小 artifact: 一次全文, 无续读提示。
        let (body, n) = artifact_chunk_body("short content", 0).unwrap();
        assert_eq!(body, "short content");
        assert_eq!(n, "short content".chars().count());

        // 超 ARTIFACT_CHUNK_CHARS: 首块 = cap 大小, 带续读提示, 且整体 < proxy keep-full 100k
        // (否则回传会被 keep_recent_tool_output_full 再压 → 大 artifact 取不全, MOC-235 review #3)。
        let big = "x".repeat(ARTIFACT_CHUNK_CHARS + 5_000);
        let (body0, n0) = artifact_chunk_body(&big, 0).unwrap();
        assert_eq!(n0, ARTIFACT_CHUNK_CHARS);
        assert!(body0.contains(&format!("offset={ARTIFACT_CHUNK_CHARS}")));
        assert!(
            body0.chars().count() < 100_000,
            "首块含提示也必须 < keep-full 100k, 否则回传被再压"
        );

        // 续读: offset=cap 取剩余, 无续读提示(已到尾)。
        let (body1, n1) = artifact_chunk_body(&big, ARTIFACT_CHUNK_CHARS).unwrap();
        assert_eq!(n1, 5_000);
        assert!(!body1.contains("再次调用"));

        // offset 越界 → Err。
        assert!(artifact_chunk_body("abc", 99).is_err());
    }

    #[test]
    fn cache_in_roundtrip_update_and_cap() {
        let mut m: std::collections::HashMap<String, CachedDoc> = std::collections::HashMap::new();
        assert!(cache_get_in(&mut m, "u1").is_none(), "初始未命中");
        cache_put_in(&mut m, "u1".into(), "正文".into());
        assert_eq!(cache_get_in(&mut m, "u1").as_deref(), Some("正文"));
        // 同 key 重复 put = 更新, 不增容量。
        cache_put_in(&mut m, "u1".into(), "正文2".into());
        assert_eq!(cache_get_in(&mut m, "u1").as_deref(), Some("正文2"));
        assert_eq!(m.len(), 1);
        // 写满 CAP+ 后容量不超 CAP(最旧被驱逐)。
        for i in 0..(FETCH_CACHE_CAP + 5) {
            cache_put_in(&mut m, format!("k{i}"), format!("c{i}"));
        }
        assert!(m.len() <= FETCH_CACHE_CAP, "容量应 ≤ CAP, 实际 {}", m.len());
    }

    #[test]
    fn server_info_icon_is_valid_png_data_uri() {
        // MOC-190 followup: serverInfo.icons 用本应用图标(Codex 折叠工具汇总渲染的小叠层图标)。
        let uri = app_icon_data_uri();
        assert!(
            uri.starts_with("data:image/png;base64,"),
            "应是 PNG data URI, 实际开头: {}",
            &uri[..40.min(uri.len())]
        );
        use base64::Engine as _;
        let b64 = uri.strip_prefix("data:image/png;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("base64 段应可解码");
        // PNG magic number: 89 50 4E 47。
        assert_eq!(&decoded[..4], &[0x89, 0x50, 0x4e, 0x47], "应是合法 PNG");
    }

    #[test]
    fn web_search_tool_def_shape() {
        let d = web_search_tool_def();
        assert_eq!(d["name"], "web_search");
        assert_eq!(d["inputSchema"]["required"][0], "query");
        assert_eq!(d["inputSchema"]["properties"]["query"]["type"], "string");
        assert_eq!(
            d["inputSchema"]["properties"]["max_results"]["type"],
            "integer"
        );
        // MOC-172: readOnlyHint=true / destructiveHint=false 让 guardian 跳过 auto-review 审批。
        assert_eq!(d["annotations"]["readOnlyHint"], true);
        assert_eq!(d["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn web_search_more_tool_def_shape() {
        let d = web_search_more_tool_def();
        assert_eq!(d["name"], "web_search_more");
        let req = d["inputSchema"]["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "query"));
        assert!(req.iter().any(|v| v == "page"));
        // page 从 2 起(第 1 页走 web_search)
        assert_eq!(d["inputSchema"]["properties"]["page"]["minimum"], 2);
        assert_eq!(d["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn format_search_results_shape() {
        let results = vec![codex_app_transfer_http::SearchResult {
            title: "T".into(),
            url: "https://e.com".into(),
            snippet: "S".into(),
        }];
        let out = format_search_results("q", &results, 1);
        assert!(out.contains("https://e.com"));
        assert!(out.contains("web_fetch")); // 带两段式用法提示
        assert!(out.contains("web_search_more")); // 尾部翻页诱导(MOC-215)
    }

    #[test]
    fn arg_usize_lenient_parses_string_and_number() {
        let args = json!({"a": 2, "b": "3", "c": 2.0, "d": " 5 ", "e": "x"});
        assert_eq!(arg_usize(&args, "a"), Some(2)); // JSON number
        assert_eq!(arg_usize(&args, "b"), Some(3)); // 数字字符串(MiMo 实测 page="2")
        assert_eq!(arg_usize(&args, "c"), Some(2)); // 浮点 2.0
        assert_eq!(arg_usize(&args, "d"), Some(5)); // 带空格字符串
        assert_eq!(arg_usize(&args, "e"), None); // 非数字字符串
        assert_eq!(arg_usize(&args, "missing"), None);
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
        // 只看正文部分(截断标记前): 退到换行边界、不含半行 b。标记文案本身可能含 b(如 web_fetch),
        // 不纳入判定。
        let body = t.split("\n\n[").next().unwrap_or(&t);
        assert!(!body.contains('b'), "正文应退到换行边界、不含半行 b: {t}");
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
