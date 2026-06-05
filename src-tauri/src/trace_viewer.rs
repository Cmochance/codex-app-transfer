//! 诊断流量查看器服务(MOC-169 / MOC-181)。
//!
//! 把 [`codex_app_transfer_proxy::trace_store`] 的记录在一个**独立本地端口**
//! (默认 `127.0.0.1:18090`)以网页 + SSE 实时展示。**为什么独立端口**:admin 走 Tauri
//! `cas://`,`handle_cas_request` 用 `to_bytes(body, usize::MAX)` 一次性 buffer 响应,
//! hold 不住 SSE 长连接;独立 axum 服务才能做实时流。
//!
//! 结构**照搬** [`crate::proxy_runner::ProxyManager`]:独立 `std::thread` + 独立
//! `tokio::runtime::Runtime`,stop 时 `shutdown_background()` 一键 abort。无鉴权
//! (loopback + 只读 + store 内已脱敏)。**默认关**:仅在 `CAS_DIAG_TRACE` 开 / app 内
//! 「诊断模式」开关时才 start。
//!
//! ## 路由一览
//! - `GET  /`                  — viewer 单页 HTML(inline CSS/JS,无外部依赖)
//! - `GET  /api/traces?kind=`  — 历史快照(`kind=forward/mcp/cat_webfetch/all`,缺省全部)
//! - `GET  /api/stream`        — SSE 实时流(所有类别)
//! - `POST /api/clear`         — 清空 ring
//! - `POST /api/ingest`        — **MOC-181**:cat-webfetch 子进程反向上报内部链路;只接受
//!   `trace_kind=cat_webfetch`,viewer 统一分配全局 seq 后 push 进 store(子进程是独立
//!   stdio 进程,跨进程拿不到主 app 的 store,故经此 HTTP 端点上报)。
//!
//! ## viewer 分页
//! - **forward** — Codex 请求 → adapter → 上游回包(协议转换诊断)
//! - **mcp**     — Codex Desktop MCP / OAuth 流量(依赖插件解锁器 daemon)
//! - **cat-webfetch** — 内置 web_fetch / web_search 每次调用的完整链路(MOC-181):
//!   请求参数 / 抓取后端 + 升级链 + HTTP status / 大页选块统计 /
//!   摘要 prompt + 模型响应 + 延迟 / 返回字符数。供 `GET /api/traces?kind=cat_webfetch`
//!   机读(AI 调试分析)或页面实时查看。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, Json};
use axum::routing::{get, post};
use axum::Router;
use codex_app_transfer_proxy::diagnostics::set_forward_trace_enabled;
use codex_app_transfer_proxy::{trace_store, TraceKind};
use futures::Stream;
use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

/// viewer 默认端口(区隔 proxy 18080)。增量 3 会让它可由 settings 覆盖。
pub const DEFAULT_TRACE_VIEWER_PORT: u16 = 18090;
/// `GET /api/traces` 返回的历史条数上限。
const TRACES_HISTORY_LIMIT: usize = 500;

struct ViewerHandle {
    addr: SocketAddr,
    /// viewer 跑在此独立 runtime 上;stop_silent 时 `shutdown_background()` 一键 abort。
    runtime: tokio::runtime::Runtime,
}

#[derive(Default)]
pub struct TraceViewerManager {
    handle: Mutex<Option<ViewerHandle>>,
    /// 串行化整个 start 序列(bind + 安装 handle),防并发 start 同时过 `handle==None` 检查
    /// 都去 bind 18090 → 输者 bind 失败返假错误(codex-connector:并发 start race)。
    /// 持有它跨同步 bind 是 OK 的——start 经 `spawn_blocking` 在阻塞线程跑;handle/stop/addr
    /// 不碰它,无嵌套锁、无死锁。
    start_lock: Mutex<()>,
}

