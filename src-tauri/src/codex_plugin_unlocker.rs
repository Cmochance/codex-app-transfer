//! Codex Desktop Plugins 解锁器 —— 运行时伴侣守护进程
//!
//! 通过 Chrome DevTools Protocol (CDP) 向 Codex Desktop 渲染进程注入 JavaScript,
//! 调用 React state 中的 `setAuthMethod('chatgpt')` 来解锁 Plugins 选项卡。
//!
//! 使用方式:
//! 1. 创建 `PluginUnlockService`
//! 2. 调用 `start()` 启动守护循环
//! 3. 调用 `stop()` 停止
//!
//! 守护循环行为:
//! - 检测 Codex Desktop 进程是否存在
//! - 尝试连接 `http://127.0.0.1:9222/json/list` 获取 CDP endpoint
//! - WebSocket 连接后注入解锁脚本
//! - 监听 `Page.loadEventFired`,刷新后自动重新注入
//! - 断开时指数退避重连

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// CDP debug port 默认值。Chrome / Electron 约定俗成的 remote debugging port。
pub const DEFAULT_CDP_PORT: u16 = 9222;

/// 全局共享的 CDP port — desktop.rs 启动 Codex Desktop 前探测可用端口写入,
/// daemon loop 每次 `detect_cdp` 时读取拼 URL。9222 被 Chrome / Edge / 其它
/// Electron 占用时,desktop.rs 会 fallback 到 OS 分配的随机空闲端口,daemon
/// 通过这个 atomic 看到新值,无需重启。
///
/// 借鉴 `BigPizzaV3/CodexPlusPlus` `launcher.py:267-281`(MIT)端口冲突探测
/// 思路(本仓 Rust 实现用 `TcpListener::bind` 探测,不用 `SO_EXCLUSIVEADDRUSE`
/// 因为 Tokio + std::net::TcpListener 在 Linux/macOS 跨平台一致行为足够)。
pub static CDP_PORT: AtomicU16 = AtomicU16::new(DEFAULT_CDP_PORT);

/// 拼当前 CDP `/json/list` URL,使用 [`CDP_PORT`] 的最新值。
pub fn current_cdp_url() -> String {
    format!(
        "http://127.0.0.1:{}/json/list",
        CDP_PORT.load(Ordering::Relaxed)
    )
}

use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 解锁器状态（线程安全共享）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UnlockStatus {
    /// 未启动或 Codex Desktop 未运行/无调试端口
    Disconnected,
    /// 正在连接 CDP
    Connecting,
    /// 已连接,等待页面就绪
    Connected,
    /// 注入成功,Plugins 已解锁
    Injected,
    /// 注入失败
    Failed { error: String },
}

/// 服务配置
///
/// **注**:CDP HTTP URL 不在这里 — 改用全局 [`CDP_PORT`] atomic 让 desktop.rs
/// 启动 Codex Desktop 时动态写入(端口冲突 fallback)。daemon loop 每次
/// detect 时通过 [`current_cdp_url`] 读最新 port。
#[derive(Debug, Clone)]
pub struct UnlockConfig {
    /// 重连退避:初始延迟（毫秒）。第一次失败后等这么久重试,
    /// 每次失败 ×2 直到 `reconnect_max_ms`。1s 起够快,不会让用户感觉卡。
    pub reconnect_base_ms: u64,
    /// 重连退避上限。30s 是经验值:Codex 启动 / 系统休眠唤醒最长 ~30s 内
    /// CDP 必然就绪,再长意义不大。
    pub reconnect_max_ms: u64,
}

impl Default for UnlockConfig {
    fn default() -> Self {
        Self {
            reconnect_base_ms: 1_000,
            reconnect_max_ms: 30_000,
        }
    }
}

/// CDP Page 信息（来自 `/json/list`）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CdpPage {
    id: String,
    #[serde(rename = "type")]
    page_type: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws_url: Option<String>,
}

/// CDP WebSocket 消息
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CdpResponse {
    id: Option<u64>,
    #[serde(rename = "result")]
    result: Option<serde_json::Value>,
    error: Option<CdpError>,
    #[serde(rename = "method")]
    method: Option<String>,
    #[serde(rename = "params")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CdpError {
    code: i32,
    message: String,
}

/// 解锁服务
pub struct PluginUnlockService {
    config: UnlockConfig,
    status: Arc<RwLock<UnlockStatus>>,
    /// 控制守护循环的通道
    cmd_tx: mpsc::Sender<ServiceCommand>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    /// CDP 消息 ID 单调递增计数器(无锁,daemon + tests 共享)
    msg_id: Arc<AtomicU64>,
}

#[derive(Debug)]
enum ServiceCommand {
    Stop,
    /// 强制重新注入（前端手动触发）
    Reinject,
}

