//! 内嵌 axum 代理生命周期管理。
//!
//! **核心设计**:proxy 跑在**独立 `std::thread` + 独立 `tokio::runtime::Runtime`**。
//! stop 时把整个 Runtime drop(`shutdown_background()`)——
//! - 所有 spawn 在 runtime 上的 task **同步 abort**
//! - worker thread 退出 → 没人 poll task → task drop
//! - task 持有的 `TcpStream` / `TcpListener` 跟着 drop → fd close
//! - **所有 proxy 相关功能一锅端,只保留 Tauri 主界面**
//!
//! 不再使用 CancellationToken / JoinSet / 自己写 accept loop / raw fd shutdown /
//! application-level gate middleware 等"兜底逻辑"—— `Runtime::shutdown_background`
//! 是 tokio 提供的 OS-level "杀光所有 task" 原语,不需要 user-space cancel chain。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use codex_app_transfer_proxy::{build_router, StaticResolver};
use codex_app_transfer_registry::{config_file, Config};
use serde::Serialize;
use tokio::sync::oneshot;

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
    /// **核心**:proxy 跑在这个独立 runtime 上,stop_silent 时调
    /// `shutdown_background()` 一键 abort 所有 task + worker thread 退出
    /// → 所有 fd / 资源 cleanup。
    runtime: tokio::runtime::Runtime,
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
        // 1. 预检查
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

        // 2. 装载 resolver
        let snapshot = load_resolver_snapshot()?;

        // 3. 创建 dedicated runtime + 启 server
        //    Runtime::new 不能在 async context 内调,用 std::thread 包。
        //    用 tokio::sync::oneshot 而非 std::sync::mpsc,让 receiver 端 .await
        //    yield Tauri worker thread 而不是同步 block(Devin review fix)。
        let (addr_tx, addr_rx) =
            oneshot::channel::<Result<(SocketAddr, tokio::runtime::Runtime), String>>();
        let resolver = Arc::new(snapshot.resolver);
        std::thread::Builder::new()
            .name(format!("cas-proxy-bootstrap-{port}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("cas-proxy")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = addr_tx.send(Err(format!("create proxy runtime failed: {e}")));
                        return;
                    }
                };
                let bind_result = rt.block_on(async {
                    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
                        .await
                        .map_err(|e| format!("bind 127.0.0.1:{port} failed: {e}"))?;
                    let addr = listener
                        .local_addr()
                        .map_err(|e| format!("cannot read listener address: {e}"))?;
                    let router = build_router(resolver);
                    // 在 runtime 上 spawn server —— 当 runtime shutdown_background
                    // 时此 task 同步被 abort,listener + 所有 connection sub-task
                    // 一起 drop,fd close。
                    rt.spawn(async move {
                        let _ = axum::serve(listener, router.into_make_service()).await;
                    });
                    Ok::<SocketAddr, String>(addr)
                });
                match bind_result {
                    Ok(addr) => {
                        let _ = addr_tx.send(Ok((addr, rt)));
                    }
                    Err(e) => {
                        rt.shutdown_background();
                        let _ = addr_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("spawn proxy thread failed: {e}"))?;

        let (addr, runtime) = addr_rx
            .await
            .map_err(|_| "proxy bootstrap channel closed".to_owned())??;

        // 4. 落盘 handle(短锁;若期间被另一路径插入,关掉自己回滚)
        let new_handle = ProxyHandle {
            addr,
            runtime,
            gateway_auth: snapshot.gateway_auth,
            provider_count: snapshot.provider_count,
            active_provider: snapshot.active_provider.clone(),
        };
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            new_handle.runtime.shutdown_background();
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

    /// 停止转发 —— 一键 drop 整个 dedicated runtime,所有 spawn task 同步 abort,
    /// worker thread 退出,所有 fd / 连接 cleanup,**只保留 Tauri 主界面**。
    ///
    /// `Runtime::shutdown_background` 是 tokio 显式提供的 "from within another
    /// runtime 安全 shutdown" API,不触发 "async context drop runtime" panic
    /// (tokio docs: "useful if you want to drop a runtime from within another
    /// runtime")。所以即使 stop_proxy admin handler 是 async fn 在此调用,
    /// 也无需 std::thread 包装。
    #[allow(dead_code)]
    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.handle.lock().unwrap();
        match guard.take() {
            Some(h) => {
                h.runtime.shutdown_background();
                Ok(())
            }
            None => Err("proxy is not running".to_owned()),
        }
    }

    /// 静默 stop:app exit / 异常路径用,不报错只尽力关。
    pub fn stop_silent(&self) {
        let mut guard = self.handle.lock().unwrap();
        if let Some(h) = guard.take() {
            h.runtime.shutdown_background();
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
