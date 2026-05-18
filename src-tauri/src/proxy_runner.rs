//! 内嵌 axum 代理生命周期管理(Stage 4.3 + Stage 5).
//!
//! Tauri 主进程启动时构造一个 [`ProxyManager`] 注入到 `State<T>`,前端通过
//! `start_proxy` / `stop_proxy` / `proxy_status` 命令操控,Tauri 主进程
//! 退出时通过 [`ProxyManager::stop_silent`] **同步**关闭代理。
//!
//! 设计要点:
//! - 内部 `std::sync::Mutex<Option<ProxyHandle>>` —— 锁持有时间极短(只读/写
//!   单个 Option),没有跨 await,**stop / status / stop_silent 全部是同步方法**,
//!   方便从 Tauri 的 `RunEvent::Exit` 同步路径调用而不需要 `block_on`。
//! - **`start` 是 async**(TcpListener::bind 必需),但锁取放都在显式 scope 里,
//!   不跨越 await。
//! - **生命周期**:`start` 时 spawn tokio task 跑自己写的 accept loop —
//!   每个 connection 用 `JoinSet.spawn` 而**不**是裸 `tokio::spawn`(关键区别)。
//!   `stop` / `stop_silent` 通过 `oneshot::Sender::send(())` 触发 select 退 →
//!   listener drop(端口立即 free)→ `joinset.abort_all()` 强制 cancel **所有**
//!   in-flight connection sub-task → reqwest stream / SSE 同步被打断 →
//!   client 收 FIN/RST connection 立即断。
//! - **为什么不用 `axum::serve`**:它内部对每个 connection 用裸 `tokio::spawn`
//!   出 detached task,外部无 handle,即使 outer future drop sub-task 仍 alive
//!   继续 process 同 connection 上的 keep-alive request → 用户感知"停止后还
//!   在转发"(2026-05-18 真机复现:发过 1 条 message 后 stop 转发不受影响,
//!   PR #209 select! 方案不够强制)。`JoinSet.abort_all()` 是唯一保证所有
//!   in-flight connection 同步 abort 的方式。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use codex_app_transfer_proxy::{build_router_with_gate, StaticResolver};
use codex_app_transfer_registry::{config_file, Config};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[derive(Debug, Serialize, Clone)]
pub struct ProxyStatus {
    pub running: bool,
    pub addr: Option<String>,
    /// 当前生效的 gateway 鉴权状态 —— 仅当代理 running 且配置了 gateway_api_key
    /// 时才是 `true`;running 但未配 key 表示"无鉴权调试模式"。
    pub gateway_auth: bool,
    pub provider_count: usize,
    pub active_provider: Option<String>,
}

struct ProxyHandle {
    addr: SocketAddr,
    /// 停止信号 — `stop` / `stop_silent` send 给跑着的 accept-loop task,
    /// task 收到后 break accept loop → drop listener(端口 free)→ 跑
    /// `connections.shutdown().await` 等所有 sub-task die。
    shutdown_tx: oneshot::Sender<()>,
    /// **关键** in-flight connection cancel token —— 每个 connection sub-task
    /// 在 `tokio::select! { conn.await, cancel.cancelled().await }` 里跑;
    /// stop_silent 调 `cancel.cancel()` 同步 wake 所有 sub-task 的 cancelled
    /// arm → select arm 命中 → drop conn future → hyper connection drop →
    /// **TCP socket 同步 close → client 收 FIN/RST 立即断**。
    ///
    /// 为什么不能只靠 `task.abort()`:tokio abort 是 schedule cancellation,
    /// task 必须在 yield point 才生效。reqwest stream / SSE await upstream
    /// response 可能很久不 yield,abort 滞后到数秒甚至永远。
    /// `CancellationToken.cancel()` 立刻 wake 所有 `cancelled()` listener,
    /// select arm 立即 ready 触发 drop,不依赖 sub-task 主动 yield。
    cancel: CancellationToken,
    /// accept-loop task 的 JoinHandle —— 主路径靠 shutdown_tx + cancel 协同
    /// 让 task 自然跑完(break loop + drop listener + 等 sub-task die),
    /// **stop_silent 不调 task.abort()**(否则会抢先 cancel task,task 跑不
    /// 到 connections.shutdown().await,sub-task cancel 链断)。
    ///
    /// 历史踩坑(2026-05-18):
    /// - PR #207 task.abort:同上,reqwest stream 不 yield → abort 永远等
    /// - PR #209 select! + axum::serve:future drop 不传到 axum 内裸 spawn
    ///   出的 connection sub-task
    /// - PR #209 第二 commit JoinSet + abort_all:对了一半 —— JoinSet
    ///   拿到 handle 了,**但 stop_silent 同时调 task.abort() 抢先 cancel
    ///   task,task 跑不到 connections.shutdown() → sub-task 仍 detached**
    ///   (2026-05-18 真机第二次复现根因)
    task: JoinHandle<()>,
    /// Application-level shutdown gate(in-flight request 三保险):cancel
    /// → sub-task select wake → conn drop 有微小窗口,gate 在 stop 时同步
    /// set true,router 顶层 middleware 检查后立刻 503 + Connection: close。
    gate: Arc<AtomicBool>,
    gateway_auth: bool,
    provider_count: usize,
    active_provider: Option<String>,
}

