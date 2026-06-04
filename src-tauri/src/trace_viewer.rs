//! 诊断流量查看器服务(MOC-169 增量 2)。
//!
//! 把 [`codex_app_transfer_proxy::trace_store`] 的记录在一个**独立本地端口**
//! (默认 `127.0.0.1:18090`)以网页 + SSE 实时展示。**为什么独立端口**:admin 走 Tauri
//! `cas://`,`handle_cas_request` 用 `to_bytes(body, usize::MAX)` 一次性 buffer 响应,
//! hold 不住 SSE 长连接;独立 axum 服务才能做实时流。
//!
//! 结构**照搬** [`crate::proxy_runner::ProxyManager`]:独立 `std::thread` + 独立
//! `tokio::runtime::Runtime`,stop 时 `shutdown_background()` 一键 abort。无鉴权
//! (loopback + 只读 + store 内已脱敏)。**默认关**:仅在 `CAS_DIAG_TRACE` 开 / app 内
//! 「诊断模式」开关(后续增量)时才 start。

use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::Mutex;

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, Json};
use axum::routing::{get, post};
use axum::Router;
use codex_app_transfer_proxy::diagnostics::set_forward_trace_enabled;
use codex_app_transfer_proxy::trace_store;
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
            return Ok(guard.as_ref().unwrap().addr);
        }
        *guard = Some(ViewerHandle { addr, runtime });
        // gate 仅在 viewer 确认运行后开(start 失败不设 → 无残留,满足 P1);与 stop 同在
        // start_lock 内设置,使「gate + viewer」原子一致、按锁顺序串行(并发 on/off 最后一次胜)。
        set_forward_trace_enabled(true);
        Ok(addr)
    }

    /// 静默 stop:app exit / 诊断关 / 异常路径。**与 start 同走 start_lock 串行化**:并发
    /// 的 in-flight start(还在 bootstrap bind)会先完成(装 handle + gate on),stop 再拿锁
    /// 关掉它 + gate off,避免 stop 撞 handle==None 空跑、留 orphan viewer(codex-connector)。
    pub fn stop_silent(&self) {
        let _start_guard = self.start_lock.lock().unwrap();
        set_forward_trace_enabled(false);
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

fn viewer_router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/traces", get(api_traces))
        .route("/api/stream", get(api_stream))
        .route("/api/clear", post(api_clear))
}

/// 单页 viewer(零外部依赖,inline CSS/JS,编进二进制)。
async fn index() -> Html<&'static str> {
    Html(include_str!("../resources/trace_viewer.html"))
}

/// 历史快照:ring 里最近 [`TRACES_HISTORY_LIMIT`] 条(已脱敏的 value 数组)。
async fn api_traces() -> Json<Vec<Value>> {
    let entries = trace_store().recent(TRACES_HISTORY_LIMIT);
    Json(entries.iter().map(|e| e.value.clone()).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_transfer_proxy::{trace_store, TraceKind};
    use serde_json::json;

    #[tokio::test]
    async fn viewer_serves_history_html_and_clear() {
        let manager = TraceViewerManager::new();
        let addr = manager.start(0).expect("viewer start");

        // 推一条带唯一标记的记录进全局 store(进程共享,故用标记搜索而非断言总数)。
        let marker = "moc169-viewer-test-marker-7x";
        trace_store().push(
            TraceKind::Forward,
            999_001,
            json!({"trace_kind": "forward_protocol", "seq": 999_001, "marker": marker}),
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let base = format!("http://{addr}");

        // GET / → HTML 外壳
        let html = client.get(&base).send().await.unwrap();
        assert_eq!(html.status().as_u16(), 200);
        let body = html.text().await.unwrap();
        assert!(body.contains("<!doctype html") || body.contains("<!DOCTYPE html"));

        // GET /api/traces → 能找到刚推的标记记录
        let traces = client
            .get(format!("{base}/api/traces"))
            .send()
            .await
            .unwrap();
        assert_eq!(traces.status().as_u16(), 200);
        let arr: Vec<Value> = traces.json().await.unwrap();
        assert!(
            arr.iter().any(|v| v["marker"] == marker),
            "标记记录应出现在 /api/traces"
        );

        // POST /api/clear → 204,之后 ring 空(标记记录不再出现)
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
            !after.iter().any(|v| v["marker"] == marker),
            "clear 后标记记录应消失"
        );

        manager.stop_silent();
    }
}
