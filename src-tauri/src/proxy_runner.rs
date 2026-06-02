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

use crate::admin::handlers::proxy::ensure_gateway_key;
use crate::admin::registry_io::{with_config_write, ConfigMutation};

#[derive(Debug, Serialize, Clone)]
pub struct ProxyStatus {
    pub running: bool,
    pub addr: Option<String>,
    /// 当前生效的 gateway 鉴权状态。代理启动边界会自动生成缺失的
    /// gateway_api_key,所以 running 时必须为 `true`。
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
        // fix(#210 P2): 停止前 flush L1 session cache 到 L2 sqlite,
        // 减少重启后 previous_response_id cache miss 导致对话中断。
        // flush 是同步操作(纯 mutex lock + sqlite write),不需要 runtime。
        let (total, failed) =
            codex_app_transfer_adapters::responses::session::global_response_session_cache()
                .flush_to_persistent();
        if total > 0 {
            codex_app_transfer_proxy::proxy_telemetry().logs.add(
                "INFO",
                format!("session cache flush before stop: {total} entries, {failed} failed"),
            );
        }

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

    let cfg: Config = with_config_write(|raw| {
        let mut cfg: Config = serde_json::from_value(raw.clone())
            .map_err(|e| format!("config.json schema mismatch: {e}"))?;
        if cfg.providers.is_empty() {
            return Err("no providers configured; add one first".to_owned());
        }
        if cfg
            .gateway_api_key
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(ConfigMutation::Unchanged(cfg));
        }

        let gateway_key = ensure_gateway_key(raw)?;
        cfg.gateway_api_key = Some(gateway_key);
        Ok(ConfigMutation::Modified(cfg))
    })?;

    let gateway_key = cfg
        .gateway_api_key
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "gateway api key was not generated".to_owned())?;
    Ok(ResolverSnapshot {
        provider_count: cfg.providers.len(),
        active_provider: cfg.active_provider.clone(),
        resolver: StaticResolver::new(Some(gateway_key), cfg.providers, cfg.active_provider),
        gateway_auth: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{body::Body, extract::Request, response::Response, routing::any, Router};
    use serde_json::{json, Value};
    use tokio::net::TcpListener;

    use crate::admin::handlers::common::test_support::with_isolated_home;
    use crate::admin::registry_io::{load as load_registry, save_for_test as save_registry};

    fn config_with_gateway(base_url: String, gateway: Value) -> Value {
        json!({
            "version": "2.1.15",
            "activeProvider": "p1",
            "gatewayApiKey": gateway,
            "providers": [{
                "id": "p1",
                "name": "Provider One",
                "baseUrl": base_url,
                "authScheme": "bearer",
                "apiFormat": "openai_chat",
                "apiKey": "sk-upstream",
                "models": {"default": "model-one"},
                "extraHeaders": {},
                "modelCapabilities": {},
                "requestOptions": {},
                "sortIndex": 0
            }],
            "settings": {
                "theme": "default",
                "language": "zh",
                "proxyPort": 0,
                "adminPort": 18081,
                "autoStart": false,
                "autoApplyOnStart": true,
                "exposeAllProviderModels": false,
                "restoreCodexOnExit": true,
                "updateUrl": codex_app_transfer_registry::DEFAULT_UPDATE_URL
            }
        })
    }

    fn config_with_null_gateway(base_url: String) -> Value {
        config_with_gateway(base_url, Value::Null)
    }

    fn echo_mock() -> Router {
        Router::new().fallback(any(|req: Request| async move {
            let path = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_else(|| "/".to_owned());
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(json!({"path": path}).to_string()))
                .unwrap()
        }))
    }

    async fn spawn(router: Router) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router.into_make_service())
                .await
                .unwrap();
        });
        addr
    }

    #[test]
    fn start_generates_gateway_key_and_requires_auth_when_config_key_is_null() {
        with_isolated_home(|_| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let upstream = spawn(echo_mock()).await;
                save_registry(&config_with_null_gateway(format!("http://{upstream}"))).unwrap();

                let manager = ProxyManager::new();
                let status = manager.start(0).await.unwrap();
                assert!(status.running);
                assert!(status.gateway_auth);

                let saved = load_registry().unwrap();
                let gateway_key = saved
                    .get("gatewayApiKey")
                    .and_then(|v| v.as_str())
                    .expect("gateway key generated");
                assert!(gateway_key.starts_with("cas_"));

                let proxy_addr = status.addr.expect("proxy addr");
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap();

                let unauthorized = client
                    .post(format!("http://{proxy_addr}/v1/chat/completions"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"p1/model-one"}"#)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(unauthorized.status().as_u16(), 401);

                let authorized = client
                    .post(format!("http://{proxy_addr}/v1/chat/completions"))
                    .header("authorization", format!("Bearer {gateway_key}"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"p1/model-one"}"#)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(authorized.status().as_u16(), 200);

                // 错误 key 必须被拒(集成层覆盖,防 middleware 接线错误)
                let wrong = client
                    .post(format!("http://{proxy_addr}/v1/chat/completions"))
                    .header("authorization", "Bearer cas_wrong_key")
                    .header("content-type", "application/json")
                    .body(r#"{"model":"p1/model-one"}"#)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(wrong.status().as_u16(), 401);

                manager.stop_silent();
            });
        });
    }

    /// B1(空串 key 是裸奔的第二入口):gatewayApiKey="" 等同无 key,start 必须
    /// 重新生成并强制鉴权,而不是把 Some("") 交给 resolver。
    #[test]
    fn start_regenerates_gateway_key_when_config_key_is_empty_string() {
        with_isolated_home(|_| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let upstream = spawn(echo_mock()).await;
                save_registry(&config_with_gateway(
                    format!("http://{upstream}"),
                    json!(""),
                ))
                .unwrap();

                let manager = ProxyManager::new();
                let status = manager.start(0).await.unwrap();
                assert!(status.gateway_auth);

                let saved = load_registry().unwrap();
                let gateway_key = saved
                    .get("gatewayApiKey")
                    .and_then(|v| v.as_str())
                    .expect("empty-string key must be regenerated");
                assert!(
                    gateway_key.starts_with("cas_"),
                    "empty key must be replaced by a real cas_ key, not left empty"
                );

                let proxy_addr = status.addr.expect("proxy addr");
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap();
                let unauthorized = client
                    .post(format!("http://{proxy_addr}/v1/chat/completions"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"p1/model-one"}"#)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(unauthorized.status().as_u16(), 401);

                manager.stop_silent();
            });
        });
    }

    /// I1(防"每次 start 覆盖用户 key"回归):已配置非空 gateway key 时,start
    /// 走 Unchanged 分支,磁盘 key 逐字不变,且该 key 可用于鉴权。
    #[test]
    fn start_preserves_existing_gateway_key() {
        with_isolated_home(|_| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let upstream = spawn(echo_mock()).await;
                let existing = "cas_existing_user_key_do_not_touch";
                save_registry(&config_with_gateway(
                    format!("http://{upstream}"),
                    json!(existing),
                ))
                .unwrap();

                let manager = ProxyManager::new();
                let status = manager.start(0).await.unwrap();
                assert!(status.gateway_auth);

                let saved = load_registry().unwrap();
                assert_eq!(
                    saved.get("gatewayApiKey").and_then(|v| v.as_str()),
                    Some(existing),
                    "existing user gateway key must not be overwritten on start"
                );

                let proxy_addr = status.addr.expect("proxy addr");
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap();
                let authorized = client
                    .post(format!("http://{proxy_addr}/v1/chat/completions"))
                    .header("authorization", format!("Bearer {existing}"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"p1/model-one"}"#)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(authorized.status().as_u16(), 200);

                manager.stop_silent();
            });
        });
    }
}