#[derive(Default)]
pub struct ProxyManager {
    handle: Mutex<Option<ProxyHandle>>,
}

impl ProxyManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 启动代理监听 `127.0.0.1:<port>`。已 running 时沿用旧版语义返回当前状态。
    pub async fn start(&self, port: u16) -> Result<ProxyStatus, String> {
        // 1. 预检查(短锁)
        {
            let guard = self.handle.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                return Ok(ProxyStatus {
                    running: true,
                    addr: Some(h.addr.to_string()),
                    gateway_auth: h.gateway_auth,
                    provider_count: h.provider_count,
                    active_provider: h.active_provider.clone(),
                });
            }
        }

        // 2. 装载 resolver + 绑定 listener(async)
        let snapshot = load_resolver_snapshot()?;
        let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .map_err(|e| format!("bind 127.0.0.1:{port} failed: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("cannot read listener address: {e}"))?;
        // Application-level gate:start 时 false(放行),stop 时 set true →
        // router 顶层 middleware 检查后 503 + close 任何 in-flight sub-task
        // 的后续 request。
        let gate = Arc::new(AtomicBool::new(false));
        let router = build_router_with_gate(Arc::new(snapshot.resolver), gate.clone());
        let (tx, mut rx) = oneshot::channel::<()>();
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            // 自己写 accept loop 替代 axum::serve,真正强制释放方案:
            //
            // 1. per-connection task 用 `JoinSet.spawn`(拿 handle)+ **每个
            //    sub-task 内 select { conn.await, cancel.cancelled() }**
            //    —— cancel 来时 select arm 立刻 wake,drop conn future →
            //    hyper connection drop → **TCP socket 同步 close** →
            //    client 收 FIN/RST 立即断
            //
            // 2. stop_silent 调 `cancel.cancel()` 同步触发所有 sub-task →
            //    sub-task naturally complete → connections.shutdown() 返
            //
            // 3. 关键:**stop_silent 不调 task.abort()** —— 之前两个 commit
            //    fail 在这里(task.abort 抢先 cancel server task,task 跑不
            //    到 connections.shutdown(),sub-task 仍 detached)
            //
            // 真机复现历史(2026-05-18):
            // - PR #207 task.abort:reqwest stream 不 yield → abort 永远等
            // - PR #209 select! + axum::serve:future drop 不传到 axum 内
            //   裸 spawn 的 sub-task
            // - PR #209 JoinSet + abort_all without cancel:task.abort 抢先,
            //   shutdown() 跑不到
            let mut connections: JoinSet<()> = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut rx => break,
                    accept = listener.accept() => {
                        let Ok((stream, _peer)) = accept else { continue };
                        // **强制 close socket 三保险**:dup 一份 socket fd
                        // 给 sub-task 持有,cancel 时直接调 `shutdown(SHUT_RDWR)`
                        // OS-level 强制 close。**经验**:hyper conn future drop
                        // 不可靠让 socket 真 close(2026-05-18 真机第三次复现:
                        // listener drop ✓ 但 ESTABLISHED 持续不消失);raw fd
                        // shutdown 绕开 hyper / tokio 间接 cancellation,直接
                        // 让 OS kernel 发 FIN/RST。
                        #[cfg(unix)]
                        let dup_fd = {
                            use std::os::fd::AsRawFd;
                            let raw = stream.as_raw_fd();
                            let dup = unsafe { libc::dup(raw) };
                            if dup < 0 { None } else { Some(dup) }
                        };
                        #[cfg(not(unix))]
                        let dup_fd: Option<i32> = None; // TODO: Windows WSADuplicateSocket
                        let io = TokioIo::new(stream);
                        let tower_service = router.clone().map_request(
                            |req: Request<Incoming>| req.map(Body::new),
                        );
                        let hyper_service = TowerToHyperService::new(tower_service);
                        let conn_cancel = cancel_for_task.child_token();
                        connections.spawn(async move {
                            let builder = Builder::new(TokioExecutor::new());
                            let conn = builder.serve_connection_with_upgrades(io, hyper_service);
                            tokio::pin!(conn);
                            tokio::select! {
                                _ = conn.as_mut() => {}
                                _ = conn_cancel.cancelled() => {
                                    #[cfg(unix)]
                                    if let Some(fd) = dup_fd {
                                        // OS-level shutdown:client 立即收 FIN/RST
                                        unsafe { libc::shutdown(fd, libc::SHUT_RDWR); }
                                    }
                                }
                            }
                            // close dup fd(无论哪个 arm 走完都要释放)
                            #[cfg(unix)]
                            if let Some(fd) = dup_fd {
                                unsafe { libc::close(fd); }
                            }
                        });
                    }
                }
            }
            drop(listener); // 端口立即 free
            connections.shutdown().await; // cancel 已触发,sub-task 应快速 die
        });

        // 3. 落盘 handle(短锁;若期间被另一路径插入,关掉自己回滚)
        let new_handle = ProxyHandle {
            addr,
            shutdown_tx: tx,
            cancel,
            task,
            gate,
            gateway_auth: snapshot.gateway_auth,
            provider_count: snapshot.provider_count,
            active_provider: snapshot.active_provider.clone(),
        };
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            // race condition,自己的 listener 让出去:gate + cancel + signal,
            // task 自然跑完 connections.shutdown() 释放端口。
            new_handle.gate.store(true, Ordering::Release);
            new_handle.cancel.cancel();
            let _ = new_handle.shutdown_tx.send(());
            new_handle.task.abort(); // race 路径用 abort 兜底
            return Err("proxy already started by another path".to_owned());
        }
        *guard = Some(new_handle);
        Ok(ProxyStatus {
            running: true,
            addr: Some(addr.to_string()),
            gateway_auth: snapshot.gateway_auth,
            provider_count: snapshot.provider_count,
            active_provider: snapshot.active_provider,
        })
    }

    /// 强制停转发(同步 fn,fire-and-forget,不等 task 自然完成)。
    ///
    /// 顺序(每步同步立即生效):
    /// 1. `gate.store(true)` —— in-flight middleware 立刻 503
    /// 2. `cancel.cancel()` —— 所有 connection sub-task 的 select arm
    ///    立刻 wake → drop conn → **socket close**(client 收 FIN/RST)
    /// 3. `shutdown_tx.send(())` —— 唤醒 accept loop break → drop listener
    ///    → 端口 free
    ///
    /// **不**调 `task.abort()` —— 否则会抢先 cancel server task,task 跑不
    /// 到 `connections.shutdown().await`,sub-task cancel 链断(2026-05-18
    /// PR #209 第二 commit fail 根因)。
    #[allow(dead_code)]
    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.handle.lock().unwrap();
        match guard.take() {
            Some(h) => {
                h.gate.store(true, Ordering::Release);
                h.cancel.cancel();
                let _ = h.shutdown_tx.send(());
                drop(h.task); // 让 task 自然完成,不 abort
                Ok(())
            }
            None => Err("proxy is not running".to_owned()),
        }
    }

    /// 静默 stop:app exit / 异常路径用,不报错只尽力关。同样走 gate +
    /// cancel + signal 三步同步触发,**不**调 task.abort(详见 [`Self::stop`])。
    pub fn stop_silent(&self) {
        let mut guard = self.handle.lock().unwrap();
        if let Some(h) = guard.take() {
            h.gate.store(true, Ordering::Release);
            h.cancel.cancel();
            let _ = h.shutdown_tx.send(());
            drop(h.task); // 让 task 自然完成,不 abort
        }
    }

    pub fn status(&self) -> ProxyStatus {
        let guard = self.handle.lock().unwrap();
        match guard.as_ref() {
            Some(h) => ProxyStatus {
                running: true,
                addr: Some(h.addr.to_string()),
                gateway_auth: h.gateway_auth,
                provider_count: h.provider_count,
                active_provider: h.active_provider.clone(),
            },
            None => ProxyStatus {
                running: false,
                addr: None,
                gateway_auth: false,
                provider_count: 0,
                active_provider: None,
            },
        }
    }
}

