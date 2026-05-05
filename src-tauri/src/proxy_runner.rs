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
//! - **生命周期**:`start` 时 spawn tokio task 持有 `axum::serve` future,附带
//!   `with_graceful_shutdown(oneshot::Receiver<()>)`;`stop` / `stop_silent`
//!   通过 `oneshot::Sender::send(())` 触发 graceful 关停。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use codex_app_transfer_proxy::{build_router, StaticResolver};
use codex_app_transfer_registry::{config_file, Config};
use serde::Serialize;
use tokio::net::TcpListener;
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
    shutdown_tx: oneshot::Sender<()>,
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
            .map_err(|e| format!("绑定 127.0.0.1:{port} 失败: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("无法读取 listener 地址: {e}"))?;
        let router = build_router(Arc::new(snapshot.resolver));
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });

        // 3. 落盘 handle(短锁;若期间被另一路径插入,关掉自己回滚)
        let new_handle = ProxyHandle {
            addr,
            shutdown_tx: tx,
            gateway_auth: snapshot.gateway_auth,
            provider_count: snapshot.provider_count,
            active_provider: snapshot.active_provider.clone(),
        };
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            // race condition,自己的 listener 让出去:发 shutdown 给自己再报错
            let _ = new_handle.shutdown_tx.send(());
            return Err("代理已被其它路径启动".to_owned());
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

    /// 触发 graceful shutdown。未 running 时报错。
    #[allow(dead_code)]
    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.handle.lock().unwrap();
        match guard.take() {
            Some(h) => {
                let _ = h.shutdown_tx.send(());
                Ok(())
            }
            None => Err("代理未在运行".to_owned()),
        }
    }

    /// 静默 stop:用于 app exit / 异常路径,不报错只尽力关。
    pub fn stop_silent(&self) {
        let mut guard = self.handle.lock().unwrap();
        if let Some(h) = guard.take() {
            let _ = h.shutdown_tx.send(());
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
    let path = config_file().ok_or_else(|| "无法定位配置目录".to_owned())?;
    if !path.exists() {
        return Err(
            "配置文件 ~/.codex-app-transfer/config.json 不存在;请先到「提供商」页添加".to_owned(),
        );
    }
    let s = std::fs::read_to_string(&path).map_err(|e| format!("读取 config.json 失败: {e}"))?;
    let cfg: Config =
        serde_json::from_str(&s).map_err(|e| format!("解析 config.json 失败: {e}"))?;
    if cfg.providers.is_empty() {
        return Err("当前没有任何 provider,请先添加".to_owned());
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