impl TraceViewerManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 启动 viewer 监听 `127.0.0.1:<port>`。已 running 则返回当前地址(幂等)。
    /// 同步:在独立 bootstrap 线程里 build runtime + bind(快),经 std mpsc 回传结果;
    /// runtime move 进 handle 常驻,bootstrap 线程发完即退。整个序列由 `start_lock` 串行化
    /// → 并发 start 第二个会等第一个装好 handle 后看到 Some、幂等返回,不会二次 bind。
    pub fn start(&self, port: u16) -> Result<SocketAddr, String> {
        let _start_guard = self.start_lock.lock().unwrap();
        {
            let guard = self.handle.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                set_forward_trace_enabled(true);
                write_sentinel(h.addr.port());
                return Ok(h.addr);
            }
        }

        let (tx, rx) = mpsc::channel::<Result<(SocketAddr, tokio::runtime::Runtime), String>>();
        std::thread::Builder::new()
            .name(format!("cas-trace-viewer-bootstrap-{port}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(1)
                    .thread_name("cas-trace-viewer")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(Err(format!("create trace-viewer runtime failed: {e}")));
                        return;
                    }
                };
                let bind = rt.block_on(async {
                    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
                        .await
                        .map_err(|e| format!("bind 127.0.0.1:{port} failed: {e}"))?;
                    let addr = listener
                        .local_addr()
                        .map_err(|e| format!("cannot read viewer listener address: {e}"))?;
                    rt.spawn(async move {
                        let _ = axum::serve(listener, viewer_router().into_make_service()).await;
                    });
                    Ok::<SocketAddr, String>(addr)
                });
                match bind {
                    Ok(addr) => {
                        let _ = tx.send(Ok((addr, rt)));
                    }
                    Err(e) => {
                        rt.shutdown_background();
                        let _ = tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("spawn trace-viewer thread failed: {e}"))?;

        let (addr, runtime) = rx
            .recv()
            .map_err(|_| "trace-viewer bootstrap channel closed".to_owned())??;

        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            runtime.shutdown_background();
            set_forward_trace_enabled(true);
            let a = guard.as_ref().unwrap().addr;
            write_sentinel(a.port());
            return Ok(a);
        }
        *guard = Some(ViewerHandle { addr, runtime });
        // gate 仅在 viewer 确认运行后开(start 失败不设 → 无残留,满足 P1);与 stop 同在
        // start_lock 内设置,使「gate + viewer」原子一致、按锁顺序串行(并发 on/off 最后一次胜)。
        // 同步写 runtime sentinel:cat-webfetch 子进程据此判 viewer 真在跑 + 拿对端口(MOC-181)。
        set_forward_trace_enabled(true);
        write_sentinel(addr.port());
        Ok(addr)
    }

    /// 静默 stop:app exit / 诊断关 / 异常路径。**与 start 同走 start_lock 串行化**:并发
    /// 的 in-flight start(还在 bootstrap bind)会先完成(装 handle + gate on),stop 再拿锁
    /// 关掉它 + gate off,避免 stop 撞 handle==None 空跑、留 orphan viewer(codex-connector)。
    pub fn stop_silent(&self) {
        let _start_guard = self.start_lock.lock().unwrap();
        set_forward_trace_enabled(false);
        remove_sentinel();
        let mut guard = self.handle.lock().unwrap();
        if let Some(h) = guard.take() {
            h.runtime.shutdown_background();
        }
    }

    /// 当前监听地址(未启动返 `None`)。
    pub fn addr(&self) -> Option<SocketAddr> {
        self.handle.lock().unwrap().as_ref().map(|h| h.addr)
    }
}

/// viewer 运行时 sentinel 路径(`{config_dir}/.trace-viewer-runtime.json`)。MOC-181:
/// start 成功写(含实际监听 port)、stop 删 —— cat-webfetch 子进程据此判定 viewer **真在跑**
/// (gate on running viewer 而非持久化 config)+ 拿对端口上报。viewer bind 失败 / app 关时无
/// sentinel = 子进程不上报,避免把诊断数据发给占用固定端口的任意进程(chatgpt-codex P2)。
fn sentinel_path() -> Option<PathBuf> {
    Some(codex_app_transfer_registry::config_dir()?.join(".trace-viewer-runtime.json"))
}

/// 写 sentinel(失败静默 —— 写不出顶多让子进程不上报,不影响 viewer 自身)。
fn write_sentinel(port: u16) {
    if let Some(p) = sentinel_path() {
        let _ = std::fs::write(&p, format!("{{\"port\":{port}}}"));
    }
}

/// 删 sentinel(stop / 诊断关时;失败静默)。
fn remove_sentinel() {
    if let Some(p) = sentinel_path() {
        let _ = std::fs::remove_file(&p);
    }
}