struct ResolverSnapshot {
    resolver: StaticResolver,
    gateway_auth: bool,
    provider_count: usize,
    active_provider: Option<String>,
}

fn load_resolver_snapshot() -> Result<ResolverSnapshot, String> {
    let path = config_file().ok_or_else(|| "cannot locate config directory".to_owned())?;
    if !path.exists() {
        return Err(
            "config file ~/.codex-app-transfer/config.json does not exist; add a provider on the Providers page first".to_owned(),
        );
    }
    let s = std::fs::read_to_string(&path).map_err(|e| format!("read config.json failed: {e}"))?;
    // 先 raw Value 解析 + healing(强制覆盖 builtin provider 的 apiFormat /
    // authScheme / extraHeaders),再转 typed Config。proxy 这条路径**不写回
    // 磁盘**(避免与 admin 路径写盘竞争),仅在内存中保证当前 resolver 拿到
    // 修过的配置;真正的盘写入由 admin/registry_io.rs::load 在用户打开应用
    // 时触发。详见 registry::healing 模块说明。
    let mut raw: serde_json::Value =
        serde_json::from_str(&s).map_err(|e| format!("parse config.json failed: {e}"))?;
    codex_app_transfer_registry::heal_builtin_provider_fields(&mut raw);
    let cfg: Config =
        serde_json::from_value(raw).map_err(|e| format!("config.json schema mismatch: {e}"))?;
    if cfg.providers.is_empty() {
        return Err("no providers configured; add one first".to_owned());
    }
    let gateway_key = cfg.gateway_api_key.filter(|s| !s.is_empty());
    let gateway_auth = gateway_key.is_some();
    Ok(ResolverSnapshot {
        provider_count: cfg.providers.len(),
        active_provider: cfg.active_provider.clone(),
        resolver: StaticResolver::new(gateway_key, cfg.providers, cfg.active_provider),
        gateway_auth,
    })
}