impl PluginUnlockService {
    pub fn new(config: UnlockConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        Self {
            config,
            status: Arc::new(RwLock::new(UnlockStatus::Disconnected)),
            cmd_tx,
            cmd_rx: Arc::new(Mutex::new(cmd_rx)),
            msg_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 使用默认配置创建。前端 HTTP handler 跟 setup hook 都通过这个共享同
    /// 一份 OnceCell 实例,见 `admin::handlers::plugin_unlock::get_service`。
    pub fn with_defaults() -> Self {
        Self::new(UnlockConfig::default())
    }

    /// 获取当前状态
    pub async fn status(&self) -> UnlockStatus {
        self.status.read().await.clone()
    }

    /// 启动守护循环（非阻塞）
    pub fn start(&self) {
        let config = self.config.clone();
        let status = self.status.clone();
        let cmd_rx = self.cmd_rx.clone();
        let msg_id = self.msg_id.clone();

        tokio::spawn(async move {
            run_daemon(config, status, cmd_rx, msg_id).await;
        });
    }

    /// 停止守护循环
    pub async fn stop(&self) {
        let _ = self.cmd_tx.send(ServiceCommand::Stop).await;
    }

    /// 前端手动触发重新注入
    pub async fn reinject(&self) {
        let _ = self.cmd_tx.send(ServiceCommand::Reinject).await;
    }
}

/// 守护循环主逻辑
async fn run_daemon(
    config: UnlockConfig,
    status: Arc<RwLock<UnlockStatus>>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    msg_id: Arc<AtomicU64>,
) {
    let mut reconnect_delay = config.reconnect_base_ms;

    loop {
        // 检查是否有外部命令(此时未连 WS — 真正注入态的 Reinject 在
        // `connect_and_monitor` 内 select! 处理)。未连接时收到 Reinject
        // 视作"加速重连请求":reset backoff 让下一次 detect 立即跑,
        // 而不是静默 noop。
        {
            let mut rx = cmd_rx.lock().await;
            if let Ok(cmd) = rx.try_recv() {
                match cmd {
                    ServiceCommand::Stop => {
                        tracing::info!("[PluginUnlock] daemon stopped by command");
                        set_status(&status, UnlockStatus::Disconnected).await;
                        return;
                    }
                    ServiceCommand::Reinject => {
                        tracing::info!(
                            "[PluginUnlock] reinject requested while disconnected, resetting backoff"
                        );
                        reconnect_delay = config.reconnect_base_ms;
                    }
                }
            }
        }

        // 阶段 1: 检测 CDP 端口是否可用 — 用 current_cdp_url() 每次重新拼,
        // 这样 desktop.rs 在端口冲突 fallback 时写入 CDP_PORT atomic 后,daemon
        // 下一轮 loop 立刻看到新 port,无需重启。
        match detect_cdp(&current_cdp_url()).await {
            Some(pages) => {
                // Codex Desktop 同时开多个 BrowserWindow:主窗口
                // `app://-/index.html` + 宠物悬浮窗 `app://-/index.html?initialRoute=
                // %2Favatar-overlay` + 可能的 DevTools / extension。我们只想注主
                // 窗口(那里才有 Plugins UI 跟 AuthContext)。
                //
                // 早期版本 `find(|p| p.page_type == "page")` 拿第一个 — 真机
                // 发现宠物窗排第一,导致一直注错地方(log 里 "找不到
                // setAuthMethod hook" 正是因为宠物窗根本没这个 Context)。
                //
                // 筛选规则:type=page + URL 含 `index.html` + 不含 `avatar-overlay`
                // (宠物窗用 query param 路由,主窗口无 query 或别的路由)。
                let (target, all_pages_for_log) = {
                    let snapshot: Vec<String> = pages
                        .iter()
                        .filter(|p| p.page_type == "page")
                        .map(|p| p.url.clone())
                        .collect();
                    let target = pages.into_iter().find(|p| {
                        p.page_type == "page"
                            && p.url.contains("index.html")
                            && !p.url.contains("avatar-overlay")
                    });
                    (target, snapshot)
                };
                if let Some(page) = target {
                    if let Some(ws_url) = page.ws_url {
                        set_status(&status, UnlockStatus::Connecting).await;
                        tracing::info!("[PluginUnlock] connecting to CDP: {}", ws_url);
                        match connect_and_monitor(&ws_url, &cmd_rx, &msg_id, &status).await {
                            Ok(()) => {
                                tracing::info!("[PluginUnlock] connection ended gracefully");
                                reconnect_delay = config.reconnect_base_ms;
                            }
                            Err(e) => {
                                tracing::warn!("[PluginUnlock] connection error: {}", e);
                                set_status(
                                    &status,
                                    UnlockStatus::Failed {
                                        error: e.to_string(),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                } else {
                    // CDP 在跑但没找到主窗口 — 可能 Codex 还在 mount / 只
                    // 开了宠物悬浮窗 / 未来 Codex URL schema 变了。warn 级日志
                    // 列出我们看到的全部 page URLs,方便 support 诊断"我的
                    // Codex 在开但 daemon 一直显示 Disconnected"。状态保持
                    // Disconnected 让 backoff 重试。
                    tracing::warn!(
                        "[PluginUnlock] CDP reachable but no main window matched (need URL containing 'index.html' and not 'avatar-overlay'); visible pages={:?}",
                        all_pages_for_log
                    );
                    set_status(&status, UnlockStatus::Disconnected).await;
                }
            }
            None => {
                // CDP 不可用,保持 Disconnected。set_status 内部已做 != 比对,
                // 无需额外 if_not 包装。
                set_status(&status, UnlockStatus::Disconnected).await;
            }
        }

        // 指数退避:1s → 2s → 4s → ... → 30s 封顶
        sleep(Duration::from_millis(reconnect_delay)).await;
        reconnect_delay = (reconnect_delay * 2).min(config.reconnect_max_ms);
    }
}

/// 检测 CDP HTTP 端点，返回 page 列表
async fn detect_cdp(url: &str) -> Option<Vec<CdpPage>> {
    match reqwest::get(url).await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<Vec<CdpPage>>().await {
                    Ok(pages) if !pages.is_empty() => Some(pages),
                    _ => None,
                }
            } else {
                None
            }
        }
        Err(e) => {
            tracing::debug!("[PluginUnlock] CDP detect failed: {}", e);
            None
        }
    }
}

/// WebSocket 连接、注入、并持续监控页面刷新
async fn connect_and_monitor(
    ws_url: &str,
    cmd_rx: &Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    msg_id_counter: &AtomicU64,
    status: &Arc<RwLock<UnlockStatus>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    // 1. 启用 Runtime domain
    let (runtime_enable, runtime_enable_id) =
        make_cdp_msg(msg_id_counter, "Runtime.enable", json!({}));
    write.send(WsMessage::Text(runtime_enable)).await?;
    let _ = await_cdp_response(&mut read, runtime_enable_id, Duration::from_secs(5)).await;

    // 2. 启用 Page domain（监听刷新事件）
    let (page_enable, page_enable_id) = make_cdp_msg(msg_id_counter, "Page.enable", json!({}));
    write.send(WsMessage::Text(page_enable)).await?;
    let _ = await_cdp_response(&mut read, page_enable_id, Duration::from_secs(5)).await;

    // 3. 首次注入
    inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;

    // 4. 持续监控:监听 Page.loadEventFired / 外部命令 / 心跳响应
    //
    // 重要不变量:WS read 流**只能由 select! 的 read.next() 分支消费**,绝不能
    // 在 select! 的其它分支内 `await read.next()` 或 `await_cdp_response(read, …)`
    // —— 否则会从公用 stream 里抢走 `Page.loadEventFired` 等无 id 事件帧,导致
    // 整页 reload 后 daemon 失去重新注入触发(Devin Review BUG-0002 在 PR #255
    // 抓到的回归)。所以心跳采用 fire-and-forget 风格:send ping,把 ping_id
    // 寄存到 `pending_heartbeat_id`,主 read 分支统一匹配并处理响应。
    let mut pending_heartbeat_id: Option<u64> = None;

    loop {
        tokio::select! {
            // 监听 WebSocket 消息(唯一的 read 消费者)
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Ok(resp) = serde_json::from_str::<CdpResponse>(&text) {
                            // 心跳响应 — 提取 unlocked flag,把 in-page MutationObserver
                            // 异步解锁的状态升级到 daemon 端 Injected。
                            if pending_heartbeat_id.is_some() && resp.id == pending_heartbeat_id {
                                pending_heartbeat_id = None;
                                let unlocked = resp
                                    .result
                                    .as_ref()
                                    .and_then(|r| r.get("result"))
                                    .and_then(|r| r.get("value"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                if unlocked {
                                    set_status(status, UnlockStatus::Injected).await;
                                }
                                continue;
                            }
                            // 整页 reload — 重新注入
                            if resp.method.as_deref() == Some("Page.loadEventFired") {
                                tracing::info!("[PluginUnlock] page refreshed, reinjecting...");
                                inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        tracing::info!("[PluginUnlock] WebSocket closed");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("[PluginUnlock] WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            // 监听外部命令
            cmd = async {
                let mut rx = cmd_rx.lock().await;
                rx.recv().await
            } => {
                match cmd {
                    Some(ServiceCommand::Reinject) => {
                        tracing::info!("[PluginUnlock] manual reinject requested");
                        inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;
                    }
                    Some(ServiceCommand::Stop) => {
                        tracing::info!("[PluginUnlock] stop requested, closing connection");
                        let _ = write.close().await;
                        return Ok(());
                    }
                    None => break,
                }
            }

            // 心跳 + 状态回收(fire-and-forget):每 30s eval
            // `window.__codexAppTransferPluginUnlocker?.unlocked` ——
            //   1. 探活 WS / Codex Desktop 还在响应(send 失败即 WS 死)
            //   2. 把页内 MutationObserver 在登录态 mount 后异步成功的状态
            //      **round-trip 回 Rust 端**。`Page.loadEventFired` 不会在
            //      SPA 内 router 切换 / OAuth 登录回调时触发,只有 MutationObserver
            //      跑得到;那条路径完成解锁后只更新了 in-page `window[MARKER].unlocked`,
            //      靠这里 30s 轮询补回 Rust 端的可观察信号。
            //
            // 不在这里 await 响应 —— 见上方"重要不变量"。
            _ = sleep(Duration::from_secs(30)) => {
                // 上一轮心跳还没回包就丢弃 id 不追了(网络慢 / Codex stutter),
                // 用新 id 重发。旧响应到达时不匹配 pending_heartbeat_id 会被忽略。
                let (ping, ping_id) = make_cdp_msg(
                    msg_id_counter,
                    "Runtime.evaluate",
                    json!({
                        "expression": "window.__codexAppTransferPluginUnlocker?.unlocked === true",
                        "returnByValue": true
                    }),
                );
                if let Err(e) = write.send(WsMessage::Text(ping)).await {
                    tracing::warn!("[PluginUnlock] heartbeat send failed, connection dead: {}", e);
                    break;
                }
                pending_heartbeat_id = Some(ping_id);
            }
        }
    }

    Ok(())
}

/// 发送注入脚本
async fn inject_unlock_script(
    write: &mut (impl Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    read: &mut (impl Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
    msg_id_counter: &AtomicU64,
    status: &Arc<RwLock<UnlockStatus>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    set_status(status, UnlockStatus::Connected).await;

    // 注入脚本 — 解锁 Codex Desktop Plugins 选项卡。
    //
    // 算法借鉴 galaxywk223/codex-plugin-unlocker (MIT, 2026-05-11);上游
    // 2026-05-20 commit `47d0b0a` 引入 `wait_for_injection`(90s 等可注入页面),
    // 我们在脚本内做同等耐心(30 × 500ms = 15s),外层 daemon 维持指数 backoff
    // 重连,组合后总体耐心 ≈ 15s 内部 + 30s backoff 上限,够 Codex Desktop
    // 首启慢机器场景。
    //   https://github.com/galaxywk223/codex-plugin-unlocker/blob/main/codex_plugin_unlocker/inject/plugin-unlock.js
    //
    // 关键差异 vs. 早期版本(找 useState hook 链上的 setAuthMethod setter):
    // - 新策略走 React Context — 从 plugin 入口 DOM 节点拿 fiber,沿 `fiber.return`
    //   向上爬,检查每层 `memoizedProps.value` / `pendingProps.value`,找带
    //   `setAuthMethod` + `authMethod` 字段的对象(即 `AuthContext.Provider` value)
    // - Codex Desktop 26.513+ 的 React state 结构变了,旧 hook-scan 策略失效;
    //   Context.Provider 是 React 公开 API,比 hook 链表稳定得多(实测 26.519
    //   仍有效,见 #253)
    // - 加 DOM-level enable(清 disabled / __reactProps disabled),即使 setter
    //   找不到也能让按钮可点(strict fallback)
    // - 加 MutationObserver,SPA 路由跳转重渲时自动重跑
    //
    // 异步包装:返回 Promise<{ ok, reason, error? }>,通过 awaitPromise: true 拿到结果。
    // `reason` 取值:
    //   - `unlocked`         setAuthMethod('chatgpt') 已调用成功(或本来就是 chatgpt)
    //   - `no_plugin_button` 重试窗口内 DOM 始终没渲染 Plugins 入口(未登录 /
    //                       SPA 还在 mount / 在 onboarding 页等)— 可恢复,不算 Failed
    //   - `no_auth_context`  按钮渲染了但 fiber 链上爬不到 AuthContext.Provider
    //                       value —— 真版本不兼容信号
    //   - `setter_threw`     setAuthMethod 调用本身抛错 —— React 内部异常
    // 内部最多 30 次重试(每次 500ms 间隔)等 plugin 按钮 DOM 出现 + Context 就绪。
    let unlock_script = r#"
(async function() {
    const MARKER = '__codexAppTransferPluginUnlocker';
    window[MARKER] = window[MARKER] || { version: '2.1.16', unlocked: false };

    const selectors = {
        disabledInstallButton: 'button:disabled.w-full.justify-center, [role="button"][aria-disabled="true"].cursor-not-allowed',
        pluginNavButton: 'nav[role="navigation"] button.h-token-nav-row.w-full',
        pluginSvgPath: 'svg path[d^="M7.94562 14.0277"]',
    };

    function reactFiberFrom(element) {
        const key = Object.keys(element).find((k) => k.startsWith('__reactFiber'));
        return key ? element[key] : null;
    }
    function reactPropsKeyFrom(element) {
        return Object.keys(element).find((k) => k.startsWith('__reactProps'));
    }
    function authContextValueFrom(element) {
        for (let fiber = reactFiberFrom(element); fiber; fiber = fiber.return) {
            for (const v of [fiber.memoizedProps?.value, fiber.pendingProps?.value]) {
                if (v && typeof v === 'object'
                    && typeof v.setAuthMethod === 'function'
                    && 'authMethod' in v) {
                    return v;
                }
            }
        }
        return null;
    }
    // 返回值约定:
    //   'unlocked'       已成功(本来就是 chatgpt 或刚 set 成 chatgpt)
    //   'no_auth_context' fiber 爬不到带 setAuthMethod 的 Context value
    //   'setter_threw'    setAuthMethod 调用本身抛错
    function spoofChatGPTAuthMethod(element) {
        const auth = authContextValueFrom(element);
        if (!auth) return 'no_auth_context';
        if (auth.authMethod === 'chatgpt') {
            window[MARKER].unlocked = true;
            return 'unlocked';
        }
        try {
            auth.setAuthMethod('chatgpt');
        } catch (e) {
            window[MARKER].lastError = String(e?.stack || e);
            return 'setter_threw';
        }
        window[MARKER].unlocked = true;
        return 'unlocked';
    }
    function pluginEntryButton() {
        const byIcon = document.querySelector(
            selectors.pluginNavButton + ' ' + selectors.pluginSvgPath
        )?.closest('button');
        if (byIcon) return byIcon;
        return Array.from(document.querySelectorAll(selectors.pluginNavButton)).find((b) => {
            const t = (b.textContent || '').trim();
            return /^(插件|Plugins)(\s+-\s+.*)?$/i.test(t);
        }) || null;
    }
    function normalizePluginEntryLabel(button) {
        const node = Array.from(button.querySelectorAll('span, div')).reverse()
            .flatMap((n) => Array.from(n.childNodes))
            .find((n) => n.nodeType === 3
                && /^(插件|Plugins)( - 已解锁| - Unlocked)?$/i.test((n.nodeValue || '').trim()));
        if (!node) return;
        const cur = (node.nodeValue || '').trim();
        node.nodeValue = /^Plugins/i.test(cur) ? 'Plugins' : '插件';
    }
    // 返回:
    //   'no_plugin_button'  按钮还没渲染(DOM 尚未就绪)
    //   spoof 函数的返回值('unlocked' / 'no_auth_context' / 'setter_threw')
    function enablePluginEntry() {
        const btn = pluginEntryButton();
        if (!btn) return 'no_plugin_button';
        const reason = spoofChatGPTAuthMethod(btn);
        btn.disabled = false;
        btn.removeAttribute('disabled');
        btn.style.display = '';
        btn.querySelectorAll('*').forEach((n) => { n.style.display = ''; });
        normalizePluginEntryLabel(btn);
        const propsKey = reactPropsKeyFrom(btn);
        if (propsKey) { btn[propsKey].disabled = false; }
        if (btn.dataset.codexAppTransferPluginUnlocked !== 'true') {
            btn.dataset.codexAppTransferPluginUnlocked = 'true';
            btn.addEventListener('click', () => spoofChatGPTAuthMethod(btn), true);
        }
        return reason;
    }
    function unblockButtonElement(button) {
        button.disabled = false;
        button.removeAttribute('disabled');
        button.removeAttribute('aria-disabled');
        button.classList.remove('disabled', 'opacity-50', 'cursor-not-allowed', 'pointer-events-none');
        button.style.pointerEvents = 'auto';
        button.tabIndex = 0;
        const propsKey = reactPropsKeyFrom(button);
        if (propsKey) {
            button[propsKey].disabled = false;
            button[propsKey]['aria-disabled'] = false;
        }
    }
    function labelForcedInstallButton(button) {
        const node = Array.from(button.childNodes).find((n) => {
            const t = (n.nodeValue || '').trim();
            return n.nodeType === 3
                && (/^安装\s/.test(t) || /^Install\s/.test(t) || t === '强制安装');
        });
        if (node) node.nodeValue = '强制安装';
    }
    function unblockPluginInstallButtons() {
        document.querySelectorAll(selectors.disabledInstallButton).forEach((b) => {
            const t = (b.textContent || '').trim();
            if (!/^安装\s/.test(t) && !/^Install\s/.test(t) && t !== '强制安装') return;
            unblockButtonElement(b);
            labelForcedInstallButton(b);
        });
    }
    function runUnlock() {
        // 拆开 try block:enablePluginEntry 决定 reason,unblockPluginInstallButtons
        // 的异常不应该污染主入口的 reason(install 按钮装饰失败 ≠ entry button 解锁失败)
        let reason;
        try {
            reason = enablePluginEntry();
        } catch (e) {
            window[MARKER].lastError = String(e?.stack || e);
            return 'setter_threw';
        }
        try {
            unblockPluginInstallButtons();
        } catch (e) {
            // 仅记录,不降级 reason — entry 已成功解锁,install 按钮装饰是 nice-to-have
            window[MARKER].lastError = String(e?.stack || e);
        }
        return reason;
    }
    function scheduleUnlock() {
        if (window[MARKER].scanPending) return;
        window[MARKER].scanPending = true;
        setTimeout(() => {
            window[MARKER].scanPending = false;
            runUnlock();
        }, 200);
    }

    // 重试等 plugin 按钮 DOM 出现 + AuthContext 就绪。30 × 500ms = 15 秒预算,
    // 覆盖 Codex Desktop 首启慢 / 登录态异步 mount / SPA 路由切换。
    // 早停规则:一旦拿到 'unlocked' 就 break;'no_auth_context' / 'setter_threw'
    // 是"按钮在但 Context 异常"信号,继续等也大概率不会变(版本不兼容)— 但仍
    // 让循环跑完,因为有可能 React 在重渲过程中暂时性 Context 缺失。
    let lastReason = 'no_plugin_button';
    for (let i = 0; i < 30; i++) {
        lastReason = runUnlock();
        if (lastReason === 'unlocked') break;
        await new Promise((r) => setTimeout(r, 500));
    }

    // SPA 路由跳转 / sidebar 重渲会冲掉我们的 DOM mutation,装 observer 持续 enforce。
    // **不基于 unlocked 标志决定是否 disconnect** — `window[MARKER].unlocked`
    // 一旦置 true 永不 reset(marker 用 `|| { ... }` 复用),但用户后续 logout /
    // 切账号会让 authMethod 切回非 chatgpt 重新锁 Plugins;observer 必须始终在
    // 装,才能在 re-lock 场景下被 mutation 触发重新跑 runUnlock → 重 inject
    // setAuthMethod('chatgpt') 解锁。`spoofChatGPTAuthMethod` 内 early-return
    // 已保证已 chatgpt 不重复调 setAuthMethod,所以已解锁后 observer
    // 反复 fire 也不会触发 React 重渲 → 不会有视觉抖动。
    window[MARKER].observer?.disconnect();
    window[MARKER].observer = new MutationObserver(scheduleUnlock);
    window[MARKER].observer.observe(
        document.body || document.documentElement,
        { childList: true, subtree: true }
    );

    return {
        ok: lastReason === 'unlocked',
        reason: lastReason,
        error: window[MARKER].lastError || null,
    };
})()
"#;

    let (evaluate, evaluate_id) = make_cdp_msg(
        msg_id_counter,
        "Runtime.evaluate",
        json!({
            "expression": unlock_script,
            "awaitPromise": true,
            "returnByValue": true
        }),
    );

    write.send(WsMessage::Text(evaluate)).await?;

    // 必须按 CDP message id 匹配响应,而不是简单读"下一帧"。
    // 注入脚本内 `console.log` 会触发 `Runtime.consoleAPICalled` 事件帧,
    // 30 秒心跳的 evaluate 响应也可能排队 — 这些都不带 evaluate_id,
    // 会跟我们的目标响应交错。await_cdp_response 循环丢弃非目标 id 的帧。
    //
    // 超时预算 = JS 内部 30 × 500ms 重试窗口 (15s) + WS / 序列化开销 buffer。
    // `awaitPromise: true` 使 CDP 必须等到 Promise resolve 才返回,如果这里
    // 超时小于 JS 重试窗口,daemon 会在脚本还在重试时就 tear down WS,把 15s
    // 耐心变成 dead code。20s 给 5s buffer 容 React 重渲 / GC stutter。
    // (Devin Review BUG-0001 在 PR #255 抓到的初始 8s 超时不够。)
    let parsed = match await_cdp_response(read, evaluate_id, Duration::from_secs(20)).await {
        Ok(resp) => resp,
        Err(e) => {
            set_status(
                status,
                UnlockStatus::Failed {
                    error: format!("CDP Runtime.evaluate 响应等待失败: {e}"),
                },
            )
            .await;
            return Err(e.into());
        }
    };

    if let Some(error) = parsed.error {
        let msg = format!("CDP error {}: {}", error.code, error.message);
        set_status(status, UnlockStatus::Failed { error: msg.clone() }).await;
        return Err(msg.into());
    }

    // 脚本返回 `{ ok, reason, error? }`,按 reason 分流:
    // - unlocked: 成功,状态 Injected
    // - no_plugin_button: DOM 尚未渲染(未登录/SPA mount 中),保持 Connected
    //   不报 Failed,observer 装好后会自动重试。返回 Ok 让 daemon 继续维持 WS
    // - no_auth_context / setter_threw / unknown: 真版本不兼容或 React 异常,
    //   报 Failed 并返回 Err 让外层 backoff 重连
    let raw_value = parsed
        .result
        .as_ref()
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"));
    let outcome = classify_inject_outcome(raw_value);

    match outcome {
        InjectOutcome::Unlocked => {
            set_status(status, UnlockStatus::Injected).await;
            Ok(())
        }
        InjectOutcome::NoPluginButton => {
            tracing::info!(
                "[PluginUnlock] inject pending: Codex Desktop 主界面尚未渲染 Plugins 入口 \
                 (可能未登录 / 启动中 / 在 onboarding 页),DOM observer 将在 mount 时自动重试"
            );
            // 不切 Failed — 保持 Connected。回收路径有两条:
            //   1. 整页 reload 时 `Page.loadEventFired` 触发 daemon 重新 inject
            //   2. 页内 MutationObserver 检测到 DOM mount 后跑 runUnlock 直接置
            //      `window[MARKER].unlocked = true`;daemon 30s 心跳轮询该 flag,
            //      读到 true 后把状态升级到 Injected
            // 路径 2 是 SPA 登录回调 / 路由切换场景的主要恢复通道
            // (`loadEventFired` 不会在 SPA 内 nav 时触发)。
            Ok(())
        }
        InjectOutcome::NoAuthContext { script_error } => {
            let mut msg = "找到 Plugins 入口但未发现 React AuthContext.Provider — \
                 Codex Desktop 版本可能不兼容,请在 issue 中附 Codex Desktop 版本号"
                .to_string();
            if let Some(err) = script_error {
                msg.push_str(&format!(" (脚本日志: {err})"));
            }
            set_status(status, UnlockStatus::Failed { error: msg.clone() }).await;
            Err(msg.into())
        }
        InjectOutcome::SetterThrew { script_error } => {
            let msg = format!(
                "调用 React setAuthMethod 时抛错: {}",
                script_error.unwrap_or_else(|| "未知错误".into())
            );
            set_status(status, UnlockStatus::Failed { error: msg.clone() }).await;
            Err(msg.into())
        }
        InjectOutcome::Unknown { raw } => {
            let msg = format!("注入脚本返回未知形态: {raw}");
            set_status(status, UnlockStatus::Failed { error: msg.clone() }).await;
            Err(msg.into())
        }
    }
}

/// 注入脚本返回值的分类。
///
/// 跟 JS 端 return shape 严格对应:`{ ok, reason, error? }`。从纯 JSON 推断
/// 解出来,跟 daemon 状态切换逻辑解耦,方便单元测试。
#[derive(Debug, Clone, PartialEq)]
enum InjectOutcome {
    /// 已成功调用 `setAuthMethod('chatgpt')`(或 authMethod 本来就是 chatgpt)
    Unlocked,
    /// DOM 还没渲染 Plugins 按钮 —— 可恢复(未登录 / SPA mount 中 / onboarding 页)
    NoPluginButton,
    /// 按钮在 DOM 里但 fiber 链上爬不到 AuthContext.Provider value —— 版本不兼容
    NoAuthContext { script_error: Option<String> },
    /// `setAuthMethod` 调用本身抛错
    SetterThrew { script_error: Option<String> },
    /// 脚本返回了我们不认识的形态(老/新版本协议错位 / CDP 异常)
    Unknown { raw: String },
}

/// 解析脚本 `Runtime.evaluate` 的 `result.value` 字段。
///
/// 严格只接受新版 object 形态:`{ ok, reason, error? }`。脚本跟 daemon 同 binary
/// 发布,不存在版本错位场景 —— 不接受 `bool` 等老形态作为 fallback("假设最
/// 常见 reason" 违反 no-silent-destructive-fallback 规则,会让真正的 selector
/// 漂移失败被静默归类为"DOM 还没就绪"而被吞掉)。
///
/// - `null` / 缺失 → `Unknown`
/// - object 带认识的 `reason` → 对应 variant
/// - object 带不认识的 `reason` / 非 object → `Unknown`(报 Failed,不静默)
fn classify_inject_outcome(raw: Option<&serde_json::Value>) -> InjectOutcome {
    let Some(value) = raw else {
        return InjectOutcome::Unknown {
            raw: "<missing>".into(),
        };
    };

    let reason = value.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    let script_error = value
        .get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    match reason {
        "unlocked" => InjectOutcome::Unlocked,
        "no_plugin_button" => InjectOutcome::NoPluginButton,
        "no_auth_context" => InjectOutcome::NoAuthContext { script_error },
        "setter_threw" => InjectOutcome::SetterThrew { script_error },
        _ => InjectOutcome::Unknown {
            raw: value.to_string(),
        },
    }
}

/// 循环读 WebSocket 帧,丢弃非目标 id 的事件 / 响应,直到拿到 `target_id`
/// 对应的响应或超时。
///
/// CDP 协议在 active session 上会持续推送各种事件帧(`Runtime.consoleAPICalled`
/// / `Page.loadEventFired` / 其他 request 的 response),不能假设"下一帧
/// 就是我刚发的 request 的 reply"— 必须按 `resp.id == Some(target_id)`
/// 精确匹配。
async fn await_cdp_response(
    read: &mut (impl Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
    target_id: u64,
    timeout: Duration,
) -> Result<CdpResponse, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frame = match tokio::time::timeout_at(deadline, read.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(e))) => return Err(format!("ws read error: {e}")),
            Ok(None) => return Err("ws closed before response".into()),
            Err(_) => return Err(format!("timed out waiting for id={target_id}")),
        };
        let WsMessage::Text(text) = frame else {
            continue;
        };
        let resp: CdpResponse = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if resp.id == Some(target_id) {
            return Ok(resp);
        }
        tracing::trace!(
            target_id,
            dropped_id = ?resp.id,
            dropped_method = ?resp.method,
            "[PluginUnlock] dropping non-target CDP frame while awaiting response"
        );
    }
}

/// 生成 CDP 消息 JSON,返回 `(序列化后的 text frame, 该消息的 id)`。
/// 调用方需保留 `id` 用于 `await_cdp_response` 匹配响应。
fn make_cdp_msg(counter: &AtomicU64, method: &str, params: serde_json::Value) -> (String, u64) {
    let id = counter.fetch_add(1, Ordering::Relaxed) + 1;
    let json = json!({
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string();
    (json, id)
}

/// 设置状态（带日志）
async fn set_status(status: &Arc<RwLock<UnlockStatus>>, new: UnlockStatus) {
    let mut s = status.write().await;
    if *s != new {
        tracing::info!("[PluginUnlock] status: {:?} → {:?}", *s, new);
        *s = new;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_serialization() {
        let s = UnlockStatus::Failed {
            error: "test".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("failed"));
    }

    #[test]
    fn test_default_config_has_reconnect_bounds() {
        let c = UnlockConfig::default();
        assert_eq!(c.reconnect_base_ms, 1_000);
        assert_eq!(c.reconnect_max_ms, 30_000);
    }

    #[test]
    fn classify_inject_outcome_handles_new_object_shape() {
        // 新脚本 happy path
        let v = json!({"ok": true, "reason": "unlocked", "error": null});
        assert_eq!(classify_inject_outcome(Some(&v)), InjectOutcome::Unlocked);

        // 新脚本:DOM 没渲染(可恢复)
        let v = json!({"ok": false, "reason": "no_plugin_button"});
        assert_eq!(
            classify_inject_outcome(Some(&v)),
            InjectOutcome::NoPluginButton
        );

        // 新脚本:版本不兼容 + 带脚本错误信息
        let v = json!({"ok": false, "reason": "no_auth_context", "error": "TypeError: x"});
        assert_eq!(
            classify_inject_outcome(Some(&v)),
            InjectOutcome::NoAuthContext {
                script_error: Some("TypeError: x".into())
            }
        );

        // 新脚本:setter 抛错
        let v = json!({"ok": false, "reason": "setter_threw", "error": "boom"});
        assert_eq!(
            classify_inject_outcome(Some(&v)),
            InjectOutcome::SetterThrew {
                script_error: Some("boom".into())
            }
        );

        // 新脚本:不认识的 reason(未来扩展或协议错位)
        let v = json!({"ok": false, "reason": "weird"});
        match classify_inject_outcome(Some(&v)) {
            InjectOutcome::Unknown { raw } => assert!(raw.contains("weird")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn classify_inject_outcome_rejects_non_object_returns() {
        // bool 不再被静默兼容 —— 必须走对象形态,不然算 Unknown(报 Failed)
        for raw in [json!(true), json!(false), json!(0), json!("ok"), json!([])] {
            match classify_inject_outcome(Some(&raw)) {
                InjectOutcome::Unknown { .. } => {}
                other => panic!("expected Unknown for {raw:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn classify_inject_outcome_missing_value_is_unknown() {
        match classify_inject_outcome(None) {
            InjectOutcome::Unknown { raw } => assert_eq!(raw, "<missing>"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn current_cdp_url_reflects_cdp_port_atomic() {
        // 防止默认 9222 假阴性:跨测试可能其它 case 改过 CDP_PORT,显式设回 9222
        CDP_PORT.store(DEFAULT_CDP_PORT, Ordering::Relaxed);
        assert_eq!(current_cdp_url(), "http://127.0.0.1:9222/json/list");

        // 模拟 desktop.rs 端口冲突 fallback 写入随机端口
        CDP_PORT.store(54321, Ordering::Relaxed);
        assert_eq!(current_cdp_url(), "http://127.0.0.1:54321/json/list");

        // 恢复默认避免污染其它测试
        CDP_PORT.store(DEFAULT_CDP_PORT, Ordering::Relaxed);
    }
}