fn viewer_router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/traces", get(api_traces))
        .route("/api/stream", get(api_stream))
        .route("/api/clear", post(api_clear))
        // MOC-181: cat-webfetch 子进程反向上报内部链路(跨进程无法直接 push 本进程 store)。
        .route("/api/ingest", post(api_ingest))
        // MOC-181: 身份探测 —— cat-webfetch 上报前 GET 此端点确认该端口上真是本 viewer
        // (sentinel 残留 / 端口被别的进程占时拒发敏感数据, chatgpt-codex P2)。
        .route("/api/health", get(api_health))
}

/// 单页 viewer(零外部依赖,inline CSS/JS,编进二进制)。
async fn index() -> Html<&'static str> {
    Html(include_str!("../resources/trace_viewer.html"))
}

/// `GET /api/traces` 查询参数。`kind=cat_webfetch`(或 forward / mcp)只返该类;缺省 / `all` 返全部。
#[derive(serde::Deserialize)]
struct TracesQuery {
    kind: Option<String>,
}

/// `TraceKind` → viewer `?kind=` / select 用的字符串。**不用 `value.trace_kind`**:forward 的
/// trace_kind 是 `"forward_protocol"`(与 select 值 `"forward"` 不一致), 故按类型化的 enum 判别。
fn kind_str(k: TraceKind) -> &'static str {
    match k {
        TraceKind::Forward => "forward",
        TraceKind::Mcp => "mcp",
        TraceKind::CatWebfetch => "cat_webfetch",
    }
}

/// 历史快照:ring 里最近 [`TRACES_HISTORY_LIMIT`] 条(已脱敏的 value 数组)。`?kind=` 按类型化的
/// [`TraceKind`] 过滤 —— 供 viewer 分页 / AI 只拉某类(如 `?kind=cat_webfetch` 取 cat-webfetch
/// 链路做调试分析)。
async fn api_traces(Query(q): Query<TracesQuery>) -> Json<Vec<Value>> {
    let entries = trace_store().recent(TRACES_HISTORY_LIMIT);
    let want = q.kind.as_deref().filter(|k| !k.is_empty() && *k != "all");
    Json(
        entries
            .iter()
            .filter(|e| match want {
                Some(k) => kind_str(e.kind) == k,
                None => true,
            })
            .map(|e| e.value.clone())
            .collect(),
    )
}

/// 实时流(SSE)。订阅 store 的 broadcast;每条记录发一个默认 message(data=已脱敏 value
/// 的 JSON);慢前端落后时 `BroadcastStream` 给 `Lagged(n)` → 发 `event: lagged` 让前端
/// refetch `/api/traces` resync(不静默丢)。`KeepAlive` 周期注释帧防代理/浏览器掐空闲连接。
async fn api_stream() -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = trace_store().subscribe();
    let stream = BroadcastStream::new(rx).map(|res| {
        let event = match res {
            Ok(entry) => {
                Event::default().data(serde_json::to_string(&entry.value).unwrap_or_default())
            }
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                Event::default().event("lagged").data(n.to_string())
            }
        };
        Ok::<_, std::convert::Infallible>(event)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// 清空 ring(不动已落盘 jsonl)。
async fn api_clear() -> StatusCode {
    trace_store().clear();
    StatusCode::NO_CONTENT
}

/// MOC-181: cat-webfetch 子进程反向上报内部链路。子进程是独立 stdio 进程,跨进程拿不到主 app 的
/// `trace_store()` / `next_seq()`,故把结构化 value(无 seq)POST 到此,由 viewer 统一分配全局 seq
/// 后 push(与 forward / mcp 共用单调 seq,viewer 按 seq 做行主键)。**只收 `trace_kind=cat_webfetch`**
/// (防别的本地进程灌噪声);loopback only(viewer 已 bind 127.0.0.1);body 超限由 axum 默认上限兜底。
async fn api_ingest(Json(mut value): Json<Value>) -> StatusCode {
    if value.get("trace_kind").and_then(|v| v.as_str()) != Some("cat_webfetch") {
        return StatusCode::BAD_REQUEST;
    }
    let seq = trace_store::next_seq();
    if let Value::Object(map) = &mut value {
        map.insert("seq".to_owned(), Value::from(seq));
    }
    trace_store().push(TraceKind::CatWebfetch, seq, value);
    StatusCode::NO_CONTENT
}

/// MOC-181: 身份探测端点。cat-webfetch 子进程上报前 GET 此端点, body 含固定 `service` 标识 →
/// 确认 sentinel 指向的端口上真是本 viewer(而非 crash 残留后占用该端口的其它进程), 是才发诊断数据。
async fn api_health() -> Json<Value> {
    Json(serde_json::json!({ "service": "cas-trace-viewer" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_transfer_proxy::{trace_store, TraceKind};
    use serde_json::json;

    /// MOC-169 + MOC-181: viewer 端到端 —— 历史 / ingest(cat-webfetch 子进程上报) / ?kind 过滤 / clear。
    /// **合并为单测**:全局 `trace_store` 进程共享, 拆成多个并发 test 会因 `clear` 互相清数据而 flaky。
    #[tokio::test]
    async fn viewer_history_ingest_filter_and_clear() {
        let manager = TraceViewerManager::new();
        let addr = manager.start(0).expect("viewer start");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let base = format!("http://{addr}");

        // ── 历史(MOC-169): 推一条 forward 记录;GET / 返 HTML 外壳、GET /api/traces 能查到。
        let fwd_marker = "moc169-viewer-test-marker-7x";
        trace_store().push(
            TraceKind::Forward,
            999_001,
            json!({"trace_kind": "forward_protocol", "seq": 999_001, "marker": fwd_marker}),
        );
        let html = client.get(&base).send().await.unwrap();
        assert_eq!(html.status().as_u16(), 200);
        let body = html.text().await.unwrap();
        assert!(body.contains("<!doctype html") || body.contains("<!DOCTYPE html"));
        let arr: Vec<Value> = client
            .get(format!("{base}/api/traces"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            arr.iter().any(|v| v["marker"] == fwd_marker),
            "forward 标记记录应出现在 /api/traces"
        );

        // ── health(MOC-181): cat-webfetch 上报前 GET /api/health 确认身份, body 带固定 service 标识。
        let health = client
            .get(format!("{base}/api/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status().as_u16(), 200);
        assert!(
            health.text().await.unwrap().contains("cas-trace-viewer"),
            "health 应返回 viewer 身份标识"
        );

        // ── ingest(MOC-181): cat-webfetch 子进程 POST 一条(无 seq, viewer 自分配);非 cat_webfetch 被拒。
        let cat_marker = "moc181-ingest-marker-9z";
        let ok = client
            .post(format!("{base}/api/ingest"))
            .json(&json!({"trace_kind": "cat_webfetch", "tool": "web_fetch", "marker": cat_marker}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status().as_u16(), 204);
        let bad = client
            .post(format!("{base}/api/ingest"))
            .json(&json!({"trace_kind": "forward_protocol"}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status().as_u16(), 400, "ingest 只收 cat_webfetch");

        // ── ?kind 过滤(MOC-181): cat_webfetch 含刚 ingest 的(带 viewer 分配 seq)且只含该类;forward 含 fwd、不含 cat。
        let cat: Vec<Value> = client
            .get(format!("{base}/api/traces?kind=cat_webfetch"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let found = cat
            .iter()
            .find(|v| v["marker"] == cat_marker)
            .expect("ingest 的条目应出现在 ?kind=cat_webfetch");
        assert!(found["seq"].is_number(), "viewer 应给 ingest 条目分配 seq");
        assert!(
            cat.iter().all(|v| v["trace_kind"] == "cat_webfetch"),
            "?kind=cat_webfetch 应只返 cat_webfetch"
        );
        let fwd: Vec<Value> = client
            .get(format!("{base}/api/traces?kind=forward"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            fwd.iter().any(|v| v["marker"] == fwd_marker)
                && !fwd.iter().any(|v| v["marker"] == cat_marker),
            "?kind=forward 应含 forward、不含 cat_webfetch"
        );

        // ── clear(MOC-169): 清空 ring 后两条标记都消失。
        let cleared = client
            .post(format!("{base}/api/clear"))
            .send()
            .await
            .unwrap();
        assert_eq!(cleared.status().as_u16(), 204);
        let after: Vec<Value> = client
            .get(format!("{base}/api/traces"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !after
                .iter()
                .any(|v| v["marker"] == fwd_marker || v["marker"] == cat_marker),
            "clear 后标记记录应消失"
        );

        manager.stop_silent();
    }
}
